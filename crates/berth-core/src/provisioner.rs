//! A provisioner as a subject of its own: the settings that say where it is,
//! what it says of itself, the machines berth made with it, and its actions.
//! Its program runs where berth runs, as for creating a machine (see
//! [`crate::provision`]): `status` without an id answers for the provisioner
//! itself, and `act` runs one of its actions.

use crate::{
    Result,
    paths::Paths,
    plugins::Provisioner,
    provision::{
        self, Options, Record, call, find, found, needs_secrets, request, runnable, secrets,
        values_of,
    },
    registry::Store,
    store::{self, OWN, kind},
};
use berth_api::{
    Action, Argument, Detail, MadeMachine, ProvisionerInfo, ProvisionerState, ops::Role,
};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    io::{BufRead, Read, Write},
    os::unix::process::CommandExt,
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// berth's own action that keeps the values of a provisioner's own arguments.
pub const SETTINGS: &str = "settings";

// ── Settings ────────────────────────────────────────────────────────────────

/// The values of a provisioner's own arguments that berth keeps, by name.
pub fn settings(paths: &Paths, name: &str) -> Result<BTreeMap<String, String>> {
    let kept = store::shared(paths)?.read_json(kind::PROVISIONER, OWN, name)?;
    Ok(kept
        .and_then(|kept| serde_json::from_value(kept).ok())
        .unwrap_or_default())
}

/// Keep the values of a provisioner's own arguments; an empty one is forgotten.
pub fn save_settings(paths: &Paths, name: &str, values: &BTreeMap<String, String>) -> Result<()> {
    let kept: BTreeMap<&String, &String> = values
        .iter()
        .filter(|(_, value)| !value.trim().is_empty())
        .collect();
    let store = store::shared(paths)?;
    match kept.is_empty() {
        true => {
            store.forget(kind::PROVISIONER, OWN, name)?;
        }
        false => store.write_json(kind::PROVISIONER, OWN, name, &json!(kept))?,
    }
    Ok(())
}

/// The provisioner's own arguments, as declared.
fn own_arguments(provisioner: &Provisioner) -> Vec<Argument> {
    provisioner
        .arguments
        .iter()
        .filter(|argument| provisioner.own.contains(&argument.name))
        .cloned()
        .collect()
}

// ── What it is ──────────────────────────────────────────────────────────────

/// What is said of a provisioner of the plugin `plugin`: with its settings,
/// the required secrets that have no value, and why it cannot be used here.
pub fn describe(
    paths: &Paths,
    store: &Store,
    plugin: &str,
    provisioner: &Provisioner,
) -> ProvisionerInfo {
    let missing = secrets(store, provisioner).1;
    let mut info = provisioner.info(plugin, missing.clone());
    info.settings = settings(paths, &info.id).unwrap_or_default();
    if info.problem.is_empty() {
        info.problem = problem(&info.id, provisioner, &missing, &info.settings);
    }
    info
}

/// Why a provisioner of protocol 2 cannot be asked anything here: a secret
/// without a value, or one of its own arguments it needs and has no value of.
fn problem(
    name: &str,
    provisioner: &Provisioner,
    missing: &[String],
    settings: &BTreeMap<String, String>,
) -> String {
    if let Err(error) = needs_secrets(name, missing) {
        return error.to_string();
    }
    let own = own_arguments(provisioner);
    let unset: Vec<&str> = own
        .iter()
        .filter(|argument| argument.required && argument.default.is_empty())
        .filter(|argument| settings.get(&argument.name).is_none_or(String::is_empty))
        .map(|argument| argument.name.as_str())
        .collect();
    match unset.is_empty() {
        true => String::new(),
        false => format!(
            "{name} needs {}: berth provisioner {name} run {SETTINGS} {}",
            unset.join(" and "),
            unset
                .iter()
                .map(|name| format!("{name}=VALUE"))
                .collect::<Vec<_>>()
                .join(" ")
        ),
    }
}

/// A provisioner by `PLUGIN/ID`, with its plugin's folder.
fn load(paths: &Paths, name: &str) -> Result<(PathBuf, String, Provisioner)> {
    let plugins = found(paths);
    let (plugin, provisioner) = find(&plugins, name)?;
    Ok((plugin.dir.clone(), plugin.id.clone(), provisioner.clone()))
}

