//! The store of an installation: where its settings say it is, sealed with
//! the key file beside its configuration when there is one.
//!
//! A key is changed by writing the new one to `<file>.new`, rewriting every
//! row with it in one transaction, and then moving the current key to
//! `<file>.old` and the new one into its place. Opening the store finishes
//! or undoes a change that was interrupted.

use super::{Store, Target};
use crate::paths::Paths;
use crate::seal::{Key, Keys};
use std::collections::HashMap;
use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// The SQLite file of an installation that names no store.
pub fn default_file(paths: &Paths) -> PathBuf {
    paths.data_dir.join("berth.db")
}

/// Where an installation's store is and what it is opened with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installation {
    pub target: Target,
    /// The key file. The store is encrypted when it exists.
    pub key_file: PathBuf,
    /// Certificate authorities trusted as well for PostgreSQL over TLS.
    pub ca: Option<PathBuf>,
}

impl Installation {
    /// From the settings `store`, `store_key` and `store_ca`, where the
    /// environment wins as for every setting.
    pub fn of(paths: &Paths) -> Installation {
        Installation::with(paths, &|key| paths.setting(key))
    }

    /// From the settings as `setting` gives them.
    pub fn with(paths: &Paths, setting: &dyn Fn(&str) -> Option<String>) -> Installation {
        let target = Target::parse(setting("store").as_deref(), &default_file(paths));
        let key_file = match setting("store_key") {
            Some(file) => crate::discovery::expand_path(&file),
            None => paths.config_dir.join("store.key"),
        };
        let ca = setting("store_ca").map(|file| crate::discovery::expand_path(&file));
        Installation {
            target,
            key_file,
            ca,
        }
    }

    /// The previous key, still taken for reading.
    pub fn old_key_file(&self) -> PathBuf {
        beside(&self.key_file, "old")
    }

    /// A key being changed to.
    pub fn new_key_file(&self) -> PathBuf {
        beside(&self.key_file, "new")
    }

    /// Held while the key changes, so that no store opened meanwhile takes
    /// the change for an interrupted one.
    pub fn lock_file(&self) -> PathBuf {
        beside(&self.key_file, "lock")
    }

    /// Hold the lock of key changes; `None` when someone else holds it.
    pub fn lock_keys(&self) -> crate::Result<Option<fs::File>> {
        hold_lock(&self.lock_file())
    }

    /// Open the store with its keys, finishing or undoing a key change that
    /// was interrupted. A SQLite store an older berth wrote is copied first
    /// ([`keep_before_converting`]).
    pub fn open(&self) -> crate::Result<Store> {
        let _held = self.hold_converting()?;
        self.open_held()
    }

    /// Held while a store is copied and converted, so that two berths copy
    /// it once, before either converts it.
    fn hold_converting(&self) -> crate::Result<Option<fs::File>> {
        match &self.target {
            Target::Sqlite(file) => Ok(Some(wait_lock(
                &beside(file, "convert.lock"),
                "converting the store",
            )?)),
            Target::Postgres(_) => Ok(None),
        }
    }

    fn open_held(&self) -> crate::Result<Store> {
        if let Target::Sqlite(file) = &self.target
            && let Some(said) = keep_before_converting(file)?
        {
            eprintln!("berth: {said}");
        }
        let store = Store::connect_trusting(&self.target, self.ca.as_deref())?;
        let keys = self.keys(&store)?;
        Ok(store.with_keys(keys, Some(self.key_file.clone())))
    }

    fn keys(&self, store: &Store) -> crate::Result<Option<Keys>> {
        let mut current = Key::read(&self.key_file)?;
        let mut others: Vec<Key> = Key::read(&self.old_key_file())?.into_iter().collect();
        if let Some(new) = Key::read(&self.new_key_file())? {
            match self.lock_keys()? {
                // A change is under way: what it wrote already opens with the new key.
                None => others.push(new),
                Some(_lock) if store.key_ids()?.contains(&new.id()) => {
                    // The rows were rewritten; only the renames were left.
                    if current.is_some() {
                        fs::rename(&self.key_file, self.old_key_file())?;
                    }
                    fs::rename(self.new_key_file(), &self.key_file)?;
                    others.extend(current.take());
                    current = Some(new);
                }
                Some(_lock) => fs::remove_file(self.new_key_file())?,
            }
        }
        Ok(current.map(|current| Keys::new(current, others)))
    }
}

