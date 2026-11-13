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
    level,
    link::{self, Link},
    paths::Paths,
    profiles::Place,
    provision, registry, runner,
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
    let password = host.sudo_password(paths)?;
    let link = Link::new(host.link(paths, password.as_deref())?);
    Ok(Reach {
        host,
        link,
        runtime: Some(link::runtime()?),
    })
}

/// The way to a host's controller, for what needs one: a host meant to be
/// only reachable is refused with the action that installs it.
fn controlled(paths: &Paths, name: &str) -> Result<Reach> {
    let reach = reach(paths, name)?;
    level::guard(&reach.host, level::Capability::Sessions)?;
    Ok(reach)
}

/// An error of the link in words that say what to do about it.
fn explain(error: Error, host: &str) -> Box<dyn std::error::Error> {
    if Link::missing(&error) {
        return format!(
            "berth's controller is not on {host} yet. Install it alone with: berth host install {host}. For a workstation, apply its configuration: berth apply {host}"
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
            .inventory(paths)
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
    let reach = controlled(paths, name)?;
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
    let reach = controlled(paths, name)?;
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
    let reach = controlled(paths, name)?;
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
    let reach = controlled(paths, name)?;
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

/// What a host can do, with the tasks its controller lists for a host of
/// this machine. A berth server's host has the server's list of its
/// actions, which has berth's own beside its tasks.
fn host_actions(paths: &Paths, reach: &Reach) -> Result<(Vec<api::Action>, Vec<api::Task>)> {
    match reach.host.served(paths) {
        Some(server) => {
            let listed = server.call("hosts.actions", json!({"name": reach.host.name}), None)?;
            Ok((serde_json::from_value(listed)?, vec![]))
        }
        None => {
            // A host that is only reachable has no controller to ask.
            let host_level = level::of(&reach.host.config);
            let tasks = match host_level {
                level::Level::Reachable => vec![],
                _ => host_tasks(reach)?,
            };
            let mut actions = api::Action::of_tasks(&tasks);
            let own = level::local_actions(&reach.host.name, host_level, observed(reach));
            actions.retain(|action| !own.iter().any(|known| known.name == action.name));
            actions.extend(own);
            Ok((actions, tasks))
        }
    }
}

/// What the host's controller says the host is, when one answers in time.
/// Only a host whose profile says less than a workstation is asked.
fn observed(reach: &Reach) -> Option<level::Level> {
    if level::of(&reach.host.config) == level::Level::Workstation {
        return None;
    }
    let asked = reach.wait(async {
        match tokio::time::timeout(
            std::time::Duration::from_secs(15),
            reach.link.get::<api::Version>("/v1/version"),
        )
        .await
        {
            Ok(Ok(version)) => Ok(version),
            _ => Err(()),
        }
    });
    asked
        .ok()
        .as_ref()
        .and_then(|version| level::seen(Ok(version)))
}

/// `berth tasks HOST`: what the host can do, by group.
pub fn tasks(paths: &Paths, name: &str) -> Result<()> {
    let reach = reach(paths, name)?;
    let (actions, _) = host_actions(paths, &reach)?;
    if let Some(said) = level::mismatch(name, level::of(&reach.host.config), observed(&reach)) {
        println!("! {said}\n");
    }
    let mut groups: Vec<(&str, Vec<&api::Action>)> = vec![];
    for action in &actions {
        let title = if action.group.is_empty() {
            "System"
        } else {
            action.group.as_str()
        };
        match groups
            .iter_mut()
            .find(|(known, _)| known.eq_ignore_ascii_case(title))
        {
            Some((_, list)) => list.push(action),
            None => groups.push((title, vec![action])),
        }
    }
    for (title, list) in groups {
        println!("{title}");
        for action in list {
            let arguments: Vec<String> = action
                .arguments
                .iter()
                .map(|argument| match argument.required {
                    true => format!("{}=…", argument.name),
                    false => format!("[{}=…]", argument.name),
                })
                .collect();
            let why = match action.available {
                true => String::new(),
                false => format!("  ({})", action.why),
            };
            println!(
                "  {:<22} {}  {}{why}",
                action.name,
                action.label,
                arguments.join(" ")
            );
            for step in &action.steps {
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

/// The action of that name, with the values of its arguments: those given,
/// and the required ones asked for on a terminal. Its question is asked
/// unless `yes`.
pub(crate) fn ready(
    subject: &str,
    actions: &[api::Action],
    name: &str,
    mut values: BTreeMap<String, String>,
    yes: bool,
) -> Result<(api::Action, BTreeMap<String, String>)> {
    let Some(found) = actions.iter().find(|known| known.name == name) else {
        let names: Vec<&str> = actions.iter().map(|known| known.name.as_str()).collect();
        return Err(format!("{subject} has no task {name}. Tasks: {}", names.join(", ")).into());
    };
    if !found.available {
        return Err(format!("{name} cannot run now: {}", found.why).into());
    }
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
            let value = match argument.secret {
                true => registry::read_secret(label)?,
                false => ask(&format!("{label}:"))?,
            };
            values.insert(argument.name.clone(), value);
        } else if argument.required {
            return Err(format!("{name} needs {}=VALUE", argument.name).into());
        }
    }
    if let (Some(question), false) = (&found.confirm, yes)
        && !ask(&format!("{question} [y/N]"))?.eq_ignore_ascii_case("y")
    {
        return Err("Not run".into());
    }
    Ok((found.clone(), values))
}

/// `berth run HOST TASK [NAME=VALUE…] [--yes]`: run one of the host's
/// actions and leave with its exit code.
pub fn run(paths: &Paths, name: &str, task: &str, args: &[&str]) -> Result<()> {
    let (values, yes) = parse_arguments(args)?;
    let reach = reach(paths, name)?;
    let (actions, tasks) = host_actions(paths, &reach)?;
    let (action, values) = ready(name, &actions, task, values, yes)?;
    let mut act = api::Act {
        arguments: values,
        ..api::Act::default()
    };
    if action.terminal {
        let (rows, cols) = terminal_size();
        (act.rows, act.cols) = (Some(rows), Some(cols));
    }
    let code = match reach.host.served(paths) {
        Some(server) => {
            // The server has no terminal to ask on: a password it needs is asked for here.
            act.sudo_password = reach.host.sudo_password(paths)?;
            let acted = act_on(&server, Some(name), &action.name, &act)?;
            follow_acted(&server, Some(&reach), acted, &mut std::io::stdout())?
        }
        None if action.runs == api::Runs::Berth => run_own(paths, &reach.host, &action.name)?,
        None => run_here(paths, &reach, &tasks, &action, act)?,
    };
    std::process::exit(code);
}

/// Run one of berth's own actions on a host of this machine: the exit code.
fn run_own(paths: &Paths, host: &Host, action: &str) -> Result<i32> {
    match action {
        "install-controller" => install_controller_here(paths, host),
        other => Err(format!("{other} is not an action of berth on {}", host.name).into()),
    }
}

/// `berth host install HOST`: put berth's controller alone on the host. On
/// a berth server that is a job of the server, whose output is shown.
pub fn install_controller(paths: &Paths, name: &str) -> Result<()> {
    let host = Host::load(paths, name)?;
    let Some(server) = host.served(paths) else {
        let code = install_controller_here(paths, &host)?;
        std::process::exit(code);
    };
    let act = api::Act {
        sudo_password: host.sudo_password(paths)?,
        ..api::Act::default()
    };
    let acted = act_on(&server, Some(name), "install-controller", &act)?;
    let code = follow_acted(&server, None, acted, &mut std::io::stdout())?;
    std::process::exit(code);
}

/// Install the controller from here, holding the host's lock as an apply
/// does: the exit code.
fn install_controller_here(paths: &Paths, host: &Host) -> Result<i32> {
    level::guard(host, level::Capability::InstallController)?;
    let password = host.sudo_password(paths)?;
    let request = runner::Request {
        secrets: password.iter().cloned().collect(),
        ..runner::Request::new(
            runner::Subject::Host(host.name.clone()),
            "install-controller",
        )
    };
    let (owned, place) = (paths.clone(), host.place.clone());
    let host = Host {
        name: host.name.clone(),
        place,
        config: host.config.clone(),
    };
    let work = runner::Work::Function(Box::new(move |out| {
        berth_core::apply::install_controller(&owned, &host, password.as_deref(), out)
    }));
    let code = runner::here(paths, request, work, &mut std::io::stdout())?;
    Ok(code.unwrap_or(1))
}

/// `berth host level HOST LEVEL`: save the level the host's profile says,
/// such as for a workstation added by its address. Nothing on the host
/// changes; a workstation's set-up then fills in the rest before an apply.
pub fn set_level(paths: &Paths, name: &str, given: &str) -> Result<()> {
    let wanted = level::Level::parse(given)
        .ok_or_else(|| format!("A level is reachable, controller or workstation, not {given}"))?;
    let host = Host::load(paths, name)?;
    let mut profile = host.config.clone();
    api::profile::set_level(&mut profile, wanted);
    if wanted == level::Level::Workstation && !profile["builder"].is_object() {
        profile["builder"] = json!({});
    }
    match host.served(paths) {
        Some(server) => {
            server.save_host(&host.name, &profile)?;
            server.mirror(paths)?;
        }
        None => host.place.save(paths, &profile)?,
    }
    println!("{} is a {wanted} host now in its profile", host.name);
    if wanted == level::Level::Workstation {
        println!("Check its set-up, then apply it: berth apply {}", host.name);
    }
    Ok(())
}

/// Offer to save the level a host just added was found to have, when it is
/// more than its profile says: a workstation added by its address is not
/// to be treated as a bare machine.
fn offer_found_level(paths: &Paths, name: &str, intended: level::Level) -> Result<()> {
    let Ok(reach) = reach(paths, name) else {
        return Ok(());
    };
    let found = {
        let asked = reach.wait(async {
            tokio::time::timeout(
                std::time::Duration::from_secs(15),
                reach.link.get::<api::Version>("/v1/version"),
            )
            .await
        });
        match asked {
            Ok(Ok(version)) => level::seen(Ok(&version)),
            _ => None,
        }
    };
    drop(reach);
    let (Some(found), Some(said)) = (found, level::mismatch(name, intended, found)) else {
        return Ok(());
    };
    println!("! {said}");
    // A workstation keeps its profile, which says more than a level would.
    if found == level::Level::Workstation {
        if std::io::stdin().is_terminal()
            && !ask(&format!("Adopt {name} from the profile it keeps? [Y/n]"))?
                .eq_ignore_ascii_case("n")
        {
            return adopt(paths, name, &[]);
        }
        println!("Adopt it with: berth adopt {name}");
        return Ok(());
    }
    if std::io::stdin().is_terminal()
        && !ask(&format!("Save {name} as a {found} host? [Y/n]"))?.eq_ignore_ascii_case("n")
    {
        return set_level(paths, name, found.name());
    }
    println!("Save it with: berth host level {name} {found}");
    Ok(())
}

/// `berth host add NAME ADDRESS [--user USER] [--port PORT] [--auth AUTH]
/// [--level LEVEL] [--no-contact]`: save a host that exists already, by
/// where it is and how berth logs in, with nothing of a workstation. Then
/// first contact, unless told not to: berth's key is tried, and a host that
/// refuses it is logged in to once with a password or key to install it.
pub fn add_host(paths: &Paths, name: &str, address: &str, args: &[&str]) -> Result<()> {
    const USAGE: &str = "Usage: berth host add NAME ADDRESS [--user USER] [--port PORT] [--auth berth|key:NAME|password|ssh] [--level reachable|controller|workstation] [--no-contact]";
    let mut access = api::profile::Access {
        address: address.to_owned(),
        ..api::profile::Access::default()
    };
    let (mut host_level, mut contact) = (level::Level::Reachable, true);
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let mut value = || rest.next().copied().ok_or(USAGE);
        match *arg {
            "--user" => access.user = value()?.to_owned(),
            "--port" => access.port = value()?.to_owned(),
            "--auth" => {
                let given = value()?;
                access.auth = api::profile::Auth::parse(given).ok_or_else(|| {
                    format!("--auth is berth, key:NAME, password or ssh, not {given}")
                })?;
            }
            "--level" => {
                let given = value()?;
                host_level = level::Level::parse(given).ok_or_else(|| {
                    format!("--level is reachable, controller or workstation, not {given}")
                })?;
            }
            "--no-contact" => contact = false,
            _ => return Err(USAGE.into()),
        }
    }
    if !berth_core::profiles::valid_name(name) {
        return Err(
            format!("{name} cannot be a host's name: letters, digits, -, _ and . only").into(),
        );
    }
    let mut profile = api::profile::minimal(host_level, &access);
    // A workstation keeps what its set-up fills in; it starts empty.
    if host_level == level::Level::Workstation {
        profile["builder"] = json!({});
    }
    let problems = api::profile::problems(&profile);
    if !problems.is_empty() {
        return Err(problems.join("; ").into());
    }
    let place = Place::resolve(paths, name)?;
    if place.load(paths)?.is_some() {
        return Err(format!(
            "There is a host {name} already. Change it with: berth profile {name} --set FILE"
        )
        .into());
    }
    match berth_core::server::Server::configured(paths) {
        Some(server) => {
            server.save_host(name, &profile)?;
            server.mirror(paths)?;
            println!("Added {name} on the berth server at {}", server.url);
        }
        None => {
            if let Some(key) = access.auth.key() {
                let store = berth_core::store::shared(paths)?;
                if key != api::profile::BERTH_KEY
                    && !berth_core::keys::ring::names(&store)?
                        .iter()
                        .any(|known| known == key)
                {
                    return Err(format!(
                        "There is no key {key}. Add it with: berth key add {key} FILE"
                    )
                    .into());
                }
            }
            place.save(paths, &profile)?;
            println!("Added {name} as a {host_level} host: {}", place.describe());
            seed_host_key(paths, &place, &access)?;
        }
    }
    if !contact {
        return Ok(());
    }
    crate::key_command::trust(paths, name, &[]).map_err(|error| -> Box<dyn std::error::Error> {
        format!("{name} is saved, but logging in to it did not work yet: {error}. Try again with: berth key trust {name}").into()
    })?;
    offer_found_level(paths, name, host_level)
}

/// Take the host key of a host just added from the user's own
/// `~/.ssh/known_hosts`, when that knows the address, and say which.
fn seed_host_key(paths: &Paths, place: &Place, access: &api::profile::Access) -> Result<()> {
    let Some(home) = std::env::var_os("HOME") else {
        return Ok(());
    };
    let known_hosts = std::path::PathBuf::from(home).join(".ssh/known_hosts");
    let names = match access.port.as_str() {
        "" | "22" => vec![access.address.clone()],
        port => vec![format!("[{}]:{port}", access.address)],
    };
    let store = berth_core::store::shared(paths)?;
    if berth_core::keys::known::seed_from(&store, place, &known_hosts, &names)? {
        let record = berth_core::keys::known::Record::load(&store, place)?;
        for key in &record.host_keys {
            println!(
                "Recorded its host key from {}: {}",
                known_hosts.display(),
                berth_core::keys::ring::fingerprint(key)
            );
        }
    }
    Ok(())
}

/// Run a task of a host of this machine as a session there and follow it.
fn run_here(
    paths: &Paths,
    reach: &Reach,
    tasks: &[api::Task],
    action: &api::Action,
    act: api::Act,
) -> Result<i32> {
    let mut body = json!({"arguments": act.arguments});
    if let (Some(rows), Some(cols)) = (act.rows, act.cols) {
        (body["rows"], body["cols"]) = (json!(rows), json!(cols));
    }
    // Secrets are built here and travel on the task's standard input.
    let task = tasks.iter().find(|task| task.name == action.name);
    if task.and_then(|task| task.input.as_deref()) == Some("secrets") {
        let payload = registry::Store::new(paths)?.host_payload(&reach.host.config)?;
        body["input_b64"] = json!(api::b64(format!("{payload}\n").as_bytes()));
    }
    reach.wait(async {
        let path = format!("/v1/tasks/{}", encode(&action.name));
        let session: api::Session = reach.link.post(&path, &body).await?;
        let ended = follow_session(&reach.link, &session).await?;
        Ok::<i32, Box<dyn std::error::Error>>(finish(reach, &session.id, ended, true).await)
    })
}

async fn follow_session(link: &Link, session: &api::Session) -> Result<Ended> {
    match session.kind {
        api::SessionKind::Terminal => follow_terminal(link, &session.id).await,
        api::SessionKind::Process => follow_process(link, &session.id, 0, false).await,
    }
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
    let reach = controlled(paths, name)?;
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
    let reach = controlled(paths, name)?;
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
    let Some(server) = host.served(paths) else {
        let password = host.sudo_password(paths)?;
        let prepared = berth_core::apply::prepare(paths, &host, password.as_deref())?;
        let work = runner::Work::Program(Box::new(runner::Program {
            command: prepared.command,
            input: None,
            files: prepared.files,
        }));
        let request = runner::Request::new(runner::Subject::Host(name.to_owned()), "apply");
        let code = runner::here(paths, request, work, &mut std::io::stdout())?;
        if code == Some(0) {
            berth_core::apply::settle(paths, &host)?;
        }
        std::process::exit(code.unwrap_or(1));
    };
    // The server has no terminal to ask on: a password it needs is asked for here.
    let act = api::Act {
        sudo_password: host.sudo_password(paths)?,
        ..api::Act::default()
    };
    let acted = act_on(&server, Some(name), "apply", &act)?;
    let code = follow_acted(&server, None, acted, &mut std::io::stdout())?;
    std::process::exit(code);
}

/// Run an action of a berth server's: of the host `host`, or of the
/// installation without one.
pub(crate) fn act_on(
    server: &berth_core::server::Server,
    host: Option<&str>,
    action: &str,
    act: &api::Act,
) -> Result<api::Acted> {
    let (op, args) = match host {
        Some(name) => ("hosts.act", json!({"name": name, "action": action})),
        None => ("actions.act", json!({"action": action})),
    };
    let acted = server.call(op, args, Some(serde_json::to_value(act)?))?;
    Ok(serde_json::from_value(acted)?)
}

/// Follow the job of an action a berth server started, such as a key's rotation.
pub(crate) fn follow_server_job(
    server: &berth_core::server::Server,
    acted: api::Acted,
    out: &mut dyn Write,
) -> Result<i32> {
    follow_acted(server, None, acted, out)
}

/// Follow what running an action answered to its end: a session on the
/// host through `reach`, or the job, whose output goes to `out`. A result
/// is written out as it is. The exit code.
fn follow_acted(
    server: &berth_core::server::Server,
    reach: Option<&Reach>,
    acted: api::Acted,
    out: &mut dyn Write,
) -> Result<i32> {
    if let (Some(session), Some(reach)) = (&acted.session, reach) {
        return reach.wait(async {
            let ended = follow_session(&reach.link, session).await?;
            // The server's job copies the session's output; it is forgotten
            // on the host once the job has all of it.
            let copied = match (&ended, &acted.job) {
                (Ended::Exited(_), Some(job)) => {
                    let portal = server.portal().await?;
                    job_ended(&portal, &job.id).await
                }
                (Ended::Exited(_), None) => true,
                _ => false,
            };
            Ok::<i32, Box<dyn std::error::Error>>(finish(reach, &session.id, ended, copied).await)
        });
    }
    if let Some(job) = &acted.job {
        return link::runtime()?.block_on(async {
            let portal = server.portal().await?;
            eprintln!("Job {} on the berth server at {}", job.id, server.url);
            follow_job(&portal, &job.id, out).await
        });
    }
    if let Some(result) = &acted.result {
        writeln!(out, "{}", serde_json::to_string_pretty(result)?)?;
    }
    Ok(0)
}

/// Wait a little for a job to end: whether it did.
async fn job_ended(portal: &api::portal::Portal, id: &str) -> bool {
    for _ in 0..50 {
        let job = portal.call("jobs.get", None, json!({"id": id}), None).await;
        if job.is_ok_and(|job| !job["ended"].is_null()) {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    false
}

/// A job's output to `out` as it comes, until it ends, and its exit code.
pub(crate) async fn follow_job(
    portal: &api::portal::Portal,
    id: &str,
    out: &mut dyn Write,
) -> Result<i32> {
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
            true => host.sudo_password(paths).ok().flatten(),
            false => std::env::var("BERTH_SUDO_PASSWORD")
                .ok()
                .filter(|_| host.asks_password()),
        };
        // One roll-out at a time, as a job of a server holds the lock too.
        let _lock = runner::lock(paths, &runner::Subject::Installation, "rollout")?;
        return berth_core::rollout::local(paths, options, &password, out);
    };
    let act = api::Act {
        arguments: options.arguments(),
        ..api::Act::default()
    };
    let acted = act_on(&server, None, "rollout", &act)?;
    let id = acted.job.map(|job| job.id).unwrap_or_default();
    link::runtime()?.block_on(async {
        let portal = server.portal().await?;
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
    let server = host.served(paths).ok_or("Not a host of a berth server")?;
    let seed = act_on(&server, Some(name), "seed", &api::Act::default())?
        .result
        .unwrap_or_default();
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
    if let Some(kept) = berth_core::resource::of_host(paths, name)?
        && !kept.record.external_id.is_empty()
    {
        return Err(format!(
            "{name} has a machine berth created, {} of {}. Delete it first: berth resource {} remove",
            kept.record.external_id, kept.record.provisioner, kept.record.id
        )
        .into());
    }
    if provision::busy(paths, name) {
        return Err(format!(
            "A machine is being created for {name}. Remove it when that has ended"
        )
        .into());
    }
    if let Some(server) = host.served(paths) {
        server.call("hosts.remove", json!({"name": name}), None)?;
    }
    berth_core::profiles::remove(paths, &host.place)?;
    println!("Removed {}", host.place.describe());
    Ok(())
}

/// `berth profile HOST`: the profile as it is kept, as JSON.
pub fn show_profile(paths: &Paths, name: &str) -> Result<()> {
    let place = Place::resolve(paths, name)?;
    let profile = place
        .load(paths)?
        .ok_or_else(|| format!("No host {name}"))?;
    println!("{}", serde_json::to_string_pretty(&profile)?);
    Ok(())
}

/// `berth profile HOST --set FILE`: save a profile as the host's, checked as
/// the screen reads one. A host of a berth server is saved on the server, and
/// its copy here marks it as the server's, as a mirror does.
pub fn set_profile(paths: &Paths, name: &str, file: &str) -> Result<()> {
    let data = match file {
        "-" => {
            let mut data = vec![];
            std::io::Read::read_to_end(&mut std::io::stdin(), &mut data)?;
            data
        }
        file => std::fs::read(file).map_err(|error| format!("{file}: {error}"))?,
    };
    crate::app::parse_config(&data).map_err(|error| format!("Not a profile: {error}"))?;
    let mut profile: Value = serde_json::from_slice(&data)?;
    let place = Place::resolve(paths, name)?;
    // A profile of an older layout is kept in the one of now.
    berth_core::import::convert_profile(paths, &place.title(), &mut profile);
    if let Some(url) = place.server_url() {
        let server = berth_core::server::Server::configured(paths)
            .filter(|server| server.url == url)
            .ok_or("The berth server of this host is not set any more")?;
        server.save_host(&place.title(), &profile)?;
        profile["builder"]["served"] = json!(url);
    }
    place.save(paths, &profile)?;
    println!("Saved {}", place.describe());
    Ok(())
}

/// `berth credentials HOST [--show]`: the passwords berth generated for the
/// host's accounts, an account and its password to a line. They go to a
/// terminal, or elsewhere only when asked to with `--show`.
pub fn credentials(paths: &Paths, name: &str, show: bool) -> Result<()> {
    let shown = show || std::io::stdout().is_terminal();
    write_credentials(paths, name, shown, &mut std::io::stdout())
}

/// The passwords of [`credentials`] to `out`, when they may be `shown` there.
fn write_credentials(paths: &Paths, name: &str, shown: bool, out: &mut dyn Write) -> Result<()> {
    if !shown {
        return Err(
            "Standard output is not a terminal, and the passwords are shown only on one. Add --show to write them there anyway"
                .into(),
        );
    }
    let place = Place::resolve(paths, name)?;
    let kept = match place.server_url() {
        Some(url) => {
            let server = berth_core::server::Server::configured(paths)
                .filter(|server| server.url == url)
                .ok_or("The berth server of this host is not set any more")?;
            eprintln!("{name}'s passwords are kept on the berth server at {url}");
            let answer = server.call("hosts.credentials", json!({"name": name}), None)?;
            answer["credentials"]
                .as_object()
                .cloned()
                .unwrap_or_default()
        }
        None => {
            if place.load(paths)?.is_none() {
                return Err(format!("No host {name}").into());
            }
            berth_core::credentials::load(paths, &place)?
        }
    };
    if kept.is_empty() {
        eprintln!("berth generated no passwords for {name}");
    }
    for (user, password) in kept {
        writeln!(out, "{user} {}", password.as_str().unwrap_or(""))?;
    }
    Ok(())
}

// ── Adopting a host ─────────────────────────────────────────────────────────

/// What `berth adopt` is given after the name: `[USER@]ADDRESS`, and
/// `--port PORT`, `--key FILE` and `--force` in any order. Without an
/// address the host is a saved one, reached as its profile says.
#[derive(Debug, Default, PartialEq)]
pub struct Adopt {
    pub address: String,
    pub port: String,
    pub key: Option<String>,
    pub force: bool,
}

pub fn parse_adopt(args: &[&str]) -> Result<Adopt> {
    let mut adopt = Adopt::default();
    let mut words = args.iter();
    while let Some(word) = words.next() {
        match *word {
            "--force" => adopt.force = true,
            "--port" | "-p" => adopt.port = words.next().ok_or("--port needs a port")?.to_string(),
            "--key" | "-i" => {
                adopt.key = Some(words.next().ok_or("--key needs a file")?.to_string())
            }
            word if word.starts_with('-') => {
                return Err(format!("berth adopt has no option {word}").into());
            }
            word if adopt.address.is_empty() => adopt.address = word.to_owned(),
            word => return Err(format!("berth adopt takes one address, not also {word}").into()),
        }
    }
    if adopt.address.is_empty() && !adopt.port.is_empty() {
        return Err(
            "Usage: berth adopt NAME [[USER@]ADDRESS] [--port PORT] [--key FILE] [--force]".into(),
        );
    }
    Ok(adopt)
}

/// `berth adopt NAME [[USER@]ADDRESS]`: take a host over from the profile it
/// keeps, read over SSH with sudo so that it works while berthd is down.
/// Without an address it is the saved host NAME, as when berth host add
/// found that a host saved below a workstation is one.
pub fn adopt(paths: &Paths, name: &str, args: &[&str]) -> Result<()> {
    use berth_core::kept_profile as kept;
    let asked = parse_adopt(args)?;
    if !berth_core::profiles::valid_name(name) {
        return Err(
            format!("{name} cannot be a host's name: letters, digits, -, _ and . only").into(),
        );
    }
    let server = berth_core::server::Server::configured(paths);
    let place = Place::resolve(paths, name)?;
    // Refused before anything is read, so the host is not asked for nothing.
    let saved = place.load(paths)?;
    let reach = match (&saved, asked.address.as_str()) {
        (None, "") => {
            return Err(format!(
                "There is no host {name} to adopt by its own address. Say where it is: berth adopt {name} [USER@]ADDRESS"
            )
            .into());
        }
        // A host saved below a workstation says less than the host keeps;
        // adopting it by its own address is what replacing it means.
        (Some(profile), "") => {
            if level::of(profile) == level::Level::Workstation && !asked.force {
                return Err(format!(
                    "{name} is saved as a workstation already. Replace its profile with the one it keeps with --force"
                )
                .into());
            }
            let access = api::profile::Access::of(profile);
            if access.link.is_some() || access.address.is_empty() {
                return Err(format!(
                    "{name} has no SSH address to read its profile at: berth adopt {name} [USER@]ADDRESS"
                )
                .into());
            }
            let given = kept::Reach::parse(&access.address, &access.port, asked.key.as_deref())?;
            kept::Reach {
                key: given.key,
                ..kept::Reach::of(&access)
            }
        }
        (Some(_), _) if !asked.force => {
            return Err(format!(
                "There is a host {name} already. Choose another name, or replace it with --force"
            )
            .into());
        }
        (_, address) => kept::Reach::parse(address, &asked.port, asked.key.as_deref())?,
    };
    let target = reach.target();
    let password = || -> berth_core::Result<String> {
        if let Ok(password) = std::env::var("BERTH_SUDO_PASSWORD") {
            return Ok(password);
        }
        if !std::io::stdin().is_terminal() {
            return Err(format!(
                "sudo asks for a password on {target}: run on a terminal, or set BERTH_SUDO_PASSWORD"
            )
            .into());
        }
        registry::read_secret(&format!("Your sudo password on {target}"))
    };
    // The login is this machine's, also for a host the berth server keeps.
    let read = kept::read(paths, &place, &reach, &password)?;
    let access = api::profile::Access {
        address: reach.address.clone(),
        user: reach.user.clone(),
        port: reach.port.clone(),
        auth: read.auth.clone(),
        // The operator's sudo takes none; another account's may.
        sudo: read.sudo_asked.then_some(true),
        ..api::profile::Access::default()
    };
    let adopted = kept::adopted(paths, &place.title(), &read.kept, &access)?;
    let mut left = vec![];
    match &server {
        Some(server) => {
            server.save_host(name, &adopted.profile)?;
            server.mirror(paths)?;
            if let Some(record) = &adopted.machine {
                left.push(format!(
                    "the record of its machine, {} of {}: the berth server takes no record, so berth resource does not know the machine",
                    record.external_id, record.provisioner
                ));
            }
        }
        None => kept::save(paths, &place, &adopted)?,
    }
    let written = match (adopted.berth.as_str(), adopted.written.as_str()) {
        ("", _) => String::new(),
        (berth, "") => format!(" by berth {berth}"),
        (berth, when) => format!(" by berth {berth} at {when}"),
    };
    let was = match adopted.host.as_str() {
        "" => String::new(),
        old if old == name => String::new(),
        old => format!(", where it was {old}"),
    };
    println!("Adopted {name} from {target}: its profile was written{written}{was}.");
    let store = registry::Store::new(paths)?;
    let known = store.secret_names();
    for secret in adopted.secrets() {
        if !known.contains(&secret) {
            left.push(format!(
                "the value of secret {secret}: berth secret set {secret}"
            ));
        }
    }
    let registries = store.registries().unwrap_or_default();
    for registry in adopted.registries() {
        if !registries.contains_key(&registry) {
            left.push(format!(
                "registry {registry}, with its login: berth registry add {registry} …"
            ));
        }
    }
    if berth_api::profile::joins_automatically(&adopted.profile)
        && !known
            .iter()
            .any(|known| known == berth_api::profile::TAILNET_API_KEY)
    {
        left.push(format!(
            "secret {}, which a host that has left the tailnet needs to join again",
            berth_api::profile::TAILNET_API_KEY
        ));
    }
    if !left.is_empty() {
        println!("What did not come with it, and this berth has not got:");
        for line in &left {
            println!("  {line}");
        }
    }
    let way = match read.auth {
        api::profile::Auth::Berth => "with this berth's key".to_owned(),
        _ => "with your SSH configuration and agent, until an apply has given the operator this berth's key".to_owned(),
    };
    println!("berth logs in to it {way}.");
    println!(
        "Next: berth apply {name}. It gives this berth's own key a block of its own in the operator's authorized_keys."
    );
    println!(
        "The berth that set the host up still has access: its key keeps its BERTH block until you delete that block by hand."
    );
    if asked.key.is_some() && read.auth != api::profile::Auth::Berth {
        println!(
            "The key given with --key is not saved. If only it lets you in, give the host this berth's key with it first: berth key trust {name} --key FILE"
        );
    }
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
        None => Ok(provision::list(paths, &registry::Store::new(paths)?)),
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

/// `berth connect URL`: use the berth server at URL, with the token a setup
/// key collects. The key comes on standard input, so it is in no argv.
pub fn connect(paths: &Paths, url: &str) -> Result<()> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(format!(
            "{url} is no address of a berth server: https://HOST or http://HOST:PORT"
        )
        .into());
    }
    let key = registry::read_secret("Setup key")?;
    connect_with(paths, url, key.trim())
}

/// [`connect`] with the key in hand. Nothing is saved unless all of it works.
pub fn connect_with(paths: &Paths, url: &str, key: &str) -> Result<()> {
    use berth_core::server::{Server, http_as};
    let url = url.trim().trim_end_matches('/').to_owned();
    let key = key.to_owned();
    let at = format!("{url}/v1/setup-key");
    let ask = |method: &'static str| {
        let (at, key) = (at.clone(), key.clone());
        async move {
            let (status, body) = http_as(method, &at, Some(&key), None, vec![]).await?;
            match status {
                200 => Ok(serde_json::from_slice::<Value>(&body)?),
                401 => Err(
                    "The server did not take the setup key: it is wrong, used up or expired. Make a new one where the server runs: berth setup-key".into(),
                ),
                404 | 405 => Err("The server knows no setup keys: it is an older berth".into()),
                _ => Err(answer_error(status, &body)),
            }
        }
    };
    let (version, token) = link::runtime()?.block_on(async {
        let checked: api::SetupKey = serde_json::from_value(ask("GET").await?)?;
        let claimed: api::SetupClaim = serde_json::from_value(ask("POST").await?)?;
        // Saved only once the token is seen to open the server.
        Server {
            url: url.clone(),
            token: Some(claimed.token.clone()),
        }
        .portal()
        .await?
        .call("access", None, Value::Null, None)
        .await?;
        Ok::<_, Box<dyn std::error::Error>>((checked.version, claimed.token))
    })?;
    let before = paths.setting("server");
    paths.set("server", &url)?;
    if let Err(error) = paths.set("server_token", &token) {
        paths.set("server", before.as_deref().unwrap_or(""))?;
        return Err(error);
    }
    if let Err(error) = link::runtime()?.block_on(ask("DELETE")) {
        eprintln!(
            "The setup key could not be ended ({error}). It stops by itself within a day, or with berth setup-key --remove where the server runs."
        );
    }
    println!("This berth now uses the berth server at {url}, berth {version}");
    if std::env::var_os("BERTH_SERVER").is_some() {
        eprintln!("BERTH_SERVER is set in this shell and is used before the setting");
    }
    Ok(())
}

/// What an answer that is not a 200 says.
fn answer_error(status: u16, body: &[u8]) -> Box<dyn std::error::Error> {
    let said: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    match said["error"]["message"].as_str() {
        Some(message) => format!("HTTP {status}: {message}").into(),
        None => format!("HTTP {status}").into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adopt_takes_an_address_and_its_options_in_any_order() {
        let adopt = parse_adopt(&["--force", "ops@192.0.2.10", "--port", "2222"]).unwrap();
        assert_eq!(
            adopt,
            Adopt {
                address: "ops@192.0.2.10".into(),
                port: "2222".into(),
                key: None,
                force: true
            }
        );
        let keyed = parse_adopt(&["192.0.2.10", "--key", "~/.ssh/old"]).unwrap();
        assert_eq!(keyed.key.as_deref(), Some("~/.ssh/old"));
        // Without an address it is a saved host, reached as its profile says.
        assert_eq!(parse_adopt(&[]).unwrap(), Adopt::default());
        assert!(parse_adopt(&["--port", "2222"]).is_err());
        assert!(parse_adopt(&["192.0.2.10", "192.0.2.11"]).is_err());
        assert!(parse_adopt(&["192.0.2.10", "--port"]).is_err());
        assert!(parse_adopt(&["192.0.2.10", "--user"]).is_err());
    }

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
    fn a_machine_is_added_by_its_address_with_nothing_of_a_workstation() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: home.path().join("config"),
            data_dir: home.path().join("data"),
        };
        paths.prepare().unwrap();
        let add = |name: &str, args: &[&str]| add_host(&paths, name, "192.0.2.4", args);
        add("dock", &["--user", "ops", "--port", "2222", "--no-contact"]).unwrap();
        let saved = Place::resolve(&paths, "dock")
            .unwrap()
            .load(&paths)
            .unwrap()
            .unwrap();
        assert_eq!(
            saved,
            json!({"level": "reachable", "access": {"address": "192.0.2.4", "user": "ops", "port": "2222", "auth": "berth"}})
        );
        let said = |result: Result<()>| result.unwrap_err().to_string();
        assert!(said(add("dock", &["--no-contact"])).contains("already"));
        assert!(said(add("pier", &["--level", "server", "--no-contact"])).contains("--level"));
        assert!(said(add("pier", &["--auth", "key:none", "--no-contact"])).contains("no key none"));
        assert!(said(add("../pier", &["--no-contact"])).contains("name"));
        // A workstation starts with the part its set-up fills in.
        add("studio", &["--level", "workstation", "--no-contact"]).unwrap();
        let studio = Host::load(&paths, "studio").unwrap();
        assert!(studio.config["builder"].is_object());
        // Applying is refused below a workstation, before anything runs.
        let dock = Host::load(&paths, "dock").unwrap();
        let refused = berth_core::apply::prepare(&paths, &dock, None);
        assert!(said(refused.map(|_| ())).contains("not a workstation"));
        let refused = install_controller_here(&paths, &studio);
        assert!(said(refused.map(|_| ())).contains("berth apply studio"));
    }

    #[test]
    fn a_host_with_a_machine_berth_created_stays_until_it_is_deleted() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: home.path().join("config"),
            data_dir: home.path().join("data"),
        };
        paths.prepare().unwrap();
        let place = Place::own("studio").unwrap();
        place
            .save(&paths, &json!({"access": {"address": "192.0.2.20"}}))
            .unwrap();
        berth_core::credentials::save(&paths, &place, "ops", "made-up").unwrap();
        let mut machine = berth_core::resource::Kept {
            scope: berth_core::store::OWN.into(),
            record: berth_core::resource::Record {
                id: "r-studio".into(),
                provisioner: "cloud/vm".into(),
                kind: "machine".into(),
                name: "studio".into(),
                external_id: "vm-1".into(),
                host: "studio".into(),
                token: "t".into(),
                ..Default::default()
            },
        };
        machine.save(&paths).unwrap();
        let said = remove(&paths, "studio").unwrap_err().to_string();
        assert!(said.contains("berth resource r-studio remove"), "{said}");
        assert!(place.load(&paths).unwrap().is_some());
        // One whose creation never ended goes with the host, and its passwords with it.
        machine.record.external_id.clear();
        machine.save(&paths).unwrap();
        remove(&paths, "studio").unwrap();
        assert!(place.load(&paths).unwrap().is_none());
        assert!(
            berth_core::resource::of_host(&paths, "studio")
                .unwrap()
                .is_none()
        );
        assert!(
            berth_core::credentials::load(&paths, &place)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_profile_that_lists_its_features_keeps_them_without_the_marker() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: home.path().join("config"),
            data_dir: home.path().join("data"),
        };
        let file = home.path().join("studio.json");
        std::fs::write(
            &file,
            r#"{"hostname": "studio", "ansible_vars": {"devbox_plugin_features": ["codex/cli"]}}"#,
        )
        .unwrap();
        set_profile(&paths, "studio", file.to_str().unwrap()).unwrap();
        let place = Place::own("studio").unwrap();
        let kept = place.load(&paths).unwrap().unwrap();
        let config = crate::app::parse_config(kept.to_string().as_bytes()).unwrap();
        let mut app = crate::app::App::new(paths.clone(), place.clone(), config);
        app.save().unwrap();
        let saved = place.load(&paths).unwrap().unwrap();
        assert_eq!(
            saved["ansible_vars"]["devbox_plugin_features"],
            json!(["codex/cli"])
        );
        // One of the old layout is still converted.
        let old = crate::app::parse_config(br#"{"ansible_vars": {"devbox_agent_codex": false}}"#)
            .unwrap();
        assert!(
            !old["ansible_vars"]["devbox_plugin_features"]
                .as_array()
                .unwrap()
                .contains(&json!("codex/cli"))
        );
    }

    #[test]
    fn a_profile_goes_out_and_in_and_passwords_only_to_a_terminal() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: home.path().join("config"),
            data_dir: home.path().join("data"),
        };
        let file = home.path().join("studio.json");
        std::fs::write(
            &file,
            r#"{"hostname": "studio", "builder": {}, "access": {"address": "192.0.2.20"}}"#,
        )
        .unwrap();
        set_profile(&paths, "studio", file.to_str().unwrap()).unwrap();
        let place = Place::own("studio").unwrap();
        let kept = place.load(&paths).unwrap().unwrap();
        assert_eq!(kept["access"]["address"], "192.0.2.20");
        // What is not a profile is refused, and the one kept stays.
        std::fs::write(&file, r#"{"builder": []}"#).unwrap();
        assert!(set_profile(&paths, "studio", file.to_str().unwrap()).is_err());
        assert_eq!(place.load(&paths).unwrap().unwrap(), kept);
        assert!(show_profile(&paths, "attic").is_err());
        show_profile(&paths, "studio").unwrap();
        berth_core::credentials::save(&paths, &place, "ops", "made-up").unwrap();
        let mut out = vec![];
        let said = write_credentials(&paths, "studio", false, &mut out)
            .unwrap_err()
            .to_string();
        assert!(said.contains("--show") && out.is_empty(), "{said}");
        write_credentials(&paths, "studio", true, &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "ops made-up\n");
        assert!(write_credentials(&paths, "attic", true, &mut vec![]).is_err());
    }

    #[test]
    fn hosts_share_one_inventory() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: home.path().join("config"),
            data_dir: home.path().join("data"),
        };
        let inventory = |name: &str, access: Value| {
            let host = Host {
                name: name.into(),
                place: Place::own(name).unwrap(),
                config: json!({"access": access}),
            };
            host.inventory(&paths).unwrap()
        };
        let one = inventory("server", json!({"address": "192.0.2.1", "user": "ops"}));
        let two = inventory("laptop", json!({"address": "box", "port": "2222"}));
        let merged = merge_inventory(&[("server".into(), one), ("laptop".into(), two)]);
        assert_eq!(merged["all"]["hosts"]["server"]["ansible_user"], "ops");
        assert_eq!(merged["all"]["hosts"]["laptop"]["ansible_port"], 2222);
    }
}
