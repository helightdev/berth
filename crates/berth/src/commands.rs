//! The commands that talk to a host's controller: sessions, tasks, the
//! channel and services. Each is a plain program that waits on a runtime.

use crate::{
    Result,
    remote::{exited, list},
};
use berth_api::{
    self as api,
    client::{Error, Frame, encode, encode_path},
};
use berth_core::{
    host::Host,
    link::{self, Link},
    paths::Paths,
    provision, registry,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use tokio::io::AsyncReadExt;

/// Ctrl+Q, which leaves a terminal session without ending it.
const DETACH: u8 = 0x11;

struct Reach {
    host: Host,
    link: Link,
    /// One runtime for the whole command: the link's kept connections live on it.
    runtime: Option<tokio::runtime::Runtime>,
}

impl Reach {
    fn wait<F: std::future::Future>(&self, future: F) -> F::Output {
        self.runtime
            .as_ref()
            .expect("until dropped")
            .block_on(future)
    }
}

impl Drop for Reach {
    /// A read of the terminal that is still waiting must not keep berth from leaving.
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

fn reach(paths: &Paths, name: &str) -> Result<Reach> {
    let host = Host::load(paths, name)?;
    let password = host.sudo_password()?;
    let link = Link::new(host.link(password.as_deref())?);
    Ok(Reach {
        host,
        link,
        runtime: Some(link::runtime()?),
    })
}

/// An error of the link in words that say what to do about it.
fn explain(error: Error, host: &str) -> Box<dyn std::error::Error> {
    if Link::missing(&error) {
        return format!(
            "berth's controller is not on {host} yet. Apply its configuration first: berth {host}, then Apply this configuration."
        )
        .into();
    }
    error.into()
}

/// `HOST [--user USER] [--keep] [--] COMMAND…`
pub struct Exec {
    pub user: String,
    pub keep: bool,
    pub command: Vec<String>,
}

pub fn parse_exec(args: &[&str]) -> Result<Exec> {
    let mut exec = Exec {
        user: "agent".into(),
        keep: false,
        command: vec![],
    };
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match *arg {
            "--user" => {
                let user = rest.next().ok_or("--user needs agent, operator or root")?;
                if !["agent", "operator", "root"].contains(user) {
                    return Err("--user is agent, operator or root".into());
                }
                exec.user = (*user).to_owned();
            }
            "--keep" => exec.keep = true,
            "--" => {
                exec.command = rest.map(|word| (*word).to_owned()).collect();
                break;
            }
            word => {
                exec.command = std::iter::once(word)
                    .chain(rest.copied())
                    .map(str::to_owned)
                    .collect();
                break;
            }
        }
    }
    if exec.command.is_empty() {
        return Err(
            "Usage: berth exec HOST [--user agent|operator|root] [--keep] -- COMMAND…".into(),
        );
    }
    Ok(exec)
}

/// `NAME=VALUE…` and `--yes`.
pub fn parse_arguments(args: &[&str]) -> Result<(BTreeMap<String, String>, bool)> {
    let mut values = BTreeMap::new();
    let mut yes = false;
    for arg in args {
        match (*arg, arg.split_once('=')) {
            ("--yes" | "-y", _) => yes = true,
            (_, Some((name, value))) if !name.is_empty() && !name.starts_with('-') => {
                values.insert(name.to_owned(), value.to_owned());
            }
            _ => return Err(format!("Arguments are given as NAME=VALUE: {arg}").into()),
        }
    }
    Ok((values, yes))
}

/// Several hosts' inventories as one.
pub fn merge_inventory(hosts: &[(String, Value)]) -> Value {
    let mut all = serde_json::Map::new();
    for (name, inventory) in hosts {
        all.insert(
            name.clone(),
            inventory["all"]["hosts"]["devbox_target"].clone(),
        );
    }
    json!({"all": {"hosts": all}})
}

/// `berth inventory [HOST…]`: the hosts as an Ansible inventory.
pub fn inventory(paths: &Paths, names: &[&str]) -> Result<()> {
    let hosts = match names {
        [] => Host::all(paths)
            .into_iter()
            .filter(|host| !host.address().is_empty())
            .collect(),
        names => names
            .iter()
            .map(|name| Host::load(paths, name))
            .collect::<Result<Vec<_>>>()?,
    };
    let mut entries = vec![];
    for host in hosts {
        let inventory = host
            .inventory()
            .map_err(|error| format!("{}: {error}", host.name))?;
        entries.push((host.name, inventory));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&merge_inventory(&entries))?
    );
    Ok(())
}

/// How following a session ended.
enum Ended {
    Exited(Option<i32>),
    /// Left with Ctrl+Q or Ctrl+C; it runs on.
    Detached,
    Lost,
}

