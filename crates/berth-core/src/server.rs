//! A `berth serve` somewhere else, as this berth's way to its hosts. With the
//! `server` setting (or `BERTH_SERVER`) berth keeps no hosts of its own: it
//! shows the server's and reaches them through the server's portal.
//!
//! - `http://HOST:PORT`, `https://HOST[:PORT]`: over TCP, with the token
//! - `unix:/path/to/serve.sock`: the socket of a `berth serve` on this machine

use crate::{Result, paths::Paths, store::kind};
use berth_api::{
    client::{self, Error},
    ops,
    portal::Portal,
};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::net::{TcpStream, UnixStream};

#[derive(Debug, Clone, PartialEq)]
pub struct Server {
    pub url: String,
    pub token: Option<String>,
}

fn transport(error: impl std::fmt::Display) -> Error {
    Error::Transport(error.to_string())
}

/// `host`, `port` and whether TLS, of an http(s) address.
fn parts(url: &str) -> client::Result<(String, u16, bool)> {
    let (secure, rest) = match (url.strip_prefix("https://"), url.strip_prefix("http://")) {
        (Some(rest), _) => (true, rest),
        (_, Some(rest)) => (false, rest),
        _ => {
            return Err(Error::Transport(format!(
                "{url} is no address of a berth serve: http://, https:// or unix:"
            )));
        }
    };
    let authority = rest.split('/').next().unwrap_or("");
    let default = if secure { 443 } else { 80 };
    let (host, port) = match authority.rsplit_once(':') {
        // Not the colons of an IPv6 address in brackets.
        Some((host, port)) if !port.contains(']') => (
            host,
            port.parse()
                .map_err(|_| Error::Transport(format!("{port} is no port")))?,
        ),
        _ => (authority, default),
    };
    if host.is_empty() {
        return Err(Error::Transport(format!("{url} names no host")));
    }
    Ok((host.to_owned(), port, secure))
}

/// TLS over a connection, for the name the certificate has to be for.
async fn secure_stream(
    name: &str,
    stream: TcpStream,
) -> client::Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(transport)?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let server = rustls::pki_types::ServerName::try_from(name.to_owned()).map_err(transport)?;
    tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(server, stream)
        .await
        .map_err(transport)
}

/// One request to an http(s) address: the status and the body of the answer.
pub async fn http(
    method: &str,
    url: &str,
    content_type: Option<&str>,
    body: Vec<u8>,
) -> client::Result<(u16, Vec<u8>)> {
    http_as(method, url, None, content_type, body).await
}

/// [`http`] with a bearer in the `Authorization` header.
pub async fn http_as(
    method: &str,
    url: &str,
    bearer: Option<&str>,
    content_type: Option<&str>,
    body: Vec<u8>,
) -> client::Result<(u16, Vec<u8>)> {
    let (host, port, secure) = parts(url)?;
    let name = host.trim_matches(['[', ']']).to_owned();
    let scheme = if secure { "https://" } else { "http://" };
    let path = match url[scheme.len()..].find('/') {
        Some(at) => &url[scheme.len() + at..],
        None => "/",
    };
    let stream = TcpStream::connect((name.as_str(), port))
        .await
        .map_err(|error| Error::Transport(format!("{url}: {error}")))?;
    let authority = format!("{host}:{port}");
    let mut headers = vec![("host", authority.as_str()), ("accept", "application/json")];
    if let Some(kind) = content_type {
        headers.push(("content-type", kind));
    }
    let authorization = bearer.map(|bearer| format!("Bearer {bearer}"));
    if let Some(value) = &authorization {
        headers.push(("authorization", value));
    }
    let answer = match secure {
        true => {
            let stream = secure_stream(&name, stream)
                .await
                .map_err(|error| Error::Transport(format!("{url}: {error}")))?;
            let mut client = client::Client::over(stream).await?;
            client.send(method, path, &headers, body.into()).await?
        }
        false => {
            let mut client = client::Client::over(stream).await?;
            client.send(method, path, &headers, body.into()).await?
        }
    };
    Ok((answer.status, answer.body.to_vec()))
}

