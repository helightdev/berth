//! A provisioner as a subject of its own: the settings that say where it is,
//! what it says of itself, the resources berth made with it, and its actions.
//! Its program runs where berth runs, as for creating a machine (see
//! [`crate::provision`]): `status` without an id answers for the provisioner
//! itself, and `act` runs one of its actions.

use crate::{
    Result,
    paths::Paths,
    plugins::{self, Provisioner},
    provision::{
        self, Options, call, find, found, needs_secrets, request, runnable, secrets, values_of,
    },
    registry::Store,
    store::{self, OWN, kind},
};
use berth_api::{Action, Argument, Detail, ProvisionerInfo, ProvisionerState, ops::Role};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
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
/// berth's own action that keeps the secrets of one registered provisioner.
pub const SECRETS: &str = "secrets";
/// berth's own action that forgets a registered provisioner.
pub const REMOVE: &str = "remove";
/// The start of berth's own action that makes a resource of a kind, such as
/// `new-deployment`.
pub const NEW: &str = "new-";
/// What a secret of a provisioner's is set to, to forget the one it keeps.
const FORGET: &str = "-";

// ── Settings ────────────────────────────────────────────────────────────────

/// A provisioner berth runs under a name of its own: the `PLUGIN/ID` of the
/// plugin's provisioner it is, and the values of its own arguments.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub settings: BTreeMap<String, String>,
}

/// The entry registered under a provisioner's name, if there is one.
pub fn entry(paths: &Paths, name: &str) -> Result<Option<Entry>> {
    Ok(store::shared(paths)?
        .read_json(kind::PROVISIONER_ENTRY, OWN, name)?
        .and_then(|entry| serde_json::from_value(entry).ok()))
}

/// Register a provisioner under `name`, of the type `entry.kind`.
pub fn save_entry(paths: &Paths, name: &str, entry: &Entry) -> Result<()> {
    if !plugins::valid_name(name) {
        return Err(format!(
            "{name} is not a name for a provisioner: lowercase letters, digits, - and _, starting with a letter"
        )
        .into());
    }
    store::shared(paths)?.write_json(kind::PROVISIONER_ENTRY, OWN, name, &json!(entry))?;
    Ok(())
}

/// The registered provisioners, by name.
pub fn entries(paths: &Paths) -> Result<Vec<(String, Entry)>> {
    let shared = store::shared(paths)?;
    let mut found = vec![];
    for name in shared.names(kind::PROVISIONER_ENTRY, OWN)? {
        if let Some(entry) = entry(paths, &name)? {
            found.push((name, entry));
        }
    }
    Ok(found)
}

/// The name a provisioner of the type `kind` is registered under when berth
/// registers it itself: `incus/vm` is `incus-vm`.
pub fn default_name(kind: &str) -> String {
    kind.replace('/', "-")
}

/// The berth secret a registered provisioner keeps as its own `secret`. One
/// without it is given the secret of that name that all share.
pub fn secret_name(name: &str, secret: &str) -> String {
    format!("provisioner/{name}/{secret}")
}

/// Whether `name` is the name of a secret a provisioner keeps of its own.
pub fn is_secret_name(name: &str) -> bool {
    name.strip_prefix("provisioner/")
        .and_then(|rest| rest.split_once('/'))
        .is_some_and(|(provisioner, secret)| {
            plugins::valid_name(provisioner) && plugins::valid_name(secret)
        })
}

