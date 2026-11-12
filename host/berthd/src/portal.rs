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
    portal::{tagged, untagged},
    unb64,
};
use futures_util::{SinkExt, StreamExt};
use hyper_util::{rt::TokioIo, service::TowerToHyperService};
use serde_json::{Map, Value, json};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::mpsc;
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
        (Some(op), None) => Ok(Target {
            op,
            prefix: String::new(),
        }),
        (None, None) => Err((code::NOT_FOUND, format!("No operation {name}"))),
    })
}

fn failure(id: u64, problem: Problem) -> Value {
    let ((status, code), message) = problem;
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
        ];
        let kind = known
            .into_iter()
            .find(|known| known.1 == error.code())
            .unwrap_or((error.status(), "error"));
        (kind, error.to_string())
    })
}

/// Serve a portal until its socket closes. `router` answers as the caller
/// the portal was opened by.
pub async fn run(socket: WebSocket, router: Router, resolve: Resolve) {
    let (mut sink, mut source) = socket.split();
    let (out, mut outgoing) = mpsc::unbounded_channel::<Message>();
    tokio::spawn(async move {
        while let Some(message) = outgoing.recv().await {
            if sink.send(message).await.is_err() {
                break;
            }
        }
    });
    let say = |out: &mpsc::UnboundedSender<Message>, frame: Value| {
        let _ = out.send(Message::Text(frame.to_string().into()));
    };
    // What the caller sends towards each open stream.
    let mut streams: HashMap<u32, mpsc::UnboundedSender<upstream::Frame>> = HashMap::new();
    while let Some(Ok(message)) = source.next().await {
        let frame: Value = match message {
            Message::Text(text) => match serde_json::from_str(text.as_str()) {
                Ok(frame) => frame,
                Err(_) => continue,
            },
            Message::Binary(bytes) => {
                if let Some((id, bytes)) = untagged(&bytes)
                    && let Some(stream) = streams.get(&id)
                {
                    let _ = stream.send(upstream::Frame::Binary(bytes.to_vec().into()));
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
                let _ = stream.send(upstream::Frame::Text(frame["data"].to_string().into()));
            }
            continue;
        }
        let Some(id) = frame["id"].as_u64().filter(|id| *id <= u32::MAX as u64) else {
            continue;
        };
        let name = frame["op"].as_str().unwrap_or("");
        let target = match resolve(name, frame["host"].as_str()) {
            Ok(target) => target,
            Err(problem) => {
                say(&out, failure(id, problem));
                continue;
            }
        };
        streams.retain(|_, stream| !stream.is_closed());
        match target.op.kind {
            Kind::Call => {
                let (router, out) = (router.clone(), out.clone());
                tokio::spawn(async move {
                    let frame = match call(router, target, &frame).await {
                        Ok(result) => json!({"id": id, "result": result}),
                        Err(problem) => failure(id, problem),
                    };
                    let _ = out.send(Message::Text(frame.to_string().into()));
                });
            }
            Kind::Stream => {
                let (towards, mut input) = mpsc::unbounded_channel();
                streams.insert(id as u32, towards);
                let (router, out) = (router.clone(), out.clone());
                tokio::spawn(async move {
                    let stream = match open(router, &target, &frame["args"]).await {
                        Ok(stream) => stream,
                        Err(problem) => {
                            let _ =
                                out.send(Message::Text(failure(id, problem).to_string().into()));
                            return;
                        }
                    };
                    let opened = json!({"id": id, "result": {"open": true}});
                    let _ = out.send(Message::Text(opened.to_string().into()));
                    let (mut up, mut down) = stream.split();
                    let sub = id as u32;
                    loop {
                        tokio::select! {
                            frame = down.next() => match frame {
                                Some(Ok(upstream::Frame::Text(text))) => {
                                    let data = serde_json::from_str::<Value>(text.as_str())
                                        .unwrap_or_else(|_| json!(text.as_str()));
                                    let frame = json!({"sub": sub, "data": data});
                                    if out.send(Message::Text(frame.to_string().into())).is_err() {
                                        return;
                                    }
                                }
                                Some(Ok(upstream::Frame::Binary(bytes))) => {
                                    if out.send(Message::Binary(tagged(sub, &bytes).into())).is_err() {
                                        return;
                                    }
                                }
                                Some(Ok(upstream::Frame::Close(_))) | Some(Err(_)) | None => break,
                                Some(Ok(_)) => {}
                            },
                            frame = input.recv() => match frame {
                                Some(frame) => {
                                    if up.send(frame).await.is_err() {
                                        break;
                                    }
                                }
                                // The caller ended it, or left.
                                None => {
                                    let _ = up.close().await;
                                    return;
                                }
                            },
                        }
                    }
                    let end = json!({"sub": sub, "end": true});
                    let _ = out.send(Message::Text(end.to_string().into()));
                });
            }
        }
    }
}
