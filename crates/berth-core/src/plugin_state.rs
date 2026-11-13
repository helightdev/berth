//! The user's own plugins in berth's store. They are installed in a folder,
//! `plugins/` of the configuration directory, which Ansible, the builder and
//! the provisioners use as it is. The store holds what rebuilds that folder
//! (rows of kind `plugin` in the installation's own scope):
//!
//! - `package.json` and `package-lock.json`, as npm wrote them
//! - `upload/NAME` for each uploaded .tgz that `package.json` still names
//! - `folder/ID.tgz` for each plugin copied in as a folder, without its
//!   `node_modules` and `.git`
//!
//! [`remember`] writes them after every change berth makes to the folder.
//! [`restore`] puts back what the folder lacks of them, as on a new machine
//! restored from the store; [`plan`] is what it decides, without npm.

use crate::Result;
use crate::paths::Paths;
use crate::plugins::{self, own_dir};
use crate::store::{OWN, Store, hold_lock, kind};
use berth_api::PluginOrigin;
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const PACKAGE: &str = "package.json";
pub const LOCK: &str = "package-lock.json";
const UPLOAD: &str = "upload/";
const FOLDER: &str = "folder/";
/// What npm writes for a package installed from the folder of uploads.
const UPLOADED: &str = "file:uploads/";

/// Whether a name can be one of the uploads a plugin directory keeps in
/// `uploads/`: a file name of its own, as berth gives them (`STAMP.tgz`). A
/// name in a stored `package.json` or a row of a shared store that is not one
/// would put a file outside that folder.
pub fn valid_upload(name: &str) -> bool {
    name.len() <= 64
        && name.ends_with(".tgz")
        && !name.starts_with(['.', '-'])
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_'))
}

fn read_json(file: &Path) -> Option<Value> {
    serde_json::from_slice(&fs::read(file).ok()?).ok()
}

fn dependencies(manifest: Option<&Value>) -> Map<String, Value> {
    manifest
        .and_then(|manifest| manifest["dependencies"].as_object())
        .cloned()
        .unwrap_or_default()
}

/// A plugin folder as a gzip tar, its path inside relative to `own`, with
/// what is followed through a link, and without `node_modules` and `.git`.
fn pack_folder(own: &Path, folder: &Path) -> Result<Vec<u8>> {
    let relative = folder.strip_prefix(own).unwrap_or(folder);
    let output = std::process::Command::new("tar")
        .args([
            "-czhf",
            "-",
            "--exclude=node_modules",
            "--exclude=.git",
            "-C",
        ])
        .arg(own)
        .arg("--")
        .arg(relative)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| format!("tar is needed to keep a plugin folder in the store: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "tar {}: {}",
            folder.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(output.stdout)
}

/// The rows the store keeps of a plugin directory, by name.
pub fn snapshot(own: &Path) -> Result<Vec<(String, Vec<u8>)>> {
    let mut rows = vec![];
    for name in [PACKAGE, LOCK] {
        match fs::read(own.join(name)) {
            Ok(data) => rows.push((name.to_owned(), data)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("{}: {error}", own.join(name).display()).into()),
        }
    }
    let wanted = dependencies(read_json(&own.join(PACKAGE)).as_ref());
    let named: BTreeSet<&str> = wanted
        .values()
        .filter_map(Value::as_str)
        .filter_map(|spec| spec.strip_prefix(UPLOADED))
        .collect();
    for name in named {
        if !valid_upload(name) {
            continue;
        }
        if let Ok(data) = fs::read(own.join("uploads").join(name)) {
            rows.push((format!("{UPLOAD}{name}"), data));
        }
    }
    for plugin in plugins::load(&[own.to_owned()]) {
        if plugin.origin(own) == PluginOrigin::Folder {
            rows.push((
                format!("{FOLDER}{}.tgz", plugin.id),
                pack_folder(own, &plugin.found)?,
            ));
        }
    }
    Ok(rows)
}

/// Write the plugin directory as it is now to the store, in place of what
/// the store had of it.
pub fn remember(paths: &Paths) -> Result<()> {
    let rows = snapshot(&own_dir(&paths.config_dir))?;
    let items: Vec<(&str, &str, Vec<u8>)> = rows
        .iter()
        .map(|(name, value)| (kind::PLUGIN, name.as_str(), value.clone()))
        .collect();
    let store = crate::store::shared(paths)?;
    store.replace(OWN, &[kind::PLUGIN], &items)?;
    Ok(())
}

/// How npm is to install what the plugin directory lacks.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum Install {
    #[default]
    Nothing,
    /// `npm ci`: the lock file was written into a directory without
    /// `node_modules`.
    Clean,
    /// `npm install` of everything `package.json` names.
    All,
    /// `npm install` of these, which the directory's `package.json` lacks.
    Packages(Vec<String>),
}

