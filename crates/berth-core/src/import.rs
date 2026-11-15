//! Taking in what berth kept in files before it had a store, in three imports
//! with a marker row each:
//!
//! 1. the secrets of `secrets.json`, and the registries and the settings that
//!    are not [`is_local`] from `config.json`;
//! 2. the hosts of `hosts/`: each `NAME.json` with its `NAME.credentials.json`
//!    and `NAME.instance.json`;
//! 3. berth's own SSH key, the token of `berth serve`, its setup key and the
//!    user's own plugins ([`crate::kept`], [`crate::plugin_state`]). The key,
//!    the token and the plugin directory stay where they are, as they are
//!    used as files; only `serve.setup-key` goes.
//!
//! More change rows the store has already: how hosts are reached moves into
//! `access`, the records of hosts' machines become resources, provisioners
//! get names of their own, and profiles take the names their fields have
//! now ([`take_in_names`]).
//!
//! The rows of an import go into the store in one change with its marker, so
//! they are all there or none of them are. Only then are the old files put
//! away in `migrated/` beside them, and `config.json` keeps the local
//! settings. A start that finds a marker and the files still there was
//! interrupted between the two, and only puts the files away. The second
//! import also removes `served/` of the data directory, the profiles a mirror
//! of a berth server wrote, which the next mirror writes into the store.

use crate::Result;
use crate::paths::{Paths, SETTINGS, is_local, write_private};
use crate::store::{Installation, OWN, Store, kind, wait_lock};
use serde_json::{Map, Value, json};
use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// The name of the marker row (kind `meta`, scope [`OWN`]) of the first import.
pub const MARKER: &str = "import:settings";
/// The marker of the second import, of the hosts.
pub const PROFILES_MARKER: &str = "import:profiles";
/// The marker of the third import, of the keys, the tokens and the plugins.
pub const KEYS_MARKER: &str = "import:keys";
/// The marker of the fourth, which moves how hosts are reached out of
/// `builder` into `access` and berth's own key into the key ring.
pub const ACCESS_MARKER: &str = "import:access";
/// The marker of the fifth, which makes each record of a host's machine a
/// resource of the machine kind.
pub const RESOURCES_MARKER: &str = "import:resources";
/// The marker of the sixth, which registers each provisioner that has
/// settings or made something under a name of its own.
pub const PROVISIONERS_MARKER: &str = "import:provisioners";
/// The marker of the seventh, which gives every stored profile the names its
/// fields have now: `type` for `level` and `target` for `profile`.
pub const NAMES_MARKER: &str = "import:names";
/// The marker of the eighth, which takes out of every stored profile what
/// older berths kept of joining the tailnet without a key berth makes.
pub const TAILNET_MARKER: &str = "import:tailnet";

/// The marker of the ninth, which gives everything berth keeps under a name
/// an id, and has what refers to it, and what belongs to it, use the id.
pub const IDS_MARKER: &str = "import:ids";

/// Every import's marker: a store that lacks one is taken in by this berth.
pub const MARKERS: &[&str] = &[
    MARKER,
    PROFILES_MARKER,
    KEYS_MARKER,
    ACCESS_MARKER,
    RESOURCES_MARKER,
    PROVISIONERS_MARKER,
    NAMES_MARKER,
    TAILNET_MARKER,
    IDS_MARKER,
];

/// What a row of the import is.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub secrets: usize,
    pub registries: usize,
    pub settings: usize,
    /// Rows the store had already, which keep the store's value.
    pub kept: usize,
}

/// Take in the old files, once per installation. Safe when several berths
/// start at once and when one is stopped at any point.
pub fn run(paths: &Paths, installation: &Installation, store: &Store) -> Result<()> {
    // Names only, so a store whose key is missing still opens.
    let marked = |store: &Store, marker: &str| -> Result<bool> {
        Ok(store
            .names(kind::META, OWN)?
            .iter()
            .any(|name| name == marker))
    };
    if marked(store, MARKER)?
        && !leftover(paths)?
        && marked(store, PROFILES_MARKER)?
        && !profiles_leftover(paths)
        && marked(store, KEYS_MARKER)?
        && !setup_key_file(paths).exists()
        && marked(store, ACCESS_MARKER)?
        && marked(store, RESOURCES_MARKER)?
        && marked(store, PROVISIONERS_MARKER)?
        && marked(store, NAMES_MARKER)?
        && marked(store, TAILNET_MARKER)?
        && marked(store, IDS_MARKER)?
    {
        return Ok(());
    }
    let _lock = lock(paths)?;
    let place = installation.target.describe();
    let counts = match marked(store, MARKER)? {
        true => None,
        false => Some(take_in(paths, store)?),
    };
    let moved = put_away(paths)?;
    match (counts, moved) {
        (Some(counts), Some(dir)) => eprintln!(
            "berth: moved {} into berth's store, the {place}; the old files are in {}",
            said(&counts),
            dir.display()
        ),
        (None, Some(dir)) => eprintln!(
            "berth: the files whose contents are in berth's store, the {place}, are in {} now",
            dir.display()
        ),
        _ => {}
    }
    let hosts = match marked(store, PROFILES_MARKER)? {
        true => None,
        false => Some(take_in_profiles(paths, store)?),
    };
    let moved = put_away_profiles(paths)?;
    match (hosts, moved) {
        (Some(hosts), Some(dir)) => eprintln!(
            "berth: moved {} into berth's store, the {place}; the old profiles are in {}",
            said_hosts(&hosts),
            dir.display()
        ),
        (None, Some(dir)) => eprintln!(
            "berth: the profiles whose contents are in berth's store, the {place}, are in {} now",
            dir.display()
        ),
        _ => {}
    }
    if !marked(store, KEYS_MARKER)? {
        let taken = take_in_keys(paths, store)?;
        if !taken.is_empty() {
            eprintln!(
                "berth: put {} into berth's store, the {place}; the files stay where they are",
                taken.join(", ")
            );
        }
    }
    let file = setup_key_file(paths);
    if file.exists() {
        fs::remove_file(&file).map_err(|error| format!("{}: {error}", file.display()))?;
    }
    if !marked(store, ACCESS_MARKER)? {
        let known_hosts =
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".ssh/known_hosts"));
        for notice in take_in_access(paths, store, known_hosts.as_deref())? {
            eprintln!("berth: {notice}");
        }
    }
    if !marked(store, RESOURCES_MARKER)? {
        let converted = take_in_resources(store)?;
        if converted > 0 {
            eprintln!(
                "berth: the record of {converted} host{}'s machine is a resource now: see berth resource",
                if converted == 1 { "" } else { "s" }
            );
        }
    }
    if !marked(store, PROVISIONERS_MARKER)? {
        let registered = take_in_provisioners(store)?;
        if !registered.is_empty() {
            eprintln!(
                "berth: the provisioners in use are registered as {}: see berth provisioner list",
                registered.join(", ")
            );
        }
    }
    // Before the names, which drop the same fields without a word.
    if !marked(store, TAILNET_MARKER)? {
        let taken = take_in_tailnet(store)?;
        if let Some(said) = tailnet_message(&taken, store)? {
            eprintln!("berth: {said}");
        }
    }
    if !marked(store, NAMES_MARKER)? {
        let renamed = take_in_names(store)?;
        if renamed > 0 {
            let (noun, verb) = match renamed {
                1 => ("profile", "says"),
                _ => ("profiles", "say"),
            };
            eprintln!("berth: {renamed} {noun} {verb} type for level and target for profile now");
        }
    }
    // Last, as every import before keeps things by name.
    if !marked(store, IDS_MARKER)? {
        let plugins = crate::provision::found(paths);
        let taken = take_in_ids(store, &plugins)?;
        for said in taken.said() {
            eprintln!("berth: {said}");
        }
    }
    Ok(())
}

/// What the import of ids did.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct TakenIds {
    /// How many things were given an id.
    pub given: usize,
    /// The provisioners that were given a copy of a secret that all shared,
    /// as `PROVISIONER: SECRET`.
    pub copied: Vec<String>,
    /// The secrets all shared that went once each provisioner had its own.
    pub dropped: Vec<String>,
}

impl TakenIds {
    fn said(&self) -> Vec<String> {
        let mut said = vec![];
        if self.given > 0 {
            said.push(format!(
                "gave {} hosts, provisioners, resources, registries, secrets and keys ids of their own; their names can change now",
                self.given
            ));
        }
        if !self.copied.is_empty() {
            said.push(format!(
                "each provisioner has its own copy of the secrets it took from those all share: {}",
                self.copied.join(", ")
            ));
        }
        if !self.dropped.is_empty() {
            said.push(format!(
                "removed the secrets {} that all shared and nothing else uses",
                self.dropped.join(", ")
            ));
        }
        said
    }
}

