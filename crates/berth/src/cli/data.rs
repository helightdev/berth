//! What flows between hosts and the server: a port of a host reached from
//! here, the inbox, the key-value store, a host's channel and its services.

use super::args::{Channel, Inbox, Kv, Service};
use super::client::Client;
use super::fail::{Fail, Result};
use super::out::{self, Table};
use berth_api::portal::Piece;
use serde_json::{Value, json};
use std::io::Write;

/// One `[LOCAL:]PORT`: the port to listen on here, and the host's.
fn pair(word: &str) -> Result<(u16, u16)> {
    let port = |text: &str| {
        text.parse::<u16>()
            .ok()
            .filter(|port| *port > 0)
            .ok_or_else(|| Fail::usage(format!("{text} is no port, in {word}")))
    };
    match word.split_once(':') {
        Some((local, remote)) => Ok((port(local)?, port(remote)?)),
        None => {
            let remote = port(word)?;
            Ok((remote, remote))
        }
    }
}

/// `berth port forward HOST [LOCAL:]PORT…`: listen here, and pass each
/// connection through a tunnel of its own to the host's port, until Ctrl+C.
pub fn forward(client: &Client, host: &str, ports: &[String], address: &str) -> Result<()> {
    let pairs: Vec<(u16, u16)> = ports.iter().map(|word| pair(word)).collect::<Result<_>>()?;
    // The host is looked up first, so that a wrong name fails before anything listens.
    client.call("hosts.get", json!({"name": host}), None)?;
    let portal = client.portal().clone();
    client.wait(async {
        let mut listening = vec![];
        for (local, remote) in &pairs {
            let listener = tokio::net::TcpListener::bind((address, *local))
                .await
                .map_err(|error| Fail::failed(format!("{address}:{local}: {error}")))?;
            let bound = listener.local_addr()?;
            out::line(format!("{bound} → {host}:{remote}"));
            if out::get().json {
                out::json(&json!({"listen": bound.to_string(), "host": host, "port": remote}));
            }
            listening.push((listener, *remote));
        }
        out::hint("Ctrl+C ends it");
        let mut tasks = vec![];
        for (listener, remote) in listening {
            let (portal, host) = (portal.clone(), host.to_owned());
            tasks.push(tokio::spawn(async move {
                loop {
                    let Ok((mut socket, from)) = listener.accept().await else {
                        return;
                    };
                    let (portal, host) = (portal.clone(), host.clone());
                    tokio::spawn(async move {
                        match portal.connect_port(Some(&host), remote).await {
                            Ok(mut tunnel) => {
                                let _ =
                                    tokio::io::copy_bidirectional(&mut socket, &mut tunnel).await;
                            }
                            Err(error) => out::warn(format!(
                                "{from}: {host}:{remote} did not open: {}",
                                berth_page::net::describe(&error)
                            )),
                        }
                    });
                }
            }));
        }
        let _ = tokio::signal::ctrl_c().await;
        for task in tasks {
            task.abort();
        }
        Ok(())
    })
}

pub fn inbox(client: &Client, command: Inbox) -> Result<()> {
    let Inbox::List { host, topic, all } = command;
    let mut args = json!({"limit": 200});
    if let Some(host) = host {
        args["host"] = json!(host);
    }
    if let Some(topic) = topic {
        args["topic"] = json!(topic);
    }
    if !all {
        args["done"] = json!(false);
    }
    let found = client.call("inbox.list", args, None)?;
    out::data(&found, || {
        let mut table = Table::new(&["", "time", "host", "id", "topic", "body"]);
        let messages = found["messages"].as_array().cloned().unwrap_or_default();
        for message in messages.iter().rev() {
            let text = |key: &str| message[key].as_str().unwrap_or("").to_owned();
            table.row(vec![
                if message["done"] == true {
                    "✓".into()
                } else {
                    String::new()
                },
                text("time"),
                text("host"),
                message["id"].to_string(),
                text("topic"),
                message["body"].to_string(),
            ]);
        }
        table.print("No messages");
        if !found["next"].is_null() {
            out::hint("Older messages are not shown");
        }
    });
    Ok(())
}

