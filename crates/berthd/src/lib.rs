//! berthd: the host's API. Sessions, tasks, system calls, a channel and
//! services, over unix sockets.

mod channel;
mod error;
mod files;
pub mod http;
pub mod portal;
mod services;
mod sessions;
pub mod state;
mod system;
mod tasks;
mod transcript;
mod users;

pub use files::helper_main;
pub use state::{Caller, Mode, State};

use axum::extract::Extension;
use berth_api::Level;
use hyper_util::{rt::TokioIo, service::TowerToHyperService};
use std::{os::unix::fs::PermissionsExt, path::Path, sync::Arc};
use tokio::net::UnixListener;

/// The OpenAPI document of berthd.
pub fn openapi() -> serde_json::Value {
    http::openapi()
}

/// The same paths under `/v1{prefix}`, and the schemas, for `berth serve`.
pub fn openapi_parts(
    prefix: &str,
) -> (
    serde_json::Map<String, serde_json::Value>,
    serde_json::Value,
) {
    (http::paths(prefix), http::components())
}

pub fn asyncapi() -> serde_json::Value {
    http::asyncapi()
}

/// The API by operation, each with its HTTP request and its portal frame.
pub fn operations() -> serde_json::Value {
    http::operations()
}

fn listen(path: &Path, mode: u32, group: Option<u32>) -> std::io::Result<UnixListener> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    if let Some(group) = group {
        std::os::unix::fs::chown(path, None, Some(group))?;
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(listener)
}

async fn accept(listener: UnixListener, level: Level, router: axum::Router) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            continue;
        };
        // Who it is comes from the system, never from the caller.
        let Ok(peer) = stream.peer_cred() else {
            continue;
        };
        let caller = Caller {
            level,
            uid: peer.uid(),
            pid: peer.pid(),
        };
        let service = TowerToHyperService::new(router.clone().layer(Extension(caller)));
        tokio::spawn(async move {
            let connection = hyper::server::conn::http1::Builder::new()
                .half_close(true)
                .serve_connection(TokioIo::new(stream), service);
            let _ = connection.with_upgrades().await;
        });
    }
}

/// Listen on the two sockets in `run_dir` until told to stop.
pub async fn serve(state: Arc<State>, run_dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(run_dir)?;
    let agent_group = match state.mode {
        Mode::System => {
            std::fs::set_permissions(run_dir, std::fs::Permissions::from_mode(0o755))?;
            users_group(&state)
        }
        Mode::Single => None,
    };
    let control = listen(&run_dir.join(berth_api::CONTROL_SOCKET), 0o600, None)?;
    let agent_mode = if agent_group.is_some() { 0o660 } else { 0o600 };
    let agent = listen(
        &run_dir.join(berth_api::AGENT_SOCKET),
        agent_mode,
        agent_group,
    )?;
    let router = http::router(state.clone());
    tokio::spawn(accept(control, Level::Host, router.clone()));
    tokio::spawn(accept(agent, Level::Agent, router));

    let tidy = state.clone();
    tokio::spawn(async move {
        loop {
            tidy.sessions.tidy();
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    });

    use tokio::signal::unix::{SignalKind, signal};
    let (mut hangup, mut terminate, mut interrupt) = (
        signal(SignalKind::hangup())?,
        signal(SignalKind::terminate())?,
        signal(SignalKind::interrupt())?,
    );
    loop {
        tokio::select! {
            _ = hangup.recv() => {
                for problem in state.reload() {
                    eprintln!("berthd: {problem}");
                }
            }
            _ = terminate.recv() => break,
            _ = interrupt.recv() => break,
        }
    }
    // Sessions end with berthd.
    for session in state
        .sessions
        .list()
        .into_iter()
        .filter(|session| session.running())
    {
        if let Ok(session) = state.sessions.get(&session.id) {
            session.lose();
        }
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    for session in state
        .sessions
        .list()
        .into_iter()
        .filter(|session| session.running())
    {
        if let Ok(session) = state.sessions.get(&session.id) {
            session.signal(libc::SIGKILL);
        }
    }
    let _ = std::fs::remove_file(run_dir.join(berth_api::CONTROL_SOCKET));
    let _ = std::fs::remove_file(run_dir.join(berth_api::AGENT_SOCKET));
    Ok(())
}

/// The agent's group, which may open the agent's socket.
fn users_group(state: &State) -> Option<u32> {
    let name = state.config().berthd.agent.clone();
    let account = std::fs::read_to_string("/etc/passwd")
        .ok()?
        .lines()
        .find_map(|line| {
            let fields: Vec<&str> = line.split(':').collect();
            (fields.len() > 3 && fields[0] == name).then(|| fields[3].parse::<u32>().ok())?
        });
    if account.is_none() {
        eprintln!("berthd: no account {name}; the agent's socket stays closed to it");
    }
    account
}
