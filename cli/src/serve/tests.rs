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
