//! The routes of `berth serve`.

use super::{
    Event, Remote, Serve,
    auth::{self, Access},
    provisioners, setup, setup_key,
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{
        DefaultBodyLimit, Extension, Path, Query, RawQuery, Request, State,
        ws::{
            Message as WsMessage, WebSocket, WebSocketUpgrade, rejection::WebSocketUpgradeRejection,
        },
    },
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{any, get},
};
use berth_api::{
    Applied, Behind, Done, ErrorBody, ErrorDetail, HostBehind, Message, Output, Rollout,
    RolloutReport, Version, b64, client, code,
    ops::{self, Role},
    streams,
};
use berth_core::{
    apply,
    host::Host,
    rollout,
    store::{self, Filter, When},
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{io::Write, sync::Arc, time::Duration};
use tokio::sync::broadcast::error::RecvError;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};

pub(super) type App = State<Arc<Serve>>;
/// The upgrade when the request asks for a stream, at a path that also answers plainly.
type Ws = std::result::Result<WebSocketUpgrade, WebSocketUpgradeRejection>;

/// An error on its way out: status, code, message.
pub struct Fail(u16, String, String);

impl Fail {
    pub fn new(kind: (u16, &str), message: impl Into<String>) -> Fail {
        Fail(kind.0, kind.1.to_owned(), message.into())
    }
}

impl IntoResponse for Fail {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.0).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = ErrorBody {
            error: ErrorDetail {
                code: self.1,
                message: self.2,
            },
        };
        (status, Json(body)).into_response()
    }
}

impl From<store::Error> for Fail {
    fn from(error: store::Error) -> Fail {
        let kind = match error {
            store::Error::Conflict(_) => code::PRECONDITION,
            store::Error::TooLarge => code::TOO_LARGE,
            store::Error::Database(_) => code::INTERNAL,
        };
        Fail::new(kind, error.to_string())
    }
}

impl From<client::Error> for Fail {
    fn from(error: client::Error) -> Fail {
        Fail(error.status(), error.code().to_owned(), error.to_string())
    }
}

pub(super) type Answer<T> = std::result::Result<T, Fail>;

pub(super) fn internal(error: impl std::fmt::Display) -> Fail {
    Fail::new(code::INTERNAL, error.to_string())
}

// ── What the answers look like ──────────────────────────────────────────────

#[derive(Serialize, ToSchema)]
pub struct HostInfo {
    pub name: String,
    /// Its SSH address.
    pub address: String,
    pub reachable: bool,
    /// The version of its berthd.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api: Option<u32>,
    /// Why it is not reachable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The berth version its configuration was last applied with, when its
    /// berthd says. An older berthd does not, nor does a host not applied
    /// since.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied: Option<String>,
    /// What changed since then, when the server would apply something else now.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub behind: Option<Behind>,
}

#[derive(Serialize, ToSchema)]
pub struct JobInfo {
    pub id: String,
    pub host: String,
    /// What it does: `apply`, `rollout`, `provision` or `unprovision`.
    pub what: String,
    pub started: String,
    pub ended: Option<String>,
    pub exit_code: Option<i32>,
    /// How a roll-out went, host by host, once it has ended.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<RolloutReport>,
}

/// A stored message: the host's message with where it came from.
#[derive(Serialize, ToSchema)]
pub struct InboxMessage {
    #[serde(flatten)]
    pub message: Message,
    pub host: String,
    pub done: bool,
    /// The order it was taken in; what `before` counts in.
    pub seq: i64,
}

#[derive(Serialize, ToSchema)]
pub struct Inbox {
    pub messages: Vec<InboxMessage>,
    /// Pass as `before` for older ones, when there are more.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<i64>,
}

#[derive(Serialize, ToSchema)]
pub struct KvEntry {
    pub key: String,
    pub value: Value,
    pub revision: i64,
    pub updated: String,
}

#[derive(Serialize, ToSchema)]
pub struct KvList {
    pub entries: Vec<KvEntry>,
}

#[derive(Serialize, ToSchema)]
pub struct KvWritten {
    pub key: String,
    pub revision: i64,
}

/// Where a roll-out keeps its report: beside its output.
fn report_path(output: &str) -> std::path::PathBuf {
    std::path::Path::new(output).with_extension("json")
}

fn job_info(job: store::Job) -> JobInfo {
    let report = std::fs::read(report_path(&job.output_path))
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok());
    JobInfo {
        report,
        id: job.id,
        host: job.host,
        what: job.what,
        started: job.started,
        ended: job.ended,
        exit_code: job.exit_code,
    }
}

fn kv_entry(entry: store::Entry) -> KvEntry {
    KvEntry {
        key: entry.key,
        value: entry.value,
        revision: entry.revision,
        updated: entry.updated,
    }
}

// ── The router ──────────────────────────────────────────────────────────────

/// The routes that are plain requests, with their descriptions.
pub fn documented() -> OpenApiRouter<Arc<Serve>> {
    guarded_routes().merge(keyed_routes())
}

/// The routes that check a key of their own, for a caller without a token.
fn keyed_routes() -> OpenApiRouter<Arc<Serve>> {
    OpenApiRouter::new().routes(routes!(setup_key::check, setup_key::claim, setup_key::done))
}

