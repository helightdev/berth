//! The server's side of the portal: one WebSocket that carries any number of
//! operations. Each is turned into the request it is over HTTP and given to
//! the same router, so the portal can do nothing a request could not, and
//! nothing differently. The frames are described in `berth_api::ops`.

use axum::{
    Router,
    body::Body,
    extract::ws::{Message, WebSocket},
    http::{Request, header},
};
use berth_api::{
    ErrorBody, b64, client, code,
    ops::{self, Kind, Op},
    portal::{self, Credit, Taken, WINDOWS, tagged, untagged},
    unb64,
};
use futures_util::{SinkExt, StreamExt};
use hyper_util::{rt::TokioIo, service::TowerToHyperService};
use serde_json::{Map, Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tower::ServiceExt;

/// The messages of the WebSocket an operation is on its own.
mod upstream {
    pub use berth_api::client::Frame;
}

/// An operation and where its path goes: `prefix` comes after `/v1`.
pub struct Target {
    pub op: &'static Op,
    pub prefix: String,
}

type Problem = ((u16, &'static str), String);

/// Finds the operation a frame names, for the host it names if any.
pub type Resolve = Arc<dyn Fn(&str, Option<&str>) -> Result<Target, Problem> + Send + Sync>;

/// The resolver of berthd: its own operations, and no hosts.
pub fn host_operations() -> Resolve {
    Arc::new(|name, host| match (ops::find(ops::HOST, name), host) {
        (_, Some(_)) => Err((code::BAD_REQUEST, "A host has no hosts".into())),
        // berth serve opens these at their own path, never through a portal.
        (Some(op), None) if !op.server_only.is_empty() => {
            Err((code::FORBIDDEN, op.server_only.into()))
        }
        (Some(op), None) => Ok(Target {
            op,
            prefix: String::new(),
        }),
        (None, None) => Err((code::NOT_FOUND, format!("No operation {name}"))),
    })
}

fn failure(id: u64, problem: Problem) -> Value {
    let ((status, code), message) = problem;
    // A message may carry what the caller sent, which is not sent back whole.
    let message = portal::shorten(&message);
    json!({"id": id, "error": {"code": code, "message": message, "status": status}})
}

fn plain(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

type Headers = Vec<(&'static str, String)>;

/// The request an operation is: its path and query, and its headers.
fn address(target: &Target, args: &Value) -> Result<(String, Headers), Problem> {
    let mut args: Map<String, Value> = match args {
        Value::Object(args) => args.clone(),
        Value::Null => Map::new(),
        _ => return Err((code::BAD_REQUEST, "args is an object".into())),
    };
    let mut take = |name: &str| args.remove(name).as_ref().and_then(plain);
    let mut path = format!("/v1{}", target.prefix);
    for part in target.op.path.trim_start_matches("/v1").split('/').skip(1) {
        path.push('/');
        match part.strip_prefix('{').and_then(|p| p.strip_suffix('}')) {
            Some(name) => {
                let (name, deep) = match name.strip_prefix('*') {
                    Some(name) => (name, true),
                    None => (name, false),
                };
                let value = take(name)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        (
                            code::BAD_REQUEST,
                            format!("{} needs {name}", target.op.name),
                        )
                    })?;
                path.push_str(&match deep {
                    true => client::encode_path(&value),
                    false => client::encode(&value),
                });
            }
            None => path.push_str(part),
        }
    }
    let mut headers = vec![];
    for (argument, header) in target.op.headers {
        if let Some(value) = take(argument) {
            headers.push((*header, value));
        }
    }
    let query: Vec<String> = args
        .iter()
        .filter_map(|(name, value)| {
            Some(format!(
                "{}={}",
                client::encode(name),
                client::encode(&plain(value)?)
            ))
        })
        .collect();
    if !query.is_empty() {
        path.push('?');
        path.push_str(&query.join("&"));
    }
    Ok((path, headers))
}

async fn call(router: Router, target: Target, frame: &Value) -> Result<Value, Problem> {
    if !target.op.http_only.is_empty() {
        return Err((code::NOT_AVAILABLE, target.op.http_only.into()));
    }
    let (path, headers) = address(&target, &frame["args"])?;
    let mut request = Request::builder().method(target.op.method).uri(path);
    for (name, value) in headers {
        request = request.header(name, value);
    }
    let body = match (&frame["data"], target.op.body) {
        (Value::Null, _) => Body::empty(),
        (data, ops::Body::Bytes) => {
            let bytes = data["base64"].as_str().and_then(unb64).ok_or_else(|| {
                (
                    code::BAD_REQUEST,
                    format!("{} takes {{\"base64\": …}}", target.op.name),
                )
            })?;
            Body::from(bytes)
        }
        (data, ops::Body::Json) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(data.to_string())
        }
    };
    let request = request
        .body(body)
        .map_err(|error| (code::BAD_REQUEST, error.to_string()))?;
    let Ok(response) = router.oneshot(request).await;
    let status = response.status().as_u16();
    let kind = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let limit = ops::PORTAL_BYTES + 4096;
    let body = axum::body::to_bytes(response.into_body(), limit)
        .await
        .map_err(|_| {
            (
                code::TOO_LARGE,
                format!(
                    "More than {} bytes: ask for it over HTTP",
                    ops::PORTAL_BYTES
                ),
            )
        })?;
    if status >= 400 {
        let (code, message) = match serde_json::from_slice::<ErrorBody>(&body) {
            Ok(body) => (body.error.code, body.error.message),
            Err(_) => (
                "error".into(),
                String::from_utf8_lossy(&body).trim().to_owned(),
            ),
        };
        // The code's text is the caller's; ours has to be static.
        let known = [
            code::BAD_REQUEST,
            code::UNAUTHORIZED,
            code::FORBIDDEN,
            code::NOT_FOUND,
            code::CONFLICT,
            code::PRECONDITION,
            code::TOO_LARGE,
            code::LIMIT,
            code::INTERNAL,
            code::NOT_AVAILABLE,
            code::UNREACHABLE,
            code::NOT_SERVED,
            code::TIMEOUT,
            code::SERVICE,
        ];
        let name = known
            .iter()
            .find(|known| known.1 == code)
            .map_or("error", |known| known.1);
        return Err(((status, name), message));
    }
    if body.is_empty() {
        Ok(Value::Null)
    } else if kind.starts_with("application/json") {
        serde_json::from_slice(&body).map_err(|error| (code::INTERNAL, error.to_string()))
    } else {
        Ok(json!({"content_type": kind, "base64": b64(&body)}))
    }
}

/// Open the operation's own WebSocket on the router, through a connection
/// that never leaves the program.
async fn open(
    router: Router,
    target: &Target,
    args: &Value,
) -> Result<client::WebSocketStream<tokio::io::DuplexStream>, Problem> {
    let (path, _) = address(target, args)?;
    let (ours, theirs) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let service = TowerToHyperService::new(router);
        let connection = hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(theirs), service);
        let _ = connection.with_upgrades().await;
    });
    client::websocket(ours, &path, "").await.map_err(|error| {
        let known = [
            code::FORBIDDEN,
            code::NOT_FOUND,
            code::BAD_REQUEST,
            code::UNREACHABLE,
            code::NOT_SERVED,
            code::TIMEOUT,
        ];
        let kind = known
            .into_iter()
            .find(|known| known.1 == error.code())
            .unwrap_or((error.status(), "error"));
        (kind, error.to_string())
    })
}

