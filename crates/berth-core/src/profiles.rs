//! Where a host's profile is kept. A saved host is a row of berth's store,
//! under the scope of this installation or of the berth server it uses; a
//! profile in a file the user named stays in that file. What berth keeps of a
//! host besides its profile, its generated passwords and the record of the
//! machine berth created for it, is in the store either way: beside a saved
//! host's profile, and for a file under [`FILE_SCOPE`] by the file's path.

use crate::{
    Result,
    paths::Paths,
    store::{self, OWN, kind},
};
use serde_json::Value;
use std::{fs, path::PathBuf};

/// The scope of what is kept of a profile in a file, by the file's absolute path.
pub const FILE_SCOPE: &str = "file";

/// Where a host's profile is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    /// A saved host of this installation or of the berth server it uses:
    /// its id and its name. A host not saved yet has the id it will get.
    Stored {
        scope: String,
        id: String,
        name: String,
    },
    /// A profile in a file the user named; berth reads and writes that file.
    File(PathBuf),
}

/// The scope of the saved hosts: this installation's own. A client of a
/// server elsewhere keeps none.
pub fn scope(_: &Paths) -> String {
    OWN.to_owned()
}

/// Whether a name can be a saved host's: it is a file name of its own on
/// every machine, no option of a command and not in the shape of an id.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(['-', '.'])
        && !name.ends_with(".json")
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "-_.".contains(ch))
        && !berth_api::ids::looks_like_id(name)
}

/// A short digest of a text, as a part of a file name.
fn digest(text: &str, length: usize) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(text.as_bytes());
    hash.iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()[..length]
        .to_owned()
}

impl Place {
    /// What a host given on a command line is: anything that looks like a
    /// path is a file, and a name or an id is a saved host of [`scope`].
    pub fn resolve(paths: &Paths, host: &str) -> Result<Place> {
        if host.contains('/') || host.ends_with(".json") {
            return Ok(Place::File(std::path::absolute(host)?));
        }
        Place::stored(paths, &scope(paths), host)
    }

    /// The saved host of a scope that a name or id gives, if there is one.
    pub fn find(paths: &Paths, scope: &str, given: &str) -> Result<Option<Place>> {
        let found = store::shared(paths)?.find(kind::PROFILE, scope, given)?;
        Ok(found.map(|found| Place::at(scope, &found.id, &found.name)))
    }

    /// The saved host of a scope that a name or id gives, or a host not
    /// saved yet under a name that is checked, with a new id.
    pub fn stored(paths: &Paths, scope: &str, given: &str) -> Result<Place> {
        if let Some(place) = Place::find(paths, scope, given)? {
            return Ok(place);
        }
        if berth_api::ids::id_in(berth_api::ids::Kind::Host, given).is_some() {
            return Err(format!("No host {given}").into());
        }
        Place::new(scope, given)
    }

    /// A host of a scope not saved yet, under a name that is checked, with a
    /// new id.
    pub fn new(scope: &str, name: &str) -> Result<Place> {
        if !valid_name(name) {
            return Err(format!("Not a host name: {name}").into());
        }
        Ok(Place::at(scope, &store::new_id(), name))
    }

    /// A saved host of this installation's own, by its name or id.
    pub fn own(paths: &Paths, given: &str) -> Result<Place> {
        Place::stored(paths, OWN, given)
    }

    /// A saved host by what the store keeps of it.
    pub fn at(scope: &str, id: &str, name: &str) -> Place {
        Place::Stored {
            scope: scope.to_owned(),
            id: id.to_owned(),
            name: name.to_owned(),
        }
    }

    /// What names the host to another berth command: the saved name, or
    /// the file's absolute path.
    pub fn argument(&self) -> String {
        match self {
            Place::Stored { name, .. } => name.clone(),
            Place::File(path) => path.to_string_lossy().into_owned(),
        }
    }

    /// What the host is known by in records and jobs: the saved host's id,
    /// or the file's absolute path.
    pub fn key(&self) -> String {
        match self {
            Place::Stored { id, .. } => id.clone(),
            Place::File(path) => path.to_string_lossy().into_owned(),
        }
    }

    /// The saved host's id.
    pub fn id(&self) -> Option<&str> {
        match self {
            Place::Stored { id, .. } => Some(id),
            Place::File(_) => None,
        }
    }

    /// The host's name for people: the saved name, or the file's name
    /// without `.json`.
    pub fn title(&self) -> String {
        match self {
            Place::Stored { name, .. } => name.clone(),
            Place::File(path) => path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default(),
        }
    }

