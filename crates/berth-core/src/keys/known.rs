//! What berth knows of each host's SSH: the rows of kind `access`, beside
//! the profile, with the host's public host keys and the fingerprints of the
//! ring's keys the host trusts. ssh is given a known-hosts file of its own
//! for each host, written from that row, and looks the host up there under
//! an alias of berth's, so a restored installation knows its hosts and an
//! address that moves to another machine is not mistaken for it.

use crate::Result;
use crate::paths::Paths;
use crate::profiles::Place;
use crate::store::{Store, kind};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// A row of kind `access`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// The host's public host keys, `TYPE BASE64` each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub host_keys: Vec<String>,
    /// The fingerprints of the ring's keys the host takes, as far as berth
    /// put them there or saw them work.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trusts: Vec<String>,
    /// The last login the host refused, which nothing automatic tries again
    /// for a while ([`super::refusal`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<super::refusal::Refusal>,
}

impl Record {
    pub fn load(store: &Store, place: &Place) -> Result<Record> {
        let (scope, name) = place.side();
        Ok(store
            .read_json(kind::ACCESS, &scope, &name)?
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default())
    }

    pub fn save(&self, store: &Store, place: &Place) -> Result<()> {
        let (scope, name) = place.side();
        if self == &Record::default() {
            store.forget(kind::ACCESS, &scope, &name)?;
            return Ok(());
        }
        Ok(store.write_json(kind::ACCESS, &scope, &name, &serde_json::to_value(self)?)?)
    }

    /// Note that the host takes a key, and no longer one it replaced.
    pub fn trust(&mut self, fingerprint: &str, instead_of: Option<&str>) {
        if let Some(old) = instead_of {
            self.trusts.retain(|known| known != old);
        }
        if !fingerprint.is_empty() && !self.trusts.iter().any(|known| known == fingerprint) {
            self.trusts.push(fingerprint.to_owned());
        }
    }
}

/// The name ssh files the host's key under: berth's own for the host, the
/// same whatever its address or port.
pub fn alias(place: &Place) -> String {
    use sha2::{Digest, Sha256};
    let (scope, name) = place.side();
    let short = |text: &str| -> String {
        Sha256::digest(text.as_bytes())
            .iter()
            .take(6)
            .map(|byte| format!("{byte:02x}"))
            .collect()
    };
    match place {
        Place::Stored { .. } if scope.is_empty() => format!("berth.{name}"),
        Place::Stored { .. } => format!("berth.{}.{name}", short(&scope)),
        Place::File(_) => format!("berth.file.{}", short(&name)),
    }
}

/// The known-hosts file of one host.
pub fn file(paths: &Paths, place: &Place) -> PathBuf {
    paths.data_dir.join("known_hosts").join(alias(place))
}

/// The host keys a known-hosts file has, `TYPE BASE64` each.
fn keys_in(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let mut words = line.split_whitespace().skip(1);
            Some(format!("{} {}", words.next()?, words.next()?))
        })
        .collect()
}

/// Make the host's known-hosts file what its row says, first taking in what
/// ssh recorded there on a first contact. Returns the file.
pub fn prepare(paths: &Paths, store: &Store, place: &Place) -> Result<PathBuf> {
    let path = file(paths, place);
    let mut record = Record::load(store, place)?;
    if let Ok(text) = fs::read_to_string(&path) {
        let found = keys_in(&text);
        if record.host_keys.is_empty() && !found.is_empty() {
            record.host_keys = found;
            record.save(store, place)?;
        }
    }
    write(paths, place, &record)?;
    Ok(path)
}

fn write(paths: &Paths, place: &Place, record: &Record) -> Result<()> {
    let path = file(paths, place);
    if record.host_keys.is_empty() {
        match fs::remove_file(&path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(format!("{}: {error}", path.display()).into());
            }
            _ => {}
        }
        // ssh writes the first key it sees here, in a folder that has to exist.
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path.parent().ok_or("no folder")?)?;
        return Ok(());
    }
    let name = alias(place);
    let text: String = record
        .host_keys
        .iter()
        .map(|key| format!("{name} {key}\n"))
        .collect();
    crate::paths::write_private(&path, text.as_bytes())
}

