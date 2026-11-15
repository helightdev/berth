//! A client for berthd and `berth serve` over any byte stream: a unix socket,
//! or the standard input and output of `ssh … berthd connect`.

use crate::{ErrorBody, code};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::client::conn::http1::SendRequest;
use hyper_util::rt::TokioIo;
use serde::de::DeserializeOwned;
use serde_json::Value;
#[cfg(unix)]
use std::path::Path;
use tokio::io::{AsyncRead, AsyncWrite};
pub use tokio_tungstenite::WebSocketStream;
pub use tokio_tungstenite::tungstenite::Error as WsError;
pub use tokio_tungstenite::tungstenite::Message as Frame;

#[derive(Debug, Clone)]
pub enum Error {
    /// The other side answered with an error.
    Api {
        status: u16,
        code: String,
        message: String,
    },
    /// It could not be reached, or the connection broke.
    Transport(String),
}

impl Error {
    pub fn api(kind: (u16, &str), message: impl Into<String>) -> Error {
        Error::Api {
            status: kind.0,
            code: kind.1.into(),
            message: message.into(),
        }
    }

    pub fn status(&self) -> u16 {
        match self {
            Error::Api { status, .. } => *status,
            Error::Transport(_) => code::UNREACHABLE.0,
        }
    }

    pub fn code(&self) -> &str {
        match self {
            Error::Api { code, .. } => code,
            Error::Transport(_) => code::UNREACHABLE.1,
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Api { message, .. } => f.write_str(message),
            Error::Transport(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {}

fn transport(error: impl std::fmt::Display) -> Error {
    Error::Transport(error.to_string())
}

pub type Result<T> = std::result::Result<T, Error>;

/// A connection to a server, whatever carries it: TCP, TLS, a unix socket,
/// or a pipe to a server in the same process.
pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
pub type Stream = Box<dyn Io>;

/// How a client reaches its server: each call opens a new connection, for a
/// request or for a WebSocket.
pub type Dial = std::sync::Arc<
    dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Stream>> + Send>>
        + Send
        + Sync,
>;

/// A raw answer: status, headers and body.
pub struct Response {
    pub status: u16,
    pub headers: hyper::HeaderMap,
    pub body: Bytes,
}

impl Response {
    /// The body as JSON, or the error it carries.
    pub fn json<T: DeserializeOwned>(self) -> Result<T> {
        if self.status >= 400 {
            return Err(self.error());
        }
        let body: &[u8] = if self.body.is_empty() {
            b"null"
        } else {
            &self.body
        };
        serde_json::from_slice(body).map_err(transport)
    }

    pub fn error(&self) -> Error {
        match serde_json::from_slice::<ErrorBody>(&self.body) {
            Ok(body) => Error::Api {
                status: self.status,
                code: body.error.code,
                message: body.error.message,
            },
            Err(_) => {
                let said = String::from_utf8_lossy(&self.body).trim().to_owned();
                Error::Api {
                    status: self.status,
                    code: "error".into(),
                    message: if said.is_empty() {
                        format!("HTTP {}", self.status)
                    } else {
                        said
                    },
                }
            }
        }
    }
}

/// One connection, for one request at a time.
pub struct Client {
    sender: SendRequest<Full<Bytes>>,
}

impl Client {
    pub async fn over<S>(io: S) -> Result<Client>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(io))
            .await
            .map_err(transport)?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        Ok(Client { sender })
    }

    #[cfg(unix)]
    pub async fn unix(path: &Path) -> Result<Client> {
        let stream = tokio::net::UnixStream::connect(path)
            .await
            .map_err(|error| Error::Transport(format!("{}: {error}", path.display())))?;
        Client::over(stream).await
    }

    /// Whether the connection can take another request.
    pub fn usable(&self) -> bool {
        !self.sender.is_closed()
    }

    pub async fn send(
        &mut self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Bytes,
    ) -> Result<Response> {
        let mut request = hyper::Request::builder().method(method).uri(path);
        // Over a socket the name means nothing; a server on the network wants its own.
        if !headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("host"))
        {
            request = request.header("host", "berth");
        }
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let request = request.body(Full::new(body)).map_err(transport)?;
        self.sender.ready().await.map_err(transport)?;
        let response = self.sender.send_request(request).await.map_err(transport)?;
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let body = response
            .into_body()
            .collect()
            .await
            .map_err(transport)?
            .to_bytes();
        Ok(Response {
            status,
            headers,
            body,
        })
    }

    /// A request with an optional JSON body, answered with JSON.
    pub async fn call<T: DeserializeOwned>(
        &mut self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<T> {
        let (headers, bytes): (&[(&str, &str)], Bytes) = match body {
            Some(body) => (
                &[("content-type", "application/json")],
                serde_json::to_vec(body).map_err(transport)?.into(),
            ),
            None => (&[], Bytes::new()),
        };
        self.send(method, path, headers, bytes).await?.json()
    }

    pub async fn get<T: DeserializeOwned>(&mut self, path: &str) -> Result<T> {
        self.call("GET", path, None).await
    }

    pub async fn post<T: DeserializeOwned>(&mut self, path: &str, body: &Value) -> Result<T> {
        self.call("POST", path, Some(body)).await
    }
}

/// Open a stream. `protocols` is sent as `Sec-WebSocket-Protocol` when not empty.
pub async fn websocket<S>(io: S, path: &str, protocols: &str) -> Result<WebSocketStream<S>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    websocket_at(io, "berth", path, protocols).await
}

/// The same, naming the host the connection goes to, for a server that asks.
pub async fn websocket_at<S>(
    io: S,
    host: &str,
    path: &str,
    protocols: &str,
) -> Result<WebSocketStream<S>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};
    let mut request = format!("ws://{host}{path}")
        .into_client_request()
        .map_err(transport)?;
    if !protocols.is_empty() {
        let value = protocols.parse().map_err(transport)?;
        request
            .headers_mut()
            .insert("sec-websocket-protocol", value);
    }
    match tokio_tungstenite::client_async(request, io).await {
        Ok((stream, _)) => Ok(stream),
        Err(tungstenite::Error::Http(response)) => {
            let body = response.body().clone().unwrap_or_default();
            let status = response.status().as_u16();
            // The body may still be on its way when the head is read, and
            // then only the headers say which error it was.
            let header = |name: &str| response.headers().get(name)?.to_str().ok();
            let named = header(crate::ERROR_HEADER);
            let said = header(crate::ERROR_MESSAGE_HEADER)
                .map(decode)
                .filter(|said| !said.is_empty());
            match (serde_json::from_slice::<crate::ErrorBody>(&body), named) {
                (Err(_), Some(code)) => Err(Error::Api {
                    status,
                    code: code.to_owned(),
                    message: said.unwrap_or_else(|| format!("HTTP {status}: {code}")),
                }),
                _ => Err(Response {
                    status,
                    headers: Default::default(),
                    body: body.into(),
                }
                .error()),
            }
        }
        Err(error) => Err(transport(error)),
    }
}

