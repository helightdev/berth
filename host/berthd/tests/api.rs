//! berthd in single-user mode on a temporary directory, through its sockets.

use berth_api::client::{self, Client, Error, Frame};
use berth_api::{self as api, unb64};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{path::PathBuf, process::Child, time::Duration};

struct Daemon {
    dir: tempfile::TempDir,
    child: Child,
}

impl Daemon {
    /// Start berthd with the given configuration files. Fake `systemctl` and
    /// `journalctl` are first on its PATH.
    fn start(files: &[(&str, Value)]) -> Daemon {
        let dir = tempfile::Builder::new()
            .prefix("bd")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::create_dir_all(dir.path().join("etc")).unwrap();
        std::fs::create_dir_all(dir.path().join("files")).unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let script = |name: &str, body: &str| {
            use std::os::unix::fs::PermissionsExt;
            let path = bin.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        script(
            "systemctl",
            r#"echo "$@" >> "$FAKE_LOG"
case "$1" in
  list-units) echo '[{"unit":"ssh.service","load":"loaded","active":"active","sub":"running","description":"OpenSSH"}]' ;;
  show) echo 'Id=ssh.service'; echo 'ActiveState=active' ;;
  is-active) shift; for unit in "$@"; do echo active; done ;;
  stop) echo 'Failed to stop: no' >&2; exit 1 ;;
esac"#,
        );
        script(
            "journalctl",
            r#"echo "$@" >> "$FAKE_LOG"
echo '{"__REALTIME_TIMESTAMP":"1794441600000000","_SYSTEMD_UNIT":"ssh.service","PRIORITY":"6","MESSAGE":"hello","__CURSOR":"s=1"}'
echo '{"__REALTIME_TIMESTAMP":"1794441601000000","SYSLOG_IDENTIFIER":"kernel","PRIORITY":"4","MESSAGE":[104,105],"__CURSOR":"s=2"}'"#,
        );
        script("systemd-run", r#"echo systemd-run "$@" >> "$FAKE_LOG""#);
        let mut daemon = Daemon {
            dir,
            child: std::process::Command::new("true").spawn().unwrap(),
        };
        let mut config =
            json!({"roots": [daemon.path("files")], "systemd": true, "units": ["ssh.service"]});
        for (name, content) in files {
            if *name == "berthd.json" {
                for (key, value) in content.as_object().unwrap() {
                    config[key] = value.clone();
                }
            } else {
                std::fs::write(
                    daemon.dir.path().join("etc").join(name),
                    content.to_string(),
                )
                .unwrap();
            }
        }
        std::fs::write(
            daemon.dir.path().join("etc/berthd.json"),
            config.to_string(),
        )
        .unwrap();
        daemon.run();
        daemon
    }

    fn run(&mut self) {
        let path = format!("{}:{}", self.path("bin"), std::env::var("PATH").unwrap());
        self.child = std::process::Command::new(env!("CARGO_BIN_EXE_berthd"))
            .arg("--single")
            .arg(self.dir.path())
            .env("PATH", path)
            .env("FAKE_LOG", self.path("fake.log"))
            .spawn()
            .unwrap();
        for _ in 0..200 {
            if std::os::unix::net::UnixStream::connect(self.socket("control")).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("berthd did not start");
    }

    fn stop(&mut self) {
        // SAFETY: a plain system call on our own child.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let _ = self.child.wait();
    }

    fn path(&self, name: &str) -> String {
        self.dir.path().join(name).to_string_lossy().into_owned()
    }

    fn socket(&self, level: &str) -> PathBuf {
        self.dir.path().join(format!("{level}.sock"))
    }

    async fn host(&self) -> Client {
        Client::unix(&self.socket("control")).await.unwrap()
    }

    async fn agent(&self) -> Client {
        Client::unix(&self.socket("agent")).await.unwrap()
    }

    async fn stream(
        &self,
        level: &str,
        path: &str,
    ) -> client::WebSocketStream<tokio::net::UnixStream> {
        client::websocket_unix(&self.socket(level), path)
            .await
            .unwrap()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop();
    }
}

fn code(result: Result<Value, Error>) -> (u16, String) {
    match result {
        Err(error) => (error.status(), error.code().to_owned()),
        Ok(value) => panic!("expected an error, got {value}"),
    }
}

fn text(value: &Value) -> String {
    String::from_utf8(unb64(value.as_str().unwrap()).unwrap()).unwrap()
}

async fn next_text(stream: &mut client::WebSocketStream<tokio::net::UnixStream>) -> Value {
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("a frame in time");
        if let Some(Ok(Frame::Text(text))) = frame {
            return serde_json::from_str(&text).unwrap();
        }
    }
}

async fn screen(host: &mut Client, id: &str) -> api::Screen {
    host.get(&format!("/v1/sessions/{id}/screen"))
        .await
        .unwrap()
}

async fn put(host: &mut Client, path: String, body: &'static str) -> client::Response {
    host.send("PUT", &path, &[], body.into()).await.unwrap()
}