/// What one portal may hold for its caller. A caller that does not read
/// cannot make it hold more: the reader stops reading once its own replies
/// fill `control`, and a call or a stream's input that does not fit in
/// `held` is refused.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Bytes of frames of streams and of answers waiting for the socket.
    pub room: usize,
    /// Bytes of the reader's own replies and of credits waiting for the socket.
    pub control: usize,
    /// Bytes of calls in flight, of their answers until they are written,
    /// and of input waiting for its stream.
    pub held: usize,
    /// Calls in flight at once.
    pub calls: usize,
    /// Streams open at once.
    pub streams: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            room: 1 << 20,
            control: 64 * 1024,
            held: portal::HELD_BYTES,
            calls: portal::CALLS,
            streams: portal::STREAMS,
        }
    }
}

/// Set the most a WebSocket of the portal takes in one message.
pub fn upgrade(
    upgrade: axum::extract::ws::WebSocketUpgrade,
) -> axum::extract::ws::WebSocketUpgrade {
    upgrade
        .max_message_size(portal::FRAME_BYTES)
        .max_frame_size(portal::FRAME_BYTES)
}

/// The most each budget of a portal held at once, and the frames it read:
/// what a test looks at.
#[derive(Debug, Default)]
pub struct Gauge {
    pub read: AtomicUsize,
    pub room: AtomicUsize,
    pub control: AtomicUsize,
    pub held: AtomicUsize,
}

