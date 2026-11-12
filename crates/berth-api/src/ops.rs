//! The operations of the API by name. An operation is one thing a caller can
//! do: `sessions.start`, `kv.set`. Most can be called two ways, as an HTTP
//! request or as a frame on the portal, the one WebSocket at `/v1/portal`;
//! this table is what ties the name to both. Operations that stream are
//! subscriptions on the portal (and still a WebSocket of their own at their
//! path). A few carry bytes that do not belong in JSON frames and say so.
//!
//! ## The portal's frames
//!
//! The caller numbers its requests with `id`, an integer below 2^32 that it
//! does not reuse while the request or its stream is open.
//!
//! ```text
//! → {"id": 7, "op": "sessions.start", "args": {"wait": "30s"}, "data": {…}}
//! ← {"id": 7, "result": {…}}            or   ← {"id": 7, "error": {"code": "…", "message": "…", "status": 404}}
//! ```
//!
//! `args` are the operation's parameters (what HTTP has in the path and the
//! query), `data` is its input (what HTTP has as the body). On `berth serve`
//! a frame with `"host": "NAME"` is an operation of that host.
//!
//! A streaming operation answers `{"id": 7, "result": {"open": true}}` and
//! then sends its frames under the same number, until either side ends it:
//!
//! ```text
//! ← {"sub": 7, "data": {…}}             a text frame of the stream
//! ← binary: 4 bytes (7, big-endian), then the stream's bytes
//! → {"sub": 7, "data": {…}}             and the same two forms towards the stream
//! ↔ {"sub": 7, "end": true}             the last frame; from the server it may carry "error"
//! ```
//!
//! Bytes in a result or as input are `{"base64": "…"}` (a result adds
//! `content_type`), up to [`PORTAL_BYTES`]; larger transfers are HTTP's.

/// The portal's path.
pub const PORTAL: &str = "/v1/portal";

/// The most bytes one operation carries as base64 over the portal.
pub const PORTAL_BYTES: usize = 8 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A request and its answer.
    Call,
    /// Frames from the server until it ends; some take frames back.
    Stream,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Body {
    /// No input, or JSON.
    Json,
    /// Bytes: `{"base64": …}` on the portal.
    Bytes,
}

/// What a caller has to be allowed for an operation. Each role includes the
/// ones before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    /// Look: hosts, sessions and their output, logs, the inbox, the store.
    View,
    /// Use the hosts: run tasks and commands, write files, control units.
    Operate,
    /// Set up: profiles, secrets, registries, and applying them to hosts.
    Admin,
}

impl Role {
    pub const ALL: [Role; 3] = [Role::View, Role::Operate, Role::Admin];

    pub fn name(self) -> &'static str {
        match self {
            Role::View => "view",
            Role::Operate => "operate",
            Role::Admin => "admin",
        }
    }

    pub fn named(name: &str) -> Option<Role> {
        Role::ALL.into_iter().find(|role| role.name() == name)
    }
}

#[derive(Debug)]
pub struct Op {
    /// `group.verb`; the part before the dot groups the reference.
    pub name: &'static str,
    pub kind: Kind,
    pub method: &'static str,
    pub path: &'static str,
    pub body: Body,
    /// Parameters that are headers over HTTP: (argument, header).
    pub headers: &'static [(&'static str, &'static str)],
    /// Why the portal does not carry it, when it does not.
    pub http_only: &'static str,
    /// The least role that may call it on `berth serve`.
    pub role: Role,
}

const fn call(name: &'static str, method: &'static str, path: &'static str) -> Op {
    Op {
        name,
        kind: Kind::Call,
        method,
        path,
        body: Body::Json,
        headers: &[],
        http_only: "",
        // Reading is looking, anything else is using.
        role: match method.as_bytes() {
            b"GET" => Role::View,
            _ => Role::Operate,
        },
    }
}

const fn stream(name: &'static str, path: &'static str) -> Op {
    Op {
        name,
        kind: Kind::Stream,
        method: "GET",
        path,
        body: Body::Json,
        headers: &[],
        http_only: "",
        role: Role::Operate,
    }
}

const fn role(mut op: Op, role: Role) -> Op {
    op.role = role;
    op
}

const fn admin(op: Op) -> Op {
    role(op, Role::Admin)
}

const fn bytes(mut op: Op) -> Op {
    op.body = Body::Bytes;
    op
}

const fn headers(mut op: Op, headers: &'static [(&'static str, &'static str)]) -> Op {
    op.headers = headers;
    op
}

