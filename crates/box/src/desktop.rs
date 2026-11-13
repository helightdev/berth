//! The desktop tools: `scrot`, `xdotool` and `xclip` behind one lock, as `desktop.py` did.

use berth_api::b64;
use serde_json::{Value, json};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub type ToolResult = Value;

/// Tool name, description and properties (name, JSON type).
type Tool = (
    &'static str,
    &'static str,
    &'static [(&'static str, &'static str)],
);
pub const TOOLS: &[Tool] = &[
    ("screenshot", "Capture the shared desktop as PNG.", &[]),
    (
        "click",
        "Click a screen coordinate.",
        &[("x", "integer"), ("y", "integer"), ("button", "integer")],
    ),
    ("type", "Type literal UTF-8 text.", &[("text", "string")]),
    (
        "key",
        "Press an xdotool key chord, e.g. ctrl+l.",
        &[("key", "string")],
    ),
    (
        "scroll",
        "Scroll up or down a number of steps.",
        &[("direction", "string"), ("steps", "integer")],
    ),
    ("clipboard", "Read the clipboard text.", &[]),
];

fn run(argv: &[&str]) -> Result<Vec<u8>, String> {
    let shown = format!(
        "[{}]",
        argv.iter()
            .map(|a| format!("'{a}'"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let mut child = Command::new(argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| match e.raw_os_error() {
            Some(code) => format!("[Errno {code}] {}: '{}'", os_text(&e), argv[0]),
            None => e.to_string(),
        })?;
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out.read_to_end(&mut buf);
        buf
    });
    let drain = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = err.read_to_end(&mut buf);
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("Command '{shown}' timed out after 20 seconds"));
            }
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    };
    let stdout = reader.join().unwrap_or_default();
    let _ = drain.join();
    if !status.success() {
        let code = status.code().unwrap_or(-1);
        return Err(format!(
            "Command '{shown}' returned non-zero exit status {code}."
        ));
    }
    Ok(stdout)
}

fn os_text(error: &std::io::Error) -> String {
    let text = error.to_string();
    text.split(" (os error").next().unwrap_or(&text).to_string()
}

fn integer(
    args: &Value,
    key: &str,
    low: i64,
    high: i64,
    default: Option<i64>,
) -> Result<i64, String> {
    let value = match args.get(key) {
        Some(Value::Number(n)) => n.as_i64(),
        Some(_) => None,
        None => default,
    };
    match value {
        Some(v) if (low..=high).contains(&v) => Ok(v),
        _ => Err(format!("{key} must be an integer from {low} to {high}")),
    }
}

fn textarg<'a>(args: &'a Value, key: &str, limit: usize) -> Result<&'a str, String> {
    match args.get(key).and_then(Value::as_str) {
        Some(s) if !s.is_empty() && s.chars().count() <= limit && !s.contains('\0') => Ok(s),
        _ => Err(format!(
            "{key} must be nonempty text, at most {limit} characters, without NUL"
        )),
    }
}

pub fn folder() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(format!("/tmp/devbox-{}", unsafe { libc::getuid() })),
    }
}

/// Hold the desktop lock until the returned file is dropped.
fn lock(folder: &Path) -> Result<std::fs::File, String> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(folder)
        .map_err(|e| e.to_string())?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(folder.join("desktop.lock"))
        .map_err(|e| e.to_string())?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(file)
}

/// `pause` or `resume`: set or clear the marker that makes input tools refuse.
pub fn set_paused(paused: bool) -> Result<(), String> {
    let folder = folder();
    let _lock = lock(&folder)?;
    let marker = folder.join("desktop.paused");
    if paused {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&marker);
        file.map(drop).map_err(|e| e.to_string())
    } else {
        match std::fs::remove_file(&marker) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
            _ => Ok(()),
        }
    }
}

