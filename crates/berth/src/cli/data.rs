//! What flows between hosts and the server: a port of a host reached from
//! here, the inbox, the key-value store, a host's channel and its services.

use super::args::{Channel, Inbox, Kv, Service};
use super::client::Client;
use super::fail::{Fail, Result};
use super::out::{self, Table};
use berth_api::portal::Piece;
use serde_json::{Value, json};
use std::io::Write;

/// A port number of a word, which is more than 0.
fn port_of(text: &str, word: &str) -> Result<u16> {
    text.parse::<u16>()
        .ok()
        .filter(|port| *port > 0)
        .ok_or_else(|| Fail::usage(format!("{text} is no port, in {word}")))
}

/// One `[LOCAL:]PORT`: the port to listen on here, and the host's.
fn pair(word: &str) -> Result<(u16, String)> {
    match word.split_once(':') {
        Some((local, remote)) => Ok((port_of(local, word)?, port_of(remote, word)?.to_string())),
        None => {
            let remote = port_of(word, word)?;
            Ok((remote, remote.to_string()))
        }
    }
}

/// One `[LOCAL:]ENDPOINT` of a resource: the port to listen on here, and
/// the endpoint by its name or port. Without LOCAL it is the endpoint's port.
fn endpoint_pair(word: &str, endpoints: &[berth_api::Endpoint]) -> Result<(u16, String)> {
    let (local, wanted) = match word.split_once(':') {
        Some((local, wanted)) => (Some(port_of(local, word)?), wanted),
        None => (None, word),
    };
    let found = endpoints
        .iter()
        .find(|endpoint| endpoint.name == wanted || endpoint.port.to_string() == wanted)
        .ok_or_else(|| {
            let names: Vec<String> = endpoints
                .iter()
                .map(|endpoint| format!("{} ({})", endpoint.name, endpoint.port))
                .collect();
            Fail::usage(match names.is_empty() {
                true => format!("The resource reports no endpoints, so there is no {wanted}"),
                false => format!(
                    "The resource has no endpoint {wanted}. It has {}",
                    names.join(", ")
                ),
            })
        })?;
    Ok((local.unwrap_or(found.port), found.name.clone()))
}

/// What a forwarded connection goes to.
#[derive(Clone)]
enum Target {
    Host(String),
    Resource(String),
}