/// Register a provisioner under `name`, or change the settings of the one
/// there is. A registered one keeps its type; settings left out stay.
pub fn register(paths: &Paths, name: &str, saved: &berth_api::SavedProvisioner) -> Result<Entry> {
    let existing = entry(paths, name)?;
    let wanted = saved.kind.trim();
    let kind = match (&existing, wanted) {
        (Some(existing), "") => existing.kind.clone(),
        (Some(existing), wanted) if wanted == existing.kind => existing.kind.clone(),
        (Some(existing), wanted) => {
            return Err(format!(
                "{name} is a {}, and a provisioner keeps its type. Register {wanted} under another name",
                existing.kind
            )
            .into());
        }
        (None, "") => {
            return Err(format!(
                "Give the type of {name}, such as incus/vm. See: berth provisioner types"
            )
            .into());
        }
        (None, wanted) => wanted.to_owned(),
    };
    let plugins = found(paths);
    let (_, provisioner) = find(&plugins, &kind)
        .map_err(|_| format!("No type {kind}. See: berth provisioner types"))?;
    let settings = match &saved.settings {
        Some(given) => kept_values(name, provisioner, given)?,
        None => existing
            .map(|existing| existing.settings)
            .unwrap_or_default(),
    };
    let entry = Entry { kind, settings };
    save_entry(paths, name, &entry)?;
    Ok(entry)
}

/// The values of its own arguments a provisioner keeps of `given`: names it
/// does not take and values outside those it takes are refused, and empty
/// ones are left out.
fn kept_values(
    name: &str,
    provisioner: &Provisioner,
    given: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    let declared: Vec<Argument> = own_arguments(provisioner)
        .into_iter()
        .map(|argument| Argument {
            required: false,
            ..argument
        })
        .collect();
    values_of(name, &declared, given, &BTreeMap::new())?;
    Ok(given
        .iter()
        .filter(|(_, value)| !value.trim().is_empty())
        .map(|(key, value)| (key.clone(), value.trim().to_owned()))
        .collect())
}

/// The name of this berth's provisioner for a machine another berth made
/// with its provisioner `name` of the type `kind`: one registered here of
/// that type, or one registered for it now. Without a type, the name stays.
pub fn adopted_name(paths: &Paths, name: &str, kind: &str) -> Result<String> {
    if kind.is_empty() {
        return Ok(name.to_owned());
    }
    let registered = entries(paths)?;
    if let Some((found, _)) = registered
        .iter()
        .find(|(known, entry)| known == name && entry.kind == kind)
        .or_else(|| registered.iter().find(|(_, entry)| entry.kind == kind))
    {
        return Ok(found.clone());
    }
    let stem = match plugins::valid_name(name) {
        true => name.to_owned(),
        false => default_name(kind),
    };
    let mut free = stem.clone();
    let mut number = 2;
    while registered.iter().any(|(known, _)| *known == free) {
        free = format!("{stem}-{number}");
        number += 1;
    }
    save_entry(
        paths,
        &free,
        &Entry {
            kind: kind.to_owned(),
            ..Entry::default()
        },
    )?;
    Ok(free)
}

/// Forget a registered provisioner and the secrets it keeps. One that still
/// holds resources is kept: they would have no provisioner.
pub fn remove_entry(paths: &Paths, store: &Store, name: &str) -> Result<()> {
    if entry(paths, name)?.is_none() {
        return Err(format!("No provisioner {name}. See: berth provisioner").into());
    }
    let held: Vec<String> = crate::resource::list(paths)?
        .into_iter()
        .filter(|kept| kept.record.provisioner == name)
        .map(|kept| kept.record.id)
        .collect();
    if !held.is_empty() {
        return Err(format!(
            "{name} still holds {}. Remove {} first: berth resource ID remove",
            held.join(", "),
            if held.len() == 1 { "it" } else { "them" }
        )
        .into());
    }
    let prefix = secret_name(name, "");
    for secret in store.all_secret_names()? {
        if secret.starts_with(&prefix) {
            store.set_secret(&secret, None)?;
        }
    }
    store::shared(paths)?.forget(kind::PROVISIONER_ENTRY, OWN, name)?;
    Ok(())
}

