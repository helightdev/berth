use super::api::{self, OutputQuery, piece};
use super::{Remote, Serve};
use berth_api::client::{self, Client, Frame};
use berth_api::{Output, ops::Role, unb64};
use berth_core::{
    link::LinkSpec,
    paths::Paths,
    store::{self, Job, Store, kind},
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc, time::Duration};

struct Served {
    dir: tempfile::TempDir,
    state: Arc<Serve>,
    socket: PathBuf,
}

impl Served {
    async fn client(&self) -> Client {
        Client::unix(&self.socket).await.unwrap()
    }
}

fn state_in(dir: &std::path::Path, token: Option<&str>) -> Arc<Serve> {
    let paths = Paths {
        config_dir: dir.join("config"),
        data_dir: dir.join("data"),
    };
    let store = Store::open(&store::default_file(&paths)).unwrap();
    Serve::new(paths, store, token.map(str::to_owned))
}

async fn served() -> Served {
    served_with(Default::default).await
}

/// With provisioners that take the times and the stand-in for a new machine
/// that `provision` gives.
async fn served_with(provision: fn() -> berth_core::provision::Options) -> Served {
    let dir = tempfile::tempdir().unwrap();
    let mut state = state_in(dir.path(), None);
    Arc::get_mut(&mut state).unwrap().provision = provision;
    let socket = dir.path().join("serve.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let router = api::router(state.clone(), false);
    tokio::spawn(async move { axum::serve(listener, router).await });
    Served { dir, state, socket }
}

async fn text<S, E>(stream: &mut S) -> Value
where
    S: StreamExt<Item = Result<Frame, E>> + Unpin,
    E: std::fmt::Debug,
{
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(10), stream.next()).await;
        match frame
            .expect("a frame in time")
            .expect("the stream is open")
            .unwrap()
        {
            Frame::Text(text) => return serde_json::from_str(text.as_str()).unwrap(),
            Frame::Ping(_) | Frame::Pong(_) => {}
            other => panic!("unexpected frame {other:?}"),
        }
    }
}

#[test]
fn output_is_read_by_bytes_and_by_lines() {
    let data = b"one\ntwo\nthree\nhal";
    let all = piece(data, &OutputQuery::default(), false);
    assert_eq!(unb64(&all.data_b64).unwrap(), data);
    // A last line without its end is not counted yet, and following is by offset.
    assert_eq!(
        (all.offset, all.next_offset, all.next_line, all.ended),
        (0, 17, 3, false)
    );
    let rest = piece(
        data,
        &OutputQuery {
            since: Some(17),
            ..Default::default()
        },
        true,
    );
    assert_eq!(
        (rest.data_b64.as_str(), rest.next_offset, rest.ended),
        ("", 17, true)
    );
    let part = piece(
        data,
        &OutputQuery {
            since: Some(4),
            limit: Some(5),
            ..Default::default()
        },
        true,
    );
    assert_eq!(unb64(&part.data_b64).unwrap(), b"two\nt");
    assert_eq!(
        (part.next_offset, part.next_line, part.ended),
        (9, 2, false)
    );

    let lines = piece(
        data,
        &OutputQuery {
            line: Some(1),
            count: Some(1),
            ..Default::default()
        },
        false,
    );
    assert_eq!(lines.lines.as_deref(), Some(&["two".to_owned()][..]));
    assert_eq!(
        (lines.offset, lines.next_offset, lines.next_line),
        (4, 8, 2)
    );
    let tail = piece(
        data,
        &OutputQuery {
            tail: Some(2),
            ..Default::default()
        },
        false,
    );
    assert_eq!(tail.lines.unwrap(), ["two", "three"]);
    assert_eq!(tail.next_line, 3);
    let beyond = piece(
        data,
        &OutputQuery {
            line: Some(9),
            ..Default::default()
        },
        false,
    );
    assert_eq!((beyond.lines.unwrap().len(), beyond.next_line), (0, 3));
    let many = piece(
        data,
        &OutputQuery {
            tail: Some(99),
            ..Default::default()
        },
        false,
    );
    assert_eq!(many.lines.unwrap().len(), 3);
    assert!(piece(b"", &OutputQuery::default(), true).ended);
}

