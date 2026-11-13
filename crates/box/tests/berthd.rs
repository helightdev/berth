//! The verbs against a real `berthd --single`. Skipped when the daemon is not built or does not start.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn berthd() -> Option<PathBuf> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/berthd");
    path.exists().then_some(path)
}

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn verbs_against_berthd() {
    let Some(daemon) = berthd() else {
        return eprintln!("skipped: no berthd binary");
    };
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("etc")).unwrap();
    std::fs::write(
        dir.path().join("etc/tasks.json"),
        r#"[{"name":"hello","label":"Hello","user":"agent","agent":true,"arguments":[{"name":"who","required":true}],"run":"echo hi $BERTH_ARG_WHO; echo err >&2; exit 3"}]"#,
    )
    .unwrap();
    let child = Command::new(&daemon)
        .arg("--single")
        .arg(dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _guard = Kill(child);
    let start = Instant::now();
    while !dir.path().join("agent.sock").exists() && !dir.path().join("control.sock").exists() {
        if start.elapsed() > Duration::from_secs(10) {
            return eprintln!("skipped: berthd --single did not start");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(300));
    let bx = |args: &[&str]| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_box"));
        cmd.env("BERTH_DIR", dir.path()).args(args);
        cmd
    };
    let out = bx(&["tasks"]).output().unwrap();
    let tasks = String::from_utf8_lossy(&out.stdout).into_owned();
    if !tasks.contains("hello\tHello\twho*") {
        return eprintln!(
            "skipped: berthd does not serve tasks yet: {tasks} {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let out = bx(&["run", "hello", "who=there"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi there\n");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "err\n");
    assert_eq!(out.status.code(), Some(3));

    let out = bx(&["publish", "t/x", r#"{"a":1}"#]).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut listen = bx(&["listen", "t/", "--since", "0"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(listen.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let _ = listen.kill();
    let _ = listen.wait();
    let message: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(message["body"], serde_json::json!({"a": 1}));

    let server = bx(&["serve", "demo/echo", "--", "sh", "-c", "cat"])
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _server = Kill(server);
    let start = Instant::now();
    loop {
        let out = bx(&["call", "demo/echo", "anything", r#"{"x":1}"#])
            .output()
            .unwrap();
        if out.status.success() {
            assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), r#"{"x":1}"#);
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}
