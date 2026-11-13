//! The server's portal over the device's WebSocket: calls that are answered
//! once, and streams that send frames until either side ends them.

use crate::device::{self, Said, Socket};
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};

/// How the connection is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    Connecting,
    Open,
    /// It closed before it was open: a wrong token, or nobody there.
    Refused,
    /// It was open and is not any more.
    Lost,
}

/// A frame of a stream.
pub enum Frame {
    Data(Value),
    Bytes(Vec<u8>),
    /// The last one, with why if it ended badly.
    End(Option<String>),
}

type Answer = Box<dyn FnOnce(Result<Value, String>)>;
type Listener = Rc<dyn Fn(Frame)>;

struct Inner {
    socket: RefCell<Option<Box<dyn Socket>>>,
    link: Cell<Link>,
    next: Cell<u32>,
    /// Frames asked for before the socket was open.
    waiting: RefCell<Vec<String>>,
    calls: RefCell<HashMap<u32, Answer>>,
    streams: RefCell<HashMap<u32, Listener>>,
}

#[derive(Clone)]
pub struct Portal(Rc<Inner>);

const GONE: &str = "The connection to the berth server was lost";

fn message_of(error: &Value) -> String {
    match error["message"].as_str() {
        Some(message) if !message.is_empty() => message.to_owned(),
        _ => match error["status"].as_u64() {
            Some(status) => format!("HTTP {status}"),
            None => "The server gave no reason".to_owned(),
        },
    }
}

impl Inner {
    fn answer(&self, id: u32, result: Result<Value, String>) {
        let waiting = self.calls.borrow_mut().remove(&id);
        if let Some(answer) = waiting {
            answer(result);
        }
    }

    fn frame(&self, id: u32, frame: Frame) {
        let last = matches!(frame, Frame::End(_));
        let listener = match last {
            true => self.streams.borrow_mut().remove(&id),
            false => self.streams.borrow().get(&id).cloned(),
        };
        if let Some(listener) = listener {
            listener(frame);
        }
    }

    fn said(&self, said: Said) {
        match said {
            Said::Open => {
                self.link.set(Link::Open);
                let waiting = std::mem::take(&mut *self.waiting.borrow_mut());
                for text in waiting {
                    self.send_now(&text);
                }
            }
            Said::Text(text) => self.text(&text),
            Said::Bytes(bytes) => {
                if let Some((id, rest)) = bytes.split_first_chunk::<4>() {
                    self.frame(u32::from_be_bytes(*id), Frame::Bytes(rest.to_vec()));
                }
            }
            Said::Closed => self.closed(),
        }
    }

    fn text(&self, text: &str) {
        let Ok(frame) = serde_json::from_str::<Value>(text) else {
            return;
        };
        if let Some(id) = frame["id"].as_u64() {
            let result = match frame.get("error") {
                Some(error) => Err(message_of(error)),
                None => Ok(frame["result"].clone()),
            };
            self.answer(id as u32, result);
        } else if let Some(id) = frame["sub"].as_u64() {
            let id = id as u32;
            if frame["end"].as_bool() == Some(true) {
                self.frame(id, Frame::End(frame.get("error").map(message_of)));
            } else {
                self.frame(id, Frame::Data(frame["data"].clone()));
            }
        }
    }

    fn closed(&self) {
        self.link.set(match self.link.get() {
            Link::Open | Link::Lost => Link::Lost,
            _ => Link::Refused,
        });
        let calls: Vec<Answer> = self
            .calls
            .borrow_mut()
            .drain()
            .map(|(_, call)| call)
            .collect();
        for call in calls {
            call(Err(GONE.to_owned()));
        }
        let streams: Vec<Listener> = self
            .streams
            .borrow_mut()
            .drain()
            .map(|(_, stream)| stream)
            .collect();
        for stream in streams {
            stream(Frame::End(Some(GONE.to_owned())));
        }
    }

    fn send_now(&self, text: &str) -> bool {
        self.socket
            .borrow()
            .as_ref()
            .is_some_and(|socket| socket.send_text(text))
    }

    fn send(&self, frame: Value) -> bool {
        let text = frame.to_string();
        match self.link.get() {
            Link::Connecting => {
                self.waiting.borrow_mut().push(text);
                true
            }
            Link::Open => self.send_now(&text),
            _ => false,
        }
    }

