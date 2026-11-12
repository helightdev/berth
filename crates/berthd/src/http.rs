//! The routes, in one table that both registers them and describes them, so
//! a call cannot exist undocumented.

use crate::error::E;
use crate::state::{Caller, Mode, State};
use crate::{channel, files, services, sessions, system, tasks};
use axum::{
    Json, Router,
    extract::{Extension, Request, State as AppState},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{MethodRouter, delete, get, post, put},
};
use berth_api::{self as api, ops, streams};
use serde_json::{Value, json};
use std::sync::Arc;

type Handler = fn() -> MethodRouter<Arc<State>>;

pub struct Route {
    pub method: &'static str,
    pub path: &'static str,
    pub summary: &'static str,
    /// `host`, or `both` when the agent may call it too (the handler then decides what it sees).
    pub level: &'static str,
    pub query: &'static [(&'static str, &'static str)],
    /// The request body's type, `bytes`, or nothing.
    pub body: &'static str,
    /// The answer's type, `Type[]`, `map`, `png` or `bytes`.
    pub reply: &'static str,
    /// The stream of `berth_api::streams` this path also is, with a WebSocket handshake.
    pub stream: &'static str,
    handler: Handler,
}

const WAIT: (&str, &str) = (
    "wait",
    "A duration such as 30s, at most 10m: answer when the command has ended, with what it wrote.",
);

