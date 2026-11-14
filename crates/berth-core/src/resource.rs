//! What a provisioner made for berth: a resource. A machine is one kind,
//! and may belong to a host; a plugin may declare others, such as a
//! deployment from a specification berth keeps and never reads. berth keeps
//! a record of each in its store, written before the provisioner is asked to
//! make it, so that asking again finds the same one, and removed only once
//! it is gone.
//!
//! A host's machine is kept in the scope of the host's profile, every other
//! resource in this installation's own. A kind with a specification keeps
//! it in a row of its own, so that listing resources reads none of them.

use crate::{
    Result,
    paths::Paths,
    plugins::{self, Plugin, Provisioner},
    profiles::{FILE_SCOPE, Place},
    provision::{
        self, Options, call, found, needs_secrets, resolve as provisioner_of, runnable, secrets,
        values_of,
    },
    registry::Store,
    runner::{Lock, Subject},
    store::{self, OWN, kind},
};
use berth_api::{Action, Argument, Resource, ResourceSecret, ops::Role};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{collections::BTreeMap, io::Write, path::PathBuf};

/// The record berth keeps of a resource.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    /// The name of the provisioner that made it.
    pub provisioner: String,
    pub kind: String,
    pub name: String,
    /// The provisioner's id of it; empty until `create` has said it.
    #[serde(default)]
    pub external_id: String,
    /// The host a machine belongs to, as its place names it: a saved name,
    /// or a profile file's path. Empty for a resource of no host.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub host: String,
    /// What `create` is asked with, so asking again finds the same one.
    pub token: String,
    /// The caller's token it was asked for with, so the same call again
    /// finds it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub asked: String,
    #[serde(default)]
    pub arguments: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<ResourceSecret>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub spec_sha256: String,
    #[serde(default)]
    pub created: String,
    /// As last seen: `running`, `stopped`, `missing` or `unknown`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub state: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub checked: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub address: String,
}

/// A record with the scope it is kept in.
#[derive(Debug, Clone, PartialEq)]
pub struct Kept {
    pub scope: String,
    pub record: Record,
}

/// The scopes this berth keeps resources in.
fn scopes(paths: &Paths) -> Vec<String> {
    let mut scopes = vec![OWN.to_owned(), FILE_SCOPE.to_owned()];
    let saved = crate::profiles::scope(paths);
    if !scopes.contains(&saved) {
        scopes.push(saved);
    }
    scopes
}

/// A new id for a resource.
pub fn new_id() -> Result<String> {
    let mut bytes = [0u8; 8];
    use std::io::Read;
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let letters = b"abcdefghijklmnopqrstuvwxyz234567";
    Ok(format!(
        "r-{}",
        bytes
            .iter()
            .map(|byte| letters[(byte & 31) as usize] as char)
            .collect::<String>()
    ))
}

/// Whether a text can be a resource's id: one word of berth's own making.
pub fn valid_id(id: &str) -> bool {
    id.starts_with("r-")
        && id.len() <= 40
        && id[2..]
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
}

/// Every resource berth keeps here.
pub fn list(paths: &Paths) -> Result<Vec<Kept>> {
    let store = store::shared(paths)?;
    let mut kept = vec![];
    for scope in scopes(paths) {
        for (name, value) in store.entries(kind::RESOURCE, &scope)? {
            let record: Record = serde_json::from_slice(&value).map_err(|error| {
                format!("The record of the resource {name} in berth's store: {error}")
            })?;
            kept.push(Kept {
                scope: scope.clone(),
                record,
            });
        }
    }
    kept.sort_by(|a, b| {
        (&a.record.provisioner, &a.record.kind, &a.record.name).cmp(&(
            &b.record.provisioner,
            &b.record.kind,
            &b.record.name,
        ))
    });
    Ok(kept)
}