/// Hold an exclusive `flock` on the file, made if it is not there; `None`
/// when another holds it. A child this process forks holds the lock too until
/// it runs its program, so a lock that is taken is tried again for a moment.
pub fn hold_lock(path: &Path) -> crate::Result<Option<fs::File>> {
    if let Some(dir) = path.parent() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    let file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    for _ in 0..25 {
        // SAFETY: the descriptor is open; the lock goes when the file is closed.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(Some(file));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    Ok(None)
}

/// Hold the lock, waiting for it up to half a minute; `what` says what the
/// other berth is doing, for the error when it takes longer.
pub fn wait_lock(path: &Path, what: &str) -> crate::Result<fs::File> {
    // Each try waits half a second.
    for _ in 0..60 {
        if let Some(lock) = hold_lock(path)? {
            return Ok(lock);
        }
    }
    Err(format!(
        "Another berth has been {what} for a while; {} is held",
        path.display()
    )
    .into())
}

fn beside(file: &Path, suffix: &str) -> PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push(".");
    name.push(suffix);
    PathBuf::from(name)
}

type Opened = Mutex<HashMap<(PathBuf, PathBuf), Arc<Store>>>;

/// The store of this installation, opened once per process and data
/// directory. The first open in a process takes in what older versions of
/// berth kept in files, so nothing reads the store before that is done.
pub fn shared(paths: &Paths) -> crate::Result<Arc<Store>> {
    static OPENED: OnceLock<Opened> = OnceLock::new();
    let opened = OPENED.get_or_init(Default::default);
    // Held while opening, so that two threads do not both open it.
    let mut opened = opened
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let key = (paths.config_dir.clone(), paths.data_dir.clone());
    if let Some(store) = opened.get(&key) {
        return Ok(store.clone());
    }
    let store = Arc::new(open_converting(paths)?);
    opened.insert(key, store.clone());
    Ok(store)
}

/// Open the installation's store and take in what older versions kept. A
/// store an older berth wrote is copied first, as it was, for that berth to
/// go back to: once converted it reads it no more.
pub fn open_converting(paths: &Paths) -> crate::Result<Store> {
    let installation = Installation::of(paths);
    // Held until the imports are done too, which convert it as well.
    let _held = installation.hold_converting()?;
    let store = installation.open_held()?;
    if let Target::Postgres(_) = installation.target
        && let Some(old) = older_writer(&store)?
    {
        eprintln!(
            "berth: berth {} converts the store in PostgreSQL that berth {old} wrote, which berth {old} cannot read afterwards. Going back needs a dump made before, with pg_dump: {}",
            berth_api::VERSION,
            berth_api::compat::UPGRADE_DOCS
        );
    }
    crate::import::run(paths, &installation, &store)?;
    Ok(store)
}

/// The version of berth that last took the store in, when it is older than
/// this one: a marker of this berth's imports is missing.
fn older_writer(store: &Store) -> crate::Result<Option<String>> {
    let names = store.names(super::kind::META, super::OWN)?;
    // A store no berth took in yet is a new one.
    if !names.iter().any(|name| name == crate::import::MARKER) {
        return Ok(None);
    }
    if crate::import::MARKERS
        .iter()
        .all(|marker| names.iter().any(|name| name == marker))
    {
        return Ok(None);
    }
    let mut versions: Vec<String> = vec![];
    for name in &names {
        if let Some(value) = store.read_json(super::kind::META, super::OWN, name)?
            && let Some(version) = value["version"].as_str()
        {
            versions.push(version.to_owned());
        }
    }
    versions.sort_by_key(|version| version_key(version));
    Ok(Some(versions.pop().unwrap_or_else(|| "0.5".into())))
}

fn version_key(version: &str) -> Vec<u64> {
    version
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|part| part.parse().ok())
        .collect()
}