#[rustfmt::skip]
pub const ROUTES: &[Route] = &[
    Route { method: "GET", path: "/v1/version", summary: "The version of berthd and of its API.", level: "both", query: &[], body: "", reply: "Version", stream: "", handler: || get(version) },
    Route { method: "POST", path: "/v1/reload", summary: "Read the configuration files again. Sessions go on.", level: "host", query: &[], body: "", reply: "Done", stream: "", handler: || post(reload) },

    Route { method: "POST", path: "/v1/sessions", summary: "Start a session.", level: "host", query: &[WAIT], body: "StartSession", reply: "Started", stream: "", handler: || post(sessions::start) },
    Route { method: "GET", path: "/v1/sessions", summary: "All sessions, running and ended.", level: "host", query: &[], body: "", reply: "Session[]", stream: "", handler: || get(sessions::list) },
    Route { method: "GET", path: "/v1/sessions/{id}", summary: "One session's state. The agent sees those it started through a task.", level: "both", query: &[], body: "", reply: "Session", stream: "session", handler: || get(sessions::one) },
    Route { method: "DELETE", path: "/v1/sessions/{id}", summary: "End it if it runs (HUP, after five seconds KILL) and forget it with its transcript.", level: "host", query: &[], body: "", reply: "Done", stream: "", handler: || delete(sessions::delete) },
    Route { method: "GET", path: "/v1/sessions/{id}/output", summary: "The transcript, by bytes or by lines.", level: "both", query: &[("stream", "stdout, stderr or merged (default), for a process."), ("since", "Byte offset to read from."), ("limit", "Most bytes to return; default 1 MB."), ("line", "Read lines from this line number (a process only)."), ("count", "How many lines; default 100."), ("tail", "The last so many lines.")], body: "", reply: "Output", stream: "", handler: || get(sessions::output) },
    Route { method: "GET", path: "/v1/sessions/{id}/screen", summary: "A terminal as it looks now, as text.", level: "both", query: &[("format", "text (default) or ansi, which keeps colours."), ("tail", "That many lines of screen and scrollback instead.")], body: "", reply: "Screen", stream: "", handler: || get(sessions::screen) },
    Route { method: "POST", path: "/v1/sessions/{id}/input", summary: "Write to standard input or type into the terminal. Never recorded.", level: "host", query: &[], body: "Input", reply: "Done", stream: "", handler: || post(sessions::input) },
    Route { method: "POST", path: "/v1/sessions/{id}/resize", summary: "Give a terminal a new size.", level: "host", query: &[], body: "Resize", reply: "Done", stream: "", handler: || post(sessions::resize) },
    Route { method: "POST", path: "/v1/sessions/{id}/signal", summary: "Send INT, TERM, HUP or KILL to the session's process group.", level: "host", query: &[], body: "Signal", reply: "Done", stream: "", handler: || post(sessions::signal) },

    Route { method: "GET", path: "/v1/tasks", summary: "The tasks the caller may run, in the order of the file.", level: "both", query: &[], body: "", reply: "Task[]", stream: "", handler: || get(tasks::list) },
    Route { method: "POST", path: "/v1/tasks/{name}", summary: "Run a task. It and what extends it are the steps of one session.", level: "both", query: &[WAIT], body: "RunTask", reply: "Started", stream: "", handler: || post(tasks::run) },
    Route { method: "GET", path: "/v1/tasks/{name}/choices/{argument}", summary: "The values to offer for an argument.", level: "both", query: &[], body: "", reply: "Choices", stream: "", handler: || get(tasks::choices) },

    Route { method: "GET", path: "/v1/stats", summary: "Load, CPU, memory, filesystems, network, GPU and unit states, read now.", level: "host", query: &[], body: "", reply: "Stats", stream: "", handler: || get(system::stats) },
    Route { method: "GET", path: "/v1/screenshot", summary: "The shared desktop as PNG.", level: "host", query: &[], body: "", reply: "png", stream: "", handler: || get(system::screenshot) },
    Route { method: "GET", path: "/v1/files", summary: "A folder's entries (a Listing), or a file's content; Range is honoured. As the agent, under the configured roots.", level: "host", query: &[("path", "An absolute path under a root.")], body: "", reply: "bytes", stream: "", handler: || get(files::get) },
    Route { method: "PUT", path: "/v1/files", summary: "Write one file, creating its folders.", level: "host", query: &[("path", "An absolute path under a root."), ("overwrite", "true to replace an existing file.")], body: "bytes", reply: "Done", stream: "", handler: || put(files::put) },
    Route { method: "DELETE", path: "/v1/files", summary: "Remove a file or an empty folder; never a root itself.", level: "host", query: &[("path", "An absolute path under a root."), ("recursive", "true for a folder with its contents.")], body: "", reply: "Done", stream: "", handler: || delete(files::delete) },
    Route { method: "GET", path: "/v1/units", summary: "The units and their state.", level: "host", query: &[], body: "", reply: "Unit[]", stream: "", handler: || get(system::units) },
    Route { method: "GET", path: "/v1/units/{unit}", summary: "One unit's properties, as systemctl show gives them.", level: "host", query: &[], body: "", reply: "map", stream: "", handler: || get(system::unit) },
    Route { method: "POST", path: "/v1/units/{unit}/{verb}", summary: "Start, stop, restart, reload, enable or disable a unit.", level: "host", query: &[], body: "", reply: "Done", stream: "", handler: || post(system::unit_verb) },
    Route { method: "GET", path: "/v1/logs", summary: "Journal entries.", level: "host", query: &[("unit", "Only this unit."), ("since", "A cursor from an earlier entry, or a time."), ("limit", "Most entries; default 100.")], body: "", reply: "LogEntry[]", stream: "logs", handler: || get(system::logs) },
    Route { method: "GET", path: "/v1/processes", summary: "The processes, from /proc.", level: "host", query: &[], body: "", reply: "Process[]", stream: "", handler: || get(system::processes) },
    Route { method: "POST", path: "/v1/processes/{pid}/signal", summary: "Send a signal to a process.", level: "host", query: &[], body: "Signal", reply: "Done", stream: "", handler: || post(system::process_signal) },
    Route { method: "POST", path: "/v1/power/{verb}", summary: "Reboot the host or power it off. Refused while sessions run.", level: "host", query: &[("force", "true to do it although sessions run.")], body: "", reply: "Done", stream: "", handler: || post(system::power) },

    Route { method: "POST", path: "/v1/channel/{*topic}", summary: "Publish a message on a topic.", level: "both", query: &[], body: "Publish", reply: "Message", stream: "", handler: || post(channel::publish) },
    Route { method: "GET", path: "/v1/channel", summary: "Kept messages after `since`.", level: "both", query: &[("topic", "Only topics starting with this."), ("since", "Messages after this id; default 0."), ("limit", "Most messages; default 100, at most 1000.")], body: "", reply: "Messages", stream: "channel", handler: || get(channel::read) },

    Route { method: "GET", path: "/v1/services", summary: "The services that can be called, and whether each is up.", level: "both", query: &[], body: "", reply: "Service[]", stream: "", handler: || get(services::list) },
    Route { method: "POST", path: "/v1/services/{plugin}/{name}/{method}", summary: "Call a method of the service PLUGIN/NAME: JSON in, JSON out.", level: "both", query: &[], body: "json", reply: "json", stream: "", handler: || post(services::call) },
    Route { method: "GET", path: "/v1/services/{plugin}/{name}/serve", summary: "Only as a WebSocket: for the program that answers the service's calls.", level: "both", query: &[], body: "", reply: "", stream: "serve", handler: || get(services::serve) },
];

