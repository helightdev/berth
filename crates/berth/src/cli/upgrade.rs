//! `upgrade-server source=here`: a node builds the berth server's image from
//! this berth, which sends itself with the request, and runs the server from
//! it. The swap is the last thing the node does, and the server it replaces
//! may be the one this command talks to; so the node's session is followed
//! rather than the server's job, and read again once the server answers anew.

use super::client::Client;
use super::fail::{Fail, Result};
use super::out;
use super::things::{Ended, attach_stream, follow_process};
use berth_api::upgrade_server;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// How long a replaced server has to answer again.
const BACK_WITHIN: Duration = Duration::from_secs(300);

/// Whether these are the arguments of `upgrade-server` that build from here.
pub fn wanted(action: &str, given: &BTreeMap<String, String>) -> bool {
    action == upgrade_server::ACTION
        && given
            .get(upgrade_server::SOURCE)
            .is_some_and(|source| source.trim() == upgrade_server::HERE)
}

/// The CPU of the host, as `uname -m` names it there.
fn cpu_of(client: &Client, host: &str) -> Result<String> {
    let body = json!({
        "kind": "process", "command": ["uname", "-m"], "user": "root",
        "title": "the CPU, for the server's image", "timeout": 20,
    });
    let started = client.on(host, "sessions.start", json!({"wait": "20s"}), Some(body))?;
    if let Some(id) = started["id"].as_str() {
        let _ = client.on(host, "sessions.delete", json!({"id": id}), None);
    }
    let said = started["stdout_b64"]
        .as_str()
        .and_then(berth_api::unb64)
        .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_owned())
        .unwrap_or_default();
    match said.is_empty() {
        true => Err(Fail::failed(format!(
            "{host} did not say what CPU it has (uname -m)"
        ))),
        false => Ok(said),
    }
}

/// Check this berth against the host and put its build context in `act`,
/// with the version it is.
pub fn send_here(
    client: &Client,
    host: &str,
    given: &mut BTreeMap<String, String>,
) -> Result<String> {
    let version = berth_api::VERSION;
    match given.get("version").map(|asked| asked.trim()) {
        Some(asked) if !asked.is_empty() && asked != version => {
            return Err(Fail::usage(format!(
                "source=here sends this berth, which is {version}, not {asked}. Leave out version, or run the berth of {asked}"
            )));
        }
        _ => {}
    }
    given.insert("version".into(), version.into());
    let cpu = cpu_of(client, host)?;
    let (context, sha256) = crate::local::server_context(&cpu)?;
    out::note(format!(
        "Sending berth {version} ({}, {:.1} MB) to {host}, which builds the image berth:{version} from it",
        &sha256[..12],
        context.len() as f64 / 1e6
    ));
    Ok(berth_api::b64(&context))
}

/// The lines of the end of the host's session.
fn tail(client: &Client, host: &str, id: &str) -> Vec<String> {
    let said = client
        .on(host, "sessions.output", json!({"id": id, "tail": 15}), None)
        .unwrap_or(Value::Null);
    said["lines"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|line| line.as_str().map(str::to_owned))
        .collect()
}

/// Follow the node's session to its end. When the connection goes with the
/// server, wait for the server to answer again and read how it ended.
pub fn follow(client: &Client, host: &str, id: &str) -> Result<()> {
    let stream = attach_stream(client, host, id, 0)?;
    match client.wait(follow_process(stream, false))? {
        Ended::Exited(Some(0)) => return Ok(()),
        Ended::Exited(code) => return Err(Fail::exit(code.unwrap_or(1))),
        Ended::Detached => {
            eprintln!(
                "\r\nLeft the upgrade running on {host}. berth session logs {host} {id} -f follows it again"
            );
            return Ok(());
        }
        Ended::Lost => {}
    }
    out::note(format!(
        "The connection to the server ended, as it does when the upgrade replaces it. Waiting for it to answer again (up to {} minutes)",
        BACK_WITHIN.as_secs() / 60
    ));
    let since = Instant::now();
    loop {
        std::thread::sleep(Duration::from_secs(3));
        let session = client.reopen().and_then(|again| {
            Ok((
                again.on(host, "sessions.get", json!({"id": id}), None)?,
                again,
            ))
        });
        match session {
            Ok((session, again)) if !session["ended"].is_null() => {
                for line in tail(&again, host, id) {
                    println!("{line}");
                }
                let version = again
                    .server_version()
                    .map(|(version, _)| version)
                    .unwrap_or_default();
                return match session["exit_code"].as_i64() {
                    Some(0) => {
                        out::done(format!("The server answers again, as berth {version}"));
                        Ok(())
                    }
                    code => Err(Fail::exit(code.unwrap_or(1) as i32)),
                };
            }
            // Back, and the node is not done yet; or not back yet.
            _ if since.elapsed() < BACK_WITHIN => {}
            Ok(_) => {
                return Err(Fail::failed(format!(
                    "The upgrade on {host} has not ended after {} minutes. berth session logs {host} {id} -f follows it",
                    BACK_WITHIN.as_secs() / 60
                )));
            }
            Err(error) => {
                return Err(Fail::failed(format!(
                    "The server did not answer again within {} minutes: {}. On {host}, berth session logs {host} {id} shows how the upgrade ended",
                    BACK_WITHIN.as_secs() / 60,
                    error.message
                )));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_upgrade_server_with_source_here_sends_this_berth() {
        let given = |words: &[(&str, &str)]| -> BTreeMap<String, String> {
            words
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect()
        };
        assert!(wanted("upgrade-server", &given(&[("source", "here")])));
        assert!(!wanted("upgrade-server", &given(&[("source", "release")])));
        assert!(!wanted("upgrade-server", &given(&[("version", "0.6.0")])));
        assert!(!wanted("update", &given(&[("source", "here")])));
    }
}