/// Before this berth converts a SQLite store an older one wrote, for the
/// first time, a consistent copy of it goes beside it, named for the schema
/// and the berth it had, in place of any copy made before. The key file is
/// not copied: the copy opens with the same one. Says what it did, or why
/// not, and nothing when the store needs no conversion.
pub fn keep_before_converting(file: &Path) -> crate::Result<Option<String>> {
    use rusqlite::{Connection, OpenFlags};
    if !file.is_file() {
        return Ok(None);
    }
    let db = Connection::open_with_flags(
        file,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    let schema: i64 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    // A file no berth wrote yet, or one of a newer berth, which opening refuses.
    if schema == 0 || schema as usize > super::sqlite::MIGRATIONS.len() {
        return Ok(None);
    }
    // A sealed store's values are not text, and then say no version.
    let markers: Vec<(String, Vec<u8>)> = match db.prepare(
        "SELECT name, value FROM state WHERE kind = 'meta' AND scope = '' AND name LIKE 'import:%'",
    ) {
        Ok(mut statement) => statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?,
        // A schema older than the table of state.
        Err(_) => vec![],
    };
    // An older schema, or a store an older berth took in: one that has the
    // first import's marker and lacks one of this berth's. A store with none
    // was made by this berth, as a copy or before its first import.
    let marked = |marker: &str| markers.iter().any(|(name, _)| name == marker);
    let older = (schema as usize) < super::sqlite::MIGRATIONS.len()
        || (marked(crate::import::MARKER) && !crate::import::MARKERS.iter().all(|m| marked(m)));
    if !older {
        return Ok(None);
    }
    let mut versions: Vec<String> = markers
        .iter()
        .filter_map(|(_, value)| {
            serde_json::from_slice::<serde_json::Value>(value).ok()?["version"]
                .as_str()
                .map(str::to_owned)
        })
        .collect();
    versions.sort_by_key(|version| version_key(version));
    let old = versions.pop();
    let suffix = match &old {
        Some(version) => format!("berth-{version}-schema-{schema}"),
        None => format!("schema-{schema}"),
    };
    let copy = beside(file, &suffix);
    let dir = file.parent().unwrap_or(Path::new("."));
    let wal = PathBuf::from(format!("{}-wal", file.display()));
    let size = fs::metadata(file)?.len() + fs::metadata(wal).map_or(0, |wal| wal.len());
    let named = match &old {
        Some(version) => format!("berth {version}"),
        None => "the older berth".to_owned(),
    };
    if free_space(dir).is_some_and(|free| free < size + size / 10) {
        return Ok(Some(format!(
            "{} has no room for a copy of the store ({size} bytes), so none was made before berth {} converts it. {named} cannot read it afterwards: {}",
            dir.display(),
            berth_api::VERSION,
            berth_api::compat::UPGRADE_DOCS
        )));
    }
    // Made empty and private first, as the copy holds what the store does.
    let partial = beside(file, &format!("{suffix}.partial"));
    let _ = fs::remove_file(&partial);
    {
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&partial)?;
    }
    if let Err(error) = db.execute("VACUUM INTO ?1", [partial.to_string_lossy()]) {
        let _ = fs::remove_file(&partial);
        return Ok(Some(format!(
            "No copy of the store was made before berth {} converts it ({error}). {named} cannot read it afterwards: {}",
            berth_api::VERSION,
            berth_api::compat::UPGRADE_DOCS
        )));
    }
    drop(db);
    fs::rename(&partial, &copy)?;
    // Only the latest copy is kept.
    let prefix = format!(
        "{}.",
        file.file_name().unwrap_or_default().to_string_lossy()
    );
    for entry in fs::read_dir(dir)?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(rest) = name.strip_prefix(&prefix) else {
            continue;
        };
        let earlier = (rest.starts_with("berth-") || rest.starts_with("schema-"))
            && rest.contains("schema-")
            && !rest.ends_with(".partial");
        if earlier && entry.path() != copy {
            let _ = fs::remove_file(entry.path());
        }
    }
    Ok(Some(format!(
        "before berth {} converts the store, a copy of it as {named} left it is in {}. {named} needs that copy to go back: {}",
        berth_api::VERSION,
        copy.display(),
        berth_api::compat::UPGRADE_DOCS
    )))
}

