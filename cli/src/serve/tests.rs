use super::api::{self, OutputQuery, piece};
use super::{Remote, Serve};
use berth_api::client::{self, Client, Frame};
use berth_api::{Output, unb64};
use berth_core::{
    link::LinkSpec,
    paths::Paths,
    store::{Job, Store},
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
    let store = Store::open(&super::store_path(&paths)).unwrap();
    Serve::new(paths, store, token.map(str::to_owned))
}

async fn served() -> Served {
    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path(), None);
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

fn message(id: u64, topic: &str, keep: &str) -> berth_api::Message {
    serde_json::from_value(json!({
        "id": id, "topic": topic, "body": {"n": id}, "keep": keep,
        "from": {"level": "agent", "uid": 1001}, "time": berth_api::now(),
    }))
    .unwrap()
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
            .get::<Value>("/v1/hosts/nope/version")
            .await
            .unwrap_err()
            .status(),
        404
    );
    assert_eq!(
        client
            .post::<Value>("/v1/hosts/nope/apply", &json!({}))
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
        via: None,
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
        .get::<Value>("/v1/hosts/broken/tasks")
        .await
        .unwrap_err();
    assert_eq!((through.status(), through.code()), (502, "unreachable"));
    // Secrets are refused before the host is even tried.
    let secrets = client
        .post::<Value>("/v1/hosts/broken/tasks/secrets", &json!({}))
        .await
        .unwrap_err();
    assert_eq!((secrets.status(), secrets.code()), (403, "forbidden"));

    // A job that ran: its state, its output both ways, and the stream of it.
    let log = served.dir.path().join("j1.log");
    std::fs::write(&log, "PLAY [all]\nok: [devbox_target]\n").unwrap();
    let job = Job {
        id: "j1".into(),
        host: "broken".into(),
        what: "apply".into(),
        started: berth_api::now(),
        ended: None,
        exit_code: None,
        output_path: log.to_string_lossy().into_owned(),
    };
    served.state.store.lock().unwrap().add_job(&job).unwrap();
    let jobs: Value = client.get("/v1/jobs").await.unwrap();
    assert_eq!(
        (&jobs[0]["id"], &jobs[0]["ended"]),
        (&json!("j1"), &Value::Null)
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
    served
        .state
        .store
        .lock()
        .unwrap()
        .end_job("j1", Some(2))
        .unwrap();
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

    // Messages as the taking-in stores them.
    let mut live = client::websocket_unix(&served.socket, "/v1/inbox?topic=build/")
        .await
        .unwrap();
    let mut events = client::websocket_unix(&served.socket, "/v1/events")
        .await
        .unwrap();
    let follow = r#"{"topics":["build/"],"hosts":["ovh"],"kv":["ui"]}"#;
    events.send(Frame::Text(follow.into())).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    served.state.accept("ovh", message(1, "other/x", "log"));
    served
        .state
        .accept("ovh", message(2, "build/progress", "none"));
    served
        .state
        .accept("ovh", message(3, "build/result", "log"));
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
        (&json!("channel"), &json!("ovh"), &json!(2))
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

    let inbox: Value = client.get("/v1/inbox?host=ovh").await.unwrap();
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
        .post::<Value>("/v1/inbox/ovh/3/done", &json!({}))
        .await
        .unwrap();
    let open: Value = client
        .get("/v1/inbox?done=false&topic=build/")
        .await
        .unwrap();
    assert_eq!(open["messages"].as_array().unwrap().len(), 1);
    client
        .call::<Value>("DELETE", "/v1/inbox/ovh/3/done", None)
        .await
        .unwrap();
    let open: Value = client
        .get("/v1/inbox?done=false&topic=build/")
        .await
        .unwrap();
    assert_eq!(open["messages"].as_array().unwrap().len(), 2);
    assert_eq!(
        client
            .post::<Value>("/v1/inbox/ovh/99/done", &json!({}))
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
        let program = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/debug/berthd");
        if !program.is_file() {
            eprintln!("skipped: {} is not built", program.display());
            return None;
        }
        let dir = tempfile::tempdir().unwrap();
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
            via: None,
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
    let version: Value = client.get("/v1/hosts/box/version").await.unwrap();
    assert_eq!(version["api"], berth_api::API);
    // A request with a body and a query, and the host's own error passed back.
    let start = json!({"kind": "process", "command": "echo through; exit 3"});
    let ran: Value = client
        .post("/v1/hosts/box/sessions?wait=20", &start)
        .await
        .unwrap();
    assert_eq!(ran["exit_code"], 3, "{ran}");
    assert_eq!(
        unb64(ran["stdout_b64"].as_str().unwrap()).unwrap(),
        b"through\n"
    );
    let missing = client
        .get::<Value>("/v1/hosts/box/sessions/snope")
        .await
        .unwrap_err();
    assert_eq!((missing.status(), missing.code()), (404, "not_found"));
    let secrets = client
        .post::<Value>("/v1/hosts/box/tasks/secrets", &json!({}))
        .await
        .unwrap_err();
    assert_eq!(secrets.status(), 403);

    // A stream, both ways: what is published through it comes back on it.
    let mut channel = client::websocket_unix(&served.socket, "/v1/hosts/box/channel?topic=t/")
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
    let session: Value = client.post("/v1/hosts/box/sessions", &start).await.unwrap();
    let path = format!(
        "/v1/hosts/box/sessions/{}?since=0",
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
    let secrets = portal
        .call(
            "tasks.run",
            Some("box"),
            json!({"name": "secrets"}),
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
    let docs = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/openapi");
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

#[cfg(feature = "web")]
#[tokio::test(flavor = "multi_thread")]
async fn the_page_is_given_without_the_token() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path(), Some("s3cret-token"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = api::router(state, true).merge(super::page::router());
    tokio::spawn(async move { axum::serve(listener, router).await });
    let stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let mut client = Client::over(stream).await.unwrap();
    // The page for a browser holds nothing, so it is given without the token;
    // what it shows it asks the portal for.
    let page = client
        .send("GET", "/", &[], Default::default())
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&page.body).into_owned();
    assert!(page.status == 200 && text.contains("berth-web.js?v=") && !text.contains("@BUILD@"));
    let wasm = client
        .send("GET", "/berth-web_bg.wasm", &[], Default::default())
        .await
        .unwrap();
    assert!(wasm.status == 200 && wasm.body.starts_with(b"\0asm"));
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
    let profile = json!({"hostname": "studio", "builder": {"ssh_host": "192.0.2.7", "ssh_user": "ops", "served": "x"}});

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
    assert_eq!(kept["builder"]["ssh_host"], "192.0.2.7");
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
    let linked = json!({"builder": {"link": ["sh", "-c", "id"]}});
    assert_eq!(
        send(client, "PUT", "/v1/hosts/studio/profile", linked)
            .await
            .0,
        400
    );
    assert_eq!(
        send(client, "PUT", "/v1/hosts/studio/profile", json!([]))
            .await
            .0,
        400
    );

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
    let wanting = json!({"builder": {"ssh_host": "192.0.2.7"}, "secrets": [{"name": "github", "env": "GH_TOKEN"}]});
    assert_eq!(
        send(client, "PUT", "/v1/hosts/studio/profile", wanting)
            .await
            .0,
        200
    );
    let (status, said) = send(client, "POST", "/v1/hosts/studio/secrets", Value::Null).await;
    assert_eq!(status, 400);
    assert!(
        said["error"]["message"]
            .as_str()
            .unwrap()
            .contains("github")
    );

    // Some settings are a caller's to change, who gets in is not.
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
        403
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
    let store = Store::open(&super::store_path(&paths)).unwrap();
    let auth = super::auth::Auth::trusting("s3cret-token", "https://id.example", &jwks, setup);
    let state = Serve::with(paths, store, auth);
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
    let (_dir, address) = trusting(&issuer, true).await;
    let with = |scope: &str| issuer.token(json!({"scope": scope}));
    let (viewer, operator, admin) = (
        with("openid berth:view"),
        with("berth:operate"),
        with("berth:admin"),
    );
    let profile = json!({"builder": {"ssh_host": "192.0.2.7"}});

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
        as_bearer(address, &viewer, "POST", "/v1/hosts/a/tasks/x", json!({}))
            .await
            .0,
        403
    );
    assert_eq!(
        as_bearer(
            address,
            &viewer,
            "GET",
            "/v1/hosts/a/files?path=x",
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
            "tasks.run",
            Some("a"),
            json!({"name": "x"}),
            Some(json!({})),
        )
        .await;
    assert_eq!(run.unwrap_err().status(), 403);
    assert!(
        client::websocket(connect().await, "/v1/hosts/a/sessions/x", &viewer)
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn set_up_can_be_closed_to_the_network() {
    let issuer = Issuer::new();
    let (_dir, address) = trusting(&issuer, false).await;
    let admin = issuer.token(json!({"scope": "berth:admin"}));
    let profile = json!({"builder": {"ssh_host": "192.0.2.7"}});
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

/// An issuer's endpoints, as far as berth as a client uses them.
async fn issuer_endpoints(issuer: Arc<Issuer>) -> String {
    use axum::{Json, http::StatusCode, routing};
    // A form as an issuer is sent one.
    fn fields(body: &str) -> std::collections::HashMap<String, String> {
        body.split('&')
            .filter_map(|pair| pair.split_once('='))
            .map(|(name, value)| (name.to_owned(), berth_api::escape::decode(value)))
            .collect()
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let metadata = json!({
        "issuer": base,
        "token_endpoint": format!("{base}/token"),
        "device_authorization_endpoint": format!("{base}/device"),
    });
    let asked = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let token = move |body: String| {
        let form = fields(&body);
        let field = |name: &str| form.get(name).map(String::as_str).unwrap_or("").to_owned();
        let grant = |scope: &str, refresh: &str, left: u64| {
            let access = issuer.token(json!({"scope": scope}));
            (
                StatusCode::OK,
                Json(json!({"access_token": access, "refresh_token": refresh, "expires_in": left})),
            )
        };
        let refused = |error: &str| (StatusCode::BAD_REQUEST, Json(json!({"error": error})));
        let answer = match (field("grant_type").as_str(), field("client_id").as_str()) {
            (_, id) if id != "berth-cli" => refused("invalid_client"),
            ("urn:ietf:params:oauth:grant-type:device_code", _)
                if field("device_code") == "dc 1" =>
            {
                // Nobody has approved the first time berth asks.
                match asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                    0 => refused("authorization_pending"),
                    _ => grant(&field("scope"), "r1", 600),
                }
            }
            ("refresh_token", _) if field("refresh_token") == "r1" => {
                grant("berth:view", "r2", 600)
            }
            ("client_credentials", _) if field("client_secret") == "s3cret" => {
                grant("berth:admin", "", 600)
            }
            _ => refused("invalid_grant"),
        };
        std::future::ready(answer)
    };
    let device = |body: String| {
        let form = fields(&body);
        assert_eq!(form.get("scope").map(String::as_str), Some("berth:operate"));
        std::future::ready(Json(json!({
            "device_code": "dc 1", "user_code": "WDJB-MJHT", "interval": 0,
            "verification_uri": "https://id.example/device", "expires_in": 60,
        })))
    };
    let routes = axum::Router::new()
        .route(
            "/.well-known/openid-configuration",
            routing::get(move || std::future::ready(Json(metadata.clone()))),
        )
        .route("/device", routing::post(device))
        .route("/token", routing::post(token));
    tokio::spawn(async move { axum::serve(listener, routes).await });
    base
}

#[tokio::test(flavor = "multi_thread")]
async fn berth_gets_access_tokens_as_a_client() {
    use berth_core::oauth::{self, Client};
    let issuer = Arc::new(Issuer::new());
    let (dir, address) = trusting(&issuer, true).await;
    let base = issuer_endpoints(issuer.clone()).await;
    let role = |token: String| async move {
        as_bearer(address, &token, "GET", "/v1/access", Value::Null)
            .await
            .1["role"]
            .clone()
    };

    // The server says whose tokens it takes, to anyone who asks.
    let server = format!("http://{address}");
    let named = berth_core::server::fetch(&format!("{server}{}", oauth::RESOURCE))
        .await
        .unwrap();
    let named: Value = serde_json::from_slice(&named).unwrap();
    assert_eq!(
        named["authorization_servers"],
        json!(["https://id.example"])
    );
    assert_eq!(named["resource"], "berth");

    // A person approves on another device, and berth has waited for it.
    let client = Client {
        issuer: base,
        id: "berth-cli".into(),
        secret: None,
        scope: Some("berth:operate".into()),
        audience: None,
    };
    let mut shown = None;
    let tokens = client
        .device(|asked| shown = asked["user_code"].as_str().map(str::to_owned))
        .await
        .unwrap();
    assert_eq!(shown.as_deref(), Some("WDJB-MJHT"));
    assert_eq!(tokens.refresh.as_deref(), Some("r1"));
    assert!(tokens.expires > jsonwebtoken::get_current_timestamp());
    assert_eq!(role(tokens.access.clone()).await, "operate");
    // The login is renewed without them, and a refresh token is used once.
    let renewed = client.refresh("r1").await.unwrap();
    assert_eq!(renewed.refresh.as_deref(), Some("r2"));
    assert_eq!(role(renewed.access).await, "view");
    assert!(
        client
            .refresh("stale")
            .await
            .unwrap_err()
            .contains("invalid_grant")
    );

    // A client with a secret needs nobody.
    assert!(client.credentials().await.is_err());
    let service = Client {
        secret: Some("s3cret".into()),
        ..client.clone()
    };
    assert_eq!(
        role(service.credentials().await.unwrap().access).await,
        "admin"
    );
    let stranger = Client {
        id: "other".into(),
        ..client
    };
    assert!(stranger.device(|_| {}).await.is_err());

    // Tokens are kept per server, private to the user.
    let paths = Paths {
        config_dir: dir.path().join("config"),
        data_dir: dir.path().join("client"),
    };
    oauth::save(&paths, &server, Some(&tokens)).unwrap();
    assert_eq!(oauth::load(&paths, &server), Some(tokens));
    assert_eq!(oauth::load(&paths, "http://another"), None);
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(paths.data_dir.join("oauth.json"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    oauth::save(&paths, &server, None).unwrap();
    assert_eq!(oauth::load(&paths, &server), None);
}
