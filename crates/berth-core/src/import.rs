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
    Ok(())
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
    for (kept, what) in [
        (crate::kept::own_key(paths), "berth's own SSH key"),
        (crate::kept::serve_token(paths), "the token of berth serve"),
    ] {
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

    fn read(store: &Store, kind: &str, name: &str) -> Option<String> {
        let value = store.read(kind, OWN, name).unwrap()?;
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
    fn a_new_installation_gets_only_the_marker() {
        let (_dir, paths) = scratch();
        let store = open(&paths);
        let report = store.seal_report().unwrap();
        assert_eq!((report.sealed, report.plain), (0, 3));
        assert!(read(&store, kind::META, MARKER).is_some());
        assert!(read(&store, kind::META, PROFILES_MARKER).is_some());
        assert!(read(&store, kind::META, KEYS_MARKER).is_some());
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
        // Two secrets, a registry, two settings and the three markers.
        assert_eq!((report.sealed, report.plain), (8, 0));
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

    fn json_row(store: &Store, kind: &str, name: &str) -> Option<Value> {
        store.read_json(kind, OWN, name).unwrap()
    }

    #[test]
    fn the_hosts_move_into_the_store() {
        let (_dir, paths) = scratch();
        old_hosts(&paths);
        let store = open(&paths);
        assert_eq!(
            json_row(&store, kind::PROFILE, "studio").unwrap(),
            serde_json::from_str::<Value>(STUDIO).unwrap()
        );
        assert!(json_row(&store, kind::PROFILE, "attic").is_some());
        assert_eq!(
            json_row(&store, kind::CREDENTIALS, "studio").unwrap()["ops"],
            "made-up-pw"
        );
        let record: crate::provision::Record =
            serde_json::from_value(json_row(&store, kind::INSTANCE, "studio").unwrap()).unwrap();
        assert_eq!((record.id.as_str(), record.token.as_str()), ("vm-1", "t0k"));
        assert_eq!(
            store.names(kind::PROFILE, OWN).unwrap(),
            ["attic", "studio"]
        );
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
        assert_eq!(
            json_row(&store, kind::INSTANCE, "studio").unwrap()["id"],
            "vm-2"
        );
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
        assert_eq!(
            store.names(kind::CREDENTIALS, OWN).unwrap(),
            ["attic", "studio"]
        );
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
        assert_eq!(read(&store, kind::KEY, "ssh").unwrap(), "private");
        assert_eq!(
            read(&store, kind::KEY, "ssh.pub").unwrap(),
            "ssh-ed25519 AAAA berth@studio"
        );
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
}
