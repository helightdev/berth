//! Plain HTTP and HTTPS requests of berth's own: an OAuth issuer's keys, a
//! resource's page, a release.

use berth_api::client::{self, Error};
use std::sync::Arc;
use tokio::net::TcpStream;

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