/// The routes that over TCP are for a caller with a token and a role.
fn guarded_routes() -> OpenApiRouter<Arc<Serve>> {
    OpenApiRouter::new()
        .routes(routes!(version))
        .routes(routes!(access))
        .routes(routes!(hosts))
        .routes(routes!(host, setup::remove_host))
        .routes(routes!(setup::profile, setup::save_profile))
        .routes(routes!(apply_host))
        .routes(routes!(rollout_hosts))
        .routes(routes!(setup::seed))
        .routes(routes!(setup::send_secrets))
        .routes(routes!(setup::secrets))
        .routes(routes!(setup::set_secret, setup::remove_secret))
        .routes(routes!(setup::registries))
        .routes(routes!(setup::save_registry, setup::remove_registry))
        .routes(routes!(setup::check_registry))
        .routes(routes!(setup::settings))
        .routes(routes!(setup::set_setting))
        .routes(routes!(setup::plugins, setup::add_plugin))
        .routes(routes!(setup::upload_plugin))
        .routes(routes!(setup::remove_plugin))
        .routes(routes!(setup::check_plugins))
        .routes(routes!(setup::update_plugins))
        .routes(routes!(setup::catalogue))
        .routes(routes!(setup::generate))
        .routes(routes!(provisioners::list))
        .routes(routes!(provisioners::instances))
        .routes(routes!(
            provisioners::get,
            provisioners::create,
            provisioners::destroy
        ))
        .routes(routes!(provisioners::power))
        .routes(routes!(jobs))
        .routes(routes!(job))
        .routes(routes!(job_output))
        .routes(routes!(inbox))
        .routes(routes!(mark_done, mark_open))
        .routes(routes!(kv_list))
        .routes(routes!(kv_get, kv_put, kv_delete))
}

pub fn router(state: Arc<Serve>, guarded: bool) -> Router {
    let (mut router, _) = guarded_routes().split_for_parts();
    // Every stream of the table gets its route here, so none can exist undescribed.
    for stream in streams::SERVE {
        router = match stream.id {
            // At the path of a plain GET, whose handler upgrades when asked to.
            "job" | "inbox" | "kv" => router,
            "events" => router.route(stream.path, get(events)),
            other => panic!("no handler for the stream {other}"),
        };
    }
    router = router.route("/v1/hosts/{name}/{*rest}", any(proxy));
    router = router.route(ops::PORTAL, get(portal));
    if guarded {
        router = router.layer(middleware::from_fn_with_state(state.clone(), guard));
        // Asked before a caller has a token: which issuer gives one.
        if let Some(resource) = state.auth.resource() {
            router = router.route(
                berth_core::oauth::RESOURCE,
                get(move || std::future::ready(Json(resource.clone()))),
            );
        }
    }
    // Added after the guard, which they are not behind: they check their key themselves.
    router = router.merge(keyed_routes().split_for_parts().0);
    router
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        .with_state(state)
}

/// What a request shows to be let in: a bearer, and the protocols of a
/// WebSocket, because a browser cannot set a header on one.
fn shown(headers: &HeaderMap) -> Vec<String> {
    let header = |name| {
        headers
            .get(name)
            .and_then(|value: &HeaderValue| value.to_str().ok())
    };
    let bearer = header(header::AUTHORIZATION).and_then(|value| value.strip_prefix("Bearer "));
    let protocols = header(header::SEC_WEBSOCKET_PROTOCOL)
        .into_iter()
        .flat_map(|value| value.split(','));
    bearer
        .into_iter()
        .chain(protocols)
        .map(|given| given.trim().to_owned())
        .filter(|given| !given.is_empty())
        .collect()
}

/// Over TCP every request shows who it is from, and is one the caller's role
/// allows. What a handshake offered as its protocol is named in the answer.
async fn guard(State(state): App, mut request: Request, next: Next) -> Response {
    let refuse = |(kind, message): auth::Refusal| Fail::new(kind, message).into_response();
    let (access, given) = match state.auth.admit(&shown(request.headers())).await {
        Ok(admitted) => admitted,
        Err(refusal) => return refuse(refusal),
    };
    let stream = request
        .headers()
        .get(header::UPGRADE)
        .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"websocket"));
    let allowed = match auth::operation(request.method().as_str(), request.uri().path(), stream) {
        Ok(Some(op)) => state.auth.allows(&access, op),
        // The portal and the documents: each operation on the portal is asked about there.
        Ok(None) => Ok(()),
        Err(()) if state.auth.role(&access) == Role::Admin => Ok(()),
        Err(()) => Err((
            code::FORBIDDEN,
            "No operation is known for this request, which only an admin may then send".into(),
        )),
    };
    if let Err(refusal) = allowed {
        return refuse(refusal);
    }
    let offered = request
        .headers()
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .is_some();
    request.extensions_mut().insert(access);
    let mut response = next.run(request).await;
    if offered
        && response.status() == StatusCode::SWITCHING_PROTOCOLS
        && let Ok(value) = HeaderValue::from_str(&given)
    {
        response
            .headers_mut()
            .insert(header::SEC_WEBSOCKET_PROTOCOL, value);
    }
    response
}

/// Who a request is from: whoever the guard let in, or this machine's user.
pub fn caller(access: Option<Extension<Access>>) -> Access {
    access.map_or_else(Access::local, |Extension(access)| access)
}

/// The portal: every operation over one WebSocket. With `host` an operation
/// is that host's, and goes the way a request under `/v1/hosts/NAME` does.
async fn portal(
    State(state): App,
    access: Option<Extension<Access>>,
    ws: WebSocketUpgrade,
) -> Response {
    // Who it is was shown when the portal was opened; what they may do is
    // asked for each operation. The operations see the caller as they would
    // over HTTP, so `access` names its role and not this machine's user's.
    let inner = match &access {
        Some(Extension(access)) => router(state.clone(), false).layer(Extension(access.clone())),
        None => router(state.clone(), false),
    };
    let access = caller(access);
    let asked = state.clone();
    let resolve: berthd::portal::Resolve = Arc::new(move |name, host| {
        let (table, prefix) = match host {
            Some(host) => (ops::HOST, format!("/hosts/{}", client::encode(host))),
            None => (ops::SERVE, String::new()),
        };
        let op = ops::find(table, name)
            .ok_or_else(|| (code::NOT_FOUND, format!("No operation {name}")))?;
        asked.auth.allows(&access, op)?;
        Ok(berthd::portal::Target { op, prefix })
    });
    ws.on_upgrade(move |socket| berthd::portal::run(socket, inner, resolve))
}

