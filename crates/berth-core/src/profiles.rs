//! Where a host's profile is kept. A saved host is a row of berth's store,
//! under the scope of this installation or of the berth server it uses; a
//! profile in a file the user named stays in that file. What berth keeps of a
//! host besides its profile, its generated passwords and the record of the
//! machine berth created for it, is in the store either way: beside a saved
//! host's profile, and for a file under [`FILE_SCOPE`] by the file's path.

use crate::{
    Result,
    paths::Paths,
    server::Server,
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

/// The scope of the saved hosts: the berth server's when one is set, and
/// this installation's own otherwise.
pub fn scope(paths: &Paths) -> String {
    match Server::configured(paths) {
        Some(server) => server.scope(),
        None => OWN.to_owned(),
    }
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
                Ok(store::shared(paths)?.read_json(kind::PROFILE, scope, name)?)
            }
            Place::File(path) => match fs::read(path) {
                Ok(data) => Ok(Some(
                    serde_json::from_slice(&data)
                        .map_err(|error| format!("{}: {error}", path.display()))?,
                )),
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
        .filter_map(|(name, value)| Some((name, serde_json::from_slice(&value).ok()?)))
        .collect())
}

/// Forget a host: its profile, its generated passwords and the record of its
/// machine, in one change. A profile in a file is removed with the file.
pub fn remove(paths: &Paths, place: &Place) -> Result<()> {
    let (scope, name) = place.side();
    let mut removals = vec![
        (kind::CREDENTIALS, scope.as_str(), name.as_str()),
        (kind::INSTANCE, scope.as_str(), name.as_str()),
    ];
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
    Ok(())
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
        // With a berth server set, a name is one of the server's hosts.
        paths.set("server", "http://192.0.2.7:7777").unwrap();
        let served = Place::resolve(&paths, "studio").unwrap();
        assert_eq!(served.server_url(), Some("http://192.0.2.7:7777"));
        assert_eq!(
            served.describe(),
            "studio on the berth server at http://192.0.2.7:7777"
        );
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
                .save(&paths, &json!({"builder": {"ssh_host": address}}))
                .unwrap();
            let loaded = place.load(&paths).unwrap().unwrap();
            assert_eq!(loaded["builder"]["ssh_host"], address);
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