/// The bytes free for this user in `dir`, when the system says.
fn free_space(dir: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: the path is a C string and the struct is written by the call.
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    Some(stat.f_bavail as u64 * stat.f_frsize as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::kind;

    fn scratch() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: dir.path().join("config"),
            data_dir: dir.path().join("data"),
        };
        (dir, paths)
    }

    /// The settings of the files only, whatever the environment of the tests holds.
    fn installation(paths: &Paths) -> Installation {
        Installation::with(paths, &|key| paths.setting_with(key, None))
    }

    fn copy(key: &Key) -> Key {
        Key::from_hex(&key.to_hex()).unwrap()
    }

    #[test]
    fn the_settings_say_where_the_store_and_key_are() {
        let (dir, paths) = scratch();
        let found = installation(&paths);
        assert_eq!(
            found.target,
            Target::Sqlite(paths.data_dir.join("berth.db"))
        );
        assert_eq!(found.key_file, paths.config_dir.join("store.key"));
        assert_eq!(found.ca, None);
        assert_eq!(found.new_key_file(), paths.config_dir.join("store.key.new"));

        paths.set("store", "/srv/berth/file.db").unwrap();
        let key = dir.path().join("keys/k");
        paths.set("store_key", key.to_str().unwrap()).unwrap();
        paths.set("store_ca", "/etc/ssl/db-ca.pem").unwrap();
        let found = installation(&paths);
        assert_eq!(found.target, Target::Sqlite("/srv/berth/file.db".into()));
        assert_eq!(found.key_file, key);
        assert_eq!(found.ca, Some("/etc/ssl/db-ca.pem".into()));

        // BERTH_STORE wins over the file, as the environment does for every setting.
        let url = "postgres://berth:made-up@192.0.2.10/berth";
        let from_env = Installation::with(&paths, &|key| {
            paths.setting_with(key, (key == "store").then(|| url.to_owned()))
        });
        assert_eq!(from_env.target, Target::Postgres(url.into()));
        assert_eq!(from_env.key_file, key);
    }

    #[test]
    fn a_process_opens_each_installation_once() {
        let (_one, first) = scratch();
        let (_two, second) = scratch();
        let a = shared(&first).unwrap();
        let again = shared(&first).unwrap();
        assert!(Arc::ptr_eq(&a, &again));
        let b = shared(&second).unwrap();
        assert!(!Arc::ptr_eq(&a, &b));
        a.write(kind::META, "", "n", b"1").unwrap();
        assert_eq!(again.read(kind::META, "", "n").unwrap().unwrap(), b"1");
        assert!(b.read(kind::META, "", "n").unwrap().is_none());
    }

    #[test]
    fn the_key_file_seals_the_store() {
        let (_dir, paths) = scratch();
        let found = installation(&paths);
        assert!(!found.open().unwrap().encrypted());
        fs::create_dir_all(&paths.config_dir).unwrap();
        let key = Key::generate().unwrap();
        key.write_new(&found.key_file).unwrap();
        let store = found.open().unwrap();
        assert!(store.encrypted());
        store.write(kind::SECRET, "", "a", b"x").unwrap();
        assert_eq!(
            store.seal_report().unwrap().key_ids,
            [crate::seal::hex(&key.id())]
        );
        drop(store);

        // Without the file the value does not open, and the error names the file.
        fs::rename(&found.key_file, paths.config_dir.join("elsewhere")).unwrap();
        let store = found.open().unwrap();
        let error = store.read(kind::SECRET, "", "a").unwrap_err().to_string();
        assert!(error.contains("encrypted"), "{error}");
        assert!(
            error.contains(&found.key_file.display().to_string()),
            "{error}"
        );
        assert!(!error.contains(&key.to_hex()), "{error}");
    }

    /// A store sealed with `old`, and `new` written beside it as a rekey does first.
    fn half_rekeyed(paths: &Paths) -> (Installation, Key, Key) {
        let found = installation(paths);
        fs::create_dir_all(&paths.config_dir).unwrap();
        let (old, new) = (Key::generate().unwrap(), Key::generate().unwrap());
        old.write_new(&found.key_file).unwrap();
        let store = found.open().unwrap();
        store.write(kind::SECRET, "", "a", b"x").unwrap();
        store.write(kind::SECRET, "", "b", b"y").unwrap();
        new.write_new(&found.new_key_file()).unwrap();
        (found, old, new)
    }

    #[test]
    fn a_rekey_that_rewrote_the_rows_is_finished() {
        let (_dir, paths) = scratch();
        let (found, old, new) = half_rekeyed(&paths);
        let store = Store::connect(&found.target)
            .unwrap()
            .with_keys(Some(Keys::new(copy(&old), vec![])), None);
        store.reseal(Some(&Keys::new(copy(&new), vec![]))).unwrap();
        drop(store);

        let store = found.open().unwrap();
        assert!(!found.new_key_file().exists());
        assert_eq!(Key::read(&found.key_file).unwrap().unwrap().id(), new.id());
        assert_eq!(
            Key::read(&found.old_key_file()).unwrap().unwrap().id(),
            old.id()
        );
        assert_eq!(store.read(kind::SECRET, "", "b").unwrap().unwrap(), b"y");
        store.write(kind::SECRET, "", "c", b"z").unwrap();
        assert_eq!(
            store.seal_report().unwrap().key_ids,
            [crate::seal::hex(&new.id())]
        );
    }

    #[test]
    fn a_rekey_that_did_not_rewrite_the_rows_is_undone() {
        let (_dir, paths) = scratch();
        let (found, old, _new) = half_rekeyed(&paths);
        let store = found.open().unwrap();
        assert!(!found.new_key_file().exists());
        assert_eq!(Key::read(&found.key_file).unwrap().unwrap().id(), old.id());
        assert!(!found.old_key_file().exists());
        assert_eq!(store.read(kind::SECRET, "", "a").unwrap().unwrap(), b"x");
    }

    #[test]
    fn a_rekey_under_way_is_left_alone() {
        let (_dir, paths) = scratch();
        let (found, _old, new) = half_rekeyed(&paths);
        let _lock = found.lock_keys().unwrap().unwrap();
        let store = found.open().unwrap();
        assert!(found.new_key_file().exists());
        // What the change wrote already opens.
        let sealed = Store::connect(&found.target)
            .unwrap()
            .with_keys(Some(Keys::new(copy(&new), vec![])), None);
        sealed.write(kind::SECRET, "", "d", b"w").unwrap();
        assert_eq!(store.read(kind::SECRET, "", "d").unwrap().unwrap(), b"w");
    }
}
