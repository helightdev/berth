//! A TCP connection to a port on the host's loopback, as a stream.

use crate::error::{E, R};
use axum::{
    extract::{
        Path,
        ws::{Message, WebSocket, WebSocketUpgrade, rejection::WebSocketUpgradeRejection},
    },
    response::Response,
};
use berth_api::{code, streams::EOF};
use futures_util::{SinkExt, StreamExt};
use std::{net::Ipv4Addr, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
};

/// How much is read from the port at a time, which is the most one frame carries.
const FRAME: usize = 64 * 1024;

/// The port is dialled before the stream opens, so one that nothing listens
/// on is an answer of its own and not a stream that ends at once.
pub async fn connect(
    Path(port): Path<String>,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> R<Response> {
    let upgrade = upgrade.map_err(|_| E::bad("A port is opened only as a WebSocket"))?;
    let port = port
        .parse::<u16>()
        .ok()
        .filter(|port| *port > 0)
        .ok_or_else(|| E::bad(format!("{port} is no port: a number from 1 to 65535")))?;
    let dialled = tokio::time::timeout(
        Duration::from_secs(10),
        TcpStream::connect((Ipv4Addr::LOCALHOST, port)),
    )
    .await;
    let tcp = match dialled {
        Ok(Ok(tcp)) => tcp,
        Ok(Err(error)) => {
            return Err(E(
                code::NOT_SERVED,
                format!("Nothing answers on port {port} of the host: {error}"),
            ));
        }
        Err(_) => {
            return Err(E(
                code::TIMEOUT,
                format!("Port {port} of the host does not answer"),
            ));
        }
    };
    let _ = tcp.set_nodelay(true);
    Ok(upgrade.on_upgrade(move |socket| tunnel(socket, tcp)))
}

fn said_end(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text).is_ok_and(|frame| frame["eof"] == true)
}

/// Bytes both ways between a stream and a connection, each direction on its
/// own. Each side's end is a frame, so the other direction goes on until it
/// ends too; the caller leaving, or the connection failing, ends both at once.
pub async fn tunnel<C>(socket: WebSocket, connection: C)
where
    C: AsyncRead + AsyncWrite + Send + 'static,
{
    let (mut sink, mut source) = socket.split();
    let (reader, writer) = tokio::io::split(connection);
    let (said, heard) = tokio::sync::oneshot::channel::<()>();
    let upstream = async move {
        let (mut writer, mut said) = (Some(writer), Some(said));
        while let Some(Ok(message)) = source.next().await {
            match message {
                Message::Binary(bytes) => {
                    if let Some(open) = writer.as_mut()
                        && open.write_all(&bytes).await.is_err()
                    {
                        writer = None;
                    }
                }
                Message::Text(text) if said_end(text.as_str()) => {
                    if let Some(mut open) = writer.take() {
                        let _ = open.shutdown().await;
                    }
                    if let Some(said) = said.take() {
                        let _ = said.send(());
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    };
    let downstream = async move {
        let mut reader = reader;
        let mut buffer = vec![0; FRAME];
        let ended = loop {
            match reader.read(&mut buffer).await {
                Ok(0) => break true,
                Ok(count) => {
                    let bytes = buffer[..count].to_vec();
                    if sink.send(Message::Binary(bytes.into())).await.is_err() {
                        return;
                    }
                }
                Err(_) => break false,
            }
        };
        if ended {
            if sink.send(Message::Text(EOF.into())).await.is_err() {
                return;
            }
            // Until the caller has said its end too.
            let _ = heard.await;
        }
        let _ = sink.send(Message::Close(None)).await;
    };
    tokio::select! {
        () = upstream => {}
        () = downstream => {}
    }
}