pub fn perform(name: &str, args: &Value) -> Result<ToolResult, String> {
    let Some((_, _, props)) = TOOLS.iter().find(|t| t.0 == name) else {
        return Err("Unknown desktop tool or invalid arguments".into());
    };
    let Some(map) = args.as_object() else {
        return Err("Unknown desktop tool or invalid arguments".into());
    };
    if map.keys().any(|k| !props.iter().any(|p| p.0 == k)) {
        return Err("Unexpected tool argument".into());
    }
    let folder = folder();
    let _lock = lock(&folder)?;
    if name != "screenshot" && name != "clipboard" && folder.join("desktop.paused").exists() {
        return Err("Desktop input paused for human takeover".into());
    }
    match name {
        "screenshot" => {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos());
            let dir =
                std::env::temp_dir().join(format!("devbox-shot-{}-{nanos}", std::process::id()));
            std::fs::create_dir(&dir).map_err(|e| e.to_string())?;
            let path = dir.join("screen.png");
            let result = run(&["scrot", &path.to_string_lossy()])
                .and_then(|_| std::fs::read(&path).map_err(|e| e.to_string()));
            let _ = std::fs::remove_dir_all(&dir);
            return Ok(json!({"type": "image", "mimeType": "image/png", "data": b64(&result?)}));
        }
        "click" => {
            let geometry =
                String::from_utf8_lossy(&run(&["xdotool", "getdisplaygeometry"])?).into_owned();
            let sizes: Vec<i64> = geometry
                .split_whitespace()
                .filter_map(|s| s.parse().ok())
                .collect();
            let [width, height] = sizes[..] else {
                return Err("not enough values to unpack (expected 2)".into());
            };
            let x = integer(args, "x", 0, width - 1, None)?;
            let y = integer(args, "y", 0, height - 1, None)?;
            let button = integer(args, "button", 1, 3, Some(1))?;
            run(&[
                "xdotool",
                "mousemove",
                "--sync",
                &x.to_string(),
                &y.to_string(),
                "click",
                &button.to_string(),
            ])?;
        }
        "type" => {
            run(&[
                "xdotool",
                "type",
                "--clearmodifiers",
                "--delay",
                "1",
                "--",
                textarg(args, "text", 16384)?,
            ])?;
        }
        "key" => {
            let key = textarg(args, "key", 128)?;
            if key.starts_with('-') || key.chars().any(char::is_whitespace) {
                return Err("Invalid key chord".into());
            }
            run(&["xdotool", "key", "--clearmodifiers", key])?;
        }
        "scroll" => {
            let button = match args.get("direction").and_then(Value::as_str) {
                Some("up") => "4",
                Some("down") => "5",
                _ => return Err("direction must be up or down".into()),
            };
            let steps = integer(args, "steps", 1, 50, Some(3))?;
            run(&[
                "xdotool",
                "click",
                "--repeat",
                &steps.to_string(),
                "--delay",
                "50",
                button,
            ])?;
        }
        _ => {
            let out = run(&["xclip", "-selection", "clipboard", "-o"])?;
            let text: String = String::from_utf8_lossy(&out).chars().take(65536).collect();
            return Ok(json!({"type": "text", "text": text}));
        }
    }
    Ok(json!({"type": "text", "text": "ok"}))
}

/// One JSON-RPC request as `desktop.py`'s `handle()`; `None` for a notification.
pub fn handle(request: &Value) -> Option<Value> {
    let Some(object) = request.as_object() else {
        return Some(
            json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32600, "message": "Invalid request"}}),
        );
    };
    let id = object.get("id")?.clone();
    let error = |code: i32, message: &str| {
        Some(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}))
    };
    let empty = json!({});
    let params = object.get("params").unwrap_or(&empty);
    let Some(params) = params.as_object() else {
        return error(-32602, "params must be an object");
    };
    let result = match object.get("method").and_then(Value::as_str) {
        Some("initialize") => {
            let requested = params.get("protocolVersion").and_then(Value::as_str);
            let version = requested
                .filter(|v| ["2024-11-05", "2025-03-26", "2025-06-18"].contains(v))
                .unwrap_or("2024-11-05");
            json!({"protocolVersion": version, "capabilities": {"tools": {}},
                   "serverInfo": {"name": "box", "version": berth_api::VERSION}})
        }
        Some("ping") => json!({}),
        Some("tools/list") => {
            let tools: Vec<Value> = TOOLS
                .iter()
                .map(|(name, description, props)| {
                    let properties: serde_json::Map<String, Value> = props.iter().map(|(k, t)| (k.to_string(), json!({"type": t}))).collect();
                    let required: Vec<&str> = props.iter().map(|p| p.0).filter(|k| *k != "button" && *k != "steps").collect();
                    json!({"name": name, "description": description, "inputSchema":
                        {"type": "object", "properties": properties, "required": required, "additionalProperties": false}})
                })
                .collect();
            json!({"tools": tools})
        }
        Some("tools/call") => {
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            match perform(
                params.get("name").and_then(Value::as_str).unwrap_or(""),
                &args,
            ) {
                Ok(content) => json!({"content": [content]}),
                Err(message) => {
                    json!({"isError": true, "content": [{"type": "text", "text": message}]})
                }
            }
        }
        _ => return error(-32601, "Method not found"),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

/// `box mcp`: newline-delimited JSON-RPC on stdio.
pub fn serve_mcp() {
    use std::io::{BufRead, Write};
    let mut out = std::io::stdout();
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(request) => handle(&request),
            Err(_) => Some(
                json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "Parse error"}}),
            ),
        };
        if let Some(reply) = reply {
            let _ = writeln!(out, "{reply}");
            let _ = out.flush();
        }
    }
}
