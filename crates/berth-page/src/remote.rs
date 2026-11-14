//! One host of the server, for its overview: requests become operations of
//! that host on the portal, and what comes back becomes events.

use crate::portal::{Frame, Portal};
use berth_api as api;
use berth_tui::host::{Event, NEEDS_API, Request, Transport, exited, list};
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

type Events = Rc<RefCell<VecDeque<Event>>>;

pub struct Remote {
    portal: Portal,
    host: String,
    events: Events,
    /// The stream of the session in the panel, and whether it is a terminal.
    stream: Cell<Option<(u32, bool)>>,
}

/// Signs that the host has no controller, in what the server says of it.
fn missing(message: &str) -> bool {
    ["command not found", "berthd: not found", "control.sock"]
        .iter()
        .any(|sign| message.contains(sign))
}

impl Remote {
    pub fn new(portal: Portal, host: &str) -> Remote {
        Remote {
            portal,
            host: host.to_owned(),
            events: Events::default(),
            stream: Cell::new(None),
        }
    }

    fn call(
        &self,
        op: &str,
        args: Value,
        data: Value,
        then: impl FnOnce(Result<Value, String>, &Events) + 'static,
    ) {
        let events = self.events.clone();
        self.portal
            .call(op, Some(&self.host), args, data, move |result| {
                then(result, &events)
            });
    }

    /// Ask for the sessions and the stats again.
    fn poll(&self) {
        self.call("sessions.list", json!({}), Value::Null, |result, events| {
            if let Ok(sessions) = result {
                events
                    .borrow_mut()
                    .push_back(Event::Sessions(list(sessions, "sessions")));
            }
        });
        self.call("system.stats", json!({}), Value::Null, |result, events| {
            if let Ok(stats) = result
                .and_then(|stats| serde_json::from_value(stats).map_err(|error| error.to_string()))
            {
                events.borrow_mut().push_back(Event::Stats(Box::new(stats)));
            }
        });
    }

    fn opened(result: Result<Value, String>, events: &Events) {
        let session = result.and_then(|session| {
            serde_json::from_value::<api::Session>(session).map_err(|error| error.to_string())
        });
        events.borrow_mut().push_back(match session {
            Ok(session) => Event::Opened(Box::new(session)),
            Err(error) => Event::Error(error),
        });
    }

    fn follow(&self, session: api::Session) {
        let terminal = session.kind == api::SessionKind::Terminal;
        let id = session.id.clone();
        // A terminal replays its screen; a process is read from the start.
        let args = match terminal {
            true => json!({"id": id}),
            false => json!({"id": id, "since": 0}),
        };
        let events = self.events.clone();
        let (portal, host) = (self.portal.clone(), self.host.clone());
        let heard = Cell::new(false);
        let stream = self
            .portal
            .stream("sessions.attach", Some(&self.host), args, move |frame| {
                let event = match frame {
                    Frame::Bytes(bytes) => match (terminal, bytes.split_first()) {
                        (true, _) => Event::Output(id.clone(), bytes),
                        (false, Some((_, rest))) => Event::Output(id.clone(), rest.to_vec()),
                        (false, None) => return,
                    },
                    Frame::Data(data) => match exited(&data) {
                        Some(code) => Event::Exited(id.clone(), code),
                        None => return,
                    },
                    // An ended session whose stream does not open can still be read back.
                    Frame::End(Some(error)) if !heard.get() && !session.running() => {
                        let (events, id, code) = (events.clone(), id.clone(), session.exit_code);
                        portal.call(
                            "sessions.output",
                            Some(&host),
                            json!({"id": id}),
                            Value::Null,
                            move |result| {
                                let bytes = result
                                    .ok()
                                    .and_then(|output| api::unb64(output["data_b64"].as_str()?));
                                let mut events = events.borrow_mut();
                                match bytes {
                                    Some(bytes) => {
                                        events.push_back(Event::Output(id.clone(), bytes));
                                        events.push_back(Event::Exited(id, code));
                                    }
                                    None => {
                                        events.push_back(Event::Error(error));
                                        events.push_back(Event::Closed(id));
                                    }
                                }
                            },
                        );
                        return;
                    }
                    Frame::End(_) => Event::Closed(id.clone()),
                };
                heard.set(true);
                events.borrow_mut().push_back(event);
            });
        self.stream.set(Some((stream, terminal)));
    }
}