/// Follow a process session: its two streams to ours, apart. With `stdin`,
/// ours is its input and Ctrl+C interrupts it; without, Ctrl+C only leaves.
async fn follow_process(link: &Link, id: &str, since: u64, stdin: bool) -> Result<Ended> {
    let stream = link
        .stream(&format!("/v1/sessions/{id}?since={since}"))
        .await?;
    let (mut sink, mut source) = stream.split();
    let (sender, mut outgoing) = tokio::sync::mpsc::unbounded_channel::<Frame>();
    if stdin && !std::io::stdin().is_terminal() {
        let sender = sender.clone();
        tokio::spawn(async move {
            let mut input = tokio::io::stdin();
            let mut buffer = vec![0; 16384];
            while let Ok(count) = input.read(&mut buffer).await
                && count > 0
            {
                let frame = [&[0u8][..], &buffer[..count]].concat();
                if sender.send(Frame::Binary(frame.into())).is_err() {
                    return;
                }
            }
            let _ = sender.send(Frame::Text(json!({"eof": true}).to_string().into()));
        });
    } else if stdin {
        // Nothing will be typed into it.
        let _ = sender.send(Frame::Text(json!({"eof": true}).to_string().into()));
    }
    let (mut out, mut err) = (std::io::stdout(), std::io::stderr());
    loop {
        tokio::select! {
            frame = source.next() => match frame {
                Some(Ok(Frame::Binary(bytes))) => match bytes.split_first() {
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
                Some(Ok(Frame::Text(text))) => {
                    if let Some(code) = exited(text.as_str()) {
                        return Ok(Ended::Exited(code));
                    }
                }
                Some(Ok(_)) => {}
                _ => return Ok(Ended::Lost),
            },
            Some(frame) = outgoing.recv() => {
                if sink.send(frame).await.is_err() {
                    return Ok(Ended::Lost);
                }
            }
            _ = tokio::signal::ctrl_c() => {
                if !stdin {
                    return Ok(Ended::Detached);
                }
                let _ = sink.send(Frame::Text(json!({"signal": "INT"}).to_string().into())).await;
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
async fn follow_terminal(link: &Link, id: &str) -> Result<Ended> {
    let stream = link.stream(&format!("/v1/sessions/{id}")).await?;
    let (mut sink, mut source) = stream.split();
    let raw = Raw::enter();
    let resize = |(rows, cols): (u16, u16)| {
        Frame::Text(
            json!({"resize": {"rows": rows, "cols": cols}})
                .to_string()
                .into(),
        )
    };
    let mut size = terminal_size();
    if raw.0 {
        sink.send(resize(size))
            .await
            .map_err(|error| error.to_string())?;
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
            frame = source.next() => match frame {
                Some(Ok(Frame::Binary(bytes))) => {
                    out.write_all(&bytes)?;
                    out.flush()?;
                }
                Some(Ok(Frame::Text(text))) => {
                    if let Some(code) = exited(text.as_str()) {
                        return Ok(Ended::Exited(code));
                    }
                }
                Some(Ok(_)) => {}
                _ => return Ok(Ended::Lost),
            },
            Some(bytes) = typed.recv() => {
                if raw.0 && let Some(at) = bytes.iter().position(|byte| *byte == DETACH) {
                    if at > 0 {
                        let _ = sink.send(Frame::Binary(bytes[..at].to_vec().into())).await;
                    }
                    return Ok(Ended::Detached);
                }
                if sink.send(Frame::Binary(bytes.into())).await.is_err() {
                    return Ok(Ended::Lost);
                }
            }
            _ = tick.tick() => {
                let now = terminal_size();
                if raw.0 && now != size {
                    size = now;
                    let _ = sink.send(resize(size)).await;
                }
            }
        }
    }
}

/// What to say and which code to leave with once following has ended. A
/// session that was only for this call is forgotten when it has ended.
async fn finish(reach: &Reach, id: &str, ended: Ended, forget: bool) -> i32 {
    let name = &reach.host.name;
    match ended {
        Ended::Exited(code) => {
            if forget {
                let _ = reach
                    .link
                    .call::<Value>("DELETE", &format!("/v1/sessions/{id}"), None)
                    .await;
            }
            code.unwrap_or(1)
        }
        Ended::Detached => {
            eprintln!(
                "\r\nLeft {id} running on {name}. Back to it: berth attach {name} {id}, or berth output {name} {id} --follow"
            );
            0
        }
        Ended::Lost => {
            eprintln!(
                "\r\nThe connection was lost; {id} may still run on {name}: berth output {name} {id} --follow"
            );
            1
        }
    }
}

/// `berth exec HOST [--user …] [--keep] -- COMMAND…`: run a command on the
/// host as a process session and leave with its exit code.
pub fn exec(paths: &Paths, name: &str, args: &[&str]) -> Result<()> {
    let exec = parse_exec(args)?;
    let reach = reach(paths, name)?;
    let code = reach.wait(async {
        let body = json!({
            "kind": "process", "command": exec.command, "user": exec.user,
            "title": exec.command.join(" ").chars().take(200).collect::<String>(),
        });
        let session: api::Session = reach
            .link
            .post("/v1/sessions", &body)
            .await
            .map_err(|error| explain(error, name))?;
        let ended = follow_process(&reach.link, &session.id, 0, true).await?;
        Ok::<i32, Box<dyn std::error::Error>>(finish(&reach, &session.id, ended, !exec.keep).await)
    })?;
    std::process::exit(code);
}

/// `berth attach HOST [ID]`: this terminal onto a terminal session; without
/// an id, onto a new shell as the agent.
pub fn attach(paths: &Paths, name: &str, id: Option<&str>) -> Result<()> {
    let reach = reach(paths, name)?;
    let code = reach.wait(async {
        let (rows, cols) = terminal_size();
        let session: api::Session = match id {
            Some(id) => reach.link.get(&format!("/v1/sessions/{}", encode(id))).await,
            None => {
                let body = json!({
                    "kind": "terminal", "command": ["bash", "-l"], "user": "agent",
                    "title": format!("{} on {name}", reach.host.agent()), "rows": rows, "cols": cols,
                });
                reach.link.post("/v1/sessions", &body).await
            }
        }
        .map_err(|error| explain(error, name))?;
        let ended = match session.kind {
            api::SessionKind::Terminal => follow_terminal(&reach.link, &session.id).await?,
            api::SessionKind::Process => follow_process(&reach.link, &session.id, 0, false).await?,
        };
        // A shell started here and left with exit is done with.
        Ok::<i32, Box<dyn std::error::Error>>(finish(&reach, &session.id, ended, id.is_none()).await)
    })?;
    std::process::exit(code);
}

/// `berth sessions HOST`
pub fn sessions(paths: &Paths, name: &str) -> Result<()> {
    let reach = reach(paths, name)?;
    let sessions: Vec<api::Session> = reach
        .wait(reach.link.get::<Value>("/v1/sessions"))
        .map(|value| list(value, "sessions"))
        .map_err(|error| explain(error, name))?;
    for session in sessions {
        let state = match (session.running(), session.lost, session.exit_code) {
            (true, ..) => "running".to_owned(),
            (_, true, _) => "lost".to_owned(),
            (_, _, Some(code)) => format!("exit {code}"),
            _ => "ended".to_owned(),
        };
        let kind = match session.kind {
            api::SessionKind::Terminal => "terminal",
            api::SessionKind::Process => "process",
        };
        println!(
            "{:<14} {:<9} {:<9} {:<9} {}  {}",
            session.id, kind, state, session.user, session.started, session.title
        );
    }
    Ok(())
}

/// `berth output HOST ID [--tail N] [--follow]`
pub fn output(paths: &Paths, name: &str, id: &str, args: &[&str]) -> Result<()> {
    let (mut tail, mut follow) = (None, false);
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match *arg {
            "--follow" | "-f" => follow = true,
            "--tail" => {
                let count = rest.next().and_then(|count| count.parse::<u64>().ok());
                tail = Some(count.ok_or("--tail needs a number of lines")?);
            }
            _ => return Err("Usage: berth output HOST ID [--tail N] [--follow]".into()),
        }
    }
    let reach = reach(paths, name)?;
    let id = encode(id);
    let code = reach.wait(async {
        let link = &reach.link;
        let session: api::Session = link
            .get(&format!("/v1/sessions/{id}"))
            .await
            .map_err(|error| explain(error, name))?;
        let mut out = std::io::stdout();
        if session.kind == api::SessionKind::Terminal {
            // A terminal's bytes are cursor movement; its screen is what reads.
            let path = match tail {
                Some(lines) => format!("/v1/sessions/{id}/screen?tail={lines}"),
                None => format!("/v1/sessions/{id}/screen"),
            };
            let screen: api::Screen = link.get(&path).await?;
            println!("{}", screen.text.trim_end());
            if follow && session.running() {
                eprintln!("A terminal is followed with: berth attach {name} {id}");
            }
            return Ok::<i32, Box<dyn std::error::Error>>(0);
        }
        let mut since = 0;
        loop {
            let path = match tail {
                Some(lines) => format!("/v1/sessions/{id}/output?tail={lines}"),
                None => format!("/v1/sessions/{id}/output?since={since}"),
            };
            let part: api::Output = link.get(&path).await?;
            let bytes = api::unb64(&part.data_b64).unwrap_or_default();
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
            return Ok(0);
        }
        let ended = follow_process(link, &session.id, since, false).await?;
        Ok(finish(&reach, &session.id, ended, false).await)
    })?;
    std::process::exit(code);
}

fn host_tasks(reach: &Reach) -> Result<Vec<api::Task>> {
    reach
        .wait(reach.link.get::<Value>("/v1/tasks"))
        .map(|value| list(value, "tasks"))
        .map_err(|error| explain(error, &reach.host.name))
}

/// `berth tasks HOST`: what the host can do, by group.
pub fn tasks(paths: &Paths, name: &str) -> Result<()> {
    let reach = reach(paths, name)?;
    let tasks = host_tasks(&reach)?;
    let mut groups: Vec<(&str, Vec<&api::Task>)> = vec![];
    for task in tasks.iter().filter(|task| task.extends.is_empty()) {
        let title = if task.group.is_empty() {
            "System"
        } else {
            task.group.as_str()
        };
        match groups
            .iter_mut()
            .find(|(known, _)| known.eq_ignore_ascii_case(title))
        {
            Some((_, list)) => list.push(task),
            None => groups.push((title, vec![task])),
        }
    }
    for (title, list) in groups {
        println!("{title}");
        for task in list {
            let arguments: Vec<String> = task
                .arguments
                .iter()
                .map(|argument| match argument.required {
                    true => format!("{}=…", argument.name),
                    false => format!("[{}=…]", argument.name),
                })
                .collect();
            println!(
                "  {:<22} {}  {}",
                task.name,
                task.label,
                arguments.join(" ")
            );
            for step in tasks
                .iter()
                .filter(|step| step.extends.contains(&task.name))
            {
                println!("  {:<22}   + {}", "", step.label);
            }
        }
    }
    Ok(())
}

fn ask(question: &str) -> Result<String> {
    print!("{question} ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(answer.trim().to_owned())
}

/// `berth run HOST TASK [NAME=VALUE…] [--yes]`: run one of the host's tasks
/// and leave with its exit code.
pub fn run(paths: &Paths, name: &str, task: &str, args: &[&str]) -> Result<()> {
    let (mut values, yes) = parse_arguments(args)?;
    let reach = reach(paths, name)?;
    let tasks = host_tasks(&reach)?;
    let Some(found) = tasks.iter().find(|known| known.name == task) else {
        let names: Vec<&str> = tasks
            .iter()
            .filter(|task| task.extends.is_empty())
            .map(|task| task.name.as_str())
            .collect();
        return Err(format!("{name} has no task {task}. Tasks: {}", names.join(", ")).into());
    };
    for argument in &found.arguments {
        if values.contains_key(&argument.name) {
            continue;
        }
        if argument.required && std::io::stdin().is_terminal() {
            let label = if argument.label.is_empty() {
                &argument.name
            } else {
                &argument.label
            };
            values.insert(argument.name.clone(), ask(&format!("{label}:"))?);
        } else if argument.required {
            return Err(format!("{task} needs {}=VALUE", argument.name).into());
        }
    }
    if let (Some(question), false) = (&found.confirm, yes)
        && !ask(&format!("{question} [y/N]"))?.eq_ignore_ascii_case("y")
    {
        return Err("Not run".into());
    }
    let mut body = json!({"arguments": values});
    if found.terminal {
        let (rows, cols) = terminal_size();
        (body["rows"], body["cols"]) = (json!(rows), json!(cols));
    }
    // Secrets are built here and travel on the task's standard input. A
    // server builds them from its own, and starts the task itself.
    let served = match found.input.as_deref() {
        Some("secrets") => reach.host.served(),
        _ => None,
    };
    let started = match &served {
        Some(server) => Some(server.call("hosts.secrets", json!({"name": name}), None)?),
        None => None,
    };
    if found.input.as_deref() == Some("secrets") && served.is_none() {
        let payload = registry::Store::new(paths).host_payload(&reach.host.config)?;
        body["input_b64"] = json!(api::b64(format!("{payload}\n").as_bytes()));
    }
    let code = reach.wait(async {
        let path = format!("/v1/tasks/{}", encode(task));
        let session: api::Session = match started {
            Some(session) => serde_json::from_value(session)?,
            None => reach.link.post(&path, &body).await?,
        };
        let ended = match session.kind {
            api::SessionKind::Terminal => follow_terminal(&reach.link, &session.id).await?,
            api::SessionKind::Process => follow_process(&reach.link, &session.id, 0, false).await?,
        };
        Ok::<i32, Box<dyn std::error::Error>>(finish(&reach, &session.id, ended, true).await)
    })?;
    std::process::exit(code);
}

/// `berth channel HOST [TOPIC-PREFIX] [--since ID]`: messages as they come,
/// one JSON object per line.
pub fn channel(paths: &Paths, name: &str, args: &[&str]) -> Result<()> {
    let (mut prefix, mut since) = ("", None);
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match *arg {
            "--since" => {
                since = Some(
                    rest.next()
                        .ok_or("--since needs a message id")?
                        .parse::<u64>()?,
                )
            }
            topic if !topic.starts_with('-') && prefix.is_empty() => prefix = topic,
            _ => return Err("Usage: berth channel HOST [TOPIC-PREFIX] [--since ID]".into()),
        }
    }
    let reach = reach(paths, name)?;
    let mut path = format!("/v1/channel?topic={}", encode(prefix));
    if let Some(since) = since {
        path += &format!("&since={since}");
    }
    reach.wait(async {
        let mut stream = reach
            .link
            .stream(&path)
            .await
            .map_err(|error| explain(error, name))?;
        let mut out = std::io::stdout();
        while let Some(Ok(frame)) = stream.next().await {
            if let Frame::Text(text) = frame {
                writeln!(out, "{}", text.as_str())?;
                out.flush()?;
            }
        }
        Err("The connection was lost".into())
    })
}

/// `berth call HOST SERVICE METHOD [JSON]`: call a plugin's service.
pub fn call(
    paths: &Paths,
    name: &str,
    service: &str,
    method: &str,
    body: Option<&str>,
) -> Result<()> {
    let body: Value = match body {
        Some(text) => {
            serde_json::from_str(text).map_err(|error| format!("The body is not JSON: {error}"))?
        }
        None => json!({}),
    };
    let reach = reach(paths, name)?;
    let path = format!("/v1/services/{}/{}", encode_path(service), encode(method));
    let answer: Value = reach
        .wait(reach.link.post(&path, &body))
        .map_err(|error| explain(error, name))?;
    println!("{}", serde_json::to_string_pretty(&answer)?);
    Ok(())
}

/// `berth apply HOST`: apply the host's profile to it. On a berth server that
/// is a job of the server, whose output is shown as it comes.
pub fn apply(paths: &Paths, name: &str) -> Result<()> {
    let host = Host::load(paths, name)?;
    let Some(server) = host.served() else {
        let password = host.sudo_password()?;
        let mut prepared = berth_core::apply::prepare(paths, &host, password.as_deref())?;
        let status = prepared.command.status()?;
        std::process::exit(status.code().unwrap_or(1));
    };
    // The server has no terminal to ask on: a password it needs is asked for here.
    let with = host
        .sudo_password()?
        .map(|password| json!({"sudo_password": password}));
    let code = served_job(&server, "hosts.apply", json!({"name": name}), with)?;
    std::process::exit(code);
}

/// Start a job of the berth server with an operation, and its id.
async fn start_job(
    portal: &api::portal::Portal,
    op: &str,
    args: Value,
    data: Option<Value>,
) -> Result<String> {
    let job = portal.call(op, None, args, data).await?;
    Ok(job["id"].as_str().unwrap_or("").to_owned())
}

/// A job's output to `out` as it comes, until it ends, and its exit code.
async fn follow_job(portal: &api::portal::Portal, id: &str, out: &mut dyn Write) -> Result<i32> {
    let mut output = portal.open("jobs.follow", None, json!({"id": id})).await?;
    let mut code = None;
    while let Some(piece) = output.recv().await {
        match piece? {
            api::portal::Piece::Binary(data) => {
                out.write_all(&data)?;
                out.flush()?;
            }
            api::portal::Piece::Text(said) => {
                if let Some(exited) = said.get("exited") {
                    code = Some(exited["exit_code"].as_i64().unwrap_or(1) as i32);
                }
            }
        }
    }
    Ok(code.unwrap_or(1))
}

/// Run an operation that is a job of the berth server, with its output shown
/// as it comes, and the job's exit code.
fn served_job(
    server: &berth_core::server::Server,
    op: &str,
    args: Value,
    data: Option<Value>,
) -> Result<i32> {
    link::runtime()?.block_on(async {
        let portal = server.portal().await?;
        let id = start_job(&portal, op, args, data).await?;
        eprintln!("Job {id} on the berth server at {}", server.url);
        follow_job(&portal, &id, &mut std::io::stdout()).await
    })
}

/// A roll-out, from here or as a job of the berth server berth is set to
/// use. Its output goes to `out` as it comes. With `ask`, a host whose sudo
/// asks for a password has it asked for on the terminal.
pub fn rollout(
    paths: &Paths,
    options: &api::Rollout,
    ask: bool,
    out: &mut dyn Write,
) -> Result<api::RolloutReport> {
    let Some(server) = berth_core::server::Server::configured(paths) else {
        let password = |host: &Host| match ask {
            true => host.sudo_password().ok().flatten(),
            false => std::env::var("BERTH_SUDO_PASSWORD")
                .ok()
                .filter(|_| host.asks_password()),
        };
        return berth_core::rollout::local(paths, options, &password, out);
    };
    link::runtime()?.block_on(async {
        let portal = server.portal().await?;
        let options = Some(serde_json::to_value(options)?);
        let id = start_job(&portal, "hosts.rollout", Value::Null, options).await?;
        let _ = writeln!(out, "Job {id} on the berth server at {}", server.url);
        follow_job(&portal, &id, out).await?;
        let ended = portal
            .call("jobs.get", None, json!({"id": id}), None)
            .await?;
        Ok(serde_json::from_value(ended["report"].clone()).unwrap_or_default())
    })
}

/// `[--all] [--update] [--reboot] [--force]` of a roll-out.
pub fn parse_rollout(args: &[&str]) -> Result<api::Rollout> {
    let mut options = api::Rollout::default();
    for arg in args {
        match *arg {
            "--behind" => {}
            "--all" => options.all = true,
            "--update" => options.update = true,
            "--reboot" => options.reboot = true,
            "--force" => options.force = true,
            other => return Err(format!("Not an option of a roll-out: {other}").into()),
        }
    }
    if options.force && !options.reboot {
        return Err("--force only goes with --reboot".into());
    }
    Ok(options)
}

/// `berth apply --behind|--all [--update] [--reboot] [--force]`: bring the
/// hosts up to date, one at a time.
pub fn apply_hosts(paths: &Paths, args: &[&str]) -> Result<()> {
    let options = parse_rollout(args)?;
    let mut out = std::io::stdout();
    let report = rollout(paths, &options, true, &mut out)?;
    println!("{}", rollout_summary(&report));
    if !report.ok {
        std::process::exit(1);
    }
    Ok(())
}

/// A line per host of a roll-out's report.
pub fn rollout_summary(report: &api::RolloutReport) -> String {
    berth_tui::plugins::summary(report).join("\n")
}

/// `berth plugins update [ID] [--check] [--apply [--reboot [--force]]]`.
pub fn update_plugins(paths: &Paths, args: &[&str]) -> Result<()> {
    let (mut name, mut check, mut rollout_args) = (None, false, vec![]);
    let mut apply = false;
    for arg in args {
        match *arg {
            "--check" => check = true,
            "--apply" => apply = true,
            "--reboot" | "--force" | "--update" => rollout_args.push(*arg),
            word if !word.starts_with('-') && name.is_none() => name = Some(word),
            other => return Err(format!("Not an option of berth plugins update: {other}").into()),
        }
    }
    if !rollout_args.is_empty() && !apply {
        return Err(format!("{} goes with --apply", rollout_args[0]).into());
    }
    let manager = berth_core::plugins::Manager::new(paths);
    if check {
        let checked = manager.check()?;
        for plugin in checked.plugins {
            let state = match (&plugin.latest, plugin.note.as_str()) {
                (Some(latest), _) => format!("{} → {latest}", plugin.version),
                (None, "") => format!("{} is the latest", plugin.version),
                (None, note) => format!("{}: {note}", plugin.version),
            };
            println!("{:<16} {state}", plugin.id);
        }
        return Ok(());
    }
    let mut report = manager.update(name)?;
    for line in &report.lines {
        println!("{line}");
    }
    if manager.server().is_none() {
        report.behind = berth_core::rollout::survey(paths);
    }
    for host in &report.behind {
        println!("{} needs applying: {}", host.name, host.behind.text());
    }
    if !apply {
        if !report.behind.is_empty() {
            println!("berth apply --behind brings them up to date.");
        }
        return Ok(());
    }
    apply_hosts(paths, &rollout_args)
}

/// `berth seed HOST` with a berth server: the server renders the seed, and it
/// is written here.
pub fn seed(paths: &Paths, name: &str) -> Result<()> {
    let host = Host::load(paths, name)?;
    let server = host.served().ok_or("Not a host of a berth server")?;
    let seed = server.call("hosts.seed", json!({"name": name}), None)?;
    let dir = paths.build_dir().join(name).join("cloud");
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
    }
    let file = dir.join("user-data");
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&file)?;
        out.write_all(seed["user_data"].as_str().unwrap_or("").as_bytes())?;
    }
    println!("{}", file.display());
    Ok(())
}

