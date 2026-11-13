//! The WebSocket under the page's portal: it runs on the runtime, and what it
//! says is handed to the main thread at [`pump`].

use crate::net;
use berth_api::client::{self, Frame as Message};
use berth_page::device::{Said, Socket};
use futures_util::{SinkExt, StreamExt};
use std::cell::RefCell;
use std::sync::mpsc;
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// What a socket says goes to its listener here, a turn at a time.
struct Heard {
    said: mpsc::Receiver<Said>,
    to: Box<dyn Fn(Said)>,
}

thread_local! {
    static OPEN: RefCell<Vec<Heard>> = RefCell::default();
}

/// What is taken in per socket between two draws: a session that floods
/// still lets the screen be drawn and the keys be read.
const TURN: usize = 512;

/// How long a connection and its handshake may take.
const CONNECT: Duration = Duration::from_secs(20);

pub struct NativeSocket {
    /// Frames to send; dropping it closes the socket.
    out: UnboundedSender<Message>,
}

impl Socket for NativeSocket {
    fn send_text(&self, text: &str) -> bool {
        self.out.send(Message::text(text.to_owned())).is_ok()
    }

    fn send_bytes(&self, bytes: &[u8]) -> bool {
        self.out.send(Message::binary(bytes.to_vec())).is_ok()
    }
}

/// Open a WebSocket to `url`, with `token` as its protocol, the way the page
/// sends it.
pub fn open(url: &str, token: &str, to: Box<dyn Fn(Said)>) -> Result<NativeSocket, String> {
    let target = net::target(url)?;
    let (out, outgoing) = unbounded_channel();
    let (said, heard) = mpsc::channel();
    net::runtime().spawn(run(target, token.to_owned(), outgoing, said));
    OPEN.with_borrow_mut(|open| open.push(Heard { said: heard, to }));
    Ok(NativeSocket { out })
}

/// Hand what every socket said since the last turn to its listener.
pub fn pump() {
    // A listener may open another socket, which adds to the list.
    let mut taken = OPEN.with_borrow_mut(std::mem::take);
    taken.retain(|heard| {
        for _ in 0..TURN {
            match heard.said.try_recv() {
                Ok(said) => (heard.to)(said),
                Err(mpsc::TryRecvError::Empty) => return true,
                Err(mpsc::TryRecvError::Disconnected) => return false,
            }
        }
        true
    });
    OPEN.with_borrow_mut(|open| {
        taken.append(open);
        *open = taken;
    });
}

/// The socket, from its handshake to its end.
async fn run(
    target: net::Target,
    token: String,
    mut out: UnboundedReceiver<Message>,
    said: mpsc::Sender<Said>,
) {
    let opened = async {
        let stream = net::connect(&target).await?;
        client::websocket_at(stream, &target.authority, &target.path, &token)
            .await
            .map_err(|error| net::describe(&error))
    };
    let opened = tokio::select! {
        opened = tokio::time::timeout(CONNECT, opened) => opened.ok(),
        // The portal was dropped while it was still connecting.
        _ = async {
            while !out.is_closed() {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        } => return,
    };
    let Some(Ok(socket)) = opened else {
        let _ = said.send(Said::Closed);
        return;
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
