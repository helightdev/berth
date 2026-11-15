//! `berth job …` and `berth rollout`: the work the server does, followed
//! again or started over all hosts.

use super::args::{self, Job};
use super::client::{Client, Subject};
use super::fail::{Fail, Result};
use super::follow;
use super::out::{self, Style, Table, styled};
use berth_api::Act;
use berth_api::portal::Piece;
use serde_json::{Value, json};
use std::io::Write;

fn state_of(job: &Value) -> String {
    if job["ended"].is_null() {
        return styled("running", Style::Accent);
    }
    match job["exit_code"].as_i64() {
        Some(0) => styled("done", Style::Good),
        Some(code) => styled(&format!("failed ({code})"), Style::Bad),
        None => styled("failed", Style::Bad),
    }
}

pub fn command(client: &Client, command: Job) -> Result<()> {
    match command {
        Job::List { limit } => {
            let found = client.call("jobs.list", json!({"limit": limit}), None)?;
            out::data(&found, || {
                let mut table = Table::new(&["id", "action", "subject", "state", "started"]);
                for job in found.as_array().into_iter().flatten() {
                    let text = |key: &str| job[key].as_str().unwrap_or("").to_owned();
                    let subject = match text("host").as_str() {
                        "" => text("subject"),
                        host => host.to_owned(),
                    };
                    table.row(vec![
                        text("id"),
                        text("action"),
                        subject,
                        state_of(job),
                        text("started"),
                    ]);
                }
                table.print("No jobs yet");
            });
            Ok(())
        }
        Job::Show { id } => {
            let job = client.call("jobs.get", json!({"id": id}), None)?;
            out::data(&job, || {
                let text = |key: &str| job[key].as_str().unwrap_or("").to_owned();
                out::fields(&[
                    ("Id", text("id")),
                    ("Action", text("action")),
                    ("Subject", text("subject")),
                    ("Host", text("host")),
                    (
                        match text("action").as_str() {
                            "exec" => "Command",
                            _ => "Arguments",
                        },
                        berth_tui::host::job_arguments(&job).unwrap_or_default(),
                    ),
                    ("State", state_of(&job)),
                    ("Started", text("started")),
                    ("Ended", text("ended")),
                ]);
                if let Some(report) = job["report"].as_object() {
                    let report: berth_api::RolloutReport =
                        serde_json::from_value(Value::Object(report.clone())).unwrap_or_default();
                    for line in berth_tui::plugins::summary(&report) {
                        println!("{line}");
                    }
                }
            });
            Ok(())
        }
        Job::Logs { id, follow } => logs(client, &id, follow),
        Job::Stop { id } => {
            let job = client.call("jobs.stop", json!({"id": id}), None)?;
            out::data(&job, || {
                out::done(format!(
                    "Asked {id} to stop. berth job wait {id} waits for its end"
                ))
            });
            Ok(())
        }
        Job::Wait { id } => {
            let job = client.call("jobs.get", json!({"id": id}), None)?;
            let title = format!(
                "{} {}",
                job["action"].as_str().unwrap_or("job"),
                job["host"].as_str().unwrap_or("")
            );
            let ended = follow::job(client, &id, title.trim(), false)?;
            if out::get().json {
                out::json(&ended.job);
            }
            match ended.code {
                0 => Ok(()),
                _ => Err(Fail::exit(1)),
            }
        }
    }
}

/// What a job wrote, with its colours where they are shown, and with
/// `follow` what it writes until it ends.
fn logs(client: &Client, id: &str, follow: bool) -> Result<()> {
    let colour = out::get().colour.0;
    let write = |bytes: &[u8]| -> std::io::Result<()> {
        let mut stdout = std::io::stdout().lock();
        match colour {
            true => stdout.write_all(bytes)?,
            false => {
                stdout.write_all(out::strip_ansi(&String::from_utf8_lossy(bytes)).as_bytes())?
            }
        }
        stdout.flush()
    };
    if !follow {
        let found = client.call("jobs.output", json!({"id": id}), None)?;
        let bytes = match &found["data_b64"] {
            Value::String(data) => berth_api::unb64(data).unwrap_or_default(),
            _ => match &found["lines"] {
                Value::Array(lines) => lines
                    .iter()
                    .map(|line| format!("{}\n", line.as_str().unwrap_or("")))
                    .collect::<String>()
                    .into_bytes(),
                _ => found["base64"]
                    .as_str()
                    .and_then(berth_api::unb64)
                    .unwrap_or_default(),
            },
        };
        write(&bytes)?;
        return Ok(());
    }
    let mut stream = client.stream("jobs.follow", None, json!({"id": id}))?;
    let code = client.wait(async {
        let mut code = None;
        loop {
            tokio::select! {
                piece = stream.recv() => match piece {
                    Some(Ok(Piece::Binary(bytes))) => write(&bytes)?,
                    Some(Ok(Piece::Text(said))) => {
                        if let Some(exited) = said.get("exited") {
                            code = Some(exited["exit_code"].as_i64().unwrap_or(1) as i32);
                        }
                    }
                    Some(Err(error)) => return Err(Fail::from(error)),
                    None => return Ok(code),
                },
                _ = tokio::signal::ctrl_c() => return Ok(Some(0)),
            }
        }
    })?;
    match code.unwrap_or(0) {
        0 => Ok(()),
        _ => Err(Fail::exit(1)),
    }
}

/// `berth rollout`: the installation's roll-out, as a job of the server.
pub fn rollout(client: &Client, rollout: &args::Rollout) -> Result<()> {
    let options = berth_api::Rollout {
        hosts: rollout.only.clone(),
        except: rollout.except.clone(),
        all: rollout.all,
        update: rollout.update,
        reboot: rollout.reboot,
        force: rollout.force,
    };
    let act = Act {
        arguments: options.arguments(),
        ..Act::default()
    };
    let acted = client.act(&Subject::Installation, "rollout", &act)?;
    let (Some(job), false) = (&acted.job, rollout.detach.detach) else {
        follow::acted(client, acted, "Rolling out", rollout.detach.detach, false)?;
        return Ok(());
    };
    // The line of each host is said whether the roll-out ended well or not.
    let ended = follow::job(client, &job.id, "Rolling out", false)?;
    if out::get().json {
        out::json(&ended.job);
    } else if ended.job["report"].is_object() {
        let report: berth_api::RolloutReport =
            serde_json::from_value(ended.job["report"].clone()).unwrap_or_default();
        for line in berth_tui::plugins::summary(&report) {
            println!("{line}");
        }
    }
    match ended.code {
        0 => Ok(()),
        _ => Err(Fail::exit(1)),
    }
}