pub fn kv(client: &Client, command: Kv) -> Result<()> {
    match command {
        Kv::List { namespace } => {
            let found = client.call("kv.list", json!({"namespace": namespace}), None)?;
            out::data(&found, || {
                let mut table = Table::new(&["key", "revision", "value"]);
                let entries = match &found {
                    Value::Array(entries) => entries.clone(),
                    found => found["entries"].as_array().cloned().unwrap_or_default(),
                };
                for entry in entries {
                    table.row(vec![
                        entry["key"].as_str().unwrap_or("").to_owned(),
                        entry["revision"].to_string(),
                        entry["value"].to_string(),
                    ]);
                }
                table.print(&format!("Nothing in {namespace}"));
            });
            Ok(())
        }
        Kv::Get { namespace, key } => {
            let found = client.call("kv.get", json!({"namespace": namespace, "key": key}), None)?;
            out::data(&found, || println!("{}", found["value"]));
            Ok(())
        }
        Kv::Set {
            namespace,
            key,
            value,
        } => {
            // Anything that is not JSON is taken as text.
            let value: Value = serde_json::from_str(&value).unwrap_or_else(|_| json!(value));
            let found = client.call(
                "kv.set",
                json!({"namespace": namespace, "key": key}),
                Some(json!({"value": value})),
            )?;
            out::data(&found, || {
                out::done(format!("{key} is at revision {}", found["revision"]));
            });
            Ok(())
        }
        Kv::Remove { namespace, key } => {
            client.call(
                "kv.delete",
                json!({"namespace": namespace, "key": key}),
                None,
            )?;
            out::done(format!("Removed {key} from {namespace}"));
            Ok(())
        }
    }
}

pub fn channel(client: &Client, command: Channel) -> Result<()> {
    let Channel::Listen { host, topic, since } = command;
    let mut args = json!({});
    if let Some(since) = since {
        args["since"] = json!(since);
    }
    let mut stream = client.stream("channel.listen", Some(&host), args)?;
    client.wait(async {
        let mut stdout = std::io::stdout();
        loop {
            tokio::select! {
                piece = stream.recv() => match piece {
                    Some(Ok(Piece::Text(message))) => {
                        let fits = topic.as_ref().is_none_or(|prefix| {
                            message["topic"].as_str().is_some_and(|topic| topic.starts_with(prefix.as_str()))
                        });
                        if fits {
                            writeln!(stdout, "{message}")?;
                            stdout.flush()?;
                        }
                    }
                    Some(Ok(Piece::Binary(_))) => {}
                    Some(Err(error)) => return Err(Fail::from(error)),
                    None => return Ok(()),
                },
                _ = tokio::signal::ctrl_c() => return Ok(()),
            }
        }
    })
}

pub fn service(client: &Client, command: Service) -> Result<()> {
    let Service::Call {
        host,
        service,
        method,
        body,
    } = command;
    let (plugin, name) = service
        .split_once('/')
        .ok_or_else(|| Fail::usage(format!("A service is PLUGIN/NAME, not {service}")))?;
    let body: Value = match body {
        Some(body) => serde_json::from_str(&body)
            .map_err(|error| Fail::usage(format!("The body is no JSON: {error}")))?,
        None => json!({}),
    };
    let answer = client.on(
        &host,
        "services.call",
        json!({"plugin": plugin, "name": name, "method": method}),
        Some(body),
    )?;
    out::json(&answer);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::pair;

    #[test]
    fn ports_are_the_hosts_and_maybe_a_local_one() {
        assert_eq!(pair("8080").unwrap(), (8080, 8080));
        assert_eq!(pair("9000:80").unwrap(), (9000, 80));
        assert!(pair("0").is_err() && pair("a:80").is_err() && pair("70000").is_err());
    }
}
