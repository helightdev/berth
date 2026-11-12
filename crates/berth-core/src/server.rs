//! A `berth serve` somewhere else, as this berth's way to its hosts. With the
//! `server` setting (or `BERTH_SERVER`) berth keeps no hosts of its own: it
//! shows the server's and reaches them through the server's portal.
//!
//! - `http://HOST:PORT`, `https://HOST[:PORT]`: over TCP, with the token
//! - `unix:/path/to/serve.sock`: the socket of a `berth serve` on this machine

use crate::{Result, paths::Paths};
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
        let settings = paths.settings().unwrap_or_default();
        let setting = |key: &str| settings.get(key).and_then(Value::as_str).map(str::to_owned);
        let variable = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
        let url = variable("BERTH_SERVER").or_else(|| setting("server"))?;
        Some(Server {
            url: url.trim_end_matches('/').to_owned(),
            token: variable("BERTH_TOKEN").or_else(|| setting("server_token")),
        })
    }

    /// Open the server's portal. Without a token of the server's, berth
    /// shows an access token of its own when it has or can get one.
    pub async fn portal(&self) -> client::Result<Portal> {
        let own = match (&self.token, self.url.starts_with("unix:")) {
            (None, false) => crate::oauth::token(&self.url)
                .await
                .map_err(|error| Error::Transport(error.to_string()))?,
            _ => None,
        };
        let token = own.as_deref().or(self.token.as_deref()).unwrap_or("");
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
        Ok(crate::link::runtime()?
            .block_on(async { self.portal().await?.call(op, None, args, data).await })?)
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

    /// Where what is known of this server is kept: a folder of the data
    /// directory per server, written anew by [`Server::mirror`].
    pub fn dir(&self, paths: &Paths) -> std::path::PathBuf {
        let name: String = self
            .url
            .chars()
            .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
            .collect();
        paths.data_dir.join("served").join(name)
    }

    /// The profiles of the server's hosts.
    pub fn hosts_dir(&self, paths: &Paths) -> std::path::PathBuf {
        self.dir(paths).join("hosts")
    }

    /// Write what the server has: a profile for each of its hosts, its
    /// registries and the names of its secrets. Whoever may set the server up
    /// gets the profiles as they are there, and saving one sends it back;
    /// anyone else gets profiles that only say where the hosts are.
    pub fn mirror(&self, paths: &Paths) -> Result<()> {
        let fetched = crate::link::runtime()?.block_on(self.fetch())?;
        let dir = self.dir(paths);
        let _ = std::fs::remove_dir_all(&dir);
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir.join("hosts"))?;
        }
        // A whole profile, so every screen has what it expects; only where the
        // host is and that it is the server's are its own.
        let example = std::fs::read(paths.source()?.join(crate::paths::layout::EXAMPLE))?;
        let example: Value = serde_json::from_slice(&example)?;
        for (host, profile) in fetched.hosts {
            let Some(name) = host["name"].as_str().filter(|name| safe(name)) else {
                continue;
            };
            let mut profile = match profile {
                Some(profile) if profile.is_object() => profile,
                _ => {
                    let mut profile = example.clone();
                    profile["builder"]["ssh_host"] = host["address"].clone();
                    profile["builder"]["ask_become_pass"] = json!(false);
                    profile["builder"]["served_only"] = json!(true);
                    profile
                }
            };
            profile["builder"]["served"] = json!(self.url);
            private(
                &dir.join("hosts").join(format!("{name}.json")),
                &serde_json::to_string_pretty(&profile)?,
            )?;
        }
        private(
            &dir.join("config.json"),
            &serde_json::to_string_pretty(&json!({"registries": fetched.registries}))?,
        )?;
        let secrets: serde_json::Map<String, Value> = fetched
            .secrets
            .into_iter()
            .map(|name| (name, json!(KEPT)))
            .collect();
        private(
            &dir.join("secrets.json"),
            &serde_json::to_string_pretty(&secrets)?,
        )?;
        private(
            &dir.join("access.json"),
            &serde_json::to_string_pretty(&fetched.access)?,
        )?;
        Ok(())
    }

    async fn fetch(&self) -> client::Result<Fetched> {
        let portal = self.portal().await?;
        let call = |op: &'static str, args: Value| portal.call(op, None, args, None);
        let listed = call("hosts.list", Value::Null).await?;
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
        })
    }

    /// Whether this berth may set the server up, as the last mirror found.
    pub fn admin(&self, paths: &Paths) -> bool {
        std::fs::read(self.dir(paths).join("access.json"))
            .ok()
            .and_then(|data| serde_json::from_slice::<Value>(&data).ok())
            .is_some_and(|access| access["role"] == "admin" && access["setup"] == true)
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
}

fn private(path: &std::path::Path, text: &str) -> Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(format!("{text}\n").as_bytes())?;
    Ok(())
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
}