/// The ninth import, in one change with its marker. Each host, provisioner,
/// resource, registry, global secret and key gets an id, and its name
/// becomes its label. What belongs to one is kept by its id: a host's
/// generated passwords, what berth knows of its SSH, its passwords for
/// logging in, a registry's login, a provisioner's secrets, a resource's
/// specification and a key's successor. Records, profiles, jobs, messages
/// and cursors refer to them by id. A provisioner that took a secret of the
/// name it declares from those all share gets its own copy, and a shared one
/// that nothing else uses then goes. A row that has a label already was
/// converted before and is left as it is.
pub(crate) fn take_in_ids(store: &Store, plugins: &[crate::plugins::Plugin]) -> Result<TakenIds> {
    use crate::store::{Batch, Rows, Table, new_id};
    use std::collections::{BTreeMap, BTreeSet};
    let rows = match store.export(Table::State)? {
        Rows::State(rows) => rows,
        _ => vec![],
    };
    // A row by name: one without a label, which no thing owns and which is
    // no resource of an id of the new shape.
    let converted = |name: &str| {
        berth_api::ids::is_id(name)
            || name
                .split_once('/')
                .is_some_and(|(owner, _)| berth_api::ids::is_id(owner))
    };
    let legacy = |kind: &str| -> Vec<(String, String)> {
        rows.iter()
            .filter(|row| row.kind == kind && row.label.is_none() && !converted(&row.name))
            .map(|row| (row.scope.clone(), row.name.clone()))
            .collect()
    };
    let labelled = |kind: &str| -> Vec<(String, String, String)> {
        rows.iter()
            .filter(|row| row.kind == kind)
            .filter_map(|row| Some((row.scope.clone(), row.name.clone(), row.label.clone()?)))
            .collect()
    };
    let mut batch = Batch::default();
    let mut taken = TakenIds::default();
    // (kind, scope, name) → id, for those converted now and before.
    let mut ids: BTreeMap<(&str, String, String), String> = BTreeMap::new();
    for (scope, id, label) in labelled(kind::PROFILE) {
        ids.insert((kind::PROFILE, scope, label), id);
    }
    for kind in [
        kind::PROVISIONER_ENTRY,
        kind::REGISTRY,
        kind::KEY,
        kind::SECRET,
    ] {
        for (scope, id, label) in labelled(kind) {
            ids.insert((kind, scope, label), id);
        }
    }
    // The things themselves.
    let mut values: Vec<(&str, String, String, String, Vec<u8>)> = vec![];
    for kind in [kind::PROFILE, kind::PROVISIONER_ENTRY, kind::REGISTRY] {
        for (scope, name) in legacy(kind) {
            let Some(value) = store.read(kind, &scope, &name)? else {
                continue;
            };
            let id = new_id();
            ids.insert((kind, scope.clone(), name.clone()), id.clone());
            batch.remove(kind, &scope, &name);
            values.push((kind, scope, id, name, value));
        }
    }
    for (scope, name) in legacy(kind::KEY) {
        if name.ends_with(crate::keys::ring::PENDING) {
            continue;
        }
        let Some(value) = store.read(kind::KEY, &scope, &name)? else {
            continue;
        };
        let id = new_id();
        ids.insert((kind::KEY, scope.clone(), name.clone()), id.clone());
        batch.remove(kind::KEY, &scope, &name);
        values.push((kind::KEY, scope, id, name, value));
    }
    for (scope, name) in legacy(kind::KEY) {
        let Some(key) = name.strip_suffix(crate::keys::ring::PENDING) else {
            continue;
        };
        batch.remove(kind::KEY, &scope, &name);
        if let (Some(id), Some(value)) = (
            ids.get(&(kind::KEY, scope.clone(), key.to_owned())),
            store.read(kind::KEY, &scope, &name)?,
        ) {
            batch.write(kind::KEY, &scope, &format!("{id}/pending"), value);
        }
    }
    let id_of =
        |ids: &BTreeMap<(&str, String, String), String>,
         kind: &'static str,
         scope: &str,
         name: &str| { ids.get(&(kind, scope.to_owned(), name.to_owned())).cloned() };
    // Secrets: what belongs to a registry, a host or a provisioner goes
    // under its owner; one whose owner is gone goes with it.
    for (scope, name) in legacy(kind::SECRET) {
        let owner_row =
            |owner: Option<String>, label: &str| owner.map(|id| format!("{id}/{label}"));
        let moved = if let Some(registry) = name.strip_prefix("registry/") {
            Some(owner_row(
                id_of(&ids, kind::REGISTRY, &scope, registry),
                "login",
            ))
        } else if let Some((label, host)) = name
            .strip_prefix("ssh/")
            .map(|host| ("ssh", host))
            .or_else(|| name.strip_prefix("sudo/").map(|host| ("sudo", host)))
        {
            // A profile in a file keeps its passwords by its path.
            match host.contains('/') || host.ends_with(".json") {
                true => continue,
                false => Some(owner_row(id_of(&ids, kind::PROFILE, &scope, host), label)),
            }
        } else if let Some((provisioner, secret)) = name
            .strip_prefix("provisioner/")
            .and_then(|rest| rest.split_once('/'))
        {
            Some(owner_row(
                id_of(&ids, kind::PROVISIONER_ENTRY, &scope, provisioner),
                secret,
            ))
        } else {
            None
        };
        let Some(value) = store.read(kind::SECRET, &scope, &name)? else {
            continue;
        };
        batch.remove(kind::SECRET, &scope, &name);
        match moved {
            Some(Some(row)) => {
                batch.write(kind::SECRET, &scope, &row, value);
            }
            Some(None) => {}
            None => {
                let id = new_id();
                ids.insert((kind::SECRET, scope.clone(), name.clone()), id.clone());
                values.push((kind::SECRET, scope, id, name, value));
            }
        }
    }
    taken.given = values.len();
    // What hangs off a host by its name: by its id now.
    for kind in [kind::CREDENTIALS, kind::ACCESS, kind::TAILNET] {
        for (scope, name) in legacy(kind) {
            let Some(id) = id_of(&ids, kind::PROFILE, &scope, &name) else {
                continue;
            };
            if let Some(value) = store.read(kind, &scope, &name)? {
                batch
                    .remove(kind, &scope, &name)
                    .write(kind, &scope, &id, value);
            }
        }
    }
    // Resources: an id of the new shape, their provisioner and host by id,
    // and their specification beside them.
    let mut resources: BTreeMap<String, String> = BTreeMap::new();
    for (scope, old) in legacy(kind::RESOURCE) {
        let Some(value) = store.read(kind::RESOURCE, &scope, &old)? else {
            continue;
        };
        let Ok(mut record) = serde_json::from_slice::<crate::resource::Record>(&value) else {
            continue;
        };
        let id = match berth_api::ids::is_id(&old) {
            true => old.clone(),
            false => new_id(),
        };
        record.id = id.clone();
        if let Some(provisioner) = id_of(&ids, kind::PROVISIONER_ENTRY, OWN, &record.provisioner) {
            record.provisioner = provisioner;
        }
        if scope != crate::profiles::FILE_SCOPE
            && let Some(host) = id_of(&ids, kind::PROFILE, &scope, &record.host)
        {
            record.host = host;
        }
        batch.remove(kind::RESOURCE, &scope, &old);
        if let Some(spec) = store.read(kind::SPEC, &scope, &old)? {
            batch
                .remove(kind::SPEC, &scope, &old)
                .write(kind::SPEC, &scope, &id, spec);
        }
        resources.insert(old, id.clone());
        values.push((
            kind::RESOURCE,
            scope,
            id,
            String::new(),
            serde_json::to_vec(&record)?,
        ));
        taken.given += 1;
    }
    // A provisioner's node by its id, and the secrets of the names its type
    // declares that it took from those all share: its own copies now.
    let mut copied_from: BTreeSet<String> = BTreeSet::new();
    let owned_secrets: BTreeSet<String> = rows
        .iter()
        .filter(|row| row.kind == kind::SECRET)
        .map(|row| row.name.clone())
        .collect();
    let global = |ids: &BTreeMap<(&str, String, String), String>, name: &str| {
        id_of(ids, kind::SECRET, OWN, name)
    };
    let mut provisioner_values = vec![];
    for (kind, scope, id, name, value) in values.iter_mut() {
        if *kind != kind::PROVISIONER_ENTRY {
            continue;
        }
        let Ok(mut entry) = serde_json::from_slice::<crate::provisioner::Entry>(value) else {
            continue;
        };
        if let Some(node) = id_of(&ids, kind::PROFILE, scope, &entry.node) {
            entry.node = node;
        }
        *value = serde_json::to_vec(&entry)?;
        provisioner_values.push((id.clone(), name.clone(), entry.kind.clone(), scope.clone()));
    }
    for (_, id, label) in labelled(kind::PROVISIONER_ENTRY) {
        if let Some(value) = store.read(kind::PROVISIONER_ENTRY, OWN, &id)?
            && let Ok(entry) = serde_json::from_slice::<crate::provisioner::Entry>(&value)
        {
            provisioner_values.push((id, label, entry.kind, OWN.to_owned()));
        }
    }
    for (id, name, kind_of, scope) in &provisioner_values {
        let Ok((_, declared)) = crate::provision::find(plugins, kind_of) else {
            continue;
        };
        for secret in &declared.secrets {
            let own = format!("{id}/{}", secret.name);
            let had = owned_secrets.contains(&format!("provisioner/{name}/{}", secret.name))
                || owned_secrets.contains(&own);
            if had {
                continue;
            }
            let Some(value) = store.read(kind::SECRET, scope, &secret.name)?.or(
                match global(&ids, &secret.name) {
                    Some(global) => store.read(kind::SECRET, scope, &global)?,
                    None => None,
                },
            ) else {
                continue;
            };
            batch.write(kind::SECRET, scope, &own, value);
            taken.copied.push(format!("{name}: {}", secret.name));
            copied_from.insert(secret.name.clone());
        }
    }
    // Profiles refer to registries, secrets and keys by id.
    let mut profiles: Vec<(String, String, Value)> = vec![];
    for (kind, scope, id, _, value) in &values {
        if *kind == kind::PROFILE
            && let Ok(profile) = serde_json::from_slice::<Value>(value)
        {
            profiles.push((scope.clone(), id.clone(), profile));
        }
    }
    for (scope, id, _) in labelled(kind::PROFILE) {
        if let Some(profile) = store.read_json(kind::PROFILE, &scope, &id)? {
            profiles.push((scope, id, profile));
        }
    }
    let mut handed: BTreeSet<String> = BTreeSet::new();
    let mut converted_profiles: BTreeMap<(String, String), Value> = BTreeMap::new();
    for (scope, id, mut profile) in profiles {
        for entry in profile["secrets"].as_array().into_iter().flatten() {
            if let Some(name) = entry["name"].as_str() {
                handed.insert(name.to_owned());
            }
        }
        let mut changed = false;
        if let Some(secrets) = profile.get_mut("secrets").and_then(Value::as_array_mut) {
            for entry in secrets {
                if let Some(name) = entry["name"].as_str()
                    && let Some(found) = id_of(&ids, kind::SECRET, OWN, name)
                {
                    entry["name"] = json!(found);
                    changed = true;
                }
            }
        }
        if let Some(registries) = profile.get_mut("registries").and_then(Value::as_array_mut) {
            for name in registries {
                if let Some(found) = name
                    .as_str()
                    .and_then(|name| id_of(&ids, kind::REGISTRY, OWN, name))
                {
                    *name = json!(found);
                    changed = true;
                }
            }
        }
        if let Some(auth) = profile.pointer_mut("/access/auth")
            && let Some(key) = auth.as_str().and_then(|text| text.strip_prefix("key:"))
            && let Some(found) = id_of(&ids, kind::KEY, OWN, key)
        {
            *auth = json!(format!("key:{found}"));
            changed = true;
        }
        if changed {
            converted_profiles.insert((scope, id), profile);
        }
    }
    for (kind, scope, id, _, value) in values.iter_mut() {
        if *kind == kind::PROFILE
            && let Some(profile) = converted_profiles.remove(&(scope.clone(), id.clone()))
        {
            *value = profile.to_string().into_bytes();
        }
    }
    for ((scope, id), profile) in converted_profiles {
        batch.write(kind::PROFILE, &scope, &id, profile.to_string().into_bytes());
    }
    // A shared secret a provisioner had through the name it declares goes
    // when nothing else uses it: no profile hands it to a host, and no
    // feature makes nodes trust it.
    let trusted: BTreeSet<String> = plugins
        .iter()
        .flat_map(crate::plugins::hosting)
        .filter_map(|hosting| hosting.trusts)
        .flat_map(|(cert, key)| [cert, key])
        .collect();
    for name in &copied_from {
        if handed.contains(name) || trusted.contains(name) {
            continue;
        }
        if let Some(index) = values.iter().position(|(kind, scope, _, label, _)| {
            *kind == kind::SECRET && scope == OWN && label == name
        }) {
            values.remove(index);
            taken.given -= 1;
        } else if let Some(id) = global(&ids, name) {
            batch.remove(kind::SECRET, OWN, &id);
        }
        taken.dropped.push(name.clone());
    }
    for (kind, scope, id, label, value) in values {
        match label.is_empty() {
            true => batch.write(kind, &scope, &id, value),
            false => batch.write_labelled(kind, &scope, &id, &label, value),
        };
    }
    // Jobs, messages and cursors, by the ids of what they are about.
    let host_id = |name: &str| id_of(&ids, kind::PROFILE, OWN, name);
    if let Rows::Jobs(jobs) = store.export(Table::Jobs)? {
        for job in jobs {
            let subject = match crate::runner::Subject::parse(&job.subject) {
                Some(crate::runner::Subject::Host(name)) => {
                    host_id(&name).map(|id| format!("host:{id}"))
                }
                Some(crate::runner::Subject::Provisioner(name)) => {
                    id_of(&ids, kind::PROVISIONER_ENTRY, OWN, &name)
                        .map(|id| format!("provisioner:{id}"))
                }
                Some(crate::runner::Subject::Resource(old)) => {
                    resources.get(&old).map(|id| format!("resource:{id}"))
                }
                _ => None,
            };
            let hosts: Vec<String> = job
                .host
                .split(", ")
                .filter(|host| !host.is_empty())
                .map(|host| host_id(host).unwrap_or_else(|| host.to_owned()))
                .collect();
            let hosts = hosts.join(", ");
            if subject.is_some() || hosts != job.host {
                batch.set_job(&job.id, &hosts, &subject.unwrap_or(job.subject));
            }
        }
    }
    let mut seen_hosts = BTreeSet::new();
    if let Rows::Messages(messages) = store.export(Table::Messages)? {
        seen_hosts.extend(messages.into_iter().map(|message| message.host));
    }
    if let Rows::Cursors(cursors) = store.export(Table::Cursors)? {
        seen_hosts.extend(cursors.into_iter().map(|cursor| cursor.host));
    }
    for host in seen_hosts {
        if let Some(id) = host_id(&host) {
            batch.move_host(&host, &id);
        }
    }
    let marker =
        json!({"version": berth_api::VERSION, "at": berth_api::rfc3339(berth_api::unix_now())});
    batch.write(kind::META, OWN, IDS_MARKER, marker.to_string().into_bytes());
    store.apply(&batch)?;
    Ok(taken)
}

/// The seventh import, in one change with its marker: every stored profile,
/// of every scope, takes the names of now ([`berth_api::profile::take_old_names`]).
/// The values may be sealed, so this is done here rather than by a migration
/// of the database. Returns how many profiles changed.
pub(crate) fn take_in_names(store: &Store) -> Result<usize> {
    let mut scopes = std::collections::BTreeSet::new();
    if let crate::store::Rows::State(rows) = store.export(crate::store::Table::State)? {
        for row in rows.into_iter().filter(|row| row.kind == kind::PROFILE) {
            scopes.insert(row.scope);
        }
    }
    let mut writes: Vec<(String, String, Vec<u8>)> = vec![];
    for scope in scopes {
        for (name, value) in store.entries(kind::PROFILE, &scope)? {
            let Ok(mut config) = serde_json::from_slice::<Value>(&value) else {
                continue;
            };
            if berth_api::profile::take_old_names(&mut config) {
                writes.push((scope.clone(), name, config.to_string().into_bytes()));
            }
        }
    }
    let renamed = writes.len();
    let marker =
        json!({"version": berth_api::VERSION, "at": berth_api::rfc3339(berth_api::unix_now())});
    let marker = marker.to_string().into_bytes();
    let mut rows: Vec<(&str, &str, &str, Vec<u8>)> = writes
        .iter()
        .map(|(scope, name, value)| (kind::PROFILE, scope.as_str(), name.as_str(), value.clone()))
        .collect();
    rows.push((kind::META, OWN, NAMES_MARKER, marker));
    store.change(&rows, &[])?;
    Ok(renamed)
}

