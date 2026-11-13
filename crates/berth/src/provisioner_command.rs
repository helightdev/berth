//! `berth provisioner …`: a provisioner as a subject of its own, its page and
//! its actions. With a berth server set the provisioners are the server's,
//! and their actions run there; otherwise here, with this berth's secrets.
//! The Provisioners tab of the terminal screen goes the same way.

use crate::Result;
use crate::commands::{follow_job, parse_arguments, ready};
use berth_api::{self as api, Act, Action, ProvisionerInfo, ProvisionerState};
use berth_core::{
    paths::Paths,
    provision, provisioner, registry,
    runner::{self, Request, Subject, Work},
    server::Server,
};
pub use berth_tui::provisioners::answer_lines;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::Write;

const USAGE: &str = "Usage: berth provisioner [PLUGIN/ID [NAME=VALUE…] | PLUGIN/ID actions | PLUGIN/ID run ACTION [NAME=VALUE…] [--yes]]";

/// Where the provisioners are: here, or on the berth server berth uses.
pub enum Source {
    Here(Paths),
    Server(Server),
}

/// What running an action came to.
pub enum Ran {
    /// It only read, and answered with a message and rows.
    Answered(Value),
    /// It ran as a job to its end, with this exit code.
    Ended(i32),
}

/// `PLUGIN/ID` as the two parts of a route.
fn parts(name: &str) -> Result<(&str, &str)> {
    name.split_once('/')
        .filter(|(plugin, id)| !plugin.is_empty() && !id.is_empty())
        .ok_or_else(|| format!("No provisioner {name}. See: berth provisioner").into())
}

impl Source {
    pub fn of(paths: &Paths) -> Source {
        match Server::configured(paths) {
            Some(server) => Source::Server(server),
            None => Source::Here(paths.clone()),
        }
    }

    /// Where they are, for people.
    pub fn place(&self) -> String {
        match self {
            Source::Here(_) => "this machine".into(),
            Source::Server(server) => format!("the berth server at {}", server.url),
        }
    }

    pub fn list(&self) -> Result<Vec<ProvisionerInfo>> {
        match self {
            Source::Here(paths) => Ok(provision::list(paths, &registry::Store::new(paths)?)),
            Source::Server(server) => Ok(serde_json::from_value(server.call(
                "provisioners.list",
                Value::Null,
                None,
            )?)?),
        }
    }

    /// Its page; `given` are values of its own arguments for this once.
    pub fn state(&self, name: &str, given: &BTreeMap<String, String>) -> Result<ProvisionerState> {
        match self {
            Source::Here(paths) => provisioner::state(
                paths,
                &registry::Store::new(paths)?,
                name,
                given,
                &Default::default(),
            ),
            Source::Server(server) => {
                let (plugin, id) = parts(name)?;
                let mut asked = json!(given);
                (asked["plugin"], asked["id"]) = (json!(plugin), json!(id));
                Ok(serde_json::from_value(server.call(
                    "provisioners.get",
                    asked,
                    None,
                )?)?)
            }
        }
    }

    pub fn actions(&self, name: &str) -> Result<Vec<Action>> {
        match self {
            Source::Here(paths) => provisioner::actions(paths, &registry::Store::new(paths)?, name),
            Source::Server(server) => {
                let (plugin, id) = parts(name)?;
                Ok(serde_json::from_value(server.call(
                    "provisioners.actions",
                    json!({"plugin": plugin, "id": id}),
                    None,
                )?)?)
            }
        }
    }