/// What a GET of an http(s) address answers with, when it is a 200.
pub async fn fetch(url: &str) -> client::Result<Vec<u8>> {
    match http("GET", url, None, vec![]).await? {
        (200, body) => Ok(body),
        (status, _) => Err(Error::Transport(format!("{url}: HTTP {status}"))),
    }
}

impl Server {
    /// The server this berth is set to use, if any. The environment wins.
    pub fn configured(paths: &Paths) -> Option<Server> {
        // Local settings only, so that finding the server never opens the store.
        let settings = paths.local_settings().unwrap_or_default();
        let setting = |key: &str| settings.get(key).and_then(Value::as_str).map(str::to_owned);
        let variable = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
        let url = variable("BERTH_SERVER").or_else(|| setting("server"))?;
        Some(Server {
            url: url.trim_end_matches('/').to_owned(),
            token: variable("BERTH_TOKEN").or_else(|| setting("server_token")),
        })
    }

    /// Open the server's portal, with the configured token if there is one.
    pub async fn portal(&self) -> client::Result<Portal> {
        let token = self.token.as_deref().unwrap_or("");
        if let Some(path) = self.url.strip_prefix("unix:") {
            let stream = UnixStream::connect(path)
                .await
                .map_err(|error| Error::Transport(format!("{path}: {error}")))?;
            let socket = client::websocket(stream, ops::PORTAL, "").await?;
            return Ok(Portal::over(socket));
        }
        let (host, port, secure) = parts(&self.url)?;
        let name = host.trim_matches(['[', ']']).to_owned();
        let stream = TcpStream::connect((name.as_str(), port))
            .await
            .map_err(|error| Error::Transport(format!("{}: {error}", self.url)))?;
        let authority = format!("{host}:{port}");
        if !secure {
            let socket = client::websocket_at(stream, &authority, ops::PORTAL, token).await?;
            return Ok(Portal::over(socket));
        }
        let stream = secure_stream(&name, stream)
            .await
            .map_err(|error| Error::Transport(format!("{}: {error}", self.url)))?;
        let socket = client::websocket_at(stream, &authority, ops::PORTAL, token).await?;
        Ok(Portal::over(socket))
    }

    /// One operation of the server itself, waited for.
    pub fn call(&self, op: &str, args: Value, data: Option<Value>) -> Result<Value> {
        crate::link::runtime()?.block_on(async {
            let portal = self.portal().await?;
            self.ask(&portal, op, args, data).await
        })
    }

    /// One operation on an open portal. A server that lacks the operation is
    /// an older one, which is said in words, with its version when it tells.
    pub async fn ask(
        &self,
        portal: &Portal,
        op: &str,
        args: Value,
        data: Option<Value>,
    ) -> Result<Value> {
        match portal.call(op, None, args, data).await {
            Err(error) if missing_operation(&error).is_some() => {
                let version = portal
                    .call("version", None, Value::Null, None)
                    .await
                    .ok()
                    .and_then(|version| version["version"].as_str().map(str::to_owned));
                Err(unsupported(&self.url, version.as_deref(), &error).into())
            }
            other => Ok(other?),
        }
    }

    /// The server's hosts: name, address and whether each answers.
    pub async fn hosts(&self) -> client::Result<Vec<Value>> {
        let hosts = self
            .portal()
            .await?
            .call("hosts.list", None, Value::Null, None)
            .await?;
        Ok(hosts.as_array().cloned().unwrap_or_default())
    }

    /// The scope of the store that holds what is known of this server: its
    /// registries, the names of its secrets, and what the last mirror found.
    pub fn scope(&self) -> String {
        format!("served:{}", self.url)
    }

    /// Keep what the server has in the store, under the server's scope: a
    /// profile for each of its hosts, its registries and the names of its
    /// secrets. Whoever may set the
    /// server up gets the profiles as they are there, and saving one sends it
    /// back; anyone else gets profiles that only say where the hosts are.
    pub fn mirror(&self, paths: &Paths) -> Result<()> {
        let fetched = crate::link::runtime()?.block_on(self.fetch())?;
        self.keep(paths, fetched)
    }