/// What is said of one provisioner, as in the list.
pub fn info(paths: &Paths, store: &Store, name: &str) -> Result<ProvisionerInfo> {
    let (_, plugin, provisioner) = load(paths, name)?;
    Ok(describe(paths, store, &plugin, &provisioner))
}

/// The values of the provisioner's own arguments for a call: those given,
/// then its settings, then their defaults.
fn own_values(
    name: &str,
    provisioner: &Provisioner,
    given: &BTreeMap<String, String>,
    kept: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    values_of(name, &own_arguments(provisioner), given, kept)
}

/// The rows of `details` in an answer, as they are given.
fn details(answer: &Value) -> Vec<Detail> {
    answer["details"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let label = row["label"].as_str()?.to_owned();
            let value = match &row["value"] {
                Value::String(text) => text.clone(),
                Value::Null => String::new(),
                other => other.to_string(),
            };
            Some(Detail { label, value })
        })
        .collect()
}

/// The page of a provisioner: what it says of itself now, and the machines
/// berth made with it. `given` are values of its own arguments for this
/// once, such as another server to ask.
pub fn state(
    paths: &Paths,
    store: &Store,
    name: &str,
    given: &BTreeMap<String, String>,
    options: &Options,
) -> Result<ProvisionerState> {
    let (dir, plugin, provisioner) = load(paths, name)?;
    let info = describe(paths, store, &plugin, &provisioner);
    let mut machines = made(paths, name);
    let mut state = ProvisionerState {
        provisioner: info.clone(),
        state: "unusable".into(),
        ..ProvisionerState::default()
    };
    // Values given for this once may be what its settings lack.
    let own = runnable(&provisioner)
        .and_then(|()| needs_secrets(name, &info.missing_secrets))
        .and_then(|()| own_values(name, &provisioner, given, &info.settings));
    let own = match own {
        Ok(own) => own,
        Err(error) => {
            state.message = error.to_string();
            state.machines = machines;
            return Ok(state);
        }
    };
    let (secrets, _) = secrets(store, &provisioner);
    let asked = request("status", &provisioner, None, &own, &secrets);
    match call(
        &dir,
        &provisioner,
        "status",
        &asked,
        options.quick,
        &mut std::io::sink(),
    ) {
        Ok(answer) => {
            state.state = answer["state"]
                .as_str()
                .filter(|said| !said.is_empty())
                .unwrap_or("ready")
                .to_owned();
            state.message = answer["message"].as_str().unwrap_or("").to_owned();
            state.details = details(&answer);
            if let Some(values) = answer["values"].as_object() {
                for (argument, offered) in values {
                    let offered: Vec<String> = offered
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|value| value.as_str().map(str::to_owned))
                        .collect();
                    state.values.insert(argument.clone(), offered);
                }
            }
        }
        Err(error) => {
            state.state = "unreachable".into();
            state.message = error.to_string();
        }
    }
    if state.state != "unreachable" && provisioner.verbs.iter().any(|verb| verb == "list") {
        let mut asked = request("list", &provisioner, None, &own, &secrets);
        asked["kind"] = json!(crate::plugins::MACHINE);
        if let Ok(answer) = call(
            &dir,
            &provisioner,
            "list",
            &asked,
            options.quick,
            &mut std::io::sink(),
        ) {
            merge(&mut machines, provision::listed_machines(&answer));
        }
    }
    state.machines = machines;
    Ok(state)
}

/// The machines berth has records of that `name` made, by host.
fn made(paths: &Paths, name: &str) -> Vec<MadeMachine> {
    let Ok(store) = store::shared(paths) else {
        return vec![];
    };
    let mut machines: Vec<MadeMachine> = store
        .names(kind::INSTANCE, OWN)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|host| {
            let kept = store.read_json(kind::INSTANCE, OWN, &host).ok()??;
            let record: Record = serde_json::from_value(kept).ok()?;
            (record.provisioner == name).then(|| MadeMachine {
                host,
                id: record.id,
                state: String::new(),
                recorded: true,
            })
        })
        .collect();
    machines.sort_by(|a, b| a.host.cmp(&b.host));
    machines
}