/// Take in what ssh recorded at a first contact. Returns the host keys.
pub fn absorb(paths: &Paths, store: &Store, place: &Place) -> Result<Vec<String>> {
    prepare(paths, store, place)?;
    Ok(Record::load(store, place)?.host_keys)
}

/// Forget the host's keys, so the next contact records the ones it has
/// then: for a machine berth made again, or a change the user accepted.
pub fn forget_host_key(paths: &Paths, store: &Store, place: &Place) -> Result<()> {
    let mut record = Record::load(store, place)?;
    record.host_keys.clear();
    record.save(store, place)?;
    write(paths, place, &record)
}

/// Record host keys from another known-hosts file, such as `~/.ssh/known_hosts`,
/// for a host berth has none of yet. `names` are what that file may list
/// the host as. Returns whether any were found.
pub fn seed_from(
    store: &Store,
    place: &Place,
    known_hosts: &std::path::Path,
    names: &[String],
) -> Result<bool> {
    let mut record = Record::load(store, place)?;
    if !record.host_keys.is_empty() || !known_hosts.is_file() {
        return Ok(false);
    }
    for name in names.iter().filter(|name| !name.is_empty()) {
        let found = std::process::Command::new("ssh-keygen")
            .args(["-F", name, "-f"])
            .arg(known_hosts)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output();
        let Ok(found) = found else { continue };
        let keys = keys_in(&String::from_utf8_lossy(&found.stdout));
        if !keys.is_empty() {
            record.host_keys = keys;
            record.save(store, place)?;
            return Ok(true);
        }
    }
    Ok(false)
}

/// The sentence for an ssh that refused a host because its key changed.
pub fn changed_key(name: &str, said: &str) -> Option<String> {
    let changed = said.contains("REMOTE HOST IDENTIFICATION HAS CHANGED")
        || said.contains("Host key verification failed");
    changed.then(|| {
        format!(
            "The SSH host key of {name} is not the one berth recorded: the machine was set up again or replaced, or something is in between. If you know which, accept the new key with: berth key host {name} --accept"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_are_one_word_per_place() {
        let own = Place::own("studio").unwrap();
        let served = Place::stored("served:http://192.0.2.7:7777", "studio").unwrap();
        let file = Place::File("/profiles/studio.json".into());
        let names = [alias(&own), alias(&served), alias(&file)];
        assert_eq!(names[0], "berth.studio");
        assert!(names.iter().all(|name| !name.contains(char::is_whitespace)));
        assert!(names[1] != names[0] && names[2] != names[0]);
    }

    #[test]
    fn a_first_contact_is_taken_in_and_written_back_under_the_alias() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: dir.path().join("config"),
            data_dir: dir.path().join("data"),
        };
        let store = Store::open(&dir.path().join("berth.db")).unwrap();
        let place = Place::own("studio").unwrap();
        let path = prepare(&paths, &store, &place).unwrap();
        assert!(!path.exists() && path.parent().unwrap().is_dir());
        // What ssh writes there at first contact.
        fs::write(&path, "berth.studio ssh-ed25519 AAAAmadeup\n").unwrap();
        assert_eq!(
            absorb(&paths, &store, &place).unwrap(),
            ["ssh-ed25519 AAAAmadeup"]
        );
        // The row wins over anything written there later.
        fs::write(&path, "berth.studio ssh-ed25519 AAAAother\n").unwrap();
        prepare(&paths, &store, &place).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "berth.studio ssh-ed25519 AAAAmadeup\n"
        );
        forget_host_key(&paths, &store, &place).unwrap();
        assert!(!path.exists());
        assert!(Record::load(&store, &place).unwrap().host_keys.is_empty());
        assert!(
            changed_key("studio", "Host key verification failed.")
                .unwrap()
                .contains("berth key host studio --accept")
        );
    }
}
