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
    /// was interrupted.
    pub fn open(&self) -> crate::Result<Store> {
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
    let installation = Installation::of(paths);
    let store = Arc::new(installation.open()?);
    crate::import::run(paths, &installation, &store)?;
    opened.insert(key, store.clone());
    Ok(store)
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