#[derive(Serialize, ToSchema)]
pub struct AccessInfo {
    /// `view`, `operate` or `admin`: each includes the ones before.
    pub role: String,
    /// Whether an admin may set the server up over the network.
    pub setup: bool,
    /// Whose access token it is, when it is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
}

#[utoipa::path(get, path = "/v1/access", summary = "What the caller may do here.", tag = "serve",
    responses((status = 200, description = "The caller's role", body = AccessInfo)))]
async fn access(State(state): App, access: Option<Extension<Access>>) -> Json<AccessInfo> {
    let local = access.is_none();
    let access = caller(access);
    Json(AccessInfo {
        role: state.auth.role(&access).name().into(),
        setup: local || state.auth.setup(),
        subject: access.subject,
    })
}

// ── Hosts ───────────────────────────────────────────────────────────────────

/// What the server would apply now, to compare a host with.
pub(super) type Compare = Arc<dyn Fn(&Applied) -> Option<Behind> + Send + Sync>;

pub(super) fn compare(state: &Serve) -> Compare {
    Arc::new(rollout::comparer(&state.paths))
}

async fn info(remote: Arc<Remote>, compare: Compare) -> HostInfo {
    let seen = remote.version().await;
    let applied = seen
        .as_ref()
        .ok()
        .and_then(|version| version.applied.clone());
    HostInfo {
        name: remote.name.clone(),
        address: remote.address.clone(),
        reachable: seen.is_ok(),
        version: seen.as_ref().ok().map(|version| version.version.clone()),
        api: seen.as_ref().ok().map(|version| version.api),
        error: seen.err(),
        behind: applied.as_ref().and_then(|applied| compare(applied)),
        applied: applied.map(|applied| applied.berth),
    }
}

/// The server's hosts that are behind, asked all at once.
pub(super) async fn behind_hosts(state: &Arc<Serve>) -> Vec<HostBehind> {
    state.refresh(false);
    let compare = compare(state);
    let remotes: Vec<_> = state.hosts.read().unwrap().values().cloned().collect();
    let infos = futures_util::future::join_all(
        remotes
            .into_iter()
            .map(|remote| info(remote, compare.clone())),
    )
    .await;
    infos
        .into_iter()
        .filter_map(|info| {
            Some(HostBehind {
                name: info.name,
                behind: info.behind?,
            })
        })
        .collect()
}

pub(super) fn remote(state: &Serve, name: &str) -> Answer<Arc<Remote>> {
    state
        .host(name)
        .ok_or_else(|| Fail::new(code::NOT_FOUND, format!("No host {name}")))
}

#[utoipa::path(get, path = "/v1/version", summary = "The version of berth serve and of its API.", tag = "serve",
    responses((status = 200, description = "The version of berth serve", body = Version)))]
async fn version() -> Json<Version> {
    Json(Version {
        version: berth_api::VERSION.into(),
        api: berth_api::API,
        mode: "serve".into(),
        applied: None,
    })
}

#[utoipa::path(get, path = "/v1/hosts", summary = "The saved hosts and whether each answers.", tag = "hosts",
    responses((status = 200, description = "The saved hosts and whether each answers", body = Vec<HostInfo>)))]
async fn hosts(State(state): App) -> Json<Vec<HostInfo>> {
    state.refresh(false);
    let compare = compare(&state);
    let remotes: Vec<_> = state.hosts.read().unwrap().values().cloned().collect();
    Json(
        futures_util::future::join_all(
            remotes
                .into_iter()
                .map(|remote| info(remote, compare.clone())),
        )
        .await,
    )
}

#[utoipa::path(get, path = "/v1/hosts/{name}", summary = "One host: its address, and whether it answers.", tag = "hosts",
    params(("name" = String, Path, description = "A saved host")),
    responses((status = 200, description = "One host", body = HostInfo),
              (status = 404, description = "No such host", body = ErrorBody)))]
async fn host(State(state): App, Path(name): Path<String>) -> Answer<Json<HostInfo>> {
    state.refresh(false);
    Ok(Json(info(remote(&state, &name)?, compare(&state)).await))
}

/// What applying may be given.
#[derive(Deserialize, ToSchema, Default)]
pub struct ApplyWith {
    /// The SSH user's sudo password, for a host that asks for one.
    pub sudo_password: Option<String>,
}

fn new_id() -> std::io::Result<String> {
    let mut bytes = [0u8; 12];
    std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut bytes)?;
    let letters = b"abcdefghijklmnopqrstuvwxyz234567";
    Ok(format!(
        "j{}",
        bytes
            .iter()
            .map(|byte| letters[(byte & 31) as usize] as char)
            .collect::<String>()
    ))
}

#[utoipa::path(post, path = "/v1/hosts/{name}/apply", summary = "Apply the host's profile to it, as a job.", tag = "hosts",
    params(("name" = String, Path, description = "A saved host")),
    request_body(content = Option<ApplyWith>, description = "Nothing, or what the run needs that the server does not have"),
    responses((status = 200, description = "The job that applies the host's configuration", body = JobInfo),
              (status = 404, description = "No such host", body = ErrorBody)))]
