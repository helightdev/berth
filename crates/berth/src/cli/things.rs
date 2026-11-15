//! What a host does and runs, and what provisioners make: sessions, exec,
//! actions, provisioners and resources, each through the server's portal,
//! with a host's own operations sent to it by name.

use super::args::{self, Provisioner, Resource, Session};
use super::ask;
use super::client::{Client, Subject};
use super::fail::{Fail, Result};
use super::follow;
use super::host;
use super::out::{self, Style, Table, styled};
use super::pages;
use berth_api::portal::{Piece, PortalStream};
use berth_api::{Act, SessionKind};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use tokio::io::AsyncReadExt;

/// Ctrl+Q, which leaves a terminal session without ending it.
const DETACH: u8 = 0x11;

/// The items of a list the API answers with, bare or under `key`.
fn items(value: Value, key: &str) -> Vec<Value> {
    match value {
        Value::Array(items) => items,
        mut value => value[key].take().as_array().cloned().unwrap_or_default(),
    }
}

/// `NAME=VALUE` words as a map.
pub fn arguments(words: &[String]) -> Result<BTreeMap<String, String>> {
    words
        .iter()
        .map(|word| match word.split_once('=') {
            Some((name, value)) if !name.is_empty() && !name.starts_with('-') => {
                Ok((name.to_owned(), value.to_owned()))
            }
            _ => Err(Fail::usage(format!(
                "Arguments are given as NAME=VALUE, not {word}"
            ))),
        })
        .collect()
}

/// How following a session ended.
enum Ended {
    Exited(Option<i32>),
    /// Left with Ctrl+Q, or Ctrl+C when nothing is typed into it; it runs on.
    Detached,
    Lost,
}

fn exited(said: &Value) -> Option<Option<i32>> {
    berth_tui::host::exited(said)
}

/// Follow a process session: its two streams to ours, apart. With `stdin`,
/// ours is its input and Ctrl+C interrupts it; without, Ctrl+C only leaves.
async fn follow_process(mut stream: PortalStream, stdin: bool) -> Result<Ended> {
    let (sender, mut typed) = tokio::sync::mpsc::unbounded_channel::<Piece>();
    if stdin && !std::io::stdin().is_terminal() {
        tokio::spawn(async move {
            let mut input = tokio::io::stdin();
            let mut buffer = vec![0; 16384];
            while let Ok(count) = input.read(&mut buffer).await
                && count > 0
            {
                let frame = [&[0u8][..], &buffer[..count]].concat();
                if sender.send(Piece::Binary(frame)).is_err() {
                    return;
                }
            }
            let _ = sender.send(Piece::Text(json!({"eof": true})));
        });
    } else if stdin {
        // Nothing will be typed into it.
        let _ = sender.send(Piece::Text(json!({"eof": true})));
    }
    let (mut out, mut err) = (std::io::stdout(), std::io::stderr());
    loop {
        tokio::select! {
            piece = stream.recv() => match piece {
                Some(Ok(Piece::Binary(bytes))) => match bytes.split_first() {
                    Some((2, rest)) => {
                        err.write_all(rest)?;
                        err.flush()?;
                    }
                    Some((_, rest)) => {
                        out.write_all(rest)?;
                        out.flush()?;
                    }
                    None => {}
                },
                Some(Ok(Piece::Text(said))) => {
                    if let Some(code) = exited(&said) {
                        return Ok(Ended::Exited(code));
                    }
                }
                _ => return Ok(Ended::Lost),
            },
            Some(piece) = typed.recv() => {
                if stream.push(piece).is_err() {
                    return Ok(Ended::Lost);
                }
            }
            _ = tokio::signal::ctrl_c() => {
                if !stdin {
                    return Ok(Ended::Detached);
                }
                let _ = stream.push(Piece::Text(json!({"signal": "INT"})));
            }
        }
    }
}

/// Puts the terminal back as it was when dropped.
struct Raw(bool);

