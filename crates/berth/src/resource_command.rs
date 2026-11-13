//! `berth resource …`, `berth deploy …`, and the short forms for a host's
//! machine, `berth provision` and `berth instance`. With a berth server set
//! the resources are the server's, and what changes them runs there as jobs;
//! otherwise here, with this berth's secrets.

use crate::Result;
use crate::commands::{parse_arguments, ready};
use crate::provisioner_command::{Change, Ran, Source, answer_lines};
use berth_api::{Action, NewResource, Resource, ResourceSecret};
use berth_core::{paths::Paths, resource};
use std::io::{IsTerminal, Read, Write};

const USAGE: &str = "Usage: berth resource [--provisioner PLUGIN/ID] [--kind KIND] [--host HOST]
       berth resource ID [actions | run ACTION [NAME=VALUE…] [--yes] | logs [--tail N] [--follow]
                          | change [--spec FILE|-] [--secret NAME[:VARIABLE]]… [NAME=VALUE…] | remove [--yes]]";

const DEPLOY: &str = "Usage: berth deploy NAME --provisioner PLUGIN/ID --spec FILE|- [--secret NAME[:VARIABLE]]… [--token TEXT] [NAME=VALUE…]";

fn ask(question: &str) -> Result<String> {
    print!("{question} ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(answer.trim().to_owned())
}

/// A berth secret given as `NAME` or `NAME:VARIABLE`.
fn secret(given: &str) -> ResourceSecret {
    match given.split_once(':') {
        Some((name, variable)) => ResourceSecret {
            name: name.to_owned(),
            env: variable.to_owned(),
        },
        None => ResourceSecret {
            name: given.to_owned(),
            env: String::new(),
        },
    }
}

/// A specification from a file, or from standard input for `-`.
fn read_spec(from: &str) -> Result<String> {
    let mut text = String::new();
    match from {
        "-" => {
            std::io::stdin().read_to_string(&mut text)?;
        }
        path => {
            text = std::fs::read_to_string(path).map_err(|error| format!("{path}: {error}"))?;
        }
    }
    Ok(text)
}

/// What the line of a resource in a list says.
pub fn line(found: &Resource) -> String {
    let name = match &found.host {
        Some(host) if found.kind == "machine" => format!("{} (host {host})", found.name),
        _ => found.name.clone(),
    };
    format!(
        "{:<12} {:<11} {:<26} {:<20} {}",
        found.id, found.kind, name, found.provisioner, found.state
    )
}

/// What `berth resource ID` says of it.
pub fn describe(found: &Resource, actions: &[Action]) -> String {
    let mut text = format!(
        "{}  {} {} of {}\n",
        found.id, found.kind, found.name, found.provisioner
    );
    let mut row = |label: &str, value: &str| {
        if !value.is_empty() {
            text += &format!("{label:<11}{value}\n");
        }
    };
    row("State", &found.state);
    row("Problem", &found.problem);
    row("Made as", &found.external_id);
    row("Host", found.host.as_deref().unwrap_or(""));
    row("Address", &found.address);
    row("Created", &found.created);
    row("Asked", &found.checked);
    row("Spec", &found.spec_sha256);
    let secrets: Vec<String> = found
        .secrets
        .iter()
        .map(|secret| format!("{} as {}", secret.name, secret.variable()))
        .collect();
    row("Secrets", &secrets.join(", "));
    let arguments: Vec<String> = found
        .arguments
        .iter()
        .filter(|(_, value)| !value.is_empty())
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    row("Arguments", &arguments.join(" "));
    if !actions.is_empty() {
        text += &format!("\nActions (berth resource {} run ACTION)\n", found.id);
    }
    for action in actions {
        let why = match action.available {
            true => String::new(),
            false => format!("  ({})", action.why),
        };
        text += &format!("  {:<18} {}{why}\n", action.name, action.title());
    }
    if found.verbs.iter().any(|verb| verb == "logs") {
        text += &format!("\nIts log: berth resource {} logs [--follow]\n", found.id);
    }
    text
}

/// Leave with how a run of an action ended, after saying what it answered.
fn finish(ran: Ran) -> Result<()> {
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

/// The options of `change` and `deploy`: the spec, the secrets, a token and
/// the arguments.
#[derive(Default)]
struct Given {
    provisioner: Option<String>,
    spec: Option<String>,
    secrets: Option<Vec<ResourceSecret>>,
    token: Option<String>,
    values: std::collections::BTreeMap<String, String>,
    yes: bool,
}

fn options(args: &[&str], usage: &str) -> Result<Given> {
    let mut given = Given::default();
    let mut plain = vec![];
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let mut value = || rest.next().copied().ok_or(usage);
        match *arg {
            "--provisioner" => given.provisioner = Some(value()?.to_owned()),
            "--spec" => given.spec = Some(read_spec(value()?)?),
            "--secret" => given
                .secrets
                .get_or_insert_with(Vec::new)
                .push(secret(value()?)),
            "--token" => given.token = Some(value()?.to_owned()),
            other => plain.push(other),
        }
    }
    (given.values, given.yes) = parse_arguments(&plain)?;
    Ok(given)
}

/// `berth resource …`.
pub fn command(paths: &Paths, args: &[&str]) -> Result<()> {
    let source = Source::of(paths);
    let filtered = |args: &[&str]| -> Result<()> {
        let (mut provisioner, mut kind, mut host) = (None, None, None);
        let mut rest = args.iter();
        while let Some(arg) = rest.next() {
            let value = rest.next().copied().ok_or(USAGE)?;
            match *arg {
                "--provisioner" => provisioner = Some(value),
                "--kind" => kind = Some(value),
                "--host" => host = Some(value),
                _ => return Err(USAGE.into()),
            }
        }
        let listed = source.resources(provisioner, kind, host)?;
        if listed.is_empty() {
            println!("berth made nothing with a provisioner yet. See: berth provisioner");
        }
        for found in &listed {
            println!("{}", line(found));
        }
        Ok(())
    };
    match args {
        [] => filtered(&[]),
        [first, ..] if first.starts_with("--") => filtered(args),
        [given] => {
            let id = source.resolve(given)?;
            let found = source.resource(&id)?;
            let actions = source.resource_actions(&id).unwrap_or_default();
            print!("{}", describe(&found, &actions));
            Ok(())
        }
        [given, "actions"] => {
            let id = source.resolve(given)?;
            for action in source.resource_actions(&id)? {
                let arguments: Vec<String> = action
                    .arguments
                    .iter()
                    .map(|argument| format!("[{}=…]", argument.name))
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
        [given, "logs", rest @ ..] => {
            let id = source.resolve(given)?;
            let (mut tail, mut follow) = (100u32, false);
            let mut rest = rest.iter();
            while let Some(arg) = rest.next() {
                match *arg {
                    "--follow" | "-f" => follow = true,
                    "--tail" => {
                        tail = rest
                            .next()
                            .ok_or("--tail needs a number of lines")?
                            .parse()
                            .map_err(|_| "--tail needs a number of lines")?
                    }
                    _ => return Err(USAGE.into()),
                }
            }
            source.logs(&id, tail, follow, &mut std::io::stdout())
        }
        [given, "change", rest @ ..] => {
            let id = source.resolve(given)?;
            let given = options(rest, USAGE)?;
            let change = Change {
                arguments: given.values,
                spec: given.spec,
                secrets: given.secrets,
            };
            finish(source.resource_run(
                &id,
                resource::CHANGE,
                change,
                &mut std::io::stdout(),
                &mut std::io::stderr(),
            )?)
        }
        [given, "remove", rest @ ..] => {
            let id = source.resolve(given)?;
            let (_, yes) = parse_arguments(rest)?;
            let actions = source.resource_actions(&id)?;
            ready(&id, &actions, resource::REMOVE, Default::default(), yes)?;
            finish(source.resource_run(
                &id,
                resource::REMOVE,
                Change::default(),
                &mut std::io::stdout(),
                &mut std::io::stderr(),
            )?)
        }
        [given, "run", action, rest @ ..] => {
            let id = source.resolve(given)?;
            let (values, yes) = parse_arguments(rest)?;
            let actions = source.resource_actions(&id)?;
            let (found, values) = ready(&id, &actions, action, values, yes)?;
            finish(source.resource_run(
                &id,
                &found.name,
                Change {
                    arguments: values,
                    ..Change::default()
                },
                &mut std::io::stdout(),
                &mut std::io::stderr(),
            )?)
        }
        _ => Err(USAGE.into()),
    }
}

/// `berth deploy NAME --provisioner PLUGIN/ID --spec FILE|- …`: make a
/// deployment from a specification, or give the one of that name the new
/// specification.
pub fn deploy(paths: &Paths, name: &str, args: &[&str]) -> Result<()> {
    let given = options(args, DEPLOY)?;
    let provisioner = given.provisioner.ok_or(DEPLOY)?;
    let spec = given.spec.ok_or(DEPLOY)?;
    let source = Source::of(paths);
    let existing = source
        .resources(Some(&provisioner), Some("deployment"), None)?
        .into_iter()
        .find(|known| known.name == name && !known.external_id.is_empty());
    if let Some(existing) = existing {
        eprintln!(
            "There is a deployment {name} already, {}: giving it this specification",
            existing.id
        );
        let change = Change {
            arguments: given.values,
            spec: Some(spec),
            secrets: given.secrets,
        };
        return finish(source.resource_run(
            &existing.id,
            resource::CHANGE,
            change,
            &mut std::io::stdout(),
            &mut std::io::stderr(),
        )?);
    }
    let asked = NewResource {
        provisioner,
        kind: "deployment".into(),
        name: name.to_owned(),
        arguments: given.values,
        spec: Some(spec),
        secrets: given.secrets.unwrap_or_default(),
        token: given.token,
        ..NewResource::default()
    };
    let (made, code) = source.create(&asked, &mut std::io::stdout(), &mut std::io::stderr())?;
    eprintln!("{}: berth resource {}", made.name, made.id);
    std::process::exit(code);
}

/// `berth provision HOST PLUGIN/ID [NAME=VALUE…] [--yes]`: create the
/// machine for a saved host and set it up.
pub fn provision(paths: &Paths, name: &str, provisioner: &str, args: &[&str]) -> Result<()> {
    let (mut values, yes) = parse_arguments(args)?;
    let source = Source::of(paths);
    let known = source.list()?;
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
    let asked = NewResource {
        provisioner: provisioner.to_owned(),
        kind: "machine".into(),
        host: Some(name.to_owned()),
        arguments: values,
        ..NewResource::default()
    };
    let (_, code) = source.create(&asked, &mut std::io::stdout(), &mut std::io::stderr())?;
    std::process::exit(code);
}

/// The machine of a host, as a resource.
fn machine_of(source: &Source, name: &str) -> Result<Resource> {
    source
        .resources(None, Some("machine"), Some(name))?
        .into_iter()
        .next()
        .ok_or_else(|| format!("{name} has no machine that berth created").into())
}

/// `berth instance HOST [start|stop|destroy [--yes]]`: the machine berth
/// created for the host, or a change to it, as its resource.
pub fn instance(paths: &Paths, name: &str, args: &[&str]) -> Result<()> {
    let source = Source::of(paths);
    let id = machine_of(&source, name)?.id;
    let run = |action: &str| {
        source.resource_run(
            &id,
            action,
            Change::default(),
            &mut std::io::stdout(),
            &mut std::io::stderr(),
        )
    };
    match args {
        [] => {
            let found = source.resource(&id)?;
            let actions = source.resource_actions(&id).unwrap_or_default();
            print!("{}", describe(&found, &actions));
            Ok(())
        }
        [verb @ ("start" | "stop")] => finish(run(verb)?),
        ["destroy", rest @ ..] => {
            let (_, yes) = parse_arguments(rest)?;
            if !yes && ask(&format!("Delete {name}'s machine? Type {name} to say so:"))? != name {
                return Err("Not deleted".into());
            }
            finish(run(resource::REMOVE)?)
        }
        _ => Err("Usage: berth instance HOST [start|stop|destroy [--yes]]".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_resource_is_described_with_what_can_be_done_to_it() {
        let found = Resource {
            id: "r-wiki".into(),
            provisioner: "docker/container".into(),
            kind: "deployment".into(),
            name: "wiki".into(),
            state: "running".into(),
            external_id: "wiki".into(),
            secrets: vec![secret("wiki_db:DB_PASSWORD")],
            spec_sha256: "ab12".into(),
            verbs: vec!["logs".into()],
            ..Resource::default()
        };
        let actions = [Action {
            name: "remove".into(),
            label: "Remove the deployment".into(),
            ..Action::default()
        }];
        let text = describe(&found, &actions);
        for line in [
            "r-wiki  deployment wiki of docker/container\n",
            "State      running\n",
            "Secrets    wiki_db as DB_PASSWORD\n",
            "  remove             Remove the deployment\n",
            "Its log: berth resource r-wiki logs [--follow]\n",
        ] {
            assert!(text.contains(line), "{line:?} in\n{text}");
        }
        assert_eq!(secret("token").variable(), "token");
        assert!(line(&found).starts_with("r-wiki       deployment  wiki"));
    }
}