#[cfg(unix)]
pub async fn websocket_unix(
    path: &Path,
    target: &str,
) -> Result<WebSocketStream<tokio::net::UnixStream>> {
    let stream = tokio::net::UnixStream::connect(path)
        .await
        .map_err(|error| Error::Transport(format!("{}: {error}", path.display())))?;
    websocket(stream, target, "").await
}

pub use crate::escape::{decode, encode, encode_path};

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn a_refused_stream_says_why_from_the_head_alone() {
        let (ours, mut theirs) = tokio::io::duplex(4096);
        let server = tokio::spawn(async move {
            let mut asked = [0; 1024];
            let _ = theirs.read(&mut asked).await;
            let said = crate::error_message_header("Nothing answers at db: né");
            // The head alone: the body it promises never comes.
            let head = format!(
                "HTTP/1.1 503 Service Unavailable\r\ncontent-length: 80\r\n{}: unreachable\r\n{}: {said}\r\n\r\n",
                crate::ERROR_HEADER,
                crate::ERROR_MESSAGE_HEADER,
            );
            theirs.write_all(head.as_bytes()).await.unwrap();
            theirs
        });
        let refused = websocket(ours, "/v1/sessions/1", "").await.err().unwrap();
        assert_eq!(refused.status(), 503);
        assert_eq!(refused.to_string(), "Nothing answers at db: né");
        drop(server.await);
    }
}
