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
    /// A saved host of this installation or of the berth server it uses.
    Stored { scope: String, name: String },
    /// A profile in a file the user named; berth reads and writes that file.
    File(PathBuf),
}

/// The scope of the saved hosts: this installation's own. A client of a
/// server elsewhere keeps none.
pub fn scope(_: &Paths) -> String {
    OWN.to_owned()
}

/// Whether a name can be a saved host's: it is a file name of its own on
/// every machine, and no option of a command.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(['-', '.'])
        && !name.ends_with(".json")
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "-_.".contains(ch))
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
    /// path is a file, and a name is a saved host of [`scope`].
    pub fn resolve(paths: &Paths, host: &str) -> Result<Place> {
        if host.contains('/') || host.ends_with(".json") {
            return Ok(Place::File(std::path::absolute(host)?));
        }
        Place::stored(&scope(paths), host)
    }

    /// A saved host of a scope, by a name that is checked.
    pub fn stored(scope: &str, name: &str) -> Result<Place> {
        if !valid_name(name) {
            return Err(format!("Not a host name: {name}").into());
        }
        Ok(Place::Stored {
            scope: scope.to_owned(),
            name: name.to_owned(),
        })
    }

    /// A saved host of this installation's own.
    pub fn own(name: &str) -> Result<Place> {
        Place::stored(OWN, name)
    }

    /// What names the host to another berth command: the saved name, or the
    /// file's absolute path.
    pub fn argument(&self) -> String {
        match self {
            Place::Stored { name, .. } => name.clone(),
            Place::File(path) => path.to_string_lossy().into_owned(),
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

    /// The scope and name of what is kept beside the profile.
    pub fn side(&self) -> (String, String) {
        match self {
            Place::Stored { scope, name } => (scope.clone(), name.clone()),
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

    /// The profile, or `None` when there is none.
    pub fn load(&self, paths: &Paths) -> Result<Option<Value>> {
        match self {
            Place::Stored { scope, name } => {
                let mut profile: Option<Value> =
                    store::shared(paths)?.read_json(kind::PROFILE, scope, name)?;
                // A row an older berth or berth server wrote after the store's
                // names were converted is read with the names of now.
                if let Some(profile) = &mut profile {
                    berth_api::profile::take_old_names(profile);
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
                    Ok(Some(profile))
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(format!("{}: {error}", path.display()).into()),
            },
        }
    }

    /// Save the profile in place of the one there was. A file is written
    /// privately and in one step.
    pub fn save(&self, paths: &Paths, profile: &Value) -> Result<()> {
        match self {
            Place::Stored { scope, name } => {
                Ok(store::shared(paths)?.write_json(kind::PROFILE, scope, name, profile)?)
            }
            Place::File(path) => {
                let text = serde_json::to_string_pretty(profile)? + "\n";
                crate::paths::write_private(path, text.as_bytes())
            }
        }
    }

    /// The file whose lock is held while something is done to the host's
    /// machine. A lock is held by a process and its children for as long as
    /// a provisioning takes, which a row of the store cannot be.
    pub fn lock_file(&self, paths: &Paths) -> PathBuf {
        let name = match self {
            Place::Stored { scope, name } if scope == OWN => format!("{name}.lock"),
            Place::Stored { scope, name } => format!("{}.{name}.lock", digest(scope, 8)),
            Place::File(path) => format!("file.{}.lock", digest(&path.to_string_lossy(), 16)),
        };
        paths.data_dir.join("locks").join(name)
    }
}

/// The saved hosts of a scope with their profiles, by name.
pub fn list(paths: &Paths, scope: &str) -> Result<Vec<(String, Value)>> {
    let entries = store::shared(paths)?.entries(kind::PROFILE, scope)?;
    Ok(entries
        .into_iter()
        .filter_map(|(name, value)| {
            let mut profile = serde_json::from_slice(&value).ok()?;
            berth_api::profile::take_old_names(&mut profile);
            Some((name, profile))
        })
        .collect())
}

/// Forget a host: its profile, its generated passwords, the record of a
/// machine of it that was never made, what berth knows of its SSH and the passwords it keeps for
/// logging in, in one change. A profile in a file is removed with the file.
pub fn remove(paths: &Paths, place: &Place) -> Result<()> {
    let (scope, name) = place.side();
    // The provisioners a node offers go with it, unless they still hold resources.
    if scope == OWN {
        let secrets = crate::registry::Store::own(store::shared(paths)?);
        crate::node::forget(paths, &secrets, &name)?;
    }
    let password = crate::keys::password_secret(place);
    let sudo = crate::keys::sudo_secret(place);
    // The record of a machine that was never made goes with the host; a
    // made one is refused before this.
    let machine = crate::resource::of_place(paths, place)?
        .map(|kept| kept.record.id)
        .unwrap_or_default();
    let mut removals = vec![
        (kind::CREDENTIALS, scope.as_str(), name.as_str()),
        (kind::ACCESS, scope.as_str(), name.as_str()),
    ];
    if !machine.is_empty() {
        removals.push((kind::RESOURCE, scope.as_str(), machine.as_str()));
        removals.push((kind::SPEC, scope.as_str(), machine.as_str()));
    }
    if scope == OWN || matches!(place, Place::File(_)) {
        removals.push((kind::SECRET, OWN, password.as_str()));
        removals.push((kind::SECRET, OWN, sudo.as_str()));
    }
    if let Place::Stored { .. } = place {
        removals.push((kind::PROFILE, scope.as_str(), name.as_str()));
    }
    store::shared(paths)?.change(&[], &removals)?;
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

/// Give a saved host another name in its scope: its profile, its generated
/// passwords, what berth knows of its SSH, the passwords it keeps for logging
/// in and the resources that belong to it, in one change. A host of this
/// installation's own takes its messages and jobs along. The machine stays as
/// it is, and so does its hostname unless `hostname` asks for the profile's to
/// be the new name, which an apply then gives the machine. Returns the new place.
pub fn rename(paths: &Paths, from: &Place, to: &str, hostname: bool) -> Result<Place> {
    let Place::Stored { scope, name: old } = from else {
        return Err("A profile in a file is renamed by moving the file".into());
    };
    let place = Place::stored(scope, to)?;
    if old == to {
        return Err(format!("{old} has that name already").into());
    }
    // Nothing is done to either name meanwhile, such as creating its machine.
    fn busy(name: &str) -> impl FnOnce(&str) -> String + '_ {
        move |holder| {
            format!("Something else is being done to {name}{holder}. Try again when it has ended")
        }
    }
    let held = crate::runner::lock_at(&from.lock_file(paths), "rename the host", busy(old))?;
    let _new = crate::runner::lock_at(&place.lock_file(paths), "rename the host", busy(to))?;
    let mut profile = from.load(paths)?.ok_or_else(|| format!("No host {old}"))?;
    if place.load(paths)?.is_some() {
        return Err(format!("There is a host {to} already").into());
    }
    if hostname {
        hostname_allowed(old, &profile)?;
        profile["hostname"] = serde_json::json!(to);
    }
    let store = store::shared(paths)?;
    let mut writes = vec![(
        kind::PROFILE,
        scope.as_str(),
        to,
        serde_json::to_vec(&profile)?,
    )];
    let mut removals = vec![(kind::PROFILE, scope.as_str(), old.as_str())];
    let mut moved = vec![
        (kind::CREDENTIALS, scope.clone(), old.clone(), to.to_owned()),
        (kind::ACCESS, scope.clone(), old.clone(), to.to_owned()),
    ];
    if scope == OWN {
        for secret in [crate::keys::password_secret, crate::keys::sudo_secret] {
            moved.push((kind::SECRET, OWN.into(), secret(from), secret(&place)));
        }
    }
    for (kind, scope, old, new) in &moved {
        if let Some(value) = store.read(kind, scope, old)? {
            writes.push((kind, scope.as_str(), new.as_str(), value));
            removals.push((kind, scope.as_str(), old.as_str()));
        }
    }
    // A resource keeps its id and its name at the provisioner; only the host
    // it belongs to is named anew.
    let mut resources = vec![];
    for (id, value) in store.entries(kind::RESOURCE, scope)? {
        let mut record: crate::resource::Record =
            serde_json::from_slice(&value).map_err(|error| {
                format!("The record of the resource {id} in berth's store: {error}")
            })?;
        if record.host == *old {
            record.host = to.to_owned();
            resources.push((id, serde_json::to_vec(&record)?));
        }
    }
    for (id, value) in &resources {
        writes.push((kind::RESOURCE, scope.as_str(), id.as_str(), value.clone()));
    }
    // The provisioners a node offers name it, and go on naming it.
    let mut offered = vec![];
    if scope == OWN {
        for (name, value) in store.entries(kind::PROVISIONER_ENTRY, OWN)? {
            let Ok(mut entry) = serde_json::from_slice::<crate::provisioner::Entry>(&value) else {
                continue;
            };
            if entry.node == *old {
                entry.node = to.to_owned();
                offered.push((name, serde_json::to_vec(&entry)?));
            }
        }
    }
    for (name, value) in &offered {
        writes.push((kind::PROVISIONER_ENTRY, OWN, name.as_str(), value.clone()));
    }
    match scope == OWN {
        true => store.rename_host(old, to, &writes, &removals)?,
        false => store.change(&writes, &removals)?,
    }
    // ssh's file for the host is written from its row under the new alias.
    let _ = fs::remove_file(crate::keys::known::file(paths, from));
    drop(held);
    let _ = fs::remove_file(from.lock_file(paths));
    Ok(place)
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
    let Place::Stored { scope, name: old } = from else {
        return Err("A profile in a file is copied by copying the file".into());
    };
    let place = Place::stored(scope, to)?;
    if old == to {
        return Err(format!("{old} has that name already").into());
    }
    let _new = crate::runner::lock_at(&place.lock_file(paths), "duplicate the host", |holder| {
        format!("Something else is being done to {to}{holder}. Try again when it has ended")
    })?;
    let profile = from.load(paths)?.ok_or_else(|| format!("No host {old}"))?;
    if place.load(paths)?.is_some() {
        return Err(format!("There is a host {to} already").into());
    }
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
        assert_eq!(
            Place::resolve(&paths, "studio").unwrap(),
            Place::Stored {
                scope: OWN.into(),
                name: "studio".into()
            }
        );
        let file = Place::resolve(&paths, "../studio.json").unwrap();
        assert!(matches!(&file, Place::File(path) if path.is_absolute()));
        assert_eq!(file.title(), "studio");
        for bad in ["-x", ".hidden", "a b", ""] {
            assert!(Place::resolve(&paths, bad).is_err(), "{bad}");
        }
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
        let own = Place::own("studio").unwrap();
        let served = Place::stored("served:http://192.0.2.7:7777", "studio").unwrap();
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
                .map(|(name, _)| name)
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
    fn a_renamed_host_takes_what_berth_keeps_of_it() {
        let (_home, paths) = scratch();
        let test = Place::own("test").unwrap();
        let profile = json!({"hostname": "test", "access": {"address": "192.0.2.10"}});
        test.save(&paths, &profile).unwrap();
        crate::credentials::save(&paths, &test, "ops", "made-up-1").unwrap();
        let store = store::shared(&paths).unwrap();
        store
            .write_json(
                kind::ACCESS,
                OWN,
                "test",
                &json!({"host_keys": ["ssh-ed25519 AAAA"]}),
            )
            .unwrap();
        store
            .write(kind::SECRET, OWN, "ssh/test", b"made-up-2")
            .unwrap();
        store
            .write(kind::SECRET, OWN, "ssh/lab", b"made-up-3")
            .unwrap();
        let machine = crate::resource::Record {
            id: "r-test".into(),
            provisioner: "incus".into(),
            kind: crate::plugins::MACHINE.into(),
            name: "berth-test".into(),
            host: "test".into(),
            ..Default::default()
        };
        store
            .write_json(
                kind::RESOURCE,
                OWN,
                "r-test",
                &serde_json::to_value(&machine).unwrap(),
            )
            .unwrap();
        let known = crate::keys::known::file(&paths, &test);
        fs::create_dir_all(known.parent().unwrap()).unwrap();
        fs::write(&known, "berth.test ssh-ed25519 AAAA\n").unwrap();

        let studio = rename(&paths, &test, "studio", false).unwrap();
        assert_eq!(studio, Place::own("studio").unwrap());
        assert_eq!(test.load(&paths).unwrap(), None);
        // The hostname stays unless asked for.
        assert_eq!(studio.load(&paths).unwrap().unwrap(), profile);
        assert_eq!(
            crate::credentials::load(&paths, &studio).unwrap()["ops"],
            "made-up-1"
        );
        assert!(crate::credentials::load(&paths, &test).unwrap().is_empty());
        let record = crate::keys::known::Record::load(&store, &studio).unwrap();
        assert_eq!(record.host_keys, ["ssh-ed25519 AAAA"]);
        assert_eq!(store.read(kind::ACCESS, OWN, "test").unwrap(), None);
        assert_eq!(
            store
                .read(kind::SECRET, OWN, "ssh/studio")
                .unwrap()
                .unwrap(),
            b"made-up-2"
        );
        assert_eq!(store.read(kind::SECRET, OWN, "ssh/test").unwrap(), None);
        assert_eq!(
            store.read(kind::SECRET, OWN, "ssh/lab").unwrap().unwrap(),
            b"made-up-3"
        );
        let kept = crate::resource::of_place(&paths, &studio).unwrap().unwrap();
        assert_eq!(
            (kept.record.id.as_str(), kept.record.name.as_str()),
            ("r-test", "berth-test")
        );
        assert!(crate::resource::of_place(&paths, &test).unwrap().is_none());
        assert!(!known.exists());

        // With hostname, the profile's is the new name.
        let lab = rename(&paths, &studio, "lab", true).unwrap();
        assert_eq!(lab.load(&paths).unwrap().unwrap()["hostname"], "lab");
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
        let studio = Place::own("studio").unwrap();
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
                "studio",
                &json!({"host_keys": ["ssh-ed25519 AAAA"]}),
            )
            .unwrap();
        let machine = crate::resource::Record {
            id: "r-studio".into(),
            provisioner: "incus".into(),
            kind: crate::plugins::MACHINE.into(),
            name: "studio".into(),
            host: "studio".into(),
            external_id: "berth-studio".into(),
            arguments: [("cpus".to_owned(), "4".to_owned())].into(),
            state: "running".into(),
            ..Default::default()
        };
        store
            .write_json(
                kind::RESOURCE,
                OWN,
                "r-studio",
                &serde_json::to_value(&machine).unwrap(),
            )
            .unwrap();

        let (lab, planned) = duplicate(&paths, &studio, "lab").unwrap();
        assert_eq!(lab, Place::own("lab").unwrap());
        assert_eq!(
            lab.load(&paths).unwrap().unwrap(),
            json!({"access": {"user": "ops"}})
        );
        // The original keeps all it had.
        assert_eq!(studio.load(&paths).unwrap().unwrap()["hostname"], "studio");
        assert!(crate::credentials::load(&paths, &lab).unwrap().is_empty());
        assert_eq!(store.read(kind::ACCESS, OWN, "lab").unwrap(), None);
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
        assert_ne!(kept.record.id, "r-studio");
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
        assert!(duplicate(&paths, &Place::own("none").unwrap(), "other").is_err());
    }

    #[test]
    fn a_rename_needs_a_host_a_free_name_and_a_saved_host() {
        let (home, paths) = scratch();
        let test = Place::own("test").unwrap();
        let studio = Place::own("studio").unwrap();
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
        let dock = Place::own("dock").unwrap();
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
            Place::own("studio").unwrap(),
            Place::stored("served:http://192.0.2.7:7777", "studio").unwrap(),
            Place::stored("served:http://192.0.2.8:7777", "studio").unwrap(),
            Place::File(home.path().join("studio.json")),
        ]
        .map(|place| place.lock_file(&paths));
        for (at, file) in files.iter().enumerate() {
            assert_eq!(file.parent().unwrap(), paths.data_dir.join("locks"));
            assert!(!files[at + 1..].contains(file), "{file:?}");
        }
        assert!(files[0].ends_with("studio.lock"));
    }
}