/// `berth remove HOST`: forget a host. The machine itself is left as it is,
/// and a host with a machine berth created keeps it until that is deleted.
pub fn remove(paths: &Paths, name: &str) -> Result<()> {
    let host = Host::load(paths, name).map_err(|_| format!("No host {name}"))?;
    if let Some(record) = provision::Record::load(paths, name)?
        && !record.id.is_empty()
    {
        return Err(format!(
            "{name} has a machine berth created, {} of {}. Delete it first: berth instance {name} destroy",
            record.id, record.provisioner
        )
        .into());
    }
    if let Some(server) = host.served() {
        server.call("hosts.remove", json!({"name": name}), None)?;
    }
    std::fs::remove_file(&host.path)?;
    for beside in ["credentials.json", "instance.json", "instance.lock"] {
        let _ = std::fs::remove_file(host.path.with_extension(beside));
    }
    println!("Removed {name}");
    Ok(())
}

// ── Provisioners ────────────────────────────────────────────────────────────

/// Provisioners run where the hosts are kept: with a berth server that is
/// the server, which is asked for each of them.
fn provisioning_server(paths: &Paths) -> Option<berth_core::server::Server> {
    berth_core::server::Server::configured(paths)
}

/// The provisioners there are, here or on the berth server.
fn known_provisioners(
    paths: &Paths,
    server: Option<&berth_core::server::Server>,
) -> Result<Vec<api::ProvisionerInfo>> {
    match server {
        Some(server) => Ok(serde_json::from_value(server.call(
            "provisioners.list",
            Value::Null,
            None,
        )?)?),
        None => Ok(provision::list(paths, &registry::Store::new(paths))),
    }
}