/// What [`restore`] would do.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Files to write, relative to the directory; none of them is there.
    pub files: Vec<(PathBuf, Vec<u8>)>,
    /// Plugin folders to unpack, by id.
    pub folders: Vec<(String, Vec<u8>)>,
    pub install: Install,
    /// The packages, as (name, spec), installed from a file or folder of the
    /// machine they were added on, which are not installed here.
    pub left_out: Vec<(String, String)>,
}

/// Whether a dependency's spec can be installed from what the store has:
/// anything but a file or folder of the machine it was added on, unless that
/// is one of the uploads.
fn restorable(spec: &str, uploads: &BTreeSet<String>) -> bool {
    match spec.strip_prefix("file:") {
        None => true,
        Some(_) => spec
            .strip_prefix(UPLOADED)
            .is_some_and(|name| valid_upload(name) && uploads.contains(name)),
    }
}

/// What npm is given to install a dependency.
fn install_spec(own: &Path, name: &str, spec: &str) -> String {
    if let Some(file) = spec.strip_prefix(UPLOADED) {
        // npm takes a path from the folder it runs in, not from --prefix.
        own.join("uploads").join(file).display().to_string()
    } else if spec.starts_with("https://") || spec.starts_with("http://") {
        spec.to_owned()
    } else {
        format!("{name}@{spec}")
    }
}

/// The lock file without the packages left out, so that npm takes it with
/// `package.json`.
fn lock_without(lock: &[u8], left_out: &[(String, String)]) -> Vec<u8> {
    let Ok(mut lock) = serde_json::from_slice::<Value>(lock) else {
        return lock.to_vec();
    };
    for (name, _) in left_out {
        if let Some(root) = lock["packages"][""]["dependencies"].as_object_mut() {
            root.remove(name);
        }
        if let Some(packages) = lock["packages"].as_object_mut() {
            let entry = packages.remove(&format!("node_modules/{name}"));
            // A folder npm installed as a link has an entry of its own.
            if let Some(target) = entry
                .as_ref()
                .filter(|entry| entry["link"] == true)
                .and_then(|entry| entry["resolved"].as_str())
            {
                packages.remove(target);
            }
        }
        if let Some(old) = lock["dependencies"].as_object_mut() {
            old.remove(name);
        }
    }
    serde_json::to_vec_pretty(&lock).unwrap_or_default()
}