async fn version(AppState(state): AppState<Arc<State>>) -> Json<api::Version> {
    let mode = if state.mode == Mode::System {
        "system"
    } else {
        "single"
    };
    // Written by the role at the end of an apply; read each time, as it
    // changes without a reload.
    let applied = std::fs::read(state.config_dir.join("applied.json"))
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok());
    Json(api::Version {
        version: api::VERSION.into(),
        api: api::API,
        mode: mode.into(),
        applied,
    })
}

async fn reload(AppState(state): AppState<Arc<State>>) -> Result<Json<api::Done>, E> {
    match state.reload().as_slice() {
        [] => Ok(Json(api::Done { ok: true })),
        problems => Err(E::bad(problems.join("; "))),
    }
}

/// The operation a route is when called as a request.
pub fn operation(route: &Route) -> Option<&'static ops::Op> {
    ops::HOST
        .iter()
        .find(|op| op.kind == ops::Kind::Call && op.method == route.method && op.path == route.path)
}

/// The portal: every operation over one WebSocket, as the caller that opened it.
async fn portal(
    AppState(state): AppState<Arc<State>>,
    Extension(caller): Extension<Caller>,
    upgrade: axum::extract::ws::WebSocketUpgrade,
) -> Response {
    let inner = router(state).layer(Extension(caller));
    upgrade.on_upgrade(move |socket| {
        crate::portal::run(socket, inner, crate::portal::host_operations())
    })
}

async fn host_only(Extension(caller): Extension<Caller>, request: Request, next: Next) -> Response {
    if !caller.host() {
        return E::forbidden("This call is for the host level").into_response();
    }
    next.run(request).await
}

pub fn router(state: Arc<State>) -> Router {
    let mut router = Router::new();
    for route in ROUTES {
        let mut handler = (route.handler)();
        if route.level == "host" {
            handler = handler.route_layer(axum::middleware::from_fn(host_only));
        }
        router = router.route(route.path, handler);
    }
    // Every stream of the table has its route, and no route claims another.
    for stream in streams::HOST {
        assert!(
            ROUTES.iter().any(|route| route.stream == stream.id),
            "stream {} has no route",
            stream.id
        );
    }
    for route in ROUTES.iter().filter(|route| !route.stream.is_empty()) {
        assert!(
            streams::HOST.iter().any(|stream| stream.id == route.stream),
            "route {} names no stream",
            route.path
        );
    }
    // Every route is an operation with a name, and every operation has its route.
    for route in ROUTES {
        assert!(
            operation(route).is_some() || !route.stream.is_empty(),
            "{} {} is no operation",
            route.method,
            route.path
        );
    }
    for op in ops::HOST {
        assert!(
            ROUTES.iter().any(|route| route.path == op.path
                && route.method == op.method
                && (op.kind == ops::Kind::Call || !route.stream.is_empty())),
            "the operation {} has no route",
            op.name
        );
    }
    router
        .route(ops::PORTAL, get(portal))
        .fallback(|| async { E::missing("such call") })
        .layer(axum::extract::DefaultBodyLimit::disable())
        .with_state(state)
}