    /// The berth server a saved host belongs to, by its address.
    pub fn server_url(&self) -> Option<&str> {
        match self {
            Place::Stored { scope, .. } => scope.strip_prefix("served:"),
            Place::File(_) => None,
        }
    }

    /// The scope and name of what is kept beside the profile: the saved
    /// host's id, or the file's path.
    pub fn side(&self) -> (String, String) {
        match self {
            Place::Stored { scope, id, .. } => (scope.clone(), id.clone()),
            Place::File(path) => (FILE_SCOPE.to_owned(), path.to_string_lossy().into_owned()),
        }
    }

    /// Where the profile is, for people: the host's name and whose store
    /// keeps it, or the file.
    pub fn describe(&self) -> String {
        match (self, self.server_url()) {
            (Place::Stored { name, .. }, Some(url)) => {
                format!("{name} on the berth server at {url}")
            }
            (Place::Stored { name, .. }, None) => format!("{name} in berth's store"),
            (Place::File(path), _) => path.display().to_string(),
        }
    }

    /// The profile, or `None` when there is none. It names its registries
    /// and secrets by their names, as people read it.
    pub fn load(&self, paths: &Paths) -> Result<Option<Value>> {
        match self {
            Place::Stored { scope, id, .. } => {
                let store = store::shared(paths)?;
                let mut profile: Option<Value> = store.read_json(kind::PROFILE, scope, id)?;
                // A row an older berth or berth server wrote after the store's
                // names were converted is read with the names of now.
                if let Some(profile) = &mut profile {
                    berth_api::profile::take_old_names(profile);
                    refs::to_names(&store, scope, profile)?;
                }
                Ok(profile)
            }
            Place::File(path) => match fs::read(path) {
                Ok(data) => {
                    let mut profile: Value = serde_json::from_slice(&data)
                        .map_err(|error| format!("{}: {error}", path.display()))?;
                    // A profile of an older berth is read in the layout of now;
                    // the file keeps the old one until the user saves.
                    crate::import::convert_profile(paths, &self.title(), &mut profile);
                    let store = store::shared(paths)?;
                    refs::to_names(&store, OWN, &mut profile)?;
                    Ok(Some(profile))
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(format!("{}: {error}", path.display()).into()),
            },
        }
    }

    /// Save the profile in place of the one there was. The store keeps the
    /// ids of the registries and secrets it names, by name or id; a file
    /// keeps their names, and is written privately and in one step.
    pub fn save(&self, paths: &Paths, profile: &Value) -> Result<()> {
        let store = store::shared(paths)?;
        match self {
            Place::Stored { scope, id, name } => {
                let mut profile = profile.clone();
                refs::to_ids(&store, scope, &mut profile)?;
                Ok(store.write_labelled(
                    kind::PROFILE,
                    scope,
                    id,
                    name,
                    profile.to_string().as_bytes(),
                )?)
            }
            Place::File(path) => {
                let mut profile = profile.clone();
                refs::to_names(&store, OWN, &mut profile)?;
                let text = serde_json::to_string_pretty(&profile)? + "\n";
                crate::paths::write_private(path, text.as_bytes())
            }
        }
    }

    /// The file whose lock is held while something is done to the host's
    /// machine. A lock is held by a process and its children for as long as
    /// a provisioning takes, which a row of the store cannot be.
    pub fn lock_file(&self, paths: &Paths) -> PathBuf {
        let name = match self {
            Place::Stored { scope, id, .. } if scope == OWN => format!("{id}.lock"),
            Place::Stored { scope, id, .. } => format!("{}.{id}.lock", digest(scope, 8)),
            Place::File(path) => format!("file.{}.lock", digest(&path.to_string_lossy(), 16)),
        };
        paths.data_dir.join("locks").join(name)
    }
}

/// How a profile refers to registries and secrets: by name where people
/// read it, by id in the store. A name that has no thing yet stays a name,
/// and is taken for the thing's id when one is made with it.
pub mod refs {
    use crate::{
        Result,
        store::{Store, kind},
    };
    use serde_json::Value;