    fn keep(&self, paths: &Paths, fetched: Fetched) -> Result<()> {
        let mut state = serde_json::Map::new();
        for (host, _) in &fetched.hosts {
            if let Some(name) = host["name"].as_str().filter(|name| safe(name)) {
                state.insert(
                    name.to_owned(),
                    json!({
                        "reachable": host["reachable"] == true,
                        "error": host["error"],
                        "version": host["version"],
                        "rotating": host["rotating"] == true,
                    }),
                );
            }
        }
        // One transaction, so a screen that reads meanwhile sees the old
        // mirror or the new one, not half of one.
        let mut items: Vec<(&str, String, Vec<u8>)> = vec![];
        // A whole profile, so every screen has what it expects; only where the
        // host is and that it is the server's are its own.
        let mut example: Option<Value> = None;
        for (host, profile) in fetched.hosts {
            let Some(name) = host["name"].as_str().filter(|name| safe(name)) else {
                continue;
            };
            let mut profile = match profile {
                Some(profile) if profile.is_object() => profile,
                _ => {
                    if example.is_none() {
                        let file = paths.source()?.join(crate::paths::layout::EXAMPLE);
                        example = Some(serde_json::from_slice(&std::fs::read(file)?)?);
                    }
                    let mut profile = example.clone().unwrap_or_default();
                    profile["access"] = json!({"address": host["address"], "sudo": "none"});
                    profile["builder"]["served_only"] = json!(true);
                    profile
                }
            };
            profile["builder"]["served"] = json!(self.url);
            items.push((
                kind::PROFILE,
                name.to_owned(),
                profile.to_string().into_bytes(),
            ));
        }
        for (name, registry) in fetched.registries.as_object().into_iter().flatten() {
            items.push((
                kind::REGISTRY,
                name.clone(),
                registry.to_string().into_bytes(),
            ));
        }
        for name in fetched.secrets {
            items.push((kind::SECRET, name, KEPT.as_bytes().to_vec()));
        }
        let state = json!({"version": fetched.version, "hosts": state});
        items.push((kind::MIRROR, "state".into(), state.to_string().into_bytes()));
        items.push((
            kind::MIRROR,
            "access".into(),
            fetched.access.to_string().into_bytes(),
        ));
        let items: Vec<(&str, &str, Vec<u8>)> = items
            .iter()
            .map(|(kind, name, value)| (*kind, name.as_str(), value.clone()))
            .collect();
        crate::store::shared(paths)?.replace(
            &self.scope(),
            &[kind::PROFILE, kind::REGISTRY, kind::SECRET, kind::MIRROR],
            &items,
        )?;
        Ok(())
    }

    /// A row the last mirror wrote, as JSON; null when there is none.
    fn mirrored(&self, paths: &Paths, name: &str) -> Value {
        crate::store::shared(paths)
            .ok()
            .and_then(|store| store.read_json(kind::MIRROR, &self.scope(), name).ok())
            .flatten()
            .unwrap_or(Value::Null)
    }

    /// What the last mirror found out about the server and its hosts.
    pub fn known(&self, paths: &Paths) -> Known {
        let state = self.mirrored(paths, "state");
        let hosts = state["hosts"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(name, host)| {
                let text = |key: &str| host[key].as_str().map(str::to_owned);
                (
                    name.clone(),
                    Reach {
                        reachable: host["reachable"] == true,
                        error: text("error"),
                        version: text("version"),
                        rotating: host["rotating"] == true,
                    },
                )
            })
            .collect();
        Known {
            version: state["version"].as_str().map(str::to_owned),
            hosts,
        }
    }

    async fn fetch(&self) -> Result<Fetched> {
        let portal = self.portal().await?;
        let call = |op: &'static str, args: Value| self.ask(&portal, op, args, None);
        let listed = call("hosts.list", Value::Null).await?;
        // A server too old to tell its version is shown as not knowing it.
        let version = call("version", Value::Null)
            .await
            .ok()
            .and_then(|version| version["version"].as_str().map(str::to_owned));
        // A server from before roles were there lets its token do everything
        // it has, which is not set-up.
        let access = call("access", Value::Null)
            .await
            .unwrap_or_else(|_| json!({"role": "operate", "setup": false}));
        let admin = access["role"] == "admin" && access["setup"] == true;
        let mut hosts = vec![];
        for host in listed.as_array().cloned().unwrap_or_default() {
            let profile = match (admin, host["name"].as_str()) {
                (true, Some(name)) => call("hosts.profile", json!({"name": name})).await.ok(),
                _ => None,
            };
            hosts.push((host, profile));
        }
        let (mut registries, mut secrets) = (json!({}), vec![]);
        if admin {
            registries = call("registries.list", Value::Null).await?["registries"].take();
            secrets = call("secrets.list", Value::Null).await?["names"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|name| name.as_str().map(str::to_owned))
                .collect();
        }
        Ok(Fetched {
            hosts,
            registries,
            secrets,
            access,
            version,
        })
    }