/// The path as OpenAPI writes it, and the names in it.
fn documented(path: &str) -> (String, Vec<String>) {
    let path = path.replace("{*", "{");
    let names = path
        .split('/')
        .filter_map(|part| Some(part.strip_prefix('{')?.strip_suffix('}')?.to_owned()))
        .collect();
    (path, names)
}

fn schema(name: &str) -> Value {
    match name.strip_suffix("[]") {
        Some(item) => {
            json!({"type": "array", "items": {"$ref": format!("#/components/schemas/{item}")}})
        }
        None if name == "map" => {
            json!({"type": "object", "additionalProperties": {"type": "string"}})
        }
        None if name == "json" => json!({}),
        None => json!({"$ref": format!("#/components/schemas/{name}")}),
    }
}

fn content(name: &str) -> Value {
    match name {
        "png" => json!({"image/png": {"schema": {"type": "string", "format": "binary"}}}),
        "bytes" => {
            json!({"application/octet-stream": {"schema": {"type": "string", "format": "binary"}}})
        }
        name => json!({"application/json": {"schema": schema(name)}}),
    }
}

/// OpenAPI paths for a table of routes, with `prefix` put after `/v1` (for
/// `berth serve`, which has each host's API under `/hosts/{name}`).
pub fn paths(prefix: &str) -> serde_json::Map<String, Value> {
    let mut paths = serde_json::Map::new();
    for route in ROUTES {
        let (path, names) = documented(route.path);
        let (path, prefixed) = documented(&path.replacen("/v1", &format!("/v1{prefix}"), 1));
        let mut parameters: Vec<Value> = prefixed
            .iter()
            .map(|name| json!({"name": name, "in": "path", "required": true, "schema": {"type": "string"}}))
            .collect();
        let _ = names;
        for (name, meaning) in route.query {
            parameters.push(json!({"name": name, "in": "query", "description": meaning, "schema": {"type": "string"}}));
        }
        let mut responses = serde_json::Map::new();
        if !route.reply.is_empty() {
            responses.insert(
                "200".into(),
                json!({"description": "Done.", "content": content(route.reply)}),
            );
        }
        if !route.stream.is_empty() {
            responses.insert("101".into(), json!({"description": format!("With a WebSocket handshake: the stream `{}` of the AsyncAPI document.", route.stream)}));
        }
        responses.insert(
            "default".into(),
            json!({"description": "An error.", "content": content("ErrorBody")}),
        );
        let tag = path
            .trim_start_matches(&format!("/v1{prefix}/"))
            .split('/')
            .next()
            .unwrap_or("")
            .to_owned();
        let level = if route.level == "host" {
            "Host level only."
        } else {
            "Host and agent level."
        };
        let mut operation = json!({
            "summary": route.summary,
            "description": level,
            "operationId": format!("{}{}", route.method.to_lowercase(), route.path.replace(['/', '{', '}', '*'], "_")),
            "tags": [tag],
            "parameters": parameters,
            "responses": responses,
        });
        if !route.body.is_empty() {
            operation["requestBody"] = json!({"content": content(route.body)});
        }
        let item = paths.entry(path).or_insert_with(|| json!({}));
        item[route.method.to_lowercase()] = operation;
    }
    paths
}

#[derive(utoipa::OpenApi)]
#[openapi(components(schemas(
    api::ErrorBody,
    api::ErrorDetail,
    api::Version,
    api::Level,
    api::From,
    api::SessionKind,
    api::CommandLine,
    api::StartSession,
    api::Step,
    api::Session,
    api::Started,
    api::Output,
    api::Screen,
    api::Input,
    api::Resize,
    api::Signal,
    api::Argument,
    api::Task,
    api::RunTask,
    api::Choices,
    api::Memory,
    api::Filesystem,
    api::Interface,
    api::Gpu,
    api::UnitState,
    api::Stats,
    api::FileEntry,
    api::Listing,
    api::Unit,
    api::LogEntry,
    api::Process,
    api::Done,
    api::Keep,
    api::Message,
    api::Publish,
    api::PublishFrame,
    api::Messages,
    api::Topic,
    api::Method,
    api::ServiceSpec,
    api::Service,
    api::CallFrame,
    api::ReplyFrame,
    api::Config
)))]
struct Schemas;