/// The hosts whose profiles the tailnet's import changed: those that held a
/// pasted key, and those berth 0.5 never joined by itself.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct TakenTailnet {
    pub pasted: Vec<String>,
    pub optional: Vec<String>,
}

/// The role's variable that lets an apply go on, without the tailnet, when
/// berth cannot make the host's key.
pub const TAILNET_OPTIONAL: &str = "devbox_tailnet_optional";

/// The eighth import, in one change with its marker: every stored profile,
/// of every scope, loses its pasted enrollment key and the switch of keys
/// berth makes ([`berth_api::profile::take_old_tailnet`]), so no key lingers
/// in the store. berth 0.5 joined a host only with a pasted key or with that
/// switch on, which was off unless set, and an apply without either left the
/// host off the tailnet. A profile of such a host keeps that: it joins once
/// berth can make its key, and until then an apply goes on without it. Says
/// which hosts of berth's own scope it changed.
pub(crate) fn take_in_tailnet(store: &Store) -> Result<TakenTailnet> {
    let mut scopes = std::collections::BTreeSet::new();
    if let crate::store::Rows::State(rows) = store.export(crate::store::Table::State)? {
        for row in rows.into_iter().filter(|row| row.kind == kind::PROFILE) {
            scopes.insert(row.scope);
        }
    }
    let mut taken = TakenTailnet::default();
    let mut writes: Vec<(String, String, Vec<u8>)> = vec![];
    for scope in scopes {
        for (name, value) in store.entries(kind::PROFILE, &scope)? {
            let Ok(mut config) = serde_json::from_slice::<Value>(&value) else {
                continue;
            };
            if !config.is_object() {
                continue;
            }
            let joins = berth_api::profile::joins_tailnet(&config);
            let old = berth_api::profile::take_old_tailnet(&mut config);
            let optional = joins
                && old.is_none_or(|old| old.by_hand && !old.pasted)
                && config["ansible_vars"].get(TAILNET_OPTIONAL).is_none();
            if optional {
                if !config["ansible_vars"].is_object() {
                    config["ansible_vars"] = json!({});
                }
                config["ansible_vars"][TAILNET_OPTIONAL] = json!(true);
            }
            if old.is_none() && !optional {
                continue;
            }
            if scope == OWN {
                match (old.is_some_and(|old| old.pasted), optional) {
                    (true, _) => taken.pasted.push(name.clone()),
                    (false, true) => taken.optional.push(name.clone()),
                    _ => {}
                }
            }
            writes.push((scope.clone(), name, config.to_string().into_bytes()));
        }
    }
    let marker =
        json!({"version": berth_api::VERSION, "at": berth_api::rfc3339(berth_api::unix_now())});
    let marker = marker.to_string().into_bytes();
    let mut rows: Vec<(&str, &str, &str, Vec<u8>)> = writes
        .iter()
        .map(|(scope, name, value)| (kind::PROFILE, scope.as_str(), name.as_str(), value.clone()))
        .collect();
    rows.push((kind::META, OWN, TAILNET_MARKER, marker));
    store.change(&rows, &[])?;
    Ok(taken)
}

/// What the tailnet's import tells the user, once, or `None` when it
/// changed nothing they need to know.
pub(crate) fn tailnet_message(taken: &TakenTailnet, store: &Store) -> Result<Option<String>> {
    if taken.pasted.is_empty() && taken.optional.is_empty() {
        return Ok(None);
    }
    let list = |names: &[String]| names.join(", ");
    let joins = |names: &[String]| if names.len() == 1 { "joins" } else { "join" };
    let has_key = store
        .names(kind::SECRET, OWN)?
        .iter()
        .any(|name| name == berth_api::profile::TAILNET_API_KEY);
    let secret = berth_api::profile::TAILNET_API_KEY;
    let mut said = String::from("berth makes the keys hosts join the tailnet with itself now.");
    if !taken.pasted.is_empty() {
        said += &format!(
            " The keys pasted into the profiles of {0} are removed. {0} {1} the tailnet on the next apply, unless already on it",
            list(&taken.pasted),
            joins(&taken.pasted)
        );
        said += &match has_key {
            true => ".".to_owned(),
            false => format!(", once the secret {secret} is set: berth secret set {secret}."),
        };
    }
    if !taken.optional.is_empty() {
        let names = list(&taken.optional);
        said += &match has_key {
            true => format!(
                " {names} {} the tailnet on the next apply, unless already on it.",
                joins(&taken.optional)
            ),
            false => format!(
                " {names} {} the tailnet once the secret {secret} is set: berth secret set {secret}. Until then an apply leaves {} off the tailnet, as berth 0.5 did.",
                joins(&taken.optional),
                if taken.optional.len() == 1 {
                    "it"
                } else {
                    "them"
                }
            ),
        };
    }
    Ok(Some(said))
}