    /// The places in a profile that refer to things: the kind and each
    /// reference, as a value to read or change.
    fn each(
        profile: &mut Value,
        mut visit: impl FnMut(&str, &mut Value) -> Result<()>,
    ) -> Result<()> {
        if let Some(secrets) = profile.get_mut("secrets").and_then(Value::as_array_mut) {
            for entry in secrets {
                if let Some(name) = entry.get_mut("name") {
                    visit(kind::SECRET, name)?;
                }
            }
        }
        if let Some(registries) = profile.get_mut("registries").and_then(Value::as_array_mut) {
            for name in registries {
                visit(kind::REGISTRY, name)?;
            }
        }
        // A key of the ring is `key:KEY`.
        if let Some(auth) = profile.pointer_mut("/access/auth")
            && let Some(key) = auth.as_str().and_then(|text| text.strip_prefix("key:"))
        {
            let mut key = Value::String(key.to_owned());
            visit(kind::KEY, &mut key)?;
            *auth = Value::String(format!("key:{}", key.as_str().unwrap_or("")));
        }
        Ok(())
    }

    /// The things a profile refers to, as the store keeps them: their ids.
    pub fn to_ids(store: &Store, scope: &str, profile: &mut Value) -> Result<()> {
        each(profile, |kind, value| {
            if let Some(given) = value.as_str()
                && let Some(found) = store.find(kind, scope, given)?
            {
                *value = Value::String(found.id);
            }
            Ok(())
        })
    }

    /// The things a profile refers to, by name.
    pub fn to_names(store: &Store, scope: &str, profile: &mut Value) -> Result<()> {
        each(profile, |kind, value| {
            if let Some(id) = value.as_str().filter(|text| berth_api::ids::is_id(text))
                && let Some(name) = store.label_of(kind, scope, id)?
            {
                *value = Value::String(name);
            }
            Ok(())
        })
    }

    /// Whether a profile as the store keeps it refers to the thing.
    pub fn refers_to(profile: &Value, kind: &str, id: &str, name: &str) -> bool {
        let mut profile = profile.clone();
        let mut found = false;
        let _ = each(&mut profile, |of, value| {
            found |= of == kind && (value == id || value == name);
            Ok(())
        });
        found
    }

    /// A profile as the store keeps it with the references to a thing
    /// changed: its name taken for its id, or its id for its name.
    pub fn swap(profile: &mut Value, kind: &str, from: &str, to: &str) -> bool {
        let mut changed = false;
        let _ = each(profile, |of, value| {
            if of == kind && value == from {
                *value = Value::String(to.to_owned());
                changed = true;
            }
            Ok(())
        });
        changed
    }
}

/// The saved hosts of a scope with their profiles as people read them, from
/// a store that is open.
pub fn stored_entries(store: &store::Store, scope: &str) -> Result<Vec<(store::Found, Value)>> {
    let mut found = vec![];
    for (host, value) in store.labelled_entries(kind::PROFILE, scope)? {
        let Ok(mut profile) = serde_json::from_slice::<Value>(&value) else {
            continue;
        };
        refs::to_names(store, scope, &mut profile)?;
        found.push((host, profile));
    }
    Ok(found)
}

/// In a batch, change the references to a thing in every saved host's
/// profile of a scope, from `from` to `to`: a name to the id of a thing
/// made under it, or the id of a thing removed to its name.
pub fn swap_references(
    store: &store::Store,
    batch: &mut store::Batch,
    scope: &str,
    kind: &str,
    from: &str,
    to: &str,
) -> Result<()> {
    for (found, value) in store.labelled_entries(kind::PROFILE, scope)? {
        let Ok(mut profile) = serde_json::from_slice::<Value>(&value) else {
            continue;
        };
        if refs::swap(&mut profile, kind, from, to) {
            batch.write(
                kind::PROFILE,
                scope,
                &found.id,
                serde_json::to_vec(&profile)?,
            );
        }
    }
    Ok(())
}

/// The saved hosts of a scope with their profiles, by name.
pub fn list(paths: &Paths, scope: &str) -> Result<Vec<(Place, Value)>> {
    let store = store::shared(paths)?;
    let entries = store.labelled_entries(kind::PROFILE, scope)?;
    let mut found = vec![];
    for (host, value) in entries {
        let Ok(mut profile) = serde_json::from_slice(&value) else {
            continue;
        };
        berth_api::profile::take_old_names(&mut profile);
        refs::to_names(&store, scope, &mut profile)?;
        found.push((Place::at(scope, &host.id, &host.name), profile));
    }
    Ok(found)
}

/// Forget a host: its profile and everything that belongs to it, its
/// generated passwords, the record of a machine of it that was never made,
/// what berth knows of its SSH and of its device on the tailnet, and the
/// passwords it keeps for logging in, in one change. The device stays on the
/// tailnet. A profile in a file is removed with the file.
pub fn remove(paths: &Paths, place: &Place) -> Result<()> {
    let (scope, name) = place.side();
    // The provisioners a node offers go with it, unless they still hold resources.
    if let Place::Stored { scope, id, .. } = place
        && scope == OWN
    {
        let secrets = crate::registry::Store::own(store::shared(paths)?);
        crate::node::forget(paths, &secrets, id)?;
    }
    // The record of a machine that was never made goes with the host; a
    // made one is refused before this.
    let machine = crate::resource::of_place(paths, place)?
        .map(|kept| (kept.scope, kept.record.id))
        .filter(|(_, id)| !id.is_empty());
    let mut batch = store::Batch::default();
    if let Some((scope, id)) = &machine {
        batch
            .remove(kind::RESOURCE, scope, id)
            .forget_owned(scope, id);
    }
    match place {
        Place::Stored { scope, id, .. } => {
            batch
                .remove(kind::PROFILE, scope, id)
                .forget_owned(scope, id);
            if scope != OWN {
                batch.forget_owned(OWN, id);
            }
        }
        Place::File(_) => {
            batch
                .remove(kind::CREDENTIALS, &scope, &name)
                .remove(kind::ACCESS, &scope, &name)
                .remove(kind::TAILNET, &scope, &name)
                .remove(kind::SECRET, OWN, &crate::keys::password_secret(place))
                .remove(kind::SECRET, OWN, &crate::keys::sudo_secret(place));
        }
    }
    store::shared(paths)?.apply(&batch)?;
    if let Place::File(path) = place {
        match fs::remove_file(path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(format!("{}: {error}", path.display()).into());
            }
            _ => {}
        }
    }
    let _ = fs::remove_file(place.lock_file(paths));
    let _ = fs::remove_file(crate::keys::known::file(paths, place));
    Ok(())
}