impl Raw {
    fn enter() -> Raw {
        let terminal = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
        Raw(terminal && crossterm::terminal::enable_raw_mode().is_ok())
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        if self.0 {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}

fn terminal_size() -> (u16, u16) {
    // A terminal that does not know its size says 0 by 0.
    match crossterm::terminal::size() {
        Ok((cols, rows)) if cols > 0 && rows > 0 => (rows, cols),
        _ => (24, 80),
    }
}

/// This terminal onto a terminal session. Ctrl+Q leaves it running.
async fn follow_terminal(mut stream: PortalStream) -> Result<Ended> {
    let raw = Raw::enter();
    let resize =
        |(rows, cols): (u16, u16)| Piece::Text(json!({"resize": {"rows": rows, "cols": cols}}));
    let mut size = terminal_size();
    if raw.0 {
        let _ = stream.push(resize(size));
    }
    let (sender, mut typed) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    tokio::spawn(async move {
        let mut input = tokio::io::stdin();
        let mut buffer = vec![0; 4096];
        while let Ok(count) = input.read(&mut buffer).await
            && count > 0
            && sender.send(buffer[..count].to_vec()).is_ok()
        {}
    });
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
    let mut out = std::io::stdout();
    loop {
        tokio::select! {
            piece = stream.recv() => match piece {
                Some(Ok(Piece::Binary(bytes))) => {
                    out.write_all(&bytes)?;
                    out.flush()?;
                }
                Some(Ok(Piece::Text(said))) => {
                    if let Some(code) = exited(&said) {
                        return Ok(Ended::Exited(code));
                    }
                }
                _ => return Ok(Ended::Lost),
            },
            Some(bytes) = typed.recv() => {
                if raw.0 && let Some(at) = bytes.iter().position(|byte| *byte == DETACH) {
                    if at > 0 {
                        let _ = stream.push(Piece::Binary(bytes[..at].to_vec()));
                    }
                    return Ok(Ended::Detached);
                }
                if stream.push(Piece::Binary(bytes)).is_err() {
                    return Ok(Ended::Lost);
                }
            }
            _ = tick.tick() => {
                let now = terminal_size();
                if raw.0 && now != size {
                    size = now;
                    let _ = stream.push(resize(size));
                }
            }
        }
    }
}

/// What to say and which status to leave with once following has ended. A
/// session that was only for this command is forgotten when it has ended.
fn finish(client: &Client, host: &str, id: &str, ended: Ended, forget: bool) -> Result<()> {
    match ended {
        Ended::Exited(code) => {
            if forget {
                let _ = client.on(host, "sessions.delete", json!({"id": id}), None);
            }
            match code.unwrap_or(1) {
                0 => Ok(()),
                code => Err(Fail::exit(code)),
            }
        }
        Ended::Detached => {
            eprintln!(
                "\r\nLeft {id} running on {host}. Back to it: berth session attach {host} {id}, or berth session logs {host} {id} -f"
            );
            Ok(())
        }
        Ended::Lost => Err(Fail::failed(format!(
            "The connection was lost; {id} may still run on {host}: berth session logs {host} {id} -f"
        ))),
    }
}

fn attach_stream(client: &Client, host: &str, id: &str, since: u64) -> Result<PortalStream> {
    client.stream(
        "sessions.attach",
        Some(host),
        json!({"id": id, "since": since}),
    )
}

/// `berth exec HOST -- COMMAND…`: a process session, followed to its end,
/// with its exit status passed through.
pub fn exec(client: &Client, exec: &args::Exec) -> Result<()> {
    if !["agent", "operator", "root"].contains(&exec.user.as_str()) {
        return Err(Fail::usage("--user is agent, operator or root"));
    }
    let title: String = exec.command.join(" ").chars().take(200).collect();
    let body =
        json!({"kind": "process", "command": exec.command, "user": exec.user, "title": title});
    let session: berth_api::Session =
        serde_json::from_value(client.on(&exec.host, "sessions.start", json!({}), Some(body))?)
            .map_err(Fail::from)?;
    if exec.detach {
        out::data(&serde_json::to_value(&session)?, || {
            println!("{}", session.id)
        });
        return Ok(());
    }
    let stream = attach_stream(client, &exec.host, &session.id, 0)?;
    let ended = client.wait(follow_process(stream, true))?;
    finish(client, &exec.host, &session.id, ended, !exec.keep)
}

fn attach(client: &Client, host: &str, id: Option<&str>) -> Result<()> {
    let (rows, cols) = terminal_size();
    let session: berth_api::Session = match id {
        Some(id) => {
            serde_json::from_value(client.on(host, "sessions.get", json!({"id": id}), None)?)?
        }
        None => {
            let body = json!({
                "kind": "terminal", "command": ["bash", "-l"], "user": "agent",
                "title": format!("a shell on {host}"), "rows": rows, "cols": cols,
            });
            serde_json::from_value(client.on(host, "sessions.start", json!({}), Some(body))?)?
        }
    };
    let stream = attach_stream(client, host, &session.id, 0)?;
    let ended = match session.kind {
        SessionKind::Terminal => client.wait(follow_terminal(stream))?,
        SessionKind::Process => client.wait(follow_process(stream, false))?,
    };
    // A shell started here and left with exit is done with.
    finish(client, host, &session.id, ended, id.is_none())
}

fn session_state(session: &Value) -> String {
    if session["ended"].is_null() {
        return styled("running", Style::Good);
    }
    if session["lost"] == true {
        return styled("lost", Style::Warn);
    }
    match session["exit_code"].as_i64() {
        Some(0) => "exit 0".into(),
        Some(code) => styled(&format!("exit {code}"), Style::Bad),
        None => "ended".into(),
    }
}

pub fn session(client: &Client, command: Session) -> Result<()> {
    match command {
        Session::List { host } => {
            let found = client.on(&host, "sessions.list", json!({}), None)?;
            out::data(&found, || {
                let mut table = Table::new(&["id", "kind", "state", "user", "started", "title"]);
                for session in items(found.clone(), "sessions") {
                    let text = |key: &str| session[key].as_str().unwrap_or("").to_owned();
                    table.row(vec![
                        text("id"),
                        text("kind"),
                        session_state(&session),
                        text("user"),
                        text("started"),
                        text("title"),
                    ]);
                }
                table.print(&format!("No sessions on {host}"));
            });
            Ok(())
        }
        Session::Attach { host, id } => attach(client, &host, id.as_deref()),
        Session::Logs {
            host,
            id,
            follow,
            tail,
        } => logs(client, &host, &id, follow, tail),
        Session::Stop { host, id } => {
            client.on(&host, "sessions.delete", json!({"id": id}), None)?;
            out::done(format!("Stopped {id} on {host}"));
            Ok(())
        }
    }
}

fn logs(client: &Client, host: &str, id: &str, follow: bool, tail: Option<u64>) -> Result<()> {
    let session: berth_api::Session =
        serde_json::from_value(client.on(host, "sessions.get", json!({"id": id}), None)?)?;
    let mut out = std::io::stdout();
    if session.kind == SessionKind::Terminal {
        // A terminal's bytes are cursor movement; its screen is what reads.
        let mut args = json!({"id": id});
        if let Some(lines) = tail {
            args["tail"] = json!(lines);
        }
        let screen = client.on(host, "sessions.screen", args, None)?;
        println!("{}", screen["text"].as_str().unwrap_or("").trim_end());
        if follow && session.running() {
            out::hint(format!(
                "A terminal is followed with: berth session attach {host} {id}"
            ));
        }
        return Ok(());
    }
    let mut since = 0;
    loop {
        let args = match tail {
            Some(lines) => json!({"id": id, "tail": lines}),
            None => json!({"id": id, "since": since}),
        };
        let part: berth_api::Output =
            serde_json::from_value(client.on(host, "sessions.output", args, None)?)?;
        let bytes = berth_api::unb64(&part.data_b64).unwrap_or_default();
        match &part.lines {
            Some(lines) => lines.iter().try_for_each(|line| writeln!(out, "{line}"))?,
            None => out.write_all(&bytes)?,
        }
        let more = part.next_offset > since && !bytes.is_empty() && tail.is_none();
        since = part.next_offset;
        if !more {
            break;
        }
    }
    out.flush()?;
    if !follow || !session.running() {
        return Ok(());
    }
    let stream = attach_stream(client, host, id, since)?;
    let ended = client.wait(follow_process(stream, false))?;
    finish(client, host, id, ended, false)
}

// ── actions ─────────────────────────────────────────────────────────────────

pub fn action_table(actions: &[berth_api::Action]) -> Table {
    // Why one cannot run can be long, so it comes last and widens nothing else.
    let mut table = Table::new(&["action", "group", "label", "available"]);
    for action in actions {
        let available = match action.available {
            true => styled("yes", Style::Good),
            false => styled(&format!("no: {}", action.why), Style::Dim),
        };
        table.row(vec![
            action.name.clone(),
            action.group.clone(),
            action.label.clone(),
            available,
        ]);
    }
    table
}

pub fn actions(client: &Client, host: Option<&str>) -> Result<()> {
    let subject = match host {
        Some(host) => Subject::Host(host.to_owned()),
        None => Subject::Installation,
    };
    let actions = client.actions(&subject)?;
    out::data(&serde_json::to_value(&actions)?, || {
        action_table(&actions).print("Nothing to do");
        if let Some(host) = host {
            out::hint(format!(
                "Run one with: berth run {host} ACTION [NAME=VALUE…]"
            ));
        }
    });
    Ok(())
}

/// Ask for the arguments an action requires and was not given, and confirm
/// it when it asks to be.
fn complete(
    action: &berth_api::Action,
    given: &mut BTreeMap<String, String>,
    subject: &str,
) -> Result<()> {
    for argument in &action.arguments {
        if !argument.required
            || given
                .get(&argument.name)
                .is_some_and(|value| !value.is_empty())
        {
            continue;
        }
        let label = match argument.label.as_str() {
            "" => argument.name.clone(),
            label => label.to_owned(),
        };
        let flag = format!("{}=VALUE", argument.name);
        let value = match argument.secret {
            true => ask::secret(&label, &flag)?,
            false => ask::text(&label, &argument.help, &flag)?,
        };
        given.insert(argument.name.clone(), value);
    }
    if !action.available {
        return Err(Fail::refused(format!(
            "{} of {subject} cannot run now: {}",
            action.name, action.why
        )));
    }
    if let Some(question) = &action.confirm {
        ask::confirm(question)?;
    }
    Ok(())
}

fn find_action(
    actions: Vec<berth_api::Action>,
    name: &str,
    subject: &str,
    list: &str,
) -> Result<berth_api::Action> {
    actions
        .into_iter()
        .find(|action| action.name == name)
        .ok_or_else(|| Fail::usage(format!("{subject} has no action {name}. {list} names them")))
}

/// Run an action of a subject with its arguments, and follow what it started.
fn run_on(
    client: &Client,
    subject: &Subject,
    name: &str,
    words: &[String],
    detach: bool,
    sudo: Option<String>,
    own: bool,
) -> Result<Option<berth_api::Acted>> {
    let shown = subject.shown();
    let list = match subject {
        Subject::Host(host) => format!("berth action list {host}"),
        Subject::Provisioner(provisioner) => format!("berth provisioner actions {provisioner}"),
        Subject::Resource(id) => format!("berth resource actions {id}"),
        Subject::Installation => "berth action list".into(),
    };
    let action = find_action(client.actions(subject)?, name, &shown, &list)?;
    let mut given = arguments(words)?;
    complete(&action, &mut given, &shown)?;
    let (rows, cols) = terminal_size();
    let act = Act {
        arguments: given,
        sudo_password: sudo,
        rows: Some(rows),
        cols: Some(cols),
        ..Act::default()
    };
    let acted = client.act(subject, name, &act)?;
    // A terminal of the host's is attached to here.
    if acted.job.is_none()
        && let (Some(session), Subject::Host(host)) = (&acted.session, subject)
    {
        let stream = attach_stream(client, host, &session.id, 0)?;
        let ended = match session.kind {
            SessionKind::Terminal => client.wait(follow_terminal(stream))?,
            SessionKind::Process => client.wait(follow_process(stream, false))?,
        };
        finish(client, host, &session.id, ended, true)?;
        return Ok(None);
    }
    let label = match action.label.as_str() {
        "" => action.name.clone(),
        label => label.to_owned(),
    };
    let result = follow::acted(client, acted, &format!("{label} · {shown}"), detach, own)?;
    if !result.is_null() && result.get("id").is_none() {
        out::data(&result, || {
            for line in pages::answer_lines(&result) {
                println!("{line}");
            }
        });
    }
    Ok(None)
}

pub fn run_action(client: &Client, run: &args::RunAction) -> Result<()> {
    let sudo = match out::get().sudo_stdin {
        true => host::sudo_password(client, &run.host)?,
        false => None,
    };
    run_on(
        client,
        &Subject::Host(run.host.clone()),
        &run.action,
        &run.arguments,
        run.detach.detach,
        sudo,
        true,
    )?;
    Ok(())
}

// ── provisioners ────────────────────────────────────────────────────────────

pub fn provisioner(client: &Client, command: Provisioner) -> Result<()> {
    match command {
        Provisioner::List => {
            let found = client.call("provisioners.list", json!({}), None)?;
            out::data(&found, || {
                let mut table = Table::new(&["name", "type", "node", "usable", "label"]);
                for provisioner in items(found.clone(), "provisioners") {
                    let text = |key: &str| provisioner[key].as_str().unwrap_or("").to_owned();
                    let usable = match text("problem") {
                        problem if problem.is_empty() => styled("yes", Style::Good),
                        problem => styled(&format!("no: {problem}"), Style::Dim),
                    };
                    table.row(vec![
                        text("id"),
                        text("type"),
                        text("node"),
                        usable,
                        text("label"),
                    ]);
                }
                table.print("No provisioners. berth provisioner types lists what can be added");
            });
            Ok(())
        }
        Provisioner::Types => {
            let found = client.call("provisioners.types", json!({}), None)?;
            out::data(&found, || {
                let mut table = Table::new(&["type", "label", "makes"]);
                for kind in items(found.clone(), "types") {
                    let text = |key: &str| kind[key].as_str().unwrap_or("").to_owned();
                    let makes: Vec<&str> = kind["kinds"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|made| made["id"].as_str())
                        .collect();
                    table.row(vec![text("id"), text("label"), makes.join(", ")]);
                }
                table.print("No plugin brings a provisioner");
            });
            Ok(())
        }
        Provisioner::Add {
            name,
            kind,
            settings,
        } => {
            let body = json!({"type": kind, "settings": arguments(&settings)?});
            let saved = client.call("provisioners.save", json!({"name": name}), Some(body))?;
            out::data(&saved, || {
                out::done(format!("Added the provisioner {name}, of the type {kind}"))
            });
            Ok(())
        }
        Provisioner::Show { name, settings } => {
            let given = arguments(&settings)?;
            let mut args = json!({"name": name});
            for (key, value) in &given {
                args[key] = json!(value);
            }
            let found = client.call("provisioners.get", args, None)?;
            let state: berth_api::ProvisionerState = serde_json::from_value(found.clone())?;
            let actions = client
                .actions(&Subject::Provisioner(name.clone()))
                .unwrap_or_default();
            out::data(&found, || {
                print!("{}", pages::provisioner(&state, &actions, &given))
            });
            Ok(())
        }
        Provisioner::Set { name, settings } => {
            let run = settings_action(client, &name)?;
            let words: Vec<String> = settings;
            run_on(
                client,
                &Subject::Provisioner(name.clone()),
                &run,
                &words,
                false,
                None,
                false,
            )?;
            Ok(())
        }
        Provisioner::Remove { name } => {
            ask::confirm(&format!(
                "This forgets the provisioner {name} with its settings."
            ))?;
            client.call("provisioners.remove", json!({"name": name}), None)?;
            out::done(format!("Removed the provisioner {name}"));
            Ok(())
        }
        Provisioner::Actions { name } => {
            let actions = client.actions(&Subject::Provisioner(name.clone()))?;
            out::data(&serde_json::to_value(&actions)?, || {
                action_table(&actions).print("Nothing to do");
                out::hint(format!(
                    "Run one with: berth provisioner run {name} ACTION [NAME=VALUE…]"
                ));
            });
            Ok(())
        }
        Provisioner::Run {
            name,
            action,
            arguments,
            detach,
        } => {
            run_on(
                client,
                &Subject::Provisioner(name),
                &action,
                &arguments,
                detach.detach,
                None,
                false,
            )?;
            Ok(())
        }
    }
}

/// The action that keeps a provisioner's settings.
fn settings_action(client: &Client, name: &str) -> Result<String> {
    let actions = client.actions(&Subject::Provisioner(name.to_owned()))?;
    actions
        .iter()
        .find(|action| action.name == "settings")
        .map(|action| action.name.clone())
        .ok_or_else(|| Fail::failed(format!("{name} keeps no settings")))
}

// ── resources ───────────────────────────────────────────────────────────────

fn secrets_of(words: &[String]) -> Vec<berth_api::ResourceSecret> {
    words
        .iter()
        .map(|word| match word.split_once(':') {
            Some((name, variable)) => berth_api::ResourceSecret {
                name: name.to_owned(),
                env: variable.to_owned(),
            },
            None => berth_api::ResourceSecret {
                name: word.clone(),
                ..Default::default()
            },
        })
        .collect()
}

fn spec_of(spec: &Option<String>) -> Result<Option<String>> {
    spec.as_deref()
        .map(|file| {
            let bytes = host::read_file(file)?;
            String::from_utf8(bytes).map_err(|_| Fail::usage(format!("{file} is no text")))
        })
        .transpose()
}

/// The id of a resource given by its id, or by a name only it has.
pub(super) fn resource_id(client: &Client, given: &str) -> Result<String> {
    match client.call("resources.get", json!({"id": given}), None) {
        Ok(_) => return Ok(given.to_owned()),
        Err(fail) if fail.code == "not_found" => {}
        Err(fail) => return Err(fail),
    }
    let found = client.call("resources.list", json!({}), None)?;
    let named: Vec<String> = items(found, "resources")
        .into_iter()
        .filter(|made| made["name"] == given)
        .filter_map(|made| made["id"].as_str().map(str::to_owned))
        .collect();
    match named.as_slice() {
        [id] => Ok(id.clone()),
        [] => Err(Fail::failed(format!(
            "There is no resource {given}. berth resource list lists them"
        ))),
        _ => Err(Fail::usage(format!(
            "Several resources are named {given}: give one of their ids, {}",
            named.join(", ")
        ))),
    }
}

pub fn resource(client: &Client, command: Resource) -> Result<()> {
    match command {
        Resource::List {
            kind,
            provisioner,
            host,
        } => {
            let mut args = json!({});
            for (key, value) in [("kind", kind), ("provisioner", provisioner), ("host", host)] {
                if let Some(value) = value {
                    args[key] = json!(value);
                }
            }
            let found = client.call("resources.list", args, None)?;
            out::data(&found, || {
                let mut table = Table::new(&["id", "kind", "name", "provisioner", "state", "host"]);
                for made in items(found.clone(), "resources") {
                    let text = |key: &str| made[key].as_str().unwrap_or("").to_owned();
                    table.row(vec![
                        text("id"),
                        text("kind"),
                        text("name"),
                        text("provisioner"),
                        text("state"),
                        text("host"),
                    ]);
                }
                table.print("Nothing made yet");
            });
            Ok(())
        }
        Resource::Show { id } => {
            let id = resource_id(client, &id)?;
            let found = client.call("resources.get", json!({"id": id}), None)?;
            let made: berth_api::Resource = serde_json::from_value(found.clone())?;
            let actions = client
                .actions(&Subject::Resource(id.clone()))
                .unwrap_or_default();
            out::data(&found, || print!("{}", pages::resource(&made, &actions)));
            Ok(())
        }
        Resource::Create(create) => {
            // A specification is what a deployment is made from; without one it is a host's machine.
            let kind = match (&create.kind, &create.spec) {
                (Some(kind), _) => kind.clone(),
                (None, Some(_)) => "deployment".to_owned(),
                (None, None) => String::new(),
            };
            let found = client.call(
                "resources.list",
                json!({"provisioner": create.provisioner, "kind": kind}),
                None,
            )?;
            let existing = items(found, "resources").into_iter().find(|made| {
                made["name"] == create.name.as_str()
                    && made["external_id"]
                        .as_str()
                        .is_some_and(|id| !id.is_empty())
            });
            if let Some(existing) = existing
                && !kind.is_empty()
            {
                let id = existing["id"].as_str().unwrap_or("").to_owned();
                out::note(format!(
                    "There is a {kind} {} already, {id}: giving it this specification",
                    create.name
                ));
                let set = args::ResourceSet {
                    id,
                    spec: create.spec.clone(),
                    secret: create.secret.clone(),
                    arguments: create.arguments.clone(),
                    detach: create.detach.clone(),
                };
                return resource(client, Resource::Set(set));
            }
            let request = berth_api::NewResource {
                provisioner: create.provisioner.clone(),
                kind,
                name: create.name.clone(),
                arguments: arguments(&create.arguments)?,
                spec: spec_of(&create.spec)?,
                secrets: secrets_of(&create.secret),
                ..Default::default()
            };
            let made = client.call(
                "resources.create",
                json!({}),
                Some(serde_json::to_value(&request)?),
            )?;
            let made: berth_api::Created = serde_json::from_value(made)?;
            let id = made.resource.id.clone();
            let acted = berth_api::Acted {
                job: Some(made.job),
                ..Default::default()
            };
            follow::acted(
                client,
                acted,
                &format!("Creating {} with {}", create.name, create.provisioner),
                create.detach.detach,
                false,
            )?;
            out::hint(format!("{} is {id}: berth resource show {id}", create.name));
            Ok(())
        }
        Resource::Set(mut set) => {
            set.id = resource_id(client, &set.id)?;
            let change = berth_api::ResourceChange {
                arguments: arguments(&set.arguments)?,
                spec: spec_of(&set.spec)?,
                secrets: (!set.secret.is_empty()).then(|| secrets_of(&set.secret)),
                ..Default::default()
            };
            let acted: berth_api::Acted = serde_json::from_value(client.call(
                "resources.change",
                json!({"id": set.id}),
                Some(serde_json::to_value(&change)?),
            )?)?;
            follow::acted(
                client,
                acted,
                &format!("Changing {}", set.id),
                set.detach.detach,
                false,
            )?;
            Ok(())
        }
        Resource::Remove { id, detach } => {
            let id = resource_id(client, &id)?;
            ask::confirm(&format!("This removes {id} with its provisioner."))?;
            let acted: berth_api::Acted = serde_json::from_value(client.call(
                "resources.remove",
                json!({"id": id}),
                None,
            )?)?;
            follow::acted(
                client,
                acted,
                &format!("Removing {id}"),
                detach.detach,
                false,
            )?;
            Ok(())
        }
        Resource::Logs { id, follow, tail } => {
            let id = resource_id(client, &id)?;
            let mut args = json!({"id": id});
            if let Some(tail) = tail {
                args["tail"] = json!(tail);
            }
            if !follow {
                // A text answer comes as a string, or as its bytes in base64.
                let found = client.call("resources.logs", args, None)?;
                let text = match &found {
                    Value::String(text) => text.clone(),
                    other => other["base64"]
                        .as_str()
                        .and_then(berth_api::unb64)
                        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                        .unwrap_or_default(),
                };
                print!("{text}");
                return Ok(());
            }
            let mut stream = client.stream("resources.follow", None, args)?;
            client.wait(async {
                let mut out = std::io::stdout();
                loop {
                    tokio::select! {
                        piece = stream.recv() => match piece {
                            Some(Ok(Piece::Binary(bytes))) => { out.write_all(&bytes)?; out.flush()?; }
                            Some(Ok(Piece::Text(said))) => {
                                if let Some(line) = said["line"].as_str() { writeln!(out, "{line}")?; }
                            }
                            Some(Err(error)) => return Err(Fail::from(error)),
                            None => return Ok(()),
                        },
                        _ = tokio::signal::ctrl_c() => return Ok(()),
                    }
                }
            })
        }
        Resource::Exec {
            id,
            stdin,
            command,
            detach,
        } => {
            let id = resource_id(client, &id)?;
            let mut arguments = std::collections::BTreeMap::new();
            arguments.insert("command".to_owned(), serde_json::to_string(&command)?);
            if stdin {
                let bytes = super::host::read_file("-")?;
                let text = String::from_utf8(bytes)
                    .map_err(|_| Fail::usage("--stdin takes text, and this is not UTF-8"))?;
                arguments.insert("stdin".to_owned(), text);
            }
            let act = Act {
                arguments,
                ..Act::default()
            };
            let acted = client.act(&Subject::Resource(id.clone()), "exec", &act)?;
            follow::acted(
                client,
                acted,
                &format!("{} in {id}", command[0]),
                detach.detach,
                true,
            )?;
            Ok(())
        }
        Resource::Actions { id } => {
            let id = resource_id(client, &id)?;
            let actions = client.actions(&Subject::Resource(id.clone()))?;
            out::data(&serde_json::to_value(&actions)?, || {
                action_table(&actions).print("Nothing to do");
                out::hint(format!(
                    "Run one with: berth resource run {id} ACTION [NAME=VALUE…]"
                ));
            });
            Ok(())
        }
        Resource::Run {
            id,
            action,
            arguments,
            detach,
        } => {
            let id = resource_id(client, &id)?;
            run_on(
                client,
                &Subject::Resource(id),
                &action,
                &arguments,
                detach.detach,
                None,
                false,
            )?;
            Ok(())
        }
    }
}