/// The values of a provisioner's own arguments that berth keeps, by name.
pub fn settings(paths: &Paths, name: &str) -> Result<BTreeMap<String, String>> {
    if let Some(entry) = entry(paths, name)? {
        return Ok(entry.settings);
    }
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
    if let Some(mut entry) = entry(paths, name)? {
        entry.settings = kept
            .into_iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        return save_entry(paths, name, &entry);
    }
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
    name: &str,
    plugin: &str,
    provisioner: &Provisioner,
) -> ProvisionerInfo {
    let missing = secrets(store, name, provisioner).1;
    let mut info = provisioner.info(plugin, missing.clone());
    info.id = name.to_owned();
    info.settings = settings(paths, name).unwrap_or_default();
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

/// A provisioner by its name, with its plugin's folder and id.
fn load(paths: &Paths, name: &str) -> Result<(PathBuf, String, Provisioner)> {
    let plugins = found(paths);
    let kind = entry(paths, name)?.map_or_else(|| name.to_owned(), |entry| entry.kind);
    let (plugin, provisioner) = find(&plugins, &kind)?;
    Ok((plugin.dir.clone(), plugin.id.clone(), provisioner.clone()))
}

/// What is said of one provisioner, as in the list.
pub fn info(paths: &Paths, store: &Store, name: &str) -> Result<ProvisionerInfo> {
    let (_, plugin, provisioner) = load(paths, name)?;
    Ok(describe(paths, store, name, &plugin, &provisioner))
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
pub(crate) fn details(answer: &Value) -> Vec<Detail> {
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

/// The page of a provisioner: what it says of itself now, and the resources
/// berth made with it with what its own list says of them. `given` are values of its own arguments for this
/// once, such as another server to ask.
pub fn state(
    paths: &Paths,
    store: &Store,
    name: &str,
    given: &BTreeMap<String, String>,
    options: &Options,
) -> Result<ProvisionerState> {
    let (dir, plugin, provisioner) = load(paths, name)?;
    let info = describe(paths, store, name, &plugin, &provisioner);
    let records = crate::resource::list(paths).unwrap_or_default();
    let mut listed = vec![];
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
            state.resources = crate::resource::merge(&records, name, vec![]);
            return Ok(state);
        }
    };
    let (secrets, _) = secrets(store, name, &provisioner);
    let asked = request("status", &provisioner, &own, &secrets);
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
        let asked = request("list", &provisioner, &own, &secrets);
        if let Ok(answer) = call(
            &dir,
            &provisioner,
            "list",
            &asked,
            options.quick,
            &mut std::io::sink(),
        ) {
            listed = crate::resource::listed_of(&answer);
        }
    }
    state.resources = crate::resource::merge(&records, name, listed);
    Ok(state)
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
    let why = needs_secrets(name, &secrets(store, name, &provisioner).1).err();
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
    for kind in provisioner
        .kinds
        .iter()
        .filter(|kind| kind.id != plugins::MACHINE)
    {
        let mut action = new_action(&provisioner, kind, &unset);
        if let Some(why) = &why {
            action.available = false;
            action.why = why.to_string();
        }
        actions.push(action);
    }
    if !provisioner.own.is_empty() {
        actions.push(settings_action(&provisioner, &kept));
    }
    if entry(paths, name)?.is_some() {
        if !provisioner.secrets.is_empty() {
            actions.push(secrets_action(name, &provisioner));
        }
        actions.push(remove_action(paths, name));
    }
    Ok(actions)
}

