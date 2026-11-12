//! The client's side of the portal: any number of calls and streams over one
//! WebSocket. The frames are described in [`crate::ops`].

use crate::client::{Error, Result};
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    },
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{mpsc, oneshot},
};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

/// One frame of a stream.
#[derive(Debug, Clone, PartialEq)]
pub enum Piece {
    Text(Value),
    Binary(Vec<u8>),
}

enum Event {
    Piece(Piece),
    End(Option<Error>),
}

#[derive(Default)]
struct Shared {
    next: AtomicU32,
    calls: Mutex<HashMap<u32, oneshot::Sender<Result<Value>>>>,
    streams: Mutex<HashMap<u32, mpsc::UnboundedSender<Event>>>,
    /// Why the connection ended, once it has.
    closed: Mutex<Option<String>>,
}

impl Shared {
    /// Fail everything that still waits.
    fn close(&self, reason: &str) {
        *self.closed.lock().unwrap() = Some(reason.to_owned());
        for (_, call) in self.calls.lock().unwrap().drain() {
            let _ = call.send(Err(Error::Transport(reason.to_owned())));
        }
        for (_, stream) in self.streams.lock().unwrap().drain() {
            let _ = stream.send(Event::End(Some(Error::Transport(reason.to_owned()))));
        }
    }
}

/// A binary frame of stream `id`.
pub fn tagged(id: u32, bytes: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(bytes.len() + 4);
    frame.extend_from_slice(&id.to_be_bytes());
    frame.extend_from_slice(bytes);
    frame
}

/// The stream a binary frame belongs to, and its bytes.
pub fn untagged(frame: &[u8]) -> Option<(u32, &[u8])> {
    let (id, bytes) = frame.split_first_chunk::<4>()?;
    Some((u32::from_be_bytes(*id), bytes))
}

fn error_of(value: &Value) -> Error {
    Error::Api {
        status: value["status"].as_u64().unwrap_or(500) as u16,
        code: value["code"].as_str().unwrap_or("error").to_owned(),
        message: value["message"].as_str().unwrap_or("").to_owned(),
    }
}

#[derive(Clone)]
pub struct Portal {
    out: mpsc::UnboundedSender<Message>,
    shared: Arc<Shared>,
}

impl Portal {
    /// Take over an open WebSocket to `/v1/portal`.
    pub fn over<S>(socket: WebSocketStream<S>) -> Portal
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (mut sink, mut source) = socket.split();
        let (out, mut outgoing) = mpsc::unbounded_channel::<Message>();
        let shared = Arc::new(Shared::default());
        tokio::spawn(async move {
            while let Some(message) = outgoing.recv().await {
                if sink.send(message).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });
        let reader = shared.clone();
        tokio::spawn(async move {
            let reason = loop {
                match source.next().await {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(frame) = serde_json::from_str::<Value>(text.as_str()) {
                            reader.take(frame);
                        }
                    }
                    Some(Ok(Message::Binary(bytes))) => {
                        if let Some((id, bytes)) = untagged(&bytes)
                            && let Some(stream) = reader.streams.lock().unwrap().get(&id)
                        {
                            let _ = stream.send(Event::Piece(Piece::Binary(bytes.to_vec())));
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break "The connection closed".to_owned(),
                    Some(Ok(_)) => {}
                    Some(Err(error)) => break error.to_string(),
                }
            };
            reader.close(&reason);
        });
        Portal { out, shared }
    }

    /// Whether the connection is still there.
    pub fn usable(&self) -> bool {
        self.shared.closed.lock().unwrap().is_none() && !self.out.is_closed()
    }

    fn request(
        &self,
        op: &str,
        host: Option<&str>,
        args: Value,
        data: Option<Value>,
    ) -> Result<(u32, oneshot::Receiver<Result<Value>>)> {
        if let Some(reason) = self.shared.closed.lock().unwrap().clone() {
            return Err(Error::Transport(reason));
        }
        let id = self
            .shared
            .next
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let (answer, waiting) = oneshot::channel();
        self.shared.calls.lock().unwrap().insert(id, answer);
        let mut frame = json!({"id": id, "op": op});
        if let Some(host) = host {
            frame["host"] = json!(host);
        }
        if !args.is_null() {
            frame["args"] = args;
        }
        if let Some(data) = data {
            frame["data"] = data;
        }
        if self
            .out
            .send(Message::Text(frame.to_string().into()))
            .is_err()
        {
            self.shared.calls.lock().unwrap().remove(&id);
            return Err(Error::Transport("The connection closed".into()));
        }
        Ok((id, waiting))
    }

    /// One operation and its result.
    pub async fn call(
        &self,
        op: &str,
        host: Option<&str>,
        args: Value,
        data: Option<Value>,
    ) -> Result<Value> {
        let (_, waiting) = self.request(op, host, args, data)?;
        waiting
            .await
            .unwrap_or_else(|_| Err(Error::Transport("The connection closed".into())))
    }

    /// Open a streaming operation.
    pub async fn open(&self, op: &str, host: Option<&str>, args: Value) -> Result<PortalStream> {
        let (frames, incoming) = mpsc::unbounded_channel();
        let (id, waiting) = {
            // Registered before the request leaves, so no frame can come too early.
            let mut streams = self.shared.streams.lock().unwrap();
            let (id, waiting) = self.request(op, host, args, None)?;
            streams.insert(id, frames);
            (id, waiting)
        };
        let opened = waiting
            .await
            .unwrap_or_else(|_| Err(Error::Transport("The connection closed".into())));
        if let Err(error) = opened {
            self.shared.streams.lock().unwrap().remove(&id);
            return Err(error);
        }
        Ok(PortalStream {
            id,
            out: self.out.clone(),
            incoming,
            shared: self.shared.clone(),
            ended: false,
        })
    }
}

impl Shared {
    fn take(&self, frame: Value) {
        if let Some(id) = frame["id"].as_u64() {
            if let Some(call) = self.calls.lock().unwrap().remove(&(id as u32)) {
                let _ = call.send(match frame.get("error") {
                    Some(error) => Err(error_of(error)),
                    None => Ok(frame["result"].clone()),
                });
            }
        } else if let Some(id) = frame["sub"].as_u64() {
            let id = id as u32;
            let mut streams = self.streams.lock().unwrap();
            if frame["end"].as_bool() == Some(true) {
                if let Some(stream) = streams.remove(&id) {
                    let _ = stream.send(Event::End(frame.get("error").map(error_of)));
                }
            } else if let Some(stream) = streams.get(&id) {
                let _ = stream.send(Event::Piece(Piece::Text(frame["data"].clone())));
            }
        }
    }
}

/// A streaming operation while it is open. Dropping it ends it.
pub struct PortalStream {
    id: u32,
    out: mpsc::UnboundedSender<Message>,
    incoming: mpsc::UnboundedReceiver<Event>,
    shared: Arc<Shared>,
    ended: bool,
}

impl PortalStream {
    /// The next frame; `None` once the stream has ended.
    pub async fn recv(&mut self) -> Option<Result<Piece>> {
        std::future::poll_fn(|cx| self.poll_piece(cx)).await
    }