async fn apply_host(
    State(state): App,
    Path(name): Path<String>,
    body: Bytes,
) -> Answer<Json<JobInfo>> {
    state.refresh(false);
    let remote = remote(&state, &name)?;
    let bad = |error: Box<dyn std::error::Error>| Fail::new(code::BAD_REQUEST, error.to_string());
    let with: ApplyWith = match body.is_empty() {
        true => ApplyWith::default(),
        false => serde_json::from_slice(&body)
            .map_err(|error| Fail::new(code::BAD_REQUEST, error.to_string()))?,
    };
    let host = Host::load(&state.paths, &name).map_err(bad)?;
    let password = with.sudo_password.or_else(|| remote.password.clone());
    if host.asks_password() && password.is_none() {
        return Err(Fail::new(
            code::NOT_AVAILABLE,
            format!("{name} asks for a sudo password: send it as sudo_password"),
        ));
    }
    if host.config["builder"]["ask_pass"]
        .as_bool()
        .unwrap_or(false)
    {
        return Err(Fail::new(
            code::NOT_AVAILABLE,
            format!(
                "{name} is set to log in with an SSH password, which a job cannot type: give the server a key for it"
            ),
        ));
    }
    let prepared = apply::prepare(&state.paths, &host, password.as_deref()).map_err(bad)?;
    let id = new_id().map_err(internal)?;
    std::fs::create_dir_all(&state.jobs_dir).map_err(internal)?;
    let path = state.jobs_dir.join(format!("{id}.log"));
    let output = std::fs::File::create(&path).map_err(internal)?;
    let mut command = tokio::process::Command::from(prepared.command);
    command
        .stdin(std::process::Stdio::null())
        .stdout(output.try_clone().map_err(internal)?)
        .stderr(output);
    let mut child = command
        .spawn()
        .map_err(|error| internal(format!("ansible-playbook: {error}")))?;
    let job = store::Job {
        id: id.clone(),
        host: name,
        what: "apply".into(),
        started: berth_api::now(),
        ended: None,
        exit_code: None,
        output_path: path.to_string_lossy().into_owned(),
    };
    state.store.lock().unwrap().add_job(&job)?;
    let _ = state.events.send(Event::Job(job.json()));
    let files = prepared.files;
    let watcher = state.clone();
    tokio::spawn(async move {
        let exit_code = child.wait().await.ok().and_then(|status| status.code());
        // The private files the run read live until here.
        drop(files);
        let mut store = watcher.store.lock().unwrap();
        let _ = store.end_job(&id, exit_code);
        if let Ok(Some(job)) = store.job(&id) {
            let _ = watcher.events.send(Event::Job(job.json()));
        }
    });
    Ok(Json(job_info(job)))
}

#[utoipa::path(post, path = "/v1/rollout", summary = "Bring hosts up to date one after another, as a job: apply each that is behind, maybe run its update and reboot it, and check it. The first that fails stops the rest.", tag = "hosts",
    request_body(content = Option<Rollout>, description = "Which hosts and what to do; by default the saved hosts that are behind, applied only"),
    responses((status = 200, description = "The job; its report says how each host went once it has ended", body = JobInfo),
              (status = 400, description = "The body is not a roll-out", body = ErrorBody),
              (status = 404, description = "No such host", body = ErrorBody)))]
async fn rollout_hosts(State(state): App, body: Bytes) -> Answer<Json<JobInfo>> {
    state.refresh(false);
    let options: Rollout = match body.is_empty() {
        true => Rollout::default(),
        false => serde_json::from_slice(&body)
            .map_err(|error| Fail::new(code::BAD_REQUEST, error.to_string()))?,
    };
    let known: Vec<String> = state.hosts.read().unwrap().keys().cloned().collect();
    let names = rollout::chosen(known, &options.hosts)
        .map_err(|error| Fail::new(code::NOT_FOUND, error.to_string()))?;
    let id = new_id().map_err(internal)?;
    std::fs::create_dir_all(&state.jobs_dir).map_err(internal)?;
    let path = state.jobs_dir.join(format!("{id}.log"));
    let mut output = std::fs::File::create(&path).map_err(internal)?;
    let job = store::Job {
        id: id.clone(),
        host: names.join(", "),
        what: "rollout".into(),
        started: berth_api::now(),
        ended: None,
        exit_code: None,
        output_path: path.to_string_lossy().into_owned(),
    };
    state.store.lock().unwrap().add_job(&job)?;
    let _ = state.events.send(Event::Job(job.json()));
    let passwords: Vec<Option<String>> = names
        .iter()
        .map(|name| state.host(name).and_then(|remote| remote.password.clone()))
        .collect();
    let watcher = state.clone();
    // Its own thread: the roll-out waits on hosts with a runtime of its own.
    std::thread::spawn(move || {
        let paths = &watcher.paths;
        let mut targets: Vec<Box<dyn rollout::Target + '_>> = vec![];
        let mut problem = None;
        for (name, password) in names.iter().zip(passwords) {
            match Host::load(paths, name).and_then(|host| rollout::Live::new(paths, host, password))
            {
                Ok(target) => targets.push(Box::new(target)),
                Err(error) => problem = Some(format!("✗ {name}: {error}")),
            }
        }
        let report = match problem {
            Some(problem) => {
                let _ = writeln!(output, "{problem}");
                RolloutReport::default()
            }
            None => {
                let compare = rollout::comparer(paths);
                rollout::run(&mut targets, &options, &compare, &mut output)
            }
        };
        drop(targets);
        let _ = std::fs::write(
            report_path(&path.to_string_lossy()),
            serde_json::to_vec(&report).unwrap_or_default(),
        );
        let mut store = watcher.store.lock().unwrap();
        let _ = store.end_job(&id, Some(if report.ok { 0 } else { 1 }));
        if let Ok(Some(job)) = store.job(&id) {
            let _ = watcher.events.send(Event::Job(job.json()));
        }
    });
    Ok(Json(job_info(job)))
}