/// `berth provisioners`: what can create machines, and what each asks for.
pub fn provisioners(paths: &Paths) -> Result<()> {
    let found = known_provisioners(paths, provisioning_server(paths).as_ref())?;
    if found.is_empty() {
        println!("No plugin brings a provisioner. See: berth plugins");
    }
    for provisioner in found {
        println!("{:<24} {}", provisioner.id, provisioner.label);
        if !provisioner.description.is_empty() {
            println!("  {}", provisioner.description);
        }
        for note in berth_core::plugins::provisioner_notes(&provisioner) {
            println!("  · {note}");
        }
    }
    Ok(())
}

/// `berth provisioners PLUGIN/ID [NAME=VALUE…]`: the machines it has that
/// berth created.
pub fn provisioner_instances(paths: &Paths, name: &str, args: &[&str]) -> Result<()> {
    let (values, _) = parse_arguments(args)?;
    let listed: Vec<api::ListedInstance> = match provisioning_server(paths) {
        Some(server) => {
            let (plugin, id) = name
                .split_once('/')
                .ok_or_else(|| format!("No provisioner {name}. See: berth provisioners"))?;
            // The arguments are the query of the request, beside its path.
            let mut asked = json!(values);
            (asked["plugin"], asked["id"]) = (json!(plugin), json!(id));
            serde_json::from_value(server.call("provisioners.instances", asked, None)?)?
        }
        None => {
            let store = registry::Store::new(paths);
            provision::instances(paths, &store, name, &values, &Default::default())?
        }
    };
    for instance in listed {
        println!(
            "{:<24} {:<10} {}",
            instance.host, instance.state, instance.id
        );
    }
    Ok(())
}