async fn wait_ended(host: &mut Client, id: &str) -> api::Session {
    for _ in 0..500 {
        let session: api::Session = host.get(&format!("/v1/sessions/{id}")).await.unwrap();
        if !session.running() {
            return session;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("session {id} did not end");
}

#[tokio::test]
async fn a_process_has_its_streams_apart_and_a_transcript() {
    let daemon = Daemon::start(&[]);
    let mut host = daemon.host().await;
    let version: api::Version = host.get("/v1/version").await.unwrap();
    assert_eq!((version.api, version.mode.as_str()), (api::API, "single"));

    let run =
        json!({"kind": "process", "command": "printf 'one\\ntwo\\nthr'; echo oops >&2; exit 3"});
    let done: Value = host.post("/v1/sessions?wait=10s", &run).await.unwrap();
    assert_eq!(done["exit_code"], 3);
    assert_eq!(
        (text(&done["stdout_b64"]), text(&done["stderr_b64"])),
        ("one\ntwo\nthr".into(), "oops\n".into())
    );
    let id = done["id"].as_str().unwrap();

    // By bytes, from an offset; by lines; the last lines. A last line without
    // its end counts because the session has ended.
    let output: api::Output = host
        .get(&format!(
            "/v1/sessions/{id}/output?stream=stdout&since=4&limit=3"
        ))
        .await
        .unwrap();
    assert_eq!(
        (
            unb64(&output.data_b64).unwrap(),
            output.offset,
            output.next_offset,
            output.ended
        ),
        (b"two".to_vec(), 4, 7, false)
    );
    let output: api::Output = host
        .get(&format!(
            "/v1/sessions/{id}/output?stream=stdout&line=1&count=5"
        ))
        .await
        .unwrap();
    assert_eq!(output.lines.unwrap(), ["two", "thr"]);
    assert_eq!((output.next_line, output.ended), (3, true));
    let output: api::Output = host
        .get(&format!("/v1/sessions/{id}/output?stream=stderr&tail=1"))
        .await
        .unwrap();
    assert_eq!(output.lines.unwrap(), ["oops"]);
    let merged: api::Output = host
        .get(&format!("/v1/sessions/{id}/output"))
        .await
        .unwrap();
    assert_eq!(merged.next_offset, 16);
    assert_eq!(
        code(host.get(&format!("/v1/sessions/{id}/screen")).await),
        (400, "bad_request".into())
    );

    // Input and the end of input; what was typed is not in the transcript.
    let started: Value = host
        .post(
            "/v1/sessions",
            &json!({"kind": "process", "command": ["sh", "-c", "read a; echo got $a; cat"]}),
        )
        .await
        .unwrap();
    let id = started["id"].as_str().unwrap().to_owned();
    let _: Value = host
        .post(
            &format!("/v1/sessions/{id}/input"),
            &json!({"text": "secret\nrest"}),
        )
        .await
        .unwrap();
    let _: Value = host
        .post(&format!("/v1/sessions/{id}/input"), &json!({"eof": true}))
        .await
        .unwrap();
    assert_eq!(wait_ended(&mut host, &id).await.exit_code, Some(0));
    let output: api::Output = host
        .get(&format!("/v1/sessions/{id}/output"))
        .await
        .unwrap();
    assert_eq!(unb64(&output.data_b64).unwrap(), b"got secret\nrest");

    // A signal reaches the whole group; a timeout ends what runs too long.
    let started: Value = host
        .post(
            "/v1/sessions",
            &json!({"kind": "process", "command": "sleep 60 & wait"}),
        )
        .await
        .unwrap();
    let id = started["id"].as_str().unwrap().to_owned();
    let _: Value = host
        .post(
            &format!("/v1/sessions/{id}/signal"),
            &json!({"signal": "TERM"}),
        )
        .await
        .unwrap();
    assert_eq!(wait_ended(&mut host, &id).await.exit_code, Some(143));
    let timed: Value = host
        .post(
            "/v1/sessions?wait=10s",
            &json!({"kind": "process", "command": "sleep 60", "timeout": 1}),
        )
        .await
        .unwrap();
    assert_eq!(timed["exit_code"], 143);
    assert_eq!(
        code(
            host.post(&format!("/v1/sessions/{id}/input"), &json!({"text": "x"}))
                .await
        )
        .0,
        409
    );

    // Followed live from an offset: output frames carry their stream, the last frame the exit code.
    let started: Value = host
        .post(
            "/v1/sessions",
            &json!({"kind": "process", "command": "echo a; sleep 0.3; echo b >&2; exit 4"}),
        )
        .await
        .unwrap();
    let id = started["id"].as_str().unwrap();
    let mut stream = daemon
        .stream("control", &format!("/v1/sessions/{id}?since=0"))
        .await;
    let mut seen = vec![];
    let exited = loop {
        match stream.next().await.unwrap().unwrap() {
            Frame::Binary(bytes) => {
                seen.push((bytes[0], String::from_utf8_lossy(&bytes[1..]).into_owned()))
            }
            Frame::Text(text) => break serde_json::from_str::<Value>(&text).unwrap(),
            _ => {}
        }
    };
    assert_eq!(seen, [(1, "a\n".to_owned()), (2, "b\n".to_owned())]);
    assert_eq!(exited["exited"]["exit_code"], 4);

    // Deleting ends and forgets; the agent's level sees none of this.
    let started: Value = host
        .post(
            "/v1/sessions",
            &json!({"kind": "process", "command": "sleep 60"}),
        )
        .await
        .unwrap();
    let id = started["id"].as_str().unwrap();
    assert_eq!(
        code(
            daemon
                .agent()
                .await
                .get(&format!("/v1/sessions/{id}"))
                .await
        )
        .0,
        403
    );
    assert_eq!(code(daemon.agent().await.get("/v1/sessions").await).0, 403);
    let _: Value = host
        .call("DELETE", &format!("/v1/sessions/{id}"), None)
        .await
        .unwrap();
    assert_eq!(
        code(host.get(&format!("/v1/sessions/{id}")).await),
        (404, "not_found".into())
    );
    assert_eq!(
        code(
            host.post(
                "/v1/sessions",
                &json!({"kind": "process", "command": "true", "user": "root"})
            )
            .await
        )
        .0,
        501
    );
}

#[tokio::test]
async fn a_terminal_keeps_its_screen_for_viewers_that_come_and_go() {
    let daemon = Daemon::start(&[]);
    let mut host = daemon.host().await;
    let run = json!({"kind": "terminal", "command": ["sh", "-c", "stty size; printf 'name? '; read name; echo hello $name; stty size; read end"], "rows": 10, "cols": 60});
    let started: Value = host.post("/v1/sessions", &run).await.unwrap();
    let id = started["id"].as_str().unwrap().to_owned();
    let mut first = daemon
        .stream("control", &format!("/v1/sessions/{id}"))
        .await;
    for _ in 0..200 {
        if screen(&mut host, &id).await.text.contains("name?") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(screen(&mut host, &id).await.text.starts_with("10 60"));
    // One viewer types and resizes; a second that connects later gets the screen replayed.
    first
        .send(Frame::Text(
            json!({"resize": {"rows": 12, "cols": 70}})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    first
        .send(Frame::Binary(b"world\r".to_vec().into()))
        .await
        .unwrap();
    for _ in 0..200 {
        if screen(&mut host, &id).await.text.contains("12 70") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let now = screen(&mut host, &id).await;
    assert!(
        now.text.contains("hello world") && now.text.contains("12 70"),
        "{}",
        now.text
    );
    assert_eq!((now.rows, now.cols), (12, 70));
    let state: api::Session = host.get(&format!("/v1/sessions/{id}")).await.unwrap();
    assert_eq!((state.viewers, state.cols), (1, Some(70)));
    drop(first);
    let mut second = daemon
        .stream("control", &format!("/v1/sessions/{id}"))
        .await;
    let Frame::Binary(replay) = second.next().await.unwrap().unwrap() else {
        panic!("a replay first")
    };
    assert!(String::from_utf8_lossy(&replay).contains("hello world"));
    let lines: api::Screen = host
        .get(&format!("/v1/sessions/{id}/screen?tail=2"))
        .await
        .unwrap();
    assert_eq!(lines.text, "hello world\n12 70");
    assert_eq!(
        code(host.get(&format!("/v1/sessions/{id}/output?tail=2")).await).0,
        400
    );
    second
        .send(Frame::Binary(b"\r".to_vec().into()))
        .await
        .unwrap();
    assert_eq!(next_text(&mut second).await["exited"]["exit_code"], 0);
    // The screen of an ended session is still there.
    assert!(screen(&mut host, &id).await.text.contains("hello world"));
}

#[tokio::test]
async fn tasks_run_as_steps_with_arguments() {
    let tasks = json!([
        {"name": "greet", "label": "Greet", "group": "Demo", "agent": true, "confirm": "Really?",
         "arguments": [{"name": "who", "required": true}, {"name": "how-loud", "default": "quiet"},
                       {"name": "pick", "choices": "echo one; echo; echo 'two words'"}],
         "run": "printf '%s|%s\\n' \"$BERTH_ARG_WHO\" \"$BERTH_ARG_HOW_LOUD\"; exit 2"},
        {"name": "plug:extra", "extends": ["greet"], "run": "echo extra for $BERTH_TASK"},
        {"name": "plug:last", "extends": ["greet", "other"], "run": "exit 5"},
        {"name": "private", "run": "echo host only"},
        {"name": "feed", "input": "secrets", "run": "cat"},
        {"name": "ask", "terminal": true, "agent": true, "run": "printf 'go? '; read answer; echo went $answer"},
    ]);
    let daemon = Daemon::start(&[("tasks.json", tasks)]);
    let mut host = daemon.host().await;
    let mut agent = daemon.agent().await;

    let listed: Vec<api::Task> = host.get("/v1/tasks").await.unwrap();
    assert_eq!(listed.len(), 6);
    assert!(listed.iter().all(|task| task.run.is_empty()));
    assert_eq!(listed[0].confirm.as_deref(), Some("Really?"));
    let listed: Vec<api::Task> = agent.get("/v1/tasks").await.unwrap();
    assert_eq!(
        listed
            .iter()
            .map(|task| task.name.as_str())
            .collect::<Vec<_>>(),
        ["greet", "ask"]
    );

    // Arguments arrive in the environment, quotes and spaces untouched. All
    // steps run; the exit code is the first that was not 0.
    let run = json!({"arguments": {"who": "it's \"me\" $HOME; ls"}});
    let done: Value = host.post("/v1/tasks/greet?wait=10s", &run).await.unwrap();
    assert_eq!(
        text(&done["stdout_b64"]),
        "it's \"me\" $HOME; ls|quiet\nextra for greet\n"
    );
    assert_eq!(done["exit_code"], 2);
    let steps: Vec<(String, i64)> = done["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| {
            (
                step["name"].as_str().unwrap().to_owned(),
                step["exit_code"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        steps,
        [
            ("greet".to_owned(), 2),
            ("plug:extra".to_owned(), 0),
            ("plug:last".to_owned(), 5)
        ]
    );
    assert_eq!(done["task"], "greet");

    assert_eq!(code(host.post("/v1/tasks/greet", &json!({})).await).0, 400);
    assert_eq!(
        code(
            host.post(
                "/v1/tasks/greet",
                &json!({"arguments": {"who": "x", "nope": "y"}})
            )
            .await
        )
        .0,
        400
    );
    assert_eq!(
        code(host.post("/v1/tasks/missing", &json!({})).await).0,
        404
    );
    let choices: api::Choices = host.get("/v1/tasks/greet/choices/pick").await.unwrap();
    assert_eq!(choices.choices, ["one", "two words"]);

    // What the workstation sends as input reaches the command and nothing else does.
    assert_eq!(code(host.post("/v1/tasks/feed", &json!({})).await).0, 400);
    let done: Value = host
        .post(
            "/v1/tasks/feed?wait=10s",
            &json!({"input_b64": api::b64(b"{\"k\":1}\n")}),
        )
        .await
        .unwrap();
    assert_eq!(text(&done["stdout_b64"]), "{\"k\":1}\n");

    // The agent runs what is marked for it, reads and follows that session, and nothing else.
    assert_eq!(
        code(agent.post("/v1/tasks/private", &json!({})).await).0,
        403
    );
    let hosts: Value = host
        .post("/v1/tasks/private?wait=10s", &json!({}))
        .await
        .unwrap();
    assert_eq!(
        code(
            agent
                .get(&format!("/v1/sessions/{}", hosts["id"].as_str().unwrap()))
                .await
        )
        .0,
        403
    );
    let started: Value = agent
        .post("/v1/tasks/ask", &json!({"rows": 5, "cols": 40}))
        .await
        .unwrap();
    assert_eq!(
        (
            started["kind"].as_str(),
            started["started_by"]["level"].as_str()
        ),
        (Some("terminal"), Some("agent"))
    );
    let id = started["id"].as_str().unwrap();
    let mut stream = daemon
        .stream("agent", &format!("/v1/sessions/{id}?since=0"))
        .await;
    stream
        .send(Frame::Binary(b"yes\r".to_vec().into()))
        .await
        .unwrap();
    assert_eq!(next_text(&mut stream).await["exited"]["exit_code"], 0);
    let screen: api::Screen = agent
        .get(&format!("/v1/sessions/{id}/screen"))
        .await
        .unwrap();
    assert!(screen.text.contains("went yes"), "{}", screen.text);
    assert_eq!(
        code(
            agent
                .call("DELETE", &format!("/v1/sessions/{id}"), None)
                .await
        )
        .0,
        403
    );
}

#[tokio::test]
async fn files_stay_under_their_roots() {
    let daemon = Daemon::start(&[]);
    let mut host = daemon.host().await;
    let files = daemon.path("files");
    let at = |rest: &str| {
        format!(
            "/v1/files?path={}",
            client::encode(&format!("{files}/{rest}"))
        )
    };
    assert_eq!(
        put(&mut host, at("a/b/c.txt"), "hello file").await.status,
        200
    );
    assert_eq!(put(&mut host, at("a/b/c.txt"), "again").await.status, 409);
    assert_eq!(
        put(
            &mut host,
            format!("{}&overwrite=true", at("a/b/c.txt")),
            "hello files"
        )
        .await
        .status,
        200
    );
    let listing: api::Listing = host.get(&at("a/b")).await.unwrap();
    assert_eq!(
        (
            listing.entries[0].name.as_str(),
            listing.entries[0].kind.as_str(),
            listing.entries[0].size
        ),
        ("c.txt", "file", 11)
    );
    let whole = host
        .send("GET", &at("a/b/c.txt"), &[], Default::default())
        .await
        .unwrap();
    assert_eq!((whole.status, &whole.body[..]), (200, &b"hello files"[..]));
    let part = host
        .send(
            "GET",
            &at("a/b/c.txt"),
            &[("range", "bytes=6-8")],
            Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        (
            part.status,
            &part.body[..],
            part.headers["content-range"].to_str().unwrap()
        ),
        (206, &b"fil"[..], "bytes 6-8/11")
    );

    // Neither `..` nor a symlink leads out, for reading, writing or removing.
    std::os::unix::fs::symlink("/etc", daemon.dir.path().join("files/out")).unwrap();
    std::fs::write(daemon.dir.path().join("outside"), "x").unwrap();
    for path in ["out/passwd", "../outside", "a/../../outside"] {
        assert_eq!(
            host.send("GET", &at(path), &[], Default::default())
                .await
                .unwrap()
                .status,
            403,
            "{path}"
        );
        assert_eq!(
            host.send("DELETE", &at(path), &[], Default::default())
                .await
                .unwrap()
                .status,
            403,
            "{path}"
        );
    }
    assert_eq!(put(&mut host, at("out/berth-test"), "x").await.status, 403);
    assert_eq!(
        host.send("GET", "/v1/files?path=/etc/passwd", &[], Default::default())
            .await
            .unwrap()
            .status,
        403
    );
    assert_eq!(
        host.send("GET", &at("nothing"), &[], Default::default())
            .await
            .unwrap()
            .status,
        404
    );

    // A folder with contents needs recursive; a symlink inside is removed, not followed; a root never goes.
    std::os::unix::fs::symlink(
        daemon.dir.path().join("outside"),
        daemon.dir.path().join("files/a/link"),
    )
    .unwrap();
    assert_eq!(
        host.send("DELETE", &at("a"), &[], Default::default())
            .await
            .unwrap()
            .status,
        409
    );
    assert_eq!(
        host.send(
            "DELETE",
            &format!("{}&recursive=true", at("a")),
            &[],
            Default::default()
        )
        .await
        .unwrap()
        .status,
        200
    );
    assert!(
        !daemon.dir.path().join("files/a").exists() && daemon.dir.path().join("outside").exists()
    );
    assert_eq!(
        host.send(
            "DELETE",
            &format!("/v1/files?path={files}&recursive=true"),
            &[],
            Default::default()
        )
        .await
        .unwrap()
        .status,
        403
    );
    assert_eq!(code(daemon.agent().await.get(&at("x")).await).0, 403);
}

#[tokio::test]
async fn system_calls_go_to_the_systems_own_tools() {
    let daemon = Daemon::start(&[]);
    let mut host = daemon.host().await;
    let stats: api::Stats = host.get("/v1/stats").await.unwrap();
    assert!(stats.memory.total > 0 && !stats.cores.is_empty() && stats.uptime > 0);
    assert_eq!(
        (stats.units[0].unit.as_str(), stats.units[0].active.as_str()),
        ("ssh.service", "active")
    );
    let units: Vec<api::Unit> = host.get("/v1/units").await.unwrap();
    assert_eq!(units[0].sub, "running");
    let unit: Value = host.get("/v1/units/ssh.service").await.unwrap();
    assert_eq!(unit["ActiveState"], "active");
    let _: Value = host
        .post("/v1/units/ssh.service/restart", &json!({}))
        .await
        .unwrap();
    match host
        .post::<Value>("/v1/units/ssh.service/stop", &json!({}))
        .await
    {
        Err(error) => assert_eq!(
            (error.status(), error.to_string()),
            (500, "Failed to stop: no".into())
        ),
        Ok(_) => panic!("the fake refuses to stop"),
    }
    assert_eq!(
        code(host.post("/v1/units/ssh.service/explode", &json!({})).await).0,
        400
    );
    assert_eq!(code(host.get("/v1/units/-x").await).0, 400);
    let logs: Vec<api::LogEntry> = host.get("/v1/logs?unit=ssh.service&limit=5").await.unwrap();
    assert_eq!(
        (
            logs[0].message.as_str(),
            logs[0].time.as_str(),
            logs[1].unit.as_str(),
            logs[1].message.as_str()
        ),
        ("hello", "2026-11-12T00:00:00Z", "kernel", "hi")
    );
    let mut stream = daemon.stream("control", "/v1/logs?since=s%3D1").await;
    assert_eq!(next_text(&mut stream).await["cursor"], "s=1");
    let processes: Vec<api::Process> = host.get("/v1/processes").await.unwrap();
    let own = processes
        .iter()
        .find(|process| process.pid == daemon.child.id())
        .unwrap();
    assert!(own.command.contains("--single") && own.memory > 0);

    // Power is refused while a session runs, unless forced.
    let started: Value = host
        .post(
            "/v1/sessions",
            &json!({"kind": "process", "command": "sleep 61.5"}),
        )
        .await
        .unwrap();
    assert_eq!(code(host.post("/v1/power/reboot", &json!({})).await).0, 409);
    let child: api::Process = {
        let all: Vec<api::Process> = host.get("/v1/processes").await.unwrap();
        all.into_iter()
            .find(|process| process.command == "sleep 61.5")
            .unwrap()
    };
    let _: Value = host
        .post(
            &format!("/v1/processes/{}/signal", child.pid),
            &json!({"signal": "KILL"}),
        )
        .await
        .unwrap();
    wait_ended(&mut host, started["id"].as_str().unwrap()).await;
    let _: Value = host.post("/v1/power/reboot", &json!({})).await.unwrap();
    assert_eq!(
        code(host.post("/v1/power/explode", &json!({})).await).0,
        400
    );
    let log = std::fs::read_to_string(daemon.path("fake.log")).unwrap();
    for line in [
        "restart -- ssh.service",
        "-u ssh.service",
        "--after-cursor=s=1",
        "systemd-run --quiet --on-active=2 systemctl reboot",
    ] {
        assert!(log.contains(line), "{line} not in {log}");
    }

    // Without systemd the same calls say so.
    let plain = Daemon::start(&[("berthd.json", json!({"systemd": false}))]);
    assert_eq!(
        code(plain.host().await.get("/v1/units").await),
        (501, "not_available".into())
    );
    assert_eq!(
        code(
            plain
                .host()
                .await
                .post("/v1/power/reboot", &json!({}))
                .await
        )
        .0,
        501
    );
}

#[tokio::test]
async fn the_channel_keeps_what_it_is_told_to() {
    let topics = json!([{"topic": "small/", "max": 2}, {"topic": "live/", "keep": "none"}]);
    let mut daemon = Daemon::start(&[("channels.json", topics)]);
    let mut host = daemon.host().await;
    let mut agent = daemon.agent().await;
    let mut listener = daemon.stream("agent", "/v1/channel?topic=small").await;

    let state: api::Message = host
        .post("/v1/channel/state/now", &json!({"body": 1, "keep": "last"}))
        .await
        .unwrap();
    let first = state.id;
    let state: api::Message = host
        .post("/v1/channel/state/now", &json!({"body": 2, "keep": "last"}))
        .await
        .unwrap();
    assert!(state.id > first);
    for index in 0..3 {
        let _: Value = agent
            .post("/v1/channel/small/log", &json!({"body": {"n": index}}))
            .await
            .unwrap();
    }
    let gone: api::Message = host
        .post("/v1/channel/live/progress", &json!({"body": "now"}))
        .await
        .unwrap();
    assert_eq!(gone.keep, api::Keep::None);
    assert_eq!(gone.from.level, api::Level::Host);
    assert_eq!(
        code(
            agent
                .post("/v1/channel/berth/session/started", &json!({"body": 1}))
                .await
        )
        .0,
        403
    );
    assert_eq!(
        code(
            host.post("/v1/channel/bad%20topic", &json!({"body": 1}))
                .await
        )
        .0,
        400
    );
    let big = json!({"body": "x".repeat(70_000)});
    assert_eq!(code(host.post("/v1/channel/a/b", &big).await).0, 413);

    // The log keeps its last two, `last` its newest, `none` nothing.
    let kept: api::Messages = agent.get("/v1/channel?since=0").await.unwrap();
    let seen: Vec<(String, Value)> = kept
        .messages
        .iter()
        .map(|message| (message.topic.clone(), message.body.clone()))
        .collect();
    assert_eq!(
        seen,
        [
            ("state/now".into(), json!(2)),
            ("small/log".into(), json!({"n": 1})),
            ("small/log".into(), json!({"n": 2}))
        ]
    );
    assert_eq!(kept.messages[1].from.level, api::Level::Agent);
    let page: api::Messages = host.get("/v1/channel?topic=small&limit=1").await.unwrap();
    assert_eq!(
        (page.messages.len(), page.next),
        (1, Some(page.messages[0].id))
    );

    // A listener got them live, and can publish over its socket.
    for index in 0..3 {
        assert_eq!(next_text(&mut listener).await["body"]["n"], index);
    }
    listener
        .send(Frame::Text(
            json!({"topic": "small/other", "body": "mine"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    assert_eq!(next_text(&mut listener).await["body"], "mine");
    listener
        .send(Frame::Text(
            json!({"topic": "berth/x", "body": 1}).to_string().into(),
        ))
        .await
        .unwrap();
    assert_eq!(next_text(&mut listener).await["error"]["code"], "forbidden");

    // Across a restart: kept messages are there, ids go on rising, and a new
    // listener without `since` gets the `last` ones first.
    drop((host, agent, listener));
    daemon.stop();
    daemon.run();
    let mut host = daemon.host().await;
    let kept: api::Messages = host.get("/v1/channel?since=0").await.unwrap();
    assert_eq!(kept.messages.len(), 4);
    let mut fresh = daemon.stream("control", "/v1/channel").await;
    assert_eq!(next_text(&mut fresh).await["id"], state.id);
    let after: api::Message = host
        .post("/v1/channel/small/log", &json!({"body": "after"}))
        .await
        .unwrap();
    assert!(after.id > gone.id);
    assert_eq!(next_text(&mut fresh).await["body"], "after");
    let since: api::Messages = host
        .get(&format!("/v1/channel?since={}", gone.id))
        .await
        .unwrap();
    // What the listener published before the restart, and the one after it.
    assert_eq!(since.messages.len(), 2);
}

#[tokio::test]
async fn services_answer_calls_through_their_socket() {
    let services = json!([
        {"name": "plug/open", "agent": true, "timeout": 1, "methods": [{"name": "echo"}]},
        {"name": "plug/closed", "agent": false},
    ]);
    let daemon = Daemon::start(&[("services.json", services)]);
    let mut host = daemon.host().await;
    let mut agent = daemon.agent().await;
    assert_eq!(
        code(agent.post("/v1/services/plug/open/echo", &json!({})).await),
        (503, "not_served".into())
    );
    assert_eq!(
        code(
            agent
                .post("/v1/services/plug/closed/echo", &json!({}))
                .await
        )
        .0,
        403
    );
    assert_eq!(
        code(host.post("/v1/services/no/such/echo", &json!({})).await).0,
        404
    );

    // A program that answers `echo`, fails `fail` and never answers `hang`.
    let mut serving = daemon
        .stream("control", "/v1/services/plug/open/serve")
        .await;
    let program = tokio::spawn(async move {
        while let Some(Ok(frame)) = serving.next().await {
            let Frame::Text(text) = frame else { continue };
            let call: api::CallFrame = serde_json::from_str(&text).unwrap();
            let reply = match call.method.as_str() {
                "echo" => {
                    json!({"id": call.id, "result": {"body": call.body, "level": call.from.level}})
                }
                "fail" => {
                    json!({"id": call.id, "error": {"code": "nope", "message": "no such thing"}})
                }
                _ => continue,
            };
            serving
                .send(Frame::Text(reply.to_string().into()))
                .await
                .unwrap();
        }
    });
    let reply: Value = agent
        .post("/v1/services/plug/open/echo", &json!({"x": 1}))
        .await
        .unwrap();
    assert_eq!(reply, json!({"body": {"x": 1}, "level": "agent"}));
    match host
        .post::<Value>("/v1/services/plug/open/fail", &json!({}))
        .await
    {
        Err(error) => assert_eq!(
            (error.status(), error.to_string()),
            (520, "no such thing".into())
        ),
        Ok(_) => panic!("fail fails"),
    }
    assert_eq!(
        code(host.post("/v1/services/plug/open/hang", &json!({})).await),
        (504, "timeout".into())
    );
    assert!(
        client::websocket_unix(&daemon.socket("control"), "/v1/services/plug/open/serve")
            .await
            .is_err_and(|error| error.status() == 409)
    );

    // An undeclared one, served from the agent's level, can be called from there and from the host.
    let mut own = daemon.stream("agent", "/v1/services/mine/test/serve").await;
    let listed: Vec<api::Service> = agent.get("/v1/services").await.unwrap();
    let names: Vec<(&str, bool, bool)> = listed
        .iter()
        .map(|service| (service.name.as_str(), service.declared, service.up))
        .collect();
    assert_eq!(
        names,
        [("mine/test", false, true), ("plug/open", true, true)]
    );
    let call = tokio::spawn(async move {
        host.post::<Value>("/v1/services/mine/test/anything", &json!(7))
            .await
    });
    let frame = next_text(&mut own).await;
    assert_eq!(
        (frame["method"].as_str(), &frame["body"]),
        (Some("anything"), &json!(7))
    );
    own.send(Frame::Text(
        json!({"id": frame["id"], "result": "ok"})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    assert_eq!(call.await.unwrap().unwrap(), json!("ok"));

    // When the program goes, the service is down.
    program.abort();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        code(agent.post("/v1/services/plug/open/echo", &json!({})).await).0,
        503
    );
}

#[tokio::test]
async fn a_restart_ends_sessions_and_keeps_their_transcripts() {
    let mut daemon =
        Daemon::start(&[("tasks.json", json!([{"name": "one", "run": "echo first"}]))]);
    let mut host = daemon.host().await;
    let started: Value = host
        .post(
            "/v1/sessions",
            &json!({"kind": "process", "command": "echo before; sleep 60"}),
        )
        .await
        .unwrap();
    let id = started["id"].as_str().unwrap().to_owned();
    tokio::time::sleep(Duration::from_millis(200)).await;

    // A reload takes up new tasks and leaves the session running.
    std::fs::write(
        daemon.dir.path().join("etc/tasks.json"),
        json!([{"name": "two", "run": "true"}]).to_string(),
    )
    .unwrap();
    let _: Value = host.post("/v1/reload", &json!({})).await.unwrap();
    let tasks: Vec<api::Task> = host.get("/v1/tasks").await.unwrap();
    assert_eq!(tasks[0].name, "two");
    let state: api::Session = host.get(&format!("/v1/sessions/{id}")).await.unwrap();
    assert!(state.running());
    std::fs::write(daemon.dir.path().join("etc/tasks.json"), "not json").unwrap();
    assert_eq!(code(host.post("/v1/reload", &json!({})).await).0, 400);

    drop(host);
    daemon.stop();
    daemon.run();
    let mut host = daemon.host().await;
    let state: api::Session = host.get(&format!("/v1/sessions/{id}")).await.unwrap();
    assert!(!state.running() && state.lost);
    let output: api::Output = host
        .get(&format!("/v1/sessions/{id}/output"))
        .await
        .unwrap();
    assert_eq!(unb64(&output.data_b64).unwrap(), b"before\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_portal_carries_calls_and_streams_together() {
    use berth_api::portal::{Piece, Portal};
    let daemon = Daemon::start(&[]);
    let socket = daemon.stream("control", "/v1/portal").await;
    let portal = Portal::over(socket);

    // Calls, at once, each answered under its own number.
    let (version, sessions) = tokio::join!(
        portal.call("version", None, Value::Null, None),
        portal.call("sessions.list", None, Value::Null, None),
    );
    assert_eq!(version.unwrap()["api"], api::API);
    assert_eq!(sessions.unwrap(), json!([]));

    // Arguments go into the path and the query, data is the body.
    let run = json!({"kind": "process", "command": "printf hello"});
    let done = portal
        .call("sessions.start", None, json!({"wait": "10s"}), Some(run))
        .await
        .unwrap();
    assert_eq!(text(&done["stdout_b64"]), "hello");
    let id = done["id"].as_str().unwrap().to_owned();
    let output = portal
        .call(
            "sessions.output",
            None,
            json!({"id": id, "since": 1, "limit": 3}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(unb64(output["data_b64"].as_str().unwrap()).unwrap(), b"ell");

    // Errors keep their code and status; unknown names and missing arguments are errors too.
    let missing = portal
        .call("sessions.get", None, json!({"id": "nothing"}), None)
        .await
        .unwrap_err();
    assert_eq!((missing.status(), missing.code()), (404, "not_found"));
    let unknown = portal.call("no.such", None, Value::Null, None).await;
    assert_eq!(unknown.unwrap_err().status(), 404);
    let incomplete = portal.call("sessions.get", None, Value::Null, None).await;
    assert_eq!(incomplete.unwrap_err().status(), 400);

    // Bytes are base64 both ways.
    let file = format!("{}/note.txt", daemon.path("files"));
    let written = portal
        .call(
            "files.write",
            None,
            json!({"path": file}),
            Some(json!({"base64": api::b64(b"over the portal")})),
        )
        .await
        .unwrap();
    assert_eq!(written["ok"], true);
    let read = portal
        .call("files.read", None, json!({"path": file}), None)
        .await
        .unwrap();
    assert_eq!(
        unb64(read["base64"].as_str().unwrap()).unwrap(),
        b"over the portal"
    );

    // A terminal: output and input as binary frames, an end when it exits,
    // while a second stream and calls go on beside it.
    let start = json!({"kind": "terminal", "command": ["sh", "-c", "read line; echo got-$line"]});
    let session = portal
        .call("sessions.start", None, Value::Null, Some(start))
        .await
        .unwrap();
    let id = session["id"].as_str().unwrap();
    let mut terminal = portal
        .open("sessions.attach", None, json!({"id": id}))
        .await
        .unwrap();
    let mut channel = portal
        .open("channel.listen", None, json!({"topic": "t/"}))
        .await
        .unwrap();
    terminal.push(Piece::Binary(b"abc\r".to_vec())).unwrap();
    let mut seen = Vec::new();
    let exited = loop {
        match tokio::time::timeout(Duration::from_secs(10), terminal.recv())
            .await
            .unwrap()
        {
            Some(Ok(Piece::Binary(bytes))) => seen.extend(bytes),
            Some(Ok(Piece::Text(frame))) if frame.get("exited").is_some() => break frame,
            Some(Ok(Piece::Text(_))) => {}
            other => panic!("the terminal ended early: {other:?}"),
        }
    };
    assert!(String::from_utf8_lossy(&seen).contains("got-abc"));
    assert_eq!(exited["exited"]["exit_code"], 0);

    let published = portal
        .call(
            "channel.publish",
            None,
            json!({"topic": "t/one"}),
            Some(json!({"body": {"n": 1}})),
        )
        .await
        .unwrap();
    let heard = tokio::time::timeout(Duration::from_secs(10), channel.recv())
        .await
        .unwrap();
    match heard {
        Some(Ok(Piece::Text(message))) => assert_eq!(message["id"], published["id"]),
        other => panic!("no message: {other:?}"),
    }
    // Ending a stream from this side leaves the portal usable.
    drop(channel);
    assert!(
        portal
            .call("version", None, Value::Null, None)
            .await
            .is_ok()
    );

    // The agent's socket has the portal too, with the agent's rights.
    let agent = Portal::over(daemon.stream("agent", "/v1/portal").await);
    assert!(
        agent
            .call("tasks.list", None, Value::Null, None)
            .await
            .is_ok()
    );
    let refused = agent.call("sessions.list", None, Value::Null, None).await;
    assert_eq!(refused.unwrap_err().status(), 403);
}

/// The committed API documents are what the program generates. Run with
/// BERTH_UPDATE_DOCS=1 to write them after a change to the API.
#[test]
fn the_committed_documents_are_current() {
    let docs = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/openapi");
    for (name, document) in [
        ("berthd.openapi.json", berthd::openapi()),
        ("berthd.asyncapi.json", berthd::asyncapi()),
        ("berthd.operations.json", berthd::operations()),
    ] {
        let text = serde_json::to_string_pretty(&document).unwrap() + "\n";
        let path = docs.join(name);
        if std::env::var_os("BERTH_UPDATE_DOCS").is_some() {
            std::fs::create_dir_all(&docs).unwrap();
            std::fs::write(&path, &text).unwrap();
        }
        let committed = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            committed == text,
            "{name} is out of date: run BERTH_UPDATE_DOCS=1 cargo test -p berthd the_committed_documents"
        );
    }
}