/// Start a job of `what` for a host that runs `work` in a thread of its own,
/// with its output in the job's log. It ends with 0 when `work` says it went
/// well, and with 1 otherwise.
pub(super) fn start_job(
    state: &Arc<Serve>,
    host: &str,
    what: &str,
    work: impl FnOnce(&mut std::fs::File) -> bool + Send + 'static,
) -> Answer<JobInfo> {
    let id = new_id().map_err(internal)?;
    std::fs::create_dir_all(&state.jobs_dir).map_err(internal)?;
    let path = state.jobs_dir.join(format!("{id}.log"));
    let mut output = std::fs::File::create(&path).map_err(internal)?;
    let job = store::Job {
        id: id.clone(),
        host: host.to_owned(),
        what: what.to_owned(),
        started: berth_api::now(),
        ended: None,
        exit_code: None,
        output_path: path.to_string_lossy().into_owned(),
    };
    state.store.lock().unwrap().add_job(&job)?;
    let _ = state.events.send(Event::Job(job.json()));
    let watcher = state.clone();
    std::thread::spawn(move || {
        let ok = work(&mut output);
        let mut store = watcher.store.lock().unwrap();
        let _ = store.end_job(&id, Some(if ok { 0 } else { 1 }));
        if let Ok(Some(job)) = store.job(&id) {
            let _ = watcher.events.send(Event::Job(job.json()));
        }
    });
    Ok(job_info(job))
}

// ── Jobs ────────────────────────────────────────────────────────────────────

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
struct Limit {
    /// At most this many (default 100, at most 1,000).
    limit: Option<usize>,
}

#[utoipa::path(get, path = "/v1/jobs", summary = "The jobs, newest first.", tag = "jobs", params(Limit),
    responses((status = 200, description = "Jobs, newest first", body = Vec<JobInfo>)))]
async fn jobs(State(state): App, Query(query): Query<Limit>) -> Answer<Json<Vec<JobInfo>>> {
    let jobs = state
        .store
        .lock()
        .unwrap()
        .jobs(query.limit.unwrap_or(100))?;
    Ok(Json(jobs.into_iter().map(job_info).collect()))
}

fn find_job(state: &Serve, id: &str) -> Answer<store::Job> {
    let job = state.store.lock().unwrap().job(id)?;
    job.ok_or_else(|| Fail::new(code::NOT_FOUND, format!("No job {id}")))
}

#[derive(Deserialize)]
struct Since {
    since: Option<u64>,
}

#[utoipa::path(get, path = "/v1/jobs/{id}", summary = "One job's state.", tag = "jobs",
    params(("id" = String, Path, description = "A job's id")),
    responses((status = 200, description = "One job", body = JobInfo),
              (status = 101, description = "As a WebSocket: its output as it is produced"),
              (status = 404, description = "No such job", body = ErrorBody)))]
async fn job(
    State(state): App,
    Path(id): Path<String>,
    Query(query): Query<Since>,
    ws: Ws,
) -> Answer<Response> {
    let job = find_job(&state, &id)?;
    Ok(match ws {
        Ok(ws) => {
            let offset = query.since.unwrap_or(0);
            ws.on_upgrade(move |socket| follow_job(state, id, offset, socket))
        }
        Err(_) => Json(job_info(job)).into_response(),
    })
}

/// A job's output from an offset, then what it writes, then how it ended.
async fn follow_job(state: Arc<Serve>, id: String, mut offset: u64, mut socket: WebSocket) {
    loop {
        // Whether it has ended is read first, so nothing written before the end is missed.
        let Ok(Some(job)) = state.store.lock().unwrap().job(&id) else {
            return;
        };
        let data = read_from(&job.output_path, offset);
        if !data.is_empty() {
            offset += data.len() as u64;
            if socket.send(WsMessage::Binary(data.into())).await.is_err() {
                return;
            }
            continue;
        }
        if job.ended.is_some() {
            let last = json!({"exited": {"exit_code": job.exit_code}}).to_string();
            let _ = socket.send(WsMessage::Text(last.into())).await;
            let _ = socket.send(WsMessage::Close(None)).await;
            return;
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
            frame = socket.recv() => if !matches!(frame, Some(Ok(_))) { return },
        }
    }
}

fn read_from(path: &str, offset: u64) -> Vec<u8> {
    use std::io::{Read, Seek, SeekFrom};
    let mut data = vec![];
    if let Ok(mut file) = std::fs::File::open(path)
        && file.seek(SeekFrom::Start(offset)).is_ok()
    {
        let _ = file.take(1024 * 1024).read_to_end(&mut data);
    }
    data
}

#[derive(Deserialize, IntoParams, Default)]
#[into_params(parameter_in = Query)]
pub struct OutputQuery {
    /// By bytes: start at this offset.
    pub since: Option<u64>,
    /// By bytes: at most this many (default and most 1 MB).
    pub limit: Option<u64>,
    /// By lines: start at this line, from 0.
    pub line: Option<u64>,
    /// By lines: this many (default 100).
    pub count: Option<u64>,
    /// The last lines.
    pub tail: Option<u64>,
}