/// `berth provision HOST PLUGIN/ID [NAME=VALUE…] [--yes]`: create the
/// machine for a saved host and set it up. On a berth server that is a job of
/// the server, whose output is shown as it comes.
pub fn provision(paths: &Paths, name: &str, provisioner: &str, args: &[&str]) -> Result<()> {
    let (mut values, yes) = parse_arguments(args)?;
    let server = provisioning_server(paths);
    let known = known_provisioners(paths, server.as_ref())?;
    let found = known
        .iter()
        .find(|known| known.id == provisioner)
        .ok_or_else(|| format!("No provisioner {provisioner}. See: berth provisioners"))?;
    // What is required and has no default is asked for here, as `berth run` does.
    if std::io::stdin().is_terminal() {
        for argument in &found.arguments {
            if argument.required
                && argument.default.is_empty()
                && !values.contains_key(&argument.name)
            {
                let label = match argument.label.as_str() {
                    "" => &argument.name,
                    label => label,
                };
                values.insert(argument.name.clone(), ask(&format!("{label}:"))?);
            }
        }
    }
    if !yes
        && !ask(&format!(
            "Create a machine for {name} with {} ({provisioner})? [y/N]",
            found.label
        ))?
        .eq_ignore_ascii_case("y")
    {
        return Err("Not created".into());
    }
    let asked = api::Provision {
        provisioner: provisioner.to_owned(),
        arguments: values,
    };
    if let Some(server) = server {
        let asked = Some(serde_json::to_value(&asked)?);
        let code = served_job(&server, "instances.create", json!({"name": name}), asked)?;
        std::process::exit(code);
    }
    provision::provision(
        paths,
        &registry::Store::new(paths),
        name,
        &asked,
        &Default::default(),
        &mut std::io::stdout(),
    )
}