    /// Whether this berth may set the server up, as the last mirror found.
    pub fn admin(&self, paths: &Paths) -> bool {
        let access = self.mirrored(paths, "access");
        access["role"] == "admin" && access["setup"] == true
    }

    /// Give the server a profile to keep under a name. What marks it as the
    /// server's stays here.
    pub fn save_host(&self, name: &str, profile: &Value) -> Result<()> {
        let mut profile = profile.clone();
        if let Some(builder) = profile["builder"].as_object_mut() {
            builder.remove("served");
            builder.remove("served_only");
        }
        self.call("hosts.save", json!({"name": name}), Some(profile))?;
        Ok(())
    }
}

/// What stands for a secret's value here: the server keeps it.
pub const KEPT: &str = "(kept on the berth server)";

struct Fetched {
    /// What `hosts.list` says of each host, with its profile for an admin.
    hosts: Vec<(Value, Option<Value>)>,
    registries: Value,
    secrets: Vec<String>,
    access: Value,
    /// The server's own version, when it tells.
    version: Option<String>,
}

/// Whether one host answered the server when the mirror was made.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Reach {
    pub reachable: bool,
    /// Why it did not.
    pub error: Option<String>,
    /// The version of its berthd.
    pub version: Option<String>,
    /// A rotation of a key it logs in with has not reached it yet.
    pub rotating: bool,
}

/// What a mirror kept of the server beside the profiles.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Known {
    /// The version of the server, when it said.
    pub version: Option<String>,
    pub hosts: std::collections::BTreeMap<String, Reach>,
}

/// The operation a server does not have, when that is what the error says.
pub fn missing_operation(error: &Error) -> Option<&str> {
    match error {
        Error::Api { code, message, .. } if code == "not_found" => {
            message.strip_prefix("No operation ")
        }
        _ => None,
    }
}

/// What to tell of an operation the server does not have.
pub fn unsupported(url: &str, version: Option<&str>, error: &Error) -> String {
    let op = missing_operation(error).unwrap_or("this");
    match version {
        Some(version) => format!(
            "The berth server at {url} runs {version}, which does not have {op} yet. Update the berth server."
        ),
        None => format!(
            "The berth server at {url} does not have {op} yet, so it is older than this berth. Update the berth server."
        ),
    }
}