/// The sixth import, in one change with its marker: each type of provisioner
/// that has settings, or that made a resource of any scope, is registered
/// under a name of its own, such as `incus-vm` for `incus/vm`, with those
/// settings. Its resources name it by that name. Returns the names.
pub(crate) fn take_in_provisioners(store: &Store) -> Result<Vec<String>> {
    use crate::provisioner::{Entry, default_name};
    let mut types: std::collections::BTreeMap<String, Entry> = Default::default();
    let mut writes: Vec<(&'static str, String, String, Vec<u8>)> = vec![];
    let mut removals: Vec<(&'static str, String, String)> = vec![];
    for (kind, value) in store.entries(kind::PROVISIONER, OWN)? {
        removals.push((kind::PROVISIONER, OWN.into(), kind.clone()));
        let settings = serde_json::from_slice(&value).unwrap_or_default();
        types.insert(
            kind.clone(),
            Entry {
                kind,
                settings,
                ..Entry::default()
            },
        );
    }
    let mut scopes = std::collections::BTreeSet::new();
    if let crate::store::Rows::State(rows) = store.export(crate::store::Table::State)? {
        for row in rows.into_iter().filter(|row| row.kind == kind::RESOURCE) {
            scopes.insert(row.scope);
        }
    }
    for scope in scopes {
        for (id, value) in store.entries(kind::RESOURCE, &scope)? {
            let Ok(mut record) = serde_json::from_slice::<crate::resource::Record>(&value) else {
                continue;
            };
            if !record.provisioner.contains('/') {
                continue;
            }
            types
                .entry(record.provisioner.clone())
                .or_insert_with(|| Entry {
                    kind: record.provisioner.clone(),
                    ..Entry::default()
                });
            record.provisioner = default_name(&record.provisioner);
            writes.push((
                kind::RESOURCE,
                scope.clone(),
                id,
                serde_json::to_vec(&record)?,
            ));
        }
    }
    let taken = store.names(kind::PROVISIONER_ENTRY, OWN)?;
    let mut registered = vec![];
    for (kind, entry) in types {
        let name = default_name(&kind);
        // One registered under that name already keeps what it has.
        if taken.contains(&name) {
            continue;
        }
        writes.push((
            kind::PROVISIONER_ENTRY,
            OWN.into(),
            name.clone(),
            serde_json::to_vec(&entry)?,
        ));
        registered.push(name);
    }
    let marker =
        json!({"version": berth_api::VERSION, "at": berth_api::rfc3339(berth_api::unix_now())});
    writes.push((
        kind::META,
        OWN.into(),
        PROVISIONERS_MARKER.into(),
        marker.to_string().into_bytes(),
    ));
    let writes: Vec<(&str, &str, &str, Vec<u8>)> = writes
        .iter()
        .map(|(kind, scope, name, value)| (*kind, scope.as_str(), name.as_str(), value.clone()))
        .collect();
    let removals: Vec<(&str, &str, &str)> = removals
        .iter()
        .map(|(kind, scope, name)| (*kind, scope.as_str(), name.as_str()))
        .collect();
    store.change(&writes, &removals)?;
    Ok(registered)
}

/// What berth before 0.5 kept of a host's machine, beside its profile.
#[derive(serde::Deserialize)]
struct Instance {
    provisioner: String,
    #[serde(default)]
    id: String,
    token: String,
    #[serde(default)]
    arguments: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    created: String,
}

/// The fifth import, in one change with its marker: each record of a host's
/// machine, of every scope, becomes a resource of the machine kind in the
/// same scope, keeping its token and the provisioner's id. Returns how many.
pub(crate) fn take_in_resources(store: &Store) -> Result<usize> {
    let mut scopes = std::collections::BTreeSet::new();
    if let crate::store::Rows::State(rows) = store.export(crate::store::Table::State)? {
        for row in rows.into_iter().filter(|row| row.kind == kind::INSTANCE) {
            scopes.insert(row.scope);
        }
    }
    let mut writes: Vec<(&'static str, String, String, Vec<u8>)> = vec![];
    let mut removals: Vec<(&'static str, String, String)> = vec![];
    for scope in scopes {
        for (host, value) in store.entries(kind::INSTANCE, &scope)? {
            removals.push((kind::INSTANCE, scope.clone(), host.clone()));
            let Ok(old) = serde_json::from_slice::<Instance>(&value) else {
                eprintln!(
                    "berth: the record of {host}'s machine is not one berth reads, and is left out"
                );
                continue;
            };
            let name = match scope.as_str() {
                crate::profiles::FILE_SCOPE => Path::new(&host)
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
                    .unwrap_or_else(|| host.clone()),
                _ => host.clone(),
            };
            let record = crate::resource::Record {
                id: crate::resource::new_id()?,
                provisioner: old.provisioner,
                kind: crate::plugins::MACHINE.into(),
                name,
                external_id: old.id,
                host: host.clone(),
                token: old.token,
                arguments: old.arguments,
                created: old.created,
                ..crate::resource::Record::default()
            };
            writes.push((
                kind::RESOURCE,
                scope.clone(),
                record.id.clone(),
                serde_json::to_vec(&record)?,
            ));
        }
    }
    let converted = writes.len();
    let marker =
        json!({"version": berth_api::VERSION, "at": berth_api::rfc3339(berth_api::unix_now())});
    writes.push((
        kind::META,
        OWN.into(),
        RESOURCES_MARKER.into(),
        marker.to_string().into_bytes(),
    ));
    let writes: Vec<(&str, &str, &str, Vec<u8>)> = writes
        .iter()
        .map(|(kind, scope, name, value)| (*kind, scope.as_str(), name.as_str(), value.clone()))
        .collect();
    let removals: Vec<(&str, &str, &str)> = removals
        .iter()
        .map(|(kind, scope, name)| (*kind, scope.as_str(), name.as_str()))
        .collect();
    store.change(&writes, &removals)?;
    Ok(converted)
}

/// What becomes of a profile's access fields: the rows to write besides the
/// profile, and what to tell the user.
#[derive(Default)]
pub(crate) struct Converted {
    /// Keys taken into the ring, by name.
    pub keys: Vec<(String, crate::keys::ring::Halves)>,
    pub notices: Vec<String>,
}

/// Move a profile's access fields from `builder` into `access`, in place.
/// `served` is a profile of a berth server's host, which the server logs in
/// to with its own key. A key file the profile named becomes a key of the
/// ring when it can be read; `ring` is what the ring has, with what this
/// conversion added so far. Returns whether the profile changed.
pub(crate) fn convert_access(
    paths: &Paths,
    title: &str,
    config: &mut Value,
    served: bool,
    ring: &mut Vec<(String, String)>,
    converted: &mut Converted,
) -> bool {
    use berth_api::profile::{Access, Auth};
    let Some(old) = berth_api::profile::take_builder_access(config) else {
        return false;
    };
    let auth = if !config["access"]["auth"].is_null() {
        return true;
    } else if old.ask_pass {
        Auth::Password
    } else if served {
        Auth::Berth
    } else if old.key_file.is_empty() {
        // What every connection offered: the user's own keys and berth's.
        Auth::Ssh
    } else {
        let file = crate::discovery::expand_path(&old.key_file);
        let taken = fs::read(&file)
            .map_err(|error| error.to_string().into())
            .and_then(|data| crate::keys::ring::validate(paths, &data));
        match taken {
            Ok(halves) => {
                let blob = crate::keys::ring::blob(&halves.public);
                let name = match ring.iter().find(|(_, public)| *public == blob) {
                    Some((name, _)) => name.clone(),
                    None => {
                        let stem: String = file
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_default()
                            .chars()
                            .map(
                                |ch| match ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                                    true => ch,
                                    false => '-',
                                },
                            )
                            .collect();
                        let stem = stem.trim_matches('-').to_owned();
                        let stem = match stem.as_str() {
                            "" | "berth" => "key".to_owned(),
                            _ => stem.chars().take(48).collect(),
                        };
                        let mut name = stem.clone();
                        let mut number = 2;
                        while ring.iter().any(|(known, _)| *known == name) {
                            name = format!("{stem}-{number}");
                            number += 1;
                        }
                        ring.push((name.clone(), blob));
                        converted.keys.push((name.clone(), halves));
                        converted.notices.push(format!(
                            "took the key file {} of {title} into the key ring as {name}",
                            file.display()
                        ));
                        name
                    }
                };
                Auth::Key(name)
            }
            Err(error) => {
                converted.notices.push(format!(
                    "{title} named the key file {}, which berth could not take into its key ring ({error}). It logs in with your SSH configuration now. Add the key with: berth key import NAME FILE, and choose it as the host's login",
                    file.display()
                ));
                Auth::Ssh
            }
        }
    };
    let mut access = Access::of(config);
    access.auth = auth;
    let sudo = config["access"]["sudo"].clone();
    access.write(config);
    if sudo.is_null()
        && let Some(object) = config["access"].as_object_mut()
    {
        object.remove("sudo");
    }
    true
}

/// The host keys a known-hosts file such as `~/.ssh/known_hosts` has for a
/// host at `address` and `port`.
fn known_host_keys(known_hosts: &Path, address: &str, port: &str) -> Vec<String> {
    let names = match port {
        "" | "22" => vec![address.to_owned()],
        port => vec![format!("[{address}]:{port}")],
    };
    let text = fs::read_to_string(known_hosts).unwrap_or_default();
    for name in names {
        let keys = crate::keys::known::lookup(&text, &name);
        if !keys.is_empty() {
            return keys;
        }
    }
    vec![]
}

/// The fourth import, in one change with its marker: berth's own key from
/// the rows `ssh` and `ssh.pub` into the ring as `berth`, and the access
/// fields of every stored profile, of every scope, into `access`. A host of
/// this installation whose key `known_hosts` has keeps that key. Returns
/// what to tell the user.
pub(crate) fn take_in_access(
    paths: &Paths,
    store: &Store,
    known_hosts: Option<&Path>,
) -> Result<Vec<String>> {
    use crate::keys::ring::{Halves, blob};
    let mut writes: Vec<(&'static str, String, String, Vec<u8>)> = vec![];
    let mut removals: Vec<(&'static str, String, String)> = vec![];
    let mut converted = Converted::default();
    let names = store.names(kind::KEY, OWN)?;
    let has = |name: &str| names.iter().any(|known| known == name);
    if has("ssh") && has("ssh.pub") && !has(berth_api::profile::BERTH_KEY) {
        let private = store.read(kind::KEY, OWN, "ssh")?.unwrap_or_default();
        let public = store.read(kind::KEY, OWN, "ssh.pub")?.unwrap_or_default();
        let halves = Halves {
            private: String::from_utf8_lossy(&private).into_owned(),
            public: String::from_utf8_lossy(&public).trim().to_owned(),
        };
        writes.push((
            kind::KEY,
            OWN.into(),
            berth_api::profile::BERTH_KEY.into(),
            serde_json::to_vec(&halves)?,
        ));
    }
    if has("ssh") || has("ssh.pub") {
        removals.push((kind::KEY, OWN.into(), "ssh".into()));
        removals.push((kind::KEY, OWN.into(), "ssh.pub".into()));
    }
    // What the ring has, by the type and base64 of each public half.
    let mut ring: Vec<(String, String)> = vec![];
    for (name, value) in store.entries(kind::KEY, OWN)? {
        if let Ok(halves) = serde_json::from_slice::<Halves>(&value) {
            ring.push((name, blob(&halves.public)));
        }
    }
    let mut scopes = std::collections::BTreeSet::new();
    if let crate::store::Rows::State(rows) = store.export(crate::store::Table::State)? {
        for row in rows.into_iter().filter(|row| row.kind == kind::PROFILE) {
            scopes.insert(row.scope);
        }
    }
    for scope in scopes {
        for (name, value) in store.entries(kind::PROFILE, &scope)? {
            let Ok(mut config) = serde_json::from_slice::<Value>(&value) else {
                continue;
            };
            let served =
                crate::store::served_scope(&scope) || config["builder"]["served"].is_string();
            if !convert_access(paths, &name, &mut config, served, &mut ring, &mut converted) {
                continue;
            }
            if let (false, Some(file)) = (served, known_hosts) {
                let access = berth_api::profile::Access::of(&config);
                let keys = match access.address.is_empty() {
                    true => vec![],
                    false => known_host_keys(file, &access.address, &access.port),
                };
                if !keys.is_empty() {
                    let record = crate::keys::known::Record {
                        host_keys: keys,
                        ..Default::default()
                    };
                    writes.push((
                        kind::ACCESS,
                        scope.clone(),
                        name.clone(),
                        serde_json::to_vec(&record)?,
                    ));
                }
            }
            writes.push((
                kind::PROFILE,
                scope.clone(),
                name,
                config.to_string().into_bytes(),
            ));
        }
    }
    for (name, halves) in &converted.keys {
        writes.push((
            kind::KEY,
            OWN.into(),
            name.clone(),
            serde_json::to_vec(halves)?,
        ));
    }
    let marker =
        json!({"version": berth_api::VERSION, "at": berth_api::rfc3339(berth_api::unix_now())});
    writes.push((
        kind::META,
        OWN.into(),
        ACCESS_MARKER.into(),
        marker.to_string().into_bytes(),
    ));
    let writes: Vec<(&str, &str, &str, Vec<u8>)> = writes
        .iter()
        .map(|(kind, scope, name, value)| (*kind, scope.as_str(), name.as_str(), value.clone()))
        .collect();
    let removals: Vec<(&str, &str, &str)> = removals
        .iter()
        .map(|(kind, scope, name)| (*kind, scope.as_str(), name.as_str()))
        .collect();
    store.change(&writes, &removals)?;
    Ok(converted.notices)
}

/// A profile of an older layout as it is read or given: with the names its
/// fields have now, and of the access layout, its key file taken into the
/// ring. A file is written only when the user saves it.
pub fn convert_profile(paths: &Paths, title: &str, config: &mut Value) {
    berth_api::profile::take_old_names(config);
    convert_layout(paths, title, config);
}

/// A profile of the layout before `access`, in the access layout, its key
/// file taken into the ring.
pub fn convert_layout(paths: &Paths, title: &str, config: &mut Value) {
    use crate::keys::ring::{Halves, blob};
    if config["builder"].as_object().is_none_or(|builder| {
        ![
            "ssh_host",
            "ssh_user",
            "ssh_port",
            "ssh_key",
            "ask_become_pass",
            "ask_pass",
            "link",
        ]
        .iter()
        .any(|key| builder.contains_key(*key))
    }) {
        return;
    }
    let store = crate::store::shared(paths).ok();
    let mut ring: Vec<(String, String)> = store
        .as_ref()
        .and_then(|store| store.entries(kind::KEY, OWN).ok())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(name, value)| {
            let halves = serde_json::from_slice::<Halves>(&value).ok()?;
            Some((name, blob(&halves.public)))
        })
        .collect();
    let mut converted = Converted::default();
    let served = config["builder"]["served"].is_string();
    convert_access(paths, title, config, served, &mut ring, &mut converted);
    let Some(store) = store else {
        return;
    };
    for (name, halves) in &converted.keys {
        if let Ok(value) = serde_json::to_vec(halves)
            && let Err(error) = store.write(kind::KEY, OWN, name, &value)
        {
            eprintln!("berth: the key {name} was not taken into the key ring: {error}");
        }
    }
    for notice in converted.notices {
        eprintln!("berth: {notice}");
    }
}

/// Where berth 0.4 kept the setup key of `berth serve`.
fn setup_key_file(paths: &Paths) -> PathBuf {
    paths.data_dir.join("serve.setup-key")
}

/// Write berth's own key, the token, the setup key and the plugins from
/// their files with the marker, in one change. A row the store has already
/// stays as it is. Returns what was taken, in words.
pub(crate) fn take_in_keys(paths: &Paths, store: &Store) -> Result<Vec<&'static str>> {
    let mut rows: Vec<(&str, String, Vec<u8>)> = vec![];
    let mut taken = vec![];
    // Names only, as in the first import.
    let has = |kind: &str, name: &str| -> Result<bool> {
        Ok(store.names(kind, OWN)?.iter().any(|known| known == name))
    };
    // berth 0.4 kept its own key in data_dir/id_ed25519; it becomes the key
    // berth of the ring, unless the store has one.
    let own = paths.data_dir.join("id_ed25519");
    if !has(kind::KEY, berth_api::profile::BERTH_KEY)?
        && !has(kind::KEY, "ssh")?
        && let (Ok(private), Ok(public)) = (fs::read(&own), fs::read(own.with_extension("pub")))
        && !private.is_empty()
        && !public.is_empty()
    {
        let halves = crate::keys::ring::Halves {
            private: String::from_utf8_lossy(&private).into_owned(),
            public: String::from_utf8_lossy(&public).trim().to_owned(),
        };
        rows.push((
            kind::KEY,
            berth_api::profile::BERTH_KEY.to_owned(),
            serde_json::to_vec(&halves)?,
        ));
        taken.push("berth's own SSH key");
    }
    for (kept, what) in [(crate::kept::serve_token(paths), "the token of berth serve")] {
        let Some(values) = kept.from_files()? else {
            continue;
        };
        let mut added = false;
        for (kind, name, value) in kept.rows(&values) {
            if !has(kind, name)? {
                rows.push((kind, name.to_owned(), value));
                added = true;
            }
        }
        if added {
            taken.push(what);
        }
    }
    let file = setup_key_file(paths);
    if !has(kind::TOKEN, crate::kept::SETUP_KEY)?
        && let Ok(data) = fs::read(&file)
        && serde_json::from_slice::<Value>(&data).is_ok()
    {
        rows.push((
            kind::TOKEN,
            crate::kept::SETUP_KEY.to_owned(),
            data.trim_ascii().to_vec(),
        ));
        taken.push("its setup key");
    }
    if store.names(kind::PLUGIN, OWN)?.is_empty() {
        let own = crate::plugins::own_dir(&paths.config_dir);
        // The plugins are written to the store at their next change too, so
        // a folder that cannot be packed now does not hold up the rest.
        match crate::plugin_state::snapshot(&own) {
            Ok(plugins) if !plugins.is_empty() => {
                rows.extend(
                    plugins
                        .into_iter()
                        .map(|(name, value)| (kind::PLUGIN, name, value)),
                );
                taken.push("the list of your own plugins");
            }
            Ok(_) => {}
            Err(error) => eprintln!(
                "berth: your own plugins in {} are not in berth's store yet: {error}",
                own.display()
            ),
        }
    }
    let marker =
        json!({"version": berth_api::VERSION, "at": berth_api::rfc3339(berth_api::unix_now())});
    rows.push((
        kind::META,
        KEYS_MARKER.to_owned(),
        marker.to_string().into_bytes(),
    ));
    let writes: Vec<(&str, &str, &str, Vec<u8>)> = rows
        .iter()
        .map(|(kind, name, value)| (*kind, OWN, name.as_str(), value.clone()))
        .collect();
    store.change(&writes, &[])?;
    Ok(taken)
}

/// The counts in words.
fn said(counts: &Counts) -> String {
    let plural = |count: usize, one: &str, more: &str| match count {
        1 => format!("1 {one}"),
        count => format!("{count} {more}"),
    };
    let mut parts: Vec<String> = [
        (counts.secrets, "secret", "secrets"),
        (counts.registries, "registry", "registries"),
        (counts.settings, "setting", "settings"),
    ]
    .into_iter()
    .filter(|(count, ..)| *count > 0)
    .map(|(count, one, more)| plural(count, one, more))
    .collect();
    let mut text = match parts.len() {
        0 => "nothing new".to_owned(),
        1 => parts.remove(0),
        _ => {
            let last = parts.pop().unwrap_or_default();
            format!("{} and {last}", parts.join(", "))
        }
    };
    if counts.kept > 0 {
        text += &format!(
            " (and left {} as the store had {})",
            plural(counts.kept, "value", "values"),
            if counts.kept == 1 { "it" } else { "them" }
        );
    }
    text
}

/// Held while importing, so that two berths starting at once do not both do it.
fn lock(paths: &Paths) -> Result<fs::File> {
    wait_lock(
        &paths.data_dir.join("import.lock"),
        "taking in the old settings files",
    )
}

fn read_map(file: &Path) -> Result<Option<Map<String, Value>>> {
    match fs::read(file) {
        Ok(data) => serde_json::from_slice(&data)
            .map(Some)
            .map_err(|error| format!("{}: {error}", file.display()).into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("{}: {error}", file.display()).into()),
    }
}

/// A key of `config.json` that moves to the store.
fn moves(key: &str) -> bool {
    key == "registries" || (SETTINGS.iter().any(|(known, _)| *known == key) && !is_local(key))
}

/// Whether the old files still hold what moves to the store.
fn leftover(paths: &Paths) -> Result<bool> {
    if paths.config_dir.join("secrets.json").exists() {
        return Ok(true);
    }
    let config = read_map(&paths.settings_file())?.unwrap_or_default();
    Ok(config.keys().any(|key| moves(key)))
}

fn text(value: &Value) -> Vec<u8> {
    match value {
        Value::String(text) => text.clone().into_bytes(),
        other => other.to_string().into_bytes(),
    }
}

/// Write the rows of the old files and the marker in one change. What the
/// store has already stays as it is.
pub(crate) fn take_in(paths: &Paths, store: &Store) -> Result<Counts> {
    let config = read_map(&paths.settings_file())?.unwrap_or_default();
    let secrets = read_map(&paths.config_dir.join("secrets.json"))?.unwrap_or_default();
    let registries = config
        .get("registries")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let settings = config
        .iter()
        .filter(|(key, _)| *key != "registries" && moves(key));
    let mut counts = Counts::default();
    let mut rows: Vec<(&str, String, Vec<u8>)> = vec![];
    let mut add = |kind: &'static str, name: &str, value: Vec<u8>, known: &[String]| {
        if known.iter().any(|known| known == name) {
            counts.kept += 1;
            return false;
        }
        rows.push((kind, name.to_owned(), value));
        true
    };
    let known = store.names(kind::SECRET, OWN)?;
    let mut added = (0, 0, 0);
    for (name, value) in &secrets {
        added.0 += add(kind::SECRET, name, text(value), &known) as usize;
    }
    let known = store.names(kind::REGISTRY, OWN)?;
    for (name, registry) in &registries {
        added.1 += add(
            kind::REGISTRY,
            name,
            registry.to_string().into_bytes(),
            &known,
        ) as usize;
    }
    let known = store.names(kind::SETTING, OWN)?;
    for (key, value) in settings {
        added.2 += add(kind::SETTING, key, text(value), &known) as usize;
    }
    let marker =
        json!({"version": berth_api::VERSION, "at": berth_api::rfc3339(berth_api::unix_now())});
    rows.push((
        kind::META,
        MARKER.to_owned(),
        marker.to_string().into_bytes(),
    ));
    let writes: Vec<(&str, &str, &str, Vec<u8>)> = rows
        .iter()
        .map(|(kind, name, value)| (*kind, OWN, name.as_str(), value.clone()))
        .collect();
    store.change(&writes, &[])?;
    (counts.secrets, counts.registries, counts.settings) = added;
    Ok(counts)
}

/// A name in `dir` that is not taken: `name`, or with a number before its extension.
fn free_name(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let numbered = |number: usize| match name.rsplit_once('.') {
        Some((stem, extension)) => format!("{stem}.{number}.{extension}"),
        None => format!("{name}.{number}"),
    };
    (1..)
        .map(|number| dir.join(numbered(number)))
        .find(|candidate| !candidate.exists())
        .unwrap_or(candidate)
}

/// Put the old files away in `migrated/` and leave in `config.json` what
/// stays there. Returns the folder when anything was put there.
pub(crate) fn put_away(paths: &Paths) -> Result<Option<PathBuf>> {
    let dir = paths.config_dir.join("migrated");
    let make_dir = || -> Result<()> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        Ok(())
    };
    let mut moved = false;
    let file = paths.settings_file();
    if let Some(config) = read_map(&file)?
        && config.keys().any(|key| moves(key))
    {
        make_dir()?;
        // The original is copied first: until config.json is rewritten the
        // next start does this again, and only gets a second copy.
        let copy = free_name(&dir, "config.json");
        write_private(&copy, &fs::read(&file)?)?;
        let kept: Map<String, Value> = config.into_iter().filter(|(key, _)| !moves(key)).collect();
        let unknown: Vec<&str> = kept
            .keys()
            .map(String::as_str)
            .filter(|key| !SETTINGS.iter().any(|(known, _)| known == key))
            .collect();
        if !unknown.is_empty() {
            eprintln!(
                "berth: {} keeps {}, which this berth does not know",
                file.display(),
                unknown.join(", ")
            );
        }
        write_private(
            &file,
            (serde_json::to_string_pretty(&kept)? + "\n").as_bytes(),
        )?;
        moved = true;
    }
    let secrets = paths.config_dir.join("secrets.json");
    if secrets.exists() {
        make_dir()?;
        let to = free_name(&dir, "secrets.json");
        fs::rename(&secrets, &to)?;
        fs::set_permissions(&to, fs::Permissions::from_mode(0o600))?;
        moved = true;
    }
    Ok(moved.then_some(dir))
}

/// What the second import took in.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct HostCounts {
    pub profiles: usize,
    pub credentials: usize,
    pub records: usize,
    /// Rows the store had already, which keep the store's value.
    pub kept: usize,
}

