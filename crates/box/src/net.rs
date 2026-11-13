//! The verbs that talk to berthd.

use crate::{assignments, json_or_string, serve, take_flag};
use berth_api::client::{self, Client, Frame, encode, encode_path};
use berth_api::{Service, Session, SessionKind, Task};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use tokio::io::AsyncReadExt;

pub fn dir() -> PathBuf {
    PathBuf::from(std::env::var_os("BERTH_DIR").unwrap_or_else(|| berth_api::RUN_DIR.into()))
}

/// The socket to use: the control one when it can be opened, else the agent's.
pub fn socket() -> Result<PathBuf, String> {
    let dir = dir();
    [berth_api::CONTROL_SOCKET, berth_api::AGENT_SOCKET]
        .iter()
        .map(|name| dir.join(name))
        .find(|path| std::os::unix::net::UnixStream::connect(path).is_ok())
        .ok_or_else(|| format!("berthd is not running (no socket in {})", dir.display()))
}

pub fn run_verb(verb: &str, args: Vec<String>) -> Result<i32, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let result = rt.block_on(verb_async(verb, args));
    // A blocked stdin read must not hold the process up.
    std::mem::forget(rt);
    result
}

fn e(error: impl std::fmt::Display) -> String {
    error.to_string()
}

async fn verb_async(verb: &str, mut args: Vec<String>) -> Result<i32, String> {
    let usage = |text: &str| Err(format!("usage: box {text}"));
    match (verb, args.len()) {
        ("tasks", 0) | ("services", 0) => {}
        ("run", 1..) | ("publish", 1..) | ("listen", 1..) | ("call", 2..) | ("serve", 3..) => {}
        ("tasks" | "services", _) => return usage(verb),
        ("run", _) => return usage("run TASK [NAME=VALUE…]"),
        ("publish", _) => return usage("publish TOPIC [JSON] [--keep none|last|log]"),
        ("listen", _) => return usage("listen TOPIC-PREFIX [--since ID]"),
        ("call", _) => return usage("call SERVICE METHOD [JSON]"),
        ("serve", _) => return usage("serve SERVICE -- COMMAND…"),
        _ => return Err(format!("unknown verb {verb}\ntry: box help")),
    }
    if verb == "serve" {
        return serve::serve(args).await;
    }
    let sock = socket()?;
    let mut api = Client::unix(&sock).await.map_err(e)?;
    match verb {
        "tasks" => {
            let tasks: Vec<Task> = api.get("/v1/tasks").await.map_err(e)?;
            for task in tasks.iter().filter(|t| t.extends.is_empty()) {
                let mut line = format!("{}\t{}", task.name, task.label);
                if !task.arguments.is_empty() {
                    let names: Vec<String> = task
                        .arguments
                        .iter()
                        .map(|a| format!("{}{}", a.name, if a.required { "*" } else { "" }))
                        .collect();
                    line += &format!("\t{}", names.join(" "));
                }
                println!("{line}");
            }
        }
        "services" => {
            let services: Vec<Service> = api.get("/v1/services").await.map_err(e)?;
            for s in services {
                let methods: Vec<&str> = s.methods.iter().map(|m| m.name.as_str()).collect();
                let (up, declared) = (
                    if s.up { "up" } else { "down" },
                    if s.declared { "declared" } else { "undeclared" },
                );
                println!("{}\t{up}\t{declared}\t{}", s.name, methods.join(","));
            }
        }
        "run" => {
            let task = args.remove(0);
            return run_task(&sock, &mut api, &task, &args).await;
        }
        "publish" => {
            let keep = take_flag(&mut args, "--keep")?;
            if let Some(k) = &keep
                && !["none", "last", "log"].contains(&k.as_str())
            {
                return Err("--keep must be none, last or log".into());
            }
            let topic = args.remove(0);
            let body = match args.first() {
                Some(text) => json_or_string(text),
                None => {
                    let mut text = String::new();
                    std::io::stdin().read_to_string(&mut text).map_err(e)?;
                    json_or_string(&text)
                }
            };
            let mut request = json!({"body": body});
            if let Some(k) = keep {
                request["keep"] = k.into();
            }
            let reply: Value = api
                .post(&format!("/v1/channel/{}", encode_path(&topic)), &request)
                .await
                .map_err(e)?;
            println!("{}", reply.get("id").unwrap_or(&reply));
        }
        "listen" => {
            let since = take_flag(&mut args, "--since")?;
            let mut target = format!("/v1/channel?topic={}", encode(&args[0]));
            if let Some(id) = since {
                target += &format!("&since={}", encode(&id));
            }
            let mut ws = client::websocket_unix(&sock, &target).await.map_err(e)?;
            let mut out = std::io::stdout();
            while let Some(Ok(frame)) = ws.next().await {
                if let Frame::Text(text) = frame {
                    let _ = writeln!(out, "{}", text.as_str());
                    let _ = out.flush();
                }
            }
        }
        "call" => {
            let body = match args.get(2) {
                Some(text) => {
                    serde_json::from_str(text).map_err(|x| format!("body is not JSON: {x}"))?
                }
                None => json!({}),
            };
            let path = format!(
                "/v1/services/{}/{}",
                encode_path(&args[0]),
                encode(&args[1])
            );
            let result: Value = api.post(&path, &body).await.map_err(e)?;
            println!("{result}");
        }
        _ => unreachable!(),
    }
    Ok(0)
}

