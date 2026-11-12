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

    /// Open the server's portal.
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
        let roots =
            rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(transport)?
            .with_root_certificates(roots)
            .with_no_client_auth();
        let server = rustls::pki_types::ServerName::try_from(name).map_err(transport)?;
        let stream = tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect(server, stream)
            .await
            .map_err(|error| Error::Transport(format!("{}: {error}", self.url)))?;
        let socket = client::websocket_at(stream, &authority, ops::PORTAL, token).await?;
        Ok(Portal::over(socket))
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

    /// Where the hosts of this server are kept as profiles: a folder of the
    /// data directory per server, written anew by [`Server::mirror`].
    pub fn hosts_dir(&self, paths: &Paths) -> std::path::PathBuf {
        let name: String = self
            .url
            .chars()
            .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
            .collect();
        paths.data_dir.join("served").join(name)
    }

    /// Write a profile for each of the server's hosts, and remove those of
    /// hosts it no longer has. Such a profile only says where the host is.
    pub fn mirror(&self, paths: &Paths) -> Result<()> {
        let hosts = crate::link::runtime()?.block_on(self.hosts())?;
        let dir = self.hosts_dir(paths);
        let _ = std::fs::remove_dir_all(&dir);
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&dir)?;
        }
        // A whole profile, so every screen has what it expects; only where the
        // host is and that it is the server's are its own.
        let example = std::fs::read(paths.source()?.join("config/example.json"))?;
        let example: Value = serde_json::from_slice(&example)?;
        for host in hosts {
            let Some(name) = host["name"].as_str().filter(|name| safe(name)) else {
                continue;
            };
            let mut profile = example.clone();
            profile["builder"]["ssh_host"] = host["address"].clone();
            profile["builder"]["served"] = json!(self.url);
            profile["builder"]["ask_become_pass"] = json!(false);
            std::fs::write(
                dir.join(format!("{name}.json")),
                serde_json::to_string_pretty(&profile)? + "\n",
            )?;
        }
        Ok(())
    }
}

/// A name that is a file name and nothing else.
fn safe(name: &str) -> bool {
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