/// Take in what the provisioner lists: the state of each machine berth has
/// a record of, and those it lists that berth has none of.
fn merge(machines: &mut Vec<MadeMachine>, listed: Vec<berth_api::ListedInstance>) {
    for entry in listed {
        match machines.iter_mut().find(|known| known.id == entry.id) {
            Some(known) => known.state = entry.state,
            None => machines.push(MadeMachine {
                host: entry.host,
                id: entry.id,
                state: entry.state,
                recorded: false,
            }),
        }
    }
}

// ── Its actions ─────────────────────────────────────────────────────────────

/// berth's own action of a provisioner with arguments of its own: keeping
/// their values, which every call to it then uses.
fn settings_action(provisioner: &Provisioner, kept: &BTreeMap<String, String>) -> Action {
    Action {
        name: SETTINGS.into(),
        label: "Settings".into(),
        help: "Where the provisioner is, such as the server or engine to use. berth keeps these values in its store and uses them for every call to it, and for each machine made with it unless it is given others. An empty value is forgotten.".into(),
        group: "Set-Up".into(),
        arguments: own_arguments(provisioner)
            .into_iter()
            .map(|argument| Argument {
                default: kept
                    .get(&argument.name)
                    .cloned()
                    .unwrap_or(argument.default.clone()),
                // A value may be cleared, so it is never needed here.
                required: false,
                ..argument
            })
            .collect(),
        role: Role::Admin,
        ..Action::default()
    }
}

/// What can be done to a provisioner: its plugin's actions, then berth's
/// own, each saying whether it can run here now. An action of one whose
/// settings lack a value it needs asks for that value first.
pub fn actions(paths: &Paths, store: &Store, name: &str) -> Result<Vec<Action>> {
    let (_, _, provisioner) = load(paths, name)?;
    if provisioner.refused.is_some() {
        return Ok(vec![]);
    }
    let kept = settings(paths, name)?;
    let why = needs_secrets(name, &secrets(store, &provisioner).1).err();
    let unset: Vec<Argument> = own_arguments(&provisioner)
        .into_iter()
        .filter(|argument| argument.required && argument.default.is_empty())
        .filter(|argument| kept.get(&argument.name).is_none_or(String::is_empty))
        .collect();
    let mut actions: Vec<Action> = provisioner
        .actions
        .iter()
        .cloned()
        .map(|mut action| {
            let mut arguments = unset.clone();
            arguments.append(&mut action.arguments);
            action.arguments = arguments;
            if let Some(why) = &why {
                action.available = false;
                action.why = why.to_string();
            }
            action
        })
        .collect();
    if !provisioner.own.is_empty() {
        actions.push(settings_action(&provisioner, &kept));
    }
    Ok(actions)
}

/// An action of a provisioner that has been checked, ready to run.
pub struct Acting {
    name: String,
    action: Action,
    work: Doing,
    hidden: Vec<String>,
}

enum Doing {
    Settings(BTreeMap<String, String>),
    Program {
        dir: PathBuf,
        provisioner: Box<Provisioner>,
        request: Value,
    },
}

/// What an action said: a sentence and rows, shown as they are.
#[derive(Debug, Default, PartialEq)]
pub struct Answered {
    pub message: String,
    pub details: Vec<Detail>,
}

impl Answered {
    /// What an action that answers at once gives its caller.
    pub fn value(&self) -> Value {
        json!({"message": self.message, "details": self.details})
    }

    /// The answer as lines of output, for a job.
    pub fn lines(&self) -> Vec<String> {
        let mut lines = vec![];
        if !self.message.is_empty() {
            lines.push(format!("✓ {}", self.message));
        }
        let width = self
            .details
            .iter()
            .map(|row| row.label.chars().count())
            .max()
            .unwrap_or(0);
        for row in &self.details {
            lines.push(format!("  {:<width$}  {}", row.label, row.value));
        }
        lines
    }
}