/// Puts the local terminal in raw mode until dropped.
struct RawMode(libc::termios);

impl RawMode {
    fn enter() -> Option<RawMode> {
        unsafe {
            let mut saved = std::mem::zeroed::<libc::termios>();
            if libc::tcgetattr(0, &mut saved) != 0 {
                return None;
            }
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            (libc::tcsetattr(0, libc::TCSANOW, &raw) == 0).then_some(RawMode(saved))
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.0) };
    }
}

fn window_size() -> Option<(u16, u16)> {
    let mut size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let ok = unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut size) } == 0;
    (ok && size.ws_row > 0).then_some((size.ws_row, size.ws_col))
}

async fn winch(signal: &mut Option<tokio::signal::unix::Signal>) {
    match signal {
        Some(s) => {
            s.recv().await;
        }
        None => std::future::pending().await,
    }
}

async fn stdin_read(
    stdin: &mut Option<tokio::io::Stdin>,
    buf: &mut [u8],
) -> std::io::Result<usize> {
    match stdin {
        Some(s) => s.read(buf).await,
        None => std::future::pending().await,
    }
}

fn exit_status(code: Option<i64>) -> i32 {
    code.map_or(1, |n| (n & 0xff) as i32)
}

async fn run_task(
    sock: &std::path::Path,
    api: &mut Client,
    task: &str,
    words: &[String],
) -> Result<i32, String> {
    let mut body = json!({"arguments": assignments(words)?});
    let interactive = std::io::stdout().is_terminal();
    if interactive && let Some((rows, cols)) = window_size() {
        body["rows"] = rows.into();
        body["cols"] = cols.into();
    }
    let session: Session = api
        .post(&format!("/v1/tasks/{}", encode(task)), &body)
        .await
        .map_err(e)?;
    let mut ws = client::websocket_unix(
        sock,
        &format!("/v1/sessions/{}?since=0", encode(&session.id)),
    )
    .await
    .map_err(e)?;
    let terminal = session.kind == SessionKind::Terminal;
    let tty_in = terminal && std::io::stdin().is_terminal();
    let _raw = if tty_in { RawMode::enter() } else { None };
    let mut stdin = (tty_in && _raw.is_some()).then(tokio::io::stdin);
    let mut resize = if stdin.is_some() {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()).ok()
    } else {
        None
    };
    let (mut out, mut err) = (std::io::stdout(), std::io::stderr());
    let mut buf = [0u8; 4096];
    let mut exit = None;
    loop {
        tokio::select! {
            frame = ws.next() => match frame {
                Some(Ok(Frame::Binary(data))) => match (terminal, data.first()) {
                    (true, _) => { let _ = out.write_all(&data).and_then(|_| out.flush()); }
                    (false, Some(1)) => { let _ = out.write_all(&data[1..]).and_then(|_| out.flush()); }
                    (false, Some(2)) => { let _ = err.write_all(&data[1..]); }
                    _ => {}
                },
                Some(Ok(Frame::Text(text))) => {
                    if let Ok(v) = serde_json::from_str::<Value>(text.as_str())
                        && let Some(done) = v.get("exited")
                    {
                        exit = Some(exit_status(done.get("exit_code").and_then(Value::as_i64)));
                        break;
                    }
                }
                Some(Ok(_)) => {}
                _ => break,
            },
            n = stdin_read(&mut stdin, &mut buf) => match n {
                Ok(n) if n > 0 => { let _ = ws.send(Frame::Binary(buf[..n].to_vec().into())).await; }
                _ => stdin = None,
            },
            _ = winch(&mut resize) => {
                if let Some((rows, cols)) = window_size() {
                    let _ = ws.send(Frame::Text(json!({"resize": {"rows": rows, "cols": cols}}).to_string().into())).await;
                }
            }
        }
    }
    if let Some(code) = exit {
        return Ok(code);
    }
    let session: Session = api
        .get(&format!("/v1/sessions/{}", encode(&session.id)))
        .await
        .map_err(e)?;
    Ok(exit_status(session.exit_code.map(i64::from)))
}

#[cfg(test)]
mod tests {
    #[test]
    fn exit_codes() {
        assert_eq!(super::exit_status(Some(3)), 3);
        assert_eq!(super::exit_status(Some(256)), 0);
        assert_eq!(super::exit_status(None), 1);
    }
}