impl Gauge {
    fn saw(&self, which: &AtomicUsize, budget: &Semaphore, size: usize) {
        which.fetch_max(
            size.saturating_sub(budget.available_permits()),
            Ordering::Relaxed,
        );
    }
}

/// A message on its way to the socket, with the room it takes until it is written.
struct Queued {
    message: Message,
    _taken: Vec<OwnedSemaphorePermit>,
}

/// The frames on their way to the socket, in the order they were sent. Only
/// the writer gives room back, and it waits on nothing but the socket, so a
/// task that waits for room waits for the network and never for another
/// task.
#[derive(Clone)]
struct Out {
    queue: mpsc::UnboundedSender<Queued>,
    room: Arc<Semaphore>,
    control: Arc<Semaphore>,
    limits: Limits,
    gauge: Arc<Gauge>,
}

fn size_of(message: &Message) -> usize {
    match message {
        Message::Text(text) => text.len(),
        Message::Binary(bytes) => bytes.len(),
        _ => 0,
    }
}

impl Out {
    /// A frame of a stream or the answer to a call: it waits for room, and
    /// keeps what it holds of the portal's budget until it is written.
    async fn data(&self, message: Message, held: Option<OwnedSemaphorePermit>) -> bool {
        let size = size_of(&message).clamp(1, self.limits.room) as u32;
        let Ok(permit) = self.room.clone().acquire_many_owned(size).await else {
            return false;
        };
        self.gauge
            .saw(&self.gauge.room, &self.room, self.limits.room);
        let mut taken = vec![permit];
        taken.extend(held);
        self.queue
            .send(Queued {
                message,
                _taken: taken,
            })
            .is_ok()
    }

    /// A small frame of the portal's own, such as an error or a credit. It
    /// waits only for the writer, which waits only for the socket.
    async fn control(&self, frame: Value) -> bool {
        let message = Message::Text(frame.to_string().into());
        let size = size_of(&message).clamp(1, self.limits.control) as u32;
        let Ok(permit) = self.control.clone().acquire_many_owned(size).await else {
            return false;
        };
        self.gauge
            .saw(&self.gauge.control, &self.control, self.limits.control);
        self.queue
            .send(Queued {
                message,
                _taken: vec![permit],
            })
            .is_ok()
    }
}

/// What the caller sends towards an open stream, each frame with what it
/// holds of the portal's budget until the stream took it.
struct Inbound {
    input: mpsc::UnboundedSender<(upstream::Frame, OwnedSemaphorePermit)>,
    credit: Arc<Credit>,
}

/// An error the portal answers with for a caller's frame, as the frame
/// that says it.
fn limit(id: u64, message: String) -> Value {
    failure(id, (code::LIMIT, message))
}

/// Serve a portal until its socket closes. `router` answers as the caller
/// the portal was opened by.
pub async fn run(socket: WebSocket, router: Router, resolve: Resolve) {
    serve(socket, router, resolve, Limits::default(), Arc::default()).await
}