/// The operations of berthd.
pub const HOST: &[Op] = &[
    call("version", "GET", "/v1/version"),
    call("reload", "POST", "/v1/reload"),
    call("sessions.start", "POST", "/v1/sessions"),
    call("sessions.list", "GET", "/v1/sessions"),
    call("sessions.get", "GET", "/v1/sessions/{id}"),
    stream("sessions.attach", "/v1/sessions/{id}"),
    call("sessions.delete", "DELETE", "/v1/sessions/{id}"),
    call("sessions.output", "GET", "/v1/sessions/{id}/output"),
    call("sessions.screen", "GET", "/v1/sessions/{id}/screen"),
    call("sessions.input", "POST", "/v1/sessions/{id}/input"),
    call("sessions.resize", "POST", "/v1/sessions/{id}/resize"),
    call("sessions.signal", "POST", "/v1/sessions/{id}/signal"),
    call("tasks.list", "GET", "/v1/tasks"),
    call("tasks.run", "POST", "/v1/tasks/{name}"),
    call(
        "tasks.choices",
        "GET",
        "/v1/tasks/{name}/choices/{argument}",
    ),
    call("system.stats", "GET", "/v1/stats"),
    call("system.power", "POST", "/v1/power/{verb}"),
    call("desktop.screenshot", "GET", "/v1/screenshot"),
    role(call("files.read", "GET", "/v1/files"), Role::Operate),
    bytes(call("files.write", "PUT", "/v1/files")),
    call("files.delete", "DELETE", "/v1/files"),
    call("units.list", "GET", "/v1/units"),
    call("units.get", "GET", "/v1/units/{unit}"),
    call("units.act", "POST", "/v1/units/{unit}/{verb}"),
    call("logs.read", "GET", "/v1/logs"),
    role(stream("logs.follow", "/v1/logs"), Role::View),
    call("processes.list", "GET", "/v1/processes"),
    call("processes.signal", "POST", "/v1/processes/{pid}/signal"),
    call("channel.publish", "POST", "/v1/channel/{*topic}"),
    call("channel.read", "GET", "/v1/channel"),
    role(stream("channel.listen", "/v1/channel"), Role::View),
    call("services.list", "GET", "/v1/services"),
    call(
        "services.call",
        "POST",
        "/v1/services/{plugin}/{name}/{method}",
    ),
    stream("services.serve", "/v1/services/{plugin}/{name}/serve"),
];

const IF_MATCH: (&str, &str) = ("if_match", "if-match");

/// The operations of `berth serve` itself. A host's are reached with `host`.
pub const SERVE: &[Op] = &[
    call("version", "GET", "/v1/version"),
    call("access", "GET", "/v1/access"),
    call("hosts.list", "GET", "/v1/hosts"),
    call("hosts.get", "GET", "/v1/hosts/{name}"),
    admin(call("hosts.remove", "DELETE", "/v1/hosts/{name}")),
    admin(call("hosts.profile", "GET", "/v1/hosts/{name}/profile")),
    admin(call("hosts.save", "PUT", "/v1/hosts/{name}/profile")),
    admin(call("hosts.apply", "POST", "/v1/hosts/{name}/apply")),
    admin(call("hosts.seed", "POST", "/v1/hosts/{name}/seed")),
    admin(call("hosts.secrets", "POST", "/v1/hosts/{name}/secrets")),
    admin(call("secrets.list", "GET", "/v1/secrets")),
    admin(call("secrets.set", "PUT", "/v1/secrets/{name}")),
    admin(call("secrets.remove", "DELETE", "/v1/secrets/{name}")),
    admin(call("registries.list", "GET", "/v1/registries")),
    admin(call("registries.save", "PUT", "/v1/registries/{name}")),
    admin(call("registries.remove", "DELETE", "/v1/registries/{name}")),
    admin(call(
        "registries.check",
        "POST",
        "/v1/registries/{name}/check",
    )),
    admin(call("settings.list", "GET", "/v1/settings")),
    admin(call("settings.set", "PUT", "/v1/settings/{key}")),
    call("jobs.list", "GET", "/v1/jobs"),
    call("jobs.get", "GET", "/v1/jobs/{id}"),
    role(stream("jobs.follow", "/v1/jobs/{id}"), Role::View),
    call("jobs.output", "GET", "/v1/jobs/{id}/output"),
    call("inbox.list", "GET", "/v1/inbox"),
    role(stream("inbox.follow", "/v1/inbox"), Role::View),
    call("inbox.done", "POST", "/v1/inbox/{host}/{id}/done"),
    call("inbox.reopen", "DELETE", "/v1/inbox/{host}/{id}/done"),
    call("kv.list", "GET", "/v1/kv/{namespace}"),
    role(stream("kv.follow", "/v1/kv/{namespace}"), Role::View),
    call("kv.get", "GET", "/v1/kv/{namespace}/{key}"),
    headers(
        call("kv.set", "PUT", "/v1/kv/{namespace}/{key}"),
        &[IF_MATCH, ("if_none_match", "if-none-match")],
    ),
    headers(
        call("kv.delete", "DELETE", "/v1/kv/{namespace}/{key}"),
        &[IF_MATCH],
    ),
    role(stream("events.follow", "/v1/events"), Role::View),
];