/// What the plugin directory `own` lacks of what the store has, and how to
/// put it back; `None` when it lacks nothing. Nothing that is there is
/// overwritten or removed. Only the names of the store's rows and its
/// `package.json` are read unless there is something to put back.
pub fn plan(own: &Path, store: &Store) -> Result<Option<Plan>> {
    let names = store.names(kind::PLUGIN, OWN)?;
    if names.is_empty() {
        return Ok(None);
    }
    let read = |name: &str| store.read(kind::PLUGIN, OWN, name);
    let stored: Option<Value> = match names.iter().any(|name| name == PACKAGE) {
        true => read(PACKAGE)?.and_then(|data| serde_json::from_slice(&data).ok()),
        false => None,
    };
    let uploads: BTreeSet<String> = names
        .iter()
        .filter_map(|name| name.strip_prefix(UPLOAD))
        .filter(|name| valid_upload(name))
        .map(str::to_owned)
        .collect();
    let mut plan = Plan::default();
    let here = read_json(&own.join(PACKAGE));
    let installed = |name: &str| own.join("node_modules").join(name).join(PACKAGE).is_file();
    let mut wanted: Vec<(String, String)> = vec![];
    match &here {
        None if stored.is_some() => {
            let mut manifest = stored.clone().unwrap_or_default();
            for (name, spec) in dependencies(stored.as_ref()) {
                let spec = spec.as_str().unwrap_or("").to_owned();
                match restorable(&spec, &uploads) {
                    true => wanted.push((name, spec)),
                    false => plan.left_out.push((name, spec)),
                }
            }
            if let Some(dependencies) = manifest["dependencies"].as_object_mut() {
                dependencies.retain(|name, _| !plan.left_out.iter().any(|(out, _)| out == name));
            }
            let mut text = serde_json::to_vec_pretty(&manifest)?;
            text.push(b'\n');
            plan.files.push((PACKAGE.into(), text));
            let lock = match names.iter().any(|name| name == LOCK) {
                true => read(LOCK)?,
                false => None,
            };
            let clean = !own.join("node_modules").exists() && plan.left_out.is_empty();
            if let Some(lock) = lock.filter(|_| !own.join(LOCK).exists()) {
                plan.files
                    .push((LOCK.into(), lock_without(&lock, &plan.left_out)));
                if clean && !wanted.is_empty() {
                    plan.install = Install::Clean;
                }
            }
            if !wanted.is_empty() && plan.install == Install::Nothing {
                plan.install = Install::All;
            }
        }
        None => {}
        Some(here) => {
            let have = dependencies(Some(here));
            let mut missing = vec![];
            for (name, spec) in dependencies(stored.as_ref()) {
                // The directory's own package.json says what it was installed from.
                let spec = have.get(&name).unwrap_or(&spec);
                let spec = spec.as_str().unwrap_or("").to_owned();
                // One the directory lacks that cannot be installed here was
                // named when the directory was first restored.
                if (have.contains_key(&name) && installed(&name)) || !restorable(&spec, &uploads) {
                    continue;
                }
                missing.push(install_spec(own, &name, &spec));
                wanted.push((name, spec));
            }
            if !missing.is_empty() {
                plan.install = Install::Packages(missing);
            }
        }
    }
    for (_, spec) in &wanted {
        if let Some(file) = spec.strip_prefix(UPLOADED)
            && valid_upload(file)
            && !own.join("uploads").join(file).exists()
            && let Some(data) = read(&format!("{UPLOAD}{file}"))?
        {
            plan.files.push((PathBuf::from("uploads").join(file), data));
        }
    }
    let folders: Vec<&str> = names
        .iter()
        .filter_map(|name| name.strip_prefix(FOLDER)?.strip_suffix(".tgz"))
        .filter(|id| plugins::valid_name(id))
        .collect();
    if !folders.is_empty() {
        let found: BTreeSet<String> = plugins::load(&[own.to_owned()])
            .into_iter()
            .filter(|plugin| plugin.origin(own) == PluginOrigin::Folder)
            .map(|plugin| plugin.id)
            .collect();
        for id in folders.into_iter().filter(|id| !found.contains(*id)) {
            if let Some(data) = read(&format!("{FOLDER}{id}.tgz"))? {
                plan.folders.push((id.to_owned(), data));
            }
        }
    }
    let nothing =
        plan.files.is_empty() && plan.folders.is_empty() && plan.install == Install::Nothing;
    Ok((!nothing || !plan.left_out.is_empty()).then_some(plan))
}