/// A resource by its id, or by its name when only one has it.
pub fn find_by(paths: &Paths, given: &str) -> Result<Kept> {
    let all = list(paths)?;
    if let Some(found) = all.iter().find(|kept| kept.record.id == given) {
        return Ok(found.clone());
    }
    let named: Vec<&Kept> = all
        .iter()
        .filter(|kept| kept.record.name == given)
        .collect();
    match named.as_slice() {
        [one] => Ok((*one).clone()),
        [] => Err(format!("No resource {given}. See: berth resource").into()),
        _ => Err(format!(
            "Several resources are called {given}: name one by its id ({})",
            named
                .iter()
                .map(|kept| kept.record.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
        .into()),
    }
}

/// A resource by its id.
pub fn find(paths: &Paths, id: &str) -> Result<Kept> {
    list(paths)?
        .into_iter()
        .find(|kept| kept.record.id == id)
        .ok_or_else(|| format!("No resource {id}. See: berth resource").into())
}

/// The machine of the host whose profile is at `place`.
pub fn of_place(paths: &Paths, place: &Place) -> Result<Option<Kept>> {
    let (scope, name) = place.side();
    let store = store::shared(paths)?;
    for (id, value) in store.entries(kind::RESOURCE, &scope)? {
        let record: Record = serde_json::from_slice(&value).map_err(|error| {
            format!("The record of the resource {id} in berth's store: {error}")
        })?;
        if record.kind == plugins::MACHINE && record.host == name {
            return Ok(Some(Kept { scope, record }));
        }
    }
    Ok(None)
}

/// The machine of a host given as a command line names it.
pub fn of_host(paths: &Paths, host: &str) -> Result<Option<Kept>> {
    of_place(paths, &Place::resolve(paths, host)?)
}

impl Kept {
    pub fn id(&self) -> &str {
        &self.record.id
    }

    /// The place of the host it belongs to.
    pub fn place(&self) -> Option<Place> {
        if self.record.host.is_empty() {
            return None;
        }
        match self.scope.as_str() {
            FILE_SCOPE => Some(Place::File(PathBuf::from(&self.record.host))),
            scope => Place::stored(scope, &self.record.host).ok(),
        }
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        let value = serde_json::to_value(&self.record)?;
        store::shared(paths)?.write_json(kind::RESOURCE, &self.scope, &self.record.id, &value)?;
        Ok(())
    }

    /// Save the record with its specification, in one change.
    pub fn save_with_spec(&mut self, paths: &Paths, spec: Option<&str>) -> Result<()> {
        let Some(spec) = spec else {
            return self.save(paths);
        };
        self.record.spec_sha256 = sha256(spec);
        let value = serde_json::to_vec(&self.record)?;
        store::shared(paths)?.change(
            &[
                (kind::RESOURCE, &self.scope, &self.record.id, value),
                (
                    kind::SPEC,
                    &self.scope,
                    &self.record.id,
                    spec.as_bytes().to_vec(),
                ),
            ],
            &[],
        )?;
        Ok(())
    }

    /// Forget it with its specification.
    pub fn forget(&self, paths: &Paths) -> Result<()> {
        store::shared(paths)?.change(
            &[],
            &[
                (kind::RESOURCE, &self.scope, &self.record.id),
                (kind::SPEC, &self.scope, &self.record.id),
            ],
        )?;
        Ok(())
    }

    /// Its specification, for a kind that has one.
    pub fn spec(&self, paths: &Paths) -> Result<Option<String>> {
        let spec = store::shared(paths)?.read(kind::SPEC, &self.scope, &self.record.id)?;
        Ok(spec.map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
    }

    /// The subject of its jobs.
    pub fn subject(&self) -> Subject {
        Subject::Resource(self.record.id.clone())
    }

    /// Held while something is done to it. A host's machine shares the
    /// host's lock, so that applying and changing the machine exclude each
    /// other.
    pub fn lock(&self, paths: &Paths) -> Result<Lock> {
        match self.place() {
            Some(place) if self.record.kind == plugins::MACHINE => {
                let host = place.argument();
                crate::runner::lock_at(&place.lock_file(paths), "work on the machine", |holder| {
                    format!(
                        "Something else is being done to {host}'s machine{holder}. Try again when it has ended"
                    )
                })
            }
            _ => crate::runner::lock(paths, &self.subject(), "work on the resource"),
        }
    }

    /// Whether something is being done to it right now.
    pub fn busy(&self, paths: &Paths) -> bool {
        self.lock(paths).is_err()
    }

    /// What it is called for people: its name, and its host's for a machine.
    pub fn title(&self) -> String {
        match self.place() {
            Some(place) => format!("{}'s machine", place.title()),
            None => self.record.name.clone(),
        }
    }
}

pub(crate) fn sha256(text: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A provisioner's kind by its id.
fn kind_of<'a>(provisioner: &'a Provisioner, id: &str) -> Option<&'a berth_api::ProvisionerKind> {
    provisioner.kinds.iter().find(|kind| kind.id == id)
}

/// What a resource of `kind` takes: the provisioner's own arguments, then
/// the kind's. A machine's are the provisioner's whole list, as before.
pub(crate) fn declared(provisioner: &Provisioner, kind: &str) -> Vec<Argument> {
    if kind == plugins::MACHINE {
        return provisioner.arguments.clone();
    }
    let mut arguments: Vec<Argument> = provisioner
        .arguments
        .iter()
        .filter(|argument| provisioner.own.contains(&argument.name))
        .cloned()
        .collect();
    if let Some(found) = kind_of(provisioner, kind) {
        arguments.extend(
            found
                .arguments
                .iter()
                .filter(|argument| !provisioner.own.contains(&argument.name))
                .cloned(),
        );
    }
    arguments
}

/// What is said of a resource, as berth's record has it.
pub fn info(paths: &Paths, kept: &Kept, plugins: &[Plugin], busy: bool) -> Resource {
    let record = &kept.record;
    let verbs = provisioner_of(paths, plugins, &record.provisioner)
        .ok()
        .and_then(|(_, provisioner)| kind_of(provisioner, &record.kind).cloned())
        .map(|kind| kind.verbs)
        .unwrap_or_default();
    let (state, problem) = match (record.external_id.is_empty(), busy) {
        (true, true) => ("creating".to_owned(), String::new()),
        (true, false) => (
            "unknown".to_owned(),
            format!(
                "Creating it did not end. Asking again finds the same one; removing it forgets it: berth resource {} remove",
                record.id
            ),
        ),
        (false, _) if record.state.is_empty() => ("unknown".to_owned(), String::new()),
        (false, _) => (record.state.clone(), String::new()),
    };
    Resource {
        id: record.id.clone(),
        provisioner: record.provisioner.clone(),
        kind: record.kind.clone(),
        name: record.name.clone(),
        state,
        external_id: record.external_id.clone(),
        host: (!record.host.is_empty()).then(|| record.host.clone()),
        address: record.address.clone(),
        arguments: record.arguments.clone(),
        secrets: record.secrets.clone(),
        spec_sha256: record.spec_sha256.clone(),
        created: record.created.clone(),
        checked: record.checked.clone(),
        verbs,
        problem,
    }
}

/// The resources berth keeps, as they were last seen, narrowed by what is
/// given: a provisioner, a kind, a host. Nothing is asked.
pub fn listed(
    paths: &Paths,
    provisioner: Option<&str>,
    kind: Option<&str>,
    host: Option<&str>,
) -> Result<Vec<Resource>> {
    let plugins = found(paths);
    Ok(list(paths)?
        .into_iter()
        .filter(|kept| provisioner.is_none_or(|wanted| kept.record.provisioner == wanted))
        .filter(|kept| kind.is_none_or(|wanted| kept.record.kind == wanted))
        .filter(|kept| host.is_none_or(|wanted| kept.record.host == wanted))
        .map(|kept| {
            let busy = kept.record.external_id.is_empty() && kept.busy(paths);
            info(paths, &kept, &plugins, busy)
        })
        .collect())
}

// ── Asking the provisioner ──────────────────────────────────────────────────

/// A resource with its provisioner, ready to be asked about.
pub(crate) struct Called {
    pub kept: Kept,
    pub dir: PathBuf,
    pub provisioner: Provisioner,
    pub secrets: Map<String, Value>,
}

impl Called {
    pub(crate) fn load(paths: &Paths, store: &Store, kept: Kept) -> Result<Called> {
        let plugins = found(paths);
        let (plugin, provisioner) = provisioner_of(paths, &plugins, &kept.record.provisioner)
            .map_err(|_| {
                format!(
                    "No provisioner {} any more, which made {}: add its plugin again",
                    kept.record.provisioner,
                    kept.title()
                )
            })?;
        runnable(provisioner)?;
        let (secrets, missing) = secrets(store, &kept.record.provisioner, provisioner);
        needs_secrets(&kept.record.provisioner, &missing)?;
        Ok(Called {
            dir: plugin.dir.clone(),
            provisioner: provisioner.clone(),
            secrets,
            kept,
        })
    }

    /// Its arguments, with the defaults of those it did not have when it
    /// was made.
    pub(crate) fn arguments(&self) -> BTreeMap<String, String> {
        let mut arguments = self.kept.record.arguments.clone();
        for argument in declared(&self.provisioner, &self.kept.record.kind) {
            arguments
                .entry(argument.name.clone())
                .or_insert_with(|| argument.default.clone());
        }
        arguments
    }

    /// A request about it.
    pub(crate) fn request(&self, verb: &str, arguments: &BTreeMap<String, String>) -> Value {
        request(
            verb,
            &self.provisioner,
            &self.kept.record,
            arguments,
            &self.secrets,
        )
    }

    pub(crate) fn kind(&self) -> Option<&berth_api::ProvisionerKind> {
        kind_of(&self.provisioner, &self.kept.record.kind)
    }

    pub(crate) fn can(&self, verb: &str) -> bool {
        self.kind()
            .is_some_and(|kind| kind.verbs.iter().any(|known| known == verb))
    }
}

/// What every verb about a resource is sent.
pub(crate) fn request(
    verb: &str,
    provisioner: &Provisioner,
    record: &Record,
    arguments: &BTreeMap<String, String>,
    secrets: &Map<String, Value>,
) -> Value {
    let mut request = provision::request(verb, provisioner, arguments, secrets);
    request["kind"] = json!(record.kind);
    request["name"] = json!(record.name);
    if record.kind == plugins::MACHINE {
        // A machine's labels are those of berth before resources, which
        // plugins find their machines by.
        request["host"] = json!(record.name);
        request["labels"] = json!({"berth-host": record.name, "berth-instance": record.token});
    } else {
        request["labels"] = json!({"berth-resource": record.id, "berth-token": record.token});
    }
    if !record.external_id.is_empty() {
        request["id"] = json!(record.external_id);
    }
    request
}

/// The values of the berth secrets a resource is given, by the variable
/// each becomes, or the names of those without one.
pub(crate) fn environment(store: &Store, wanted: &[ResourceSecret]) -> Result<Map<String, Value>> {
    let mut given = Map::new();
    let mut missing = vec![];
    for secret in wanted {
        match store
            .secret(&secret.name)?
            .filter(|value| !value.is_empty() && value != crate::server::KEPT)
        {
            Some(value) => {
                given.insert(secret.variable().to_owned(), json!(value));
            }
            None => missing.push(secret.name.clone()),
        }
    }
    match missing.is_empty() {
        true => Ok(given),
        false => Err(format!(
            "It is given the secret{} {}, which {} no value: berth secret set NAME",
            if missing.len() == 1 { "" } else { "s" },
            missing.join(", "),
            if missing.len() == 1 { "has" } else { "have" }
        )
        .into()),
    }
}

/// Whether the secrets a resource is given are named as berth names secrets
/// and become variables a shell could take.
fn check_secrets(wanted: &[ResourceSecret]) -> Result<()> {
    for secret in wanted {
        if !plugins::valid_name(&secret.name) {
            return Err(format!("{} is not the name of a berth secret", secret.name).into());
        }
        let variable = secret.variable();
        let first = variable.chars().next().unwrap_or('0');
        if !(first.is_ascii_alphabetic() || first == '_')
            || !variable
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        {
            return Err(format!("{variable} cannot be the name of an environment variable").into());
        }
    }
    Ok(())
}

/// The state a provisioner's answer says, as berth shows it: what is gone
/// is missing.
fn state_of(answer: &Value) -> Option<String> {
    match answer["state"].as_str()? {
        "gone" | "missing" => Some("missing".into()),
        state @ ("running" | "stopped" | "creating") => Some(state.into()),
        _ => None,
    }
}

/// Ask the provisioner how a resource is, and keep what it says. One it no
/// longer finds is marked missing, and its record stays.
pub fn status(paths: &Paths, store: &Store, id: &str, options: &Options) -> Result<Resource> {
    let mut kept = find(paths, id)?;
    let plugins = found(paths);
    if kept.record.external_id.is_empty() {
        let busy = kept.busy(paths);
        return Ok(info(paths, &kept, &plugins, busy));
    }
    let called = match Called::load(paths, store, kept.clone()) {
        Ok(called) => called,
        Err(error) => {
            let mut said = info(paths, &kept, &plugins, false);
            (said.state, said.problem) = ("unknown".into(), error.to_string());
            return Ok(said);
        }
    };
    let asked = called.request("status", &called.arguments());
    let answer = call(
        &called.dir,
        &called.provisioner,
        "status",
        &asked,
        options.quick,
        &mut std::io::sink(),
    );
    let mut problem = String::new();
    match answer {
        Ok(answer) => match state_of(&answer) {
            Some(state) => {
                kept.record.state = state;
                if let Some(address) = answer["address"]
                    .as_str()
                    .filter(|address| !address.is_empty())
                {
                    kept.record.address = address.into();
                }
                kept.record.checked = berth_api::now();
                // Seen by someone else meanwhile, such as removed: what it said stands.
                if find(paths, id).is_ok() {
                    kept.save(paths)?;
                }
            }
            None => problem = format!("{} status said no state", kept.record.provisioner),
        },
        Err(error) => problem = error.to_string(),
    }
    let mut said = info(paths, &kept, &plugins, false);
    if !problem.is_empty() {
        (said.state, said.problem) = ("unknown".into(), problem);
    }
    Ok(said)
}

// ── Creating one ────────────────────────────────────────────────────────────

/// What asking for a resource came to.
pub enum Asked {
    /// The same call came before: its resource, made or being made.
    Again(Box<Kept>),
    /// A resource to make, checked and saved, with its lock taken.
    New(Box<Creating>),
}

/// A resource that has been checked and saved, ready to be made.
pub struct Creating {
    kept: Kept,
    work: Making,
    lock: Option<Lock>,
    hidden: Vec<String>,
}

enum Making {
    /// A host's machine, which is set up as the host once it answers.
    Machine(Box<provision::Prepared>),
    Other {
        dir: PathBuf,
        provisioner: Box<Provisioner>,
        request: Value,
    },
}

/// Check what is asked for and save its record before anything is made.
/// Refused: an unknown provisioner or kind, unknown or missing arguments
/// and secrets, a specification a kind does not take or lacks, a name that
/// is taken, and one something else is being done to.
pub fn prepare(paths: &Paths, store: &Store, asked: &berth_api::NewResource) -> Result<Asked> {
    let kind = match asked.kind.trim() {
        "" => plugins::MACHINE.to_owned(),
        kind => kind.to_owned(),
    };
    if let Some(token) = asked.token.as_deref().filter(|token| !token.is_empty())
        && let Some(again) = list(paths)?.into_iter().find(|kept| {
            kept.record.asked == token
                && kept.record.provisioner == asked.provisioner
                && kept.record.kind == kind
        })
    {
        return Ok(Asked::Again(Box::new(again)));
    }
    if kind == plugins::MACHINE {
        let host = asked
            .host
            .as_deref()
            .filter(|host| !host.is_empty())
            .ok_or("A machine is made for a saved host: give its host")?;
        let mut prepared = provision::prepare(paths, store, host, asked)?;
        let lock = prepared.take_lock();
        let hidden = prepared.secrets();
        let kept = prepared.kept().clone();
        return Ok(Asked::New(Box::new(Creating {
            kept,
            work: Making::Machine(Box::new(prepared)),
            lock,
            hidden,
        })));
    }
    if asked.host.as_deref().is_some_and(|host| !host.is_empty()) {
        return Err(format!("A {kind} belongs to no host; only a machine does").into());
    }
    let name = asked.name.trim();
    let usable = !name.is_empty()
        && name.len() <= 63
        && name
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || "-_".contains(ch))
        && name.starts_with(|ch: char| ch.is_ascii_alphanumeric());
    if !usable {
        return Err(format!(
            "{name:?} cannot be a {kind}'s name: lowercase letters, digits, - and _, starting with a letter or digit"
        )
        .into());
    }
    let plugins = found(paths);
    let (plugin, provisioner) = provisioner_of(paths, &plugins, &asked.provisioner)?;
    runnable(provisioner)?;
    let declared_kind = kind_of(provisioner, &kind).ok_or_else(|| {
        let kinds: Vec<&str> = provisioner
            .kinds
            .iter()
            .map(|kind| kind.id.as_str())
            .collect();
        format!(
            "{} makes no {kind}. It makes: {}",
            asked.provisioner,
            match kinds.is_empty() {
                true => "nothing".to_owned(),
                false => kinds.join(", "),
            }
        )
    })?;
    let spec = match (&declared_kind.spec, asked.spec.as_deref()) {
        (Some(_), Some(spec)) if !spec.trim().is_empty() => Some(spec),
        (Some(_), _) => {
            return Err(format!("A {kind} is made from a specification: give it").into());
        }
        (None, Some(_)) => return Err(format!("A {kind} takes no specification").into()),
        (None, None) => None,
    };
    if spec.is_some_and(|spec| spec.len() > store::VALUE_LIMIT) {
        return Err(format!(
            "The specification is {} bytes, and berth keeps at most {}",
            spec.map_or(0, str::len),
            store::VALUE_LIMIT
        )
        .into());
    }
    check_secrets(&asked.secrets)?;
    let existing = list(paths)?.into_iter().find(|kept| {
        kept.record.provisioner == asked.provisioner
            && kept.record.kind == kind
            && kept.record.name == name
    });
    if let Some(existing) = existing
        .as_ref()
        .filter(|kept| !kept.record.external_id.is_empty())
    {
        return Err(format!(
            "There is a {kind} {name} of {} already, {}. Change it with: berth resource {} change, or remove it first",
            asked.provisioner, existing.record.id, existing.record.id
        )
        .into());
    }
    let settings = crate::provisioner::settings(paths, &asked.provisioner)?;
    let arguments = values_of(
        &asked.provisioner,
        &declared(provisioner, &kind),
        &asked.arguments,
        &settings,
    )?;
    let (secrets, missing) = secrets(store, &asked.provisioner, provisioner);
    needs_secrets(&asked.provisioner, &missing)?;
    let environment = environment(store, &asked.secrets)?;
    // Asked again after a failure, the provisioner finds it by its token.
    let mut kept = match existing {
        Some(mut existing) => {
            existing.record.arguments = arguments;
            existing.record.secrets = asked.secrets.clone();
            existing.record.asked = asked.token.clone().unwrap_or_default();
            existing
        }
        None => Kept {
            scope: OWN.to_owned(),
            record: Record {
                id: new_id()?,
                provisioner: asked.provisioner.clone(),
                kind: kind.clone(),
                name: name.to_owned(),
                token: provision::new_token()?,
                asked: asked.token.clone().unwrap_or_default(),
                arguments,
                secrets: asked.secrets.clone(),
                created: berth_api::now(),
                ..Record::default()
            },
        },
    };
    let lock = kept.lock(paths)?;
    let mut request = request(
        "create",
        provisioner,
        &kept.record,
        &kept.record.arguments,
        &secrets,
    );
    request["token"] = json!(kept.record.token);
    if let Some(spec) = spec {
        request["spec"] = json!(spec);
    }
    request["environment"] = Value::Object(environment.clone());
    let mut hidden = provision::hidden(&secrets);
    hidden.extend(provision::hidden(&environment));
    // Written before it is asked for, so a crash leaves the token to ask again with.
    kept.save_with_spec(paths, spec)?;
    Ok(Asked::New(Box::new(Creating {
        kept,
        work: Making::Other {
            dir: plugin.dir.clone(),
            provisioner: Box::new(provisioner.clone()),
            request,
        },
        lock: Some(lock),
        hidden,
    })))
}

impl Creating {
    pub fn kept(&self) -> &Kept {
        &self.kept
    }

    /// Its lock, for a job to hold while it runs.
    pub fn take_lock(&mut self) -> Option<Lock> {
        self.lock.take()
    }

    /// What its job never shows.
    pub fn secrets(&self) -> Vec<String> {
        self.hidden.clone()
    }

    /// Make it, with the progress to `out`. A failure after it was made
    /// leaves it and its record, and says so.
    pub fn run(
        self,
        paths: &Paths,
        store: &Store,
        options: &Options,
        out: &mut dyn Write,
    ) -> Result<()> {
        let Creating {
            mut kept,
            work,
            lock,
            ..
        } = self;
        match work {
            Making::Machine(prepared) => {
                let _lock = lock;
                prepared.run(paths, store, options, out)
            }
            Making::Other {
                dir,
                provisioner,
                request,
            } => {
                let _lock = lock;
                say(
                    out,
                    &format!(
                        "▸ Creating the {} {} with {}",
                        kept.record.kind, kept.record.name, kept.record.provisioner
                    ),
                );
                let answer = call(
                    &dir,
                    &provisioner,
                    "create",
                    &request,
                    options.limit("create"),
                    out,
                )?;
                let external = answer["id"].as_str().unwrap_or("").to_owned();
                if external.is_empty() {
                    return Err(format!(
                        "{} create said no id. Asking again finds the same one",
                        kept.record.provisioner
                    )
                    .into());
                }
                kept.record.external_id = external.clone();
                kept.record.state = state_of(&answer).unwrap_or_else(|| "running".into());
                kept.record.checked = berth_api::now();
                if let Some(address) = answer["address"].as_str() {
                    kept.record.address = address.to_owned();
                }
                kept.save(paths)?;
                say(
                    out,
                    &format!(
                        "✓ {} created {external}: {}",
                        kept.record.provisioner, kept.record.state
                    ),
                );
                Ok(())
            }
        }
    }
}

fn say(out: &mut dyn Write, line: &str) {
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

// ── Its actions ─────────────────────────────────────────────────────────────

/// berth's own action that removes a resource.
pub const REMOVE: &str = "remove";
/// berth's own action that changes a resource's arguments and specification.
pub const CHANGE: &str = "change";

/// The argument of the change form that holds the specification, when the
/// kind has one.
pub const SPEC_FIELD: &str = "spec";

fn own(name: &str, label: &str, help: &str) -> Action {
    Action {
        name: name.into(),
        label: label.into(),
        help: help.into(),
        group: "Resource".into(),
        role: Role::Admin,
        ..Action::default()
    }
}

/// What can be done to a resource: start and stop when its kind can,
/// changing it, its plugin's actions of the kind, and removing it. Each
/// says whether it can run now.
pub fn actions(paths: &Paths, store: &Store, kept: &Kept) -> Vec<Action> {
    let plugins = found(paths);
    let record = &kept.record;
    let made = !record.external_id.is_empty();
    let machine = record.kind == plugins::MACHINE;
    let provisioner = provisioner_of(paths, &plugins, &record.provisioner)
        .ok()
        .map(|(_, found)| found);
    let kind = provisioner.and_then(|found| kind_of(found, &record.kind));
    let why = match provisioner {
        None => Some(format!(
            "No provisioner {} any more: add its plugin again",
            record.provisioner
        )),
        Some(found) => runnable(found)
            .and_then(|()| {
                needs_secrets(
                    &record.provisioner,
                    &secrets(store, &record.provisioner, found).1,
                )
            })
            .err()
            .map(|error| error.to_string())
            .or_else(|| (!made).then(|| format!("{} is not made yet", kept.title()))),
    };
    let limit = |mut action: Action| {
        if let Some(why) = &why {
            action.available = false;
            action.why = why.clone();
        }
        action
    };
    let mut actions = vec![];
    let can = |verb: &str| kind.is_some_and(|kind| kind.verbs.iter().any(|known| known == verb));
    if can("start") {
        actions.push(limit(Action {
            group: "Power".into(),
            ..own("start", "Start", "Start it, as its provisioner does.")
        }));
    }
    if can("stop") {
        actions.push(limit(Action {
            group: "Power".into(),
            confirm: Some(format!("Stop {}?", kept.title())),
            ..own(
                "stop",
                "Stop",
                "Stop it. It keeps what it has, and starts again with Start.",
            )
        }));
    }
    if can("change")
        && let (Some(provisioner), Some(kind)) = (provisioner, kind)
    {
        let arguments: Vec<Argument> = declared(provisioner, &record.kind)
            .into_iter()
            .filter(|argument| !provisioner.own.contains(&argument.name))
            .map(|argument| Argument {
                default: record
                    .arguments
                    .get(&argument.name)
                    .cloned()
                    .unwrap_or(argument.default.clone()),
                required: false,
                ..argument
            })
            .collect();
        let help = match kind.spec.is_some() {
            true => {
                "Give it other values of its arguments, and its specification, which berth keeps and opens in the editor. From the command line a new one is given with: berth resource ID change --spec FILE"
            }
            false => "Give it other values of its arguments, as its provisioner can.",
        };
        let mut arguments = arguments;
        if kind.spec.is_some() {
            arguments.push(Argument {
                name: SPEC_FIELD.into(),
                label: "Specification".into(),
                kind: Argument::TEXT.into(),
                default: kept.spec(paths).ok().flatten().unwrap_or_default(),
                required: false,
                help: "The specification, as the kind takes it and as berth keeps it.".into(),
                ..Argument::default()
            });
        }
        actions.push(limit(own(CHANGE, "Change", help).with_arguments(arguments)));
    }
    for action in kind.map(|kind| kind.actions.clone()).unwrap_or_default() {
        actions.push(limit(action));
    }
    // Removing an unfinished one only forgets it, which needs no provisioner.
    let remove = match machine {
        true => own(
            REMOVE,
            "Delete the machine",
            "Delete the machine with its provisioner and forget it. A host's profile stays, and so does its node on a tailnet.",
        ),
        false => own(
            REMOVE,
            &format!("Remove the {}", record.kind),
            "Remove it with its provisioner and forget it, with its specification.",
        ),
    };
    let remove = Action {
        confirm: Some(match kept.place() {
            Some(_) if machine => format!("Delete {}? The host's profile stays.", kept.title()),
            _ => format!(
                "Remove the {} {} with what it holds?",
                record.kind, record.name
            ),
        }),
        ..remove
    };
    actions.push(match (made, &why) {
        (true, Some(why)) => Action {
            available: false,
            why: why.clone(),
            ..remove
        },
        _ => remove,
    });
    actions
}

trait WithArguments {
    fn with_arguments(self, arguments: Vec<Argument>) -> Self;
}

impl WithArguments for Action {
    fn with_arguments(mut self, arguments: Vec<Argument>) -> Action {
        self.arguments = arguments;
        self
    }
}

/// What an action of a resource is given besides its arguments.
#[derive(Debug, Clone, Default)]
pub struct Given {
    pub arguments: BTreeMap<String, String>,
    /// A new specification, for `change`.
    pub spec: Option<String>,
    /// Other secrets, for `change`.
    pub secrets: Option<Vec<ResourceSecret>>,
}

/// What a resource has after a change: its arguments, its secrets and a new
/// specification.
type After = (
    BTreeMap<String, String>,
    Vec<ResourceSecret>,
    Option<String>,
);

/// An action of a resource that has been checked, ready to run.
pub struct Acting {
    kept: Kept,
    action: Action,
    work: Doing,
    lock: Option<Lock>,
    hidden: Vec<String>,
}

enum Doing {
    /// Forget a record whose `create` never said an id.
    Forget,
    Remove(Box<Called>),
    Verb {
        called: Box<Called>,
        verb: String,
        request: Value,
        /// What it has after a change: its arguments, secrets and specification.
        after: Option<After>,
    },
    /// One of its plugin's actions, run with `act`.
    Act {
        called: Box<Called>,
        request: Value,
        changes: bool,
    },
}

/// What a change is given, apart: the values of the arguments of its form,
/// kept ones where none is given, and a new specification. The form's
/// specification field is no argument, and is new only when it differs
/// from `stored`.
fn change_values(
    name: &str,
    form: &[Argument],
    given: &Given,
    kept: &BTreeMap<String, String>,
    stored: Option<&str>,
) -> Result<(BTreeMap<String, String>, Option<String>)> {
    let declared: Vec<Argument> = form
        .iter()
        .filter(|argument| argument.name != SPEC_FIELD)
        .cloned()
        .collect();
    let mut arguments = given.arguments.clone();
    let form_spec = arguments.remove(SPEC_FIELD);
    let spec = given
        .spec
        .clone()
        .or_else(|| form_spec.filter(|spec| stored != Some(spec.as_str())));
    Ok((values_of(name, &declared, &arguments, kept)?, spec))
}

/// The names of the arguments whose value in `after` is not the one in `before`.
fn changed(before: &BTreeMap<String, String>, after: &BTreeMap<String, String>) -> Vec<String> {
    after
        .iter()
        .filter(|(name, value)| before.get(*name) != Some(*value))
        .map(|(name, _)| name.clone())
        .collect()
}

/// Check that the action `action` of a resource can run with what is given,
/// and make its request. One that changes something takes the resource's
/// lock now.
pub fn prepare_action(
    paths: &Paths,
    store: &Store,
    id: &str,
    action: &str,
    given: &Given,
) -> Result<Acting> {
    let kept = find(paths, id)?;
    let listed = actions(paths, store, &kept);
    let Some(found) = listed.iter().find(|known| known.name == action).cloned() else {
        let names: Vec<&str> = listed.iter().map(|known| known.name.as_str()).collect();
        return Err(format!(
            "{} has no action {action}. Its actions: {}",
            kept.record.id,
            names.join(", ")
        )
        .into());
    };
    if !found.available {
        return Err(found.why.clone().into());
    }
    if (given.spec.is_some() || given.secrets.is_some()) && action != CHANGE {
        return Err(format!("Only {CHANGE} takes a specification or secrets").into());
    }
    let lock = match found.changes {
        true => Some(kept.lock(paths)?),
        false => None,
    };
    if action == REMOVE && kept.record.external_id.is_empty() {
        return Ok(Acting {
            kept,
            action: found,
            work: Doing::Forget,
            lock,
            hidden: vec![],
        });
    }
    let called = Box::new(Called::load(paths, store, kept.clone())?);
    let mut hidden = provision::hidden(&called.secrets);
    let spec_of = |kept: &Kept| -> Result<Option<String>> {
        match called.kind().and_then(|kind| kind.spec.as_ref()) {
            Some(_) => kept.spec(paths),
            None => Ok(None),
        }
    };
    let work = match action {
        REMOVE => Doing::Remove(called),
        "start" | "stop" => {
            let request = called.request(action, &called.arguments());
            Doing::Verb {
                verb: action.to_owned(),
                called,
                request,
                after: None,
            }
        }
        CHANGE => {
            let (theirs, new_spec) = change_values(
                &format!("{CHANGE} of {}", kept.record.id),
                &found.arguments,
                given,
                &kept.record.arguments,
                spec_of(&kept).ok().flatten().as_deref(),
            )?;
            let mut given = given.clone();
            given.spec = new_spec;
            let before = called.arguments();
            let changed = changed(&before, &theirs);
            let mut arguments = before;
            arguments.extend(theirs);
            let spec = match (
                &given.spec,
                called.kind().and_then(|kind| kind.spec.as_ref()),
            ) {
                (Some(_), None) => {
                    return Err(format!("A {} takes no specification", kept.record.kind).into());
                }
                (Some(spec), Some(_)) if spec.trim().is_empty() => {
                    return Err("The specification is empty".into());
                }
                (Some(spec), Some(_)) if spec.len() > store::VALUE_LIMIT => {
                    return Err(format!(
                        "berth keeps a specification of at most {} bytes",
                        store::VALUE_LIMIT
                    )
                    .into());
                }
                (Some(spec), Some(_)) => Some(spec.clone()),
                (None, _) => spec_of(&kept)?,
            };
            let wanted = given
                .secrets
                .clone()
                .unwrap_or_else(|| kept.record.secrets.clone());
            check_secrets(&wanted)?;
            let environment = environment(store, &wanted)?;
            hidden.extend(provision::hidden(&environment));
            let mut request = called.request(CHANGE, &arguments);
            if let Some(spec) = &spec {
                request["spec"] = json!(spec);
            }
            request["environment"] = Value::Object(environment);
            // The arguments carry every value, so a provisioner that changes only some can tell
            // which were given anew.
            request["changed"] = json!(changed);
            let new_spec = given.spec.clone();
            Doing::Verb {
                verb: CHANGE.to_owned(),
                called,
                request,
                after: Some((arguments, wanted, new_spec)),
            }
        }
        _ => {
            let declared = found.arguments.clone();
            let theirs = values_of(
                &format!("{action} of {}", kept.record.id),
                &declared,
                &given.arguments,
                &BTreeMap::new(),
            )?;
            hidden.extend(
                declared
                    .iter()
                    .filter(|argument| argument.secret)
                    .filter_map(|argument| theirs.get(&argument.name).cloned()),
            );
            let mut request = called.request("act", &called.arguments());
            request["action"] = json!({"name": action, "arguments": theirs});
            if let Some(spec) = spec_of(&kept)? {
                request["spec"] = json!(spec);
            }
            let environment = environment(store, &kept.record.secrets)?;
            hidden.extend(provision::hidden(&environment));
            request["environment"] = Value::Object(environment);
            Doing::Act {
                called,
                request,
                changes: found.changes,
            }
        }
    };
    Ok(Acting {
        kept,
        action: found,
        work,
        lock,
        hidden,
    })
}

impl Acting {
    pub fn action(&self) -> &Action {
        &self.action
    }

    pub fn kept(&self) -> &Kept {
        &self.kept
    }

    /// Whether it changes something: then it runs as a job.
    pub fn changes(&self) -> bool {
        self.action.changes
    }

    pub fn take_lock(&mut self) -> Option<Lock> {
        self.lock.take()
    }

    /// What never reaches its output.
    pub fn secrets(&self) -> Vec<String> {
        self.hidden.clone()
    }

    /// Run it, with its progress to `out`, and what it answered.
    pub fn run(
        self,
        paths: &Paths,
        options: &Options,
        out: &mut dyn Write,
    ) -> Result<crate::provisioner::Answered> {
        let Acting {
            kept, work, lock, ..
        } = self;
        let _lock = lock;
        let done = |message: String| crate::provisioner::Answered {
            message,
            details: vec![],
        };
        match work {
            Doing::Forget => {
                kept.forget(paths)?;
                Ok(done(format!(
                    "Forgot {}, whose making did not end. One {} made anyway is not removed",
                    kept.title(),
                    kept.record.provisioner
                )))
            }
            Doing::Remove(called) => {
                say(
                    out,
                    &format!(
                        "▸ Removing {} ({}) with {}",
                        kept.title(),
                        kept.record.external_id,
                        kept.record.provisioner
                    ),
                );
                let request = called.request("destroy", &called.arguments());
                if let Err(error) = call(
                    &called.dir,
                    &called.provisioner,
                    "destroy",
                    &request,
                    options.limit("destroy"),
                    out,
                ) {
                    // One that is gone already is what was asked for.
                    let mut asked = request.clone();
                    asked["verb"] = json!("status");
                    let gone = call(
                        &called.dir,
                        &called.provisioner,
                        "status",
                        &asked,
                        options.quick,
                        &mut std::io::sink(),
                    )
                    .is_ok_and(|answer| {
                        matches!(answer["state"].as_str(), Some("gone" | "missing"))
                    });
                    if !gone {
                        return Err(error);
                    }
                }
                kept.forget(paths)?;
                Ok(done(match kept.place() {
                    Some(place) if kept.record.kind == plugins::MACHINE => format!(
                        "Deleted {}'s machine. The profile stays, and so does its node on the tailnet: remove that there",
                        place.argument()
                    ),
                    _ => format!("Removed the {} {}", kept.record.kind, kept.record.name),
                }))
            }
            Doing::Verb {
                called,
                verb,
                request,
                after,
            } => {
                let answer = call(
                    &called.dir,
                    &called.provisioner,
                    &verb,
                    &request,
                    options.limit(&verb),
                    out,
                )?;
                // Read again: a status meanwhile may have written it.
                let mut kept = find(paths, &kept.record.id).unwrap_or(kept);
                if let Some(state) = state_of(&answer) {
                    kept.record.state = state;
                    kept.record.checked = berth_api::now();
                } else if verb == "stop" {
                    kept.record.state = "stopped".into();
                } else if verb == "start" {
                    kept.record.state = "running".into();
                }
                match after {
                    Some((arguments, secrets, spec)) => {
                        kept.record.arguments = arguments;
                        kept.record.secrets = secrets;
                        kept.save_with_spec(paths, spec.as_deref())?;
                    }
                    None => kept.save(paths)?,
                }
                Ok(done(match verb.as_str() {
                    "start" => format!("Started {}", kept.title()),
                    "stop" => format!("Stopped {}", kept.title()),
                    _ => format!("Changed {}: {}", kept.title(), kept.record.state),
                }))
            }
            Doing::Act {
                called,
                request,
                changes,
            } => {
                let limit = match changes {
                    true => options.create,
                    false => options.quick,
                };
                let answer = call(
                    &called.dir,
                    &called.provisioner,
                    "act",
                    &request,
                    limit,
                    out,
                )?;
                Ok(crate::provisioner::Answered {
                    message: answer["message"].as_str().unwrap_or("").to_owned(),
                    details: crate::provisioner::details(&answer),
                })
            }
        }
    }
}

// ── Its log ─────────────────────────────────────────────────────────────────

/// What a resource wrote, as its provisioner's `logs` verb gives it: the
/// last `tail` lines, and with `follow` what comes after until it ends or
/// `limit` has passed. Its lines go to `out` with secrets masked.
pub fn logs(
    paths: &Paths,
    store: &Store,
    id: &str,
    tail: u32,
    follow: bool,
    limit: std::time::Duration,
    out: &mut dyn Write,
) -> Result<()> {
    let kept = find(paths, id)?;
    if kept.record.external_id.is_empty() {
        return Err(format!("{} is not made yet", kept.title()).into());
    }
    let called = Called::load(paths, store, kept)?;
    if !called.can("logs") {
        return Err(format!(
            "{} gives no logs of a {}",
            called.kept.record.provisioner, called.kept.record.kind
        )
        .into());
    }
    let mut asked = called.request("logs", &called.arguments());
    (asked["tail"], asked["follow"]) = (json!(tail), json!(follow));
    let environment = environment(store, &called.kept.record.secrets).unwrap_or_default();
    let mut hidden = provision::hidden(&called.secrets);
    hidden.extend(provision::hidden(&environment));
    crate::provisioner::stream(
        &called.dir,
        &called.provisioner,
        &asked,
        &hidden,
        limit,
        out,
    )
}

/// What a provisioner's own list says of what it holds, joined with
/// berth's records of it.
pub(crate) fn merge(
    records: &[Kept],
    provisioner: &str,
    listed: Vec<berth_api::ListedResource>,
) -> Vec<berth_api::ListedResource> {
    let mut joined: Vec<berth_api::ListedResource> = records
        .iter()
        .filter(|kept| kept.record.provisioner == provisioner)
        .map(|kept| berth_api::ListedResource {
            id: kept.record.external_id.clone(),
            kind: kept.record.kind.clone(),
            name: kept.record.name.clone(),
            host: kept.record.host.clone(),
            state: String::new(),
            resource: kept.record.id.clone(),
        })
        .collect();
    for entry in listed {
        match joined
            .iter_mut()
            .find(|known| !known.id.is_empty() && known.id == entry.id && known.kind == entry.kind)
        {
            Some(known) => known.state = entry.state,
            None => joined.push(entry),
        }
    }
    joined
}

/// What a provisioner's `list` answered: each entry with an id, of a kind,
/// the machine kind when it says none.
pub(crate) fn listed_of(answer: &Value) -> Vec<berth_api::ListedResource> {
    let listed = answer["resources"].as_array().cloned().unwrap_or_default();
    let text = |entry: &Value, key: &str| entry[key].as_str().unwrap_or("").to_owned();
    listed
        .iter()
        .filter(|entry| !text(entry, "id").is_empty())
        .map(|entry| {
            let kind = match text(entry, "kind") {
                kind if kind.is_empty() => plugins::MACHINE.to_owned(),
                kind => kind,
            };
            let host = text(entry, "host");
            let name = match text(entry, "name") {
                name if name.is_empty() => host.clone(),
                name => name,
            };
            berth_api::ListedResource {
                id: text(entry, "id"),
                host: match (host.is_empty(), kind == plugins::MACHINE) {
                    (true, true) => name.clone(),
                    _ => host,
                },
                kind,
                name,
                state: text(entry, "state"),
                resource: String::new(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argument(name: &str, default: &str) -> Argument {
        Argument {
            name: name.into(),
            default: default.into(),
            ..Argument::default()
        }
    }

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn the_specification_of_a_change_form_is_no_argument() {
        let stored = "services:\n  wiki:\n    image: wiki:1\n";
        let form = [argument("replicas", "1"), argument(SPEC_FIELD, stored)];
        let kept = map(&[("replicas", "1")]);
        let given = |pairs: &[(&str, &str)]| Given {
            arguments: map(pairs),
            ..Given::default()
        };
        let new = "services:\n  wiki:\n    image: wiki:2\n";
        let (values, spec) = change_values(
            "change",
            &form,
            &given(&[(SPEC_FIELD, new), ("replicas", "2")]),
            &kept,
            Some(stored),
        )
        .unwrap();
        assert_eq!(values, map(&[("replicas", "2")]));
        assert_eq!(spec.as_deref(), Some(new));
        // The stored one given back is no new one, and none given is none.
        for pairs in [&[(SPEC_FIELD, stored)][..], &[]] {
            let (values, spec) =
                change_values("change", &form, &given(pairs), &kept, Some(stored)).unwrap();
            assert_eq!((values, spec), (map(&[("replicas", "1")]), None));
        }
    }

    #[test]
    fn a_change_names_the_arguments_given_anew() {
        let before = map(&[("cpus", "2"), ("image", "debian:12"), ("host", "")]);
        let after = map(&[("cpus", "4"), ("image", "debian:12"), ("memory", "2g")]);
        assert_eq!(changed(&before, &after), ["cpus", "memory"]);
    }
}