/// Check that the action `action` of `name` can run with `given`, and make
/// its request. A value is either one of the action's arguments or one of
/// the provisioner's own, for this once; any other name is refused.
pub fn prepare(
    paths: &Paths,
    store: &Store,
    name: &str,
    action: &str,
    given: &BTreeMap<String, String>,
) -> Result<Acting> {
    let (dir, _, provisioner) = load(paths, name)?;
    runnable(&provisioner)?;
    let listed = actions(paths, store, name)?;
    let Some(found) = listed.iter().find(|known| known.name == action) else {
        let names: Vec<&str> = listed.iter().map(|known| known.name.as_str()).collect();
        return Err(format!(
            "{name} has no action {action}. Its actions: {}",
            match names.is_empty() {
                true => "none".to_owned(),
                false => names.join(", "),
            }
        )
        .into());
    };
    let found = found.clone();
    let hidden_of = |values: &BTreeMap<String, String>, declared: &[Argument]| -> Vec<String> {
        declared
            .iter()
            .filter(|argument| argument.secret)
            .filter_map(|argument| values.get(&argument.name).cloned())
            .collect()
    };
    let kept = settings(paths, name)?;
    if action == SETTINGS {
        let values = values_of(name, &found.arguments, given, &BTreeMap::new())?;
        return Ok(Acting {
            name: name.to_owned(),
            hidden: hidden_of(&values, &found.arguments),
            action: found,
            work: Doing::Settings(values),
        });
    }
    if !found.available {
        return Err(found.why.clone().into());
    }
    let (mut theirs, mut own) = (BTreeMap::new(), BTreeMap::new());
    // The provisioner's own come first: an action asks for one its settings lack.
    let declared: Vec<Argument> = found
        .arguments
        .iter()
        .filter(|argument| !provisioner.own.contains(&argument.name))
        .cloned()
        .collect();
    for (key, value) in given {
        if provisioner.own.contains(key) {
            own.insert(key.clone(), value.clone());
        } else if declared.iter().any(|known| &known.name == key) {
            theirs.insert(key.clone(), value.clone());
        } else {
            let mut known: Vec<&str> = declared
                .iter()
                .map(|argument| argument.name.as_str())
                .collect();
            known.extend(provisioner.own.iter().map(String::as_str));
            return Err(format!(
                "{action} of {name} has no argument {key}. It takes: {}",
                match known.is_empty() {
                    true => "none".to_owned(),
                    false => known.join(", "),
                }
            )
            .into());
        }
    }
    let theirs = values_of(
        &format!("{action} of {name}"),
        &declared,
        &theirs,
        &BTreeMap::new(),
    )?;
    let own = own_values(name, &provisioner, &own, &kept)?;
    let (secrets, missing) = secrets(store, &provisioner);
    needs_secrets(name, &missing)?;
    let mut request = request("act", &provisioner, None, &own, &secrets);
    request["action"] = json!({"name": action, "arguments": theirs});
    let mut hidden: Vec<String> = provision::hidden(&secrets);
    hidden.extend(hidden_of(&theirs, &declared));
    hidden.extend(hidden_of(&own, &own_arguments(&provisioner)));
    Ok(Acting {
        name: name.to_owned(),
        action: found,
        hidden,
        work: Doing::Program {
            dir,
            provisioner: Box::new(provisioner),
            request,
        },
    })
}

impl Acting {
    pub fn action(&self) -> &Action {
        &self.action
    }

    /// Whether it changes something: then it runs as a job, holding the
    /// provisioner's lock.
    pub fn changes(&self) -> bool {
        self.action.changes
    }

    /// What never reaches its output: the provisioner's secrets and the
    /// values of secret arguments.
    pub fn secrets(&self) -> Vec<String> {
        self.hidden.clone()
    }

    /// Run it, with its progress to `out`, and what it answered.
    pub fn run(self, paths: &Paths, options: &Options, out: &mut dyn Write) -> Result<Answered> {
        match self.work {
            Doing::Settings(values) => {
                save_settings(paths, &self.name, &values)?;
                let shown: Vec<String> = values
                    .iter()
                    .filter(|(_, value)| !value.trim().is_empty())
                    .map(|(key, value)| format!("{key}={value}"))
                    .collect();
                Ok(Answered {
                    message: match shown.is_empty() {
                        true => format!("{} keeps no settings now", self.name),
                        false => format!("{} uses {}", self.name, shown.join(" ")),
                    },
                    details: vec![],
                })
            }
            Doing::Program {
                dir,
                provisioner,
                request,
            } => {
                let limit = match self.action.changes {
                    true => options.create,
                    false => options.quick,
                };
                let answer = call(&dir, &provisioner, "act", &request, limit, out)?;
                Ok(Answered {
                    message: answer["message"].as_str().unwrap_or("").to_owned(),
                    details: details(&answer),
                })
            }
        }
    }
}