/// [`run`], within `limits`, saying in `gauge` what it held.
pub async fn serve(
    socket: WebSocket,
    router: Router,
    resolve: Resolve,
    limits: Limits,
    gauge: Arc<Gauge>,
) {
    let (mut sink, mut source) = socket.split();
    let (queue, mut outgoing) = mpsc::unbounded_channel::<Queued>();
    let room = Arc::new(Semaphore::new(limits.room));
    let control = Arc::new(Semaphore::new(limits.control));
    let held = Arc::new(Semaphore::new(limits.held));
    let calls = Arc::new(Semaphore::new(limits.calls));
    let out = Out {
        queue,
        room: room.clone(),
        control: control.clone(),
        limits,
        gauge: gauge.clone(),
    };
    let writer = tokio::spawn(async move {
        while let Some(queued) = outgoing.recv().await {
            if sink.send(queued.message).await.is_err() {
                break;
            }
        }
        // Whoever waits for room would wait for a socket that is gone.
        room.close();
        control.close();
    });
    let hold = |size: usize| {
        let size = u32::try_from(size.max(1)).unwrap_or(u32::MAX);
        let permit = held.clone().try_acquire_many_owned(size).ok();
        gauge.saw(&gauge.held, &held, limits.held);
        permit
    };
    let mut streams: HashMap<u32, Inbound> = HashMap::new();
    while let Some(Ok(message)) = source.next().await {
        gauge.read.fetch_add(1, Ordering::Relaxed);
        let size = size_of(&message);
        let frame: Value = match message {
            Message::Text(text) => match serde_json::from_str(text.as_str()) {
                Ok(frame) => frame,
                Err(_) => continue,
            },
            Message::Binary(bytes) => {
                if let Some((id, bytes)) = untagged(&bytes) {
                    let frame = upstream::Frame::Binary(bytes.to_vec().into());
                    feed(&mut streams, id, frame, size, &hold, &out).await;
                }
                continue;
            }
            Message::Close(_) => break,
            _ => continue,
        };
        if let Some(id) = frame["sub"].as_u64() {
            let id = id as u32;
            if frame["end"].as_bool() == Some(true) {
                streams.remove(&id);
            } else if let Some(stream) = streams.get(&id) {
                match frame["credit"].as_u64() {
                    Some(bytes) => stream.credit.grant(bytes),
                    None => {
                        let text = frame["data"].to_string();
                        let frame = upstream::Frame::Text(text.into());
                        feed(&mut streams, id, frame, size, &hold, &out).await;
                    }
                }
            }
            continue;
        }
        let Some(id) = frame["id"].as_u64().filter(|id| *id <= u32::MAX as u64) else {
            continue;
        };
        let name = frame["op"].as_str().unwrap_or("");
        // The name is the caller's: a long one is not sent back.
        if name.len() > portal::NAME_BYTES {
            let problem = (
                code::BAD_REQUEST,
                format!(
                    "An operation's name is at most {} bytes",
                    portal::NAME_BYTES
                ),
            );
            if !out.control(failure(id, problem)).await {
                break;
            }
            continue;
        }
        let target = match resolve(name, frame["host"].as_str()) {
            Ok(target) => target,
            Err(problem) => {
                if !out.control(failure(id, problem)).await {
                    break;
                }
                continue;
            }
        };
        streams.retain(|_, stream| !stream.input.is_closed());
        match target.op.kind {
            Kind::Call => {
                let Ok(call_permit) = calls.clone().try_acquire_owned() else {
                    let said = format!(
                        "More than {} calls at once on this portal: wait for an answer",
                        limits.calls
                    );
                    if !out.control(limit(id, said)).await {
                        break;
                    }
                    continue;
                };
                let Some(asked) = hold(size) else {
                    let said = format!(
                        "More than {} bytes wait on this portal: read what came, or wait for an answer",
                        limits.held
                    );
                    if !out.control(limit(id, said)).await {
                        break;
                    }
                    continue;
                };
                let (router, out, held, gauge) =
                    (router.clone(), out.clone(), held.clone(), gauge.clone());
                tokio::spawn(async move {
                    let answer = match call(router, target, &frame).await {
                        Ok(result) => json!({"id": id, "result": result}),
                        Err(problem) => failure(id, problem),
                    };
                    drop((frame, asked));
                    let mut answer = answer.to_string();
                    let size = u32::try_from(answer.len().max(1)).unwrap_or(u32::MAX);
                    let taken = held.clone().try_acquire_many_owned(size).ok();
                    gauge.saw(&gauge.held, &held, limits.held);
                    if taken.is_none() {
                        answer = limit(id, format!(
                            "The answer is {} bytes, and more than {} bytes wait on this portal: read what came, or ask over HTTP",
                            answer.len(),
                            limits.held
                        ))
                        .to_string();
                    }
                    out.data(Message::Text(answer.into()), taken).await;
                    drop(call_permit);
                });
            }
            Kind::Stream => {
                if streams.len() >= limits.streams {
                    let said = format!(
                        "More than {} streams at once on this portal: end one first",
                        limits.streams
                    );
                    if !out.control(limit(id, said)).await {
                        break;
                    }
                    continue;
                }
                let window = frame["window"]
                    .as_u64()
                    .map(|window| window.clamp(WINDOWS.0.into(), WINDOWS.1.into()) as u32);
                let (towards, input) = mpsc::unbounded_channel();
                let credit = Arc::new(Credit::with(window));
                let inbound = Inbound {
                    input: towards,
                    credit: credit.clone(),
                };
                streams.insert(id as u32, inbound);
                let (router, out) = (router.clone(), out.clone());
                tokio::spawn(async move {
                    let stream = match open(router, &target, &frame["args"]).await {
                        Ok(stream) => stream,
                        Err(problem) => {
                            out.control(failure(id, problem)).await;
                            return;
                        }
                    };
                    let mut opened = json!({"id": id, "result": {"open": true}});
                    if let Some(window) = window {
                        opened["result"]["window"] = json!(window);
                    }
                    if !out
                        .data(Message::Text(opened.to_string().into()), None)
                        .await
                    {
                        return;
                    }
                    pipe(id as u32, stream, input, &out, &credit, window).await;
                });
            }
        }
    }
    drop(streams);
    drop(out);
    let _ = writer.await;
}

