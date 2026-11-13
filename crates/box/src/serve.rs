//! `box serve SERVICE -- COMMAND…`

use crate::net::socket;
use berth_api::client::{self, Frame, encode_path};
use berth_api::{CallFrame, Level};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

pub async fn serve(mut args: Vec<String>) -> Result<i32, String> {
    let service = args.remove(0);
    if args.first().map(String::as_str) != Some("--") || args.len() < 2 {
        return Err("usage: box serve SERVICE -- COMMAND…".into());
    }
    let command: Vec<String> = args.split_off(1);
    let target = format!("/v1/services/{}/serve", encode_path(&service));
    let mut delay = 1;
    loop {
        let opened = match socket() {
            Ok(path) => client::websocket_unix(&path, &target)
                .await
                .map_err(|x| (x.status(), x.to_string())),
            Err(message) => Err((0, message)),
        };
        match opened {
            Ok(ws) => {
                eprintln!("box serve: serving {service}");
                delay = 1;
                run_calls(ws, &command).await;
                eprintln!("box serve: disconnected from berthd");
            }
            Err((403 | 404, message)) => return Err(message),
            Err((_, message)) => eprintln!("box serve: {message}"),
        }
        tokio::time::sleep(Duration::from_secs(delay)).await;
        delay = (delay * 2).min(30);
    }
}

async fn run_calls(ws: client::WebSocketStream<tokio::net::UnixStream>, command: &[String]) {
    let (mut tx, mut rx) = ws.split();
    let (replies, mut queue) = mpsc::unbounded_channel::<Value>();
    loop {
        tokio::select! {
            frame = rx.next() => match frame {
                Some(Ok(Frame::Text(text))) => match serde_json::from_str::<CallFrame>(text.as_str()) {
                    Ok(call) => {
                        let (command, replies) = (command.to_vec(), replies.clone());
                        tokio::spawn(async move { let _ = replies.send(answer(&command, call).await); });
                    }
                    Err(error) => eprintln!("box serve: bad call frame: {error}"),
                },
                Some(Ok(Frame::Close(_))) | Some(Err(_)) | None => return,
                Some(Ok(_)) => {}
            },
            Some(reply) = queue.recv() => {
                if tx.send(Frame::Text(reply.to_string().into())).await.is_err() {
                    return;
                }
            }
        }
    }
}

async fn answer(command: &[String], call: CallFrame) -> Value {
    match handler(command, &call).await {
        Ok(result) => json!({"id": call.id, "result": result}),
        Err(message) => {
            json!({"id": call.id, "error": {"code": "service_error", "message": message}})
        }
    }
}

async fn handler(command: &[String], call: &CallFrame) -> Result<Value, String> {
    let level = if call.from.level == Level::Host {
        "host"
    } else {
        "agent"
    };
    let mut child = tokio::process::Command::new(&command[0])
        .args(&command[1..])
        .arg(&call.method)
        .env("BERTH_CALLER_LEVEL", level)
        .env("BERTH_CALLER_UID", call.from.uid.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("{}: {error}", command[0]))?;
    let mut stdin = child.stdin.take().unwrap();
    let body = call.body.to_string();
    // The handler may not read its input; a broken pipe is its business.
    tokio::spawn(async move {
        let _ = stdin.write_all(body.as_bytes()).await;
    });
    let output = child
        .wait_with_output()
        .await
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        let text = String::from_utf8_lossy(&output.stdout);
        Ok(if text.trim().is_empty() {
            Value::Null
        } else {
            crate::json_or_string(&text)
        })
    } else {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if message.is_empty() {
            format!("exit status {}", output.status.code().unwrap_or(-1))
        } else {
            message
        })
    }
}
