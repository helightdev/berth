//! A link: a command whose standard input and output are a connection to a
//! host's berthd. HTTP/1.1 carries one request at a time and a stream takes a
//! connection whole, so each concurrent request or stream is its own process;
//! idle ones are kept and used again.

use berth_api::client::{self, Client, Error, Frame, Response};
use futures_util::{Sink, Stream};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::{
    path::PathBuf,
    pin::Pin,
    process::Stdio,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::process::{Child, ChildStdin, ChildStdout};

/// Idle connections kept per host.
const IDLE: usize = 4;

#[derive(Debug, Clone, Default)]
pub struct LinkSpec {
    pub program: String,
    pub args: Vec<String>,
    /// Written before anything else: a sudo password.
    pub first_line: Option<String>,
    /// Set in the command's environment, as ssh's askpass helper needs.
    pub env: Vec<(String, String)>,
    /// The host's name, for what is said when it fails.
    pub name: String,
    /// Where a refused login is noted, for a link over SSH.
    pub guard: Option<crate::keys::refusal::Guard>,
}

/// A private directory for sockets of ours.
pub fn runtime_dir() -> PathBuf {
    let dir = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("berth"),
        // SAFETY: getuid cannot fail.
        _ => std::env::temp_dir().join(format!("berth-{}", unsafe { libc::getuid() })),
    };
    let _ = std::fs::create_dir_all(&dir);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    dir
}

/// One connection: the process and its two pipes. Dropping it ends the process.
pub struct Conn {
    _child: Child,
    input: ChildStdin,
    output: ChildStdout,
}

impl AsyncRead for Conn {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.output).poll_read(cx, buf)
    }
}

impl AsyncWrite for Conn {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.input).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.input).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.input).poll_shutdown(cx)
    }
}

/// A stream of the host, as the WebSocket it is.
pub trait Duplex:
    Stream<Item = std::result::Result<Frame, WsError>> + Sink<Frame, Error = WsError> + Send
{
}
impl<T> Duplex for T where
    T: Stream<Item = std::result::Result<Frame, WsError>> + Sink<Frame, Error = WsError> + Send
{
}
pub type WsError = berth_api::client::WsError;
pub type LinkStream = Pin<Box<dyn Duplex>>;

pub struct Link {
    spec: LinkSpec,
    idle: Mutex<Vec<Client>>,
    /// What the link commands last wrote to standard error.
    complaints: Arc<Mutex<String>>,
    /// Whether a login worked since the link was made, so a refusal noted
    /// before is forgotten once.
    worked: std::sync::atomic::AtomicBool,
}

impl Link {
    pub fn new(spec: LinkSpec) -> Link {
        Link {
            spec,
            idle: Mutex::new(vec![]),
            complaints: Default::default(),
            worked: Default::default(),
        }
    }

    /// Why nothing automatic should use the link now: the host refused its
    /// login a short while ago, or turned this address away. Whatever the
    /// user asks for uses it all the same, and tries once.
    pub fn refused(&self) -> Option<String> {
        self.spec.guard.as_ref()?.refused()
    }

    fn worked(&self) {
        use std::sync::atomic::Ordering;
        if let Some(guard) = &self.spec.guard
            && !self.worked.swap(true, Ordering::Relaxed)
        {
            guard.worked();
        }
    }