/// Unpack a folder plugin's archive beside the others. A folder of the same
/// name that is there already stays as it is.
fn unpack(own: &Path, id: &str, archive: &[u8]) -> Result<Option<String>> {
    let into = tempfile::tempdir_in(own)?;
    let mut tar = std::process::Command::new("tar")
        .args(["-xzf", "-", "-C"])
        .arg(into.path())
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| format!("tar is needed to unpack the plugin {id}: {error}"))?;
    if let Some(mut stdin) = tar.stdin.take() {
        stdin.write_all(archive)?;
    }
    let output = tar.wait_with_output()?;
    if !output.status.success() {
        return Err(format!(
            "unpacking the plugin {id}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    for entry in fs::read_dir(into.path())?.flatten() {
        // A plugin's archive holds its folder, and nothing berth or npm
        // keep in the directory beside it.
        let name = entry.file_name().to_string_lossy().into_owned();
        let folder = entry.file_type().is_ok_and(|kind| kind.is_dir());
        if !folder || name.starts_with('.') || ["node_modules", "uploads"].contains(&name.as_str())
        {
            return Ok(Some(format!(
                "berth: the plugin {id} in berth's store is not unpacked, as its archive holds {name}, which is not a plugin's folder"
            )));
        }
        let to = own.join(entry.file_name());
        if to.exists() {
            return Ok(Some(format!(
                "berth: the plugin {id} in berth's store is not unpacked, as {} is there already",
                to.display()
            )));
        }
        fs::rename(entry.path(), &to)?;
    }
    Ok(None)
}

/// Do what a plan says in the plugin directory. Returns what to tell the
/// user; npm that cannot be run is one of those, not an error.
pub fn carry_out(own: &Path, plan: &Plan) -> Result<Vec<String>> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(own)?;
    let mut notices = vec![];
    for (file, data) in &plan.files {
        let path = own.join(file);
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        // Never over a file that is there.
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut out) => out.write_all(data)?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("{}: {error}", path.display()).into()),
        }
    }
    for (id, archive) in &plan.folders {
        notices.extend(unpack(own, id, archive)?);
    }
    if !plan.left_out.is_empty() {
        let names: Vec<&str> = plan
            .left_out
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        notices.push(format!(
            "berth: {} came from a file or folder of the machine they were added on, and are not installed here; add them again with berth plugins add",
            names.join(", ")
        ));
    }
    let mut args: Vec<OsString> = match &plan.install {
        Install::Nothing => return Ok(notices),
        Install::Clean => vec!["ci".into()],
        Install::All => vec!["install".into()],
        Install::Packages(_) => vec!["install".into()],
    };
    args.extend(
        ["--ignore-scripts", "--no-audit", "--no-fund", "--prefix"]
            .into_iter()
            .map(OsString::from),
    );
    args.push(own.as_os_str().to_owned());
    if let Install::Packages(packages) = &plan.install {
        args.push("--".into());
        args.extend(packages.iter().map(OsString::from));
    }
    let ran = std::process::Command::new("npm")
        .args(&args)
        .stdin(std::process::Stdio::null())
        .output();
    match ran {
        Err(error) => notices.push(format!(
            "berth: npm could not be started ({error}), so the plugins of berth's store that npm installs are not installed here; berth goes on without them"
        )),
        Ok(output) if !output.status.success() => notices.push(format!(
            "berth: npm could not install the plugins of berth's store, and berth goes on without them: {}",
            String::from_utf8_lossy(&output.stderr)
                .lines()
                .filter(|line| !line.trim().is_empty())
                .take(6)
                .collect::<Vec<_>>()
                .join(" / ")
        )),
        Ok(_) => notices.push(format!(
            "berth: installed the plugins of berth's store in {}",
            own.display()
        )),
    }
    Ok(notices)
}

