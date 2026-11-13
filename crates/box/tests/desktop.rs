use serde_json::{Value, json};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

struct Env {
    bin: tempfile::TempDir,
    run: tempfile::TempDir,
}

impl Env {
    fn new() -> Env {
        let env = Env {
            bin: tempfile::tempdir().unwrap(),
            run: tempfile::tempdir().unwrap(),
        };
        let log = env.bin.path().join("log");
        let scripts = [
            (
                "scrot",
                "#!/bin/sh\nfor last; do :; done\nprintf 'PNGDATA' > \"$last\"\n".to_string(),
            ),
            (
                "xdotool",
                format!(
                    "#!/bin/sh\nif [ \"$1\" = getdisplaygeometry ]; then echo 1280 800; else echo \"$@\" >> {}; fi\n",
                    log.display()
                ),
            ),
            ("xclip", "#!/bin/sh\nprintf 'clip text'\n".to_string()),
        ];
        for (name, body) in scripts {
            let path = env.bin.path().join(name);
            std::fs::write(&path, body).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        env
    }

    fn command(&self, program: &Path) -> Command {
        let mut cmd = Command::new(program);
        let path = format!(
            "{}:{}",
            self.bin.path().display(),
            std::env::var("PATH").unwrap()
        );
        cmd.env("PATH", path)
            .env("XDG_RUNTIME_DIR", self.run.path());
        cmd
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.bin.path().join("log")).unwrap_or_default()
    }
}

fn exchange(mut cmd: Command, input: &str) -> Vec<Value> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn call(id: u32, name: &str, arguments: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": arguments}}).to_string()
}

fn text(reply: &Value) -> &str {
    reply["result"]["content"][0]["text"].as_str().unwrap()
}

#[test]
fn mcp_session() {
    let env = Env::new();
    let mut cmd = env.command(Path::new(env!("CARGO_BIN_EXE_box")));
    cmd.arg("mcp");
    let lines = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-03-26"}}).to_string(),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}).to_string(),
        call(3, "screenshot", json!({})),
        call(4, "click", json!({"x": 10, "y": 20, "button": 2})),
        call(5, "type", json!({"text": "hello"})),
        call(6, "key", json!({"key": "ctrl+l"})),
        call(7, "scroll", json!({"direction": "up", "steps": 2})),
        call(8, "clipboard", json!({})),
        call(9, "click", json!({"x": 1280, "y": 0})),
        json!({"jsonrpc": "2.0", "id": 10, "method": "nope"}).to_string(),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string(),
        "not json".to_string(),
        json!({"jsonrpc": "2.0", "id": 11, "method": "ping", "params": []}).to_string(),
        "[1]".to_string(),
        call(12, "key", json!({"key": "-x"})),
        call(13, "bogus", json!({})),
        json!({"jsonrpc": "2.0", "id": 14, "method": "initialize", "params": {"protocolVersion": "1999"}}).to_string(),
    ];
    let r = exchange(cmd, &(lines.join("\n") + "\n"));
    assert_eq!(r.len(), 16, "{r:?}");
    assert_eq!(r[0]["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(r[0]["result"]["serverInfo"]["name"], "box");
    let tools = r[1]["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 6);
    assert_eq!(tools[1]["inputSchema"]["required"], json!(["x", "y"]));
    assert_eq!(r[2]["result"]["content"][0]["type"], "image");
    assert_eq!(r[2]["result"]["content"][0]["data"], "UE5HREFUQQ==");
    assert_eq!(text(&r[3]), "ok");
    assert_eq!(text(&r[7]), "clip text");
    assert_eq!(r[8]["result"]["isError"], true);
    assert_eq!(text(&r[8]), "x must be an integer from 0 to 1279");
    assert_eq!(r[9]["error"]["code"], -32601);
    assert_eq!(r[10]["error"]["code"], -32700);
    assert_eq!(r[10]["id"], Value::Null);
    assert_eq!(r[11]["error"]["code"], -32602);
    assert_eq!(r[12]["error"]["code"], -32600);
    assert_eq!(text(&r[13]), "Invalid key chord");
    assert_eq!(text(&r[14]), "Unknown desktop tool or invalid arguments");
    assert_eq!(r[15]["result"]["protocolVersion"], "2024-11-05");
    let log = env.log();
    assert!(log.contains("mousemove --sync 10 20 click 2"), "{log}");
    assert!(
        log.contains("type --clearmodifiers --delay 1 -- hello"),
        "{log}"
    );
    assert!(log.contains("key --clearmodifiers ctrl+l"), "{log}");
    assert!(log.contains("click --repeat 2 --delay 50 4"), "{log}");
}

#[test]
fn pause_and_resume() {
    let env = Env::new();
    let bin = Path::new(env!("CARGO_BIN_EXE_box"));
    let desktop = |args: &[&str]| env.command(bin).arg("desktop").args(args).output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&desktop(&["pause"]).stdout),
        "pause\n"
    );
    assert!(env.run.path().join("desktop.paused").exists());
    let refused = desktop(&["click", "5", "5"]);
    assert_eq!(refused.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&refused.stderr),
        "Desktop input paused for human takeover\n"
    );
    assert_eq!(
        String::from_utf8_lossy(&desktop(&["resume"]).stdout),
        "resume\n"
    );
    let ok = desktop(&["click", "5", "5"]);
    assert!(ok.status.success());
    assert!(String::from_utf8_lossy(&ok.stdout).contains("\"ok\""));
}

#[test]
fn screenshot_output() {
    let env = Env::new();
    let bin = Path::new(env!("CARGO_BIN_EXE_box"));
    let out = env
        .command(bin)
        .args(["desktop", "screenshot", "--output", "-"])
        .output()
        .unwrap();
    assert_eq!(out.stdout, b"PNGDATA");
    let path = env.run.path().join("shot.png");
    let out = env
        .command(bin)
        .args(["desktop", "screenshot", "--output"])
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("{}\n", path.display())
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"PNGDATA");
    let out = env
        .command(bin)
        .args([
            "desktop",
            "scroll",
            "down",
            "--args",
            "{\"direction\":\"down\"}",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = env
        .command(bin)
        .args(["desktop", "screenshot"])
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["mimeType"], "image/png");
}

#[test]
fn devbox_desktop_name() {
    let env = Env::new();
    let links = tempfile::tempdir().unwrap();
    let link = links.path().join("devbox-desktop");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_box"), &link).unwrap();
    let mut cmd = env.command(&link);
    cmd.arg("--mcp");
    let r = exchange(
        cmd,
        &(json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}).to_string() + "\n"),
    );
    assert_eq!(r[0]["result"], json!({}));
    let out = env
        .command(&link)
        .args(["click", "--args", "{\"x\":1,\"y\":2}"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("ok"));
    assert!(env.log().contains("mousemove --sync 1 2 click 1"));
}