/// `berth instance HOST [start|stop|destroy [--yes]]`: the machine berth
/// created for the host, or a change to it.
pub fn instance(paths: &Paths, name: &str, args: &[&str]) -> Result<()> {
    let server = provisioning_server(paths);
    let store = registry::Store::new(paths);
    let options = provision::Options::default();
    let mut out = std::io::stdout();
    let named = json!({"name": name});
    match args {
        [] => {
            let instance: api::Instance = match &server {
                Some(server) => {
                    serde_json::from_value(server.call("instances.get", named, None)?)?
                }
                None => provision::status(paths, &store, name, &options)?
                    .ok_or_else(|| format!("{name} has no machine that berth created"))?,
            };
            print!("{}", describe_instance(name, &instance));
            Ok(())
        }
        [verb @ ("start" | "stop")] => {
            match &server {
                Some(server) => {
                    let asked = json!({"name": name, "verb": verb});
                    server.call("instances.power", asked, None)?;
                }
                None => provision::power(paths, &store, name, verb, &options, &mut out)?,
            }
            println!("✓ {name}: {verb} asked of {name}'s machine");
            Ok(())
        }
        ["destroy", rest @ ..] => {
            let (_, yes) = parse_arguments(rest)?;
            if !yes && ask(&format!("Delete {name}'s machine? Type {name} to say so:"))? != name {
                return Err("Not deleted".into());
            }
            match &server {
                Some(server) => {
                    std::process::exit(served_job(server, "instances.destroy", named, None)?)
                }
                None => provision::destroy(paths, &store, name, &options, &mut out),
            }
        }
        _ => Err("Usage: berth instance HOST [start|stop|destroy [--yes]]".into()),
    }
}