/// Put back in the plugin directory what it lacks of the store's plugins.
/// Costs a look at the store's names and `package.json` when it lacks
/// nothing. Another berth doing the same meanwhile is left to it.
pub fn restore(paths: &Paths) -> Result<Vec<String>> {
    let own = own_dir(&paths.config_dir);
    let store = crate::store::shared(paths)?;
    if plan(&own, &store)?.is_none() {
        return Ok(vec![]);
    }
    let Some(_lock) = hold_lock(&paths.data_dir.join("locks/plugins.lock"))? else {
        return Ok(vec![]);
    };
    // Again, now that no other berth does it.
    match plan(&own, &store)? {
        Some(plan) => carry_out(&own, &plan),
        None => Ok(vec![]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: dir.path().join("config"),
            data_dir: dir.path().join("data"),
        };
        (dir, paths)
    }

    fn store(paths: &Paths) -> Store {
        Store::open(&crate::store::default_file(paths)).unwrap()
    }

    fn put(store: &Store, rows: &[(&str, &[u8])]) {
        let items: Vec<_> = rows
            .iter()
            .map(|(name, value)| (kind::PLUGIN, *name, value.to_vec()))
            .collect();
        store.replace(OWN, &[kind::PLUGIN], &items).unwrap();
    }

    /// A plugin copied in as a folder, with what is not kept of it.
    fn folder_plugin(own: &Path) {
        let dir = own.join("hand-made");
        fs::create_dir_all(dir.join("node_modules/dep")).unwrap();
        fs::create_dir_all(dir.join("tasks")).unwrap();
        fs::write(
            dir.join(PACKAGE),
            r#"{"name": "berth-plugin-local", "berth": {}}"#,
        )
        .unwrap();
        fs::write(dir.join("tasks/main.yml"), "- debug: {msg: hi}\n").unwrap();
        fs::write(dir.join("node_modules/dep/index.js"), "").unwrap();
    }

    const MANIFEST: &str = r#"{"dependencies": {
        "berth-plugin-a": "^1.0.0",
        "berth-plugin-b": "file:uploads/17.tgz",
        "berth-plugin-c": "file:/home/someone/src/c",
        "berth-plugin-d": "https://npm.example/d-1.0.0.tgz"}}"#;

    const LOCKED: &str = r#"{"lockfileVersion": 3, "packages": {
        "": {"dependencies": {"berth-plugin-a": "^1.0.0", "berth-plugin-c": "file:/home/someone/src/c"}},
        "node_modules/berth-plugin-a": {"version": "1.0.0"},
        "node_modules/berth-plugin-c": {"resolved": "../../src/c", "link": true},
        "../../src/c": {"name": "berth-plugin-c"}}}"#;

    #[test]
    fn a_snapshot_keeps_what_rebuilds_the_directory() {
        let (_dir, paths) = scratch();
        let own = own_dir(&paths.config_dir);
        fs::create_dir_all(own.join("uploads")).unwrap();
        fs::write(own.join(PACKAGE), MANIFEST).unwrap();
        fs::write(own.join(LOCK), LOCKED).unwrap();
        fs::write(own.join("uploads/17.tgz"), "upload").unwrap();
        fs::write(own.join("uploads/16.tgz"), "an older one").unwrap();
        folder_plugin(&own);
        remember(&paths).unwrap();
        let store = crate::store::shared(&paths).unwrap();
        assert_eq!(
            store.names(kind::PLUGIN, OWN).unwrap(),
            [
                "folder/local.tgz",
                "package-lock.json",
                "package.json",
                "upload/17.tgz"
            ]
        );
        assert_eq!(
            store.read(kind::PLUGIN, OWN, PACKAGE).unwrap().unwrap(),
            MANIFEST.as_bytes()
        );

        // In an empty directory the plan puts it all back but what came
        // from that machine's own folder.
        let (_other, elsewhere) = scratch();
        let fresh = own_dir(&elsewhere.config_dir);
        let plan = plan(&fresh, &store).unwrap().unwrap();
        let files: Vec<_> = plan.files.iter().map(|(file, _)| file.clone()).collect();
        assert_eq!(
            files,
            [
                PathBuf::from(PACKAGE),
                PathBuf::from(LOCK),
                PathBuf::from("uploads/17.tgz")
            ]
        );
        let written: Value = serde_json::from_slice(&plan.files[0].1).unwrap();
        let names: Vec<_> = dependencies(Some(&written)).keys().cloned().collect();
        assert_eq!(
            names,
            ["berth-plugin-a", "berth-plugin-b", "berth-plugin-d"]
        );
        let lock: Value = serde_json::from_slice(&plan.files[1].1).unwrap();
        assert!(lock["packages"]["node_modules/berth-plugin-c"].is_null());
        assert!(lock["packages"]["../../src/c"].is_null());
        assert!(lock["packages"]["node_modules/berth-plugin-a"].is_object());
        assert_eq!(
            plan.left_out,
            [("berth-plugin-c".into(), "file:/home/someone/src/c".into())]
        );
        // With a package left out the lock file is no longer npm's own.
        assert_eq!(plan.install, Install::All);
        assert_eq!(plan.folders.len(), 1);

        // Carried out without npm, the folder plugin is back whole, without
        // what is not kept of it.
        let mut quiet = plan.clone();
        quiet.install = Install::Nothing;
        let said = carry_out(&fresh, &quiet).unwrap();
        assert!(said[0].contains("berth-plugin-c"), "{said:?}");
        assert_eq!(
            fs::read_to_string(fresh.join("hand-made/tasks/main.yml")).unwrap(),
            "- debug: {msg: hi}\n"
        );
        assert!(!fresh.join("hand-made/node_modules").exists());
        assert_eq!(fs::read(fresh.join("uploads/17.tgz")).unwrap(), b"upload");
        // What is there now is not asked for again; npm's packages still are.
        let again = super::plan(&fresh, &store).unwrap().unwrap();
        assert!(again.files.is_empty() && again.folders.is_empty() && again.left_out.is_empty());
        let fresh_uploads = fresh.join("uploads/17.tgz").display().to_string();
        assert_eq!(
            again.install,
            Install::Packages(vec![
                "berth-plugin-a@^1.0.0".into(),
                fresh_uploads,
                "https://npm.example/d-1.0.0.tgz".into()
            ])
        );
    }

    #[test]
    fn a_directory_that_has_it_all_needs_nothing() {
        let (_dir, paths) = scratch();
        let store = store(&paths);
        let own = own_dir(&paths.config_dir);
        assert_eq!(plan(&own, &store).unwrap(), None);
        let manifest = r#"{"dependencies": {"berth-plugin-a": "^1.0.0"}}"#;
        put(&store, &[(PACKAGE, manifest.as_bytes()), (LOCK, b"{}")]);
        let plan_now = plan(&own, &store).unwrap().unwrap();
        assert_eq!(plan_now.install, Install::Clean);
        fs::create_dir_all(own.join("node_modules/berth-plugin-a")).unwrap();
        fs::write(own.join("node_modules/berth-plugin-a/package.json"), "{}").unwrap();
        fs::write(own.join(PACKAGE), manifest).unwrap();
        assert_eq!(plan(&own, &store).unwrap(), None);
        // A package the directory has a different spec for is its own business.
        fs::write(
            own.join(PACKAGE),
            r#"{"dependencies": {"berth-plugin-a": "^2.0.0", "berth-plugin-x": "1"}}"#,
        )
        .unwrap();
        assert_eq!(plan(&own, &store).unwrap(), None);
        // And without node_modules it is installed as the directory says.
        fs::remove_dir_all(own.join("node_modules")).unwrap();
        assert_eq!(
            plan(&own, &store).unwrap().unwrap().install,
            Install::Packages(vec!["berth-plugin-a@^2.0.0".into()])
        );
    }

    #[test]
    fn a_folder_of_the_same_name_stays() {
        let (_dir, paths) = scratch();
        let store = store(&paths);
        let own = own_dir(&paths.config_dir);
        folder_plugin(&own);
        let archive = pack_folder(&own, &own.join("hand-made")).unwrap();
        put(&store, &[("folder/other.tgz", &archive)]);
        let plan = plan(&own, &store).unwrap().unwrap();
        assert_eq!(plan.folders.len(), 1);
        let said = carry_out(&own, &plan).unwrap();
        assert!(said[0].contains("there already"), "{said:?}");
        assert!(own.join("hand-made/node_modules/dep/index.js").exists());
    }

    #[test]
    fn rows_of_a_shared_store_cannot_write_outside_the_directory() {
        let (dir, paths) = scratch();
        let store = store(&paths);
        let own = own_dir(&paths.config_dir);
        let manifest = r#"{"dependencies": {
            "berth-plugin-a": "file:uploads/../../../escaped.tgz",
            "berth-plugin-b": "file:uploads/-x.tgz",
            "berth-plugin-c": "file:uploads/17.tgz"}}"#;
        put(
            &store,
            &[
                (PACKAGE, manifest.as_bytes()),
                ("upload/../../../escaped.tgz", b"bad"),
                ("upload/-x.tgz", b"bad"),
                ("upload/17.tgz", b"good"),
                ("folder/../escaped.tgz", b"bad"),
            ],
        );
        let plan = plan(&own, &store).unwrap().unwrap();
        let files: Vec<_> = plan.files.iter().map(|(file, _)| file.clone()).collect();
        assert_eq!(
            files,
            [PathBuf::from(PACKAGE), PathBuf::from("uploads/17.tgz")]
        );
        assert!(plan.folders.is_empty());
        let left_out: Vec<_> = plan
            .left_out
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(left_out, ["berth-plugin-a", "berth-plugin-b"]);
        assert!(!dir.path().join("escaped.tgz").exists());
        assert!(valid_upload("1763000000000000000.tgz"));
        for bad in ["", "../x.tgz", "a/b.tgz", ".x.tgz", "-x.tgz", "x.tar"] {
            assert!(!valid_upload(bad), "{bad}");
        }
    }

    #[test]
    fn an_archive_holds_one_plugin_folder_and_nothing_else() {
        let (dir, paths) = scratch();
        let own = own_dir(&paths.config_dir);
        fs::create_dir_all(&own).unwrap();
        let made = dir.path().join("made");
        fs::create_dir_all(&made).unwrap();
        fs::write(made.join("package.json"), "{}").unwrap();
        let archive = pack_folder(&made, &made.join("package.json")).unwrap();
        let said = unpack(&own, "local", &archive).unwrap().unwrap();
        assert!(said.contains("not a plugin's folder"), "{said}");
        assert!(!own.join("package.json").exists());
    }
}