/// A piece of output by bytes or by lines, with where to go on in both.
pub fn piece(data: &[u8], query: &OutputQuery, ended: bool) -> Output {
    let total = data.len() as u64;
    // Where each line starts; a last line without its end is not one yet.
    let mut starts = vec![0u64];
    starts.extend(
        data.iter()
            .enumerate()
            .filter(|(_, byte)| **byte == b'\n')
            .map(|(at, _)| at as u64 + 1),
    );
    let complete = starts.len() as u64 - 1;
    let by_lines = query.tail.is_some() || query.line.is_some();
    let (from, to) = if let Some(tail) = query.tail {
        (
            starts[complete.saturating_sub(tail) as usize],
            starts[complete as usize],
        )
    } else if let Some(line) = query.line {
        let first = line.min(complete);
        let last = first
            .saturating_add(query.count.unwrap_or(100))
            .min(complete);
        (starts[first as usize], starts[last as usize])
    } else {
        let from = query.since.unwrap_or(0).min(total);
        (
            from,
            from.saturating_add(query.limit.unwrap_or(1 << 20).min(1 << 20))
                .min(total),
        )
    };
    let chunk = &data[from as usize..to as usize];
    Output {
        data_b64: b64(chunk),
        lines: by_lines.then(|| {
            String::from_utf8_lossy(chunk)
                .lines()
                .map(str::to_owned)
                .collect()
        }),
        first_offset: None,
        offset: from,
        next_offset: to,
        next_line: starts.iter().skip(1).filter(|start| **start <= to).count() as u64,
        ended: ended && to == total,
    }
}

#[utoipa::path(get, path = "/v1/jobs/{id}/output", summary = "What a job wrote, by bytes or by lines.", tag = "jobs",
    params(("id" = String, Path, description = "A job's id"), OutputQuery),
    responses((status = 200, description = "A piece of the job's output, by bytes or by lines", body = Output),
              (status = 404, description = "No such job", body = ErrorBody)))]
async fn job_output(
    State(state): App,
    Path(id): Path<String>,
    Query(query): Query<OutputQuery>,
) -> Answer<Json<Output>> {
    let job = find_job(&state, &id)?;
    let data = std::fs::read(&job.output_path).unwrap_or_default();
    Ok(Json(piece(&data, &query, job.ended.is_some())))
}

// ── Inbox ───────────────────────────────────────────────────────────────────

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
struct InboxQuery {
    /// Only this host.
    host: Option<String>,
    /// Only topics starting with this.
    topic: Option<String>,
    /// Only messages taken in before this `seq`.
    before: Option<i64>,
    /// Only done ones, or only those that are not.
    done: Option<bool>,
    /// At most this many (default 100, at most 1,000).
    limit: Option<usize>,
}

fn inbox_message(stored: store::Stored) -> InboxMessage {
    InboxMessage {
        message: stored.message,
        host: stored.host,
        done: stored.done,
        seq: stored.seq,
    }
}

#[utoipa::path(get, path = "/v1/inbox", summary = "Messages taken in from the hosts' channels.", tag = "inbox", params(InboxQuery),
    responses((status = 200, description = "Stored messages, newest first", body = Inbox),
              (status = 101, description = "As a WebSocket: messages as they are taken in")))]
async fn inbox(State(state): App, Query(query): Query<InboxQuery>, ws: Ws) -> Answer<Response> {
    if let Ok(ws) = ws {
        let events = state.events.subscribe();
        return Ok(ws.on_upgrade(move |socket| {
            forward(socket, events, move |event| match event {
                Event::Inbox(stored)
                    if query.host.as_ref().is_none_or(|host| *host == stored.host)
                        && stored
                            .message
                            .topic
                            .starts_with(query.topic.as_deref().unwrap_or("")) =>
                {
                    Some(stored.json())
                }
                _ => None,
            })
        }));
    }
    let filter = Filter {
        host: query.host.as_deref(),
        topic: query.topic.as_deref(),
        before: query.before,
        done: query.done,
        limit: query.limit.unwrap_or(100),
    };
    let (messages, next) = state.store.lock().unwrap().inbox(&filter)?;
    Ok(Json(Inbox {
        messages: messages.into_iter().map(inbox_message).collect(),
        next,
    })
    .into_response())
}

/// Send what `pick` makes of each event until the client goes away.
async fn forward(
    mut socket: WebSocket,
    mut events: tokio::sync::broadcast::Receiver<Event>,
    pick: impl Fn(Event) -> Option<Value>,
) {
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Ok(event) => {
                    if let Some(frame) = pick(event)
                        && socket.send(WsMessage::Text(frame.to_string().into())).await.is_err()
                    {
                        return;
                    }
                }
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => return,
            },
            frame = socket.recv() => if !matches!(frame, Some(Ok(_))) { return },
        }
    }
}

fn mark(state: &Serve, host: &str, id: u64, done: bool) -> Answer<Json<Done>> {
    if !state.store.lock().unwrap().set_done(host, id, done)? {
        return Err(Fail::new(
            code::NOT_FOUND,
            format!("No message {id} from {host}"),
        ));
    }
    Ok(Json(Done { ok: true }))
}

#[utoipa::path(post, path = "/v1/inbox/{host}/{id}/done", summary = "Mark a message as done.", tag = "inbox",
    params(("host" = String, Path, description = "The host it came from"), ("id" = u64, Path, description = "The message's id on that host")),
    responses((status = 200, description = "Marked done", body = Done),
              (status = 404, description = "No such message", body = ErrorBody)))]
async fn mark_done(State(state): App, Path((host, id)): Path<(String, u64)>) -> Answer<Json<Done>> {
    mark(&state, &host, id, true)
}

#[utoipa::path(delete, path = "/v1/inbox/{host}/{id}/done", summary = "Mark a message as open again.", tag = "inbox",
    params(("host" = String, Path, description = "The host it came from"), ("id" = u64, Path, description = "The message's id on that host")),
    responses((status = 200, description = "No longer marked done", body = Done),
              (status = 404, description = "No such message", body = ErrorBody)))]