// ── A machine's log ─────────────────────────────────────────────────────────

/// What a host's machine wrote, as its provisioner's `logs` verb gives it:
/// the last `tail` lines, and with `follow` what comes after until it ends
/// or `limit` has passed. Its lines go to `out` with secrets masked.
pub fn logs(
    paths: &Paths,
    store: &Store,
    host: &str,
    tail: u32,
    follow: bool,
    limit: Duration,
    out: &mut dyn Write,
) -> Result<()> {
    let record = Record::load(paths, host)?
        .ok_or_else(|| format!("{host} has no machine that berth made"))?;
    if record.id.is_empty() {
        return Err(format!("{host}'s machine was not made yet").into());
    }
    let (dir, _, provisioner) = load(paths, &record.provisioner)?;
    runnable(&provisioner)?;
    if !provisioner
        .machine()
        .is_some_and(|kind| kind.verbs.iter().any(|verb| verb == "logs"))
    {
        return Err(format!("{} gives no logs of its machines", record.provisioner).into());
    }
    let (secrets, missing) = secrets(store, &provisioner);
    needs_secrets(&record.provisioner, &missing)?;
    let mut arguments = record.arguments.clone();
    for argument in &provisioner.arguments {
        arguments
            .entry(argument.name.clone())
            .or_insert_with(|| argument.default.clone());
    }
    let mut asked = request(
        "logs",
        &provisioner,
        Some((host, &record.token)),
        &arguments,
        &secrets,
    );
    (asked["id"], asked["tail"], asked["follow"]) = (json!(record.id), json!(tail), json!(follow));
    stream(&dir, &provisioner, &asked, limit, out)
}

/// Run `logs`: what it writes on standard output and error goes to `out`,
/// masked, until it ends; one that outlasts `limit` is stopped.
fn stream(
    dir: &std::path::Path,
    provisioner: &Provisioner,
    asked: &Value,
    limit: Duration,
    out: &mut dyn Write,
) -> Result<()> {
    let hidden = provision::hidden(asked["secrets"].as_object().unwrap_or(&Map::new()));
    let mut child = Command::new(&provisioner.command)
        .arg("logs")
        .current_dir(dir)
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("{}: {error}", provisioner.command.display()))?;
    let group = child.id() as i32;
    let mut input = serde_json::to_vec(asked)?;
    input.push(b'\n');
    let mut stdin = child.stdin.take().ok_or("no standard input")?;
    std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let (sender, lines) = std::sync::mpsc::channel::<String>();
    for reader in [
        child
            .stdout
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
        child
            .stderr
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let sender = sender.clone();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(reader).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
    }
    drop(sender);
    let deadline = Instant::now() + limit;
    let stop = |child: &mut std::process::Child| {
        // SAFETY: a signal to the process group it was started in, by number.
        unsafe { libc::kill(-group, libc::SIGKILL) };
        let _ = child.wait();
    };
    loop {
        match lines.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                let line = provision::hide(line, &hidden);
                if writeln!(out, "{line}").and_then(|_| out.flush()).is_err() {
                    // Nobody reads any more, as when the screen it went to closed.
                    stop(&mut child);
                    return Ok(());
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) if Instant::now() >= deadline => {
                stop(&mut child);
                return Ok(());
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
    let status = child.wait()?;
    // SAFETY: as above; nothing it started outlives it.
    unsafe { libc::kill(-group, libc::SIGKILL) };
    match status.success() {
        true => Ok(()),
        false => Err(format!(
            "{} logs ended with {}",
            provisioner.id,
            status.code().unwrap_or(-1)
        )
        .into()),
    }
}
