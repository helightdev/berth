//! The server's portal over a WebSocket of this program: calls that are
//! answered once, and streams that send frames until either side ends them.
//! The same as the page's, so its screens work unchanged: the socket runs on
//! the runtime and what it says is handed to the main thread at [`pump`].

use crate::net;
use berth_api::client::{self, Frame as Message};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::mpsc;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

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

/// What the socket said, for the main thread.
enum Said {
    Open,
    Text(String),
    Bytes(Vec<u8>),
    Closed,
}

type Answer = Box<dyn FnOnce(Result<Value, String>)>;
type Listener = Rc<dyn Fn(Frame)>;

struct Inner {
    /// Frames to send; dropping it closes the socket.
    out: UnboundedSender<Message>,
    said: mpsc::Receiver<Said>,
    link: Cell<Link>,
    next: Cell<u32>,
    calls: RefCell<HashMap<u32, Answer>>,
    streams: RefCell<HashMap<u32, Listener>>,
}

#[derive(Clone)]
pub struct Portal(Rc<Inner>);

const GONE: &str = "The connection to the berth server was lost";

/// Frames taken in per portal between two draws.
const TURN: usize = 512;

/// How long a connection and its handshake may take.
const CONNECT: std::time::Duration = std::time::Duration::from_secs(20);

thread_local! {
    static OPEN: RefCell<Vec<Weak<Inner>>> = RefCell::default();
}

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

    /// Take in what the socket said since the last time, up to [`TURN`]
    /// frames: a terminal that floods keeps the screen drawn and the keys read.
    fn take_in(&self) {
        for said in self.said.try_iter().take(TURN) {
            match said {
                Said::Open => self.link.set(Link::Open),
                Said::Text(text) => self.text(&text),
                Said::Bytes(bytes) => {
                    if let Some((id, rest)) = bytes.split_first_chunk::<4>() {
                        self.frame(u32::from_be_bytes(*id), Frame::Bytes(rest.to_vec()));
                    }
                }
                Said::Closed => self.closed(),
            }
        }
    }

    /// Frames sent before the socket is open wait in the channel.
    fn send(&self, frame: Value) -> bool {
        match self.link.get() {
            Link::Connecting | Link::Open => {
                self.out.send(Message::text(frame.to_string())).is_ok()
            }
            _ => false,
        }
    }

    fn id(&self) -> u32 {
        let id = self.next.get();
        self.next.set(id.wrapping_add(1).max(1));
        id
    }
}

/// Hand what every open portal's socket said to its listeners.
pub fn pump() {
    let open: Vec<Rc<Inner>> = OPEN.with(|open| {
        let mut open = open.borrow_mut();
        open.retain(|portal| portal.strong_count() > 0);
        open.iter().filter_map(Weak::upgrade).collect()
    });
    for portal in open {
        portal.take_in();
    }
}

fn request(id: u32, op: &str, host: Option<&str>, args: Value, data: Value) -> Value {
    let mut frame = json!({"id": id, "op": op, "args": args, "data": data});
    if let Some(host) = host {
        frame["host"] = json!(host);
    }
    frame
}

/// The socket, from its handshake to its end.
async fn run(
    url: String,
    token: String,
    mut out: tokio::sync::mpsc::UnboundedReceiver<Message>,
    said: mpsc::Sender<Said>,
) {
    let opened = async {
        let target = net::target(&url)?;
        let stream = net::connect(&target).await?;
        // The token goes as the protocol, the way the page sends it.
        client::websocket_at(stream, &target.authority, &target.path, &token)
            .await
            .map_err(|error| net::describe(&error))
    };
    let opened = tokio::select! {
        opened = tokio::time::timeout(CONNECT, opened) => opened.ok(),
        // The portal was dropped while it was still connecting.
        _ = async {
            while !out.is_closed() {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        } => return,
    };
    let socket = match opened {
        Some(Ok(socket)) => socket,
        _ => {
            let _ = said.send(Said::Closed);
            return;
        }
    };
    let _ = said.send(Said::Open);
    let (mut write, mut read) = socket.split();
    loop {
        tokio::select! {
            frame = out.recv() => match frame {
                Some(frame) => {
                    if write.send(frame).await.is_err() {
                        break;
                    }
                }
                // The portal is gone: close the socket the polite way.
                None => {
                    let _ = write.close().await;
                    return;
                }
            },
            frame = read.next() => match frame {
                Some(Ok(Message::Text(text))) => {
                    let _ = said.send(Said::Text(text.to_string()));
                }
                Some(Ok(Message::Binary(bytes))) => {
                    let _ = said.send(Said::Bytes(bytes.to_vec()));
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
        }
    }
    let _ = said.send(Said::Closed);
}

impl Portal {
    /// Open the portal at `url` with `token`.
    pub fn open(url: &str, token: &str) -> Result<Portal, String> {
        net::target(url)?;
        let (out, outgoing) = unbounded_channel();
        let (said, heard) = mpsc::channel();
        net::runtime().spawn(run(url.to_owned(), token.to_owned(), outgoing, said));
        let inner = Rc::new(Inner {
            out,
            said: heard,
            link: Cell::new(Link::Connecting),
            next: Cell::new(1),
            calls: RefCell::default(),
            streams: RefCell::default(),
        });
        OPEN.with(|open| open.borrow_mut().push(Rc::downgrade(&inner)));
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
            let _ = self.0.out.send(Message::binary(frame));
        }
    }

    /// End a stream from this side; nothing more of it is listened to.
    pub fn end(&self, id: u32) {
        if self.0.streams.borrow_mut().remove(&id).is_some() {
            self.0.send(json!({"sub": id, "end": true}));
        }
    }
}