pub fn find<'a>(table: &'a [Op], name: &str) -> Option<&'a Op> {
    table.iter().find(|op| op.name == name)
}

/// The names in a path: `{id}` and `{*topic}`, the latter taking slashes.
pub fn names(path: &str) -> impl Iterator<Item = (&str, bool)> {
    path.split('/').filter_map(|part| {
        let name = part.strip_prefix('{')?.strip_suffix('}')?;
        Some(match name.strip_prefix('*') {
            Some(rest) => (rest, true),
            None => (name, false),
        })
    })
}

/// The operation a request is, and its path parameters: the way back from
/// `GET /v1/sessions/abc` to `sessions.get` with `id`.
pub fn recognise<'a>(
    table: &'a [Op],
    kind: Kind,
    method: &str,
    path: &str,
) -> Option<(&'a Op, Vec<(&'a str, String)>)> {
    let given: Vec<&str> = path.split('/').collect();
    'ops: for op in table.iter().filter(|op| op.kind == kind) {
        if !op.method.eq_ignore_ascii_case(method) {
            continue;
        }
        let pattern: Vec<&str> = op.path.split('/').collect();
        let mut found = vec![];
        for (index, part) in pattern.iter().enumerate() {
            match part.strip_prefix('{').and_then(|p| p.strip_suffix('}')) {
                Some(name) => match name.strip_prefix('*') {
                    Some(name) if index < given.len() => {
                        found.push((name, crate::escape::decode(&given[index..].join("/"))));
                        return Some((op, found));
                    }
                    Some(_) => continue 'ops,
                    None => match given.get(index) {
                        Some(value) if !value.is_empty() => {
                            found.push((name, crate::escape::decode(value)))
                        }
                        _ => continue 'ops,
                    },
                },
                None if given.get(index) == Some(part) => {}
                None => continue 'ops,
            }
        }
        if pattern.len() == given.len() {
            return Some((op, found));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_and_requests_are_recognised() {
        for table in [HOST, SERVE] {
            for (index, op) in table.iter().enumerate() {
                assert!(
                    !table[..index].iter().any(|other| other.name == op.name),
                    "{} twice",
                    op.name
                );
            }
        }
        let (op, args) = recognise(HOST, Kind::Call, "GET", "/v1/sessions/a%20b/output").unwrap();
        assert_eq!(
            (op.name, args),
            ("sessions.output", vec![("id", "a b".to_owned())])
        );
        let (op, args) = recognise(HOST, Kind::Call, "post", "/v1/channel/ask/desk").unwrap();
        assert_eq!(
            (op.name, args),
            ("channel.publish", vec![("topic", "ask/desk".to_owned())])
        );
        assert_eq!(
            recognise(HOST, Kind::Stream, "GET", "/v1/sessions/x")
                .unwrap()
                .0
                .name,
            "sessions.attach"
        );
        assert_eq!(
            recognise(HOST, Kind::Call, "GET", "/v1/sessions")
                .unwrap()
                .0
                .name,
            "sessions.list"
        );
        assert!(recognise(HOST, Kind::Call, "GET", "/v1/nothing").is_none());
        // Looking is for everyone who is let in, set-up only for an admin.
        let role = |table, name| find(table, name).unwrap().role;
        assert_eq!(role(HOST, "sessions.list"), Role::View);
        assert_eq!(role(HOST, "files.read"), Role::Operate);
        assert_eq!(role(HOST, "sessions.attach"), Role::Operate);
        assert_eq!(role(SERVE, "hosts.apply"), Role::Admin);
        assert_eq!(role(SERVE, "kv.set"), Role::Operate);
        assert!(Role::View < Role::Operate && Role::Operate < Role::Admin);
        assert_eq!(Role::named("operate"), Some(Role::Operate));
        assert_eq!(
            names("/v1/channel/{*topic}").collect::<Vec<_>>(),
            vec![("topic", true)]
        );
    }
}