async fn put(socket: &std::path::Path, when: &[(&str, &str)], body: &str) -> client::Response {
    let mut client = Client::unix(socket).await.unwrap();
    let mut headers = vec![("content-type", "application/json")];
    headers.extend_from_slice(when);
    client
        .send("PUT", "/v1/kv/ui/layout", &headers, body.to_owned().into())
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_key_value_store_over_the_api() {
    let served = served().await;
    let socket = served.socket.clone();
    let mut client = served.client().await;
    let mut changes = client::websocket_unix(&socket, "/v1/kv/ui").await.unwrap();
    // Give the subscription a moment to exist before the first write.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let first = put(&socket, &[("if-none-match", "*")], r#"{"columns":2}"#).await;
    assert_eq!(
        (first.status, first.headers["etag"].to_str().unwrap()),
        (200, "\"1\"")
    );
    assert_eq!(
        put(&socket, &[("if-none-match", "*")], "1").await.status,
        412
    );
    // Two writers that both read revision 1: the second gets a 412.
    assert_eq!(
        put(&socket, &[("if-match", "\"1\"")], r#"{"columns":3}"#)
            .await
            .status,
        200
    );
    let loser = put(&socket, &[("if-match", "1")], r#"{"columns":4}"#).await;
    assert_eq!(loser.status, 412);
    assert_eq!(loser.error().code(), "precondition_failed");
    assert_eq!(put(&socket, &[("if-match", "x")], "1").await.status, 400);

    let entry: Value = client.get("/v1/kv/ui/layout").await.unwrap();
    assert_eq!(
        (&entry["value"], &entry["revision"]),
        (&json!({"columns": 3}), &json!(2))
    );
    let list: Value = client.get("/v1/kv/ui").await.unwrap();
    assert_eq!(list["entries"].as_array().unwrap().len(), 1);
    assert_eq!(
        text(&mut changes).await,
        json!({"key": "layout", "value": {"columns": 2}, "revision": 1})
    );
    assert_eq!(text(&mut changes).await["revision"], 2);

    let removed: Value = client
        .call("DELETE", "/v1/kv/ui/layout", None)
        .await
        .unwrap();
    assert_eq!(removed["ok"], true);
    assert_eq!(
        text(&mut changes).await,
        json!({"key": "layout", "removed": true})
    );
    let missing = client.get::<Value>("/v1/kv/ui/layout").await.unwrap_err();
    assert_eq!((missing.status(), missing.code()), (404, "not_found"));
    let large = format!("\"{}\"", "x".repeat(70_000));
    let refused = client
        .send(
            "PUT",
            "/v1/kv/ui/big",
            &[("content-type", "application/json")],
            large.into(),
        )
        .await
        .unwrap();
    assert_eq!(refused.status, 413);
}

fn kv_event(key: &str, value: Value) -> super::Event {
    super::Event::Kv {
        namespace: "ui".into(),
        key: key.into(),
        value: Some(value),
        revision: 1,
    }
}

/// Wait for a frame that says events were dropped, past the ones before it.
async fn dropped<S, E>(stream: &mut S) -> u64
where
    S: StreamExt<Item = Result<Frame, E>> + Unpin,
    E: std::fmt::Debug,
{
    loop {
        let frame = text(stream).await;
        if let Some(missed) = frame["dropped"]["events"].as_u64() {
            return missed;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_follower_that_falls_behind_is_told_what_it_missed() {
    let served = served().await;
    // The events stream listens from the handshake on, before it is told
    // what to follow, so all of these are waiting when it starts.
    let mut events = client::websocket_unix(&served.socket, "/v1/events")
        .await
        .unwrap();
    for n in 0..1100 {
        let _ = served.state.events.send(kv_event("layout", json!(n)));
    }
    let follow = json!({"kv": ["ui"]}).to_string();
    events.send(Frame::Text(follow.into())).await.unwrap();
    assert_eq!(text(&mut events).await, json!({"dropped": {"events": 76}}));
    assert_eq!(text(&mut events).await["value"], 76);

    // A follower of a namespace that reads nothing while the server's buffers
    // fill misses the rest, and goes on after the notice.
    let mut changes = client::websocket_unix(&served.socket, "/v1/kv/ui")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let large = "x".repeat(4096);
    for _ in 0..5000 {
        let _ = served.state.events.send(kv_event("layout", json!(large)));
    }
    let _ = served.state.events.send(kv_event("last", json!(true)));
    assert!(dropped(&mut changes).await > 0);
    loop {
        let frame = text(&mut changes).await;
        if frame["key"] == "last" {
            break;
        }
    }
}

fn message(id: u64, topic: &str, keep: &str) -> berth_api::Message {
    serde_json::from_value(json!({
        "id": id, "topic": topic, "body": {"n": id}, "keep": keep,
        "from": {"level": "agent", "uid": 1001}, "time": berth_api::now(),
    }))
    .unwrap()
}

#[tokio::test]
async fn a_browser_and_a_client_of_berth_0_5_are_told_what_to_do() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path(), Some("s3cret-token"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = api::router(state, true);
    tokio::spawn(async move { axum::serve(listener, router).await });
    let connect = || async { tokio::net::TcpStream::connect(address).await.unwrap() };

    // The address the recipes of 0.5 printed answers without a token, and says only what to run.
    let mut client = Client::over(connect().await).await.unwrap();
    let front = client
        .send("GET", "/", &[], Default::default())
        .await
        .unwrap();
    assert_eq!(front.status, 200);
    let text = String::from_utf8(front.body.to_vec()).unwrap();
    assert_eq!(text, api::FRONT);
    assert!(text.contains("berth login https://THIS-SERVER") && !text.contains('<'));
    assert!(!text.contains(berth_api::VERSION));
    let refused = client.get::<Value>("/v1/hosts").await.unwrap_err();
    assert_eq!(refused.status(), 401);

    // A call only berth 0.5 makes says that the client is older.
    let socket = client::websocket_at(connect().await, "berth", "/v1/portal", "s3cret-token")
        .await
        .unwrap();
    let portal = berth_api::portal::Portal::over(socket);
    let old = portal
        .call("tasks.list", Some("studio"), Value::Null, None)
        .await
        .unwrap_err();
    assert_eq!(old.status(), 404);
    assert!(old.to_string().contains("a call of berth 0.5"), "{old}");
}

#[tokio::test(flavor = "multi_thread")]
async fn hosts_jobs_and_the_inbox_over_the_api() {
    let served = served().await;
    let mut client = served.client().await;
    let version: Value = client.get("/v1/version").await.unwrap();
    assert_eq!(version["mode"], "serve");
    assert_eq!(client.get::<Value>("/v1/hosts").await.unwrap(), json!([]));
    assert_eq!(
        client
            .get::<Value>("/v1/hosts/nope")
            .await
            .unwrap_err()
            .status(),
        404
    );
    assert_eq!(
        client
            .get::<Value>("/v1/hosts/nope/controller/version")
            .await
            .unwrap_err()
            .status(),
        404
    );
    assert_eq!(
        client
            .post::<Value>("/v1/hosts/nope/actions/apply", &json!({}))
            .await
            .unwrap_err()
            .status(),
        404
    );

    // A host whose link command fails is listed, not reachable, with the reason.
    let broken = LinkSpec {
        program: "sh".into(),
        args: vec![
            "-c".into(),
            "echo 'bash: berthd: command not found' >&2; exit 127".into(),
        ],
        first_line: None,
        ..LinkSpec::default()
    };
    served.state.add(Remote::new("broken", "192.0.2.1", broken));
    let hosts: Value = client.get("/v1/hosts").await.unwrap();
    assert_eq!(
        (&hosts[0]["name"], &hosts[0]["reachable"]),
        (&json!("broken"), &json!(false))
    );
    assert!(
        hosts[0]["error"].as_str().unwrap().contains("no berthd"),
        "{hosts}"
    );
    let through = client
        .get::<Value>("/v1/hosts/broken/controller/actions")
        .await
        .unwrap_err();
    assert_eq!((through.status(), through.code()), (502, "unreachable"));
    // Secrets are refused before the host is even tried.
    let secrets = client
        .post::<Value>("/v1/hosts/broken/controller/actions/secrets", &json!({}))
        .await
        .unwrap_err();
    assert_eq!((secrets.status(), secrets.code()), (403, "forbidden"));

    // A job that ran: its state, its output both ways, and the stream of it.
    std::fs::create_dir_all(&served.state.jobs_dir).unwrap();
    let log = served.state.jobs_dir.join("j1.log");
    std::fs::write(&log, "PLAY [all]\nok: [devbox_target]\n").unwrap();
    let job = Job {
        id: "j1".into(),
        host: "broken".into(),
        what: "apply".into(),
        started: berth_api::now(),
        ended: None,
        exit_code: None,
        output_path: log.to_string_lossy().into_owned(),
        subject: "host:broken".into(),
        action: "apply".into(),
        token: None,
        stopped_by: None,
        arguments: vec![],
    };
    served.state.store.add_job(&job).unwrap();
    let jobs: Value = client.get("/v1/jobs").await.unwrap();
    assert_eq!(
        (&jobs[0]["id"], &jobs[0]["ended"]),
        (&json!("j1"), &Value::Null)
    );
    assert_eq!(
        (&jobs[0]["subject"], &jobs[0]["action"]),
        (&json!("host:broken"), &json!("apply"))
    );
    let output: Output = client.get("/v1/jobs/j1/output?tail=1").await.unwrap();
    assert_eq!(output.lines.unwrap(), ["ok: [devbox_target]"]);
    assert_eq!((output.next_line, output.ended), (2, false));
    let mut stream = client::websocket_unix(&served.socket, "/v1/jobs/j1?since=11")
        .await
        .unwrap();
    let Some(Ok(Frame::Binary(bytes))) = stream.next().await else {
        panic!("no output")
    };
    assert_eq!(&bytes[..], b"ok: [devbox_target]\n");
    // What it writes later comes too, and then how it ended.
    let mut file = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
    std::io::Write::write_all(&mut file, b"done\n").unwrap();
    let Some(Ok(Frame::Binary(bytes))) = stream.next().await else {
        panic!("no more output")
    };
    assert_eq!(&bytes[..], b"done\n");
    served.state.store.end_job("j1", Some(2)).unwrap();
    assert_eq!(text(&mut stream).await, json!({"exited": {"exit_code": 2}}));
    let ended: Value = client.get("/v1/jobs/j1").await.unwrap();
    assert_eq!(ended["exit_code"], 2);
    assert_eq!(
        client
            .get::<Value>("/v1/jobs/j9")
            .await
            .unwrap_err()
            .status(),
        404
    );

    // A job whose output stayed on the machine that ran it, as after a restore.
    let elsewhere = Job {
        id: "j2".into(),
        ended: Some(berth_api::now()),
        exit_code: Some(0),
        output_path: "/nonexistent/berth/jobs/j2.log".into(),
        ..job.clone()
    };
    served.state.store.add_job(&elsewhere).unwrap();
    let missing = client
        .get::<Output>("/v1/jobs/j2/output")
        .await
        .unwrap_err();
    assert_eq!(missing.status(), 404);
    assert!(
        missing
            .to_string()
            .contains("kept on the machine that ran it"),
        "{missing}"
    );
    // A row of a shared store can name any file of the server: it is not read.
    let secret = served.dir.path().join("secret.log");
    std::fs::write(&secret, "private").unwrap();
    std::fs::write(secret.with_extension("json"), r#"{"private": true}"#).unwrap();
    let planted = Job {
        id: "j3".into(),
        output_path: secret.to_string_lossy().into_owned(),
        ..elsewhere.clone()
    };
    served.state.store.add_job(&planted).unwrap();
    let refused = client
        .get::<Output>("/v1/jobs/j3/output")
        .await
        .unwrap_err();
    assert_eq!(refused.status(), 404);
    let info: Value = client.get("/v1/jobs/j3").await.unwrap();
    assert!(
        info["report"].is_null() && !info.to_string().contains("private"),
        "{info}"
    );
    let info: Value = client.get("/v1/jobs/j2").await.unwrap();
    assert_eq!(info["exit_code"], 0);
    let mut stream = client::websocket_unix(&served.socket, "/v1/jobs/j2")
        .await
        .unwrap();
    let Some(Ok(Frame::Binary(bytes))) = stream.next().await else {
        panic!("no note")
    };
    assert!(String::from_utf8_lossy(&bytes).contains("is not on this one"));
    assert_eq!(text(&mut stream).await, json!({"exited": {"exit_code": 0}}));

    // Messages as the taking-in stores them.
    let mut live = client::websocket_unix(&served.socket, "/v1/inbox?topic=build/")
        .await
        .unwrap();
    let mut events = client::websocket_unix(&served.socket, "/v1/events")
        .await
        .unwrap();
    let follow = r#"{"topics":["build/"],"hosts":["server"],"kv":["ui"]}"#;
    events.send(Frame::Text(follow.into())).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    served.state.accept("server", message(1, "other/x", "log"));
    served
        .state
        .accept("server", message(2, "build/progress", "none"));
    served
        .state
        .accept("server", message(3, "build/result", "log"));
    served
        .state
        .accept("laptop", message(1, "build/result", "log"));
    // The inbox stream has what was stored; a message only handed on is not.
    assert_eq!(text(&mut live).await["id"], 3);
    assert_eq!(text(&mut live).await["host"], "laptop");
    // The events stream has what was asked for, kept or not, with its source.
    let first = text(&mut events).await;
    assert_eq!(
        (&first["source"], &first["host"], &first["message"]["id"]),
        (&json!("channel"), &json!("server"), &json!(2))
    );
    assert_eq!(text(&mut events).await["message"]["id"], 3);
    client
        .call::<Value>("PUT", "/v1/kv/ui/k", Some(&json!(5)))
        .await
        .unwrap();
    assert_eq!(
        text(&mut events).await,
        json!({"source": "kv", "namespace": "ui", "key": "k", "value": 5, "revision": 1})
    );

    let inbox: Value = client.get("/v1/inbox?host=server").await.unwrap();
    let ids: Vec<_> = inbox["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_u64().unwrap())
        .collect();
    assert_eq!(ids, [3, 1]);
    assert_eq!(inbox["messages"][0]["from"]["level"], "agent");
    let page: Value = client.get("/v1/inbox?limit=1").await.unwrap();
    assert!(page["next"].is_i64());
    client
        .post::<Value>("/v1/inbox/server/3/done", &json!({}))
        .await
        .unwrap();
    let open: Value = client
        .get("/v1/inbox?done=false&topic=build/")
        .await
        .unwrap();
    assert_eq!(open["messages"].as_array().unwrap().len(), 1);
    client
        .call::<Value>("DELETE", "/v1/inbox/server/3/done", None)
        .await
        .unwrap();
    let open: Value = client
        .get("/v1/inbox?done=false&topic=build/")
        .await
        .unwrap();
    assert_eq!(open["messages"].as_array().unwrap().len(), 2);
    assert_eq!(
        client
            .post::<Value>("/v1/inbox/server/99/done", &json!({}))
            .await
            .unwrap_err()
            .status(),
        404
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn over_tcp_the_token_is_asked_for() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path(), Some("s3cret-token"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = api::router(state, true);
    tokio::spawn(async move { axum::serve(listener, router).await });
    let connect = || async { tokio::net::TcpStream::connect(address).await.unwrap() };

    let mut client = Client::over(connect().await).await.unwrap();
    let refused = client.get::<Value>("/v1/hosts").await.unwrap_err();
    assert_eq!((refused.status(), refused.code()), (401, "unauthorized"));
    let wrong = [("authorization", "Bearer nope")];
    assert_eq!(
        client
            .send("GET", "/v1/hosts", &wrong, Default::default())
            .await
            .unwrap()
            .status,
        401
    );
    let right = [("authorization", "Bearer s3cret-token")];
    assert_eq!(
        client
            .send("GET", "/v1/hosts", &right, Default::default())
            .await
            .unwrap()
            .status,
        200
    );

    // A browser cannot set a header on a WebSocket: the token goes as its protocol.
    assert!(
        client::websocket(connect().await, "/v1/inbox", "")
            .await
            .is_err()
    );
    assert!(
        client::websocket(connect().await, "/v1/inbox", "wrong")
            .await
            .is_err()
    );
    assert!(
        client::websocket(connect().await, "/v1/inbox", "s3cret-token")
            .await
            .is_ok()
    );
    // Only this machine may be listened on.
    let paths = Paths {
        config_dir: dir.path().join("config"),
        data_dir: dir.path().join("data"),
    };
    assert!(super::command(&paths, &["--listen", "0.0.0.0:1"]).is_err());
}

// ── Against a real berthd ───────────────────────────────────────────────────

struct Berthd {
    child: std::process::Child,
    dir: tempfile::TempDir,
    program: PathBuf,
}

impl Drop for Berthd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Berthd {
    /// A berthd in single-user mode, or nothing when it is not built or does
    /// not answer yet.
    async fn start() -> Option<Berthd> {
        Berthd::with_actions(json!([])).await
    }

    /// The same, with these actions in its `etc/actions.json`.
    async fn with_actions(actions: Value) -> Option<Berthd> {
        let program = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/berthd");
        if !program.is_file() {
            eprintln!("skipped: {} is not built", program.display());
            return None;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("etc")).unwrap();
        std::fs::write(dir.path().join("etc/actions.json"), actions.to_string()).unwrap();
        let child = std::process::Command::new(&program)
            .arg("--single")
            .arg(dir.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        let host = Berthd {
            child,
            dir,
            program,
        };
        for _ in 0..50 {
            if let Ok(mut client) = Client::unix(&host.dir.path().join("control.sock")).await
                && client.get::<Value>("/v1/version").await.is_ok()
            {
                return Some(host);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        eprintln!("skipped: berthd --single does not answer /v1/version");
        None
    }

    fn link(&self) -> LinkSpec {
        let dir = self.dir.path().to_string_lossy().into_owned();
        LinkSpec {
            program: self.program.to_string_lossy().into_owned(),
            args: vec!["connect".into(), "--dir".into(), dir],
            first_line: None,
            ..LinkSpec::default()
        }
    }

    async fn publish(&self, topic: &str, keep: &str, n: u64) {
        let mut client = Client::unix(&self.dir.path().join("control.sock"))
            .await
            .unwrap();
        let body = json!({"body": {"n": n}, "keep": keep});
        let _: Value = client
            .post(&format!("/v1/channel/{topic}"), &body)
            .await
            .unwrap();
    }
}

/// The inbox once it has `wanted` messages and has stopped growing.
async fn inbox_of(client: &mut Client, wanted: usize) -> Vec<Value> {
    for _ in 0..100 {
        let inbox: Value = client.get("/v1/inbox?host=box&topic=t/").await.unwrap();
        if inbox["messages"].as_array().unwrap().len() >= wanted {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let inbox: Value = client.get("/v1/inbox?host=box&topic=t/").await.unwrap();
            return inbox["messages"].as_array().unwrap().clone();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the inbox never had {wanted} messages");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hosts_api_is_passed_through() {
    let Some(host) = Berthd::start().await else {
        return;
    };
    let served = served().await;
    served
        .state
        .add(Remote::new("box", "198.51.100.1", host.link()));
    let mut client = served.client().await;

    let hosts: Value = client.get("/v1/hosts").await.unwrap();
    assert_eq!(hosts[0]["reachable"], true, "{hosts}");
    assert_eq!(hosts[0]["version"], berth_api::VERSION);
    let version: Value = client
        .get("/v1/hosts/box/controller/version")
        .await
        .unwrap();
    assert_eq!(version["api"], berth_api::API);
    // A request with a body and a query, and the host's own error passed back.
    let start = json!({"kind": "process", "command": "echo through; exit 3"});
    let ran: Value = client
        .post("/v1/hosts/box/controller/sessions?wait=20", &start)
        .await
        .unwrap();
    assert_eq!(ran["exit_code"], 3, "{ran}");
    assert_eq!(
        unb64(ran["stdout_b64"].as_str().unwrap()).unwrap(),
        b"through\n"
    );
    let missing = client
        .get::<Value>("/v1/hosts/box/controller/sessions/snope")
        .await
        .unwrap_err();
    assert_eq!((missing.status(), missing.code()), (404, "not_found"));
    let secrets = client
        .post::<Value>("/v1/hosts/box/controller/actions/secrets", &json!({}))
        .await
        .unwrap_err();
    assert_eq!(secrets.status(), 403);

    // A stream, both ways: what is published through it comes back on it.
    let mut channel =
        client::websocket_unix(&served.socket, "/v1/hosts/box/controller/channel?topic=t/")
            .await
            .unwrap();
    let publish = r#"{"topic":"t/x","body":{"hello":1},"keep":"none"}"#;
    channel.send(Frame::Text(publish.into())).await.unwrap();
    let message = text(&mut channel).await;
    assert_eq!(
        (&message["topic"], &message["body"]),
        (&json!("t/x"), &json!({"hello": 1}))
    );
    // A terminal session's bytes come through as binary frames.
    let start = json!({"kind": "terminal", "command": "printf ready; read line; echo got-$line"});
    let session: Value = client
        .post("/v1/hosts/box/controller/sessions", &start)
        .await
        .unwrap();
    let path = format!(
        "/v1/hosts/box/controller/sessions/{}?since=0",
        session["id"].as_str().unwrap()
    );
    let mut terminal = client::websocket_unix(&served.socket, &path).await.unwrap();
    terminal
        .send(Frame::Binary(b"42\r".to_vec().into()))
        .await
        .unwrap();
    let mut seen = String::new();
    while !seen.contains("got-42") {
        let frame = tokio::time::timeout(Duration::from_secs(10), terminal.next()).await;
        match frame.expect("output in time") {
            Some(Ok(Frame::Binary(bytes))) => seen.push_str(&String::from_utf8_lossy(&bytes)),
            Some(Ok(_)) => {}
            other => panic!("the stream ended after {seen:?}: {other:?}"),
        }
    }
}

/// A job of the server once it has ended.
async fn job_done(client: &mut Client, id: &str) -> Value {
    for _ in 0..200 {
        let job: Value = client.get(&format!("/v1/jobs/{id}")).await.unwrap();
        if !job["ended"].is_null() {
            return job;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("job {id} did not end");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hosts_actions_are_its_controllers_and_berths_own() {
    let actions = json!([
        {"name": "status", "label": "Status", "group": "System", "user": "agent", "run": "echo up"},
        {"name": "demo:status", "label": "Demo", "extends": ["status"], "user": "agent", "run": "echo demo; exit 4"},
        {"name": "greet", "label": "Greet", "user": "agent", "run": "echo \"hello $BERTH_ARG_WHO\"",
         "arguments": [{"name": "who", "required": true}]},
        {"name": "ask", "label": "Ask", "user": "agent", "terminal": true, "run": "read c; echo got $c"},
        {"name": "secrets", "label": "Send the secrets", "user": "agent", "input": "secrets", "run": "cat >/dev/null"},
        {"name": "elevated", "label": "As root", "user": "root", "run": "true"},
    ]);
    let Some(host) = Berthd::with_actions(actions).await else {
        return;
    };
    let served = served().await;
    served
        .state
        .add(Remote::new("box", "198.51.100.1", host.link()));
    let mut client = served.client().await;

    let actions: Vec<berth_api::Action> = client.get("/v1/hosts/box/actions").await.unwrap();
    let names: Vec<&str> = actions.iter().map(|action| action.name.as_str()).collect();
    // The host's actions, a step under the action it extends, then berth's own; the
    // secrets action is berth's, which sends the server's secrets.
    assert_eq!(
        names,
        [
            "status",
            "greet",
            "ask",
            "elevated",
            "apply",
            "install-controller",
            "seed",
            "secrets",
            "trust-key",
            "host-key",
            "rename",
            "installer",
            "start",
            "stop",
            "duplicate",
            "remove"
        ]
    );
    let status = &actions[0];
    assert_eq!(
        (status.runs, status.role),
        (berth_api::Runs::Host, Role::Operate)
    );
    assert_eq!(status.steps[0].label, "Demo");
    let secrets = &actions[7];
    assert_eq!(
        (secrets.label.as_str(), secrets.role),
        ("Send the secrets", Role::Admin)
    );
    assert!(secrets.available && actions[4].role == Role::Admin);

    // An action that needs no terminal is a session there and a job here, which
    // copies its output and ends with its exit code.
    let run = |body: Value| {
        let socket = served.socket.clone();
        async move {
            let mut client = Client::unix(&socket).await.unwrap();
            client
                .post::<Value>("/v1/hosts/box/actions/status", &body)
                .await
        }
    };
    let acted = run(json!({"token": "t-1"})).await.unwrap();
    let id = acted["job"]["id"].as_str().unwrap().to_owned();
    assert!(acted["session"]["id"].is_string(), "{acted}");
    assert_eq!(
        (&acted["job"]["subject"], &acted["job"]["action"]),
        (&json!("host:box"), &json!("status"))
    );
    let job = job_done(&mut client, &id).await;
    assert_eq!(job["exit_code"], 4, "the step's exit code: {job}");
    let output: Output = client
        .get(&format!("/v1/jobs/{id}/output?tail=10"))
        .await
        .unwrap();
    let lines = output.lines.unwrap();
    assert!(
        lines.contains(&"up".to_owned()) && lines.contains(&"demo".to_owned()),
        "{lines:?}"
    );
    // Asked again with its token, it is the same job, and nothing runs again.
    let sessions = |value: Value| berth_tui::host::list::<Value>(value, "sessions").len();
    let before = sessions(
        client
            .get("/v1/hosts/box/controller/sessions")
            .await
            .unwrap(),
    );
    let again = run(json!({"token": "t-1"})).await.unwrap();
    assert_eq!(again["job"]["id"], json!(id));
    assert_eq!(
        sessions(
            client
                .get("/v1/hosts/box/controller/sessions")
                .await
                .unwrap()
        ),
        before
    );

    // Callers at once with one new token start one session between them,
    // and each is told of the one job.
    let racers = (0..4).map(|_| run(json!({"token": "t-race"})));
    let answers = futures_util::future::join_all(racers).await;
    let ids: std::collections::BTreeSet<String> = answers
        .into_iter()
        .map(|answer| answer.unwrap()["job"]["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids.len(), 1, "{ids:?}");
    job_done(&mut client, ids.first().unwrap()).await;
    assert_eq!(
        sessions(
            client
                .get("/v1/hosts/box/controller/sessions")
                .await
                .unwrap()
        ),
        before + 1
    );

    // An action the host will not start ends its job as failed, with the reason,
    // and asked again with the token that is the answer.
    let path = "/v1/hosts/box/actions/elevated";
    let refused = client
        .post::<Value>(path, &json!({"token": "t-root"}))
        .await
        .unwrap_err();
    let again: Value = client
        .post(path, &json!({"token": "t-root"}))
        .await
        .unwrap();
    assert!(again["session"].is_null(), "{again}");
    let id = again["job"]["id"].as_str().unwrap();
    assert_eq!(job_done(&mut client, id).await["exit_code"], 1);
    let output: Output = client
        .get(&format!("/v1/jobs/{id}/output?tail=1"))
        .await
        .unwrap();
    assert!(
        output.lines.unwrap()[0].contains("The host did not start elevated"),
        "{refused}"
    );

    // Its arguments reach the host; a needed one is asked for first.
    let path = "/v1/hosts/box/actions/greet";
    let missing = client.post::<Value>(path, &json!({})).await.unwrap_err();
    assert_eq!(missing.status(), 400);
    let acted: Value = client
        .post(path, &json!({"arguments": {"who": "ann"}}))
        .await
        .unwrap();
    let id = acted["job"]["id"].as_str().unwrap();
    assert_eq!(job_done(&mut client, id).await["exit_code"], 0);
    let output: Output = client
        .get(&format!("/v1/jobs/{id}/output?tail=1"))
        .await
        .unwrap();
    assert_eq!(output.lines.unwrap(), ["hello ann"]);

    // One that needs a terminal is the session to attach to, without a job.
    let acted: Value = client
        .post(
            "/v1/hosts/box/actions/ask",
            &json!({"rows": 10, "cols": 40}),
        )
        .await
        .unwrap();
    assert!(
        acted["job"].is_null() && acted["session"]["kind"] == "terminal",
        "{acted}"
    );

    let nothing = client
        .post::<Value>("/v1/hosts/box/actions/nothing", &json!({}))
        .await
        .unwrap_err();
    assert_eq!(nothing.status(), 404);
    assert!(nothing.to_string().contains("status, greet"), "{nothing}");
    // The jobs list says what each was done to.
    let jobs: Value = client.get("/v1/jobs").await.unwrap();
    assert!(
        jobs.as_array()
            .unwrap()
            .iter()
            .all(|job| job["subject"] == "host:box")
    );
}

/// A listener on the loopback that sends back what it gets, and ends its
/// side once the caller has ended theirs.
async fn echo() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((connection, _)) = listener.accept().await {
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let (mut reading, mut writing) = connection.into_split();
                let _ = tokio::io::copy(&mut reading, &mut writing).await;
                let _ = writing.shutdown().await;
            });
        }
    });
    port
}

/// Write `data` and end, while what comes back is read: what came back.
async fn through<T>(open: T, data: &[u8]) -> Vec<u8>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (mut reading, mut writing) = tokio::io::split(open);
    let sent = data.to_vec();
    let writer = tokio::spawn(async move {
        writing.write_all(&sent).await.unwrap();
        writing.shutdown().await.unwrap();
        writing
    });
    let mut back = vec![];
    tokio::time::timeout(Duration::from_secs(60), reading.read_to_end(&mut back))
        .await
        .expect("all of it back in time")
        .unwrap();
    drop(writer.await.unwrap());
    back
}

#[tokio::test(flavor = "multi_thread")]
async fn a_port_of_a_host_is_reached_through_the_server() {
    use berth_api::{portal::Portal, tunnel::Tunnel};
    let Some(host) = Berthd::start().await else {
        return;
    };
    let served = served().await;
    served
        .state
        .add(Remote::new("studio", "192.0.2.10", host.link()));
    let port = echo().await;
    let data: Vec<u8> = (0..3_000_000u32).map(|n| (n % 251) as u8).collect();

    // As the host's own stream, passed through.
    let path = format!("/v1/hosts/studio/controller/ports/{port}");
    let open = client::websocket_unix(&served.socket, &path).await.unwrap();
    assert_eq!(through(Tunnel::new(open), b"hello").await, b"hello");
    let open = client::websocket_unix(&served.socket, &path).await.unwrap();
    assert!(through(Tunnel::new(open), &data).await == data);

    // On the portal, with the host named.
    let portal = Portal::over(
        client::websocket_unix(&served.socket, "/v1/portal")
            .await
            .unwrap(),
    );
    let open = portal.connect_port(Some("studio"), port).await.unwrap();
    assert!(through(open, &data).await == data);

    // A port nothing listens on is the host's error, either way.
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let path = format!("/v1/hosts/studio/controller/ports/{closed}");
    let refused = client::websocket_unix(&served.socket, &path)
        .await
        .err()
        .unwrap();
    assert_eq!((refused.status(), refused.code()), (503, "not_served"));
    let refused = portal
        .connect_port(Some("studio"), closed)
        .await
        .err()
        .unwrap();
    assert_eq!((refused.status(), refused.code()), (503, "not_served"));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_portal_reaches_serve_and_its_hosts() {
    use berth_api::portal::{Piece, Portal};
    let Some(host) = Berthd::start().await else {
        return;
    };
    let served = served().await;
    served
        .state
        .add(Remote::new("box", "198.51.100.1", host.link()));
    let socket = client::websocket_unix(&served.socket, "/v1/portal")
        .await
        .unwrap();
    let portal = Portal::over(socket);

    // Our own operations.
    let hosts = portal.call("hosts.list", None, Value::Null, None).await;
    assert_eq!(hosts.unwrap()[0]["name"], "box");
    let args = json!({"namespace": "ui", "key": "layout", "if_none_match": "*"});
    let written = portal
        .call("kv.set", None, args.clone(), Some(json!({"wide": true})))
        .await
        .unwrap();
    assert_eq!(written["revision"], 1);
    // The condition is the header it is over HTTP: the key exists now.
    let again = portal.call("kv.set", None, args, Some(json!(1))).await;
    assert_eq!(again.unwrap_err().status(), 412);
    let mut changes = portal
        .open("kv.follow", None, json!({"namespace": "ui"}))
        .await
        .unwrap();
    let args = json!({"namespace": "ui", "key": "layout"});
    portal.call("kv.delete", None, args, None).await.unwrap();
    let mut removed = false;
    while !removed {
        let change = tokio::time::timeout(Duration::from_secs(10), changes.recv()).await;
        match change.unwrap() {
            Some(Ok(Piece::Text(change))) => removed = change["value"].is_null(),
            other => panic!("no change: {other:?}"),
        }
    }

    // A host's operations, by its name; and its refusals.
    let version = portal.call("version", Some("box"), Value::Null, None).await;
    assert_eq!(version.unwrap()["mode"], "single");
    let start = json!({"kind": "terminal", "command": "read line; echo got-$line"});
    let session = portal
        .call("sessions.start", Some("box"), Value::Null, Some(start))
        .await
        .unwrap();
    let mut terminal = portal
        .open("sessions.attach", Some("box"), json!({"id": session["id"]}))
        .await
        .unwrap();
    terminal.push(Piece::Binary(b"7\r".to_vec())).unwrap();
    let mut seen = String::new();
    while !seen.contains("got-7") {
        let frame = tokio::time::timeout(Duration::from_secs(10), terminal.recv()).await;
        match frame.expect("output in time") {
            Some(Ok(Piece::Binary(bytes))) => seen.push_str(&String::from_utf8_lossy(&bytes)),
            Some(Ok(_)) => {}
            other => panic!("the stream ended after {seen:?}: {other:?}"),
        }
    }
    // The controller's actions are its own, apart from the server's of the host.
    let listed = portal
        .call("actions.list", Some("box"), Value::Null, None)
        .await
        .unwrap();
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .all(|action| action.get("available").is_none()),
        "{listed}"
    );
    let secrets = portal
        .call(
            "actions.run",
            Some("box"),
            json!({"action": "secrets"}),
            Some(json!({})),
        )
        .await;
    assert_eq!(secrets.unwrap_err().status(), 403);
    let nobody = portal
        .call("version", Some("nobody"), Value::Null, None)
        .await;
    assert_eq!(nobody.unwrap_err().status(), 404);
    // A host's operation is not ours without its name.
    let ours = portal.call("sessions.list", None, Value::Null, None).await;
    assert_eq!(ours.unwrap_err().status(), 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn messages_are_taken_in_once_also_across_a_restart() {
    let Some(host) = Berthd::start().await else {
        return;
    };
    let served = served().await;
    served
        .state
        .add(Remote::new("box", "198.51.100.1", host.link()));
    let mut client = served.client().await;
    host.publish("t/log", "log", 1).await;
    host.publish("t/live", "none", 2).await;
    host.publish("t/state", "last", 3).await;
    let messages = inbox_of(&mut client, 2).await;
    let topics: Vec<_> = messages
        .iter()
        .map(|m| m["topic"].as_str().unwrap())
        .collect();
    assert_eq!(topics, ["t/state", "t/log"]);

    // A second berth serve on the same store, as after a restart, while the
    // host went on publishing: it takes up where the first one was.
    host.publish("t/log", "log", 4).await;
    host.publish("t/state", "last", 5).await;
    let again = state_in(served.dir.path(), None);
    let socket = served.dir.path().join("again.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let router = api::router(again.clone(), false);
    tokio::spawn(async move { axum::serve(listener, router).await });
    again.add(Remote::new("box", "198.51.100.1", host.link()));
    let mut client = Client::unix(&socket).await.unwrap();
    let messages = inbox_of(&mut client, 3).await;
    let seen: Vec<_> = messages
        .iter()
        .map(|m| {
            (
                m["topic"].as_str().unwrap(),
                m["body"]["n"].as_u64().unwrap(),
            )
        })
        .collect();
    // Each once; the state replaced the one before it.
    assert_eq!(seen, [("t/state", 5), ("t/log", 4), ("t/log", 1)]);
}

/// The committed API documents are what the program generates. Run with
/// BERTH_UPDATE_DOCS=1 to write them after a change to the API.
#[test]
fn the_committed_documents_are_current() {
    let docs = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/openapi");
    for (name, document) in [
        ("berth.openapi.json", super::docs::openapi()),
        ("berth.asyncapi.json", super::docs::asyncapi()),
        ("berth.operations.json", super::docs::operations()),
    ] {
        let text = serde_json::to_string_pretty(&document).unwrap() + "\n";
        let path = docs.join(name);
        if std::env::var_os("BERTH_UPDATE_DOCS").is_some() {
            std::fs::write(&path, &text).unwrap();
        }
        let committed = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            committed == text,
            "{name} is out of date: run BERTH_UPDATE_DOCS=1 cargo test -p berth the_committed_documents"
        );
    }
}

async fn send(client: &mut Client, method: &str, path: &str, body: Value) -> (u16, Value) {
    let body = match body {
        Value::Null => String::new(),
        body => body.to_string(),
    };
    let headers = [("content-type", "application/json")];
    let answer = client
        .send(method, path, &headers, body.into())
        .await
        .unwrap();
    let said = serde_json::from_slice(&answer.body).unwrap_or(Value::Null);
    (answer.status, said)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_server_is_set_up_through_the_api() {
    let served = served().await;
    let mut client = served.client().await;
    let client = &mut client;
    let profile = json!({"hostname": "studio", "builder": {"served": "x"}, "access": {"address": "192.0.2.7", "user": "ops"}});

    // A profile makes a host, and what it says is what comes back.
    assert_eq!(
        send(client, "GET", "/v1/hosts/studio/profile", Value::Null)
            .await
            .0,
        404
    );
    assert_eq!(
        send(client, "PUT", "/v1/hosts/studio/profile", profile)
            .await
            .0,
        200
    );
    let (_, kept) = send(client, "GET", "/v1/hosts/studio/profile", Value::Null).await;
    assert_eq!(kept["access"]["address"], "192.0.2.7");
    assert!(kept["builder"].get("served").is_none());
    let (_, hosts) = send(client, "GET", "/v1/hosts", Value::Null).await;
    assert_eq!(
        (&hosts[0]["name"], &hosts[0]["address"]),
        (&json!("studio"), &json!("192.0.2.7"))
    );
    // A profile without an address is a host too, one that cannot be reached yet.
    assert_eq!(
        send(
            client,
            "PUT",
            "/v1/hosts/draft/profile",
            json!({"builder": {}})
        )
        .await
        .0,
        200
    );
    let (_, draft) = send(client, "GET", "/v1/hosts/draft", Value::Null).await;
    assert_eq!(draft["reachable"], false);
    // A name is never a path, and a command to run is not taken from a caller.
    for name in ["..%2Fx", "a.json", ".hidden"] {
        let path = format!("/v1/hosts/{name}/profile");
        assert_eq!(
            send(client, "PUT", &path, json!({"builder": {}})).await.0,
            400,
            "{name}"
        );
    }
    // The server's own user, on its socket, chooses the command that reaches a host.
    let linked = json!({"builder": {}, "access": {"link": ["sh", "-c", "id"]}});
    assert_eq!(
        send(client, "PUT", "/v1/hosts/linked/profile", linked)
            .await
            .0,
        200
    );
    assert_eq!(
        send(client, "PUT", "/v1/hosts/studio/profile", json!([]))
            .await
            .0,
        400
    );

    // What a set-up screen starts from: the profile of a new host, and what
    // the plugins that come with berth install.
    let (status, found) = send(client, "GET", "/v1/setup", Value::Null).await;
    assert_eq!(status, 200, "{found}");
    assert!(found["template"]["builder"].is_object());
    let features = found["features"].as_array().unwrap();
    assert!(features.iter().any(|feature| {
        feature["key"].as_str().is_some_and(|key| key.contains('/'))
            && feature["group"] == "Coding Agents"
    }));
    let (_, name) = send(
        client,
        "POST",
        "/v1/setup/generate",
        json!({"what": "hostname"}),
    )
    .await;
    assert!(name["value"].as_str().unwrap().starts_with("devbox-"));
    // A made-up password is kept beside a host, so it needs one.
    for what in ["nothing", "password"] {
        let asked = json!({"what": what});
        assert_eq!(
            send(client, "POST", "/v1/setup/generate", asked).await.0,
            400,
            "{what}"
        );
    }

    // Secrets are kept by name; their values do not come back.
    assert_eq!(
        send(
            client,
            "PUT",
            "/v1/secrets/github",
            json!({"value": "ghp_x"})
        )
        .await
        .0,
        200
    );
    assert_eq!(
        send(
            client,
            "PUT",
            "/v1/secrets/registry%2Fmain",
            json!({"value": "pw"})
        )
        .await
        .0,
        200
    );
    assert_eq!(
        send(
            client,
            "PUT",
            "/v1/secrets/Bad%20Name",
            json!({"value": "x"})
        )
        .await
        .0,
        400
    );
    let (_, secrets) = send(client, "GET", "/v1/secrets", Value::Null).await;
    assert_eq!(secrets, json!({"names": ["github", "registry/main"]}));
    assert_eq!(
        send(client, "DELETE", "/v1/secrets/github", Value::Null)
            .await
            .0,
        200
    );
    assert_eq!(
        send(client, "DELETE", "/v1/secrets/github", Value::Null)
            .await
            .0,
        404
    );

    // Registries, and their login goes with them.
    let registry = json!({"type": "npm", "url": "https://npm.example"});
    assert_eq!(
        send(client, "PUT", "/v1/registries/main", registry.clone())
            .await
            .0,
        200
    );
    assert_eq!(
        send(
            client,
            "PUT",
            "/v1/registries/main",
            json!({"type": "ftp", "url": "x"})
        )
        .await
        .0,
        400
    );
    let (_, registries) = send(client, "GET", "/v1/registries", Value::Null).await;
    assert_eq!(registries["registries"]["main"], registry);
    assert_eq!(
        send(client, "DELETE", "/v1/registries/main", Value::Null)
            .await
            .0,
        200
    );
    assert_eq!(
        send(client, "GET", "/v1/secrets", Value::Null).await.1,
        json!({"names": []})
    );

    // Sending secrets needs their values and the host.
    let wanting = json!({"builder": {}, "access": {"address": "192.0.2.7"}, "secrets": [{"name": "github", "env": "GH_TOKEN"}]});
    assert_eq!(
        send(client, "PUT", "/v1/hosts/studio/profile", wanting)
            .await
            .0,
        200
    );
    let (status, said) = send(
        client,
        "POST",
        "/v1/hosts/studio/actions/secrets",
        Value::Null,
    )
    .await;
    // The host does not answer, which is said before what it would be sent.
    assert_eq!(status, 501, "{said}");
    assert!(
        said["error"]["message"]
            .as_str()
            .unwrap()
            .contains("does not answer")
    );
    // A key pasted by an older caller is not kept.
    let pasted = json!({"builder": {}, "access": {"address": "192.0.2.7"}, "tailscale_auth_key": "tskey-auth-kFAKE-0000", "tailscale_auto_key": false});
    send(client, "PUT", "/v1/hosts/studio/profile", pasted).await;
    let (_, kept) = send(client, "GET", "/v1/hosts/studio/profile", Value::Null).await;
    assert!(!kept.to_string().contains("tailscale_a"), "{kept}");
    // A seed of a host that joins the tailnet is refused, with what to do,
    // while the server has no API key of the tailnet to make its key with.
    // It is a refusal before anything began, which the command line exits 3 for.
    let (status, said) = send(client, "POST", "/v1/hosts/studio/actions/seed", Value::Null).await;
    assert_eq!(status, 412, "{said}");
    assert_eq!(
        crate::cli::fail::status_of(status),
        crate::cli::fail::REFUSED
    );
    let message = said["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("berth secret set tailnet_api_key")
            && message.contains("switch Tailscale off"),
        "{message}"
    );

    // The user the server runs as changes any setting on its socket; over
    // the network only some are a caller's to change.
    let value = |value: &str| json!({"value": value});
    assert_eq!(
        send(client, "PUT", "/v1/settings/retention_days", value("30"))
            .await
            .0,
        200
    );
    assert_eq!(
        send(
            client,
            "PUT",
            "/v1/settings/oauth_issuer",
            value("https://x")
        )
        .await
        .0,
        200
    );
    let (_, settings) = send(client, "GET", "/v1/settings", Value::Null).await;
    let days = settings
        .as_array()
        .unwrap()
        .iter()
        .find(|setting| setting["key"] == "retention_days")
        .unwrap();
    assert_eq!(
        (&days["value"], &days["open"]),
        (&json!("30"), &json!(true))
    );
    // What lets someone in is not given back, whatever the caller may do.
    served.state.paths.set("server_token", "tok-123").unwrap();
    let (_, settings) = send(client, "GET", "/v1/settings", Value::Null).await;
    assert!(!settings.to_string().contains("tok-123"));
    let secret = settings
        .as_array()
        .unwrap()
        .iter()
        .find(|setting| setting["key"] == "server_token")
        .unwrap();
    assert_eq!(secret["value"], "");
    // The tailnet is set on the server, and its API key stays there.
    let at = value("http://192.0.2.9:8080");
    assert_eq!(
        send(client, "PUT", "/v1/settings/tailnet_login_server", at)
            .await
            .0,
        200
    );
    let key = value("hskey-api-x");
    assert_eq!(
        send(client, "PUT", "/v1/secrets/tailnet_api_key", key)
            .await
            .0,
        200
    );
    let giving = json!({"builder": {}, "access": {"address": "192.0.2.7"}, "secrets": [{"name": "tailnet_api_key", "env": "KEY"}]});
    send(client, "PUT", "/v1/hosts/studio/profile", giving).await;
    let (status, said) = send(
        client,
        "POST",
        "/v1/hosts/studio/actions/secrets",
        Value::Null,
    )
    .await;
    // Its controller does not answer; what it would be sent is checked by
    // the store's own test.
    assert_eq!(status, 501, "{said}");

    assert_eq!(
        send(client, "DELETE", "/v1/hosts/studio", Value::Null)
            .await
            .0,
        200
    );
    assert_eq!(
        send(client, "DELETE", "/v1/hosts/studio", Value::Null)
            .await
            .0,
        404
    );
    assert!(served.state.host("studio").is_none());
    // On this machine's socket the caller is its user, who may do everything.
    let (_, access) = send(client, "GET", "/v1/access", Value::Null).await;
    assert_eq!(access, json!({"role": "admin", "setup": true}));
}

#[tokio::test(flavor = "multi_thread")]
async fn plugins_are_managed_through_the_api() {
    let served = served().await;
    let mut client = served.client().await;
    let client = &mut client;
    let keys = |found: &Value| -> Vec<String> {
        found["features"]
            .as_array()
            .unwrap()
            .iter()
            .map(|feature| feature["key"].as_str().unwrap().to_owned())
            .collect()
    };

    // The plugins that come with berth stay.
    let (status, listed) = send(client, "GET", "/v1/plugins", Value::Null).await;
    assert_eq!(status, 200, "{listed}");
    let claude = listed["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .find(|plugin| plugin["id"] == "claude")
        .unwrap();
    assert_eq!(claude["origin"], "shipped");
    assert_eq!(claude["features"][0]["id"], "cli");
    let (status, said) = send(client, "DELETE", "/v1/plugins/claude", Value::Null).await;
    assert_eq!(status, 409, "{said}");
    assert_eq!(
        send(client, "DELETE", "/v1/plugins/nothing", Value::Null)
            .await
            .0,
        404
    );

    // A folder copied to the server's plugin directory is one of its own, and
    // a new host can have its features at once.
    let own = served.dir.path().join("config/plugins/hello");
    std::fs::create_dir_all(&own).unwrap();
    std::fs::write(
        own.join("package.json"),
        r#"{"name":"berth-plugin-hello","version":"1.0.0","displayName":"Hello",
            "berth":{"features":[{"id":"cow","label":"cowsay","group":"Development Tools",
              "actions":[{"name":"moo","run":"cowsay moo","agent":true}]}]}}"#,
    )
    .unwrap();
    let (_, listed) = send(client, "GET", "/v1/plugins", Value::Null).await;
    let hello = listed["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .find(|plugin| plugin["id"] == "hello")
        .unwrap()
        .clone();
    assert_eq!(
        (&hello["origin"], &hello["package"], &hello["version"]),
        (
            &json!("folder"),
            &json!("berth-plugin-hello"),
            &json!("1.0.0")
        )
    );
    assert_eq!(hello["features"][0]["actions"][0]["name"], "moo");
    assert!(listed["dir"].as_str().unwrap().ends_with("config/plugins"));
    let (_, found) = send(client, "GET", "/v1/setup", Value::Null).await;
    assert!(keys(&found).contains(&"hello/cow".to_owned()));
    // A folder has no registry to ask for a newer version, nor has berth's own.
    let (status, checked) = send(client, "GET", "/v1/plugins/updates", Value::Null).await;
    assert_eq!(status, 200, "{checked}");
    assert_eq!(checked["plugins"][0]["id"], "hello");
    assert!(
        checked["plugins"][0]["note"]
            .as_str()
            .unwrap()
            .contains("folder")
    );
    for (id, status, says) in [
        ("hello", 400, "copy the new one"),
        ("claude", 400, "comes with berth"),
        ("nothing", 404, "No plugin"),
    ] {
        let (got, said) = send(client, "POST", "/v1/plugins/update", json!({"id": id})).await;
        assert_eq!(got, status, "{id}: {said}");
        assert!(
            said["error"]["message"].as_str().unwrap().contains(says),
            "{said}"
        );
    }
    let asked = json!({"arguments": {"hosts": "nothing"}});
    let (status, said) = send(client, "POST", "/v1/installation/actions/rollout", asked).await;
    assert_eq!(status, 404, "{said}");
    let asked = json!({"arguments": {"force": "yes"}});
    let (status, said) = send(client, "POST", "/v1/installation/actions/rollout", asked).await;
    assert_eq!(status, 400, "{said}");
    let (status, said) = send(client, "DELETE", "/v1/plugins/hello", Value::Null).await;
    assert_eq!(status, 200, "{said}");
    assert!(said["lines"][0].as_str().unwrap().starts_with('✓'));
    assert!(!own.exists());
    let (_, found) = send(client, "GET", "/v1/setup", Value::Null).await;
    assert!(!keys(&found).contains(&"hello/cow".to_owned()));

    // npm is given a package of a registry or an address, and never a path of
    // the server, a repository or an option.
    for spec in [
        "../x",
        "/etc",
        "-g",
        "git+ssh://x/y",
        "file:x",
        "a b",
        "Bad",
        "@x",
    ] {
        let (status, said) = send(client, "POST", "/v1/plugins", json!({"spec": spec})).await;
        assert_eq!(status, 400, "{spec}: {said}");
    }
    // What is sent has to be a .tgz.
    let headers = [("content-type", "application/gzip")];
    let answer = client
        .send(
            "POST",
            "/v1/plugins/package",
            &headers,
            "not a tarball".into(),
        )
        .await
        .unwrap();
    assert_eq!(answer.status, 400);
}

/// An issuer for the tests: a key pair, and its public half as a key set.
struct Issuer {
    key: jsonwebtoken::EncodingKey,
    keys: Value,
}

fn url64(data: &[u8]) -> String {
    berth_api::b64(data)
        .replace('+', "-")
        .replace('/', "_")
        .trim_end_matches('=')
        .to_owned()
}

impl Issuer {
    fn new() -> Issuer {
        use ring::signature::KeyPair;
        let random = ring::rand::SystemRandom::new();
        let pair = ring::signature::Ed25519KeyPair::generate_pkcs8(&random).unwrap();
        let public = ring::signature::Ed25519KeyPair::from_pkcs8(pair.as_ref()).unwrap();
        Issuer {
            key: jsonwebtoken::EncodingKey::from_ed_der(pair.as_ref()),
            keys: json!({"keys": [
                {"kty": "RSA", "kid": "unreadable"},
                {"kty": "OKP", "crv": "Ed25519", "kid": "k1", "alg": "EdDSA",
                 "x": url64(public.public_key().as_ref())},
            ]}),
        }
    }

    fn token(&self, claims: Value) -> String {
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::EdDSA);
        header.kid = Some("k1".into());
        let now = jsonwebtoken::get_current_timestamp();
        let mut all =
            json!({"iss": "https://id.example", "aud": "berth", "sub": "ada", "exp": now + 600});
        for (name, value) in claims.as_object().unwrap() {
            all[name] = value.clone();
        }
        jsonwebtoken::encode(&header, &all, &self.key).unwrap()
    }
}

/// A `berth serve` over TCP that trusts the issuer, and its address.
async fn trusting(issuer: &Issuer, setup: bool) -> (tempfile::TempDir, std::net::SocketAddr) {
    let keys = issuer.keys.clone();
    let key_server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let jwks = format!("http://{}/keys", key_server.local_addr().unwrap());
    let routes = axum::Router::new().route(
        "/keys",
        axum::routing::get(move || std::future::ready(axum::Json(keys.clone()))),
    );
    tokio::spawn(async move { axum::serve(key_server, routes).await });

    let dir = tempfile::tempdir().unwrap();
    let paths = Paths {
        config_dir: dir.path().join("config"),
        data_dir: dir.path().join("data"),
    };
    let store = Store::open(&store::default_file(&paths)).unwrap();
    let auth = super::auth::Auth::trusting("s3cret-token", "https://id.example", &jwks, setup);
    let state = Serve::with(paths, Arc::new(store), auth);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = api::router(state, true);
    tokio::spawn(async move { axum::serve(listener, router).await });
    (dir, address)
}

async fn as_bearer(
    address: std::net::SocketAddr,
    token: &str,
    method: &str,
    path: &str,
    body: Value,
) -> (u16, Value) {
    let stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let mut client = Client::over(stream).await.unwrap();
    let bearer = format!("Bearer {token}");
    let body = match body {
        Value::Null => String::new(),
        body => body.to_string(),
    };
    let headers = [
        ("authorization", bearer.as_str()),
        ("content-type", "application/json"),
    ];
    let answer = client
        .send(method, path, &headers, body.into())
        .await
        .unwrap();
    (
        answer.status,
        serde_json::from_slice(&answer.body).unwrap_or(Value::Null),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn an_access_token_gets_in_with_the_role_it_names() {
    let issuer = Issuer::new();
    let (dir, address) = trusting(&issuer, true).await;
    write_fake_provisioner(dir.path());
    let with = |scope: &str| issuer.token(json!({"scope": scope}));
    let (viewer, operator, admin) = (
        with("openid berth:view"),
        with("berth:operate"),
        with("berth:admin"),
    );
    // Registering one is set-up, for an admin alone.
    let registered = json!({"type": "fake/vm"});
    let (status, said) = as_bearer(
        address,
        &operator,
        "PUT",
        "/v1/provisioners/vm",
        registered.clone(),
    )
    .await;
    assert_eq!(status, 403, "{said}");
    let (status, said) = as_bearer(address, &admin, "PUT", "/v1/provisioners/vm", registered).await;
    assert_eq!(status, 200, "{said}");
    // A command that reaches a host is not taken from a caller over the network.
    let linked =
        json!({"builder": {}, "access": {"address": "192.0.2.7", "link": ["sh", "-c", "id"]}});
    let (status, said) =
        as_bearer(address, &admin, "PUT", "/v1/hosts/linked/profile", linked).await;
    assert_eq!(status, 400, "{said}");
    assert!(
        said["error"]["message"]
            .as_str()
            .unwrap()
            .contains("access.link")
    );
    let profile = json!({"builder": {}, "access": {"address": "192.0.2.7"}});

    // Looking is for every role, using for two, set-up for one.
    assert_eq!(
        as_bearer(address, &viewer, "GET", "/v1/hosts", Value::Null)
            .await
            .0,
        200
    );
    let (status, said) = as_bearer(address, &viewer, "PUT", "/v1/kv/ui/x", json!(1)).await;
    assert_eq!((status, &said["error"]["code"]), (403, &json!("forbidden")));
    assert!(
        said["error"]["message"]
            .as_str()
            .unwrap()
            .contains("operate")
    );
    assert_eq!(
        as_bearer(
            address,
            &viewer,
            "POST",
            "/v1/hosts/a/controller/actions/x",
            json!({})
        )
        .await
        .0,
        403
    );
    assert_eq!(
        as_bearer(
            address,
            &viewer,
            "GET",
            "/v1/hosts/a/controller/files?path=x",
            Value::Null
        )
        .await
        .0,
        403
    );
    assert_eq!(
        as_bearer(address, &operator, "PUT", "/v1/kv/ui/x", json!(1))
            .await
            .0,
        200
    );
    assert_eq!(
        as_bearer(
            address,
            &operator,
            "PUT",
            "/v1/hosts/a/profile",
            profile.clone()
        )
        .await
        .0,
        403
    );
    assert_eq!(
        as_bearer(address, &operator, "GET", "/v1/secrets", Value::Null)
            .await
            .0,
        403
    );
    assert_eq!(
        as_bearer(
            address,
            &admin,
            "PUT",
            "/v1/hosts/a/profile",
            profile.clone()
        )
        .await
        .0,
        200
    );
    // An action has its own role, asked when it is run.
    for path in [
        "/v1/hosts/a/actions/apply",
        "/v1/hosts/a/actions/seed",
        "/v1/installation/actions/rollout",
    ] {
        let (status, said) = as_bearer(address, &operator, "POST", path, Value::Null).await;
        assert_eq!(status, 403, "{path}: {said}");
        assert!(
            said["error"]["message"].as_str().unwrap().contains("admin"),
            "{said}"
        );
    }
    let (status, said) = as_bearer(
        address,
        &viewer,
        "GET",
        "/v1/installation/actions",
        Value::Null,
    )
    .await;
    assert_eq!((status, &said[0]["name"]), (200, &json!("rollout")));
    assert_eq!(said[0]["role"], "admin");
    // A provisioner is looked at by anyone, and each of its actions has its own role.
    for path in [
        "/v1/provisioners",
        "/v1/provisioners/vm/actions",
        "/v1/provisioners/vm",
    ] {
        let (status, said) = as_bearer(address, &viewer, "GET", path, Value::Null).await;
        assert_eq!(status, 200, "{path}: {said}");
    }
    for (token, action, wanted) in [
        (&viewer, "prune", 403),
        (&operator, "prune", 403),
        (&viewer, "restart", 403),
        // Let in, it cannot run yet: the provisioner's secret has no value.
        (&operator, "restart", 501),
        (&viewer, "images", 501),
        (&admin, "prune", 501),
    ] {
        let path = format!("/v1/provisioners/vm/actions/{action}");
        let (status, said) = as_bearer(address, token, "POST", &path, Value::Null).await;
        assert_eq!(status, wanted, "{action}: {said}");
    }
    // Machines are created with the server's secrets: set-up too.
    for (method, path) in [
        ("POST", "/v1/resources"),
        ("PUT", "/v1/resources/r-a"),
        ("DELETE", "/v1/resources/r-a"),
    ] {
        let (status, said) = as_bearer(address, &operator, method, path, json!({})).await;
        assert_eq!(status, 403, "{method} {path}: {said}");
    }
    assert_eq!(
        as_bearer(address, &admin, "GET", "/v1/provisioners", Value::Null)
            .await
            .0,
        200
    );
    let (_, access) = as_bearer(address, &operator, "GET", "/v1/access", Value::Null).await;
    assert_eq!(
        access,
        json!({"role": "operate", "setup": true, "subject": "ada"})
    );
    // The server's own token still does everything.
    assert_eq!(
        as_bearer(address, "s3cret-token", "GET", "/v1/secrets", Value::Null)
            .await
            .0,
        200
    );

    // A token has to be the issuer's, for this server, in time, and name a role.
    let now = jsonwebtoken::get_current_timestamp();
    let refused = [
        issuer.token(json!({"scope": "berth:admin", "exp": now - 600})),
        issuer.token(json!({"scope": "berth:admin", "aud": "another"})),
        issuer.token(json!({"scope": "berth:admin", "iss": "https://evil.example"})),
        Issuer::new().token(json!({"scope": "berth:admin"})),
    ];
    for token in &refused {
        assert_eq!(
            as_bearer(address, token, "GET", "/v1/hosts", Value::Null)
                .await
                .0,
            401
        );
    }
    let roleless = issuer.token(json!({"scope": "openid admin"}));
    assert_eq!(
        as_bearer(address, &roleless, "GET", "/v1/hosts", Value::Null)
            .await
            .0,
        403
    );

    // On the portal each operation is asked about, and a token is a protocol.
    let connect = || async { tokio::net::TcpStream::connect(address).await.unwrap() };
    let socket = client::websocket(connect().await, "/v1/portal", &viewer)
        .await
        .unwrap();
    let portal = berth_api::portal::Portal::over(socket);
    assert!(
        portal
            .call("hosts.list", None, Value::Null, None)
            .await
            .is_ok()
    );
    let asked = portal.call("access", None, Value::Null, None).await;
    assert_eq!(asked.unwrap()["role"], "view");
    let saved = portal
        .call(
            "hosts.save",
            None,
            json!({"name": "b"}),
            Some(profile.clone()),
        )
        .await;
    assert_eq!(saved.unwrap_err().status(), 403);
    let run = portal
        .call(
            "actions.run",
            Some("a"),
            json!({"action": "x"}),
            Some(json!({})),
        )
        .await;
    assert_eq!(run.unwrap_err().status(), 403);
    assert!(
        client::websocket(
            connect().await,
            "/v1/hosts/a/controller/sessions/x",
            &viewer
        )
        .await
        .is_err()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn set_up_can_be_closed_to_the_network() {
    let issuer = Issuer::new();
    let (_dir, address) = trusting(&issuer, false).await;
    let admin = issuer.token(json!({"scope": "berth:admin"}));
    let profile = json!({"builder": {}, "access": {"address": "192.0.2.7"}});
    for token in [admin.as_str(), "s3cret-token"] {
        let (status, said) = as_bearer(
            address,
            token,
            "PUT",
            "/v1/hosts/a/profile",
            profile.clone(),
        )
        .await;
        assert_eq!(status, 403);
        assert!(
            said["error"]["message"]
                .as_str()
                .unwrap()
                .contains("set-up")
        );
        // Installing a plugin is set-up too: its Ansible tasks run when a host is applied.
        let spec = json!({"spec": "berth-plugin-foo"});
        assert_eq!(
            as_bearer(address, token, "POST", "/v1/plugins", spec)
                .await
                .0,
            403
        );
        for (method, path) in [
            ("POST", "/v1/resources"),
            ("DELETE", "/v1/resources/r-a"),
            ("POST", "/v1/installation/actions/rollout"),
        ] {
            let (status, said) = as_bearer(address, token, method, path, json!({})).await;
            assert_eq!(status, 403, "{method} {path}: {said}");
            assert!(
                said["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("set-up")
            );
        }
        assert_eq!(
            as_bearer(address, token, "PUT", "/v1/kv/ui/x", json!(1))
                .await
                .0,
            200
        );
        let (_, access) = as_bearer(address, token, "GET", "/v1/access", Value::Null).await;
        assert_eq!(
            (&access["role"], &access["setup"]),
            (&json!("operate"), &json!(false))
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_server_says_whose_access_tokens_it_takes() {
    let issuer = Arc::new(Issuer::new());
    let (_dir, address) = trusting(&issuer, true).await;

    // The server says whose tokens it takes, to anyone who asks.
    let named = berth_core::server::fetch(&format!("http://{address}{}", super::api::RESOURCE))
        .await
        .unwrap();
    let named: Value = serde_json::from_slice(&named).unwrap();
    assert_eq!(
        named["authorization_servers"],
        json!(["https://id.example"])
    );
    assert_eq!(named["resource"], "berth");

    // A token minted by that issuer gives its role.
    let token = issuer.token(json!({"scope": "berth:operate"}));
    let (_, access) = as_bearer(address, &token, "GET", "/v1/access", Value::Null).await;
    assert_eq!(access["role"], "operate");
}

// ── Provisioners ────────────────────────────────────────────────────────────

/// A provisioner that writes down each verb it is asked in `requests.log`
/// of its folder and answers from files there: `answer.VERB` on standard
/// output, `code.VERB` as its exit code, `say.VERB` on standard error.
const FAKE_PROVISIONER: &str = r#"#!/bin/sh
verb="$1"
request=$(cat)
echo "$verb" >> requests.log
printf '%s\n' "$request" > "last.$verb"
[ -f leak ] && printf '%s\n' "$request" >&2
[ -f "say.$verb" ] && cat "say.$verb" >&2
[ -f "answer.$verb" ] && cat "answer.$verb"
exit "$(cat "code.$verb" 2>/dev/null || echo 0)"
"#;

/// Quick, and with a new machine that answers at once.
fn quick() -> berth_core::provision::Options {
    berth_core::provision::Options {
        quick: Duration::from_secs(5),
        create: Duration::from_secs(5),
        change: Duration::from_secs(5),
        boot: Duration::from_millis(500),
        ssh: Duration::from_millis(500),
        every: Duration::from_millis(10),
        alive: Duration::from_secs(60),
        reaches: |_, _| true,
        answers: |_, _| true,
        first_boot: |_, _, _| Ok(String::new()),
        finish: |_, host, _, out| {
            writeln!(out, "finished {}", host.address())?;
            Ok(())
        },
    }
}

/// Register the fake plugin's provisioners as `vm` and `old`.
async fn register_fake(client: &mut Client) {
    for (name, kind) in [("vm", "fake/vm"), ("old", "fake/old")] {
        let path = format!("/v1/provisioners/{name}");
        let (status, said) = send(client, "PUT", &path, json!({"type": kind})).await;
        assert_eq!(status, 200, "{said}");
    }
}

/// The fake provisioner as a plugin of the server's own, and its folder.
fn fake_provisioner(served: &Served) -> PathBuf {
    write_fake_provisioner(served.dir.path())
}

/// The fake provisioner in the plugins of the server whose folder is `home`.
fn write_fake_provisioner(home: &std::path::Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir = home.join("config/plugins/fake");
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::write(dir.join("bin/provision"), FAKE_PROVISIONER).unwrap();
    std::fs::set_permissions(
        dir.join("bin/provision"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    std::fs::write(
        dir.join("package.json"),
        r#"{"name": "berth-plugin-fake", "berth": {"provisioners": [
            {"id": "vm", "label": "Fake VM", "command": "bin/provision", "protocol": 2,
             "verbs": ["list"], "secrets": ["api_token"],
             "arguments": [{"name": "server", "required": true}],
             "kinds": [{"id": "machine", "setup": "apply", "verbs": ["start"],
                        "arguments": [{"name": "cpus", "default": "4"}]},
                       {"id": "deployment", "label": "Project", "verbs": ["change", "logs"],
                        "spec": {"format": "text", "label": "Spec"},
                        "arguments": [{"name": "replicas", "default": "1"}],
                        "actions": [{"name": "services", "changes": false},
                                    {"name": "restart", "confirm": "Restart?"}]},
                       {"id": "box", "label": "Box", "verbs": ["exec"]}],
             "actions": [
               {"name": "images", "label": "Images", "changes": false},
               {"name": "prune", "label": "Prune", "confirm": "Prune?",
                "arguments": [{"name": "password", "secret": true}]},
               {"name": "restart", "role": "operate"}]},
            {"id": "old", "command": "bin/provision", "setup": "apply"}]}}"#,
    )
    .unwrap();
    dir
}

/// A job's output and exit code once it has ended.
async fn job_ended(socket: &std::path::Path, id: &str) -> (String, Value) {
    let mut stream = client::websocket_unix(socket, &format!("/v1/jobs/{id}"))
        .await
        .unwrap();
    let mut output = String::new();
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(20), stream.next()).await;
        match frame.expect("the job ends in time") {
            Some(Ok(Frame::Binary(bytes))) => output.push_str(&String::from_utf8_lossy(&bytes)),
            Some(Ok(Frame::Text(text))) => {
                let said: Value = serde_json::from_str(text.as_str()).unwrap();
                if let Some(exited) = said.get("exited") {
                    return (output, exited["exit_code"].clone());
                }
            }
            Some(Ok(_)) => {}
            other => panic!("the job's stream ended after {output:?}: {other:?}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provisioner_has_a_page_and_actions_that_run_as_jobs() {
    let served = served_with(quick).await;
    let fake = fake_provisioner(&served);
    let mut client = served.client().await;
    let client = &mut client;
    register_fake(client).await;
    served.state.paths.prepare().unwrap();
    let message = |said: &Value| said["error"]["message"].as_str().unwrap_or("").to_owned();

    // Listed with why it cannot be used, and one of the first protocol refused.
    let (_, listed) = send(client, "GET", "/v1/provisioners", Value::Null).await;
    let find = |id: &str| {
        listed
            .as_array()
            .unwrap()
            .iter()
            .find(|known| known["id"] == id)
            .unwrap()
            .clone()
    };
    assert!(
        find("vm")["problem"]
            .as_str()
            .unwrap()
            .contains("api_token")
    );
    assert!(
        find("old")["problem"]
            .as_str()
            .unwrap()
            .contains("version 1")
    );
    assert_eq!(find("vm")["own"], json!(["server"]));
    assert_eq!(find("vm")["kinds"][0]["id"], "machine");
    assert_eq!(find("vm")["actions"][0]["name"], "images");
    let (status, page) = send(client, "GET", "/v1/provisioners/vm", Value::Null).await;
    assert_eq!(
        (status, &page["state"]),
        (200, &json!("unusable")),
        "{page}"
    );
    assert_eq!(
        send(client, "GET", "/v1/provisioners/none", Value::Null)
            .await
            .0,
        404
    );
    send(
        client,
        "PUT",
        "/v1/secrets/api_token",
        json!({"value": "tok-s3cret"}),
    )
    .await;

    // Its settings are an action of berth's, a job like any that changes something.
    let (status, actions) = send(client, "GET", "/v1/provisioners/vm/actions", Value::Null).await;
    assert_eq!(status, 200, "{actions}");
    let names: Vec<&str> = actions
        .as_array()
        .unwrap()
        .iter()
        .map(|action| action["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "new-deployment",
            "new-box",
            "settings",
            "images",
            "prune",
            "restart",
            "secrets",
            "adopt",
            "remove"
        ]
    );
    assert_eq!(actions[2]["arguments"][0]["name"], "server");
    let (status, said) = send(
        client,
        "POST",
        "/v1/provisioners/vm/actions/dance",
        Value::Null,
    )
    .await;
    assert_eq!(status, 404, "{said}");
    assert!(
        message(&said).contains("Its actions: new-deployment, new-box, settings, images"),
        "{said}"
    );
    let (status, said) = send(
        client,
        "POST",
        "/v1/provisioners/vm/actions/images",
        Value::Null,
    )
    .await;
    assert_eq!(status, 400, "{said}");
    assert!(message(&said).contains("server=VALUE"), "{said}");
    let settings = json!({"arguments": {"server": "https://192.0.2.10:8443"}, "token": "set-1"});
    let (status, acted) = send(
        client,
        "POST",
        "/v1/provisioners/vm/actions/settings",
        settings.clone(),
    )
    .await;
    assert_eq!(status, 202, "{acted}");
    assert_eq!(
        (&acted["job"]["subject"], &acted["job"]["action"]),
        (&json!("provisioner:vm"), &json!("settings"))
    );
    let (output, code) = job_ended(&served.socket, acted["job"]["id"].as_str().unwrap()).await;
    assert_eq!(code, 0, "{output}");
    assert!(
        output.contains("vm uses server=https://192.0.2.10:8443"),
        "{output}"
    );
    // The same token again is the same job.
    let (_, again) = send(
        client,
        "POST",
        "/v1/provisioners/vm/actions/settings",
        settings,
    )
    .await;
    assert_eq!(again["job"]["id"], acted["job"]["id"]);

    // An action that only reads answers at once.
    std::fs::write(
        fake.join("answer.act"),
        r#"{"message": "1 image", "details": [{"label": "ubuntu", "value": "80 MB"}]}"#,
    )
    .unwrap();
    let (status, acted) = send(
        client,
        "POST",
        "/v1/provisioners/vm/actions/images",
        Value::Null,
    )
    .await;
    assert_eq!(status, 200, "{acted}");
    assert_eq!(acted["result"]["message"], "1 image");
    assert_eq!(acted["result"]["details"][0]["value"], "80 MB");
    assert!(acted.get("job").is_none());
    // One that changes something is a job, with its secret masked.
    std::fs::write(fake.join("say.act"), "using hunter2-pass\n").unwrap();
    let (status, acted) = send(
        client,
        "POST",
        "/v1/provisioners/vm/actions/prune",
        json!({"arguments": {"password": "hunter2-pass"}}),
    )
    .await;
    assert_eq!(status, 202, "{acted}");
    let (output, code) = job_ended(&served.socket, acted["job"]["id"].as_str().unwrap()).await;
    assert_eq!(code, 0, "{output}");
    assert!(
        output.contains("using [secret]") && !output.contains("hunter2"),
        "{output}"
    );
    assert!(
        output.contains("✓ 1 image") && output.contains("ubuntu  80 MB"),
        "{output}"
    );
    let (_, job) = send(
        client,
        "GET",
        &format!("/v1/jobs/{}", acted["job"]["id"].as_str().unwrap()),
        Value::Null,
    )
    .await;
    assert_eq!(job["subject"], "provisioner:vm");
    // One that fails says why, in the job or as the answer.
    std::fs::write(fake.join("code.act"), "1").unwrap();
    std::fs::write(fake.join("say.act"), "engine gone\n").unwrap();
    let (status, said) = send(
        client,
        "POST",
        "/v1/provisioners/vm/actions/images",
        Value::Null,
    )
    .await;
    assert_eq!(status, 502, "{said}");
    assert!(message(&said).contains("engine gone"), "{said}");
    // Two changes to one provisioner do not overlap.
    let held = berth_core::runner::lock(
        &served.state.paths,
        &berth_core::runner::Subject::Provisioner("vm".into()),
        "prune",
    )
    .unwrap();
    let (status, said) = send(
        client,
        "POST",
        "/v1/provisioners/vm/actions/prune",
        Value::Null,
    )
    .await;
    assert_eq!(status, 409, "{said}");
    drop(held);
    std::fs::remove_file(fake.join("code.act")).unwrap();

    // The page: what it says of itself, and the machines berth made with it.
    std::fs::write(
        fake.join("answer.status"),
        r#"{"message": "It answers", "details": [{"label": "Version", "value": "2"}]}"#,
    )
    .unwrap();
    std::fs::write(
        fake.join("answer.list"),
        r#"{"resources": [{"id": "vm-7", "kind": "machine", "name": "attic", "state": "running"}]}"#,
    )
    .unwrap();
    let (status, page) = send(client, "GET", "/v1/provisioners/vm", Value::Null).await;
    assert_eq!(status, 200, "{page}");
    assert_eq!(
        (&page["state"], &page["message"]),
        (&json!("ready"), &json!("It answers"))
    );
    assert_eq!(
        page["provisioner"]["settings"]["server"],
        "https://192.0.2.10:8443"
    );
    assert_eq!(page["resources"][0]["host"], "attic");
    assert!(page["resources"][0].get("resource").is_none());
    // Also over the portal.
    let portal = berth_api::portal::Portal::over(
        client::websocket_unix(&served.socket, "/v1/portal")
            .await
            .unwrap(),
    );
    let acted = portal
        .call(
            "provisioners.act",
            None,
            json!({"name": "vm", "action": "images"}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(acted["result"]["message"], "1 image");
    let got = portal
        .call("provisioners.get", None, json!({"name": "vm"}), None)
        .await
        .unwrap();
    assert_eq!(got["state"], "ready");

    // A registered one keeps secrets of its own, under the names its plugin gives them.
    let (status, acted) = send(
        client,
        "POST",
        "/v1/provisioners/vm/actions/secrets",
        json!({"arguments": {"api_token": "tok-own"}}),
    )
    .await;
    assert_eq!(status, 202, "{acted}");
    let (output, code) = job_ended(&served.socket, acted["job"]["id"].as_str().unwrap()).await;
    assert_eq!(code, 0, "{output}");
    assert!(!output.contains("tok-own"), "{output}");
    let registries = berth_core::registry::Store::own(served.state.store.clone());
    assert_eq!(
        registries
            .secret("provisioner/vm/api_token")
            .unwrap()
            .as_deref(),
        Some("tok-own")
    );

    // A deployment is made with an action of the provisioner, from its specification.
    std::fs::write(
        fake.join("answer.create"),
        r#"{"id": "wiki-1", "state": "running"}"#,
    )
    .unwrap();
    let (status, acted) = send(
        client,
        "POST",
        "/v1/provisioners/vm/actions/new-deployment",
        json!({"arguments": {"name": "wiki", "spec": "services:\n  wiki:\n    image: wiki\n"}}),
    )
    .await;
    assert_eq!(status, 202, "{acted}");
    let (output, code) = job_ended(&served.socket, acted["job"]["id"].as_str().unwrap()).await;
    assert_eq!(code, 0, "{output}");
    let (_, made) = send(client, "GET", "/v1/resources?provisioner=vm", Value::Null).await;
    let deployment = made
        .as_array()
        .unwrap()
        .iter()
        .find(|made| made["kind"] == "deployment")
        .unwrap_or_else(|| panic!("a deployment in {made}"));
    assert_eq!(deployment["name"], "wiki");
    let asked = std::fs::read_to_string(fake.join("last.create")).unwrap();
    assert!(
        asked.contains("\"api_token\":\"tok-own\"") && asked.contains("image: wiki"),
        "{asked}"
    );
    assert!(!deployment["spec_sha256"].as_str().unwrap().is_empty());

    // One that holds something is kept; one that holds nothing is forgotten.
    let (status, said) = send(client, "DELETE", "/v1/provisioners/vm", Value::Null).await;
    assert_eq!(status, 409, "{said}");
    assert!(
        message(&said).contains(deployment["id"].as_str().unwrap()),
        "{said}"
    );
    let (status, said) = send(client, "DELETE", "/v1/provisioners/old", Value::Null).await;
    assert_eq!(status, 204, "{said}");
    let (_, listed) = send(client, "GET", "/v1/provisioners", Value::Null).await;
    let names: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|known| known["id"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["vm"]);
    // A registered one keeps its type, and a type has to be one a plugin brings.
    let (status, said) = send(
        client,
        "PUT",
        "/v1/provisioners/vm",
        json!({"type": "fake/old"}),
    )
    .await;
    assert_eq!(status, 400, "{said}");
    for (name, kind) in [("other", "fake/none"), ("Bad/Name", "fake/vm")] {
        let path = format!("/v1/provisioners/{name}");
        let (status, said) = send(client, "PUT", &path, json!({"type": kind})).await;
        assert!(status == 400 || status == 404, "{name}: {status} {said}");
    }
    let (_, types) = send(client, "GET", "/v1/provisioner-types", Value::Null).await;
    assert!(
        types
            .as_array()
            .unwrap()
            .iter()
            .any(|known| known["type"] == "fake/vm" && known["id"] == "fake/vm"),
        "{types}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn machines_are_created_and_deleted_through_the_api() {
    let served = served_with(quick).await;
    let fake = fake_provisioner(&served);
    let mut client = served.client().await;
    let client = &mut client;
    register_fake(client).await;
    let paths = &served.state.paths;
    paths.prepare().unwrap();
    // How the server reaches it is its own; the address comes from the provisioner.
    berth_core::profiles::Place::own("studio")
        .unwrap()
        .save(
            paths,
            &json!({"builder": {}, "access": {"user": "ops", "link": ["sh", "-c", "exit 1"]},
                    "ansible_vars": {"devbox_install_tailscale": false}}),
        )
        .unwrap();
    let message = |said: &Value| said["error"]["message"].as_str().unwrap_or("").to_owned();
    let machine = || berth_core::resource::of_host(paths, "studio").unwrap();

    let (status, listed) = send(client, "GET", "/v1/provisioners", Value::Null).await;
    assert_eq!(status, 200, "{listed}");
    let vm = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|known| known["id"] == "vm")
        .unwrap()
        .clone();
    assert_eq!(vm["missing_secrets"], json!(["api_token"]));
    assert_eq!(vm["kinds"][1]["actions"][1]["role"], "admin");

    // Refusals are answers of their own, before there is a job.
    let asked = json!({"provisioner": "vm", "host": "studio",
                       "arguments": {"server": "https://192.0.2.10:8443"}});
    // A secret the server lacks is a refusal, which the command line exits 3 for.
    let (status, said) = send(client, "POST", "/v1/resources", asked.clone()).await;
    assert_eq!(status, 412, "{said}");
    assert!(message(&said).contains("api_token"), "{said}");
    let secret = json!({"value": "tok-s3cret"});
    send(client, "PUT", "/v1/secrets/api_token", secret).await;
    for (body, wanted) in [
        (json!({"provisioner": "vm", "host": "nobody"}), 404),
        (json!({"provisioner": "none", "host": "studio"}), 404),
        (json!({"provisioner": "vm", "host": "studio"}), 400),
        (json!({"provisioner": "vm"}), 400),
        (json!({"nothing": 1}), 400),
    ] {
        let (status, said) = send(client, "POST", "/v1/resources", body.clone()).await;
        assert_eq!(status, wanted, "{body}: {said}");
    }
    assert!(machine().is_none());

    // A job that fails says why, and leaves the record and its token to ask again with.
    let mut events = client::websocket_unix(&served.socket, "/v1/events")
        .await
        .unwrap();
    events
        .send(Frame::Text(r#"{"jobs": true}"#.into()))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    std::fs::write(fake.join("code.create"), "3").unwrap();
    std::fs::write(fake.join("say.create"), "quota exceeded\n").unwrap();
    let (status, created) = send(client, "POST", "/v1/resources", asked.clone()).await;
    assert_eq!(status, 202, "{created}");
    let id = created["resource"]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        (&created["resource"]["kind"], &created["resource"]["host"]),
        (&json!("machine"), &json!("studio"))
    );
    assert_eq!(
        (&created["job"]["action"], &created["job"]["subject"]),
        (&json!("create"), &json!(format!("resource:{id}")))
    );
    let started = text(&mut events).await;
    assert_eq!(
        (&started["source"], &started["job"]["action"]),
        (&json!("job"), &json!("create"))
    );
    let (output, code) = job_ended(&served.socket, created["job"]["id"].as_str().unwrap()).await;
    assert_eq!(code, 1, "{output}");
    assert!(output.contains("quota exceeded"), "{output}");
    assert!(text(&mut events).await["job"]["ended"].is_string());
    let token = machine().unwrap().record.token;
    let (_, host) = send(client, "GET", "/v1/hosts/studio", Value::Null).await;
    assert_eq!(host["address"], "", "{host}");
    let (_, listed) = send(client, "GET", "/v1/resources?host=studio", Value::Null).await;
    assert_eq!(listed[0]["state"], "unknown", "{listed}");
    assert!(message(&json!({"error": {"message": listed[0]["problem"]}})).contains("did not end"));

    std::fs::remove_file(fake.join("code.create")).unwrap();
    std::fs::write(
        fake.join("answer.create"),
        r#"{"id": "vm-1", "address": "192.0.2.20"}"#,
    )
    .unwrap();
    let mut again = asked.clone();
    again["token"] = json!("caller-1");
    let (status, created) = send(client, "POST", "/v1/resources", again.clone()).await;
    assert_eq!(status, 202, "{created}");
    // The same machine is asked for again, with the same token.
    assert_eq!(created["resource"]["id"], json!(id));
    let (output, code) = job_ended(&served.socket, created["job"]["id"].as_str().unwrap()).await;
    assert_eq!(code, 0, "{output}");
    assert!(output.contains("finished 192.0.2.20"), "{output}");
    let kept = machine().unwrap().record;
    assert_eq!((kept.external_id.as_str(), kept.token), ("vm-1", token));
    // The caller's token again answers with the job it started, and nothing runs.
    let (status, repeated) = send(client, "POST", "/v1/resources", again.clone()).await;
    assert_eq!(status, 202, "{repeated}");
    assert_eq!(repeated["job"]["id"], created["job"]["id"]);
    // The server reaches the host where the provisioner said it is.
    let (_, host) = send(client, "GET", "/v1/hosts/studio", Value::Null).await;
    assert_eq!(host["address"], "192.0.2.20", "{host}");
    let (status, said) = send(client, "POST", "/v1/resources", asked.clone()).await;
    assert_eq!(status, 409, "{said}");

    // What its provisioner says of it, and what can be done to it.
    std::fs::write(fake.join("answer.status"), r#"{"state": "running"}"#).unwrap();
    let path = format!("/v1/resources/{id}");
    let (status, seen) = send(client, "GET", &path, Value::Null).await;
    assert_eq!(status, 200, "{seen}");
    assert_eq!(
        (&seen["state"], &seen["external_id"], &seen["verbs"]),
        (&json!("running"), &json!("vm-1"), &json!(["start"]))
    );
    let (_, actions) = send(client, "GET", &format!("{path}/actions"), Value::Null).await;
    let names: Vec<&str> = actions
        .as_array()
        .unwrap()
        .iter()
        .map(|action| action["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["start", "remove"]);
    assert_eq!(
        send(client, "GET", "/v1/resources/r-nobody", Value::Null)
            .await
            .0,
        404
    );
    for (action, wanted) in [("start", 202), ("stop", 404), ("dance", 404)] {
        let (status, said) = send(
            client,
            "POST",
            &format!("{path}/actions/{action}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, wanted, "{action}: {said}");
        if status == 202 {
            let (output, code) =
                job_ended(&served.socket, said["job"]["id"].as_str().unwrap()).await;
            assert_eq!(code, 0, "{output}");
        }
    }
    // A machine that is gone is missing, and its record stays.
    std::fs::write(fake.join("answer.status"), r#"{"state": "gone"}"#).unwrap();
    let (_, seen) = send(client, "GET", &path, Value::Null).await;
    assert_eq!(seen["state"], "missing");
    assert!(machine().is_some());

    // One thing at a time is done to a host's machine, as to the host.
    let held = berth_core::provision::lock(paths, "studio").unwrap();
    for (method, at) in [
        ("POST", format!("{path}/actions/start")),
        ("DELETE", path.clone()),
    ] {
        let (status, said) = send(client, method, &at, Value::Null).await;
        assert_eq!(status, 409, "{method} {at}: {said}");
    }
    drop(held);

    // The provisioner's page lists it, joined with what the provisioner lists.
    std::fs::write(
        fake.join("answer.list"),
        r#"{"resources": [{"id": "vm-1", "kind": "machine", "name": "studio", "state": "running"},
            {"id": "vm-9", "kind": "machine", "name": "attic"}]}"#,
    )
    .unwrap();
    let (_, page) = send(client, "GET", "/v1/provisioners/vm?server=x", Value::Null).await;
    assert_eq!(page["resources"][0]["resource"], json!(id), "{page}");
    assert_eq!(page["resources"][0]["state"], "running");
    assert!(page["resources"][1].get("resource").is_none());
    let portal = berth_api::portal::Portal::over(
        client::websocket_unix(&served.socket, "/v1/portal")
            .await
            .unwrap(),
    );
    let got = portal
        .call("resources.list", None, json!({"kind": "machine"}), None)
        .await;
    assert_eq!(got.unwrap()[0]["id"], json!(id));

    // The host stays while its machine does.
    let (status, said) = send(client, "DELETE", "/v1/hosts/studio", Value::Null).await;
    assert_eq!(status, 409, "{said}");
    assert!(
        message(&said).contains("berth host remove studio --destroy"),
        "{said}"
    );

    std::fs::write(fake.join("answer.status"), r#"{"state": "running"}"#).unwrap();
    let (status, acted) = send(client, "DELETE", &path, Value::Null).await;
    assert_eq!(
        (status, &acted["job"]["action"]),
        (202, &json!("remove")),
        "{acted}"
    );
    let (output, code) = job_ended(&served.socket, acted["job"]["id"].as_str().unwrap()).await;
    assert_eq!(code, 0, "{output}");
    assert!(output.contains("tailnet"), "{output}");
    assert!(machine().is_none());
    let asked: Vec<String> = std::fs::read_to_string(fake.join("requests.log"))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(asked.last().map(String::as_str), Some("destroy"));
    assert_eq!(send(client, "DELETE", &path, Value::Null).await.0, 404);
    assert_eq!(
        send(client, "DELETE", "/v1/hosts/studio", Value::Null)
            .await
            .0,
        200
    );
    // The profile went, and with it everything kept of the host.
    let store = berth_core::store::shared(paths).unwrap();
    for kind in [
        berth_core::store::kind::PROFILE,
        berth_core::store::kind::CREDENTIALS,
    ] {
        let kept = store.read(kind, berth_core::store::OWN, "studio").unwrap();
        assert!(kept.is_none(), "{kind}");
    }
    assert!(
        store
            .names(berth_core::store::kind::RESOURCE, berth_core::store::OWN)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deployment_keeps_its_spec_and_gets_its_secrets_only_on_standard_input() {
    let served = served_with(quick).await;
    let fake = fake_provisioner(&served);
    let mut client = served.client().await;
    let client = &mut client;
    register_fake(client).await;
    let paths = &served.state.paths;
    paths.prepare().unwrap();
    let message = |said: &Value| said["error"]["message"].as_str().unwrap_or("").to_owned();
    send(
        client,
        "PUT",
        "/v1/secrets/api_token",
        json!({"value": "tok-s3cret"}),
    )
    .await;
    let spec = "services:\n  wiki:\n    image: wiki\n    environment:\n      DB: ${DB_PASSWORD}\n";
    let asked = json!({"provisioner": "vm", "kind": "deployment", "name": "wiki",
                       "arguments": {"server": "x"}, "spec": spec,
                       "secrets": [{"name": "wiki_db", "env": "DB_PASSWORD"}], "token": "t-1"});
    // A secret it is given has to have a value; a name and a spec have to fit.
    let (status, said) = send(client, "POST", "/v1/resources", asked.clone()).await;
    assert_eq!(status, 400, "{said}");
    assert!(message(&said).contains("wiki_db"), "{said}");
    send(
        client,
        "PUT",
        "/v1/secrets/wiki_db",
        json!({"value": "db-pa55word-made-up"}),
    )
    .await;
    for (change, wanted) in [
        (json!({"name": "Wiki!"}), "cannot be a deployment's name"),
        (json!({"spec": null}), "made from a specification"),
        (json!({"kind": "volume"}), "makes no volume"),
        (
            json!({"secrets": [{"name": "wiki_db", "env": "1X"}]}),
            "environment variable",
        ),
    ] {
        let mut body = asked.clone();
        for (key, value) in change.as_object().unwrap() {
            body[key] = value.clone();
        }
        let (status, said) = send(client, "POST", "/v1/resources", body).await;
        assert_eq!(status, 400, "{change}: {said}");
        assert!(message(&said).contains(wanted), "{change}: {said}");
    }
    std::fs::write(fake.join("leak"), "").unwrap();
    std::fs::write(
        fake.join("answer.create"),
        r#"{"id": "wiki", "state": "running"}"#,
    )
    .unwrap();
    let (status, created) = send(client, "POST", "/v1/resources", asked.clone()).await;
    assert_eq!(status, 202, "{created}");
    let id = created["resource"]["id"].as_str().unwrap().to_owned();
    assert!(created["resource"].get("host").is_none());
    let (output, code) = job_ended(&served.socket, created["job"]["id"].as_str().unwrap()).await;
    assert_eq!(code, 0, "{output}");
    // The request carried the secret, which the job never shows.
    assert!(!output.contains("db-pa55word-made-up"), "{output}");
    assert!(output.contains("[secret]"), "{output}");
    let request: Value =
        serde_json::from_str(&std::fs::read_to_string(fake.join("last.create")).unwrap()).unwrap();
    assert_eq!(
        (&request["kind"], &request["name"], &request["spec"]),
        (&json!("deployment"), &json!("wiki"), &json!(spec))
    );
    assert_eq!(request["environment"]["DB_PASSWORD"], "db-pa55word-made-up");
    assert_eq!(
        request["arguments"],
        json!({"server": "x", "replicas": "1"})
    );
    assert_eq!(request["labels"]["berth-resource"], json!(id));
    // berth keeps the spec apart from the record, and the secret by name only.
    let store = berth_core::store::shared(paths).unwrap();
    let record = store
        .read(
            berth_core::store::kind::RESOURCE,
            berth_core::store::OWN,
            &id,
        )
        .unwrap()
        .unwrap();
    let record = String::from_utf8(record).unwrap();
    assert!(
        !record.contains("db-pa55word") && !record.contains("services:"),
        "{record}"
    );
    let kept = store
        .read(berth_core::store::kind::SPEC, berth_core::store::OWN, &id)
        .unwrap()
        .unwrap();
    assert_eq!(kept, spec.as_bytes());
    let (_, seen) = send(client, "GET", "/v1/resources?kind=deployment", Value::Null).await;
    assert_eq!(seen[0]["state"], "running", "{seen}");
    assert_eq!(seen[0]["spec_sha256"].as_str().unwrap().len(), 64);
    // Asked again with the same token, it is the same, with the same job.
    let (status, repeated) = send(client, "POST", "/v1/resources", asked.clone()).await;
    assert_eq!(status, 202, "{repeated}");
    assert_eq!(
        (&repeated["resource"]["id"], &repeated["job"]["id"]),
        (&json!(id), &created["job"]["id"])
    );
    let mut other = asked.clone();
    other["token"] = json!("t-2");
    let (status, said) = send(client, "POST", "/v1/resources", other).await;
    assert_eq!(status, 409, "{said}");

    // Its actions: the kind's own, run with its id and kind, and change.
    let path = format!("/v1/resources/{id}");
    let (_, actions) = send(client, "GET", &format!("{path}/actions"), Value::Null).await;
    let names: Vec<&str> = actions
        .as_array()
        .unwrap()
        .iter()
        .map(|action| action["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["change", "services", "restart", "remove"]);
    std::fs::write(
        fake.join("answer.act"),
        r#"{"message": "1 service", "details": [{"label": "wiki", "value": "running"}]}"#,
    )
    .unwrap();
    let (status, acted) = send(
        client,
        "POST",
        &format!("{path}/actions/services"),
        Value::Null,
    )
    .await;
    assert_eq!(status, 200, "{acted}");
    assert_eq!(acted["result"]["message"], "1 service");
    let request: Value =
        serde_json::from_str(&std::fs::read_to_string(fake.join("last.act")).unwrap()).unwrap();
    assert_eq!(
        (&request["id"], &request["kind"], &request["action"]["name"]),
        (&json!("wiki"), &json!("deployment"), &json!("services"))
    );
    assert_eq!(request["spec"], json!(spec));
    let changed = "services:\n  wiki:\n    image: wiki:2\n";
    let (status, acted) = send(
        client,
        "PUT",
        &path,
        json!({"spec": changed, "arguments": {"replicas": "2"}, "token": "c-1"}),
    )
    .await;
    assert_eq!(status, 202, "{acted}");
    let (output, code) = job_ended(&served.socket, acted["job"]["id"].as_str().unwrap()).await;
    assert_eq!(code, 0, "{output}");
    let request: Value =
        serde_json::from_str(&std::fs::read_to_string(fake.join("last.change")).unwrap()).unwrap();
    assert_eq!(
        (&request["spec"], &request["arguments"]["replicas"]),
        (&json!(changed), &json!("2"))
    );
    let kept = store
        .read(berth_core::store::kind::SPEC, berth_core::store::OWN, &id)
        .unwrap()
        .unwrap();
    assert_eq!(kept, changed.as_bytes());
    let (_, again) = send(
        client,
        "PUT",
        &path,
        json!({"spec": changed, "token": "c-1"}),
    )
    .await;
    assert_eq!(again["job"]["id"], acted["job"]["id"]);

    // Its log, with the secret masked.
    std::fs::write(fake.join("answer.logs"), "up\nusing db-pa55word-made-up\n").unwrap();
    let request =
        format!("GET {path}/logs?tail=5 HTTP/1.1\r\nHost: berth\r\nConnection: close\r\n\r\n");
    let mut stream = tokio::net::UnixStream::connect(&served.socket)
        .await
        .unwrap();
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut said = String::new();
    stream.read_to_string(&mut said).await.unwrap();
    assert!(
        said.contains("200 OK") && said.contains("using [secret]"),
        "{said}"
    );
    assert!(!said.contains("db-pa55word"), "{said}");

    // And as a stream over the portal, which ends with how it ended.
    let portal = berth_api::portal::Portal::over(
        client::websocket_unix(&served.socket, "/v1/portal")
            .await
            .unwrap(),
    );
    let mut stream = portal
        .open("resources.follow", None, json!({"id": id, "tail": 5}))
        .await
        .unwrap();
    let (mut lines, mut ended) = (String::new(), None);
    while let Some(piece) = tokio::time::timeout(Duration::from_secs(20), stream.recv())
        .await
        .unwrap()
    {
        match piece.unwrap() {
            berth_api::portal::Piece::Binary(data) => {
                lines.push_str(&String::from_utf8_lossy(&data))
            }
            berth_api::portal::Piece::Text(said) => ended = Some(said),
        }
    }
    assert!(
        lines.contains("using [secret]") && !lines.contains("db-pa55word"),
        "{lines}"
    );
    assert_eq!(ended.unwrap()["exited"]["exit_code"], 0);
    let request: Value =
        serde_json::from_str(&std::fs::read_to_string(fake.join("last.logs")).unwrap()).unwrap();
    assert_eq!(
        (&request["tail"], &request["follow"]),
        (&json!(5), &json!(true))
    );

    // Removing it forgets it with its spec.
    let (status, acted) = send(client, "DELETE", &path, Value::Null).await;
    assert_eq!(status, 202, "{acted}");
    let (output, code) = job_ended(&served.socket, acted["job"]["id"].as_str().unwrap()).await;
    assert_eq!(code, 0, "{output}");
    assert!(output.contains("Removed the deployment wiki"), "{output}");
    assert!(
        store
            .read(berth_core::store::kind::SPEC, berth_core::store::OWN, &id)
            .unwrap()
            .is_none()
    );
}

// ── The setup key ───────────────────────────────────────────────────────────

/// A request to the setup key's route with a bearer, or none.
async fn setup_key(
    address: std::net::SocketAddr,
    method: &str,
    bearer: Option<&str>,
) -> (u16, Value) {
    let stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let mut client = Client::over(stream).await.unwrap();
    let authorization = bearer.map(|bearer| format!("Bearer {bearer}"));
    let headers: Vec<(&str, &str)> = authorization
        .iter()
        .map(|value| ("authorization", value.as_str()))
        .collect();
    let answer = client
        .send(method, "/v1/setup-key", &headers, Default::default())
        .await
        .unwrap();
    let said = serde_json::from_slice(&answer.body).unwrap_or(Value::Null);
    (answer.status, said)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_setup_key_hands_over_the_token_until_it_is_done() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path(), Some("s3cret-token"));
    let paths = state.paths.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = api::router(state.clone(), true);
    tokio::spawn(async move { axum::serve(listener, router).await });

    // Without a key there is nothing to get in with, and every refusal looks the same.
    let (status, nothing) = setup_key(address, "GET", Some("0".repeat(64).as_str())).await;
    assert_eq!(status, 401);
    let (key, _) = super::setup_key::make(&paths).unwrap();
    assert!(key.len() >= 64);
    // It is in the store, and nowhere else.
    let store = &state.store;
    let row = |store: &Store| {
        store
            .read(kind::TOKEN, store::OWN, super::setup_key::NAME)
            .unwrap()
    };
    assert!(row(store).is_some());
    assert!(!paths.data_dir.join("serve.setup-key").exists());
    for bearer in [None, Some("wrong"), Some("s3cret-token")] {
        let (status, said) = setup_key(address, "GET", bearer).await;
        assert_eq!((status, &said), (401, &nothing), "{bearer:?}");
    }
    // Nor is the key a token for anything else.
    let stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let mut client = Client::over(stream).await.unwrap();
    let bearer = format!("Bearer {key}");
    let hosts = client
        .send(
            "GET",
            "/v1/hosts",
            &[("authorization", bearer.as_str())],
            Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(hosts.status, 401);

    let (status, checked) = setup_key(address, "GET", Some(&key)).await;
    assert_eq!(status, 200);
    assert_eq!(checked["version"], berth_api::VERSION);
    assert!(checked["expires"].as_str().unwrap().ends_with('Z'));
    // Collecting may be repeated, as after a reload of the page.
    for _ in 0..2 {
        let (status, claimed) = setup_key(address, "POST", Some(&key)).await;
        assert_eq!(
            (status, claimed["token"].as_str()),
            (200, Some("s3cret-token"))
        );
    }
    // Done ends it.
    assert_eq!(setup_key(address, "DELETE", Some(&key)).await.0, 200);
    assert!(row(store).is_none());
    for method in ["GET", "POST", "DELETE"] {
        assert_eq!(setup_key(address, method, Some(&key)).await.0, 401);
    }

    // A key that has expired is refused and goes.
    let old = "a".repeat(64);
    let expired = json!({"key": old, "expires": 1}).to_string();
    store
        .write(
            kind::TOKEN,
            store::OWN,
            super::setup_key::NAME,
            expired.as_bytes(),
        )
        .unwrap();
    assert_eq!(setup_key(address, "POST", Some(&old)).await.0, 401);
    assert!(row(store).is_none());

    // A new key takes the place of the one before.
    let (first, _) = super::setup_key::make(&paths).unwrap();
    let (second, _) = super::setup_key::make(&paths).unwrap();
    assert_eq!(setup_key(address, "GET", Some(&first)).await.0, 401);
    assert_eq!(setup_key(address, "GET", Some(&second)).await.0, 200);
    assert!(super::setup_key::remove(store).unwrap());
    assert_eq!(setup_key(address, "GET", Some(&second)).await.0, 401);

    // The portal does not carry these: they are for a caller without a token.
    let op = berth_api::ops::find(berth_api::ops::SERVE, "setup_key.claim").unwrap();
    let auth = super::auth::Auth::token(Some("s3cret-token".into()));
    assert!(auth.allows(&super::auth::Access::local(), op).is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn berth_connect_saves_the_server_and_its_token() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path(), Some("s3cret-token"));
    let served = state.paths.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = api::router(state.clone(), true);
    tokio::spawn(async move { axum::serve(listener, router).await });
    let (key, _) = super::setup_key::make(&served).unwrap();

    let client = tempfile::tempdir().unwrap();
    let paths = Paths {
        config_dir: client.path().join("config"),
        data_dir: client.path().join("data"),
    };
    let connect = |key: String| {
        let (dir, url) = (paths.config_dir.clone(), url.clone());
        tokio::task::spawn_blocking(move || {
            let settings = berth_page::settings::Settings::new(Some(dir));
            crate::cli::login::claim(&settings, &url, &key)
                .map(drop)
                .map_err(|error| error.message)
        })
    };
    // A wrong key saves nothing.
    let refused = connect("wrong".into()).await.unwrap().unwrap_err();
    assert!(refused.contains("did not take the setup key"), "{refused}");
    assert_eq!(paths.setting("server"), None);

    connect(key.clone()).await.unwrap().unwrap();
    assert_eq!(paths.setting("server").as_deref(), Some(url.as_str()));
    assert_eq!(
        paths.setting("server_token").as_deref(),
        Some("s3cret-token")
    );
    // The key was ended once the token was saved.
    let row = state
        .store
        .read(kind::TOKEN, store::OWN, super::setup_key::NAME);
    assert!(row.unwrap().is_none());
    assert!(connect(key).await.unwrap().is_err());
}

#[test]
fn a_setup_key_is_refused_when_the_token_is_off() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths {
        config_dir: dir.path().join("config"),
        data_dir: dir.path().join("data"),
    };
    paths.set("serve_token", "off").unwrap();
    let refused = super::setup_key::make(&paths).unwrap_err().to_string();
    assert!(refused.contains("serve_token is off"), "{refused}");
    let store = store::shared(&paths).unwrap();
    let row = store.read(kind::TOKEN, store::OWN, super::setup_key::NAME);
    assert!(row.unwrap().is_none());
    assert!(super::setup_key::command(&paths, &["--nothing"]).is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_is_renamed_as_a_job_with_what_the_server_keeps_of_it() {
    let served = served().await;
    let mut client = served.client().await;
    let client = &mut client;
    let paths = &served.state.paths;
    let profile = json!({"hostname": "test", "builder": {}, "access": {"address": "192.0.2.7", "link": ["sh", "-c", "exit 1"]}});
    let test = berth_core::profiles::Place::own("test").unwrap();
    test.save(paths, &profile).unwrap();
    berth_core::profiles::Place::own("lab")
        .unwrap()
        .save(paths, &profile)
        .unwrap();
    let message = berth_api::Message {
        id: 1,
        topic: "agent/done".into(),
        body: json!({}),
        keep: berth_api::Keep::Log,
        from: berth_api::From {
            level: berth_api::Level::Agent,
            uid: 1001,
            session: None,
        },
        time: berth_api::rfc3339(berth_api::unix_now()),
    };
    served.state.store.take_in("test", &message).unwrap();
    let (status, listed) = send(client, "GET", "/v1/hosts/test/actions", Value::Null).await;
    assert_eq!(status, 200, "{listed}");
    let rename = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|action| action["name"] == "rename")
        .unwrap()
        .clone();
    assert_eq!(
        (&rename["available"], &rename["role"]),
        (&json!(true), &json!("admin"))
    );

    // A name that is taken, or that is no name, is refused before a job starts.
    let act =
        |name: &str, hostname: &str| json!({"arguments": {"name": name, "hostname": hostname}});
    let path = "/v1/hosts/test/actions/rename";
    let (status, said) = send(client, "POST", path, act("lab", "no")).await;
    assert_eq!(status, 409, "{said}");
    let (status, said) = send(client, "POST", path, act("a b", "no")).await;
    assert_eq!(status, 400, "{said}");
    let (status, said) = send(client, "POST", path, act("Studio_1", "yes")).await;
    assert_eq!(status, 400, "{said}");
    let (status, said) = send(client, "POST", path, json!({})).await;
    assert_eq!(status, 400, "{said}");

    let (status, acted) = send(client, "POST", path, act("studio", "yes")).await;
    assert_eq!(status, 202, "{acted}");
    let id = acted["job"]["id"].as_str().unwrap().to_owned();
    let job = job_done(client, &id).await;
    assert_eq!(job["exit_code"], 0, "{job}");
    // The job is the renamed host's now, as its messages are.
    assert_eq!(job["subject"], "host:studio");
    // The server knows it by its new name without being asked for its hosts.
    assert!(served.state.host("studio").is_some());
    assert!(served.state.host("test").is_none());
    let (status, _) = send(client, "GET", "/v1/hosts/test", Value::Null).await;
    assert_eq!(status, 404);
    let (status, host) = send(client, "GET", "/v1/hosts/studio", Value::Null).await;
    assert_eq!(status, 200, "{host}");
    let (_, saved) = send(client, "GET", "/v1/hosts/studio/profile", Value::Null).await;
    assert_eq!(saved["hostname"], "studio");
    assert_eq!(saved["access"]["address"], "192.0.2.7");
    let (_, inbox) = send(client, "GET", "/v1/inbox?host=studio", Value::Null).await;
    assert_eq!(inbox["messages"][0]["topic"], "agent/done", "{inbox}");
}

/// The frames of an events stream up to the first of `source` with `change`, which is returned.
async fn change_of<S, E>(stream: &mut S, source: &str, change: &str) -> Value
where
    S: StreamExt<Item = Result<Frame, E>> + Unpin,
    E: std::fmt::Debug,
{
    let mut seen = vec![];
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(20), text(stream))
            .await
            .unwrap_or_else(|_| panic!("no {source} {change} after {seen:?}"));
        if frame["source"] == source && frame["change"] == change {
            return frame;
        }
        seen.push(frame);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn hosts_and_resources_that_change_are_told_on_the_events_stream() {
    let served = served_with(quick).await;
    let fake = fake_provisioner(&served);
    let mut client = served.client().await;
    let client = &mut client;
    register_fake(client).await;
    let paths = &served.state.paths;
    paths.prepare().unwrap();
    let mut events = client::websocket_unix(&served.socket, "/v1/events")
        .await
        .unwrap();
    let follow = json!({"changes": ["hosts", "resources"]}).to_string();
    events.send(Frame::Text(follow.into())).await.unwrap();

    // Saved by another berth beside the server: told when the server looks.
    let profile = json!({"hostname": "studio", "builder": {}, "access": {"address": "192.0.2.7", "link": ["sh", "-c", "exit 1"]}});
    berth_core::profiles::Place::own("studio")
        .unwrap()
        .save(paths, &profile)
        .unwrap();
    served.state.notice();
    assert_eq!(
        change_of(&mut events, "host", "added").await,
        json!({"source": "host", "host": "studio", "change": "added"})
    );
    let mut moved = profile.clone();
    moved["access"]["address"] = json!("192.0.2.8");
    let (status, said) = send(client, "PUT", "/v1/hosts/studio/profile", moved).await;
    assert_eq!(status, 200, "{said}");
    assert_eq!(
        change_of(&mut events, "host", "changed").await["host"],
        "studio"
    );
    let rename = json!({"arguments": {"name": "dock", "hostname": "no"}});
    let (status, acted) = send(client, "POST", "/v1/hosts/studio/actions/rename", rename).await;
    assert_eq!(status, 202, "{acted}");
    assert_eq!(
        change_of(&mut events, "host", "renamed").await,
        json!({"source": "host", "host": "dock", "change": "renamed", "from": "studio"})
    );
    job_done(client, acted["job"]["id"].as_str().unwrap()).await;
    let (status, said) = send(client, "DELETE", "/v1/hosts/dock", Value::Null).await;
    assert_eq!(status, 200, "{said}");
    assert_eq!(
        change_of(&mut events, "host", "removed").await["host"],
        "dock"
    );

    send(
        client,
        "PUT",
        "/v1/secrets/api_token",
        json!({"value": "tok-made-up"}),
    )
    .await;
    std::fs::write(
        fake.join("answer.create"),
        r#"{"id": "wiki", "state": "running"}"#,
    )
    .unwrap();
    let asked = json!({"provisioner": "vm", "kind": "deployment", "name": "wiki",
                       "arguments": {"server": "x"}, "spec": "services:\n  wiki:\n    image: wiki\n",
                       "token": "t-1"});
    let (status, created) = send(client, "POST", "/v1/resources", asked).await;
    assert_eq!(status, 202, "{created}");
    let id = created["resource"]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        change_of(&mut events, "resource", "created").await,
        json!({"source": "resource", "resource": id, "provisioner": "vm", "kind": "deployment", "change": "created"})
    );
    let (_, code) = job_ended(&served.socket, created["job"]["id"].as_str().unwrap()).await;
    assert_eq!(code, 0);
    // Made, it has the provisioner's id now.
    assert_eq!(
        change_of(&mut events, "resource", "changed").await["resource"],
        json!(id)
    );
    let (status, removed) = send(
        client,
        "DELETE",
        &format!("/v1/resources/{id}"),
        Value::Null,
    )
    .await;
    assert_eq!(status, 202, "{removed}");
    assert_eq!(
        change_of(&mut events, "resource", "removed").await["resource"],
        json!(id)
    );

    // Not asked for, nothing of the kind is told.
    let mut quiet = client::websocket_unix(&served.socket, "/v1/events")
        .await
        .unwrap();
    let follow = json!({"kv": ["ui"]}).to_string();
    quiet.send(Frame::Text(follow.into())).await.unwrap();
    berth_core::profiles::Place::own("pier")
        .unwrap()
        .save(paths, &profile)
        .unwrap();
    served.state.notice();
    put(&served.socket, &[], "1").await;
    assert_eq!(text(&mut quiet).await["source"], "kv");
    // A kind it does not know is refused.
    let mut wrong = client::websocket_unix(&served.socket, "/v1/events")
        .await
        .unwrap();
    let follow = json!({"changes": ["plugins"]}).to_string();
    wrong.send(Frame::Text(follow.into())).await.unwrap();
    assert_eq!(text(&mut wrong).await["error"]["code"], "bad_request");
}

// Not on a runtime of the test's: the screen waits for the server on one of its own.
#[test]
fn generated_passwords_are_kept_where_the_host_is_and_shown_to_an_admin() {
    use berth_core::{credentials, profiles::Place};
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let served = runtime.block_on(served());
    let ask = |method: &str, path: &str, body: Value| {
        runtime.block_on(async {
            let mut client = served.client().await;
            send(&mut client, method, path, body).await
        })
    };
    let profile = json!({"hostname": "studio", "builder": {}, "access": {"address": "192.0.2.7"}});
    let (status, _) = ask("PUT", "/v1/hosts/studio/profile", profile.clone());
    assert_eq!(status, 200);

    // The server's route keeps what it makes up in the server's store.
    let asked = json!({"what": "password", "host": "studio", "user": "ops"});
    let (status, made) = ask("POST", "/v1/setup/generate", asked);
    assert_eq!(status, 200, "{made}");
    let password = made["password"].as_str().unwrap().to_owned();
    assert!(made["value"].as_str().unwrap().starts_with("$6$"));
    assert!(
        made["note"]
            .as_str()
            .unwrap()
            .contains("berth host credentials studio"),
        "{made}"
    );
    let (status, kept) = ask("GET", "/v1/hosts/studio/credentials", Value::Null);
    assert_eq!(
        (status, &kept["credentials"]["ops"]),
        (200, &json!(password))
    );
    let own = Place::own("studio").unwrap();
    assert_eq!(
        credentials::load(&served.state.paths, &own).unwrap()["ops"],
        json!(password)
    );
    let (status, _) = ask("GET", "/v1/hosts/attic/credentials", Value::Null);
    assert_eq!(status, 404);

    runtime.shutdown_background();
}

#[test]
fn pruning_removes_a_jobs_output_and_its_report() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path(), None);
    std::fs::create_dir_all(&state.jobs_dir).unwrap();
    let log = state.jobs_dir.join("jold.log");
    std::fs::write(&log, "done\n").unwrap();
    std::fs::write(log.with_extension("json"), "{}").unwrap();
    // A row of a shared store that names a file elsewhere: the row goes,
    // the file stays.
    let elsewhere = dir.path().join("precious.log");
    std::fs::write(&elsewhere, "kept\n").unwrap();
    std::fs::write(elsewhere.with_extension("json"), "{}").unwrap();
    let long_ago = berth_api::rfc3339(berth_api::unix_now() - 200 * 86400);
    for (id, output) in [("jold", &log), ("jelsewhere", &elsewhere)] {
        state
            .store
            .add_job(&Job {
                id: id.into(),
                host: "studio".into(),
                what: "rollout".into(),
                started: long_ago.clone(),
                ended: Some(long_ago.clone()),
                exit_code: Some(0),
                output_path: output.to_string_lossy().into_owned(),
                ..Job::default()
            })
            .unwrap();
    }
    state.prune();
    assert!(state.store.job("jold").unwrap().is_none());
    assert!(state.store.job("jelsewhere").unwrap().is_none());
    assert!(!log.exists() && !log.with_extension("json").exists());
    assert!(elsewhere.exists() && elsewhere.with_extension("json").exists());
}

/// The routes of the key ring, with the helpers of these tests.
mod keys;

/// The screens signed in to a server of the tests over the network.
mod remote;

#[tokio::test(flavor = "multi_thread")]
async fn a_server_in_the_process_names_both_ways_to_give_a_sudo_password() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = state_in(dir.path(), None);
    Arc::get_mut(&mut state).unwrap().here = true;
    let profile = json!({"builder": {}, "access": {"address": "192.0.2.9", "user": "ops", "sudo": "password"}});
    berth_core::profiles::Place::own("studio")
        .unwrap()
        .save(&state.paths, &profile)
        .unwrap();
    state.refresh(false);
    let problem = state
        .host("studio")
        .unwrap()
        .problem
        .clone()
        .unwrap_or_default();
    assert!(problem.contains("--sudo-password-stdin"), "{problem}");
    assert!(
        problem.contains("berth secret set sudo/studio"),
        "{problem}"
    );
    // Given one for the run, the host is taken up again with it.
    state
        .passwords
        .lock()
        .unwrap()
        .insert("studio".into(), "pass-word-1".into());
    state.forget("studio");
    state.refresh(false);
    let remote = state.host("studio").unwrap();
    assert!(
        !remote.problem.clone().unwrap_or_default().contains("sudo"),
        "{:?}",
        remote.problem
    );
    // berth serve says what it says to its own user.
    let other = tempfile::tempdir().unwrap();
    let served = state_in(other.path(), None);
    berth_core::profiles::Place::own("studio")
        .unwrap()
        .save(&served.paths, &profile)
        .unwrap();
    served.refresh(false);
    let problem = served
        .host("studio")
        .unwrap()
        .problem
        .clone()
        .unwrap_or_default();
    assert!(
        problem.contains("run berth serve on a terminal"),
        "{problem}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_caller_on_this_machine_takes_the_host_key_its_user_knows() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path(), Some("s3cret-token"));
    let known_hosts = dir.path().join("known_hosts");
    std::fs::write(&known_hosts, "192.0.2.8 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIEdockdockdockdockdockdockdockdockdockdockdo\n").unwrap();
    *state.known_hosts.lock().unwrap() = Some(known_hosts);
    for name in ["dock", "studio"] {
        berth_core::profiles::Place::own(name)
            .unwrap()
            .save(
                &state.paths,
                &json!({"builder": {}, "access": {"address": "192.0.2.8"}}),
            )
            .unwrap();
    }
    // Over the network nothing of the server's user's files is taken.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = api::router(state.clone(), true);
    tokio::spawn(async move { axum::serve(listener, router).await });
    let mut far = Client::over(tokio::net::TcpStream::connect(address).await.unwrap())
        .await
        .unwrap();
    let answer = far
        .send(
            "POST",
            "/v1/hosts/dock/actions/host-key",
            &[("authorization", "Bearer s3cret-token")],
            Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(answer.status, 200);
    let acted: Value = serde_json::from_slice(&answer.body).unwrap();
    assert_eq!(acted["result"]["host_keys"], json!([]), "{acted}");
    // On the socket the key that file has for the address is the host's.
    let socket = dir.path().join("serve.sock");
    let unix = tokio::net::UnixListener::bind(&socket).unwrap();
    let router = api::router(state.clone(), false);
    tokio::spawn(async move { axum::serve(unix, router).await });
    let mut client = Client::unix(&socket).await.unwrap();
    let (status, acted) = send(
        &mut client,
        "POST",
        "/v1/hosts/studio/actions/host-key",
        Value::Null,
    )
    .await;
    assert_eq!(status, 200, "{acted}");
    let keys = acted["result"]["host_keys"].as_array().unwrap();
    assert_eq!(keys.len(), 1, "{acted}");
    assert_eq!(keys[0]["type"], "ssh-ed25519");
    assert!(
        keys[0]["fingerprint"]
            .as_str()
            .unwrap()
            .starts_with("SHA256:"),
        "{acted}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn first_contact_and_keys_are_actions_of_the_server() {
    let served = served().await;
    let mut client = served.client().await;
    let client = &mut client;
    // A host that is reached by a command that fails at once, so that what
    // needs no controller is told apart from what does, without waiting.
    let profile = json!({"builder": {}, "access": {"address": "192.0.2.7", "auth": "password", "link": ["sh", "-c", "exit 1"]}});
    berth_core::profiles::Place::own("studio")
        .unwrap()
        .save(&served.state.paths, &profile)
        .unwrap();
    let (status, listed) = send(client, "GET", "/v1/hosts/studio/actions", Value::Null).await;
    assert_eq!(status, 200, "{listed}");
    let action = |listed: &Value, name: &str| {
        listed
            .as_array()
            .unwrap()
            .iter()
            .find(|action| action["name"] == name)
            .unwrap_or_else(|| panic!("no {name} in {listed}"))
            .clone()
    };
    // What makes a host reachable is there although its controller is not.
    let trust = action(&listed, "trust-key");
    assert_eq!(
        (&trust["available"], &trust["role"]),
        (&json!(true), &json!("admin"))
    );
    assert_eq!(trust["arguments"][0]["name"], "password");
    assert_eq!(trust["arguments"][0]["secret"], true);
    assert!(trust["arguments"][1].get("secret").is_none());
    assert_eq!(action(&listed, "host-key")["available"], true);
    // A host that logs in with a password a job cannot type is not applied,
    // until the server keeps the password.
    let apply = action(&listed, "apply");
    assert_eq!(apply["available"], false);
    assert!(
        apply["why"].as_str().unwrap().contains("ssh/studio"),
        "{apply}"
    );
    let (status, said) = send(
        client,
        "POST",
        "/v1/hosts/studio/actions/apply",
        Value::Null,
    )
    .await;
    assert_eq!(status, 501, "{said}");
    let value = json!({"value": "made-up-ssh-pw"});
    assert_eq!(
        send(client, "PUT", "/v1/secrets/ssh%2Fstudio", value)
            .await
            .0,
        200
    );
    let (_, listed) = send(client, "GET", "/v1/hosts/studio/actions", Value::Null).await;
    assert_eq!(action(&listed, "apply")["available"], true);

    // First contact is a job; the password it is given is in neither the job
    // nor its output.
    let asked = json!({"arguments": {"password": "made-up-once-pw", "keep_password": "no"}});
    let (status, acted) = send(client, "POST", "/v1/hosts/studio/actions/trust-key", asked).await;
    assert_eq!(status, 202, "{acted}");
    let id = acted["job"]["id"].as_str().unwrap().to_owned();
    let (output, code) = job_ended(&served.socket, &id).await;
    assert_eq!(code, 1, "{output}");
    assert!(
        output.contains("✗ studio is reached through its own command"),
        "{output}"
    );
    let (_, job) = send(client, "GET", &format!("/v1/jobs/{id}"), Value::Null).await;
    assert_eq!(
        (&job["subject"], &job["action"]),
        (&json!("host:studio"), &json!("trust-key"))
    );
    assert!(!job.to_string().contains("made-up-once-pw") && !output.contains("made-up-once-pw"));
    let both = json!({"arguments": {"password": "x", "key": "berth"}});
    let (status, _) = send(client, "POST", "/v1/hosts/studio/actions/trust-key", both).await;
    assert_eq!(status, 400);
    let nokey = json!({"arguments": {"key": "nothing"}});
    let (status, _) = send(client, "POST", "/v1/hosts/studio/actions/trust-key", nokey).await;
    assert_eq!(status, 404);

    // The host keys answer at once: none are recorded yet.
    let (status, acted) = send(
        client,
        "POST",
        "/v1/hosts/studio/actions/host-key",
        Value::Null,
    )
    .await;
    assert_eq!(status, 200, "{acted}");
    assert_eq!(acted["result"], json!({"host_keys": [], "accepted": false}));

    // The installation rotates a key as keys.rotate does.
    let (status, listed) = send(client, "GET", "/v1/installation/actions", Value::Null).await;
    assert_eq!(status, 200);
    assert_eq!(
        action(&listed, "rotate-key")["arguments"][0]["default"],
        "berth"
    );
    let nothing = json!({"arguments": {"name": "nothing"}});
    let (status, _) = send(
        client,
        "POST",
        "/v1/installation/actions/rotate-key",
        nothing,
    )
    .await;
    assert_eq!(status, 404);
    let asked = json!({"arguments": {"name": "berth"}, "token": "rotate-1"});
    let (status, acted) = send(
        client,
        "POST",
        "/v1/installation/actions/rotate-key",
        asked.clone(),
    )
    .await;
    assert_eq!(status, 202, "{acted}");
    assert_eq!(acted["job"]["action"], "rotate-key:berth");
    let id = acted["job"]["id"].as_str().unwrap().to_owned();
    let (output, _) = job_ended(&served.socket, &id).await;
    assert!(output.contains("Replacing the key berth"), "{output}");
    let (_, again) = send(client, "POST", "/v1/installation/actions/rotate-key", asked).await;
    assert_eq!(again["job"]["id"], json!(id));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_added_by_its_address_has_a_type_and_actions_to_match() {
    let served = served().await;
    let mut client = served.client().await;
    let client = &mut client;
    // Its type and how it is reached are all a host that is not a workstation needs.
    let ssh = json!({"type": "ssh", "access": {"address": "192.0.2.8", "user": "ops"}});
    let (status, said) = send(client, "PUT", "/v1/hosts/dock/profile", ssh).await;
    assert_eq!(status, 200, "{said}");
    for (refused, why) in [
        (
            json!({"type": "server", "access": {"address": "192.0.2.9"}}),
            "type",
        ),
        (json!({"type": "managed"}), "access.address"),
        // The API takes the names of now only.
        (
            json!({"level": "reachable", "access": {"address": "192.0.2.9"}}),
            "builder",
        ),
        (json!({"access": {"address": "192.0.2.9"}}), "builder"),
    ] {
        let (status, said) = send(client, "PUT", "/v1/hosts/pier/profile", refused).await;
        assert_eq!(status, 400, "{said}");
        assert!(
            said["error"]["message"].as_str().unwrap().contains(why),
            "{said}"
        );
    }
    let (_, dock) = send(client, "GET", "/v1/hosts/dock", Value::Null).await;
    assert_eq!(dock["type"], "ssh", "{dock}");

    // berth's own actions say what the type allows, and how to change it.
    let (_, actions) = send(client, "GET", "/v1/hosts/dock/actions", Value::Null).await;
    let find = |name: &str| {
        actions
            .as_array()
            .unwrap()
            .iter()
            .find(|action| action["name"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("no {name} in {actions}"))
    };
    let install = find("install-controller");
    assert_eq!(
        (&install["available"], &install["label"]),
        (&json!(true), &json!("Install the controller"))
    );
    for (name, why) in [
        ("apply", "neither a node nor a workstation"),
        ("seed", "not a workstation"),
        ("secrets", "berth run dock install-controller"),
    ] {
        let action = find(name);
        assert_eq!(action["available"], false, "{action}");
        assert!(action["why"].as_str().unwrap().contains(why), "{action}");
    }
    // Asked anyway, it is refused with the reason, before any job.
    let (status, said) = send(client, "POST", "/v1/hosts/dock/actions/apply", Value::Null).await;
    assert_eq!(
        (status, &said["error"]["code"]),
        (501, &json!("not_available"))
    );
    assert!(
        said["error"]["message"]
            .as_str()
            .unwrap()
            .contains("or a workstation in its set-up")
    );
    let (status, _) = send(client, "POST", "/v1/hosts/dock/actions/seed", Value::Null).await;
    assert_eq!(status, 501);

    // A roll-out leaves it, and says so.
    let (status, acted) = send(
        client,
        "POST",
        "/v1/installation/actions/rollout",
        Value::Null,
    )
    .await;
    assert_eq!(status, 202, "{acted}");
    let id = acted["job"]["id"].as_str().unwrap().to_owned();
    let (output, code) = job_ended(&served.socket, &id).await;
    assert_eq!(code, 0, "{output}");
    assert!(
        output.contains("Skipped 1 host that is neither a node nor a workstation: dock"),
        "{output}"
    );

    // A managed host is offered an update of its controller, a
    // workstation none, as applying installs it.
    let mut profile = said_profile(client, "dock").await;
    profile["type"] = json!("managed");
    assert_eq!(
        send(client, "PUT", "/v1/hosts/dock/profile", profile.clone())
            .await
            .0,
        200
    );
    let (_, actions) = send(client, "GET", "/v1/hosts/dock/actions", Value::Null).await;
    let install = actions
        .as_array()
        .unwrap()
        .iter()
        .find(|action| action["name"] == "install-controller")
        .cloned()
        .unwrap();
    assert_eq!(install["label"], "Update the controller");
    profile.as_object_mut().unwrap().remove("type");
    profile["builder"] = json!({});
    assert_eq!(
        send(client, "PUT", "/v1/hosts/dock/profile", profile)
            .await
            .0,
        200
    );
    let (status, said) = send(
        client,
        "POST",
        "/v1/hosts/dock/actions/install-controller",
        Value::Null,
    )
    .await;
    assert_eq!(status, 501, "{said}");
    assert!(
        said["error"]["message"]
            .as_str()
            .unwrap()
            .contains("berth apply dock")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_seen_to_be_a_workstation_never_gets_the_controller_alone() {
    let Some(host) = Berthd::start().await else {
        return;
    };
    // The host's record is a workstation's, whatever its profile here says.
    std::fs::write(
        host.dir.path().join("etc/applied.json"),
        json!({"berth": "0.5.0", "plugins": []}).to_string(),
    )
    .unwrap();
    let served = served().await;
    berth_core::profiles::Place::own("box")
        .unwrap()
        .save(
            &served.state.paths,
            &json!({"type": "ssh", "access": {"address": "198.51.100.1", "user": "ops"}}),
        )
        .unwrap();
    served
        .state
        .add(Remote::new("box", "198.51.100.1", host.link()));
    let client = &mut served.client().await;
    let (_, actions) = send(client, "GET", "/v1/hosts/box/actions", Value::Null).await;
    let install = actions
        .as_array()
        .unwrap()
        .iter()
        .find(|action| action["name"] == "install-controller")
        .cloned()
        .unwrap();
    assert_eq!(install["available"], false, "{install}");
    let why = install["why"].as_str().unwrap();
    assert!(
        why.starts_with("box is a workstation; its profile here says ssh")
            && why.contains("berth host adopt box"),
        "{why}"
    );
    let (status, said) = send(
        client,
        "POST",
        "/v1/hosts/box/actions/install-controller",
        Value::Null,
    )
    .await;
    assert_eq!(status, 501, "{said}");
    assert!(served.state.store.jobs(10).unwrap().is_empty());
}

async fn said_profile(client: &mut Client, name: &str) -> Value {
    send(
        client,
        "GET",
        &format!("/v1/hosts/{name}/profile"),
        Value::Null,
    )
    .await
    .1
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_whose_controller_does_not_answer_says_why_and_keeps_berths_own_actions() {
    let served = served().await;
    // A link that fails as ssh does for a host it cannot reach.
    let said = "ssh: connect to host 192.0.2.11 port 22: Network is unreachable";
    let profile = json!({"type": "managed", "access": {"address": "192.0.2.11",
        "link": ["sh", "-c", format!("echo '{said}' >&2; exit 255")]}});
    berth_core::profiles::Place::own("dock")
        .unwrap()
        .save(&served.state.paths, &profile)
        .unwrap();
    served.state.refresh(false);
    let mut client = served.client().await;
    let client = &mut client;

    // What the host's own API is asked for says what the link command said,
    // even when several are asked at once.
    let asks = (0..4).map(|_| {
        let socket = served.socket.clone();
        async move {
            let mut client = Client::unix(&socket).await.unwrap();
            send(
                &mut client,
                "GET",
                "/v1/hosts/dock/controller/version",
                Value::Null,
            )
            .await
        }
    });
    for (status, answer) in futures_util::future::join_all(asks).await {
        assert_eq!(status, 502, "{answer}");
        assert!(
            answer["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Network is unreachable"),
            "{answer}"
        );
    }

    // berth's own actions are listed; what goes through the controller is not
    // available, with the reason.
    let (status, actions) = send(client, "GET", "/v1/hosts/dock/actions", Value::Null).await;
    assert_eq!(status, 200, "{actions}");
    let find = |name: &str| {
        actions
            .as_array()
            .unwrap()
            .iter()
            .find(|action| action["name"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("no {name} in {actions}"))
    };
    let secrets = find("secrets");
    assert_eq!(secrets["available"], false, "{secrets}");
    assert!(
        secrets["why"]
            .as_str()
            .unwrap()
            .contains("Network is unreachable"),
        "{secrets}"
    );
    for name in ["install-controller", "trust-key", "host-key"] {
        assert_eq!(find(name)["available"], true, "{name}");
    }
    let (status, said) = send(
        client,
        "POST",
        "/v1/hosts/dock/actions/secrets",
        Value::Null,
    )
    .await;
    assert_eq!(
        (status, &said["error"]["code"]),
        (501, &json!("not_available")),
        "{said}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_removed_or_changed_in_the_store_by_another_process_is_not_listed_as_it_was() {
    let served = served().await;
    let mut client = served.client().await;
    let client = &mut client;
    let place = berth_core::profiles::Place::own("studio").unwrap();
    let paths = &served.state.paths;
    // A host whose machine is still being made has no address.
    place.save(paths, &json!({"builder": {}})).unwrap();
    let (_, hosts) = send(client, "GET", "/v1/hosts", Value::Null).await;
    assert_eq!(hosts[0]["address"], "", "{hosts}");

    // What `berth` beside the server saves is what the server lists next.
    let link = json!(["sh", "-c", "exit 255"]);
    place
        .save(
            paths,
            &json!({"builder": {}, "access": {"address": "192.0.2.40", "link": link}}),
        )
        .unwrap();
    let (_, hosts) = send(client, "GET", "/v1/hosts", Value::Null).await;
    assert_eq!(hosts[0]["address"], "192.0.2.40", "{hosts}");

    // `berth host remove` there takes it off the list, without a restart.
    berth_core::profiles::remove(paths, &place).unwrap();
    let (status, hosts) = send(client, "GET", "/v1/hosts", Value::Null).await;
    assert_eq!((status, hosts), (200, json!([])));
    let (status, _) = send(client, "GET", "/v1/hosts/studio", Value::Null).await;
    assert_eq!(status, 404);
}

/// What works with this machine's own drives and settings is offered to its
/// user, on the socket or in the process, and not over the network.
#[tokio::test(flavor = "multi_thread")]
async fn the_installer_and_every_setting_are_for_the_user_here() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path(), Some("s3cret-token"));
    let profile = json!({"hostname": "studio", "builder": {}, "access": {"address": "192.0.2.7", "link": ["sh", "-c", "exit 1"]}});
    berth_core::profiles::Place::own("studio")
        .unwrap()
        .save(&state.paths, &profile)
        .unwrap();
    let socket = dir.path().join("serve.sock");
    let unix = tokio::net::UnixListener::bind(&socket).unwrap();
    let local = api::router(state.clone(), false);
    tokio::spawn(async move { axum::serve(unix, local).await });
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = tcp.local_addr().unwrap();
    let guarded = api::router(state.clone(), true);
    tokio::spawn(async move { axum::serve(tcp, guarded).await });
    let installer = |listed: &Value| {
        listed
            .as_array()
            .unwrap()
            .iter()
            .find(|action| action["name"] == "installer")
            .cloned()
            .unwrap()
    };

    let mut client = Client::unix(&socket).await.unwrap();
    let (status, listed) = send(&mut client, "GET", "/v1/hosts/studio/actions", Value::Null).await;
    assert_eq!(status, 200, "{listed}");
    let here = installer(&listed);
    assert_eq!(here["available"], true, "{here}");
    assert_eq!(here["arguments"][1]["secret"], true);
    // Only a USB drive that is connected is written.
    let act = json!({"arguments": {"device": "/dev/berth-test-none"}});
    let (status, said) = send(
        &mut client,
        "POST",
        "/v1/hosts/studio/actions/installer",
        act,
    )
    .await;
    assert_eq!(status, 400, "{said}");
    assert!(
        said["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no USB drive")
    );

    let (status, listed) = as_bearer(
        address,
        "s3cret-token",
        "GET",
        "/v1/hosts/studio/actions",
        Value::Null,
    )
    .await;
    assert_eq!(status, 200, "{listed}");
    let elsewhere = installer(&listed);
    assert_eq!(elsewhere["available"], false);
    assert!(
        elsewhere["why"]
            .as_str()
            .unwrap()
            .contains("berth serve runs on"),
        "{elsewhere}"
    );
    let (status, _) = as_bearer(
        address,
        "s3cret-token",
        "POST",
        "/v1/hosts/studio/actions/installer",
        json!({}),
    )
    .await;
    assert_eq!(status, 501);

    // Settings of the machine itself, such as the ISO folder, change only here.
    let iso = json!({"value": "/srv/iso"});
    let (status, _) = as_bearer(
        address,
        "s3cret-token",
        "PUT",
        "/v1/settings/iso_dir",
        iso.clone(),
    )
    .await;
    assert_eq!(status, 403);
    let (status, said) = send(&mut client, "PUT", "/v1/settings/iso_dir", iso).await;
    assert_eq!(status, 200, "{said}");
    assert_eq!(state.paths.setting("iso_dir").as_deref(), Some("/srv/iso"));
    let (_, listed) = send(&mut client, "GET", "/v1/settings", Value::Null).await;
    let open = |listed: &Value| {
        listed
            .as_array()
            .unwrap()
            .iter()
            .find(|setting| setting["key"] == "iso_dir")
            .unwrap()["open"]
            .clone()
    };
    assert_eq!(open(&listed), true);
    let (_, listed) = as_bearer(address, "s3cret-token", "GET", "/v1/settings", Value::Null).await;
    assert_eq!(open(&listed), false);
}

// ── Endpoints and commands of resources ─────────────────────────────────────

/// A port of the loopback that nothing listens on.
fn closed_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// Make the fake's box `db`, whose `create` answers `answer`: its id.
async fn made_box(served: &Served, fake: &std::path::Path, answer: Value) -> String {
    let mut client = served.client().await;
    let client = &mut client;
    register_fake(client).await;
    served.state.paths.prepare().unwrap();
    send(
        client,
        "PUT",
        "/v1/secrets/api_token",
        json!({"value": "tok-made-up"}),
    )
    .await;
    std::fs::write(fake.join("answer.create"), answer.to_string()).unwrap();
    let asked =
        json!({"provisioner": "vm", "kind": "box", "name": "db", "arguments": {"server": "x"}});
    let (status, created) = send(client, "POST", "/v1/resources", asked).await;
    assert_eq!(status, 202, "{created}");
    let (output, code) = job_ended(&served.socket, created["job"]["id"].as_str().unwrap()).await;
    assert_eq!(code, 0, "{output}");
    created["resource"]["id"].as_str().unwrap().to_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resource_is_reached_at_the_endpoints_its_provisioner_reports() {
    use berth_api::{portal::Portal, tunnel::endpoint_tunnel};
    let served = served_with(quick).await;
    let fake = fake_provisioner(&served);
    let (port, closed) = (echo().await, closed_port());
    let endpoints = |port: u16| {
        json!([
            {"name": "db", "address": "127.0.0.1", "port": port, "hint": "postgres", "local": true},
            {"name": "closed", "address": "127.0.0.1", "port": closed, "local": true},
            {"name": "far", "address": "127.0.0.1", "port": 6000},
            {"name": "stats", "address": "192.0.2.20", "port": 8125, "protocol": "udp"},
            {"name": "Not a name", "address": "192.0.2.20", "port": 1},
        ])
    };
    let answer = json!({"id": "db-1", "state": "running", "endpoints": endpoints(port)});
    let id = made_box(&served, &fake, answer).await;
    let mut client = served.client().await;

    // Kept with the record, without the one that does not fit.
    let listed: Value = client.get("/v1/resources").await.unwrap();
    let kept = &listed[0]["endpoints"];
    assert_eq!(kept.as_array().unwrap().len(), 4, "{kept}");
    assert_eq!(
        kept[0],
        json!({"name": "db", "address": "127.0.0.1", "port": port, "protocol": "tcp", "hint": "postgres", "local": true})
    );
    assert_eq!(kept[3]["protocol"], "udp");

    // Straight to the server, by name and by port, both ways and with each side's end.
    let open = |endpoint: String| {
        let (socket, id) = (served.socket.clone(), id.clone());
        async move {
            let io = tokio::net::UnixStream::connect(&socket).await.unwrap();
            endpoint_tunnel(io, &id, &endpoint, "").await
        }
    };
    let data: Vec<u8> = (0..3_000_000u32).map(|n| (n % 251) as u8).collect();
    assert_eq!(
        through(open("db".into()).await.unwrap(), b"hello").await,
        b"hello"
    );
    assert!(through(open(port.to_string()).await.unwrap(), &data).await == data);
    // Nothing on it, not reported, not reachable from here, or not TCP: an answer, not a stream.
    for (endpoint, status, code) in [
        ("closed", 503, "not_served"),
        ("22", 404, "not_found"),
        ("far", 403, "forbidden"),
        ("stats", 400, "bad_request"),
    ] {
        let refused = open(endpoint.into()).await.err().unwrap();
        assert_eq!(
            (refused.status(), refused.code()),
            (status, code),
            "{endpoint}: {refused}"
        );
    }

    // Over the portal, with a window of its own.
    let portal = Portal::over(
        client::websocket_unix(&served.socket, "/v1/portal")
            .await
            .unwrap(),
    );
    let tunnel = portal.connect_endpoint(&id, "db").await.unwrap();
    assert!(through(tunnel, &data).await == data);
    let refused = portal.connect_endpoint(&id, "closed").await.err().unwrap();
    assert_eq!((refused.status(), refused.code()), (503, "not_served"));
    let refused = portal.connect_endpoint(&id, "6000").await.err().unwrap();
    assert_eq!(refused.status(), 403, "{refused}");

    // Endpoints that move with a check are a change of the resource.
    let mut events = client::websocket_unix(&served.socket, "/v1/events")
        .await
        .unwrap();
    let follow = json!({"changes": ["resources"]}).to_string();
    events.send(Frame::Text(follow.into())).await.unwrap();
    let moved = echo().await;
    let status = json!({"state": "running", "endpoints": endpoints(moved)});
    std::fs::write(fake.join("answer.status"), status.to_string()).unwrap();
    let seen: Value = client.get(&format!("/v1/resources/{id}")).await.unwrap();
    assert_eq!(seen["endpoints"][0]["port"], moved);
    assert_eq!(
        change_of(&mut events, "resource", "changed").await["resource"],
        json!(id)
    );
    assert!(through(open("db".into()).await.unwrap(), b"moved").await == b"moved");
    // One that is gone has none.
    std::fs::write(fake.join("answer.status"), r#"{"state": "gone"}"#).unwrap();
    let seen: Value = client.get(&format!("/v1/resources/{id}")).await.unwrap();
    assert!(seen.get("endpoints").is_none(), "{seen}");
    let refused = open("db".into()).await.err().unwrap();
    assert_eq!(refused.status(), 404, "{refused}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_endpoint_is_opened_by_an_operator_and_not_by_a_viewer() {
    let issuer = Issuer::new();
    let (_dir, address) = trusting(&issuer, true).await;
    let with = |scope: &str| issuer.token(json!({"scope": scope}));
    // The role is asked before anything of the resource is.
    for (scope, status) in [("berth:view", 403), ("berth:operate", 404)] {
        let io = tokio::net::TcpStream::connect(address).await.unwrap();
        let refused = berth_api::tunnel::endpoint_tunnel(io, "r-nothing", "db", &with(scope))
            .await
            .err()
            .unwrap();
        assert_eq!(refused.status(), status, "{scope}: {refused}");
    }
    let io = tokio::net::TcpStream::connect(address).await.unwrap();
    let socket = client::websocket(io, "/v1/portal", &with("berth:view"))
        .await
        .unwrap();
    let portal = berth_api::portal::Portal::over(socket);
    let refused = portal
        .connect_endpoint("r-nothing", "db")
        .await
        .err()
        .unwrap();
    assert_eq!(refused.status(), 403, "{refused}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_runs_in_a_resource_as_a_job_that_ends_as_it_did() {
    let served = served_with(quick).await;
    let fake = fake_provisioner(&served);
    let id = made_box(&served, &fake, json!({"id": "db-1", "state": "running"})).await;
    let mut client = served.client().await;
    let client = &mut client;
    let path = format!("/v1/resources/{id}/actions/exec");

    // Offered for its kind, to an operator.
    let (_, actions) = send(
        client,
        "GET",
        &format!("/v1/resources/{id}/actions"),
        Value::Null,
    )
    .await;
    let exec = actions
        .as_array()
        .unwrap()
        .iter()
        .find(|action| action["name"] == "exec")
        .cloned()
        .unwrap();
    assert_eq!(exec["role"], "operate");

    // Its output and exit status are the job's.
    std::fs::write(fake.join("say.exec"), "hi from db\n").unwrap();
    std::fs::write(fake.join("answer.exec"), r#"{"exit_code": 3}"#).unwrap();
    let asked = json!({"arguments": {"command": r#"["sh", "-c", "echo hi; exit 3"]"#, "stdin": "select 1;\n"}});
    let (status, acted) = send(client, "POST", &path, asked).await;
    assert_eq!(status, 202, "{acted}");
    let (output, code) = job_ended(&served.socket, acted["job"]["id"].as_str().unwrap()).await;
    assert_eq!(code, 3, "{output}");
    // The output is the command's alone.
    assert_eq!(output.trim(), "hi from db", "{output}");
    let request: Value =
        serde_json::from_str(&std::fs::read_to_string(fake.join("last.exec")).unwrap()).unwrap();
    assert_eq!(request["command"], json!(["sh", "-c", "echo hi; exit 3"]));
    assert_eq!(request["stdin"], "select 1;\n");
    assert_eq!(request["id"], "db-1");
    // The job's record keeps the command, and not what it read.
    assert_eq!(
        acted["job"]["arguments"],
        json!(["sh", "-c", "echo hi; exit 3"])
    );
    let (_, kept) = send(
        client,
        "GET",
        &format!("/v1/jobs/{}", acted["job"]["id"].as_str().unwrap()),
        Value::Null,
    )
    .await;
    assert_eq!(
        kept["arguments"],
        json!(["sh", "-c", "echo hi; exit 3"]),
        "{kept}"
    );
    assert!(!kept.to_string().contains("select 1"), "{kept}");

    // Words split at spaces, no input, and a command that went well.
    std::fs::write(fake.join("answer.exec"), r#"{"exit_code": 0}"#).unwrap();
    let asked = json!({"arguments": {"command": "uname -a"}});
    let (_, acted) = send(client, "POST", &path, asked).await;
    let (output, code) = job_ended(&served.socket, acted["job"]["id"].as_str().unwrap()).await;
    assert_eq!(code, 0, "{output}");
    let request: Value =
        serde_json::from_str(&std::fs::read_to_string(fake.join("last.exec")).unwrap()).unwrap();
    assert_eq!(request["command"], json!(["uname", "-a"]));
    assert!(request.get("stdin").is_none(), "{request}");

    // A provisioner that says no exit status fails the job.
    std::fs::write(fake.join("answer.exec"), "{}").unwrap();
    let (_, acted) = send(
        client,
        "POST",
        &path,
        json!({"arguments": {"command": "true"}}),
    )
    .await;
    let (output, code) = job_ended(&served.socket, acted["job"]["id"].as_str().unwrap()).await;
    assert_eq!(code, 1, "{output}");
    assert!(output.contains("said no exit status"), "{output}");

    // No command, or one that is no list, is refused before anything runs.
    for command in ["", "  ", "[\"sh\", 1]"] {
        let asked = json!({"arguments": {"command": command}});
        let (status, said) = send(client, "POST", &path, asked).await;
        assert_eq!(status, 400, "{command:?}: {said}");
    }

    // A kind without commands has no such action.
    let asked = json!({"provisioner": "vm", "kind": "deployment", "name": "wiki",
                       "arguments": {"server": "x"}, "spec": "services: {}\n"});
    let (status, created) = send(client, "POST", "/v1/resources", asked).await;
    assert_eq!(status, 202, "{created}");
    job_ended(&served.socket, created["job"]["id"].as_str().unwrap()).await;
    let wiki = created["resource"]["id"].as_str().unwrap();
    let (status, said) = send(
        client,
        "POST",
        &format!("/v1/resources/{wiki}/actions/exec"),
        json!({"arguments": {"command": "true"}}),
    )
    .await;
    assert_eq!(status, 404, "{said}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_running_job_is_stopped_through_the_api() {
    use berth_core::runner::{Program, Request, Subject, Work};
    let served = served().await;
    let mut command = std::process::Command::new("sh");
    command.args(["-c", "echo started; sleep 30"]);
    let work = Work::Program(Box::new(Program {
        command,
        input: None,
        files: vec![],
    }));
    let job = served
        .state
        .runner
        .start(Request::new(Subject::Installation, "rollout"), work)
        .unwrap();
    let mut client = served.client().await;
    let (status, stopping) = send(
        &mut client,
        "POST",
        &format!("/v1/jobs/{}/stop", job.id),
        Value::Null,
    )
    .await;
    assert_eq!(status, 202, "{stopping}");
    assert_eq!(stopping["stopped_by"], "the server's own user");
    let (output, code) = job_ended(&served.socket, &job.id).await;
    assert_eq!(code, 130, "{output}");
    assert!(
        output.contains("✗ Stopped by the server's own user"),
        "{output}"
    );
    // One that has ended is not stopped again.
    let (status, refused) = send(
        &mut client,
        "POST",
        &format!("/v1/jobs/{}/stop", job.id),
        Value::Null,
    )
    .await;
    assert_eq!(
        (status, &refused["error"]["code"]),
        (409, &json!("conflict"))
    );
}

/// A listener like `echo`, on an address of one of this machine's own
/// networks, or nothing when it has none but the loopback.
async fn echo_on_a_network() -> Option<(String, u16)> {
    for network in berthd::endpoints::networks() {
        let Ok(listener) = tokio::net::TcpListener::bind((network.address, 0)).await else {
            continue;
        };
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((connection, _)) = listener.accept().await {
                tokio::spawn(async move {
                    use tokio::io::AsyncWriteExt;
                    let (mut reading, mut writing) = connection.into_split();
                    let _ = tokio::io::copy(&mut reading, &mut writing).await;
                    let _ = writing.shutdown().await;
                });
            }
        });
        return Some((network.address.to_string(), port));
    }
    None
}

/// A controller that answers every request with the version of an older
/// berthd, for a node that has not been updated.
fn older_controller() -> LinkSpec {
    let script = r#"import sys
for line in sys.stdin.buffer:
    if line in (b"\r\n", b"\n"):
        break
body = b'{"version": "0.4.0", "api": 1, "mode": "system"}'
sys.stdout.buffer.write(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: %d\r\n\r\n" % len(body) + body)
sys.stdout.flush()
"#;
    LinkSpec {
        program: "python3".into(),
        args: vec!["-c".into(), script.into()],
        ..LinkSpec::default()
    }
}

/// Say that the fake's provisioner `vm` lives on `node`, as an apply of a node does.
fn lives_on(served: &Served, node: &str) {
    let paths = &served.state.paths;
    let mut entry = berth_core::provisioner::entry(paths, "vm")
        .unwrap()
        .unwrap();
    entry.node = node.into();
    berth_core::provisioner::save_entry(paths, "vm", &entry).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resource_of_a_node_is_reached_through_the_nodes_controller() {
    use berth_api::{portal::Portal, tunnel::endpoint_tunnel};
    let Some(node) = Berthd::start().await else {
        return;
    };
    let served = served_with(quick).await;
    let fake = fake_provisioner(&served);
    served
        .state
        .add(Remote::new("big", "192.0.2.30", node.link()));
    let (port, closed) = (echo().await, closed_port());
    let network = echo_on_a_network().await;
    let mut endpoints = vec![
        json!({"name": "db", "address": "127.0.0.1", "port": port, "local": true}),
        json!({"name": "closed", "address": "127.0.0.1", "port": closed, "local": true}),
        json!({"name": "far", "address": "192.0.2.1", "port": 5432, "local": true}),
        json!({"name": "meta", "address": "169.254.169.254", "port": 80, "local": true}),
        json!({"name": "loop", "address": "127.0.0.2", "port": port, "local": true}),
    ];
    if let Some((address, port)) = &network {
        endpoints.push(json!({"name": "bridge", "address": address, "port": port, "local": true}));
    }
    let answer = json!({"id": "db-1", "state": "running", "endpoints": endpoints});
    let id = made_box(&served, &fake, answer.clone()).await;
    lives_on(&served, "big");
    std::fs::write(fake.join("answer.status"), answer.to_string()).unwrap();

    // The provisioner is told of the node, so its plugin reports what the node sees.
    let status: Value = served
        .client()
        .await
        .get(&format!("/v1/resources/{id}"))
        .await
        .unwrap();
    assert_eq!(status["state"], "running", "{status}");
    let asked: Value =
        serde_json::from_str(&std::fs::read_to_string(fake.join("last.status")).unwrap()).unwrap();
    assert_eq!(asked["node"], "big", "{asked}");

    let open = |endpoint: &str| {
        let (socket, id, endpoint) = (served.socket.clone(), id.clone(), endpoint.to_owned());
        async move {
            let io = tokio::net::UnixStream::connect(&socket).await.unwrap();
            endpoint_tunnel(io, &id, &endpoint, "").await
        }
    };
    // The node's loopback, both ways, with each side's end and a few MB.
    let data: Vec<u8> = (0..3_000_000u32).map(|n| (n % 251) as u8).collect();
    assert_eq!(through(open("db").await.unwrap(), b"hello").await, b"hello");
    assert!(through(open("db").await.unwrap(), &data).await == data);
    let portal = Portal::over(
        client::websocket_unix(&served.socket, "/v1/portal")
            .await
            .unwrap(),
    );
    let tunnel = portal.connect_endpoint(&id, "db").await.unwrap();
    assert!(through(tunnel, &data).await == data);
    // An address on one of the node's own networks, as a container's on a bridge.
    match &network {
        Some(_) => {
            assert!(through(open("bridge").await.unwrap(), &data).await == data);
            let tunnel = portal.connect_endpoint(&id, "bridge").await.unwrap();
            assert_eq!(through(tunnel, b"bridged").await, b"bridged");
        }
        None => eprintln!("skipped the bridge: this machine has no network but the loopback"),
    }
    // Nothing on it, off the node's networks, a metadata address, another loopback address.
    for (endpoint, status, said) in [
        ("closed", 503, "Nothing answers"),
        ("far", 403, "no network"),
        ("meta", 403, "metadata"),
        ("loop", 403, "127.0.0.1"),
    ] {
        let refused = open(endpoint).await.err().unwrap();
        assert_eq!(refused.status(), status, "{endpoint}: {refused}");
        assert!(refused.to_string().contains(said), "{endpoint}: {refused}");
    }

    // A caller does not reach the node's stream through the server, however it asks.
    for path in [
        format!("/v1/hosts/big/controller/endpoints/127.0.0.1/{port}"),
        "/v1/hosts/big/controller/endpoints/192.0.2.1/80".to_owned(),
        "/v1/hosts/big/controller/%65ndpoints/192.0.2.1/80".to_owned(),
        "/v1/hosts/nobody/controller/endpoints/192.0.2.1/80".to_owned(),
    ] {
        let refused = client::websocket_unix(&served.socket, &path)
            .await
            .err()
            .unwrap();
        assert_eq!(refused.status(), 403, "{path}: {refused}");
    }
    let mut client = served.client().await;
    let plain = client
        .get::<Value>("/v1/hosts/big/controller/endpoints/192.0.2.1/80")
        .await
        .unwrap_err();
    assert_eq!(plain.status(), 403, "{plain}");
    let args = json!({"address": "192.0.2.1", "port": 80});
    let refused = portal
        .open("endpoints.connect", Some("big"), args.clone())
        .await
        .err()
        .unwrap();
    assert_eq!(refused.status(), 403, "{refused}");
    // Nor through the node's own portal, passed through.
    let passed = Portal::over(
        client::websocket_unix(&served.socket, "/v1/hosts/big/controller/portal")
            .await
            .unwrap(),
    );
    let refused = passed
        .open("endpoints.connect", None, args)
        .await
        .err()
        .unwrap();
    assert_eq!(refused.status(), 403, "{refused}");

    // A node that is gone, or whose controller is older than the stream, says so.
    lives_on(&served, "gone");
    let refused = open("db").await.err().unwrap();
    assert_eq!(refused.status(), 502, "{refused}");
    assert!(refused.to_string().contains("gone"), "{refused}");
    served
        .state
        .add(Remote::new("old", "192.0.2.31", older_controller()));
    lives_on(&served, "old");
    let refused = open("db").await.err().unwrap();
    assert_eq!(refused.status(), 501, "{refused}");
    assert!(
        refused.to_string().contains("update its controller"),
        "{refused}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_admin_over_the_network_does_not_reach_a_hosts_stream_for_the_server() {
    let issuer = Issuer::new();
    let (_dir, address) = trusting(&issuer, true).await;
    let admin = issuer.token(json!({"scope": "berth:admin"}));
    let path = "/v1/hosts/big/controller/endpoints/192.0.2.1/80";
    let io = tokio::net::TcpStream::connect(address).await.unwrap();
    let refused = client::websocket(io, path, &admin).await.err().unwrap();
    assert_eq!(refused.status(), 403, "{refused}");
    let (status, said) = as_bearer(address, &admin, "GET", path, Value::Null).await;
    assert_eq!(status, 403, "{said}");
    let io = tokio::net::TcpStream::connect(address).await.unwrap();
    let portal =
        berth_api::portal::Portal::over(client::websocket(io, "/v1/portal", &admin).await.unwrap());
    let refused = portal
        .open(
            "endpoints.connect",
            Some("big"),
            json!({"address": "192.0.2.1", "port": 80}),
        )
        .await
        .err()
        .unwrap();
    assert_eq!(refused.status(), 403, "{refused}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_that_claims_to_be_a_stream_needs_its_role_all_the_same() {
    let issuer = Issuer::new();
    let (_dir, address) = trusting(&issuer, true).await;
    let viewer = issuer.token(json!({"scope": "berth:view"}));
    let bearer = format!("Bearer {viewer}");
    for (method, path, body) in [
        ("PUT", "/v1/secrets/main", r#"{"value": "x"}"#),
        ("GET", "/v1/secrets", ""),
        ("PUT", "/v1/kv/ui/x", "1"),
        ("PUT", "/v1/provisioners/vm", r#"{"type": "fake/vm"}"#),
        ("POST", "/v1/plugins/package", ""),
    ] {
        let stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let mut client = Client::over(stream).await.unwrap();
        let headers = [
            ("authorization", bearer.as_str()),
            ("content-type", "application/json"),
            ("upgrade", "websocket"),
            ("connection", "upgrade"),
        ];
        let answer = client
            .send(method, path, &headers, body.to_owned().into())
            .await
            .unwrap();
        assert_eq!(answer.status, 403, "{method} {path}");
    }
    let (status, _) = as_bearer(address, &viewer, "HEAD", "/v1/secrets", Value::Null).await;
    assert_eq!(status, 403);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_profile_does_not_say_how_ansible_reaches_the_host() {
    let issuer = Issuer::new();
    let (_dir, address) = trusting(&issuer, true).await;
    let admin = issuer.token(json!({"scope": "berth:admin"}));
    let vars = json!({"builder": {}, "access": {"address": "192.0.2.7"},
        "ansible_vars": {"ansible_ssh_common_args": "-o ProxyCommand=\"sh -c id\"",
                         "ansible_connection": "local", "ansible_ssh_executable": "/bin/sh",
                         "devbox_geometry": "1x1"}});
    let (status, said) = as_bearer(
        address,
        &admin,
        "PUT",
        "/v1/hosts/studio/profile",
        vars.clone(),
    )
    .await;
    assert_eq!(status, 400, "{said}");
    let message = said["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("not ansible_connection, ansible_ssh_common_args, ansible_ssh_executable"),
        "{message}"
    );
    // Nor from the server's own user.
    let served = served_with(quick).await;
    let mut client = served.client().await;
    let (status, said) = send(&mut client, "PUT", "/v1/hosts/studio/profile", vars).await;
    assert_eq!(status, 400, "{said}");
    let feature = json!({"builder": {}, "access": {"address": "192.0.2.7"},
        "ansible_vars": {"devbox_plugin_features": ["../../tmp/x"]}});
    let (status, said) = send(&mut client, "PUT", "/v1/hosts/studio/profile", feature).await;
    assert_eq!(status, 400, "{said}");
    assert!(
        said["error"]["message"]
            .as_str()
            .unwrap()
            .contains("PLUGIN/FEATURE"),
        "{said}"
    );

    // A profile saved before still lists and reads.
    let paths = &served.state.paths;
    paths.prepare().unwrap();
    berth_core::profiles::Place::own("dock")
        .unwrap()
        .save(
            paths,
            &json!({"builder": {}, "access": {"address": "192.0.2.8"},
                    "ansible_vars": {"ansible_connection": "local", "devbox_geometry": "1x1"}}),
        )
        .unwrap();
    let (status, listed) = send(&mut client, "GET", "/v1/hosts", Value::Null).await;
    assert_eq!(status, 200, "{listed}");
    assert!(listed.to_string().contains("\"dock\""), "{listed}");
    let (status, profile) = send(&mut client, "GET", "/v1/hosts/dock/profile", Value::Null).await;
    assert_eq!(status, 200, "{profile}");
}

/// The names of what a host's actions list says, with each one's availability.
async fn host_action(client: &mut Client, host: &str, name: &str) -> Value {
    let (status, listed) = send(
        client,
        "GET",
        &format!("/v1/hosts/{host}/actions"),
        Value::Null,
    )
    .await;
    assert_eq!(status, 200, "{listed}");
    listed
        .as_array()
        .unwrap()
        .iter()
        .find(|action| action["name"] == name)
        .cloned()
        .unwrap_or_else(|| panic!("{host} has no action {name}: {listed}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_is_started_stopped_duplicated_and_removed_by_its_actions() {
    let served = served_with(quick).await;
    let fake = fake_provisioner(&served);
    let mut client = served.client().await;
    let client = &mut client;
    register_fake(client).await;
    send(
        client,
        "PUT",
        "/v1/secrets/api_token",
        json!({"value": "tok-s3cret"}),
    )
    .await;
    let paths = &served.state.paths;
    paths.prepare().unwrap();
    berth_core::profiles::Place::own("studio")
        .unwrap()
        .save(
            paths,
            &json!({"hostname": "studio", "builder": {}, "access": {"user": "ops", "link": ["sh", "-c", "exit 1"]},
                    "ansible_vars": {"devbox_install_tailscale": false}}),
        )
        .unwrap();
    let machine = |host: &str| berth_core::resource::of_host(paths, host).unwrap();
    let done = async |client: &mut Client, acted: &Value| {
        let job = job_done(client, acted["job"]["id"].as_str().unwrap()).await;
        let (output, _) = job_ended(&served.socket, acted["job"]["id"].as_str().unwrap()).await;
        (job, output)
    };
    let act = |path: &str| format!("/v1/hosts/{path}");

    // A host no provisioner made declares nothing, and can be neither started nor stopped.
    let start = host_action(client, "studio", "start").await;
    assert_eq!(start["available"], false, "{start}");
    assert!(
        start["why"]
            .as_str()
            .unwrap()
            .contains("declares no way to start it")
    );
    let (_, host) = send(client, "GET", "/v1/hosts/studio", Value::Null).await;
    assert_eq!(
        host["power"],
        json!({"start": false, "stop": false, "state": "unknown"})
    );
    let remove = host_action(client, "studio", "remove").await;
    assert_eq!(
        (&remove["role"], &remove["available"]),
        (&json!("admin"), &json!(true))
    );
    assert_eq!(
        remove["confirm"],
        "Forget studio? The machine is left as it is."
    );
    let (status, said) = send(
        client,
        "POST",
        &act("studio/actions/remove"),
        json!({"arguments": {"destroy": "yes"}}),
    )
    .await;
    assert_eq!(status, 501, "{said}");
    assert!(
        said["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("No provisioner made studio"),
        "{said}"
    );

    // Made by a provisioner whose machines start but do not stop.
    std::fs::write(
        fake.join("answer.create"),
        r#"{"id": "vm-1", "address": "192.0.2.20"}"#,
    )
    .unwrap();
    let asked = json!({"provisioner": "vm", "host": "studio", "arguments": {"server": "https://192.0.2.10:8443", "cpus": "2"}});
    let (status, created) = send(client, "POST", "/v1/resources", asked).await;
    assert_eq!(status, 202, "{created}");
    let (job, output) = done(client, &created).await;
    assert_eq!(job["exit_code"], 0, "{output}");
    let (_, host) = send(client, "GET", "/v1/hosts/studio", Value::Null).await;
    assert_eq!(
        host["power"],
        json!({"start": true, "stop": false, "state": "running", "start_by": "provisioner"})
    );
    let stop = host_action(client, "studio", "stop").await;
    assert_eq!(
        (&stop["available"], &stop["group"]),
        (&json!(false), &json!("Power"))
    );
    assert!(
        stop["why"]
            .as_str()
            .unwrap()
            .starts_with("vm cannot stop the machines it makes"),
        "{stop}"
    );
    let (status, said) = send(client, "POST", &act("studio/actions/stop"), Value::Null).await;
    assert_eq!(status, 501, "{said}");
    let remove = host_action(client, "studio", "remove").await;
    assert!(
        remove["confirm"]
            .as_str()
            .unwrap()
            .contains("vm destroys its machine vm-1 and the disk it has"),
        "{remove}"
    );
    let (status, acted) = send(client, "POST", &act("studio/actions/start"), Value::Null).await;
    assert_eq!(status, 202, "{acted}");
    let (job, output) = done(client, &acted).await;
    assert_eq!(
        (&job["exit_code"], &job["action"]),
        (&json!(0), &json!("start")),
        "{output}"
    );
    let asked = std::fs::read_to_string(fake.join("requests.log")).unwrap();
    assert_eq!(asked.lines().last(), Some("start"));

    // A copy is a new host whose machine is planned with the same provisioner and arguments.
    let duplicate = host_action(client, "studio", "duplicate").await;
    assert!(
        duplicate["help"]
            .as_str()
            .unwrap()
            .contains("The disk is not cloned")
    );
    assert_eq!(duplicate["arguments"][1]["name"], "create");
    let (status, acted) = send(
        client,
        "POST",
        &act("studio/actions/duplicate"),
        json!({"arguments": {"name": "lab"}}),
    )
    .await;
    assert_eq!(status, 202, "{acted}");
    let (job, output) = done(client, &acted).await;
    assert_eq!(job["exit_code"], 0, "{output}");
    assert!(
        output.contains("Make it with: berth host create lab"),
        "{output}"
    );
    let (_, copy) = send(client, "GET", "/v1/hosts/lab/profile", Value::Null).await;
    assert_eq!(
        copy["access"],
        json!({"user": "ops", "auth": "berth"}),
        "{copy}"
    );
    assert!(copy.get("hostname").is_none());
    let planned = machine("lab").unwrap().record;
    assert_eq!(
        (
            planned.provisioner.as_str(),
            planned.state.as_str(),
            planned.external_id.as_str()
        ),
        ("vm", "planned", "")
    );
    assert_eq!(planned.arguments["cpus"], "2");
    let (_, listed) = send(client, "GET", "/v1/resources?host=lab", Value::Null).await;
    assert_eq!(
        (&listed[0]["state"], &listed[0]["problem"]),
        (&json!("planned"), &Value::Null)
    );
    let start = host_action(client, "lab", "start").await;
    assert!(
        start["why"].as_str().unwrap().contains("not made yet"),
        "{start}"
    );
    let (status, said) = send(
        client,
        "POST",
        &act("studio/actions/duplicate"),
        json!({"arguments": {"name": "lab"}}),
    )
    .await;
    assert_eq!(status, 409, "{said}");
    let (status, said) = send(client, "POST", &act("studio/actions/duplicate"), json!({})).await;
    assert_eq!(status, 400, "{said}");

    // With create, the copy's machine is made in the same job.
    std::fs::write(
        fake.join("answer.create"),
        r#"{"id": "vm-2", "address": "192.0.2.21"}"#,
    )
    .unwrap();
    let (status, acted) = send(
        client,
        "POST",
        &act("studio/actions/duplicate"),
        json!({"arguments": {"name": "attic", "create": "yes"}}),
    )
    .await;
    assert_eq!(status, 202, "{acted}");
    let (job, output) = done(client, &acted).await;
    assert_eq!(job["exit_code"], 0, "{output}");
    assert!(!output.contains("tok-s3cret"), "{output}");
    let made = machine("attic").unwrap().record;
    assert_eq!(
        (made.external_id.as_str(), made.arguments["cpus"].as_str()),
        ("vm-2", "2")
    );

    // Removing a planned copy forgets it with its plan; destroying it is refused.
    let (status, said) = send(
        client,
        "POST",
        &act("lab/actions/remove"),
        json!({"arguments": {"destroy": "yes"}}),
    )
    .await;
    assert_eq!(status, 501, "{said}");
    let (status, acted) = send(client, "POST", &act("lab/actions/remove"), Value::Null).await;
    assert_eq!(status, 202, "{acted}");
    assert_eq!(done(client, &acted).await.0["exit_code"], 0);
    assert_eq!(
        send(client, "GET", "/v1/hosts/lab", Value::Null).await.0,
        404
    );
    assert!(
        berth_core::resource::list(paths)
            .unwrap()
            .iter()
            .all(|kept| kept.record.name != "lab")
    );

    // Without destroy, the machine stays, and berth keeps it under its provisioner.
    let (status, acted) = send(client, "POST", &act("attic/actions/remove"), Value::Null).await;
    assert_eq!(status, 202, "{acted}");
    let (job, output) = done(client, &acted).await;
    assert_eq!(job["exit_code"], 0, "{output}");
    assert!(output.contains("berth resource remove"), "{output}");
    assert_eq!(
        send(client, "GET", "/v1/hosts/attic", Value::Null).await.0,
        404
    );
    let kept = berth_core::resource::list(paths).unwrap();
    let left = kept
        .iter()
        .find(|kept| kept.record.external_id == "vm-2")
        .unwrap();
    assert!(left.record.host.is_empty());

    // With destroy, the provisioner destroys the machine and the host goes.
    let (status, acted) = send(
        client,
        "POST",
        &act("studio/actions/remove"),
        json!({"arguments": {"destroy": "yes"}}),
    )
    .await;
    assert_eq!(status, 202, "{acted}");
    let (job, output) = done(client, &acted).await;
    assert_eq!(job["exit_code"], 0, "{output}");
    assert!(output.contains("vm destroyed vm-1"), "{output}");
    let asked = std::fs::read_to_string(fake.join("requests.log")).unwrap();
    assert_eq!(asked.lines().last(), Some("destroy"));
    assert_eq!(
        send(client, "GET", "/v1/hosts/studio", Value::Null).await.0,
        404
    );
    assert!(machine("studio").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_that_declares_its_controller_stops_it_asks_the_controller() {
    let served = served().await;
    let mut client = served.client().await;
    let client = &mut client;
    let paths = &served.state.paths;
    let profile = json!({"type": "managed", "access": {"address": "192.0.2.7", "link": ["sh", "-c", "exit 1"]},
                         "power": {"stop": {"by": "controller"}}});
    let (status, said) = send(client, "PUT", "/v1/hosts/dock/profile", profile).await;
    assert_eq!(status, 200, "{said}");
    let (status, said) = send(
        client,
        "PUT",
        "/v1/hosts/yard/profile",
        json!({"type": "managed", "access": {"address": "192.0.2.8"}, "power": {"stop": {"by": "magic"}}}),
    )
    .await;
    assert_eq!(status, 400, "{said}");
    assert!(
        berth_core::profiles::Place::own("yard")
            .unwrap()
            .load(paths)
            .unwrap()
            .is_none()
    );
    let (_, host) = send(client, "GET", "/v1/hosts/dock", Value::Null).await;
    assert_eq!(
        host["power"],
        json!({"start": false, "stop": true, "state": "unknown", "stop_by": "controller"})
    );
    // Its controller does not answer, so it cannot be asked to shut the host down.
    let stop = host_action(client, "dock", "stop").await;
    assert_eq!(stop["available"], false, "{stop}");
    assert!(
        stop["why"]
            .as_str()
            .unwrap()
            .starts_with("The controller on dock does not answer"),
        "{stop}"
    );
    assert_eq!(stop["arguments"][0]["name"], "force");
    assert_eq!(stop["role"], "operate");
    let (status, said) = send(client, "POST", "/v1/hosts/dock/actions/stop", Value::Null).await;
    assert_eq!(status, 501, "{said}");
}