/// The hosts in words.
fn said_hosts(counts: &HostCounts) -> String {
    let mut text = match counts.profiles {
        1 => "1 host".to_owned(),
        count => format!("{count} hosts"),
    };
    if counts.kept > 0 {
        text += &format!(
            " (and left {} as the store had {})",
            match counts.kept {
                1 => "1 value".to_owned(),
                count => format!("{count} values"),
            },
            if counts.kept == 1 { "it" } else { "them" }
        );
    }
    text
}

/// The folder of the profiles of berth 0.4 and earlier.
fn hosts_dir(paths: &Paths) -> PathBuf {
    paths.config_dir.join("hosts")
}

/// Whether the old folders are still there.
fn profiles_leftover(paths: &Paths) -> bool {
    hosts_dir(paths).exists() || paths.data_dir.join("served").exists()
}

/// What a file of the old hosts folder holds, by its name: the kind of row
/// and the host's name. Other files, such as the locks, are not taken in.
fn host_file(file: &str) -> Option<(&'static str, &str)> {
    let stem = file.strip_suffix(".json")?;
    let (kind, name) = match (
        stem.strip_suffix(".credentials"),
        stem.strip_suffix(".instance"),
    ) {
        (Some(name), _) => (kind::CREDENTIALS, name),
        (_, Some(name)) => (kind::INSTANCE, name),
        _ => (kind::PROFILE, stem),
    };
    crate::profiles::valid_name(name).then_some((kind, name))
}