/// Whether a rename may give the host's profile the new name as its
/// hostname: only a workstation is applied, which is what sets it.
pub fn hostname_allowed(name: &str, profile: &Value) -> Result<()> {
    match crate::host_type::of(profile) {
        crate::host_type::HostType::Workstation => Ok(()),
        host_type => Err(format!(
            "{name} is a host of the type {host_type}, and only a workstation is given its hostname by berth"
        )
        .into()),
    }
}

/// Give a saved host another name in its scope. Everything that belongs to
/// it, its generated passwords, what berth knows of its SSH, its passwords
/// for logging in, its device on the tailnet, its resources, messages and
/// jobs, stays with it, as it is kept by the host's id. The machine stays as
/// it is, and so does its hostname unless `hostname` asks for the profile's
/// to be the new name, which an apply then gives the machine and its device
/// on the tailnet. Returns the new place.
pub fn rename(paths: &Paths, from: &Place, to: &str, hostname: bool) -> Result<Place> {
    let Place::Stored {
        scope,
        id,
        name: old,
    } = from
    else {
        return Err("A profile in a file is renamed by moving the file".into());
    };
    if !valid_name(to) {
        return Err(format!("Not a host name: {to}").into());
    }
    if old == to {
        return Err(format!("{old} has that name already").into());
    }
    // Nothing is done to the host meanwhile, such as creating its machine.
    let held = crate::runner::lock_at(&from.lock_file(paths), "rename the host", |holder| {
        format!("Something else is being done to {old}{holder}. Try again when it has ended")
    })?;
    let store = store::shared(paths)?;
    let mut profile: Value = store
        .read_json(kind::PROFILE, scope, id)?
        .ok_or_else(|| format!("No host {old}"))?;
    if Place::find(paths, scope, to)?.is_some() {
        return Err(format!("There is a host {to} already").into());
    }
    if hostname {
        hostname_allowed(old, &profile)?;
        profile["hostname"] = serde_json::json!(to);
    }
    let mut batch = store::Batch::default();
    batch.write_labelled(kind::PROFILE, scope, id, to, serde_json::to_vec(&profile)?);
    // The device on the tailnet takes the new hostname at the next apply,
    // when the machine has it too.
    if hostname && berth_api::profile::joins_tailnet(&profile) {
        let mut kept = crate::tailnet::Kept::read(&store, scope, id)?;
        kept.rename = Some(to.to_owned());
        batch.write(kind::TAILNET, scope, id, serde_json::to_vec(&kept)?);
    }
    match store.apply(&batch) {
        Err(store::Error::Taken { .. }) => {
            return Err(format!("There is a host {to} already").into());
        }
        result => result?,
    }
    // ssh's file for the host is written from its row under the new alias.
    let _ = fs::remove_file(crate::keys::known::file(paths, from));
    drop(held);
    Ok(Place::at(scope, id, to))
}

