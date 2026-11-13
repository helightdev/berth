//! The key ring: SSH keys by name, in rows of kind `key` of the store's own
//! scope, sealed with the store. A row holds both halves of a key as JSON;
//! a key being replaced has its successor beside it as `NAME.pending`.
//!
//! The key `berth` always exists and is made on first use. A private half is
//! written to `data_dir/keys/NAME` only when ssh or Ansible need it as a
//! file, and the store wins over a file that differs from it.

use crate::Result;
use crate::paths::Paths;
use crate::store::{OWN, Store, kind, wait_lock};
use berth_api::profile::{BERTH_KEY, valid_key_name};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// What a row of the key ring holds.
#[derive(Clone, Serialize, Deserialize)]
pub struct Halves {
    pub private: String,
    pub public: String,
}

impl std::fmt::Debug for Halves {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The private half never goes into a log or an error.
        f.debug_struct("Halves")
            .field("public", &self.public)
            .finish_non_exhaustive()
    }
}

/// A key as it is shown: its public half only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Public {
    pub name: String,
    pub public: String,
    pub fingerprint: String,
    /// The key that replaces it while a rotation runs.
    pub pending: Option<Box<Public>>,
}

/// The suffix of the row of a key that replaces another.
pub const PENDING: &str = ".pending";

fn row(name: &str, pending: bool) -> String {
    match pending {
        true => format!("{name}{PENDING}"),
        false => name.to_owned(),
    }
}

/// The SHA256 fingerprint of a public key line, as `ssh-keygen -l` writes it.
pub fn fingerprint(public: &str) -> String {
    use sha2::{Digest, Sha256};
    let Some(blob) = public.split_whitespace().nth(1).and_then(berth_api::unb64) else {
        return String::new();
    };
    let digest = berth_api::b64(&Sha256::digest(&blob));
    format!("SHA256:{}", digest.trim_end_matches('='))
}

/// The type and base64 of a public key line, without its comment.
pub fn blob(public: &str) -> String {
    public
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ")
}

/// The halves of a key, or of its pending successor.
pub fn read(store: &Store, name: &str, pending: bool) -> Result<Option<Halves>> {
    let Some(value) = store.read(kind::KEY, OWN, &row(name, pending))? else {
        return Ok(None);
    };
    serde_json::from_slice(&value)
        .map(Some)
        .map_err(|_| format!("The key {name} in berth's store is not readable").into())
}

fn write(store: &Store, name: &str, pending: bool, halves: &Halves) -> Result<()> {
    Ok(store.write(
        kind::KEY,
        OWN,
        &row(name, pending),
        &serde_json::to_vec(halves)?,
    )?)
}

/// Put a key that is not a real one under `name`, so that a test makes no
/// key with ssh-keygen, which needs an account in the password database.
#[cfg(test)]
pub fn give_test_key(store: &Store, name: &str) {
    let halves = Halves {
        private: "not a real private key\n".into(),
        public: format!(
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAABAgMEBQYHCAkKCwwNDg8QERITFBUWFxgZGhscHR4f {name}@studio"
        ),
    };
    write(store, name, false, &halves).unwrap();
}

/// The names of the keys, without the pending ones. Nothing is unsealed.
pub fn names(store: &Store) -> Result<Vec<String>> {
    Ok(store
        .names(kind::KEY, OWN)?
        .into_iter()
        .filter(|name| !name.ends_with(PENDING) && valid_key_name(name))
        .collect())
}

fn public_of(name: &str, halves: &Halves) -> Public {
    Public {
        name: name.to_owned(),
        public: halves.public.trim().to_owned(),
        fingerprint: fingerprint(&halves.public),
        pending: None,
    }
}

/// Every key with its public half, `berth` first, made if it is not there.
pub fn list(paths: &Paths, store: &Store) -> Result<Vec<Public>> {
    ensure(paths, store)?;
    let mut keys = vec![];
    let mut names = names(store)?;
    names.sort_by_key(|name| (name != BERTH_KEY, name.clone()));
    for name in names {
        let Some(halves) = read(store, &name, false)? else {
            continue;
        };
        let mut key = public_of(&name, &halves);
        key.pending = read(store, &name, true)?.map(|next| Box::new(public_of(&name, &next)));
        keys.push(key);
    }
    Ok(keys)
}

