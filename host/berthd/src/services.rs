//! Services: programs that plugins run on the host and that answer calls.
//! berthd routes a call to the program that holds the service's socket open.

use crate::error::{E, R};
use crate::state::{Caller, Mode, State};
use axum::{
    Json,
    extract::{Extension, Path, State as AppState, ws},
    response::Response,
};
use berth_api::{self as api, CallFrame, Level, ReplyFrame, code};
use futures_util::{SinkExt, StreamExt};
use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<ReplyFrame>>>>;

struct Serving {
    uid: u32,
    level: Level,
    calls: mpsc::UnboundedSender<CallFrame>,
    pending: Pending,
}

#[derive(Default)]
pub struct Services {
    serving: Mutex<HashMap<String, Serving>>,
    next: AtomicU64,
}

fn valid_part(part: &str) -> bool {
    !part.is_empty()
        && part.len() <= 64
        && part
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.')
}

pub async fn list(
    AppState(state): AppState<Arc<State>>,
    Extension(caller): Extension<Caller>,
) -> Json<Vec<api::Service>> {
    let config = state.config();
    let serving = state.services.serving.lock().unwrap();
    let mut services: Vec<api::Service> = config
        .services
        .iter()
        .filter(|spec| caller.host() || spec.agent)
        .map(|spec| api::Service {
            name: spec.name.clone(),
            description: spec.description.clone(),
            agent: spec.agent,
            methods: spec.methods.clone(),
            declared: true,
            up: serving.contains_key(&spec.name),
            uid: serving.get(&spec.name).map(|serving| serving.uid),
        })
        .collect();
    for (name, served) in serving.iter() {
        let declared = config.services.iter().any(|spec| &spec.name == name);
        if !declared && (caller.host() || served.level == Level::Agent) {
            services.push(api::Service {
                name: name.clone(),
                description: String::new(),
                agent: served.level == Level::Agent,
                methods: vec![],
                declared: false,
                up: true,
                uid: Some(served.uid),
            });
        }
    }
    services.sort_by(|a, b| a.name.cmp(&b.name));
    Json(services)
}

pub async fn call(
    AppState(state): AppState<Arc<State>>,
    Extension(caller): Extension<Caller>,
    Path((plugin, name, method)): Path<(String, String, String)>,
    body: Option<Json<serde_json::Value>>,
) -> R<Json<serde_json::Value>> {
    let service = format!("{plugin}/{name}");
    let config = state.config();
    let spec = config.services.iter().find(|spec| spec.name == service);
    let (id, answer) = {
        let serving = state.services.serving.lock().unwrap();
        let served = serving.get(&service);
        let open = match (spec, served) {
            (Some(spec), _) => spec.agent,
            (None, Some(served)) => served.level == Level::Agent,
            (None, None) => return Err(E::missing(format!("service {service}"))),
        };
        if !caller.host() && !open {
            return Err(E::forbidden(format!("The agent may not call {service}")));
        }
        let served =
            served.ok_or_else(|| E(code::NOT_SERVED, format!("{service} is not running")))?;
        let id = state.services.next.fetch_add(1, Ordering::Relaxed) + 1;
        let (reply, answer) = oneshot::channel();
        served.pending.lock().unwrap().insert(id, reply);
        let frame = CallFrame {
            id,
            method,
            body: body.map_or(serde_json::Value::Null, |Json(body)| body),
            from: crate::channel::from(&state, &caller),
        };
        if served.calls.send(frame).is_err() {
            return Err(E(code::NOT_SERVED, format!("{service} is not running")));
        }
        (id, answer)
    };
    let seconds = spec
        .and_then(|spec| spec.timeout)
        .unwrap_or(30)
        .clamp(1, 3600);
    match tokio::time::timeout(Duration::from_secs(seconds), answer).await {
        Err(_) => {
            if let Some(served) = state.services.serving.lock().unwrap().get(&service) {
                served.pending.lock().unwrap().remove(&id);
            }
            Err(E(
                code::TIMEOUT,
                format!("{service} did not answer within {seconds} seconds"),
            ))
        }
        Ok(Err(_)) => Err(E(
            code::NOT_SERVED,
            format!("{service} went away before it answered"),
        )),
        Ok(Ok(ReplyFrame {
            error: Some(error), ..
        })) => Err(E(code::SERVICE, error.message)),
        Ok(Ok(reply)) => Ok(Json(reply.result.unwrap_or(serde_json::Value::Null))),
    }
}

/// The socket of the program that answers a service's calls.
pub async fn serve(
    AppState(state): AppState<Arc<State>>,
    Extension(caller): Extension<Caller>,
    Path((plugin, name)): Path<(String, String)>,
    upgrade: ws::WebSocketUpgrade,
) -> R<Response> {
    if !valid_part(&plugin) || !valid_part(&name) {
        return Err(E::bad(
            "A service is named PLUGIN/NAME in letters, digits, - _ and .",
        ));
    }
    let service = format!("{plugin}/{name}");
    let config = state.config();
    if let Some(spec) = config.services.iter().find(|spec| spec.name == service)
        && state.mode == Mode::System
    {
        let owner = match spec.user.as_str() {
            "root" => Some(0),
            _ => crate::users::account(&config.berthd.agent).map(|account| account.uid),
        };
        if owner != Some(caller.uid) {
            return Err(E::forbidden(format!(
                "{service} is served by {}",
                spec.user
            )));
        }
    }
    let (calls, mut queue) = mpsc::unbounded_channel();
    let pending: Pending = Default::default();
    {
        let mut serving = state.services.serving.lock().unwrap();
        if serving.contains_key(&service) {
            return Err(E(code::CONFLICT, format!("{service} is served already")));
        }
        serving.insert(
            service.clone(),
            Serving {
                uid: caller.uid,
                level: caller.level,
                calls,
                pending: pending.clone(),
            },
        );
    }
    Ok(upgrade.on_upgrade(move |socket| async move {
        let (mut sink, mut source) = socket.split();
        loop {
            tokio::select! {
                call = queue.recv() => {
                    let Some(call) = call else { break };
                    let text = serde_json::to_string(&call).unwrap_or_default();
                    if sink.send(ws::Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                frame = source.next() => match frame {
                    Some(Ok(ws::Message::Text(text))) => {
                        if let Ok(reply) = serde_json::from_str::<ReplyFrame>(&text)
                            && let Some(waiting) = pending.lock().unwrap().remove(&reply.id)
                        {
                            let _ = waiting.send(reply);
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                },
            }
        }
        // Whoever still waits learns that it went away.
        state.services.serving.lock().unwrap().remove(&service);
        pending.lock().unwrap().clear();
    }))
}