/// What `berth instance HOST` says of the machine.
pub fn describe_instance(name: &str, instance: &api::Instance) -> String {
    let mut text = format!("{name}  {}  {}\n", instance.provisioner, instance.id);
    text += &format!("State      {}\n", instance.state);
    if !instance.problem.is_empty() {
        text += &format!("Problem    {}\n", instance.problem);
    }
    if !instance.address.is_empty() {
        text += &format!("Address    {}\n", instance.address);
    }
    text += &format!("Created    {}\n", instance.created);
    let arguments: Vec<String> = instance
        .arguments
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    if !arguments.is_empty() {
        text += &format!("Arguments  {}\n", arguments.join(" "));
    }
    let mut verbs = instance.verbs.clone();
    verbs.push("destroy".into());
    text += &format!("Can        berth instance {name} {}\n", verbs.join("|"));
    text
}

/// `berth login`: get an access token for the berth server from its issuer.
/// A person approves in a browser on any device; a client with a secret
/// needs nobody.
pub fn login(paths: &Paths) -> Result<()> {
    use berth_core::{oauth, server::Server};
    let server =
        Server::configured(paths).ok_or("Set the berth server first: berth config server URL")?;
    if server.token.is_some() {
        eprintln!(
            "server_token is set and is used before a login. Clear it: berth config server_token"
        );
    }
    let role = link::runtime()?.block_on(async {
        let client = oauth::Client::configured(paths, &server.url)
            .await?
            .ok_or("Say which client berth is at the issuer: berth config oauth_client_id ID")?;
        let tokens = match client.secret {
            Some(_) => client.credentials().await?,
            None => {
                client
                    .device(|asked| {
                        let text = |name: &str| asked[name].as_str().unwrap_or("").to_owned();
                        match text("verification_uri_complete").as_str() {
                            "" => println!(
                                "Open {} and enter {}",
                                text("verification_uri"),
                                text("user_code")
                            ),
                            link => println!(
                                "Open {link}\nThe code there should be {}",
                                text("user_code")
                            ),
                        }
                        println!("Waiting for you to approve…");
                    })
                    .await?
            }
        };
        oauth::save(paths, &server.url, Some(&tokens))?;
        let own = Server {
            url: server.url.clone(),
            token: None,
        };
        let access = own
            .portal()
            .await?
            .call("access", None, Value::Null, None)
            .await?;
        Ok::<String, Box<dyn std::error::Error>>(access["role"].as_str().unwrap_or("?").to_owned())
    })?;
    println!("Logged in to {} with the role {role}", server.url);
    Ok(())
}

