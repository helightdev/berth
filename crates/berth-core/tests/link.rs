//! A link to a local berthd in single-user mode, as `ssh … berthd connect` would be.

use berth_core::link::{Link, LinkSpec};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

fn berthd() -> Option<std::path::PathBuf> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/berthd");
    path.is_file().then_some(path)
}

#[tokio::test]
async fn connections_are_kept_replaced_and_can_start_with_a_password() {
    let Some(berthd) = berthd() else {
        eprintln!("skipped: build berthd first (cargo build -p berthd)");
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("bl")
        .tempdir_in("/tmp")
        .unwrap();
    let start = || {
        std::process::Command::new(&berthd)
            .arg("--single")
            .arg(dir.path())
            .spawn()
            .unwrap()
    };
    let wait = || {
        for _ in 0..200 {
            if std::os::unix::net::UnixStream::connect(dir.path().join("control.sock")).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("berthd did not start");
    };
    let mut daemon = start();
    wait();
    // Each link command leaves a line behind, so connections can be counted.
    let count = dir.path().join("count");
    let script = format!(
        "echo x >> {}; exec {} connect --dir {}",
        count.display(),
        berthd.display(),
        dir.path().display()
    );
    let link = Link::new(LinkSpec {
        program: "sh".into(),
        args: vec!["-c".into(), script.clone()],
        first_line: None,
        via: None,
    });
    let made = || {
        std::fs::read_to_string(&count)
            .unwrap_or_default()
            .lines()
            .count()
    };

    // Requests one after another share a connection; a stream takes its own.
    for _ in 0..3 {
        let version: Value = link.get("/v1/version").await.unwrap();
        assert_eq!(version["mode"], "single");
    }
    assert_eq!(made(), 1);
    let started: Value = link
        .post(
            "/v1/sessions",
            &json!({"kind": "process", "command": "echo streamed"}),
        )
        .await
        .unwrap();
    let mut stream = link
        .stream(&format!(
            "/v1/sessions/{}?since=0",
            started["id"].as_str().unwrap()
        ))
        .await
        .unwrap();
    let mut seen = vec![];
    while let Some(Ok(frame)) = stream.next().await {
        if frame.is_binary() {
            seen.extend_from_slice(&frame.into_data()[1..]);
        } else if frame.is_text() {
            break;
        }
    }
    assert_eq!(seen, b"streamed\n");
    assert_eq!(made(), 2);
    // Two at once need two.
    let (a, b) = tokio::join!(
        link.get::<Value>("/v1/stats"),
        link.get::<Value>("/v1/stats")
    );
    assert!(a.is_ok() && b.is_ok());
    assert_eq!(made(), 3);

    // The daemon goes away and comes back: the kept connections are dead and
    // the next request quietly gets a new one.
    unsafe { libc::kill(daemon.id() as i32, libc::SIGTERM) };
    daemon.wait().unwrap();
    let down = link.get::<Value>("/v1/version").await.unwrap_err();
    assert!(Link::missing(&down), "{down}");
    daemon = start();
    wait();
    let version: Value = link.get("/v1/version").await.unwrap();
    assert_eq!(version["api"], berth_api::API);

    // A password goes first and never reaches berthd.
    let guarded = format!(
        "read password; [ \"$password\" = secret ] || {{ echo 'Sorry, try again.' >&2; exit 1; }}; {script}"
    );
    let spec = |line: &str| LinkSpec {
        program: "sh".into(),
        args: vec!["-c".into(), guarded.clone()],
        first_line: Some(line.into()),
        via: None,
    };
    let version: Value = Link::new(spec("secret")).get("/v1/version").await.unwrap();
    assert_eq!(version["mode"], "single");
    let refused = Link::new(spec("wrong"))
        .get::<Value>("/v1/version")
        .await
        .unwrap_err();
    assert_eq!(refused.to_string(), "Sorry, try again.");

    unsafe { libc::kill(daemon.id() as i32, libc::SIGTERM) };
    daemon.wait().unwrap();
}