/// The components every document of ours shares.
pub fn components() -> Value {
    use utoipa::OpenApi;
    serde_json::to_value(Schemas::openapi().components).unwrap_or_default()
}

pub fn openapi() -> Value {
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "berthd",
            "version": api::VERSION,
            "description": "The API of a host's berthd, over its unix sockets. Streams are in the AsyncAPI document.",
            "license": {"name": "MIT"},
        },
        "paths": paths(""),
        "components": components(),
    })
}

/// What the names in the paths mean: (start of the operation's name, argument, meaning).
const PATH_ARGUMENTS: &[(&str, &str, &str)] = &[
    (
        "sessions.",
        "id",
        "A session's id, as starting it or the list gives it.",
    ),
    ("tasks.", "name", "A task's name, as the list gives it."),
    (
        "tasks.",
        "argument",
        "The name of one of the task's arguments.",
    ),
    ("system.power", "verb", "`reboot` or `poweroff`."),
    (
        "units.",
        "unit",
        "A unit's name, such as `ssh.service`. Only the configured units.",
    ),
    (
        "units.act",
        "verb",
        "`start`, `stop`, `restart`, `reload`, `enable` or `disable`.",
    ),
    ("processes.", "pid", "A process id."),
    (
        "channel.publish",
        "topic",
        "The topic, with slashes: `plugin/what`.",
    ),
    ("services.", "plugin", "The plugin the service belongs to."),
    ("services.", "name", "The service's name within its plugin."),
    ("services.call", "method", "One of the service's methods."),
    ("jobs.", "id", "A job's id."),
    ("kv.", "namespace", "A namespace."),
    ("kv.", "key", "A key."),
    ("hosts.", "name", "A saved host."),
    ("inbox.", "host", "The host the message came from."),
    ("inbox.", "id", "The message's id on that host."),
];