impl Transport for Remote {
    fn send(&self, request: Request) {
        match request {
            Request::Refresh => {
                let (portal, host) = (self.portal.clone(), self.host.clone());
                self.call("version", json!({}), Value::Null, move |result, events| {
                    let version = result.and_then(|version| {
                        serde_json::from_value::<api::Version>(version)
                            .map_err(|error| error.to_string())
                    });
                    match version {
                        Ok(version) => {
                            let old = version.api < NEEDS_API;
                            // The server says whether the host is behind what it would apply.
                            let asked = events.clone();
                            portal.call(
                                "hosts.get",
                                None,
                                json!({"name": host}),
                                Value::Null,
                                move |result| {
                                    let behind = result.ok().and_then(|found| {
                                        serde_json::from_value(found.get("behind")?.clone()).ok()
                                    });
                                    asked.borrow_mut().push_back(Event::Behind(behind));
                                },
                            );
                            events.borrow_mut().push_back(Event::Version(version));
                            if old {
                                return;
                            }
                            let again = Remote {
                                portal,
                                host,
                                events: events.clone(),
                                stream: Cell::new(None),
                            };
                            again.call("tasks.list", json!({}), Value::Null, |result, events| {
                                if let Ok(tasks) = result {
                                    events
                                        .borrow_mut()
                                        .push_back(Event::Tasks(list(tasks, "tasks")));
                                }
                            });
                            again.poll();
                        }
                        Err(message) => events.borrow_mut().push_back(Event::Failed {
                            missing: missing(&message),
                            message,
                        }),
                    }
                });
            }
            Request::Poll => self.poll(),
            // A task is the host's action on the server, which keeps a job of it.
            Request::Run { name, body } => {
                let events = self.events.clone();
                let args = json!({"name": self.host, "action": name});
                self.portal
                    .call("hosts.act", None, args, body, move |result| {
                        let session = result.map(|acted| acted["session"].clone());
                        Remote::opened(session, &events)
                    });
            }
            Request::Start { body } => self.call("sessions.start", json!({}), body, Remote::opened),
            Request::Open(session) => {
                if let Some((stream, _)) = self.stream.take() {
                    self.portal.end(stream);
                }
                self.follow(*session);
            }
            Request::Input(bytes) => match self.stream.get() {
                Some((stream, true)) => self.portal.push_bytes(stream, &bytes),
                // A process takes its input with the stream it is for in front.
                Some((stream, false)) => self.portal.push_bytes(stream, &[vec![0], bytes].concat()),
                None => {}
            },
            Request::Resize(rows, cols) => {
                if let Some((stream, _)) = self.stream.get() {
                    self.portal
                        .push(stream, json!({"resize": {"rows": rows, "cols": cols}}));
                }
            }
            Request::Close => {
                if let Some((stream, _)) = self.stream.take() {
                    self.portal.end(stream);
                }
            }
            Request::Delete(id) => {
                let again = Remote {
                    portal: self.portal.clone(),
                    host: self.host.clone(),
                    events: self.events.clone(),
                    stream: Cell::new(None),
                };
                self.call(
                    "sessions.delete",
                    json!({"id": id}),
                    Value::Null,
                    move |result, events| {
                        if let Err(error) = result {
                            events.borrow_mut().push_back(Event::Error(error));
                        }
                        again.poll();
                    },
                );
            }
            Request::Choices { task, argument } => {
                let args = json!({"name": task, "argument": argument});
                self.call("tasks.choices", args, Value::Null, move |result, events| {
                    events.borrow_mut().push_back(Event::Choices {
                        task,
                        argument,
                        choices: result
                            .map(|value| list(value, "choices"))
                            .unwrap_or_default(),
                    });
                });
            }
        }
    }

    fn next(&self) -> Option<Event> {
        self.events.borrow_mut().pop_front()
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        if let Some((stream, _)) = self.stream.take() {
            self.portal.end(stream);
        }
    }
}