/// A name that is a file name and nothing else.
pub fn safe(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(['.', '-'])
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_taken_apart() {
        assert_eq!(
            parts("http://127.0.0.1:7777").unwrap(),
            ("127.0.0.1".into(), 7777, false)
        );
        assert_eq!(
            parts("https://berth.example.ts.net").unwrap(),
            ("berth.example.ts.net".into(), 443, true)
        );
        assert_eq!(
            parts("http://[::1]:7777/").unwrap(),
            ("[::1]".into(), 7777, false)
        );
        assert_eq!(parts("https://[::1]").unwrap(), ("[::1]".into(), 443, true));
        assert!(parts("ftp://x").is_err());
        assert!(parts("http://").is_err());
        assert!(safe("studio-1") && !safe("../x") && !safe(".x") && !safe("a/b"));
    }

    #[test]
    fn a_missing_operation_is_said_in_words() {
        let error = Error::api(berth_api::code::NOT_FOUND, "No operation instances.get");
        assert_eq!(missing_operation(&error), Some("instances.get"));
        assert_eq!(
            unsupported("http://192.0.2.7:7777", Some("0.3.0"), &error),
            "The berth server at http://192.0.2.7:7777 runs 0.3.0, which does not have instances.get yet. Update the berth server."
        );
        assert!(
            unsupported("http://192.0.2.7:7777", None, &error).contains("older than this berth")
        );
        // Another not-found is not this.
        let host = Error::api(berth_api::code::NOT_FOUND, "No host studio");
        assert_eq!(missing_operation(&host), None);
        assert_eq!(
            missing_operation(&Error::Transport("No operation x".into())),
            None
        );
    }

    fn scratch() -> (tempfile::TempDir, Paths, Server) {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: directory.path().join("config"),
            data_dir: directory.path().join("data"),
        };
        let server = Server {
            url: "http://192.0.2.7:7777".into(),
            token: None,
        };
        (directory, paths, server)
    }

    fn fetched(registry: &str, secret: &str, admin: bool) -> Fetched {
        Fetched {
            hosts: vec![
                (
                    json!({"name": "studio", "reachable": true, "version": "0.3.0"}),
                    Some(
                        json!({"hostname": "studio", "builder": {}, "access": {"address": "192.0.2.20"}}),
                    ),
                ),
                (
                    json!({"name": "attic", "reachable": false, "error": "timed out"}),
                    Some(json!({"hostname": "attic", "builder": {}})),
                ),
            ],
            registries: json!({registry: {"type": "npm", "url": "https://npm.example"}}),
            secrets: vec![secret.to_owned(), format!("registry/{registry}")],
            access: json!({"role": if admin { "admin" } else { "operate" }, "setup": true}),
            version: Some("0.3.0".into()),
        }
    }

    #[test]
    fn a_mirror_keeps_who_answers() {
        let (_directory, paths, server) = scratch();
        assert_eq!(server.known(&paths), Known::default());
        assert!(!server.admin(&paths));
        server.keep(&paths, fetched("npm", "github", true)).unwrap();
        let known = server.known(&paths);
        assert_eq!(known.version.as_deref(), Some("0.3.0"));
        assert!(known.hosts["studio"].reachable);
        assert_eq!(known.hosts["attic"].error.as_deref(), Some("timed out"));
        assert!(server.admin(&paths));
        // The profiles are in the store, under the server's scope, and no file is written.
        let studio = crate::profiles::Place::stored(&server.scope(), "studio")
            .unwrap()
            .load(&paths)
            .unwrap()
            .unwrap();
        assert_eq!(studio["builder"]["served"], json!(server.url));
        assert_eq!(studio["access"]["address"], "192.0.2.20");
        assert!(!paths.data_dir.join("served").exists());
    }

    #[test]
    fn a_mirror_replaces_its_scope_and_leaves_the_own_alone() {
        let (_directory, paths, server) = scratch();
        let store = crate::store::shared(&paths).unwrap();
        let own = crate::store::OWN;
        store
            .write(kind::SECRET, own, "github", b"ghp-made-up")
            .unwrap();
        store.write(kind::REGISTRY, own, "npm", b"{}").unwrap();
        store
            .write(kind::PROFILE, own, "studio", br#"{"builder": {}}"#)
            .unwrap();
        server.keep(&paths, fetched("npm", "github", true)).unwrap();
        let profiles = |scope: &str| store.names(kind::PROFILE, scope).unwrap();
        assert_eq!(profiles(&server.scope()), ["attic", "studio"]);
        server
            .keep(&paths, fetched("hub", "deploy", false))
            .unwrap();
        let scope = server.scope();
        assert_eq!(scope, "served:http://192.0.2.7:7777");
        assert_eq!(store.names(kind::REGISTRY, &scope).unwrap(), ["hub"]);
        assert_eq!(
            store.names(kind::SECRET, &scope).unwrap(),
            ["deploy", "registry/hub"]
        );
        for (_, value) in store.entries(kind::SECRET, &scope).unwrap() {
            assert_eq!(value, KEPT.as_bytes());
        }
        assert!(!server.admin(&paths));
        assert_eq!(
            store.read(kind::SECRET, own, "github").unwrap().unwrap(),
            b"ghp-made-up"
        );
        assert_eq!(store.names(kind::REGISTRY, own).unwrap(), ["npm"]);
        // The host of this installation's own stays as it was.
        assert_eq!(profiles(own), ["studio"]);
        assert_eq!(
            store.read(kind::PROFILE, own, "studio").unwrap().unwrap(),
            br#"{"builder": {}}"#
        );
    }
}