    fn poll_piece(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<Piece>>> {
        if self.ended {
            return Poll::Ready(None);
        }
        match self.incoming.poll_recv(cx) {
            Poll::Ready(Some(Event::Piece(piece))) => Poll::Ready(Some(Ok(piece))),
            Poll::Ready(Some(Event::End(error))) => {
                self.ended = true;
                Poll::Ready(error.map(Err))
            }
            Poll::Ready(None) => {
                self.ended = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    pub fn push(&self, piece: Piece) -> Result<()> {
        let message = match piece {
            Piece::Text(data) => {
                Message::Text(json!({"sub": self.id, "data": data}).to_string().into())
            }
            Piece::Binary(bytes) => Message::Binary(tagged(self.id, &bytes).into()),
        };
        self.out
            .send(message)
            .map_err(|_| Error::Transport("The connection closed".into()))
    }
}

impl Drop for PortalStream {
    fn drop(&mut self) {
        if self
            .shared
            .streams
            .lock()
            .unwrap()
            .remove(&self.id)
            .is_some()
        {
            let end = json!({"sub": self.id, "end": true}).to_string();
            let _ = self.out.send(Message::Text(end.into()));
        }
    }
}

type WsError = tokio_tungstenite::tungstenite::Error;

fn ws_error(error: impl std::fmt::Display) -> WsError {
    WsError::Io(std::io::Error::other(error.to_string()))
}

/// As the WebSocket the stream would be on its own: text frames are the
/// stream's JSON, binary frames its bytes.
impl Stream for PortalStream {
    type Item = std::result::Result<Message, WsError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.poll_piece(cx).map(|piece| {
            piece.map(|piece| match piece {
                Ok(Piece::Text(data)) => Ok(Message::Text(data.to_string().into())),
                Ok(Piece::Binary(bytes)) => Ok(Message::Binary(bytes.into())),
                Err(error) => Err(ws_error(error)),
            })
        })
    }
}

impl Sink<Message> for PortalStream {
    type Error = WsError;

    fn poll_ready(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), WsError>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, message: Message) -> std::result::Result<(), WsError> {
        let piece = match message {
            Message::Text(text) => Piece::Text(
                serde_json::from_str(text.as_str()).unwrap_or_else(|_| json!(text.as_str())),
            ),
            Message::Binary(bytes) => Piece::Binary(bytes.to_vec()),
            _ => return Ok(()),
        };
        self.push(piece).map_err(ws_error)
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), WsError>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), WsError>> {
        Poll::Ready(Ok(()))
    }
}
