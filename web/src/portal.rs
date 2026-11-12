//! The server's portal over the browser's WebSocket: calls that are answered
//! once, and streams that send frames until either side ends them.

use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use wasm_bindgen::{JsCast, prelude::*};
use web_sys::{BinaryType, MessageEvent, WebSocket};

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
/// What the page does when the socket says something.
type Handler = Box<dyn Fn(&Inner, JsValue)>;

struct Inner {
    socket: WebSocket,
    link: Cell<Link>,
    next: Cell<u32>,
    /// Frames asked for before the socket was open.
    waiting: RefCell<Vec<String>>,
    calls: RefCell<HashMap<u32, Answer>>,
    streams: RefCell<HashMap<u32, Listener>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        for name in ["onopen", "onmessage", "onclose"] {
            let _ = js_sys::Reflect::set(&self.socket, &name.into(), &JsValue::NULL);
        }
        let _ = self.socket.close();
    }
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

    fn send(&self, frame: Value) -> bool {
        let text = frame.to_string();
        match self.link.get() {
            Link::Connecting => {
                self.waiting.borrow_mut().push(text);
                true
            }
            Link::Open => self.socket.send_with_str(&text).is_ok(),
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
        let socket = match token.is_empty() {
            true => WebSocket::new(url),
            false => WebSocket::new_with_str(url, token),
        }
        .map_err(|_| format!("{url} is no address a browser can open"))?;
        socket.set_binary_type(BinaryType::Arraybuffer);
        let inner = Rc::new(Inner {
            socket,
            link: Cell::new(Link::Connecting),
            next: Cell::new(1),
            waiting: RefCell::default(),
            calls: RefCell::default(),
            streams: RefCell::default(),
        });
        let listen = |name: &str, handler: Handler| {
            let weak: Weak<Inner> = Rc::downgrade(&inner);
            let closure = Closure::<dyn FnMut(JsValue)>::new(move |event: JsValue| {
                if let Some(inner) = weak.upgrade() {
                    handler(&inner, event);
                }
            });
            let _ = js_sys::Reflect::set(&inner.socket, &name.into(), closure.as_ref());
            // The socket keeps it for as long as the page has it.
            closure.forget();
        };
        listen(
            "onopen",
            Box::new(|inner, _| {
                inner.link.set(Link::Open);
                for text in inner.waiting.borrow_mut().drain(..) {
                    let _ = inner.socket.send_with_str(&text);
                }
            }),
        );
        listen("onclose", Box::new(|inner, _| inner.closed()));
        listen(
            "onmessage",
            Box::new(|inner, event| {
                let Ok(event) = event.dyn_into::<MessageEvent>() else {
                    return;
                };
                let data = event.data();
                if let Some(text) = data.as_string() {
                    inner.text(&text);
                } else if data.is_instance_of::<js_sys::ArrayBuffer>() {
                    let bytes = js_sys::Uint8Array::new(&data).to_vec();
                    if let Some((id, rest)) = bytes.split_first_chunk::<4>() {
                        inner.frame(u32::from_be_bytes(*id), Frame::Bytes(rest.to_vec()));
                    }
                }
            }),
        );
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
            let _ = self.0.socket.send_with_u8_array(&frame);
        }
    }

    /// End a stream from this side; nothing more of it is listened to.
    pub fn end(&self, id: u32) {
        if self.0.streams.borrow_mut().remove(&id).is_some() {
            self.0.send(json!({"sub": id, "end": true}));
        }
    }
}