async fn mark_open(State(state): App, Path((host, id)): Path<(String, u64)>) -> Answer<Json<Done>> {
    mark(&state, &host, id, false)
}

// ── Key-value ───────────────────────────────────────────────────────────────

#[utoipa::path(get, path = "/v1/kv/{namespace}", summary = "The keys of a namespace with their values.", tag = "kv",
    params(("namespace" = String, Path, description = "A namespace")),
    responses((status = 200, description = "The namespace's keys with their values", body = KvList),
              (status = 101, description = "As a WebSocket: changes to the namespace")))]
async fn kv_list(State(state): App, Path(namespace): Path<String>, ws: Ws) -> Answer<Response> {
    if let Ok(ws) = ws {
        let events = state.events.subscribe();
        return Ok(ws.on_upgrade(move |socket| {
            forward(socket, events, move |event| match event {
                Event::Kv {
                    namespace: changed,
                    key,
                    value: Some(value),
                    revision,
                } if changed == namespace => {
                    Some(json!({"key": key, "value": value, "revision": revision}))
                }
                Event::Kv {
                    namespace: changed,
                    key,
                    value: None,
                    ..
                } if changed == namespace => Some(json!({"key": key, "removed": true})),
                _ => None,
            })
        }));
    }
    let entries = state.store.lock().unwrap().list(&namespace)?;
    Ok(Json(KvList {
        entries: entries.into_iter().map(kv_entry).collect(),
    })
    .into_response())
}

fn etag(revision: i64) -> [(header::HeaderName, String); 1] {
    [(header::ETAG, format!("\"{revision}\""))]
}

/// `If-Match: REVISION` and `If-None-Match: *` as the condition of a write.
fn condition(headers: &HeaderMap) -> Answer<When> {
    let text = |name| {
        headers
            .get(name)
            .and_then(|value: &HeaderValue| value.to_str().ok())
    };
    if let Some(value) = text(header::IF_MATCH) {
        let revision = value
            .trim()
            .trim_start_matches("W/")
            .trim_matches('"')
            .parse();
        return revision
            .map(When::At)
            .map_err(|_| Fail::new(code::BAD_REQUEST, "If-Match takes a revision"));
    }
    match text(header::IF_NONE_MATCH) {
        Some(value) if value.trim() == "*" => Ok(When::New),
        Some(_) => Err(Fail::new(code::BAD_REQUEST, "If-None-Match takes only *")),
        None => Ok(When::Always),
    }
}

#[utoipa::path(get, path = "/v1/kv/{namespace}/{key}", summary = "One key's value and revision.", tag = "kv",
    params(("namespace" = String, Path, description = "A namespace"), ("key" = String, Path, description = "A key")),
    responses((status = 200, description = "The value and its revision, which is also the ETag", body = KvEntry),
              (status = 404, description = "No such key", body = ErrorBody)))]
async fn kv_get(
    State(state): App,
    Path((namespace, key)): Path<(String, String)>,
) -> Answer<Response> {
    let entry = state.store.lock().unwrap().get(&namespace, &key)?;
    let entry =
        entry.ok_or_else(|| Fail::new(code::NOT_FOUND, format!("No {key} in {namespace}")))?;
    Ok((etag(entry.revision), Json(kv_entry(entry))).into_response())
}

#[utoipa::path(put, path = "/v1/kv/{namespace}/{key}", summary = "Write a key, optionally only if nobody wrote in between.", tag = "kv",
    params(("namespace" = String, Path, description = "A namespace"), ("key" = String, Path, description = "A key"),
           ("If-Match" = Option<String>, Header, description = "Write only if the key is at this revision"),
           ("If-None-Match" = Option<String>, Header, description = "`*`: write only if the key is new")),
    request_body(content = Value, description = "The value: any JSON, at most 64 KB"),
    responses((status = 200, description = "Written", body = KvWritten),
              (status = 412, description = "Someone else wrote in between", body = ErrorBody),
              (status = 413, description = "The value is too large", body = ErrorBody)))]
async fn kv_put(
    State(state): App,
    Path((namespace, key)): Path<(String, String)>,
    headers: HeaderMap,
    Json(value): Json<Value>,
) -> Answer<Response> {
    let when = condition(&headers)?;
    let revision = state
        .store
        .lock()
        .unwrap()
        .put(&namespace, &key, &value, when)?;
    let _ = state.events.send(Event::Kv {
        namespace,
        key: key.clone(),
        value: Some(value),
        revision,
    });
    Ok((etag(revision), Json(KvWritten { key, revision })).into_response())
}

#[utoipa::path(delete, path = "/v1/kv/{namespace}/{key}", summary = "Remove a key.", tag = "kv",
    params(("namespace" = String, Path, description = "A namespace"), ("key" = String, Path, description = "A key"),
           ("If-Match" = Option<String>, Header, description = "Remove only if the key is at this revision")),
    responses((status = 200, description = "Removed", body = Done),
              (status = 404, description = "No such key", body = ErrorBody),
              (status = 412, description = "Someone else wrote in between", body = ErrorBody)))]
async fn kv_delete(
    State(state): App,
    Path((namespace, key)): Path<(String, String)>,
    headers: HeaderMap,
) -> Answer<Json<Done>> {
    let when = condition(&headers)?;
    if !state.store.lock().unwrap().remove(&namespace, &key, when)? {
        return Err(Fail::new(
            code::NOT_FOUND,
            format!("No {key} in {namespace}"),
        ));
    }
    let _ = state.events.send(Event::Kv {
        namespace,
        key,
        value: None,
        revision: 0,
    });
    Ok(Json(Done { ok: true }))
}

// ── Events ──────────────────────────────────────────────────────────────────