/// berth's own action that makes a resource of `kind`, such as a Compose
/// project: its name, its specification and the secrets it is given, then
/// the kind's own arguments.
fn new_action(
    provisioner: &Provisioner,
    kind: &berth_api::ProvisionerKind,
    unset: &[Argument],
) -> Action {
    let label = match kind.label.is_empty() {
        true => kind.id.clone(),
        false => kind.label.clone(),
    };
    let mut arguments = unset.to_vec();
    arguments.push(Argument {
        name: "name".into(),
        label: "Name".into(),
        required: true,
        help: format!(
            "The {label}'s name: lowercase letters, digits, - and _, starting with a letter or digit"
        ),
        ..Argument::default()
    });
    if let Some(spec) = &kind.spec {
        let format = spec["format"].as_str().unwrap_or("");
        arguments.push(Argument {
            name: "spec".into(),
            label: spec["label"]
                .as_str()
                .filter(|label| !label.is_empty())
                .unwrap_or("Specification")
                .to_owned(),
            required: true,
            kind: Argument::TEXT.into(),
            help: format!(
                "What it is made from{}. Paste it whole: its lines are kept.",
                match format.is_empty() {
                    true => String::new(),
                    false => format!(", as {format}"),
                }
            ),
            ..Argument::default()
        });
    }
    arguments.push(Argument {
        name: "secrets".into(),
        label: "Secrets".into(),
        help: "The berth secrets it is given in its environment, separated by commas. NAME gives the secret as the variable NAME, and NAME:VARIABLE as another.".into(),
        ..Argument::default()
    });
    arguments.extend(
        crate::resource::declared(provisioner, &kind.id)
            .into_iter()
            .filter(|argument| !provisioner.own.contains(&argument.name)),
    );
    Action {
        name: format!("{NEW}{}", kind.id),
        label: format!("New {}", label.to_lowercase()),
        help: format!(
            "Make a {label} with this provisioner, from what you paste. berth keeps it as a resource, with a page of its own and its actions.{}",
            match kind.id == "deployment" {
                true =>
                    " From the command line also: berth deploy NAME --provisioner PROVISIONER --spec FILE",
                false => "",
            }
        ),
        group: "Create".into(),
        arguments,
        role: Role::Admin,
        ..Action::default()
    }
}

/// berth's own action that keeps the secrets of one registered provisioner,
/// in place of those all of its type share.
fn secrets_action(name: &str, provisioner: &Provisioner) -> Action {
    Action {
        name: SECRETS.into(),
        label: "Secrets".into(),
        help: format!(
            "The secrets {name} uses of its own, such as the certificate of its server. Each is kept in berth's store as {}, and one it does not have is taken from the secret of that name that all share. An empty value keeps what it has, and {FORGET} forgets it.",
            secret_name(name, "NAME")
        ),
        group: "Set-Up".into(),
        arguments: provisioner
            .secrets
            .iter()
            .map(|secret| Argument {
                name: secret.name.clone(),
                label: secret.name.clone(),
                secret: true,
                ..Argument::default()
            })
            .collect(),
        role: Role::Admin,
        ..Action::default()
    }
}

/// berth's own action that forgets a registered provisioner, which it does
/// only when it holds nothing.
fn remove_action(paths: &Paths, name: &str) -> Action {
    let held = crate::resource::list(paths)
        .unwrap_or_default()
        .into_iter()
        .filter(|kept| kept.record.provisioner == name)
        .count();
    Action {
        name: REMOVE.into(),
        label: "Remove provisioner".into(),
        help: format!(
            "Forget {name}, with its settings and the secrets it keeps of its own. What it made is not touched, so berth refuses while it holds anything."
        ),
        group: "Set-Up".into(),
        confirm: Some(format!("Remove the provisioner {name}?")),
        role: Role::Admin,
        available: held == 0,
        why: match held {
            0 => String::new(),
            1 => "It holds a resource; remove it first".into(),
            count => format!("It holds {count} resources; remove them first"),
        },
        ..Action::default()
    }
}

