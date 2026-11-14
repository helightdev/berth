//! The network under the screens: TCP or TLS to a `berth serve`, plain HTTP
//! requests to it, and a way back to the thread that draws.

use berth_api::client::{self, Error};
use std::cell::RefCell;
use std::future::Future;
use std::sync::{Arc, OnceLock, mpsc};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

/// A connection, over TLS or not.
pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
pub type Stream = Box<dyn Io>;

/// The runtime the connections run on; the screens stay on the main thread.
pub fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("the runtime starts")
    })
}

/// Where an address goes: the name and port to connect to, the authority to
/// name in requests, the path after it, and whether over TLS.
pub struct Target {
    pub name: String,
    pub port: u16,
    pub authority: String,
    pub path: String,
    pub secure: bool,
}

/// Take an `http(s)://` or `ws(s)://` address apart.
pub fn target(url: &str) -> Result<Target, String> {
    let (secure, rest) = if let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("wss://"))
    {
        (true, rest)
    } else if let Some(rest) = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("ws://"))
    {
        (false, rest)
    } else {
        return Err(format!("{url} is no http:// or https:// address"));
    };
    let (authority, path) = match rest.find('/') {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, "/"),
    };
    let default = if secure { 443 } else { 80 };
    let (host, port) = match authority.rsplit_once(':') {
        // Not the colons of an IPv6 address in brackets.
        Some((host, port)) if !port.contains(']') => (
            host,
            port.parse().map_err(|_| format!("{port} is no port"))?,
        ),
        _ => (authority, default),
    };
    if host.is_empty() {
        return Err(format!("{url} names no host"));
    }
    Ok(Target {
        name: host.trim_matches(['[', ']']).to_owned(),
        port,
        authority: format!("{host}:{port}"),
        path: path.to_owned(),
        secure,
    })
}

fn tls() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let mut roots =
                rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            // The system's own as well, so that a server with a certificate of
            // an authority this machine trusts, such as a company's, is taken.
            // A certificate the system store cannot give is left out.
            let _ = roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let config = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("ring has the default protocol versions")
                .with_root_certificates(roots)
                .with_no_client_auth();
            Arc::new(config)
        })
        .clone()
}

/// Connect to where `target` points, with TLS when it says so.
pub async fn connect(target: &Target) -> Result<Stream, String> {
    let stream = TcpStream::connect((target.name.as_str(), target.port))
        .await
        .map_err(|error| format!("{}: {error}", target.authority))?;
    let _ = stream.set_nodelay(true);
    if !target.secure {
        return Ok(Box::new(stream));
    }
    let name = rustls::pki_types::ServerName::try_from(target.name.clone())
        .map_err(|error| error.to_string())?;
    let stream = tokio_rustls::TlsConnector::from(tls())
        .connect(name, stream)
        .await
        .map_err(|error| format!("{}: {error}", target.authority))?;
    Ok(Box::new(stream))
}

/// One request with a bearer: the status and the body of the answer.
pub async fn http(method: &str, url: &str, bearer: &str) -> Result<(u16, String), String> {
    let target = target(url)?;
    let stream = connect(&target).await?;
    let mut client = client::Client::over(stream)
        .await
        .map_err(|error| describe(&error))?;
    let authorization = format!("Bearer {bearer}");
    let headers = [
        ("host", target.authority.as_str()),
        ("accept", "application/json"),
        ("authorization", authorization.as_str()),
    ];
    let answer = client
        .send(method, &target.path, &headers, Default::default())
        .await
        .map_err(|error| describe(&error))?;
    Ok((
        answer.status,
        String::from_utf8_lossy(&answer.body).into_owned(),
    ))
}

pub fn describe(error: &Error) -> String {
    match error {
        Error::Api { message, .. } => message.clone(),
        Error::Transport(message) => message.clone(),
    }
}

type Waiting = Box<dyn FnMut() -> bool>;

thread_local! {
    /// What the main thread waits for: each says whether it is done.
    static WAITING: RefCell<Vec<Waiting>> = RefCell::default();
}

/// Hand `then` what `work` gives, on the main thread, at a later [`pump`].
pub fn later<T: Send + 'static>(
    work: impl Future<Output = T> + Send + 'static,
    then: impl FnOnce(T) + 'static,
) {
    let (sender, receiver) = mpsc::channel();
    runtime().spawn(async move {
        let _ = sender.send(work.await);
    });
    wait(receiver, then);
}

/// The same for work that blocks, such as a program that is waited for.
pub fn later_blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
    then: impl FnOnce(T) + 'static,
) {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(work());
    });
    wait(receiver, then);
}

fn wait<T: 'static>(receiver: mpsc::Receiver<T>, then: impl FnOnce(T) + 'static) {
    let mut then = Some(then);
    WAITING.with(|waiting| {
        waiting
            .borrow_mut()
            .push(Box::new(move || match receiver.try_recv() {
                Ok(value) => {
                    if let Some(then) = then.take() {
                        then(value);
                    }
                    true
                }
                Err(mpsc::TryRecvError::Empty) => false,
                Err(mpsc::TryRecvError::Disconnected) => true,
            }))
    });
}

/// Run what has arrived. What it runs may wait for more.
pub fn pump() {
    let mut taken = WAITING.with(|waiting| std::mem::take(&mut *waiting.borrow_mut()));
    taken.retain_mut(|done| !done());
    WAITING.with(|waiting| {
        let mut waiting = waiting.borrow_mut();
        taken.append(&mut waiting);
        *waiting = taken;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_taken_apart() {
        let at = target("wss://studio.example/v1/portal").unwrap();
        assert_eq!(
            (at.name.as_str(), at.port, at.authority.as_str()),
            ("studio.example", 443, "studio.example:443")
        );
        assert_eq!((at.path.as_str(), at.secure), ("/v1/portal", true));
        let at = target("http://192.0.2.10:7878").unwrap();
        assert_eq!((at.name.as_str(), at.port), ("192.0.2.10", 7878));
        assert_eq!((at.path.as_str(), at.secure), ("/", false));
        let at = target("ws://[2001:db8::1]:7878/v1/portal").unwrap();
        assert_eq!((at.name.as_str(), at.port), ("2001:db8::1", 7878));
        let at = target("https://[2001:db8::1]/").unwrap();
        assert_eq!((at.name.as_str(), at.port), ("2001:db8::1", 443));
        for wrong in [
            "studio.example",
            "ftp://studio.example",
            "https://",
            "http://studio:port",
        ] {
            assert!(target(wrong).is_err(), "{wrong}");
        }
    }
}