/// `berth port forward TARGET [LOCAL:]PORT…`: listen here, and pass each
/// connection through a tunnel of its own to the host's port or the
/// resource's endpoint, until Ctrl+C.
pub fn forward(
    client: &Client,
    target: &str,
    ports: &[String],
    resource: bool,
    address: &str,
) -> Result<()> {
    // A host's name comes first; what no host is called may be a resource.
    let host = match resource {
        true => false,
        false => match client.call("hosts.get", json!({"name": target}), None) {
            Ok(_) => true,
            Err(fail) if fail.code == "not_found" => false,
            Err(fail) => return Err(fail),
        },
    };
    let (target, shown, pairs) = match host {
        true => {
            let pairs: Vec<(u16, String)> =
                ports.iter().map(|word| pair(word)).collect::<Result<_>>()?;
            (Target::Host(target.to_owned()), target.to_owned(), pairs)
        }
        false => {
            let id = super::things::resource_id(client, target).map_err(|fail| match resource {
                true => fail,
                false => Fail::failed(format!(
                    "There is no host or resource {target}. berth ls and berth resource list name them"
                )),
            })?;
            let found = client.call("resources.get", json!({"id": id}), None)?;
            let made: berth_api::Resource = serde_json::from_value(found)?;
            let pairs: Vec<(u16, String)> = ports
                .iter()
                .map(|word| endpoint_pair(word, &made.endpoints))
                .collect::<Result<_>>()?;
            let shown = match made.name.is_empty() {
                true => id.clone(),
                false => made.name.clone(),
            };
            (Target::Resource(id), shown, pairs)
        }
    };
    let portal = client.portal().clone();
    client.wait(async {
        let mut listening = vec![];
        for (local, remote) in &pairs {
            let listener = tokio::net::TcpListener::bind((address, *local))
                .await
                .map_err(|error| Fail::failed(format!("{address}:{local}: {error}")))?;
            let bound = listener.local_addr()?;
            out::line(format!("{bound} → {shown}:{remote}"));
            if out::get().json {
                let to = match &target {
                    Target::Host(host) => json!({"host": host, "port": remote}),
                    Target::Resource(id) => json!({"resource": id, "endpoint": remote}),
                };
                let mut said = json!({"listen": bound.to_string()});
                for (key, value) in to.as_object().into_iter().flatten() {
                    said[key] = value.clone();
                }
                out::json(&said);
            }
            listening.push((listener, remote.clone()));
        }
        out::hint("Ctrl+C ends it");
        let mut tasks = vec![];
        for (listener, remote) in listening {
            let (portal, target, shown) = (portal.clone(), target.clone(), shown.clone());
            tasks.push(tokio::spawn(async move {
                loop {
                    let Ok((mut socket, from)) = listener.accept().await else {
                        return;
                    };
                    let (portal, target, shown, remote) = (
                        portal.clone(),
                        target.clone(),
                        shown.clone(),
                        remote.clone(),
                    );
                    tokio::spawn(async move {
                        let opened = match &target {
                            Target::Host(host) => {
                                let port = remote.parse().unwrap_or(0);
                                portal.connect_port(Some(host), port).await
                            }
                            Target::Resource(id) => portal.connect_endpoint(id, &remote).await,
                        };
                        match opened {
                            Ok(mut tunnel) => {
                                let _ =
                                    tokio::io::copy_bidirectional(&mut socket, &mut tunnel).await;
                            }
                            Err(error) => out::warn(format!(
                                "{from}: {shown}:{remote} did not open: {}",
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
    let (host, topic, all, before) = match command {
        Inbox::List {
            host,
            topic,
            all,
            before,
        } => (host, topic, all, before),
        Inbox::Done { host, id } => {
            client.call("inbox.done", json!({"host": host, "id": id}), None)?;
            out::done(format!("Message {id} of {host} is done"));
            return Ok(());
        }
        Inbox::Reopen { host, id } => {
            client.call("inbox.reopen", json!({"host": host, "id": id}), None)?;
            out::done(format!("Message {id} of {host} is open again"));
            return Ok(());
        }
    };
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
    if let Some(before) = before {
        args["before"] = json!(before);
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
        if let Some(next) = found["next"].as_i64() {
            out::hint(format!(
                "Older messages are not shown. The next ones: berth inbox list --before {next}"
            ));
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
    use super::{endpoint_pair, pair};

    #[test]
    fn ports_are_the_hosts_and_maybe_a_local_one() {
        assert_eq!(pair("8080").unwrap(), (8080, "8080".into()));
        assert_eq!(pair("9000:80").unwrap(), (9000, "80".into()));
        assert!(pair("0").is_err() && pair("a:80").is_err() && pair("70000").is_err());
    }

    #[test]
    fn an_endpoint_is_named_or_given_by_its_port() {
        let endpoints = vec![berth_api::Endpoint {
            name: "db".into(),
            address: "192.0.2.30".into(),
            port: 5432,
            protocol: "tcp".into(),
            hint: "postgres".into(),
            local: false,
        }];
        assert_eq!(
            endpoint_pair("db", &endpoints).unwrap(),
            (5432, "db".into())
        );
        assert_eq!(
            endpoint_pair("5432", &endpoints).unwrap(),
            (5432, "db".into())
        );
        assert_eq!(
            endpoint_pair("15432:db", &endpoints).unwrap(),
            (15432, "db".into())
        );
        let missing = endpoint_pair("web", &endpoints).unwrap_err().message;
        assert!(missing.contains("It has db (5432)"), "{missing}");
    }
}