/// The API by operation: for each, what it takes and gives, the request it is
/// over HTTP and how it travels on the portal. `openapi` describes the calls
/// and `stream_of` finds the stream an operation is.
pub fn operations_of(
    title: &str,
    description: &str,
    table: &[ops::Op],
    openapi: &Value,
    stream_of: &dyn Fn(&ops::Op) -> Option<&'static streams::Stream>,
) -> Value {
    let frames = |frames: &[streams::Frame]| -> Vec<Value> {
        frames
            .iter()
            .map(|frame| {
                let example = serde_json::from_str::<Value>(frame.example).ok();
                json!({
                    "name": frame.name,
                    "description": frame.description,
                    "binary": frame.example.is_empty(),
                    "example": example,
                })
            })
            .collect()
    };
    let mut operations = vec![];
    for op in table {
        let (path, _) = documented(op.path);
        let group = op
            .name
            .split_once('.')
            .map_or("general", |(group, _)| group);
        let mut entry = json!({
            "name": op.name,
            "group": group,
            "kind": if op.kind == ops::Kind::Stream { "stream" } else { "call" },
            "http": {"method": op.method, "path": path},
            "portal": op.http_only.is_empty(),
            "role": op.role.name(),
        });
        if !op.http_only.is_empty() {
            entry["http_only"] = json!(op.http_only);
        }
        let mut args: Vec<Value> = ops::names(op.path)
            .map(|(name, _)| {
                let meaning = PATH_ARGUMENTS
                    .iter()
                    .find(|(group, argument, _)| *argument == name && op.name.starts_with(group))
                    .map(|known| known.2);
                json!({"name": name, "in": "path", "required": true, "description": meaning})
            })
            .collect();
        match op.kind {
            ops::Kind::Call => {
                let item = &openapi["paths"][&path][op.method.to_lowercase()];
                let done = &item["responses"]["200"];
                entry["summary"] = match item["summary"].as_str() {
                    Some(summary) => json!(summary),
                    None => done["description"].clone(),
                };
                if let Some(level) = item["description"].as_str() {
                    entry["level"] = json!(level);
                }
                for parameter in item["parameters"].as_array().into_iter().flatten() {
                    let name = parameter["name"].as_str().unwrap_or("");
                    let place = parameter["in"].as_str().unwrap_or("");
                    // A header is an argument by another name.
                    let argument = op
                        .headers
                        .iter()
                        .find(|(_, header)| header.eq_ignore_ascii_case(name))
                        .map_or(name, |(argument, _)| argument);
                    match args.iter_mut().find(|arg| arg["name"] == argument) {
                        Some(arg) if parameter["description"].is_string() => {
                            arg["description"] = parameter["description"].clone()
                        }
                        Some(_) => {}
                        None => args.push(json!({
                            "name": argument,
                            "in": place,
                            "required": op.name.starts_with("files.") && argument == "path",
                            "description": parameter["description"],
                            "header": if place == "header" { json!(name) } else { Value::Null },
                        })),
                    }
                }
                let body = &item["requestBody"];
                if !body.is_null() {
                    entry["data"] = match op.body {
                        ops::Body::Bytes => json!({"bytes": true}),
                        ops::Body::Json => json!({
                            "schema": body["content"]["application/json"]["schema"],
                            "description": body["description"],
                        }),
                    };
                }
                let content = &done["content"];
                entry["result"] = match content.get("application/json") {
                    Some(json) => json!({"schema": json["schema"]}),
                    None if content.is_object() => json!({
                        "bytes": true,
                        "content_type": content.as_object().and_then(|kinds| kinds.keys().next()),
                    }),
                    None => Value::Null,
                };
                let errors: Vec<Value> = item["responses"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .filter(|(status, _)| status.starts_with(['4', '5']))
                    .map(|(status, answer)| json!({"status": status, "description": answer["description"]}))
                    .collect();
                if !errors.is_empty() {
                    entry["errors"] = json!(errors);
                }
            }
            ops::Kind::Stream => {
                let stream = stream_of(op).unwrap_or_else(|| panic!("{} is no stream", op.name));
                entry["summary"] = json!(stream.summary);
                // Levels are berthd's: the host's socket and the agent's.
                if std::ptr::eq(table, ops::HOST) {
                    entry["level"] = json!(if stream.level == "host" {
                        "Host level only."
                    } else {
                        "Host and agent level."
                    });
                }
                for (name, meaning) in stream.parameters {
                    args.push(json!({"name": name, "in": "query", "required": false, "description": meaning}));
                }
                entry["resumable"] = json!(stream.resumable);
                entry["receives"] = json!(frames(stream.server));
                entry["sends"] = json!(frames(stream.client));
            }
        }
        entry["args"] = json!(args);
        operations.push(entry);
    }
    json!({
        "title": title,
        "version": api::VERSION,
        "description": description,
        "portal": {
            "path": ops::PORTAL,
            "bytes": ops::PORTAL_BYTES,
        },
        "operations": operations,
        "components": openapi["components"],
    })
}

/// berthd's operations.
pub fn operations() -> Value {
    operations_of(
        "berthd",
        "The operations of a host's berthd, over its unix sockets.",
        ops::HOST,
        &openapi(),
        &|op| {
            let route = ROUTES
                .iter()
                .find(|route| route.path == op.path && !route.stream.is_empty())?;
            streams::HOST
                .iter()
                .find(|stream| stream.id == route.stream)
        },
    )
}

pub fn asyncapi() -> Value {
    let all: Vec<&streams::Stream> = streams::HOST.iter().collect();
    streams::asyncapi("berthd", "The WebSocket streams of a host's berthd.", &all)
}