    /// Run an action of `name`. A job's output goes to `out` as it comes; a
    /// provisioner's progress for one that only reads goes to `progress`.
    pub fn run(
        &self,
        name: &str,
        action: &str,
        act: Act,
        out: &mut dyn Write,
        progress: &mut dyn Write,
    ) -> Result<Ran> {
        match self {
            Source::Here(paths) => {
                let store = registry::Store::new(paths)?;
                let acting = provisioner::prepare(paths, &store, name, action, &act.arguments)?;
                let options = provision::Options::default();
                if !acting.changes() {
                    let answered = acting.run(paths, &options, progress)?;
                    return Ok(Ran::Answered(answered.value()));
                }
                let mut request = Request::new(Subject::Provisioner(name.to_owned()), action);
                request.secrets = acting.secrets();
                let held = paths.clone();
                let work = Work::Function(Box::new(move |out| {
                    match acting.run(&held, &options, out) {
                        Ok(answered) => {
                            for line in answered.lines() {
                                let _ = writeln!(out, "{line}");
                            }
                            true
                        }
                        Err(error) => {
                            let _ = writeln!(out, "✗ {error}");
                            false
                        }
                    }
                }));
                let code = runner::here(paths, request, work, out)?;
                Ok(Ran::Ended(code.unwrap_or(1)))
            }
            Source::Server(server) => {
                let (plugin, id) = parts(name)?;
                let acted = server.call(
                    "provisioners.act",
                    json!({"plugin": plugin, "id": id, "action": action}),
                    Some(serde_json::to_value(&act)?),
                )?;
                let acted: api::Acted = serde_json::from_value(acted)?;
                let Some(job) = acted.job else {
                    return Ok(Ran::Answered(acted.result.unwrap_or(Value::Null)));
                };
                let _ = writeln!(
                    progress,
                    "Job {} on the berth server at {}",
                    job.id, server.url
                );
                let code = berth_core::link::runtime()?.block_on(async {
                    let portal = server.portal().await?;
                    follow_job(&portal, &job.id, out).await
                })?;
                Ok(Ran::Ended(code))
            }
        }
    }
}

/// The line a provisioner has in the list: its name, and why it cannot be
/// used here when it cannot.
pub fn summary(provisioner: &ProvisionerInfo) -> String {
    match provisioner.problem.is_empty() {
        true => format!("{}  {}", provisioner.id, provisioner.label),
        false => format!(
            "{}  {}  · not usable: {}",
            provisioner.id, provisioner.label, provisioner.problem
        ),
    }
}

/// What `berth provisioner NAME` says of it.
pub fn describe(state: &ProvisionerState, actions: &[Action]) -> String {
    let info = &state.provisioner;
    let mut text = format!("{}  {}\n", info.id, info.label);
    if !info.description.is_empty() {
        text += &format!("{}\n", info.description);
    }
    let said = match state.message.is_empty() {
        true => state.state.clone(),
        false => format!("{} · {}", state.state, state.message),
    };
    text += &format!("\nState     {said}\n");
    let width = state
        .details
        .iter()
        .map(|row| row.label.chars().count())
        .max()
        .unwrap_or(0)
        .max(8);
    for row in &state.details {
        text += &format!("{:<width$}  {}\n", row.label, row.value);
    }
    if !info.own.is_empty() {
        let set: Vec<String> = info
            .own
            .iter()
            .map(|name| match info.settings.get(name) {
                Some(value) => format!("{name}={value}"),
                None => format!("{name} not set"),
            })
            .collect();
        text += &format!("Settings  {}\n", set.join(" · "));
    }
    text += "\nMachines berth made with it\n";
    if state.machines.is_empty() {
        text += "  none\n";
    }
    for machine in &state.machines {
        let mut notes = vec![];
        if !machine.state.is_empty() {
            notes.push(machine.state.clone());
        }
        if !machine.recorded {
            notes.push("berth has no record of it".into());
        }
        let id = match machine.id.as_str() {
            "" => "(being made)",
            id => id,
        };
        text += &format!("  {:<16} {id}  {}\n", machine.host, notes.join(" · "));
    }
    let mut groups: Vec<(&str, Vec<&Action>)> = vec![];
    for action in actions {
        let title = berth_tui::page::group_of(action);
        match groups
            .iter_mut()
            .find(|(known, _)| known.eq_ignore_ascii_case(title))
        {
            Some((_, list)) => list.push(action),
            None => groups.push((title, vec![action])),
        }
    }
    if !groups.is_empty() {
        text += &format!("\nActions (berth provisioner {} run ACTION)\n", info.id);
    }
    for (title, list) in groups {
        text += &format!("  {title}\n");
        for action in list {
            let why = match action.available {
                true => String::new(),
                false => format!("  ({})", action.why),
            };
            text += &format!("    {:<18} {}{why}\n", action.name, action.title());
        }
    }
    text
}

