//! What berth keeps in its store and also uses as files on this machine: the
//! keys of its key ring, which ssh and Ansible are given as files
//! ([`crate::keys::ring`]), and the token of `berth serve`, which recipes and
//! people read from `serve.token`. The store holds the value, so an
//! installation restored from it gets them back, and the files are written
//! from it.
//!
//! The store wins over a file that differs from it. Such a file is moved
//! aside to `<file>.replaced` rather than overwritten, as it may be the only
//! copy of a key some machine still trusts.

use crate::Result;
use crate::paths::{Paths, write_private};
use crate::store::{OWN, Store, kind, wait_lock};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// The name of the row (kind `token`) of the setup key of `berth serve`,
/// which is in the store only.
pub const SETUP_KEY: &str = "setup-key";

/// A value of the store, of one or more rows, and the files it is written to.
pub struct Kept {
    pub kind: &'static str,
    /// The name of its lock in `data_dir/locks/`, held while deciding.
    lock: &'static str,
    /// Each row's name, the file it is written to and the file's mode.
    pub files: Vec<(&'static str, PathBuf, u32)>,
    /// Whether the files' contents can be taken.
    usable: fn(&[Vec<u8>]) -> bool,
}

/// The token clients of `berth serve` show over TCP, in `serve.token`.
pub fn serve_token(paths: &Paths) -> Kept {
    Kept {
        kind: kind::TOKEN,
        lock: "serve-token",
        files: vec![("serve", paths.data_dir.join("serve.token"), 0o600)],
        usable: |values| {
            values
                .first()
                .is_some_and(|value| String::from_utf8_lossy(value).trim().len() >= 32)
        },
    }
}

/// A name in the folder of `file` for a file moved aside: `<file>.replaced`,
/// numbered when that is taken.
fn aside(file: &Path) -> PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push(".replaced");
    let first = PathBuf::from(&name);
    if !first.exists() {
        return first;
    }
    (1..)
        .map(|number| {
            let mut numbered = name.clone();
            numbered.push(format!(".{number}"));
            PathBuf::from(numbered)
        })
        .find(|candidate| !candidate.exists())
        .unwrap_or(first)
}