/// What a copy of `profile` under the name `to` keeps: everything but what
/// belongs to the one machine. Its hostname goes, so the copy takes its own
/// name; so do its address and the command that reaches it, which is also
/// how it is known on a tailnet; and so do the hashes of the passwords berth
/// generated for its accounts (`generated` names those accounts), whose
/// passwords stay with the original. A hash that was typed stays.
pub fn copy_of(profile: &Value, generated: &[String]) -> Value {
    let mut copy = profile.clone();
    let Some(fields) = copy.as_object_mut() else {
        return copy;
    };
    fields.remove("hostname");
    for (account, hash, default) in [
        ("operator_username", "operator_password_hash", "ops"),
        ("agent_username", "agent_password_hash", "agent"),
    ] {
        let user = profile[account].as_str().unwrap_or(default);
        if generated.iter().any(|known| known == user) {
            fields.remove(hash);
        }
    }
    if let Some(access) = fields.get_mut("access").and_then(Value::as_object_mut) {
        access.remove("address");
        access.remove("link");
    }
    copy
}

/// Save a copy of the saved host at `from` as the host `to`, as
/// [`copy_of`] makes it. Nothing else of the host comes along: not its
/// generated passwords, host keys, the passwords berth keeps for logging in
/// or its jobs. A machine a provisioner made for it, or one planned, is
/// planned for the copy with the same provisioner and arguments, to be made
/// when the copy's machine is created. Returns the copy's place, and the
/// record of its planned machine.
pub fn duplicate(
    paths: &Paths,
    from: &Place,
    to: &str,
) -> Result<(Place, Option<crate::resource::Kept>)> {
    let Place::Stored {
        scope, name: old, ..
    } = from
    else {
        return Err("A profile in a file is copied by copying the file".into());
    };
    if old == to {
        return Err(format!("{old} has that name already").into());
    }
    if Place::find(paths, scope, to)?.is_some() {
        return Err(format!("There is a host {to} already").into());
    }
    let place = Place::new(scope, to)?;
    let _new = crate::runner::lock_at(&place.lock_file(paths), "duplicate the host", |holder| {
        format!("Something else is being done to {to}{holder}. Try again when it has ended")
    })?;
    let profile = from.load(paths)?.ok_or_else(|| format!("No host {old}"))?;
    let generated: Vec<String> = crate::credentials::load(paths, from)?
        .keys()
        .cloned()
        .collect();
    let copy = copy_of(&profile, &generated);
    let machine = crate::resource::of_place(paths, from)?;
    place.save(paths, &copy)?;
    let planned = match machine {
        Some(machine) => Some(crate::provision::plan(
            paths,
            &place,
            &machine.record.provisioner,
            &machine.record.arguments,
        )?),
        None => None,
    };
    Ok((place, planned))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scratch() -> (tempfile::TempDir, Paths) {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: home.path().join("config"),
            data_dir: home.path().join("data"),
        };
        (home, paths)
    }

    #[test]
    fn a_name_is_a_saved_host_and_a_path_a_file() {
        let (home, paths) = scratch();
        let resolved = Place::resolve(&paths, "studio").unwrap();
        assert!(
            matches!(&resolved, Place::Stored { scope, id, name }
                if scope == OWN && name == "studio" && berth_api::ids::is_id(id)),
            "{resolved:?}"
        );
        let file = Place::resolve(&paths, "../studio.json").unwrap();
        assert!(matches!(&file, Place::File(path) if path.is_absolute()));
        assert_eq!(file.title(), "studio");
        for bad in [
            "-x",
            ".hidden",
            "a b",
            "",
            "01972c4e8f3a7b219d4e5a6b7c8d9e0f",
            "host_01972c4e8f3a7b219d4e5a6b7c8d9e0f",
        ] {
            assert!(Place::resolve(&paths, bad).is_err(), "{bad}");
        }
        // The id of another kind is refused.
        let error = Place::resolve(&paths, "prov_01972c4e8f3a7b219d4e5a6b7c8d9e0f").unwrap_err();
        assert!(error.to_string().contains("provisioner"), "{error}");
        // A berth server set for the commands leaves the saved hosts its own.
        paths.set("server", "http://192.0.2.7:7777").unwrap();
        let own = Place::resolve(&paths, "studio").unwrap();
        assert_eq!(own.server_url(), None);
        let path = home.path().join("x.json");
        assert_eq!(
            Place::resolve(&paths, path.to_str().unwrap()).unwrap(),
            Place::File(path)
        );
    }

    #[test]
    fn profiles_are_loaded_saved_listed_and_removed_in_their_place() {
        let (home, paths) = scratch();
        let own = Place::own(&paths, "studio").unwrap();
        let served = Place::stored(&paths, "served:http://192.0.2.7:7777", "studio").unwrap();
        let file = Place::File(home.path().join("elsewhere/attic.json"));
        for (place, address) in [
            (&own, "192.0.2.10"),
            (&served, "192.0.2.20"),
            (&file, "192.0.2.30"),
        ] {
            assert_eq!(place.load(&paths).unwrap(), None);
            place
                .save(&paths, &json!({"access": {"address": address}}))
                .unwrap();
            let loaded = place.load(&paths).unwrap().unwrap();
            assert_eq!(loaded["access"]["address"], address);
        }
        // Each scope lists its own; a file is no saved host.
        let names = |scope: &str| -> Vec<String> {
            list(&paths, scope)
                .unwrap()
                .into_iter()
                .map(|(place, _)| place.argument())
                .collect()
        };
        assert_eq!(names(OWN), ["studio"]);
        assert_eq!(names("served:http://192.0.2.7:7777"), ["studio"]);
        assert!(names(FILE_SCOPE).is_empty());
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            fs::metadata(home.path().join("elsewhere/attic.json"))
                .unwrap()
                .permissions()
                .mode()
        };
        assert_eq!(mode & 0o777, 0o600);

        // The side data of a file is found again by the file's path.
        crate::credentials::save(&paths, &file, "ops", "made-up-1").unwrap();
        let again = Place::resolve(
            &paths,
            home.path().join("elsewhere/attic.json").to_str().unwrap(),
        )
        .unwrap();
        assert_eq!(
            crate::credentials::load(&paths, &again).unwrap()["ops"],
            "made-up-1"
        );
        crate::credentials::save(&paths, &own, "agent", "made-up-2").unwrap();
        assert!(
            crate::credentials::load(&paths, &served)
                .unwrap()
                .is_empty()
        );

        // Removing one leaves the others.
        remove(&paths, &own).unwrap();
        assert_eq!(own.load(&paths).unwrap(), None);
        assert!(crate::credentials::load(&paths, &own).unwrap().is_empty());
        assert!(served.load(&paths).unwrap().is_some());
        remove(&paths, &file).unwrap();
        assert!(!home.path().join("elsewhere/attic.json").exists());
        assert!(crate::credentials::load(&paths, &again).unwrap().is_empty());
    }

    #[test]
    fn a_renamed_host_keeps_its_id_and_what_belongs_to_it() {
        let (_home, paths) = scratch();
        let test = Place::own(&paths, "test").unwrap();
        let id = test.key();
        let profile = json!({"hostname": "test", "access": {"address": "192.0.2.10"}});
        test.save(&paths, &profile).unwrap();
        crate::credentials::save(&paths, &test, "ops", "made-up-1").unwrap();
        let store = store::shared(&paths).unwrap();
        store
            .write_json(
                kind::ACCESS,
                OWN,
                &id,
                &json!({"host_keys": ["ssh-ed25519 AAAA"]}),
            )
            .unwrap();
        let secrets = crate::registry::Store::own(store.clone());
        secrets.set_secret("ssh/test", Some("made-up-2")).unwrap();
        Place::own(&paths, "lab")
            .unwrap()
            .save(&paths, &json!({"access": {"address": "192.0.2.11"}}))
            .unwrap();
        secrets.set_secret("ssh/lab", Some("made-up-3")).unwrap();
        let machine = crate::resource::Record {
            id: crate::store::new_id(),
            provisioner: "incus".into(),
            kind: crate::plugins::MACHINE.into(),
            name: "berth-test".into(),
            host: id.clone(),
            ..Default::default()
        };
        store
            .write_json(
                kind::RESOURCE,
                OWN,
                &machine.id,
                &serde_json::to_value(&machine).unwrap(),
            )
            .unwrap();
        let known = crate::keys::known::file(&paths, &test);
        fs::create_dir_all(known.parent().unwrap()).unwrap();
        fs::write(&known, "berth.test ssh-ed25519 AAAA\n").unwrap();

        let studio = rename(&paths, &test, "studio", false).unwrap();
        assert_eq!(studio, Place::own(&paths, "studio").unwrap());
        assert_eq!(studio.key(), id);
        // The name is free, and the id finds the host by its new name.
        assert_eq!(Place::find(&paths, OWN, "test").unwrap(), None);
        assert_eq!(Place::find(&paths, OWN, &id).unwrap(), Some(studio.clone()));
        assert_eq!(
            Place::find(&paths, OWN, &format!("host_{id}")).unwrap(),
            Some(studio.clone())
        );
        // The hostname stays unless asked for.
        assert_eq!(studio.load(&paths).unwrap().unwrap(), profile);
        assert_eq!(
            crate::credentials::load(&paths, &studio).unwrap()["ops"],
            "made-up-1"
        );
        let record = crate::keys::known::Record::load(&store, &studio).unwrap();
        assert_eq!(record.host_keys, ["ssh-ed25519 AAAA"]);
        assert_eq!(
            secrets.secret("ssh/studio").unwrap().as_deref(),
            Some("made-up-2")
        );
        assert_eq!(secrets.secret("ssh/test").unwrap(), None);
        assert_eq!(
            secrets.secret("ssh/lab").unwrap().as_deref(),
            Some("made-up-3")
        );
        let kept = crate::resource::of_place(&paths, &studio).unwrap().unwrap();
        assert_eq!(
            (kept.record.id.as_str(), kept.record.name.as_str()),
            (machine.id.as_str(), "berth-test")
        );
        assert!(!known.exists());

        // With hostname, the profile's is the new name.
        let lab = rename(&paths, &studio, "pier", true).unwrap();
        assert_eq!(lab.load(&paths).unwrap().unwrap()["hostname"], "pier");

        // Removing it removes what belongs to it, and leaves the other host's.
        remove(&paths, &lab).unwrap();
        assert!(crate::credentials::load(&paths, &lab).unwrap().is_empty());
        assert_eq!(secrets.secret("ssh/pier").unwrap(), None);
        assert_eq!(store.read(kind::ACCESS, OWN, &id).unwrap(), None);
        assert_eq!(
            secrets.secret("ssh/lab").unwrap().as_deref(),
            Some("made-up-3")
        );
    }

    #[test]
    fn a_copy_leaves_out_what_belongs_to_the_one_machine() {
        let profile = json!({
            "hostname": "studio", "target": "cloud", "operator_username": "ops",
            "operator_password_hash": "$6$made", "agent_password_hash": "$6$typed",
            "ansible_vars": {"devbox_plugin_features": ["claude/cli"]},
            "access": {"address": "studio.example.ts.net", "user": "ops", "auth": "berth",
                       "link": ["docker", "exec", "-i", "studio", "berthd", "connect"]},
            "power": {"stop": {"by": "controller"}},
            "secrets": ["github_token"],
        });
        let copy = copy_of(&profile, &["ops".into()]);
        assert_eq!(
            copy,
            json!({
                "target": "cloud", "operator_username": "ops", "agent_password_hash": "$6$typed",
                "ansible_vars": {"devbox_plugin_features": ["claude/cli"]},
                "access": {"user": "ops", "auth": "berth"},
                "power": {"stop": {"by": "controller"}},
                "secrets": ["github_token"],
            })
        );
        // The agent's generated password goes too, under the default name.
        let copy = copy_of(&profile, &["ops".into(), "agent".into()]);
        assert!(copy.get("agent_password_hash").is_none());
    }

    #[test]
    fn a_duplicate_is_a_new_host_with_its_machine_planned() {
        let (_home, paths) = scratch();
        let studio = Place::own(&paths, "studio").unwrap();
        studio
            .save(
                &paths,
                &json!({"hostname": "studio", "access": {"address": "192.0.2.10", "user": "ops"}}),
            )
            .unwrap();
        crate::credentials::save(&paths, &studio, "ops", "made-up-1").unwrap();
        let store = store::shared(&paths).unwrap();
        store
            .write_json(
                kind::ACCESS,
                OWN,
                &studio.key(),
                &json!({"host_keys": ["ssh-ed25519 AAAA"]}),
            )
            .unwrap();
        let machine = crate::resource::Record {
            id: "01972c4e8f3a7b219d4e5a6b7c8d9e0f".into(),
            provisioner: "incus".into(),
            kind: crate::plugins::MACHINE.into(),
            name: "studio".into(),
            host: studio.key(),
            external_id: "berth-studio".into(),
            arguments: [("cpus".to_owned(), "4".to_owned())].into(),
            state: "running".into(),
            ..Default::default()
        };
        store
            .write_json(
                kind::RESOURCE,
                OWN,
                &machine.id,
                &serde_json::to_value(&machine).unwrap(),
            )
            .unwrap();

        let (lab, planned) = duplicate(&paths, &studio, "lab").unwrap();
        assert_eq!(lab, Place::own(&paths, "lab").unwrap());
        assert_eq!(
            lab.load(&paths).unwrap().unwrap(),
            json!({"access": {"user": "ops"}})
        );
        // The original keeps all it had.
        assert_eq!(studio.load(&paths).unwrap().unwrap()["hostname"], "studio");
        assert!(crate::credentials::load(&paths, &lab).unwrap().is_empty());
        assert_eq!(store.read(kind::ACCESS, OWN, &lab.key()).unwrap(), None);
        let planned = planned.unwrap();
        let kept = crate::resource::of_place(&paths, &lab).unwrap().unwrap();
        assert_eq!(kept, planned);
        assert_eq!(
            (
                kept.record.provisioner.as_str(),
                kept.record.name.as_str(),
                kept.record.state.as_str()
            ),
            ("incus", "lab", crate::resource::PLANNED)
        );
        assert!(kept.record.external_id.is_empty());
        assert_eq!(kept.record.arguments["cpus"], "4");
        assert_ne!(kept.record.id, machine.id);
        assert_eq!(
            crate::resource::of_place(&paths, &studio)
                .unwrap()
                .unwrap()
                .record
                .external_id,
            "berth-studio"
        );
        // A name that is taken is refused, and so is a copy under its own name.
        let taken = duplicate(&paths, &studio, "lab").unwrap_err();
        assert_eq!(taken.to_string(), "There is a host lab already");
        assert!(duplicate(&paths, &studio, "studio").is_err());
        assert!(duplicate(&paths, &Place::own(&paths, "none").unwrap(), "other").is_err());
    }

    #[test]
    fn a_rename_needs_a_host_a_free_name_and_a_saved_host() {
        let (home, paths) = scratch();
        let test = Place::own(&paths, "test").unwrap();
        let studio = Place::own(&paths, "studio").unwrap();
        let error = |result: Result<Place>| result.unwrap_err().to_string();
        assert_eq!(
            error(rename(&paths, &test, "studio", false)),
            "No host test"
        );
        test.save(&paths, &json!({"access": {"address": "192.0.2.10"}}))
            .unwrap();
        studio
            .save(&paths, &json!({"access": {"address": "192.0.2.20"}}))
            .unwrap();
        assert_eq!(
            error(rename(&paths, &test, "studio", false)),
            "There is a host studio already"
        );
        assert_eq!(
            error(rename(&paths, &test, "a b", false)),
            "Not a host name: a b"
        );
        assert_eq!(
            error(rename(&paths, &test, "test", false)),
            "test has that name already"
        );
        // Only a workstation is given a hostname by berth.
        let dock = Place::own(&paths, "dock").unwrap();
        dock.save(
            &paths,
            &json!({"type": "ssh", "access": {"address": "192.0.2.30"}}),
        )
        .unwrap();
        assert_eq!(
            error(rename(&paths, &dock, "pier", true)),
            "dock is a host of the type ssh, and only a workstation is given its hostname by berth"
        );
        assert!(dock.load(&paths).unwrap().is_some());
        let file = Place::File(home.path().join("attic.json"));
        assert!(rename(&paths, &file, "attic2", false).is_err());
        // Something being done to the host, such as creating its machine, stops it.
        let held =
            crate::runner::lock_at(&test.lock_file(&paths), "test", |_| String::new()).unwrap();
        assert!(
            error(rename(&paths, &test, "lab", false))
                .starts_with("Something else is being done to test")
        );
        drop(held);
        assert!(test.load(&paths).unwrap().is_some());
    }

    #[test]
    fn each_place_has_a_lock_file_of_its_own() {
        let (home, paths) = scratch();
        let files = [
            Place::own(&paths, "studio").unwrap(),
            Place::stored(&paths, "served:http://192.0.2.7:7777", "studio").unwrap(),
            Place::stored(&paths, "served:http://192.0.2.8:7777", "studio").unwrap(),
            Place::File(home.path().join("studio.json")),
        ]
        .map(|place| place.lock_file(&paths));
        for (at, file) in files.iter().enumerate() {
            assert_eq!(file.parent().unwrap(), paths.data_dir.join("locks"));
            assert!(!files[at + 1..].contains(file), "{file:?}");
        }
        // A saved host's is by its id, which a rename leaves as it is.
        let own = files[0].file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            berth_api::ids::is_id(own.trim_end_matches(".lock")),
            "{own}"
        );
    }
}