    /// Start one more link command.
    pub async fn connect(&self) -> client::Result<Conn> {
        let mut child = tokio::process::Command::new(&self.spec.program)
            .args(&self.spec.args)
            .envs(self.spec.env.iter().map(|(name, value)| (name, value)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| Error::Transport(format!("{}: {error}", self.spec.program)))?;
        let mut input = child.stdin.take().expect("piped");
        let output = child.stdout.take().expect("piped");
        let mut errors = child.stderr.take().expect("piped");
        // Each command's words are its own, so requests made at once do not
        // clear each other's; the link keeps the last command's that said any.
        let complaints = self.complaints.clone();
        tokio::spawn(async move {
            let mut buffer = [0; 1024];
            let mut said = String::new();
            while let Ok(count) = errors.read(&mut buffer).await
                && count > 0
            {
                if said.len() < 4096 {
                    said.push_str(&String::from_utf8_lossy(&buffer[..count]));
                    *complaints.lock().unwrap() = said.clone();
                }
            }
        });
        if let Some(line) = &self.spec.first_line {
            let sent = input.write_all(format!("{line}\n").as_bytes()).await;
            sent.map_err(|error| Error::Transport(error.to_string()))?;
        }
        Ok(Conn {
            _child: child,
            input,
            output,
        })
    }

    /// Why a connection failed, in the link command's own words when it said
    /// any. The command is given a moment to say them, as it may still be
    /// writing when the connection is seen to be closed.
    async fn explain(&self, error: Error) -> Error {
        let Error::Transport(message) = error else {
            return error;
        };
        for _ in 0..4 {
            if !self.complaints.lock().unwrap().trim().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let said = self.complaints.lock().unwrap().trim().to_owned();
        // A refused login is noted, so nothing automatic repeats it at once.
        let noted = match (&self.spec.guard, said.is_empty()) {
            (Some(guard), false) => {
                self.worked
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                guard.failed(&said)
            }
            _ => None,
        };
        Error::Transport(if said.is_empty() {
            message
        } else if let Some(noted) = noted {
            noted
        } else if let Some(changed) = crate::keys::known::changed_key(&self.spec.name, &said) {
            changed
        } else if said.contains("a password is required") {
            format!("{said}: keep the sudo password with berth secret set sudo/HOST")
        } else {
            said
        })
    }

    async fn fresh(&self) -> client::Result<Client> {
        match Client::over(self.connect().await?).await {
            Ok(client) => Ok(client),
            Err(error) => Err(self.explain(error).await),
        }
    }

    /// One request. An idle connection is used when there is one; if it turns
    /// out to be dead the request is made once more on a new one.
    pub async fn send(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> client::Result<Response> {
        let kept = self.idle.lock().unwrap().pop().filter(Client::usable);
        let mut attempt = kept.is_some();
        let mut client = match kept {
            Some(client) => client,
            None => self.fresh().await?,
        };
        loop {
            match client
                .send(method, path, headers, body.to_vec().into())
                .await
            {
                Ok(response) => {
                    // What a command said before it worked is no reason for a later failure.
                    self.complaints.lock().unwrap().clear();
                    self.worked();
                    let mut idle = self.idle.lock().unwrap();
                    if client.usable() && idle.len() < IDLE {
                        idle.push(client);
                    }
                    return Ok(response);
                }
                Err(Error::Transport(_)) if attempt => {
                    attempt = false;
                    client = self.fresh().await?;
                }
                Err(error) => return Err(self.explain(error).await),
            }
        }
    }

    pub async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> client::Result<T> {
        let (headers, bytes): (&[(&str, &str)], Vec<u8>) = match body {
            Some(body) => (
                &[("content-type", "application/json")],
                body.to_string().into_bytes(),
            ),
            None => (&[], vec![]),
        };
        self.send(method, path, headers, &bytes).await?.json()
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> client::Result<T> {
        self.call("GET", path, None).await
    }

    /// What the host's controller says of itself, read as this API says it
    /// also from a berthd of an older API.
    pub async fn version(&self) -> client::Result<berth_api::Version> {
        let reply: Value = self.get("/v1/version").await?;
        berth_api::Version::from_reply(reply)
            .map_err(|error| Error::Transport(format!("version: {error}")))
    }

    pub async fn post<T: DeserializeOwned>(&self, path: &str, body: &Value) -> client::Result<T> {
        self.call("POST", path, Some(body)).await
    }

    /// Run an action of the host, as `actions.run` takes it, and the session it
    /// runs as.
    pub async fn run_action(&self, name: &str, body: &Value) -> client::Result<berth_api::Session> {
        let path = format!("/v1/actions/{}", client::encode(name));
        self.post(&path, body).await
    }

    /// Open a stream. It holds its connection for as long as it lives.
    pub async fn stream(&self, path: &str) -> client::Result<LinkStream> {
        let conn = self.connect().await?;
        match client::websocket(conn, path, "").await {
            Ok(stream) => {
                self.worked();
                Ok(Box::pin(stream))
            }
            Err(error) => Err(self.explain(error).await),
        }
    }

    /// Whether the failure means the host has no berthd to talk to yet.
    pub fn missing(error: &Error) -> bool {
        let Error::Transport(message) = error else {
            return false;
        };
        // The shell not finding berthd, or berthd connect not finding the daemon's socket.
        ["command not found", "berthd: not found", "control.sock"]
            .iter()
            .any(|sign| message.contains(sign))
    }
}

/// A runtime for a program that is not asynchronous itself.
pub fn runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
}
