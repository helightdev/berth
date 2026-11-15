//! `berth serve --simulate` as an application's test sees it: started from a
//! seed, logged in to over HTTP with its token, and driven through a host's
//! whole life, with the events of the portal followed. It runs the berth
//! that was built, in scratch directories, with made-up hosts.
#![cfg(target_os = "linux")]

use berth_api::client::{self, Client};
use berth_api::portal::{Piece, Portal, PortalStream};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const TOKEN: &str = "made-up-token-of-a-simulated-server-0123";

/// A simulated server, ended with SIGTERM when dropped.
struct Simulated {
    child: Child,
    address: String,
    state: PathBuf,
    _scratch: tempfile::TempDir,
}

impl Drop for Simulated {
    fn drop(&mut self) {
        terminate(&mut self.child);
    }
}

fn terminate(child: &mut Child) {
    // SAFETY: a plain system call on a child of ours.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
}

impl Simulated {
    /// Started with `seed`, its scratch state in a directory of its own.
    fn start(seed: &Value) -> Simulated {
        let scratch = tempfile::Builder::new().prefix("bs").tempdir().unwrap();
        let path = scratch.path();
        for sub in ["r", "c", "d", "home", "tmp"] {
            std::fs::create_dir_all(path.join(sub)).unwrap();
        }
        std::fs::write(path.join("seed.json"), seed.to_string()).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_berth"))
            .args(["serve", "--simulate", "--listen", "127.0.0.1:0"])
            .args(["--delay", "0.1", "--seed"])
            .arg(path.join("seed.json"))
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", path.join("home"))
            .env("TMPDIR", path.join("tmp"))
            .env("XDG_RUNTIME_DIR", path.join("r"))
            .env("XDG_CONFIG_HOME", path.join("c"))
            .env("XDG_DATA_HOME", path.join("d"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let (mut address, mut state, mut token) = (None, None, None);
        while address.is_none() || token.is_none() {
            let line = lines
                .next()
                .expect("the server says where it listens")
                .unwrap();
            if let Some(rest) = line.strip_prefix("Listening on http://") {
                address = rest.split_whitespace().next().map(str::to_owned);
            } else if let Some(rest) = line.strip_prefix("Token: ") {
                token = Some(rest.to_owned());
            } else if let Some(rest) = line.split("State in ").nth(1) {
                state = rest.split(',').next().map(PathBuf::from);
            }
        }
        assert_eq!(token.as_deref(), Some(TOKEN), "the seed's token");
        // The rest of what it says is not read.
        std::thread::spawn(move || lines.for_each(drop));
        Simulated {
            child,
            address: address.unwrap(),
            state: state.expect("it says where its state is"),
            _scratch: scratch,
        }
    }

    async fn send(&self, method: &str, path: &str, body: Option<Value>) -> client::Response {
        let stream = tokio::net::TcpStream::connect(&self.address).await.unwrap();
        let mut client = Client::over(stream).await.unwrap();
        let bearer = format!("Bearer {TOKEN}");
        let mut headers = vec![("authorization", bearer.as_str())];
        let body = match body {
            Some(body) => {
                headers.push(("content-type", "application/json"));
                body.to_string().into_bytes()
            }
            None => vec![],
        };
        client
            .send(method, path, &headers, body.into())
            .await
            .unwrap()
    }

    /// A call that has to work, and its answer.
    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let response = self.send(method, path, body).await;
        assert!(
            (200..300).contains(&response.status),
            "{method} {path}: {} {}",
            response.status,
            String::from_utf8_lossy(&response.body)
        );
        assert_eq!(response.headers["berth-simulated"], "1");
        serde_json::from_slice(&response.body).unwrap()
    }

    async fn portal(&self) -> Portal {
        let stream = tokio::net::TcpStream::connect(&self.address).await.unwrap();
        let socket = client::websocket_at(stream, &self.address, "/v1/portal", TOKEN)
            .await
            .unwrap();
        Portal::over(socket)
    }

    /// The job once it has ended, which has to be with 0, and its output.
    async fn job_done(&self, acted: &Value) -> (Value, String) {
        let id = acted["job"]["id"].as_str().expect("a job").to_owned();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let job = self.call("GET", &format!("/v1/jobs/{id}"), None).await;
            if !job["ended"].is_null() {
                let output = self
                    .call("GET", &format!("/v1/jobs/{id}/output"), None)
                    .await;
                let data = berth_api::unb64(output["data_b64"].as_str().unwrap()).unwrap();
                let output = String::from_utf8_lossy(&data).into_owned();
                assert_eq!(job["exit_code"], 0, "{job}: {output}");
                return (job, output);
            }
            assert!(Instant::now() < deadline, "the job {id} ends in time");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn act(&self, host: &str, action: &str) -> (Value, String) {
        let path = format!("/v1/hosts/{host}/actions/{action}");
        let acted = self.call("POST", &path, Some(json!({}))).await;
        self.job_done(&acted).await
    }

    /// The host once `check` holds of it.
    async fn host_until(&self, name: &str, check: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let host = self.call("GET", &format!("/v1/hosts/{name}"), None).await;
            if check(&host) {
                return host;
            }
            assert!(Instant::now() < deadline, "{name} is not as asked: {host}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// The next change of the events stream that is `change` of `source`.
async fn change_of(events: &mut PortalStream, source: &str, change: &str) -> Value {
    let mut seen = vec![];
    loop {
        let piece = tokio::time::timeout(Duration::from_secs(20), events.recv())
            .await
            .unwrap_or_else(|_| panic!("no {source} {change} after {seen:?}"));
        match piece {
            Some(Ok(Piece::Text(frame)))
                if frame["source"] == source && frame["change"] == change =>
            {
                return frame;
            }
            Some(Ok(Piece::Text(frame))) => seen.push(frame),
            other => panic!("the events ended after {seen:?}: {other:?}"),
        }
    }
}

fn gone(dir: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while dir.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    !dir.exists()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_lives_its_whole_life_on_a_simulated_server() {
    let seed = json!({
        "token": TOKEN,
        "hosts": {"studio": {"applied": true}},
        "secrets": {"api_token": "made-up"}
    });
    let mut server = Simulated::start(&seed);

    // It says what it is, and lets nobody in without the token.
    let version = server.call("GET", "/v1/version", None).await;
    assert_eq!(
        (&version["mode"], &version["simulated"]),
        (&json!("serve"), &json!(true))
    );
    let stream = tokio::net::TcpStream::connect(&server.address)
        .await
        .unwrap();
    let refused = Client::over(stream)
        .await
        .unwrap()
        .send("GET", "/v1/hosts", &[], Vec::new().into())
        .await
        .unwrap();
    assert_eq!(refused.status, 401);
    assert_eq!(refused.headers["berth-simulated"], "1");

    // The seed's world.
    let studio = server
        .host_until("studio", |host| host["reachable"] == true)
        .await;
    assert_eq!(
        (&studio["applied"], &studio["address"]),
        (&json!(berth_api::VERSION), &json!("192.0.2.20"))
    );
    let secrets = server.call("GET", "/v1/secrets", None).await;
    // Only the names are compared; the list is not printed on failure.
    let names: Vec<&str> = secrets["names"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|name| name.as_str())
        .collect();
    assert!(
        names.contains(&"api_token"),
        "the seed's secret is not listed"
    );
    let provisioners = server.call("GET", "/v1/provisioners", None).await;
    assert_eq!(
        (&provisioners[0]["name"], &provisioners[0]["type"]),
        (&json!("sim"), &json!("sim/cloud"))
    );
    assert!(berth_api::ids::is_id(
        provisioners[0]["id"].as_str().unwrap_or("")
    ));
    let shown = server.call("GET", "/v1/provisioners/sim", None).await;
    assert!(
        shown.to_string().contains("ubuntu-26.04"),
        "its images: {shown}"
    );

    let portal = server.portal().await;
    let mut events = portal.open("events.follow", None, json!({})).await.unwrap();
    events
        .push(Piece::Text(json!({"changes": ["hosts", "resources"]})))
        .unwrap();

    // A new host, made by the simulated provisioner and applied.
    let profile = json!({"hostname": "dock", "builder": {}, "access": {"user": "ops"}});
    server
        .call("PUT", "/v1/hosts/dock/profile", Some(profile))
        .await;
    assert_eq!(
        change_of(&mut events, "host", "added").await["host"],
        "dock"
    );
    let made = json!({"provisioner": "sim", "kind": "machine", "host": "dock"});
    let acted = server.call("POST", "/v1/resources", Some(made)).await;
    let machine = acted["resource"]["id"].as_str().unwrap().to_owned();
    let (_, output) = server.job_done(&acted).await;
    assert!(output.contains("✓ dock is applied"), "{output}");
    assert_eq!(
        change_of(&mut events, "resource", "created").await["resource"],
        machine.as_str()
    );
    let dock = server
        .host_until("dock", |host| host["applied"].is_string())
        .await;
    assert!(
        dock["address"].as_str().unwrap().starts_with("192.0.2."),
        "{dock}"
    );
    assert_eq!(dock["power"]["state"], "running");

    // Applied again, with an action and a session of its controller.
    let (_, output) = server.act("dock", "apply").await;
    assert!(output.contains("PLAY RECAP dock"), "{output}");
    let (_, output) = server.act("dock", "status").await;
    assert!(output.contains("dock: simulated"), "{output}");
    let started = portal
        .call(
            "sessions.start",
            Some("dock"),
            json!({"wait": "10s"}),
            Some(json!({"kind": "process", "command": "hostname; echo hello; rm -rf /"})),
        )
        .await
        .unwrap();
    let said = |key: &str| {
        String::from_utf8(berth_api::unb64(started[key].as_str().unwrap()).unwrap()).unwrap()
    };
    assert_eq!(said("stdout_b64"), "dock\nhello\n");
    assert!(
        said("stderr_b64").contains("rm: not a command"),
        "{started}"
    );
    assert_eq!(started["exit_code"], 127);
    // Nothing of the machine it runs on is the host's.
    let processes = server
        .call("GET", "/v1/hosts/dock/controller/processes", None)
        .await;
    let state = server.state.to_string_lossy().into_owned();
    let commands: Vec<&str> = processes
        .as_array()
        .unwrap()
        .iter()
        .map(|process| process["command"].as_str().unwrap())
        .collect();
    assert!(
        !commands.is_empty() && commands.iter().all(|command| command.contains(&state)),
        "{commands:?}"
    );
    let port = server
        .send("GET", "/v1/hosts/dock/controller/ports/22", None)
        .await;
    let said = String::from_utf8_lossy(&port.body);
    assert!(said.contains("no ports to reach"), "{said}");

    // Stopped, it does not answer; started, it does again.
    server.act("dock", "stop").await;
    let stopped = server
        .host_until("dock", |host| host["reachable"] == false)
        .await;
    assert_eq!(stopped["power"]["state"], "stopped");
    server.act("dock", "start").await;
    server
        .host_until("dock", |host| host["reachable"] == true)
        .await;

    // Removed with its machine.
    let acted = server
        .call("DELETE", &format!("/v1/resources/{machine}"), None)
        .await;
    server.job_done(&acted).await;
    assert_eq!(
        change_of(&mut events, "resource", "removed").await["resource"],
        machine.as_str()
    );
    server.act("dock", "remove").await;
    assert_eq!(
        change_of(&mut events, "host", "removed").await["host"],
        "dock"
    );
    let hosts = server.call("GET", "/v1/hosts", None).await;
    assert_eq!(hosts.as_array().unwrap().len(), 1, "{hosts}");

    // Ended, it leaves nothing behind.
    let state = server.state.clone();
    assert!(state.exists());
    terminate(&mut server.child);
    assert!(gone(&state), "{} is removed", state.display());
}