/// Make `file` hold `value` with `mode`, moving a file that holds something
/// else aside first, as it may be the only copy of a key some machine trusts.
pub fn write_file(file: &Path, value: &[u8], mode: u32) -> Result<()> {
    match fs::read(file) {
        Ok(there) if there == value => {
            fs::set_permissions(file, fs::Permissions::from_mode(mode))?;
            return Ok(());
        }
        Ok(_) => {
            let moved = aside(file);
            fs::rename(file, &moved)?;
            eprintln!(
                "berth: {} differed from berth's store, which has the one in use; the file that was there is {} now",
                file.display(),
                moved.display()
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("{}: {error}", file.display()).into()),
    }
    write_private(file, value)?;
    fs::set_permissions(file, fs::Permissions::from_mode(mode))?;
    Ok(())
}

impl Kept {
    fn lock_file(&self, paths: &Paths) -> PathBuf {
        paths
            .data_dir
            .join("locks")
            .join(format!("{}.lock", self.lock))
    }

    /// The files' contents, when every file is there and they can be taken.
    pub fn from_files(&self) -> Result<Option<Vec<Vec<u8>>>> {
        let mut values = vec![];
        for (_, file, _) in &self.files {
            match fs::read(file) {
                Ok(value) => values.push(value),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(format!("{}: {error}", file.display()).into()),
            }
        }
        Ok(Some(values).filter(|values| (self.usable)(values)))
    }

    /// What the store has, when it has every row.
    pub fn from_store(&self, store: &Store) -> Result<Option<Vec<Vec<u8>>>> {
        let mut values = vec![];
        for (name, _, _) in &self.files {
            match store.read(self.kind, OWN, name)? {
                Some(value) => values.push(value),
                None => return Ok(None),
            }
        }
        Ok(Some(values))
    }

    /// The rows (kind, name, value) for the values.
    pub fn rows(&self, values: &[Vec<u8>]) -> Vec<(&'static str, &'static str, Vec<u8>)> {
        self.files
            .iter()
            .zip(values)
            .map(|((name, _, _), value)| (self.kind, *name, value.clone()))
            .collect()
    }

    fn put(&self, store: &Store, values: &[Vec<u8>]) -> Result<()> {
        let writes: Vec<_> = self
            .rows(values)
            .into_iter()
            .map(|(kind, name, value)| (kind, OWN, name, value))
            .collect();
        Ok(store.change(&writes, &[])?)
    }

    /// Make each file hold its value, moving a file that holds something
    /// else aside first.
    pub fn write_files(&self, values: &[Vec<u8>]) -> Result<()> {
        for ((_, file, mode), value) in self.files.iter().zip(values) {
            write_file(file, value, *mode)?;
        }
        Ok(())
    }

    /// The values, settled between the store and the files: what the store
    /// has, written to the files; else the files, put in the store; else what
    /// `make` makes, put in both. Under a lock, so that two berths at first
    /// use do not make two.
    pub fn settle(
        &self,
        paths: &Paths,
        store: &Store,
        make: &dyn Fn() -> Result<Vec<Vec<u8>>>,
    ) -> Result<Vec<Vec<u8>>> {
        let _lock = wait_lock(&self.lock_file(paths), "making the token of berth serve")?;
        // Read after the lock is held: another berth may have just made it.
        if let Some(values) = self.from_store(store)? {
            self.write_files(&values)?;
            return Ok(values);
        }
        if let Some(values) = self.from_files()? {
            self.put(store, &values)?;
            return Ok(values);
        }
        let values = make()?;
        self.put(store, &values)?;
        self.write_files(&values)?;
        Ok(values)
    }
}

/// A new token: 32 random bytes in hex, as `serve.token` holds it.
pub fn generate_token() -> Result<Vec<Vec<u8>>> {
    let mut bytes = [0u8; 32];
    std::io::Read::read_exact(&mut fs::File::open("/dev/urandom")?, &mut bytes)?;
    let token: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(vec![format!("{token}\n").into_bytes()])
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

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn the_files_go_into_an_empty_store() {
        let (_dir, paths) = scratch();
        let store = store(&paths);
        let token = serve_token(&paths);
        fs::create_dir_all(&paths.data_dir).unwrap();
        fs::write(&token.files[0].1, format!("{}\n", "a".repeat(64))).unwrap();
        let made = || -> Result<Vec<Vec<u8>>> { panic!("nothing is made") };
        let values = token.settle(&paths, &store, &made).unwrap();
        assert_eq!(values[0], format!("{}\n", "a".repeat(64)).as_bytes());
        assert_eq!(
            store.read(kind::TOKEN, OWN, "serve").unwrap().unwrap(),
            values[0]
        );
    }

    #[test]
    fn a_token_too_short_is_made_again() {
        let (_dir, paths) = scratch();
        let store = store(&paths);
        let token = serve_token(&paths);
        fs::create_dir_all(&paths.data_dir).unwrap();
        fs::write(&token.files[0].1, "short\n").unwrap();
        let values = token.settle(&paths, &store, &generate_token).unwrap();
        assert_eq!(values[0].len(), 65);
        // The short one was not the store's, and is moved aside all the same.
        assert_eq!(fs::read(&token.files[0].1).unwrap(), values[0]);
        assert_eq!(
            fs::read_to_string(paths.data_dir.join("serve.token.replaced")).unwrap(),
            "short\n"
        );
        assert_eq!(mode(&token.files[0].1), 0o600);
    }

    #[test]
    fn the_store_wins_and_a_differing_file_is_moved_aside() {
        let (_dir, paths) = scratch();
        fs::create_dir_all(&paths.data_dir).unwrap();
        let private = paths.data_dir.join("keys/berth");
        fs::create_dir_all(private.parent().unwrap()).unwrap();
        fs::write(&private, "other").unwrap();
        fs::write(paths.data_dir.join("keys/berth.replaced"), "older").unwrap();
        write_file(&private, b"private", 0o600).unwrap();
        assert_eq!(fs::read(&private).unwrap(), b"private");
        assert_eq!(mode(&private), 0o600);
        assert_eq!(
            fs::read_to_string(paths.data_dir.join("keys/berth.replaced.1")).unwrap(),
            "other"
        );
        // Settled, nothing moves again.
        write_file(&private, b"private", 0o600).unwrap();
        assert!(!paths.data_dir.join("keys/berth.replaced.2").exists());
    }

    #[test]
    fn two_berths_at_first_use_make_one_token() {
        let (_dir, paths) = scratch();
        fs::create_dir_all(&paths.data_dir).unwrap();
        let paths = std::sync::Arc::new(paths);
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let paths = paths.clone();
                std::thread::spawn(move || {
                    let store = store(&paths);
                    serve_token(&paths)
                        .settle(&paths, &store, &generate_token)
                        .unwrap()
                })
            })
            .collect();
        let made: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert!(made.windows(2).all(|pair| pair[0] == pair[1]));
    }
}