/// The secrets a resource is given, as `NAME[:VARIABLE]` separated by
/// commas or spaces.
fn given_secrets(text: &str) -> Vec<berth_api::ResourceSecret> {
    text.split([',', ' ', '\n'])
        .map(str::trim)
        .filter(|given| !given.is_empty())
        .map(|given| match given.split_once(':') {
            Some((name, env)) => berth_api::ResourceSecret {
                name: name.trim().to_owned(),
                env: env.trim().to_owned(),
            },
            None => berth_api::ResourceSecret {
                name: given.to_owned(),
                env: String::new(),
            },
        })
        .collect()
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
    Secrets(BTreeMap<String, String>, Store),
    Remove(Store),
    Create(Box<crate::resource::Creating>, Store),
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
    if action == SECRETS {
        let values = values_of(name, &found.arguments, given, &BTreeMap::new())?;
        return Ok(Acting {
            name: name.to_owned(),
            hidden: hidden_of(&values, &found.arguments),
            action: found,
            work: Doing::Secrets(values, store.clone()),
        });
    }
    if !found.available {
        return Err(found.why.clone().into());
    }
    if action == REMOVE {
        values_of(name, &found.arguments, given, &BTreeMap::new())?;
        return Ok(Acting {
            name: name.to_owned(),
            hidden: vec![],
            action: found,
            work: Doing::Remove(store.clone()),
        });
    }
    if let Some(kind) = action
        .strip_prefix(NEW)
        .filter(|kind| provisioner.kinds.iter().any(|known| known.id == *kind))
    {
        let mut values = values_of(
            &format!("{action} of {name}"),
            &found.arguments,
            given,
            &kept,
        )?;
        let take = |values: &mut BTreeMap<String, String>, key: &str| {
            values.remove(key).unwrap_or_default()
        };
        let resource = take(&mut values, "name");
        let spec = values.remove("spec").filter(|spec| !spec.trim().is_empty());
        let secrets = given_secrets(&take(&mut values, "secrets"));
        // Only what was given: the rest are its settings and defaults, as for any resource.
        values.retain(|key, _| given.contains_key(key));
        let asked = berth_api::NewResource {
            provisioner: name.to_owned(),
            kind: kind.to_owned(),
            name: resource,
            host: None,
            arguments: values,
            spec,
            secrets,
            token: None,
        };
        let creating = match crate::resource::prepare(paths, store, &asked)? {
            crate::resource::Asked::New(creating) => creating,
            crate::resource::Asked::Again(kept) => {
                return Err(format!("{} was asked for already", kept.record.id).into());
            }
        };
        return Ok(Acting {
            name: name.to_owned(),
            hidden: creating.secrets(),
            action: found,
            work: Doing::Create(creating, store.clone()),
        });
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
    let (secrets, missing) = secrets(store, name, &provisioner);
    needs_secrets(name, &missing)?;
    let mut request = request("act", &provisioner, &own, &secrets);
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
            Doing::Secrets(values, store) => {
                let mut details = vec![];
                for (secret, value) in values {
                    let kept = secret_name(&self.name, &secret);
                    let said = match value.trim() {
                        "" => continue,
                        FORGET => {
                            store.set_secret(&kept, None)?;
                            "forgotten: the one all share is used"
                        }
                        value => {
                            store.set_secret(&kept, Some(value))?;
                            "kept"
                        }
                    };
                    details.push(Detail {
                        label: secret,
                        value: said.into(),
                    });
                }
                Ok(Answered {
                    message: match details.is_empty() {
                        true => format!("{}'s secrets are as they were", self.name),
                        false => format!("{}'s secrets", self.name),
                    },
                    details,
                })
            }
            Doing::Remove(store) => {
                remove_entry(paths, &store, &self.name)?;
                Ok(Answered {
                    message: format!("{} is removed", self.name),
                    details: vec![],
                })
            }
            Doing::Create(creating, store) => {
                let id = creating.kept().record.id.clone();
                let title = creating.kept().title();
                creating.run(paths, &store, options, out)?;
                Ok(Answered {
                    message: format!("{title} is made. Its page: berth resource {id}"),
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

/// Run `logs`: what it writes on standard output and error goes to `out`,
/// masked, until it ends; one that outlasts `limit` is stopped.
pub(crate) fn stream(
    dir: &std::path::Path,
    provisioner: &Provisioner,
    asked: &Value,
    hidden: &[String],
    limit: Duration,
    out: &mut dyn Write,
) -> Result<()> {
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
                let line = provision::hide(line, hidden);
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
