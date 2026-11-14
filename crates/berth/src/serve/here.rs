//! This installation's API for a client in the same process, which berth's
//! screens use when no server is set: the router of berth serve's own
//! socket, over pipes in memory. Like a caller on that socket, the caller is
//! the user the installation belongs to, so it is an admin and shows no
//! token. Nothing listens anywhere: only this process can reach it.

use super::{Serve, api, auth};
use crate::Result;
use berth_api::client::{Dial, Error, Stream};
use berth_core::{paths::Paths, store};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::DuplexStream;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// How much a pipe holds in each direction before a writer waits.
const PIPE: usize = 256 * 1024;

/// The socket a running `berth serve` of this user answers on, which is
/// only its user's.
pub fn socket() -> std::path::PathBuf {
    berth_core::link::runtime_dir().join("serve.sock")
}

/// The pipes clients open, as connections that axum serves.
struct Pipes(UnboundedReceiver<DuplexStream>);

impl axum::serve::Listener for Pipes {
    type Io = DuplexStream;
    type Addr = ();

    async fn accept(&mut self) -> (DuplexStream, ()) {
        match self.0.recv().await {
            Some(pipe) => (pipe, ()),
            // Every client is gone, and none can come.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<()> {
        Ok(())
    }
}

pub struct Here {
    state: Arc<Serve>,
    connections: UnboundedSender<DuplexStream>,
    /// Runs the server's tasks, and ends them when dropped.
    _runtime: tokio::runtime::Runtime,
}

impl Here {
    /// Open the store and serve its API to this process. The hosts' channels
    /// are left to `berth serve`, and so are jobs a server before left
    /// unfinished, which another berth beside this one may be running.
    pub fn start(paths: &Paths) -> Result<Here> {
        let store = super::open_store(paths)?;
        crate::restore_plugins(paths);
        let mut state = Serve::with(paths.clone(), store, auth::Auth::nobody());
        if let Some(state) = Arc::get_mut(&mut state) {
            state.follows = false;
        }
        std::fs::create_dir_all(&state.jobs_dir)?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("berth-here")
            .enable_all()
            .build()?;
        let (connections, pipes) = unbounded_channel();
        let router = api::router(state.clone(), false);
        let serving = state.clone();
        runtime.spawn(async move {
            serving.refresh(false);
            let _ = axum::serve(Pipes(pipes), router).await;
        });
        // Another berth beside this one changes the store too.
        let watching = state.clone();
        runtime.spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                if watching.events.receiver_count() > 0 {
                    watching.notice();
                }
            }
        });
        Ok(Here {
            state,
            connections,
            _runtime: runtime,
        })
    }

    /// A new connection to the server for each call.
    pub fn dial(&self) -> Dial {
        let connections = self.connections.clone();
        Arc::new(move || {
            let connections = connections.clone();
            Box::pin(async move {
                let (client, server) = tokio::io::duplex(PIPE);
                connections
                    .send(server)
                    .map_err(|_| Error::Transport("berth's own server has stopped".into()))?;
                Ok(Box::new(client) as Stream)
            })
        })
    }

    /// The jobs that run in this process, which end when it does.
    pub fn running(&self) -> Vec<store::Job> {
        self.state.runner.running()
    }

    /// Wait until no job runs in this process, saying first which do.
    pub fn wait_for_jobs(&self, say: impl Fn(&[store::Job])) {
        let running = self.running();
        if running.is_empty() {
            return;
        }
        say(&running);
        while !self.running().is_empty() {
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use berth_api::client::{self, Client, Frame};
    use futures_util::StreamExt;
    use serde_json::{Value, json};

    fn scratch() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: dir.path().join("config"),
            data_dir: dir.path().join("data"),
        };
        (dir, paths)
    }

    #[test]
    fn a_client_in_the_process_calls_streams_and_runs_a_job_to_its_end() {
        let (_dir, paths) = scratch();
        let profile = json!({"hostname": "dock", "builder": {}, "access": {"address": "192.0.2.7", "link": ["sh", "-c", "exit 1"]}});
        berth_core::profiles::Place::own("dock")
            .unwrap()
            .save(&paths, &profile)
            .unwrap();
        let here = Here::start(&paths).unwrap();
        let dial = here.dial();
        // The client has a runtime of its own, as the screens do.
        let client_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        client_runtime.block_on(async {
            // A call, as the owner, with no token.
            let mut client = Client::over(dial().await.unwrap()).await.unwrap();
            let access: Value = client.get("/v1/access").await.unwrap();
            assert_eq!(
                (&access["role"], &access["setup"]),
                (&json!("admin"), &json!(true))
            );

            // The portal over a connection of its own.
            let socket = client::websocket(dial().await.unwrap(), berth_api::ops::PORTAL, "")
                .await
                .unwrap();
            let portal = berth_api::portal::Portal::over(socket);
            let hosts = portal
                .call("hosts.list", None, json!({}), None)
                .await
                .unwrap();
            assert_eq!(hosts[0]["name"], "dock", "{hosts}");

            // A job, followed on its stream to its end.
            let body = json!({"arguments": {"name": "studio", "hostname": "yes"}});
            let answer = client
                .send(
                    "POST",
                    "/v1/hosts/dock/actions/rename",
                    &[("content-type", "application/json")],
                    body.to_string().into(),
                )
                .await
                .unwrap();
            assert_eq!(answer.status, 202);
            let acted: Value = serde_json::from_slice(&answer.body).unwrap();
            let id = acted["job"]["id"].as_str().unwrap().to_owned();
            let mut stream =
                client::websocket(dial().await.unwrap(), &format!("/v1/jobs/{id}"), "")
                    .await
                    .unwrap();
            let ended = loop {
                let frame = tokio::time::timeout(Duration::from_secs(20), stream.next())
                    .await
                    .expect("a frame in time");
                match frame {
                    Some(Ok(Frame::Text(text))) => {
                        let said: Value = serde_json::from_str(text.as_str()).unwrap();
                        if said["exited"].is_object() {
                            break said["exited"].clone();
                        }
                    }
                    Some(Ok(_)) => {}
                    other => panic!("the stream ended: {other:?}"),
                }
            };
            assert_eq!(ended["exit_code"], 0, "{ended}");
        });
        here.wait_for_jobs(|_| {});
        assert!(here.running().is_empty());
        assert!(
            berth_core::profiles::Place::own("studio")
                .unwrap()
                .load(&paths)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn nothing_listens_for_it() {
        let (dir, paths) = scratch();
        let _here = Here::start(&paths).unwrap();
        // No socket, no token and no port are made for a server in the process.
        assert!(!paths.data_dir.join("serve.token").exists());
        let made: Vec<_> = walk(dir.path());
        assert!(
            !made
                .iter()
                .any(|path| path.extension().is_some_and(|end| end == "sock")),
            "{made:?}"
        );
    }

    fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut found = vec![];
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.extend(walk(&path));
            } else {
                found.push(path);
            }
        }
        found
    }
}