    fn id(&self) -> u32 {
        let id = self.next.get();
        self.next.set(id.wrapping_add(1).max(1));
        id
    }
}

fn request(id: u32, op: &str, host: Option<&str>, args: Value, data: Value) -> Value {
    let mut frame = json!({"id": id, "op": op, "args": args, "data": data});
    if let Some(host) = host {
        frame["host"] = json!(host);
    }
    frame
}

impl Portal {
    /// Open the portal at `url`, showing the token the way a browser can: as
    /// the WebSocket's protocol.
    pub fn open(url: &str, token: &str) -> Result<Portal, String> {
        let inner = Rc::new(Inner {
            socket: RefCell::default(),
            link: Cell::new(Link::Connecting),
            next: Cell::new(1),
            waiting: RefCell::default(),
            calls: RefCell::default(),
            streams: RefCell::default(),
        });
        let weak: Weak<Inner> = Rc::downgrade(&inner);
        let socket = device::device().connect(
            url,
            token,
            Box::new(move |said| {
                if let Some(inner) = weak.upgrade() {
                    inner.said(said);
                }
            }),
        )?;
        *inner.socket.borrow_mut() = Some(socket);
        Ok(Portal(inner))
    }

    pub fn link(&self) -> Link {
        self.0.link.get()
    }

    /// Call an operation; `then` gets its result, or why there is none.
    pub fn call(
        &self,
        op: &str,
        host: Option<&str>,
        args: Value,
        data: Value,
        then: impl FnOnce(Result<Value, String>) + 'static,
    ) {
        let id = self.0.id();
        self.0.calls.borrow_mut().insert(id, Box::new(then));
        if !self.0.send(request(id, op, host, args, data)) {
            self.0.answer(id, Err(GONE.to_owned()));
        }
    }

    /// Open a stream; `on` gets its frames, the last of which is `End`.
    /// Returns the number to send to it and to end it with.
    pub fn stream(
        &self,
        op: &str,
        host: Option<&str>,
        args: Value,
        on: impl Fn(Frame) + 'static,
    ) -> u32 {
        let id = self.0.id();
        self.0.streams.borrow_mut().insert(id, Rc::new(on));
        let weak = Rc::downgrade(&self.0);
        // The answer only matters when it says the stream did not open.
        self.0.calls.borrow_mut().insert(
            id,
            Box::new(move |result| {
                if let (Err(error), Some(inner)) = (result, weak.upgrade()) {
                    inner.frame(id, Frame::End(Some(error)));
                }
            }),
        );
        if !self.0.send(request(id, op, host, args, Value::Null)) {
            self.0.answer(id, Err(GONE.to_owned()));
        }
        id
    }

    pub fn push(&self, id: u32, data: Value) {
        self.0.send(json!({"sub": id, "data": data}));
    }

    pub fn push_bytes(&self, id: u32, bytes: &[u8]) {
        if self.0.link.get() == Link::Open {
            let frame = [&id.to_be_bytes()[..], bytes].concat();
            if let Some(socket) = self.0.socket.borrow().as_ref() {
                socket.send_bytes(&frame);
            }
        }
    }