/// One key's public half, and its pending successor's.
pub fn public(paths: &Paths, store: &Store, name: &str) -> Result<Public> {
    if name == BERTH_KEY {
        ensure(paths, store)?;
    }
    let halves = read(store, name, false)?.ok_or_else(|| no_key(name))?;
    let mut key = public_of(name, &halves);
    key.pending = read(store, name, true)?.map(|next| Box::new(public_of(name, &next)));
    Ok(key)
}

fn no_key(name: &str) -> Box<dyn std::error::Error> {
    format!("No key {name} in berth's key ring. See: berth key").into()
}

/// The key `berth`, made the first time it is asked for. Under a lock, so
/// that two berths at first use do not make two.
pub fn ensure(paths: &Paths, store: &Store) -> Result<Halves> {
    if let Some(halves) = read(store, BERTH_KEY, false)? {
        return Ok(halves);
    }
    let _lock = wait_lock(&lock_file(paths), "making berth's own key")?;
    // Read after the lock is held: another berth may have just made it.
    if let Some(halves) = read(store, BERTH_KEY, false)? {
        return Ok(halves);
    }
    let halves = generate(paths, BERTH_KEY)?;
    write(store, BERTH_KEY, false, &halves)?;
    Ok(halves)
}

fn lock_file(paths: &Paths) -> PathBuf {
    paths.data_dir.join("locks").join("keys.lock")
}

/// A new ed25519 key.
pub fn generate(paths: &Paths, name: &str) -> Result<Halves> {
    let made = crate::kept::generate_key_named(paths, name)?;
    Ok(Halves {
        private: String::from_utf8(made[0].clone()).map_err(|_| "ssh-keygen made no text")?,
        public: String::from_utf8_lossy(&made[1]).trim().to_owned(),
    })
}

/// A private key as ssh can use it, with its public half from `ssh-keygen
/// -y`, which reads every type ssh takes. One with a passphrase is refused:
/// berth logs in without anyone to type it.
pub fn validate(paths: &Paths, private: &[u8]) -> Result<Halves> {
    let text = std::str::from_utf8(private).map_err(|_| "That is not an OpenSSH private key")?;
    if !text.contains("PRIVATE KEY-----") {
        return Err("That is not a private key: it has no PRIVATE KEY line".into());
    }
    let mut text = text.trim().to_owned();
    text.push('\n');
    let dir = scratch_dir(paths)?;
    let file = dir.path().join("key");
    crate::paths::write_private(&file, text.as_bytes())?;
    let said = Command::new("ssh-keygen")
        .args(["-y", "-P", ""])
        .arg("-f")
        .arg(&file)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("ssh-keygen: {error}"))?;
    if !said.status.success() {
        let why = String::from_utf8_lossy(&said.stderr).to_lowercase();
        if why.contains("passphrase") {
            return Err("That key has a passphrase. berth logs in where nobody can type one: remove it from a copy with ssh-keygen -p -N '' -f COPY and add the copy".into());
        }
        return Err("ssh-keygen cannot read that private key".into());
    }
    let public = String::from_utf8_lossy(&said.stdout).trim().to_owned();
    if public.split_whitespace().count() < 2 {
        return Err("ssh-keygen gave no public half for that key".into());
    }
    Ok(Halves {
        private: text,
        public,
    })
}

/// A private directory for files that live only as long as one step.
pub(crate) fn scratch_dir(paths: &Paths) -> Result<tempfile::TempDir> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&paths.data_dir)?;
    Ok(tempfile::Builder::new()
        .prefix(".berth-")
        .tempdir_in(&paths.data_dir)?)
}

/// Add a key: a new one when `private` is `None`, else that private key.
pub fn add(paths: &Paths, store: &Store, name: &str, private: Option<&[u8]>) -> Result<Public> {
    if !valid_key_name(name) {
        return Err(format!(
            "A key's name is letters, digits, - and _, such as deploy, not {name}"
        )
        .into());
    }
    if store
        .names(kind::KEY, OWN)?
        .iter()
        .any(|known| known == name)
    {
        return Err(format!(
            "There is a key {name} already. Remove it first, or rotate it: berth key rotate {name}"
        )
        .into());
    }
    let halves = match private {
        Some(private) => validate(paths, private)?,
        None => generate(paths, name)?,
    };
    write(store, name, false, &halves)?;
    Ok(public_of(name, &halves))
}

