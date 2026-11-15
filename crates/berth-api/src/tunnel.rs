//! A TCP connection to a port of a host, as `ports.connect` gives it, or to
//! an endpoint of a resource, as `resources.connect` does: read and written
//! like a socket, over the stream's own WebSocket or the portal.

use crate::client::{Frame, Result, WebSocketStream, WsError, websocket};
use crate::streams::EOF;
use futures_util::{Sink, Stream};
use std::{
    io,
    pin::Pin,
    task::{Context, Poll, ready},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// The most bytes one frame carries, so a large write does not become one
/// frame that the other side has to hold whole.
const FRAME: usize = 64 * 1024;

/// The bytes of a stream of a port. Shutting down the writing side sends
/// the end, and reading goes on until the other side has sent its own.
/// Dropping it ends the connection.
pub struct Tunnel<S> {
    inner: S,
    /// What was received and not read yet.
    buffer: Vec<u8>,
    at: usize,
    /// The other side said it sends no more.
    read_end: bool,
    /// This side said it sends no more.
    write_end: bool,
}

/// Open a port of the host at the other end of `io`, a connection to its berthd.
pub async fn tunnel<S>(io: S, port: u16) -> Result<Tunnel<WebSocketStream<S>>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let stream = websocket(io, &format!("/v1/ports/{port}"), "").await?;
    Ok(Tunnel::new(stream))
}

/// Open an endpoint of a resource, by its name or its port, through the
/// `berth serve` at the other end of `io`. `protocols` carries a bearer for
/// a server over the network, as the portal's handshake does.
pub async fn endpoint_tunnel<S>(
    io: S,
    id: &str,
    endpoint: &str,
    protocols: &str,
) -> Result<Tunnel<WebSocketStream<S>>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let path = format!(
        "/v1/resources/{}/endpoints/{}",
        crate::client::encode(id),
        crate::client::encode(endpoint)
    );
    let stream = websocket(io, &path, protocols).await?;
    Ok(Tunnel::new(stream))
}

impl<S> Tunnel<S> {
    /// The stream of a port that is already open.
    pub fn new(inner: S) -> Tunnel<S> {
        Tunnel {
            inner,
            buffer: vec![],
            at: 0,
            read_end: false,
            write_end: false,
        }
    }

    pub fn into_inner(self) -> S {
        self.inner
    }
}

fn broken(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

fn said_end(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text).is_ok_and(|frame| frame["eof"] == true)
}

impl<S> AsyncRead for Tunnel<S>
where
    S: Stream<Item = std::result::Result<Frame, WsError>> + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        out: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = &mut *self;
        loop {
            if this.at < this.buffer.len() {
                let count = out.remaining().min(this.buffer.len() - this.at);
                out.put_slice(&this.buffer[this.at..this.at + count]);
                this.at += count;
                return Poll::Ready(Ok(()));
            }
            if this.read_end {
                return Poll::Ready(Ok(()));
            }
            match ready!(Pin::new(&mut this.inner).poll_next(cx)) {
                Some(Ok(Frame::Binary(bytes))) => {
                    this.buffer = bytes.to_vec();
                    this.at = 0;
                }
                Some(Ok(Frame::Text(text))) if said_end(text.as_str()) => this.read_end = true,
                Some(Ok(_)) => {}
                Some(Err(error)) => return Poll::Ready(Err(broken(error))),
                // Ended without saying so first: the connection was lost, not closed.
                None => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "The connection to the port ended before its end",
                    )));
                }
            }
        }
    }
}

impl<S> AsyncWrite for Tunnel<S>
where
    S: Sink<Frame, Error = WsError> + Unpin,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.write_end {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        ready!(Pin::new(&mut self.inner).poll_ready(cx)).map_err(broken)?;
        let count = data.len().min(FRAME);
        let frame = Frame::Binary(data[..count].to_vec().into());
        Pin::new(&mut self.inner)
            .start_send(frame)
            .map_err(broken)?;
        Poll::Ready(Ok(count))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx).map_err(broken)
    }

    /// Say the end and leave the other direction open.
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if !self.write_end {
            ready!(Pin::new(&mut self.inner).poll_ready(cx)).map_err(broken)?;
            Pin::new(&mut self.inner)
                .start_send(Frame::Text(EOF.into()))
                .map_err(broken)?;
            self.write_end = true;
        }
        Pin::new(&mut self.inner).poll_flush(cx).map_err(broken)
    }
}
