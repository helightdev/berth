//! The command line as a script sees it: the exit statuses of the documented
//! table, and `-o json` printing the API's own documents. It runs the berth
//! that was built, against a `berth serve` and a server in its own process,
//! in scratch directories, with made-up hosts.
#![cfg(target_os = "linux")]

use serde_json::Value;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A scratch installation: its directories, and the `berth serve` of it
/// while one runs.
struct Scratch {
    dir: tempfile::TempDir,
    serve: Option<Child>,
}

impl Scratch {
    fn new() -> Scratch {
        let dir = tempfile::Builder::new().prefix("bc").tempdir().unwrap();
        for sub in ["r", "c", "d", "home"] {
            std::fs::create_dir_all(dir.path().join(sub)).unwrap();
        }
        Scratch { dir, serve: None }
    }

    fn command(&self) -> Command {
        let path = self.dir.path();
        let mut command = Command::new(env!("CARGO_BIN_EXE_berth"));
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", path.join("home"))
            .env("XDG_RUNTIME_DIR", path.join("r"))
            .env("XDG_CONFIG_HOME", path.join("c"))
            .env("XDG_DATA_HOME", path.join("d"))
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    /// Run berth with `args`: its exit status, standard output and error.
    fn run(&self, args: &[&str]) -> (i32, String, String) {
        let done = self.command().args(args).output().unwrap();
        (
            done.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&done.stdout).into_owned(),
            String::from_utf8_lossy(&done.stderr).into_owned(),
        )
    }

    fn socket(&self) -> PathBuf {
        self.dir.path().join("r/berth/serve.sock")
    }

    /// Start `berth serve` on its socket and wait until it answers.
    fn serve(&mut self) {
        let child = self
            .command()
            .args(["serve"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        self.serve = Some(child);
        let start = Instant::now();
        while std::os::unix::net::UnixStream::connect(self.socket()).is_err() {
            assert!(
                start.elapsed() < Duration::from_secs(20),
                "berth serve did not start"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// What the server answers to `GET path`, as JSON.
    fn get(&self, path: &str) -> Value {
        get(&self.socket(), path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Some(mut serve) = self.serve.take() {
            let _ = serve.kill();
            let _ = serve.wait();
        }
    }
}

fn get(socket: &Path, path: &str) -> Value {
    let mut stream = std::os::unix::net::UnixStream::connect(socket).unwrap();
    write!(stream, "GET {path} HTTP/1.0\r\nHost: berth\r\n\r\n").unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).unwrap();
    let (head, body) = answer.split_once("\r\n\r\n").unwrap();
    assert!(
        head.starts_with("HTTP/1.0 200") || head.starts_with("HTTP/1.1 200"),
        "{path}: {head}"
    );
    serde_json::from_str(body).unwrap_or_else(|error| panic!("{path}: {error}: {body}"))
}

#[test]
fn exit_statuses_follow_the_documented_table() {
    let scratch = Scratch::new();
    let (status, _, said) = scratch.run(&[
        "host",
        "add",
        "studio",
        "192.0.2.10",
        "--type",
        "ssh",
        "--no-contact",
    ]);
    assert_eq!(status, 0, "{said}");
    let table: &[(&[&str], i32)] = &[
        // 0: done.
        (&["--version"], 0),
        (&[], 0),
        (&["help"], 0),
        (&["host", "list"], 0),
        (&["-o", "json", "job", "list"], 0),
        // 1: the work failed.
        (&["host", "show", "dock"], 1),
        (&["host", "add", "studio", "192.0.2.11", "--no-contact"], 1),
        // 2: the command line was wrong.
        (&["aply", "studio"], 2),
        (&["host", "show"], 2),
        (&["host", "list", "--colour"], 2),
        (&["-o", "yaml", "host", "list"], 2),
        (&["host", "create", "--no-input"], 2),
        // 3: refused.
        (&["host", "apply", "studio", "--no-input"], 3),
        (&["host", "remove", "studio", "--no-input"], 3),
        // 4: the server could not be reached.
        (&["--server", "http://127.0.0.1:9", "host", "list"], 4),
        (
            &["--server", "unix:/nonexistent/serve.sock", "job", "list"],
            4,
        ),
    ];
    for (args, expected) in table {
        let (status, _, said) = scratch.run(args);
        assert_eq!(status, *expected, "berth {}: {said}", args.join(" "));
    }
    // With -o json, a failure is an object on standard error with the same status.
    let (status, out, said) = scratch.run(&["-o", "json", "aply"]);
    assert_eq!((status, out.as_str()), (2, ""));
    let error: Value = serde_json::from_str(said.trim()).unwrap();
    assert_eq!(error["error"]["exit"], 2);
    let (status, _, said) = scratch.run(&["-o", "json", "host", "apply", "studio", "--no-input"]);
    assert_eq!(status, 3);
    let error: Value = serde_json::from_str(said.trim()).unwrap();
    assert_eq!(error["error"]["exit"], 3, "{error}");
    // A confirmation answered with -y goes on.
    let (status, _, said) = scratch.run(&["host", "remove", "studio", "-y"]);
    assert_eq!(status, 0, "{said}");
}

#[test]
fn json_is_the_documents_of_the_api() {
    let mut scratch = Scratch::new();
    let (status, _, said) = scratch.run(&[
        "host",
        "add",
        "studio",
        "192.0.2.10",
        "--type",
        "ssh",
        "--no-contact",
    ]);
    assert_eq!(status, 0, "{said}");
    let (status, _, said) = scratch.run(&["key", "create", "spare"]);
    assert_eq!(status, 0, "{said}");
    scratch.serve();
    let json = |args: &[&str]| -> Value {
        let mut all = vec!["-o", "json"];
        all.extend(args);
        let (status, out, said) = scratch.run(&all);
        assert_eq!(status, 0, "berth {}: {said}", all.join(" "));
        serde_json::from_str(&out)
            .unwrap_or_else(|error| panic!("berth {}: {error}: {out}", all.join(" ")))
    };
    // What does not change between two reads is the same document.
    let same: &[(&[&str], &str)] = &[
        (&["key", "list"], "/v1/keys"),
        (&["secret", "list"], "/v1/secrets"),
        (&["registry", "list"], "/v1/registries"),
        (&["provisioner", "list"], "/v1/provisioners"),
        (&["provisioner", "types"], "/v1/provisioner-types"),
        (&["resource", "list"], "/v1/resources"),
        (&["plugin", "list"], "/v1/plugins"),
        (&["job", "list"], "/v1/jobs?limit=20"),
    ];
    for (args, path) in same {
        assert_eq!(json(args), scratch.get(path), "berth {}", args.join(" "));
    }
    // A host's live state may move between two reads; what it is does not.
    let keys = |value: &Value| -> Vec<String> {
        let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        keys
    };
    let listed = json(&["host", "list"]);
    let served = scratch.get("/v1/hosts");
    assert_eq!(
        listed.as_array().unwrap().len(),
        served.as_array().unwrap().len()
    );
    assert_eq!(keys(&listed[0]), keys(&served[0]));
    assert_eq!(listed[0]["name"], "studio");
    let shown = json(&["host", "show", "studio"]);
    let got = scratch.get("/v1/hosts/studio");
    assert_eq!(keys(&shown), keys(&got));
    assert_eq!(
        (&shown["name"], &shown["type"]),
        (&got["name"], &got["type"])
    );
}