    /// End a stream from this side; nothing more of it is listened to.
    pub fn end(&self, id: u32) {
        if self.0.streams.borrow_mut().remove(&id).is_some() {
            self.0.send(json!({"sub": id, "end": true}));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::{Answer, Device, Kept, Linked};

    type Hears = Box<dyn Fn(Said)>;

    /// A device whose socket the test speaks for.
    #[derive(Clone, Default)]
    struct Wire {
        sent: Rc<RefCell<Vec<String>>>,
        said: Rc<RefCell<Option<Hears>>>,
    }

    struct WireSocket(Rc<RefCell<Vec<String>>>);

    impl Socket for WireSocket {
        fn send_text(&self, text: &str) -> bool {
            self.0.borrow_mut().push(text.to_owned());
            true
        }

        fn send_bytes(&self, bytes: &[u8]) -> bool {
            self.0.borrow_mut().push(format!("{bytes:?}"));
            true
        }
    }

    impl Wire {
        fn say(&self, said: Said) {
            (self.said.borrow().as_ref().expect("a socket is open"))(said)
        }

        fn sent(&self) -> Vec<Value> {
            let sent = self.sent.borrow();
            sent.iter()
                .map(|text| serde_json::from_str(text).unwrap())
                .collect()
        }
    }

    impl Device for Wire {
        fn copy(&self, _: &str) -> bool {
            false
        }
        fn paste(&self) -> Option<String> {
            None
        }
        fn open(&self, _: &str) -> Result<(), String> {
            Err("no".into())
        }
        fn choose_file(&self, _: &str, _: Box<dyn FnOnce(String, Vec<u8>)>) -> Result<(), String> {
            Err("no".into())
        }
        fn stored(&self, _: Kept) -> Option<String> {
            None
        }
        fn store(&self, _: Kept, _: &str) {}
        fn embedded(&self) -> bool {
            false
        }
        fn linked(&self) -> Linked {
            Linked::default()
        }
        fn origin(&self) -> String {
            String::new()
        }
        fn page(&self) -> String {
            String::new()
        }
        fn request(&self, _: &str, _: &str, _: &str, then: Answer) {
            then(Err("no".into()))
        }
        fn connect(
            &self,
            url: &str,
            token: &str,
            said: Box<dyn Fn(Said)>,
        ) -> Result<Box<dyn Socket>, String> {
            assert_eq!(
                (url, token),
                ("ws://192.0.2.10:7878/v1/portal", "the-token")
            );
            *self.said.borrow_mut() = Some(said);
            Ok(Box::new(WireSocket(self.sent.clone())))
        }
    }

    fn portal() -> (Wire, Portal) {
        let wire = Wire::default();
        device::install(wire.clone());
        let portal = Portal::open("ws://192.0.2.10:7878/v1/portal", "the-token").unwrap();
        (wire, portal)
    }

    #[test]
    fn calls_wait_for_the_socket_and_are_answered() {
        let (wire, portal) = portal();
        let answer = Rc::new(RefCell::new(None));
        let got = answer.clone();
        portal.call("hosts", None, json!({}), Value::Null, move |result| {
            *got.borrow_mut() = Some(result)
        });
        assert!(wire.sent().is_empty());
        wire.say(Said::Open);
        assert_eq!(portal.link(), Link::Open);
        let sent = wire.sent();
        assert_eq!(
            (sent[0]["id"].as_u64(), sent[0]["op"].as_str()),
            (Some(1), Some("hosts"))
        );
        wire.say(Said::Text(r#"{"id": 1, "result": ["studio"]}"#.into()));
        assert_eq!(answer.borrow_mut().take(), Some(Ok(json!(["studio"]))));
    }

    #[test]
    fn streams_end_when_the_socket_closes() {
        let (wire, portal) = portal();
        let frames = Rc::new(RefCell::new(vec![]));
        let got = frames.clone();
        wire.say(Said::Open);
        let id = portal.stream("session", Some("studio"), json!({}), move |frame| {
            got.borrow_mut().push(match frame {
                Frame::Data(data) => data.to_string(),
                Frame::Bytes(bytes) => String::from_utf8(bytes).unwrap(),
                Frame::End(why) => format!("end {}", why.unwrap_or_default()),
            })
        });
        assert_eq!(wire.sent()[0]["host"], "studio");
        wire.say(Said::Text(format!(r#"{{"sub": {id}, "data": 7}}"#)));
        wire.say(Said::Bytes([&id.to_be_bytes()[..], b"hi"].concat()));
        wire.say(Said::Closed);
        assert_eq!(portal.link(), Link::Lost);
        assert_eq!(*frames.borrow(), ["7", "hi", &format!("end {GONE}")]);
        // A call after the end is answered at once.
        let answer = Rc::new(RefCell::new(None));
        let got = answer.clone();
        portal.call("hosts", None, json!({}), Value::Null, move |result| {
            *got.borrow_mut() = Some(result)
        });
        assert_eq!(answer.borrow_mut().take(), Some(Err(GONE.to_owned())));
    }

    #[test]
    fn a_socket_that_never_opens_is_refused() {
        let (wire, portal) = portal();
        wire.say(Said::Closed);
        assert_eq!(portal.link(), Link::Refused);
    }
}