/// What a client of `/v1/events` asks to be told, in its first frame.
#[derive(Deserialize, Default)]
#[serde(default)]
struct Follow {
    /// Channel messages of these hosts only; of all when empty.
    hosts: Vec<String>,
    /// Channel messages whose topic starts with one of these. None without it.
    topics: Vec<String>,
    jobs: bool,
    /// Changes in these namespaces of the key-value store.
    kv: Vec<String>,
}

async fn events(State(state): App, ws: WebSocketUpgrade) -> Response {
    let events = state.events.subscribe();
    ws.on_upgrade(move |mut socket| async move {
        let follow: Follow = loop {
            match socket.recv().await {
                Some(Ok(WsMessage::Text(text))) => match serde_json::from_str(text.as_str()) {
                    Ok(follow) => break follow,
                    Err(error) => {
                        let complaint = json!({"error": {"code": code::BAD_REQUEST.1, "message": error.to_string()}});
                        let _ = socket.send(WsMessage::Text(complaint.to_string().into())).await;
                        return;
                    }
                },
                Some(Ok(WsMessage::Ping(_) | WsMessage::Pong(_))) => {}
                _ => return,
            }
        };
        forward(socket, events, move |event| match event {
            Event::Channel { host, message }
                if (follow.hosts.is_empty() || follow.hosts.contains(&host))
                    && follow.topics.iter().any(|prefix| message.topic.starts_with(prefix.as_str())) =>
            {
                Some(json!({"source": "channel", "host": host, "message": message}))
            }
            Event::Job(job) if follow.jobs => Some(json!({"source": "job", "job": job})),
            Event::Kv { namespace, key, value, revision } if follow.kv.contains(&namespace) => Some(match value {
                Some(value) => json!({"source": "kv", "namespace": namespace, "key": key, "value": value, "revision": revision}),
                None => json!({"source": "kv", "namespace": namespace, "key": key, "removed": true}),
            }),
            _ => None,
        })
        .await;
    })
}

// ── A host's own API, passed through ────────────────────────────────────────

/// Headers that mean something to the host, and those of its answer worth passing back.
const REQUEST_HEADERS: &[&str] = &[
    "content-type",
    "range",
    "if-match",
    "if-none-match",
    "accept",
];
const RESPONSE_HEADERS: &[&str] = &[
    "content-type",
    "content-range",
    "accept-ranges",
    "etag",
    "content-disposition",
    "last-modified",
];

async fn proxy(
    State(state): App,
    Path((name, rest)): Path<(String, String)>,
    RawQuery(query): RawQuery,
    method: Method,
    headers: HeaderMap,
    ws: Ws,
    body: Bytes,
) -> Answer<Response> {
    let remote = remote(&state, &name)?;
    // The secrets a host gets are the server's own: a caller cannot hand it others.
    if method == Method::POST && rest.trim_end_matches('/') == "tasks/secrets" {
        return Err(Fail::new(
            code::FORBIDDEN,
            "The secrets task takes the server's secrets: call hosts.secrets (POST /v1/hosts/NAME/secrets)",
        ));
    }
    let Some(link) = &remote.link else {
        let problem = remote.problem.clone().unwrap_or_else(|| "no link".into());
        return Err(Fail::new(code::UNREACHABLE, problem));
    };
    // The path as it came, so what was encoded stays encoded.
    let target = match query {
        Some(query) => format!("/v1/{}?{query}", client::encode_path(&rest)),
        None => format!("/v1/{}", client::encode_path(&rest)),
    };
    if let Ok(ws) = ws {
        let upstream = link.stream(&target).await?;
        return Ok(ws.on_upgrade(move |socket| pump(socket, upstream)));
    }
    let passed: Vec<(&str, &str)> = REQUEST_HEADERS
        .iter()
        .filter_map(|name| Some((*name, headers.get(*name)?.to_str().ok()?)))
        .collect();
    let answer = link.send(method.as_str(), &target, &passed, &body).await?;
    let mut response = Response::new(axum::body::Body::from(answer.body));
    *response.status_mut() = StatusCode::from_u16(answer.status).unwrap_or(StatusCode::BAD_GATEWAY);
    for name in RESPONSE_HEADERS {
        if let Some(value) = answer.headers.get(*name) {
            response.headers_mut().insert(*name, value.clone());
        }
    }
    Ok(response)
}

/// Frames both ways until either side closes.
async fn pump(socket: WebSocket, upstream: berth_core::link::LinkStream) {
    use client::Frame;
    let (mut to_client, mut from_client) = socket.split();
    let (mut to_host, mut from_host) = upstream.split();
    loop {
        tokio::select! {
            frame = from_client.next() => {
                let sent = match frame {
                    Some(Ok(WsMessage::Text(text))) => to_host.send(Frame::Text(text.as_str().into())).await,
                    Some(Ok(WsMessage::Binary(data))) => to_host.send(Frame::Binary(data)).await,
                    Some(Ok(WsMessage::Ping(_) | WsMessage::Pong(_))) => Ok(()),
                    _ => break,
                };
                if sent.is_err() { break }
            }
            frame = from_host.next() => {
                let sent = match frame {
                    Some(Ok(Frame::Text(text))) => to_client.send(WsMessage::Text(text.as_str().into())).await,
                    Some(Ok(Frame::Binary(data))) => to_client.send(WsMessage::Binary(data)).await,
                    Some(Ok(Frame::Ping(_) | Frame::Pong(_) | Frame::Frame(_))) => Ok(()),
                    _ => break,
                };
                if sent.is_err() { break }
            }
        }
    }
    let _ = to_host.send(Frame::Close(None)).await;
    let _ = to_client.send(WsMessage::Close(None)).await;
}