/// `berth provisioner …`.
pub fn command(paths: &Paths, args: &[&str]) -> Result<()> {
    let source = Source::of(paths);
    match args {
        [] => {
            let listed = source.list()?;
            if listed.is_empty() {
                println!("No plugin brings a provisioner. See: berth plugins");
            }
            for provisioner in &listed {
                println!("{}", summary(provisioner));
            }
            Ok(())
        }
        [name, "actions"] => {
            for action in source.actions(name)? {
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
                    "{:<18} {}  {}{why}",
                    action.name,
                    action.title(),
                    arguments.join(" ")
                );
            }
            Ok(())
        }
        [name, "run", action, rest @ ..] => {
            let (values, yes) = parse_arguments(rest)?;
            let actions = source.actions(name)?;
            if !actions.iter().any(|known| known.name == *action) {
                let names: Vec<&str> = actions.iter().map(|known| known.name.as_str()).collect();
                return Err(format!(
                    "{name} has no action {action}. Its actions: {}",
                    names.join(", ")
                )
                .into());
            }
            let (found, values) = ready(name, &actions, action, values, yes)?;
            let act = Act {
                arguments: values,
                ..Act::default()
            };
            let ran = source.run(
                name,
                &found.name,
                act,
                &mut std::io::stdout(),
                &mut std::io::stderr(),
            )?;
            match ran {
                Ran::Answered(answer) => {
                    for line in answer_lines(&answer) {
                        println!("{line}");
                    }
                    Ok(())
                }
                Ran::Ended(code) => std::process::exit(code),
            }
        }
        [name, rest @ ..] if rest.iter().all(|arg| arg.contains('=')) => {
            let (given, _) = parse_arguments(rest)?;
            let state = source.state(name, &given)?;
            let actions = source.actions(name).unwrap_or_default();
            print!("{}", describe(&state, &actions));
            Ok(())
        }
        _ => Err(USAGE.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use berth_api::{Detail, MadeMachine};

    #[test]
    fn a_provisioners_page_says_how_it_is_what_it_made_and_what_it_can_do() {
        let state = ProvisionerState {
            provisioner: ProvisionerInfo {
                id: "incus/vm".into(),
                label: "Incus virtual machine".into(),
                own: vec!["server".into(), "project".into()],
                settings: [("server".to_owned(), "https://192.0.2.10:8443".to_owned())].into(),
                ..Default::default()
            },
            state: "ready".into(),
            message: "Incus 6.18 answers".into(),
            details: vec![Detail {
                label: "Kernel".into(),
                value: "Linux 6.17".into(),
            }],
            machines: vec![
                MadeMachine {
                    host: "studio".into(),
                    id: "berth-studio".into(),
                    state: "running".into(),
                    recorded: true,
                },
                MadeMachine {
                    host: "attic".into(),
                    id: "berth-attic".into(),
                    state: String::new(),
                    recorded: false,
                },
            ],
            ..Default::default()
        };
        let actions = [
            Action {
                name: "pools".into(),
                label: "Storage pools".into(),
                group: "Storage".into(),
                ..Action::default()
            },
            Action {
                name: "settings".into(),
                label: "Settings".into(),
                group: "Set-Up".into(),
                available: false,
                why: "no".into(),
                ..Action::default()
            },
        ];
        let text = describe(&state, &actions);
        for line in [
            "State     ready · Incus 6.18 answers\n",
            "Kernel    Linux 6.17\n",
            "Settings  server=https://192.0.2.10:8443 · project not set\n",
            "  studio           berth-studio  running\n",
            "  attic            berth-attic  berth has no record of it\n",
            "Actions (berth provisioner incus/vm run ACTION)\n",
            "    pools              Storage pools\n",
            "    settings           Settings  (no)\n",
        ] {
            assert!(text.contains(line), "{line:?} in\n{text}");
        }
        let lines = answer_lines(&json!({"message": "2 pools", "details": [
            {"label": "default", "value": "zfs"}, {"label": "fast-ssd", "value": "dir"}]}));
        assert_eq!(lines, ["2 pools", "  default   zfs", "  fast-ssd  dir"]);
        assert!(parts("incus").is_err() && parts("incus/").is_err());
        let refused = ProvisionerInfo {
            id: "lab/old".into(),
            label: "Old".into(),
            problem: "lab/old speaks version 1".into(),
            ..Default::default()
        };
        assert_eq!(
            summary(&refused),
            "lab/old  Old  · not usable: lab/old speaks version 1"
        );
    }
}