/// Give the stream `id` a frame of the caller's, holding its size of the
/// portal's budget until the stream took it. A stream whose caller sends
/// more than the budget holds is ended, as a caller within its window never
/// does.
async fn feed(
    streams: &mut HashMap<u32, Inbound>,
    id: u32,
    frame: upstream::Frame,
    size: usize,
    hold: &impl Fn(usize) -> Option<OwnedSemaphorePermit>,
    out: &Out,
) {
    let Some(stream) = streams.get(&id) else {
        return;
    };
    match hold(size) {
        Some(permit) => {
            let _ = stream.input.send((frame, permit));
        }
        None => {
            streams.remove(&id);
            let ((status, code), message) = (
                code::LIMIT,
                format!(
                    "More than {} bytes wait on this portal: send a stream no more than its window",
                    out.limits.held
                ),
            );
            let end = json!({"sub": id, "end": true,
                "error": {"code": code, "message": message, "status": status}});
            out.control(end).await;
        }
    }
}

/// One open stream: its frames to the caller and the caller's to it, each way
/// on its own, so a direction that waits does not hold up the other.
async fn pipe(
    sub: u32,
    stream: client::WebSocketStream<tokio::io::DuplexStream>,
    mut input: mpsc::UnboundedReceiver<(upstream::Frame, OwnedSemaphorePermit)>,
    out: &Out,
    credit: &Credit,
    window: Option<u32>,
) {
    let (mut up, mut down) = stream.split();
    let downstream = async {
        while let Some(Ok(frame)) = down.next().await {
            let message = match frame {
                upstream::Frame::Text(text) => {
                    let data = serde_json::from_str::<Value>(text.as_str())
                        .unwrap_or_else(|_| json!(text.as_str()));
                    Message::Text(json!({"sub": sub, "data": data}).to_string().into())
                }
                upstream::Frame::Binary(bytes) => {
                    credit.room().await;
                    credit.spend(bytes.len());
                    Message::Binary(tagged(sub, &bytes).into())
                }
                upstream::Frame::Close(_) => break,
                _ => continue,
            };
            if !out.data(message, None).await {
                return false;
            }
        }
        true
    };
    let upstream = async {
        let mut taken = Taken::new(window);
        while let Some((frame, held)) = input.recv().await {
            let size = match &frame {
                upstream::Frame::Binary(bytes) => bytes.len(),
                _ => 0,
            };
            // A stream that takes nothing more ends from its side, downstream.
            if up.send(frame).await.is_err() {
                return std::future::pending().await;
            }
            drop(held);
            if let Some(credit) = taken.took(sub, size) {
                out.control(credit).await;
            }
        }
        // The caller ended it, or left.
        let _ = up.close().await;
    };
    tokio::select! {
        ended = downstream => if ended {
            let end = json!({"sub": sub, "end": true});
            out.data(Message::Text(end.to_string().into()), None).await;
        },
        () = upstream => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::ws::WebSocketUpgrade, routing::get};
    use berth_api::{client::Frame, ops::Role, portal::Portal};
    use std::time::Duration;
    use tokio::net::TcpStream;

    const fn op(name: &'static str, kind: Kind, path: &'static str) -> Op {
        Op {
            name,
            kind,
            method: "GET",
            path,
            body: ops::Body::Json,
            headers: &[],
            http_only: "",
            role: Role::View,
            key: "",
            server_only: "",
        }
    }

    static QUICK: Op = op("quick", Kind::Call, "/v1/quick");
    static SLOW: Op = op("slow", Kind::Call, "/v1/slow");
    static SINK: Op = op("sink", Kind::Stream, "/v1/sink");

    /// A portal on a port of the loopback, within `limits`, over a router
    /// with a quick call, a slow one, and a stream that never reads.
    async fn serving(limits: Limits) -> (std::net::SocketAddr, Arc<Gauge>) {
        let inner = Router::new()
            .route(
                "/v1/quick",
                get(|| async { axum::Json(json!({"ok": true})) }),
            )
            .route(
                "/v1/slow",
                get(|| async {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    axum::Json(json!({"ok": true}))
                }),
            )
            .route(
                "/v1/sink",
                get(|upgrade: WebSocketUpgrade| async {
                    upgrade.on_upgrade(|socket| async move {
                        std::future::pending::<()>().await;
                        drop(socket);
                    })
                }),
            );
        let resolve: Resolve = Arc::new(|name, _| {
            [&QUICK, &SLOW, &SINK]
                .into_iter()
                .find(|known| known.name == name)
                .map(|op| Target {
                    op,
                    prefix: String::new(),
                })
                .ok_or_else(|| (code::NOT_FOUND, format!("No operation {name}")))
        });
        let gauge = Arc::new(Gauge::default());
        let watched = gauge.clone();
        let outer = Router::new().route(
            "/v1/portal",
            get(move |ws: WebSocketUpgrade| {
                let (inner, resolve, gauge) = (inner.clone(), resolve.clone(), watched.clone());
                async move {
                    upgrade(ws)
                        .on_upgrade(move |socket| serve(socket, inner, resolve, limits, gauge))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, outer).await });
        (address, gauge)
    }

    async fn socket(address: std::net::SocketAddr) -> client::WebSocketStream<TcpStream> {
        let stream = TcpStream::connect(address).await.unwrap();
        client::websocket(stream, ops::PORTAL, "").await.unwrap()
    }

    async fn portal(address: std::net::SocketAddr) -> Portal {
        Portal::over(socket(address).await)
    }

    async fn next_text(socket: &mut client::WebSocketStream<TcpStream>) -> Value {
        loop {
            match tokio::time::timeout(Duration::from_secs(10), socket.next())
                .await
                .unwrap()
            {
                Some(Ok(Frame::Text(text))) => return serde_json::from_str(text.as_str()).unwrap(),
                Some(Ok(_)) => continue,
                other => panic!("{other:?}"),
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_caller_that_does_not_read_is_held_within_the_limits() {
        let limits = Limits::default();
        let (address, gauge) = serving(limits).await;
        let (mut sink, mut source) = socket(address).await.split();
        // A long name is not sent back.
        let long = "x".repeat(60_000);
        sink.send(Frame::Text(json!({"id": 1, "op": long}).to_string().into()))
            .await
            .unwrap();
        let mut back = None;
        while back.is_none() {
            if let Some(Ok(Frame::Text(text))) = source.next().await {
                back = Some(text.to_string());
            }
        }
        let back = back.unwrap();
        assert!(back.len() < 300, "{back}");
        assert!(
            back.contains("An operation's name is at most 128 bytes"),
            "{back}"
        );
        // Then it sends without reading: far more replies than the socket's buffers hold.
        let sent = 400_000;
        let flood = tokio::spawn(async move {
            for id in 0..sent {
                let frame = json!({"id": id, "op": "no-such-operation"}).to_string();
                if sink.send(Frame::Text(frame.into())).await.is_err() {
                    break;
                }
            }
        });
        tokio::time::sleep(Duration::from_secs(3)).await;
        let read = gauge.read.load(Ordering::Relaxed);
        tokio::time::sleep(Duration::from_secs(1)).await;
        // The reader stopped: it waits for room for its replies.
        assert_eq!(gauge.read.load(Ordering::Relaxed), read);
        assert!(read < sent, "the portal read all {read} frames");
        assert!(gauge.control.load(Ordering::Relaxed) <= limits.control);
        assert!(gauge.room.load(Ordering::Relaxed) <= limits.room);
        assert!(gauge.held.load(Ordering::Relaxed) <= limits.held);
        // Another caller of the same server is answered meanwhile.
        let other = portal(address).await;
        let answered = tokio::time::timeout(
            Duration::from_secs(5),
            other.call("quick", None, Value::Null, None),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(answered, json!({"ok": true}));
        // Once it reads, it gets its answers.
        let mut got = 0;
        while got < 1000 {
            if let Some(Ok(Frame::Text(_))) = source.next().await {
                got += 1;
            }
        }
        flood.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn many_quick_calls_all_come_back() {
        let (address, gauge) = serving(Limits::default()).await;
        let portal = portal(address).await;
        for _ in 0..10 {
            let calls = (0..200).map(|_| portal.call("quick", None, Value::Null, None));
            for answered in futures_util::future::join_all(calls).await {
                assert_eq!(answered.unwrap(), json!({"ok": true}));
            }
        }
        assert!(gauge.held.load(Ordering::Relaxed) <= Limits::default().held);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn calls_and_streams_at_once_are_limited() {
        let limits = Limits {
            calls: 3,
            streams: 2,
            ..Limits::default()
        };
        let (address, _) = serving(limits).await;
        let portal = portal(address).await;
        let calls = (0..6).map(|_| portal.call("slow", None, Value::Null, None));
        let answered = futures_util::future::join_all(calls).await;
        let refused: Vec<String> = answered
            .iter()
            .filter_map(|answered| answered.as_ref().err())
            .map(|error| error.to_string())
            .collect();
        assert_eq!(refused.len(), 3, "{answered:?}");
        assert!(
            refused[0].contains("More than 3 calls at once"),
            "{refused:?}"
        );
        assert_eq!(answered[0].as_ref().unwrap(), &json!({"ok": true}));
        // Once they are answered, there is room again.
        portal.call("slow", None, Value::Null, None).await.unwrap();

        let first = portal.open("sink", None, Value::Null).await.unwrap();
        let _second = portal.open("sink", None, Value::Null).await.unwrap();
        let third = portal.open("sink", None, Value::Null).await.err().unwrap();
        assert!(
            third.to_string().contains("More than 2 streams at once"),
            "{third}"
        );
        drop(first);
        tokio::time::sleep(Duration::from_millis(200)).await;
        portal.open("sink", None, Value::Null).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stream_sent_past_what_the_portal_holds_ends() {
        let limits = Limits {
            held: 256 * 1024,
            ..Limits::default()
        };
        let (address, gauge) = serving(limits).await;
        let mut socket = socket(address).await;
        // Without a window, nothing but the portal's budget stops it.
        socket
            .send(Frame::Text(
                json!({"id": 1, "op": "sink"}).to_string().into(),
            ))
            .await
            .unwrap();
        assert_eq!(next_text(&mut socket).await["result"]["open"], true);
        let chunk = vec![7u8; 32 * 1024];
        for _ in 0..64 {
            socket
                .send(Frame::Binary(tagged(1, &chunk).into()))
                .await
                .unwrap();
        }
        let ended = next_text(&mut socket).await;
        assert_eq!(ended["sub"], 1, "{ended}");
        assert_eq!(ended["end"], true, "{ended}");
        assert_eq!(ended["error"]["code"], "limit", "{ended}");
        assert!(gauge.held.load(Ordering::Relaxed) <= limits.held);
    }
}