/// Write the rows of the old hosts folder and the marker in one change. A
/// file that is not JSON stops it with nothing written; what the store has
/// already stays as it is.
pub(crate) fn take_in_profiles(paths: &Paths, store: &Store) -> Result<HostCounts> {
    let dir = hosts_dir(paths);
    let mut files: Vec<(String, PathBuf)> = match fs::read_dir(&dir) {
        Ok(entries) => entries
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
            .filter_map(|entry| Some((entry.file_name().into_string().ok()?, entry.path())))
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => vec![],
        Err(error) => return Err(format!("{}: {error}", dir.display()).into()),
    };
    files.sort();
    let mut counts = HostCounts::default();
    let mut rows: Vec<(&'static str, String, Vec<u8>)> = vec![];
    // Names only, as in the first import.
    let mut known = std::collections::BTreeSet::new();
    for kind in [kind::PROFILE, kind::CREDENTIALS, kind::INSTANCE] {
        for name in store.names(kind, OWN)? {
            known.insert((kind, name));
        }
    }
    for (file, path) in &files {
        let Some((kind, name)) = host_file(file) else {
            if file.ends_with(".json") && !file.starts_with('.') {
                eprintln!(
                    "berth: {} has no host's name, and is left out",
                    path.display()
                );
            }
            continue;
        };
        let value: Value = serde_json::from_slice(&fs::read(path)?).map_err(|error| {
            format!(
                "{}: {error}. Nothing was taken in; mend or move the file and start berth again",
                path.display()
            )
        })?;
        if known.contains(&(kind, name.to_owned())) {
            counts.kept += 1;
            continue;
        }
        match kind {
            kind::PROFILE => counts.profiles += 1,
            kind::CREDENTIALS => counts.credentials += 1,
            _ => counts.records += 1,
        }
        rows.push((kind, name.to_owned(), value.to_string().into_bytes()));
    }
    let marker =
        json!({"version": berth_api::VERSION, "at": berth_api::rfc3339(berth_api::unix_now())});
    rows.push((
        kind::META,
        PROFILES_MARKER.to_owned(),
        marker.to_string().into_bytes(),
    ));
    let writes: Vec<(&str, &str, &str, Vec<u8>)> = rows
        .iter()
        .map(|(kind, name, value)| (*kind, OWN, name.as_str(), value.clone()))
        .collect();
    store.change(&writes, &[])?;
    Ok(counts)
}

/// Move the old hosts folder to `migrated/`, and remove the profiles a
/// mirror wrote. Returns the folder the hosts went to, when there were any.
pub(crate) fn put_away_profiles(paths: &Paths) -> Result<Option<PathBuf>> {
    let served = paths.data_dir.join("served");
    if served.exists() {
        fs::remove_dir_all(&served).map_err(|error| format!("{}: {error}", served.display()))?;
    }
    let dir = hosts_dir(paths);
    if !dir.exists() {
        return Ok(None);
    }
    let migrated = paths.config_dir.join("migrated");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&migrated)?;
    fs::set_permissions(&migrated, fs::Permissions::from_mode(0o700))?;
    let to = free_name(&migrated, "hosts");
    fs::rename(&dir, &to)?;
    Ok(Some(to))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seal::Key;
    use std::sync::Arc;

    fn scratch() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: dir.path().join("config"),
            data_dir: dir.path().join("data"),
        };
        fs::create_dir_all(&paths.config_dir).unwrap();
        (dir, paths)
    }

    /// The installation of the files only, whatever the environment of the tests holds.
    fn installation(paths: &Paths) -> Installation {
        Installation::with(paths, &|key| paths.setting_with(key, None))
    }

    const CONFIG: &str = r#"{
  "default_host": "studio",
  "server": "http://192.0.2.7:7777",
  "tailnet_user": "agents",
  "colour": "blue",
  "registries": {"nexus": {"type": "nexus", "url": "https://nexus.example", "username": "ci"}}
}"#;
    const SECRETS: &str = r#"{"github": "ghp-made-up", "registry/nexus": "pw"}"#;

    fn old_files(paths: &Paths) {
        fs::write(paths.settings_file(), CONFIG).unwrap();
        fs::write(paths.config_dir.join("secrets.json"), SECRETS).unwrap();
    }

    fn open(paths: &Paths) -> Store {
        let found = installation(paths);
        let store = found.open().unwrap();
        run(paths, &found, &store).unwrap();
        store
    }

    /// A value by the name of what it is: a thing by its name, which the
    /// store keeps as its label; a secret also by the address of one that
    /// belongs to something, such as `registry/NAME`.
    fn read(store: &Store, kind: &str, name: &str) -> Option<String> {
        // A row by name, as before the import of ids, is read as it is.
        if kind == kind::SECRET
            && let Some(value) = crate::registry::read_secret_in(store, OWN, name).unwrap()
        {
            return Some(value);
        }
        let row = store
            .id_of(kind, OWN, name)
            .unwrap()
            .unwrap_or(name.to_owned());
        let value = store.read(kind, OWN, &row).unwrap()?;
        Some(String::from_utf8(value).unwrap())
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn the_old_files_move_into_the_store() {
        let (_dir, paths) = scratch();
        old_files(&paths);
        let store = open(&paths);
        assert_eq!(read(&store, kind::SECRET, "github").unwrap(), "ghp-made-up");
        assert_eq!(read(&store, kind::SECRET, "registry/nexus").unwrap(), "pw");
        let nexus: Value =
            serde_json::from_str(&read(&store, kind::REGISTRY, "nexus").unwrap()).unwrap();
        assert_eq!(nexus["username"], "ci");
        assert_eq!(
            read(&store, kind::SETTING, "default_host").unwrap(),
            "studio"
        );
        assert_eq!(
            read(&store, kind::SETTING, "tailnet_user").unwrap(),
            "agents"
        );
        // Local settings and unknown keys stay in the file, and only there.
        assert_eq!(read(&store, kind::SETTING, "server"), None);
        assert_eq!(read(&store, kind::SETTING, "colour"), None);
        let marker: Value =
            serde_json::from_str(&read(&store, kind::META, MARKER).unwrap()).unwrap();
        assert_eq!(marker["version"], berth_api::VERSION);

        let left = read_map(&paths.settings_file()).unwrap().unwrap();
        assert_eq!(
            left,
            json!({"server": "http://192.0.2.7:7777", "colour": "blue"})
                .as_object()
                .cloned()
                .unwrap()
        );
        assert_eq!(mode(&paths.settings_file()), 0o600);
        assert!(!paths.config_dir.join("secrets.json").exists());
        let migrated = paths.config_dir.join("migrated");
        assert_eq!(mode(&migrated), 0o700);
        assert_eq!(
            fs::read_to_string(migrated.join("config.json")).unwrap(),
            CONFIG
        );
        assert_eq!(
            fs::read_to_string(migrated.join("secrets.json")).unwrap(),
            SECRETS
        );
        assert_eq!(mode(&migrated.join("config.json")), 0o600);
        assert_eq!(mode(&migrated.join("secrets.json")), 0o600);
        // The settings read through Paths come from the store now.
        assert_eq!(
            paths.setting_with("default_host", None).as_deref(),
            Some("studio")
        );

        // Again, nothing changes.
        let before = store.seal_report().unwrap();
        let marker = read(&store, kind::META, MARKER);
        drop(store);
        let store = open(&paths);
        assert_eq!(store.seal_report().unwrap(), before);
        assert_eq!(read(&store, kind::META, MARKER), marker);
        assert_eq!(fs::read_dir(&migrated).unwrap().count(), 2);
        assert_eq!(read_map(&paths.settings_file()).unwrap().unwrap(), left);
    }

    #[test]
    fn an_installation_of_berth_041_comes_in_whole() {
        let (_dir, paths) = scratch();
        // What berth 0.4.1 leaves: the settings of its OAuth client from
        // before it lost it, its own key and token, and a host whose first
        // apply was as an account of the machine's own, with its machine.
        fs::write(
            paths.settings_file(),
            r#"{"default_host": "studio", "oauth_client_id": "berth-cli", "oauth_scope": "openid offline_access",
                "tailnet_login_server": "https://hs.example.org"}"#,
        )
        .unwrap();
        let data = &paths.data_dir;
        fs::create_dir_all(data).unwrap();
        fs::write(data.join("oauth.json"), r#"{"http://192.0.2.7:7777": {}}"#).unwrap();
        fs::write(data.join("id_ed25519"), "private").unwrap();
        fs::write(data.join("id_ed25519.pub"), "ssh-ed25519 AAAA berth@laptop").unwrap();
        let hosts = paths.config_dir.join("hosts");
        fs::create_dir_all(&hosts).unwrap();
        fs::write(
            hosts.join("studio.json"),
            r#"{"hostname": "studio", "profile": "cloud", "operator_username": "ops",
                "builder": {"ssh_host": "192.0.2.10", "ssh_user": "admin", "ssh_key": "",
                            "ask_become_pass": true, "ask_pass": false}}"#,
        )
        .unwrap();
        fs::write(hosts.join("studio.instance.json"), RECORD).unwrap();
        let store = open(&paths);
        // The settings that are not berth's any more stay in the file, with
        // the tokens berth 0.4.1 kept, and nothing of them goes into the store.
        let left = read_map(&paths.settings_file()).unwrap().unwrap();
        assert_eq!(left["oauth_client_id"], "berth-cli");
        assert_eq!(left["oauth_scope"], "openid offline_access");
        assert!(!left.contains_key("default_host"));
        assert!(
            store
                .names(kind::SETTING, OWN)
                .unwrap()
                .iter()
                .all(|name| !name.starts_with("oauth"))
        );
        assert_eq!(
            read(&store, kind::SETTING, "default_host").unwrap(),
            "studio"
        );
        assert_eq!(
            read(&store, kind::SETTING, "tailnet_login_server").unwrap(),
            "https://hs.example.org"
        );
        assert!(data.join("oauth.json").is_file());
        assert!(
            read(&store, kind::KEY, "berth")
                .unwrap()
                .contains("berth@laptop")
        );
        // The host is in the access layout, its sudo still asked for until
        // an apply as the operator settles it.
        let studio = json_row(&store, kind::PROFILE, "studio").unwrap();
        assert_eq!(
            studio["access"],
            json!({"address": "192.0.2.10", "user": "admin", "auth": "ssh", "sudo": "password"})
        );
        assert!(berth_api::profile::asks_become_pass(&studio));
        assert_eq!(studio["target"], "cloud");
        assert!(studio.get("profile").is_none());
        assert_eq!(machine_of(&store, "studio").unwrap().external_id, "vm-1");
        let mut applied = studio.clone();
        applied["access"]["user"] = json!("ops");
        assert!(berth_api::profile::settle_after_apply(&mut applied));
        assert!(!berth_api::profile::asks_become_pass(&applied));
    }

    #[test]
    fn an_import_stopped_after_its_change_is_finished() {
        let (_dir, paths) = scratch();
        old_files(&paths);
        let found = installation(&paths);
        let store = found.open().unwrap();
        let counts = take_in(&paths, &store).unwrap();
        assert_eq!(
            counts,
            Counts {
                secrets: 2,
                registries: 1,
                settings: 2,
                kept: 0
            }
        );
        // The process ends here; a secret changes meanwhile through the store.
        store
            .write(kind::SECRET, OWN, "github", b"changed")
            .unwrap();
        drop(store);
        let store = open(&paths);
        assert_eq!(read(&store, kind::SECRET, "github").unwrap(), "changed");
        assert!(!paths.config_dir.join("secrets.json").exists());
        assert!(paths.config_dir.join("migrated/secrets.json").exists());
        assert!(
            !read_map(&paths.settings_file())
                .unwrap()
                .unwrap()
                .contains_key("registries")
        );
    }

    #[test]
    fn a_copy_put_away_before_is_not_overwritten() {
        let (_dir, paths) = scratch();
        old_files(&paths);
        let migrated = paths.config_dir.join("migrated");
        fs::create_dir_all(&migrated).unwrap();
        fs::write(migrated.join("config.json"), "{}").unwrap();
        open(&paths);
        assert_eq!(
            fs::read_to_string(migrated.join("config.json")).unwrap(),
            "{}"
        );
        assert_eq!(
            fs::read_to_string(migrated.join("config.1.json")).unwrap(),
            CONFIG
        );
    }

    #[test]
    fn what_the_store_has_wins() {
        let (_dir, paths) = scratch();
        old_files(&paths);
        let found = installation(&paths);
        let store = found.open().unwrap();
        store
            .write(kind::SECRET, OWN, "github", b"restored")
            .unwrap();
        store
            .write(kind::SETTING, OWN, "default_host", b"attic")
            .unwrap();
        let counts = take_in(&paths, &store).unwrap();
        assert_eq!((counts.secrets, counts.settings, counts.kept), (1, 1, 2));
        assert_eq!(read(&store, kind::SECRET, "github").unwrap(), "restored");
        assert_eq!(
            read(&store, kind::SETTING, "default_host").unwrap(),
            "attic"
        );
        assert_eq!(read(&store, kind::SECRET, "registry/nexus").unwrap(), "pw");
    }

    #[test]
    fn an_installation_of_berth_05_gives_its_profiles_the_names_of_now() {
        let (_dir, paths) = scratch();
        // berth 0.5 had run every import but the last, and saved hosts of each level.
        let store = open(&paths);
        store
            .change(&[], &[(kind::META, OWN, NAMES_MARKER)])
            .unwrap();
        for (name, level) in [("dock", "reachable"), ("pier", "controller")] {
            let old =
                json!({"level": level, "profile": "cloud", "access": {"address": "192.0.2.4"}});
            store
                .write(kind::PROFILE, OWN, name, old.to_string().as_bytes())
                .unwrap();
        }
        let studio = json!({"hostname": "studio", "profile": "laptop", "builder": {}});
        store
            .write(kind::PROFILE, OWN, "studio", studio.to_string().as_bytes())
            .unwrap();
        let found = installation(&paths);
        run(&paths, &found, &store).unwrap();
        assert!(read(&store, kind::META, NAMES_MARKER).is_some());
        let dock = json_row(&store, kind::PROFILE, "dock").unwrap();
        assert_eq!(
            (&dock["type"], &dock["target"]),
            (&json!("ssh"), &json!("cloud"))
        );
        assert_eq!(
            json_row(&store, kind::PROFILE, "pier").unwrap()["type"],
            "managed"
        );
        assert_eq!(
            json_row(&store, kind::PROFILE, "studio").unwrap(),
            json!({"hostname": "studio", "target": "laptop", "builder": {}})
        );
        // A row a berth server of an older version writes later is read with the names of now.
        let late = json!({"level": "controller", "access": {"address": "192.0.2.5"}});
        store
            .write_labelled(
                kind::PROFILE,
                OWN,
                &crate::store::new_id(),
                "late",
                late.to_string().as_bytes(),
            )
            .unwrap();
        let place = crate::profiles::Place::own(&paths, "late").unwrap();
        assert_eq!(place.load(&paths).unwrap().unwrap()["type"], "managed");
        let listed = crate::profiles::list(&paths, OWN).unwrap();
        let (_, listed) = listed
            .iter()
            .find(|(place, _)| place.argument() == "late")
            .unwrap();
        assert!(listed.get("level").is_none());
    }

    #[test]
    fn stored_profiles_lose_their_pasted_keys_and_say_so_once() {
        let (_dir, paths) = scratch();
        let store = open(&paths);
        store
            .change(&[], &[(kind::META, OWN, TAILNET_MARKER)])
            .unwrap();
        let rows = [
            (
                OWN,
                "studio",
                json!({"hostname": "studio", "tailscale_auth_key": "tskey-auth-kFAKE-0000", "tailscale_auto_key": false}),
            ),
            (
                OWN,
                "dock",
                json!({"hostname": "dock", "tailscale_auto_key": false}),
            ),
            (
                OWN,
                "pier",
                json!({"hostname": "pier", "tailscale_auto_key": true}),
            ),
            (OWN, "attic", json!({"hostname": "attic", "builder": {}})),
            (
                "ffffffff",
                "theirs",
                json!({"ansible_vars": {"devbox_tailscale_auth_key": "tskey-auth-kFAKE-1111"}}),
            ),
        ];
        for (scope, name, profile) in &rows {
            store
                .write(kind::PROFILE, scope, name, profile.to_string().as_bytes())
                .unwrap();
        }
        let taken = take_in_tailnet(&store).unwrap();
        assert_eq!(
            taken,
            TakenTailnet {
                pasted: vec!["studio".into()],
                optional: vec!["attic".into(), "dock".into()],
            }
        );
        assert!(read(&store, kind::META, TAILNET_MARKER).is_some());
        assert_eq!(
            json_row(&store, kind::PROFILE, "studio").unwrap(),
            json!({"hostname": "studio"})
        );
        assert_eq!(
            json_row(&store, kind::PROFILE, "pier").unwrap(),
            json!({"hostname": "pier"})
        );
        // berth 0.5 joined neither of these by itself, and an apply goes on without the tailnet still.
        assert_eq!(
            json_row(&store, kind::PROFILE, "attic").unwrap(),
            json!({"hostname": "attic", "builder": {}, "ansible_vars": {(TAILNET_OPTIONAL): true}})
        );
        assert_eq!(
            json_row(&store, kind::PROFILE, "dock").unwrap(),
            json!({"hostname": "dock", "ansible_vars": {(TAILNET_OPTIONAL): true}})
        );
        // No key is left in any row of the store, of any scope.
        let crate::store::Rows::State(all) = store.export(crate::store::Table::State).unwrap()
        else {
            panic!("no rows");
        };
        assert!(
            all.iter()
                .all(|row| !String::from_utf8_lossy(&row.value).contains("tskey-auth"))
        );
        let said = tailnet_message(&taken, &store).unwrap().unwrap();
        assert_eq!(
            said,
            "berth makes the keys hosts join the tailnet with itself now. The keys pasted into the profiles of studio are removed. studio joins the tailnet on the next apply, unless already on it, once the secret tailnet_api_key is set: berth secret set tailnet_api_key. attic, dock join the tailnet once the secret tailnet_api_key is set: berth secret set tailnet_api_key. Until then an apply leaves them off the tailnet, as berth 0.5 did."
        );
        store
            .write(
                kind::SECRET,
                OWN,
                berth_api::profile::TAILNET_API_KEY,
                b"tskey-api-kFAKE-0000",
            )
            .unwrap();
        assert!(
            tailnet_message(&taken, &store)
                .unwrap()
                .unwrap()
                .ends_with("unless already on it.")
        );
        assert_eq!(
            tailnet_message(&TakenTailnet::default(), &store).unwrap(),
            None
        );
        // One host joins, several join.
        let one = TakenTailnet {
            pasted: vec![],
            optional: vec!["dock".into()],
        };
        assert!(
            tailnet_message(&one, &store)
                .unwrap()
                .unwrap()
                .contains(" dock joins the tailnet on the next apply")
        );
        // Once is enough: the next start finds the marker and changes nothing.
        let before = store.export(crate::store::Table::State).unwrap();
        let found = installation(&paths);
        run(&paths, &found, &store).unwrap();
        assert_eq!(store.export(crate::store::Table::State).unwrap(), before);
    }

    #[test]
    fn a_profile_given_in_a_file_is_read_with_the_names_of_now() {
        let (dir, paths) = scratch();
        let file = dir.path().join("dock.json");
        fs::write(
            &file,
            r#"{"level": "reachable", "profile": "cloud", "access": {"address": "192.0.2.4"}}"#,
        )
        .unwrap();
        let place = crate::profiles::Place::File(file.clone());
        let read = place.load(&paths).unwrap().unwrap();
        assert_eq!(
            read,
            json!({"type": "ssh", "target": "cloud", "access": {"address": "192.0.2.4"}})
        );
        // The file keeps what it said until it is saved.
        assert!(fs::read_to_string(&file).unwrap().contains("reachable"));
        place.save(&paths, &read).unwrap();
        assert!(!fs::read_to_string(&file).unwrap().contains("level"));
    }

    #[test]
    fn a_new_installation_gets_only_the_marker() {
        let (_dir, paths) = scratch();
        let store = open(&paths);
        let report = store.seal_report().unwrap();
        assert_eq!((report.sealed, report.plain), (0, 9));
        assert!(read(&store, kind::META, MARKER).is_some());
        assert!(read(&store, kind::META, PROVISIONERS_MARKER).is_some());
        assert!(read(&store, kind::META, PROFILES_MARKER).is_some());
        assert!(read(&store, kind::META, KEYS_MARKER).is_some());
        assert!(read(&store, kind::META, ACCESS_MARKER).is_some());
        assert!(read(&store, kind::META, RESOURCES_MARKER).is_some());
        assert!(read(&store, kind::META, NAMES_MARKER).is_some());
        assert!(read(&store, kind::META, TAILNET_MARKER).is_some());
        assert!(!paths.config_dir.join("migrated").exists());
        assert!(!paths.settings_file().exists());
    }

    #[test]
    fn with_a_key_the_imported_values_are_sealed() {
        let (_dir, paths) = scratch();
        old_files(&paths);
        let found = installation(&paths);
        Key::generate().unwrap().write_new(&found.key_file).unwrap();
        let store = open(&paths);
        assert!(store.encrypted());
        let report = store.seal_report().unwrap();
        // Two secrets, a registry, two settings and the nine markers.
        assert_eq!((report.sealed, report.plain), (14, 0));
        assert_eq!(read(&store, kind::SECRET, "github").unwrap(), "ghp-made-up");
    }

    #[test]
    fn two_berths_starting_at_once_import_once() {
        let (_dir, paths) = scratch();
        old_files(&paths);
        let paths = Arc::new(paths);
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let paths = paths.clone();
                std::thread::spawn(move || {
                    let found = installation(&paths);
                    let store = found.open().unwrap();
                    run(&paths, &found, &store).unwrap();
                })
            })
            .collect();
        threads
            .into_iter()
            .for_each(|thread| thread.join().unwrap());
        let store = installation(&paths).open().unwrap();
        assert_eq!(store.names(kind::SECRET, OWN).unwrap().len(), 2);
        let migrated: Vec<_> = fs::read_dir(paths.config_dir.join("migrated"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(migrated.len(), 2, "{migrated:?}");
    }

    const STUDIO: &str = r#"{"hostname": "studio", "builder": {"ssh_host": "192.0.2.10"}}"#;
    const RECORD: &str = r#"{"provisioner": "cloud/vm", "id": "vm-1", "token": "t0k"}"#;

    /// The hosts folder of berth 0.4, and a mirror's folder of profiles.
    fn old_hosts(paths: &Paths) {
        let hosts = paths.config_dir.join("hosts");
        fs::create_dir_all(&hosts).unwrap();
        fs::write(hosts.join("studio.json"), STUDIO).unwrap();
        fs::write(
            hosts.join("studio.credentials.json"),
            r#"{"ops": "made-up-pw"}"#,
        )
        .unwrap();
        fs::write(hosts.join("studio.instance.json"), RECORD).unwrap();
        fs::write(hosts.join("studio.instance.lock"), "").unwrap();
        fs::write(hosts.join("attic.json"), r#"{"builder": {}}"#).unwrap();
        let served = paths.data_dir.join("served/http---192-0-2-7-7777/hosts");
        fs::create_dir_all(&served).unwrap();
        fs::write(served.join("yard.json"), "{}").unwrap();
    }

    #[test]
    fn the_provisioners_in_use_are_registered_under_names_of_their_own() {
        let (_dir, paths) = scratch();
        let store = open(&paths);
        store
            .write_json(
                kind::PROVISIONER,
                OWN,
                "incus/vm",
                &json!({"server": "https://192.0.2.10:8443"}),
            )
            .unwrap();
        // A resource of another scope, such as a mirror's, names its provisioner too.
        let record = crate::resource::Record {
            id: "r-mirror01".into(),
            provisioner: "docker/container".into(),
            kind: "deployment".into(),
            name: "wiki".into(),
            ..Default::default()
        };
        store
            .write_json(kind::RESOURCE, "mirror", &record.id, &json!(record))
            .unwrap();
        // One registered already keeps what it has.
        let kept =
            json!({"type": "docker/container", "settings": {"host": "ssh://ops@192.0.2.20"}});
        store
            .write_json(kind::PROVISIONER_ENTRY, OWN, "docker-container", &kept)
            .unwrap();
        assert_eq!(take_in_provisioners(&store).unwrap(), ["incus-vm"]);
        assert_eq!(
            json_row(&store, kind::PROVISIONER_ENTRY, "incus-vm").unwrap(),
            json!({"type": "incus/vm", "settings": {"server": "https://192.0.2.10:8443"}})
        );
        assert!(json_row(&store, kind::PROVISIONER, "incus/vm").is_none());
        assert_eq!(
            json_row(&store, kind::PROVISIONER_ENTRY, "docker-container").unwrap(),
            kept
        );
        let moved = store
            .read_json(kind::RESOURCE, "mirror", &record.id)
            .unwrap()
            .unwrap();
        assert_eq!(moved["provisioner"], "docker-container");
        assert!(read(&store, kind::META, PROVISIONERS_MARKER).is_some());
    }

    /// A row by the name of what it is, or of the host it belongs to.
    fn json_row(store: &Store, kind: &str, name: &str) -> Option<Value> {
        let of = match kind {
            kind::CREDENTIALS | kind::ACCESS => kind::PROFILE,
            kind => kind,
        };
        let row = store
            .id_of(of, OWN, name)
            .unwrap()
            .unwrap_or(name.to_owned());
        store.read_json(kind, OWN, &row).unwrap()
    }

    /// The resource that is the machine of a host of this installation.
    fn machine_of(store: &Store, host: &str) -> Option<crate::resource::Record> {
        let host = store
            .id_of(kind::PROFILE, OWN, host)
            .unwrap()
            .unwrap_or(host.to_owned());
        store
            .entries(kind::RESOURCE, OWN)
            .unwrap()
            .into_iter()
            .map(|(_, value)| serde_json::from_slice::<crate::resource::Record>(&value).unwrap())
            .find(|record| record.host == host)
    }

    #[test]
    fn the_hosts_move_into_the_store() {
        let (_dir, paths) = scratch();
        old_hosts(&paths);
        let store = open(&paths);
        // In the layout of now: an empty key file was the user's keys and berth's.
        // berth 0.5 did not join it to the tailnet by itself, and an apply still goes on without.
        assert_eq!(
            json_row(&store, kind::PROFILE, "studio").unwrap(),
            json!({"hostname": "studio", "builder": {}, "access": {"address": "192.0.2.10", "auth": "ssh"},
                   "ansible_vars": {(TAILNET_OPTIONAL): true}})
        );
        assert!(json_row(&store, kind::PROFILE, "attic").is_some());
        assert_eq!(
            json_row(&store, kind::CREDENTIALS, "studio").unwrap()["ops"],
            "made-up-pw"
        );
        // The record of its machine is a resource of the machine kind now.
        let record = machine_of(&store, "studio").unwrap();
        assert_eq!(
            (
                record.external_id.as_str(),
                record.token.as_str(),
                record.kind.as_str(),
                crate::provisioner::label(&paths, &record.provisioner).as_str(),
                record.name.as_str()
            ),
            ("vm-1", "t0k", "machine", "cloud-vm", "studio")
        );
        // Its provisioner is registered under a name of its own, of the type it named.
        assert_eq!(
            json_row(&store, kind::PROVISIONER_ENTRY, "cloud-vm").unwrap(),
            json!({"type": "cloud/vm", "settings": {}})
        );
        assert!(crate::resource::valid_id(&record.id), "{}", record.id);
        assert!(json_row(&store, kind::INSTANCE, "studio").is_none());
        let hosts: Vec<String> = store
            .labels(kind::PROFILE, OWN)
            .unwrap()
            .into_iter()
            .map(|found| found.name)
            .collect();
        assert_eq!(hosts, ["attic", "studio"]);
        // The folder went to migrated/ whole, the lock with it; the mirror's is gone.
        let moved = paths.config_dir.join("migrated/hosts");
        assert!(!paths.config_dir.join("hosts").exists());
        assert!(
            moved.join("studio.json").is_file() && moved.join("studio.instance.lock").is_file()
        );
        assert!(!paths.data_dir.join("served").exists());
        assert_eq!(paths.hosts().len(), 2);

        // Again, nothing changes.
        let before = store.seal_report().unwrap();
        let marker = read(&store, kind::META, PROFILES_MARKER);
        drop(store);
        let store = open(&paths);
        assert_eq!(store.seal_report().unwrap(), before);
        assert_eq!(read(&store, kind::META, PROFILES_MARKER), marker);
        let migrated: Vec<_> = fs::read_dir(paths.config_dir.join("migrated"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(migrated, ["hosts"]);

        // A hosts folder made again later is not taken in, and goes beside the first.
        fs::create_dir_all(paths.config_dir.join("hosts")).unwrap();
        fs::write(paths.config_dir.join("hosts/late.json"), "{}").unwrap();
        drop(store);
        let store = open(&paths);
        assert!(json_row(&store, kind::PROFILE, "late").is_none());
        assert!(
            paths
                .config_dir
                .join("migrated/hosts.1/late.json")
                .is_file()
        );
    }

    #[test]
    fn the_records_of_machines_become_resources_in_their_scopes_once() {
        let (_dir, paths) = scratch();
        let store = open(&paths);
        let file = "/profiles/attic.json";
        let served = "served:http://192.0.2.7:7777";
        for (scope, host) in [
            (OWN, "studio"),
            (crate::profiles::FILE_SCOPE, file),
            (served, "yard"),
        ] {
            store
                .write(kind::INSTANCE, scope, host, RECORD.as_bytes())
                .unwrap();
        }
        store.write(kind::INSTANCE, OWN, "broken", b"[1]").unwrap();
        assert_eq!(take_in_resources(&store).unwrap(), 3);
        for (scope, host, name) in [
            (OWN, "studio", "studio"),
            (crate::profiles::FILE_SCOPE, file, "attic"),
            (served, "yard", "yard"),
        ] {
            let found: Vec<crate::resource::Record> = store
                .entries(kind::RESOURCE, scope)
                .unwrap()
                .into_iter()
                .map(|(_, value)| serde_json::from_slice(&value).unwrap())
                .collect();
            assert_eq!(found.len(), 1, "{scope}");
            assert_eq!(
                (
                    found[0].host.as_str(),
                    found[0].name.as_str(),
                    found[0].external_id.as_str()
                ),
                (host, name, "vm-1")
            );
            assert!(store.names(kind::INSTANCE, scope).unwrap().is_empty());
        }
        // Found again as the machine of its host.
        let place = crate::profiles::Place::File(file.into());
        assert_eq!(
            crate::resource::of_place(&paths, &place)
                .unwrap()
                .unwrap()
                .record
                .token,
            "t0k"
        );
        // The marker stops it from running again.
        assert!(read(&store, kind::META, RESOURCES_MARKER).is_some());
        drop(store);
        let store = open(&paths);
        assert_eq!(store.names(kind::RESOURCE, OWN).unwrap().len(), 1);
    }

    #[test]
    fn a_hosts_import_stopped_after_its_change_is_finished() {
        let (_dir, paths) = scratch();
        old_hosts(&paths);
        let found = installation(&paths);
        let store = found.open().unwrap();
        let counts = take_in_profiles(&paths, &store).unwrap();
        assert_eq!(
            counts,
            HostCounts {
                profiles: 2,
                credentials: 1,
                records: 1,
                kept: 0
            }
        );
        // The process ends here; the record changes meanwhile through the store.
        store
            .write(
                kind::INSTANCE,
                OWN,
                "studio",
                br#"{"provisioner": "cloud/vm", "id": "vm-2", "token": "t0k"}"#,
            )
            .unwrap();
        drop(store);
        let store = open(&paths);
        assert_eq!(machine_of(&store, "studio").unwrap().external_id, "vm-2");
        assert!(!paths.config_dir.join("hosts").exists());
        assert!(
            paths
                .config_dir
                .join("migrated/hosts/studio.json")
                .is_file()
        );
        assert!(!paths.data_dir.join("served").exists());
    }

    #[test]
    fn a_file_that_is_not_json_stops_the_hosts_import_with_nothing_written() {
        let (_dir, paths) = scratch();
        old_hosts(&paths);
        let broken = paths.config_dir.join("hosts/attic.credentials.json");
        fs::write(&broken, "{not json").unwrap();
        let found = installation(&paths);
        let store = found.open().unwrap();
        let said = run(&paths, &found, &store).unwrap_err().to_string();
        assert!(said.contains(&broken.display().to_string()), "{said}");
        for kind in [kind::PROFILE, kind::CREDENTIALS, kind::INSTANCE] {
            assert!(store.names(kind, OWN).unwrap().is_empty(), "{kind}");
        }
        assert!(read(&store, kind::META, PROFILES_MARKER).is_none());
        assert!(paths.config_dir.join("hosts/studio.json").is_file());
        // Mended, it goes through.
        fs::write(&broken, "{}").unwrap();
        run(&paths, &found, &store).unwrap();
        let mut hosts: Vec<String> = store
            .names(kind::CREDENTIALS, OWN)
            .unwrap()
            .iter()
            .map(|id| store.label_of(kind::PROFILE, OWN, id).unwrap().unwrap())
            .collect();
        hosts.sort();
        assert_eq!(hosts, ["attic", "studio"]);
    }

    #[test]
    fn a_host_the_store_has_wins() {
        let (_dir, paths) = scratch();
        old_hosts(&paths);
        let found = installation(&paths);
        let store = found.open().unwrap();
        store
            .write(kind::PROFILE, OWN, "studio", br#"{"restored": true}"#)
            .unwrap();
        let counts = take_in_profiles(&paths, &store).unwrap();
        assert_eq!((counts.profiles, counts.kept), (1, 1));
        assert_eq!(
            json_row(&store, kind::PROFILE, "studio").unwrap()["restored"],
            true
        );
        // The rest of the host is taken in all the same.
        assert!(json_row(&store, kind::INSTANCE, "studio").is_some());
        assert!(json_row(&store, kind::PROFILE, "attic").is_some());
    }

    #[test]
    fn the_keys_token_and_plugins_go_into_the_store_and_their_files_stay() {
        let (_dir, paths) = scratch();
        fs::create_dir_all(&paths.data_dir).unwrap();
        let data = &paths.data_dir;
        fs::write(data.join("id_ed25519"), "private").unwrap();
        fs::write(data.join("id_ed25519.pub"), "ssh-ed25519 AAAA berth@studio").unwrap();
        fs::write(data.join("serve.token"), format!("{}\n", "b".repeat(64))).unwrap();
        let setup = r#"{"key": "made-up-setup-key-of-sixty-four-letters-or-so-0123456789abcdef", "expires": 4000000000}"#;
        fs::write(data.join("serve.setup-key"), format!("{setup}\n")).unwrap();
        let own = crate::plugins::own_dir(&paths.config_dir);
        fs::create_dir_all(&own).unwrap();
        fs::write(own.join("package.json"), r#"{"dependencies": {}}"#).unwrap();
        let found = installation(&paths);
        let store = found.open().unwrap();
        // A restored store's token wins.
        store
            .write(kind::TOKEN, OWN, "serve", b"restored-token")
            .unwrap();
        run(&paths, &found, &store).unwrap();
        let berth: Value =
            serde_json::from_str(&read(&store, kind::KEY, "berth").unwrap()).unwrap();
        assert_eq!(
            berth,
            json!({"private": "private", "public": "ssh-ed25519 AAAA berth@studio"})
        );
        assert!(read(&store, kind::KEY, "ssh").is_none());
        assert_eq!(
            read(&store, kind::TOKEN, "serve").unwrap(),
            "restored-token"
        );
        assert_eq!(read(&store, kind::TOKEN, "setup-key").unwrap(), setup);
        assert_eq!(
            read(&store, kind::PLUGIN, "package.json").unwrap(),
            r#"{"dependencies": {}}"#
        );
        assert!(read(&store, kind::META, KEYS_MARKER).is_some());
        // The setup key is in the store alone; the others are still used as files.
        assert!(!data.join("serve.setup-key").exists());
        assert!(data.join("id_ed25519").is_file() && data.join("serve.token").is_file());
        assert!(own.join("package.json").is_file());

        // Once done, a file that turns up later is not taken in, and goes.
        store.forget(kind::TOKEN, OWN, "setup-key").unwrap();
        fs::write(data.join("serve.setup-key"), setup).unwrap();
        run(&paths, &found, &store).unwrap();
        assert!(read(&store, kind::TOKEN, "setup-key").is_none());
        assert!(!data.join("serve.setup-key").exists());
    }

    /// A private key at `file`, as an older berth was given one.
    fn key_file(file: &Path) {
        let made = crate::keys::openssh::generate("made-up").unwrap();
        crate::paths::write_private(file, made.private.as_bytes()).unwrap();
        fs::write(file.with_extension("pub"), format!("{}\n", made.public)).unwrap();
    }

    #[test]
    fn how_hosts_are_reached_moves_into_access_once_for_every_scope() {
        let (dir, paths) = scratch();
        let found = installation(&paths);
        let store = found.open().unwrap();
        let deploy = dir.path().join("deploy_key");
        key_file(&deploy);
        let served = "served:http://192.0.2.7:7777";
        let profiles = [
            (
                OWN,
                "studio",
                json!({"builder": {"ssh_host": "192.0.2.10", "ssh_user": "ops", "ssh_key": ""}}),
            ),
            (
                OWN,
                "keyed",
                json!({"builder": {"ssh_host": "192.0.2.11", "ssh_port": "2222", "ssh_key": deploy}}),
            ),
            (
                OWN,
                "again",
                json!({"builder": {"ssh_host": "192.0.2.12", "ssh_key": deploy}}),
            ),
            (
                OWN,
                "lost",
                json!({"builder": {"ssh_host": "192.0.2.13", "ssh_key": "/nowhere/key"}}),
            ),
            (
                OWN,
                "typed",
                json!({"builder": {"ssh_host": "192.0.2.14", "ask_pass": true, "ask_become_pass": true}}),
            ),
            (
                OWN,
                "container",
                json!({"builder": {"link": ["docker", "exec", "-i", "c", "berthd", "connect"]}}),
            ),
            (
                OWN,
                "new",
                json!({"access": {"address": "192.0.2.15", "auth": "berth"}}),
            ),
            (
                served,
                "yard",
                json!({"builder": {"ssh_host": "192.0.2.20", "served": "http://192.0.2.7:7777"}}),
            ),
        ];
        for (scope, name, profile) in &profiles {
            store
                .write_json(kind::PROFILE, scope, name, profile)
                .unwrap();
        }
        // berth 0.5 before this import kept its key in two rows.
        store
            .write(kind::KEY, OWN, "ssh", b"made-up private")
            .unwrap();
        store
            .write(
                kind::KEY,
                OWN,
                "ssh.pub",
                b"ssh-ed25519 AAAAown berth@studio",
            )
            .unwrap();
        // The user's known_hosts knows studio.
        let known_hosts = dir.path().join("known_hosts");
        fs::write(&known_hosts, "192.0.2.10 ssh-ed25519 AAAAhostkey\n").unwrap();
        let notices = take_in_access(&paths, &store, Some(&known_hosts)).unwrap();
        let access = |scope: &str, name: &str| {
            store
                .read_json(kind::PROFILE, scope, name)
                .unwrap()
                .unwrap()["access"]
                .clone()
        };
        assert_eq!(
            access(OWN, "studio"),
            json!({"address": "192.0.2.10", "user": "ops", "auth": "ssh"})
        );
        assert_eq!(access(OWN, "keyed")["auth"], "key:deploy_key");
        assert_eq!(access(OWN, "keyed")["port"], "2222");
        assert_eq!(access(OWN, "again")["auth"], "key:deploy_key");
        assert_eq!(access(OWN, "lost")["auth"], "ssh");
        assert_eq!(
            access(OWN, "typed"),
            json!({"address": "192.0.2.14", "auth": "password", "sudo": "password"})
        );
        assert_eq!(access(OWN, "container")["link"][0], "docker");
        assert_eq!(
            access(OWN, "new"),
            json!({"address": "192.0.2.15", "auth": "berth"})
        );
        assert_eq!(access(served, "yard")["auth"], "berth");
        assert!(
            notices.iter().any(|notice| notice.contains("/nowhere/key")),
            "{notices:?}"
        );
        // The key file is a key of the ring, once; berth's key moved. The
        // rows are by name until the import of ids.
        let keys = store.names(kind::KEY, OWN).unwrap();
        assert_eq!(keys, ["berth", "deploy_key"]);
        let berth: crate::keys::ring::Halves =
            serde_json::from_slice(&store.read(kind::KEY, OWN, "berth").unwrap().unwrap()).unwrap();
        assert_eq!(berth.private, "made-up private");
        assert!(store.read(kind::KEY, OWN, "ssh.pub").unwrap().is_none());
        let record = crate::keys::known::Record::load(
            &store,
            &crate::profiles::Place::at(OWN, "studio", "studio"),
        )
        .unwrap();
        assert_eq!(record.host_keys, ["ssh-ed25519 AAAAhostkey"]);
        assert!(
            store
                .read_json(kind::META, OWN, ACCESS_MARKER)
                .unwrap()
                .is_some()
        );

        // A second run, as after an interruption once the change was made, changes nothing.
        let before: Vec<_> = profiles
            .iter()
            .map(|(scope, name, _)| access(scope, name))
            .collect();
        store.forget(kind::META, OWN, ACCESS_MARKER).unwrap();
        assert!(
            take_in_access(&paths, &store, Some(&known_hosts))
                .unwrap()
                .is_empty()
        );
        let after: Vec<_> = profiles
            .iter()
            .map(|(scope, name, _)| access(scope, name))
            .collect();
        assert_eq!(before, after);
        assert_eq!(store.names(kind::KEY, OWN).unwrap().len(), 2);
    }

    #[test]
    fn a_profile_in_a_file_is_read_in_the_new_layout_and_written_so_when_saved() {
        let (dir, paths) = scratch();
        let file = dir.path().join("studio.json");
        let old = r#"{"builder": {"ssh_host": "192.0.2.10", "ask_become_pass": false}}"#;
        fs::write(&file, old).unwrap();
        let place = crate::profiles::Place::File(file.clone());
        let loaded = place.load(&paths).unwrap().unwrap();
        assert_eq!(
            loaded["access"],
            json!({"address": "192.0.2.10", "auth": "ssh", "sudo": "none"})
        );
        assert_eq!(fs::read_to_string(&file).unwrap(), old);
        place.save(&paths, &loaded).unwrap();
        let saved: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        assert_eq!(saved["access"]["address"], "192.0.2.10");
        assert!(saved["builder"].get("ssh_host").is_none());
    }
}