/// `berth logout`: forget the login at the berth server.
pub fn logout(paths: &Paths) -> Result<()> {
    let server = berth_core::server::Server::configured(paths).ok_or("No berth server is set")?;
    berth_core::oauth::save(paths, &server.url, None)?;
    println!("Forgot the login at {}", server.url);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_takes_its_flags_before_the_command() {
        let exec = parse_exec(&["--user", "root", "--", "ls", "--keep"]).unwrap();
        assert_eq!((exec.user.as_str(), exec.keep), ("root", false));
        assert_eq!(exec.command, ["ls", "--keep"]);
        let exec = parse_exec(&["--keep", "uname", "-a"]).unwrap();
        assert!(exec.keep && exec.user == "agent");
        assert_eq!(exec.command, ["uname", "-a"]);
        assert!(parse_exec(&["--user", "nobody", "true"]).is_err());
        assert!(parse_exec(&["--keep"]).is_err());
    }

    #[test]
    fn task_arguments_are_names_with_values() {
        let (values, yes) = parse_arguments(&["folder=~/my project", "--yes", "empty="]).unwrap();
        assert!(yes);
        assert_eq!(values["folder"], "~/my project");
        assert_eq!(values["empty"], "");
        assert!(parse_arguments(&["folder"]).is_err());
        assert!(parse_arguments(&["=x"]).is_err());
    }

    #[test]
    fn a_host_with_a_machine_berth_created_stays_until_it_is_deleted() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: home.path().join("config"),
            data_dir: home.path().join("data"),
        };
        paths.prepare().unwrap();
        let profile = paths.profile("studio").unwrap();
        std::fs::write(&profile, r#"{"builder": {"ssh_host": "192.0.2.20"}}"#).unwrap();
        let mut record = provision::Record {
            provisioner: "cloud/vm".into(),
            id: "vm-1".into(),
            token: "t".into(),
            ..Default::default()
        };
        record.save(&paths, "studio").unwrap();
        let said = remove(&paths, "studio").unwrap_err().to_string();
        assert!(said.contains("berth instance studio destroy"), "{said}");
        assert!(profile.is_file());
        // One whose creation never ended goes with the host.
        record.id.clear();
        record.save(&paths, "studio").unwrap();
        remove(&paths, "studio").unwrap();
        assert!(!profile.exists());
        assert!(provision::Record::load(&paths, "studio").unwrap().is_none());
    }

    #[test]
    fn hosts_share_one_inventory() {
        let one = berth_core::host::inventory("10.0.0.1", "ops", "", "").unwrap();
        let two = berth_core::host::inventory("box", "", "2222", "").unwrap();
        let merged = merge_inventory(&[("server".into(), one), ("laptop".into(), two)]);
        assert_eq!(merged["all"]["hosts"]["server"]["ansible_user"], "ops");
        assert_eq!(merged["all"]["hosts"]["laptop"]["ansible_port"], 2222);
    }
}
