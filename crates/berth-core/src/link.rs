//! A link: a command whose standard input and output are a connection to a
//! host's berthd. HTTP/1.1 carries one request at a time and a stream takes a
//! connection whole, so each concurrent request or stream is its own process;
//! idle ones are kept and used again.

use crate::server::Server;
use berth_api::client::{self, Client, Error, Frame, Response};
use berth_api::ops::{self, Kind};
use berth_api::portal::Portal;
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
    /// Instead of a command: the host of this name behind a `berth serve`.
    pub via: Option<(Server, String)>,
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
    /// The portal of the server the host is behind, once it is open.
    portal: tokio::sync::Mutex<Option<Portal>>,
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
            portal: Default::default(),
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
            format!("{said}: set BERTH_SUDO_PASSWORD")
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
        if let Some((server, host)) = &self.spec.via {
            return self.send_via(server, host, method, path, body).await;
        }
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

    pub async fn post<T: DeserializeOwned>(&self, path: &str, body: &Value) -> client::Result<T> {
        self.call("POST", path, Some(body)).await
    }

    /// Run a task of the host, as `tasks.run` takes it, and the session it
    /// runs as. Through a berth server it is the host's action there, which
    /// the server keeps a job of and gives the server's secrets to.
    pub async fn run_task(&self, name: &str, body: &Value) -> client::Result<berth_api::Session> {
        let Some((server, host)) = &self.spec.via else {
            let path = format!("/v1/tasks/{}", client::encode(name));
            return self.post(&path, body).await;
        };
        let portal = self.portal(server).await?;
        let args = serde_json::json!({"name": host, "action": name});
        let acted = portal
            .call("hosts.act", None, args, Some(body.clone()))
            .await?;
        serde_json::from_value(acted["session"].clone()).map_err(|_| {
            Error::Transport(format!(
                "{name} on {host} is not a task of the host: run it with berth run {host} {name}"
            ))
        })
    }

    /// Open a stream. It holds its connection for as long as it lives.
    pub async fn stream(&self, path: &str) -> client::Result<LinkStream> {
        if let Some((server, host)) = &self.spec.via {
            let (path, query) = path.split_once('?').unwrap_or((path, ""));
            let (op, mut args) = recognised(Kind::Stream, "GET", path)?;
            args.extend(parameters(query));
            let portal = self.portal(server).await?;
            let stream = portal
                .open(op.name, Some(host), Value::Object(args))
                .await?;
            return Ok(Box::pin(stream));
        }
        let conn = self.connect().await?;
        match client::websocket(conn, path, "").await {
            Ok(stream) => {
                self.worked();
                Ok(Box::pin(stream))
            }
            Err(error) => Err(self.explain(error).await),
        }
    }

    /// The server's portal: the one that is open, or a new one.
    async fn portal(&self, server: &Server) -> client::Result<Portal> {
        let mut portal = self.portal.lock().await;
        match portal.as_ref().filter(|portal| portal.usable()) {
            Some(portal) => Ok(portal.clone()),
            None => {
                let opened = server.portal().await?;
                *portal = Some(opened.clone());
                Ok(opened)
            }
        }
    }

    /// A request as the operation it is, through the server's portal.
    async fn send_via(
        &self,
        server: &Server,
        host: &str,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> client::Result<Response> {
        let (path, query) = path.split_once('?').unwrap_or((path, ""));
        let (op, mut args) = recognised(Kind::Call, method, path)?;
        args.extend(parameters(query));
        let data = match (body.is_empty(), op.body) {
            (true, _) => None,
            (false, ops::Body::Bytes) => Some(serde_json::json!({"base64": berth_api::b64(body)})),
            (false, ops::Body::Json) => Some(
                serde_json::from_slice(body)
                    .map_err(|error| Error::Transport(error.to_string()))?,
            ),
        };
        let portal = self.portal(server).await?;
        let result = portal
            .call(op.name, Some(host), Value::Object(args), data)
            .await?;
        // Bytes come back wrapped; everything else is the JSON the request answers with.
        let body = match result["base64"].as_str().and_then(berth_api::unb64) {
            Some(bytes) if result.get("content_type").is_some() => bytes,
            _ if result.is_null() => vec![],
            _ => result.to_string().into_bytes(),
        };
        Ok(Response {
            status: 200,
            headers: Default::default(),
            body: body.into(),
        })
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

type Arguments = serde_json::Map<String, Value>;

fn recognised(
    kind: Kind,
    method: &str,
    path: &str,
) -> client::Result<(&'static ops::Op, Arguments)> {
    let (op, found) = ops::recognise(ops::HOST, kind, method, path)
        .ok_or_else(|| Error::Transport(format!("{method} {path} is no operation of a host")))?;
    let args = found
        .into_iter()
        .map(|(name, value)| (name.to_owned(), Value::String(value)));
    Ok((op, args.collect()))
}

/// The parameters of a query string.
fn parameters(query: &str) -> Arguments {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            (client::decode(name), Value::String(client::decode(value)))
        })
        .collect()
}

/// A runtime for a program that is not asynchronous itself.
pub fn runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
}