/// Remove a key. Refused for `berth`, while a host logs in with it, and
/// while it is being rotated.
pub fn remove(paths: &Paths, store: &Store, name: &str) -> Result<()> {
    if name == BERTH_KEY {
        return Err("The key berth is the installation's own and stays. Rotate it to replace it: berth key rotate".into());
    }
    if read(store, name, false)?.is_none() {
        return Err(no_key(name));
    }
    let users = super::users(store, name)?;
    if !users.is_empty() {
        return Err(format!(
            "{} log in with the key {name}. Give them another way to log in first",
            users.join(", ")
        )
        .into());
    }
    if read(store, name, true)?.is_some() {
        return Err(format!(
            "The key {name} is being rotated. Finish that first: berth key rotate {name}"
        )
        .into());
    }
    store.change(&[], &[(kind::KEY, OWN, name)])?;
    for file in [file_path(paths, name, false), file_path(paths, name, true)] {
        let _ = fs::remove_file(&file);
        let _ = fs::remove_file(public_path(&file));
    }
    Ok(())
}

/// Where a key's private half is written for ssh.
pub fn file_path(paths: &Paths, name: &str, pending: bool) -> PathBuf {
    paths.data_dir.join("keys").join(row(name, pending))
}

/// The private half of a key as a file, written from the store. `None` for
/// a pending key that does not exist.
pub fn file(paths: &Paths, store: &Store, name: &str, pending: bool) -> Result<Option<PathBuf>> {
    let halves = match (name, pending) {
        (BERTH_KEY, false) => ensure(paths, store)?,
        _ => match read(store, name, pending)? {
            Some(halves) => halves,
            None if pending => return Ok(None),
            None => return Err(no_key(name)),
        },
    };
    let path = file_path(paths, name, pending);
    write_halves(&path, &halves)?;
    Ok(Some(path))
}

/// The file of a key's public half, beside the private one.
pub fn public_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".pub");
    PathBuf::from(name)
}

fn write_halves(path: &Path, halves: &Halves) -> Result<()> {
    crate::kept::write_file(path, halves.private.as_bytes(), 0o600)?;
    let public = format!("{}\n", halves.public.trim());
    crate::kept::write_file(&public_path(path), public.as_bytes(), 0o644)
}

/// Start replacing a key: a new one beside it, unless one is pending already.
pub fn begin_rotation(paths: &Paths, store: &Store, name: &str) -> Result<Halves> {
    if name == BERTH_KEY {
        ensure(paths, store)?;
    }
    if read(store, name, false)?.is_none() {
        return Err(no_key(name));
    }
    if let Some(next) = read(store, name, true)? {
        return Ok(next);
    }
    let _lock = wait_lock(&lock_file(paths), "starting a key's rotation")?;
    if let Some(next) = read(store, name, true)? {
        return Ok(next);
    }
    let next = generate(paths, name)?;
    write(store, name, true, &next)?;
    Ok(next)
}

/// End a rotation: the pending key becomes the key, in one change, and the
/// old private half goes from the disk too.
pub fn finish_rotation(paths: &Paths, store: &Store, name: &str) -> Result<()> {
    let Some(next) = read(store, name, true)? else {
        return Ok(());
    };
    let pending = row(name, true);
    store.change(
        &[(kind::KEY, OWN, name, serde_json::to_vec(&next)?)],
        &[(kind::KEY, OWN, pending.as_str())],
    )?;
    let old = file_path(paths, name, false);
    // Replaced on purpose: the old key is retired, not something to keep aside.
    let _ = fs::remove_file(&old);
    let _ = fs::remove_file(public_path(&old));
    write_halves(&old, &next)?;
    let gone = file_path(paths, name, true);
    let _ = fs::remove_file(&gone);
    let _ = fs::remove_file(public_path(&gone));
    Ok(())
}
