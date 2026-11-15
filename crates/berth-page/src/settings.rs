//! The server the screens use and its token, kept between runs as berth's
//! own `server` and `server_token` settings: in `config.json` of berth's
//! folder, a file only its user can read, in a folder only its user can open.
//! The other settings of that file are left as they are.

use serde_json::{Map, Value};
use std::io::Write;
use std::path::{Path, PathBuf};

/// The settings that say which server berth uses, and with what token.
pub const SERVER: &str = "server";
pub const TOKEN: &str = "server_token";

/// Where the settings of a program are for the environment `var` gives:
/// under `%LOCALAPPDATA%` on Windows, `$XDG_CONFIG_HOME` where it is set,
/// and else `~/Library/Application Support` on macOS and `~/.config`
/// elsewhere.
fn base(var: &impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let var = |name: &str| var(name).filter(|value| !value.is_empty());
    if cfg!(windows) {
        // Local rather than roaming, so that the token is not copied to
        // other machines with the profile.
        return Some(PathBuf::from(var("LOCALAPPDATA")?));
    }
    // Only an absolute one, as berth's directories take it.
    if let Some(dir) = var("XDG_CONFIG_HOME").filter(|dir| Path::new(dir).is_absolute()) {
        return Some(PathBuf::from(dir));
    }
    let home = PathBuf::from(var("HOME")?);
    match cfg!(target_os = "macos") {
        true => Some(home.join("Library/Application Support")),
        false => Some(home.join(".config")),
    }
}

/// berth's folder of settings, which on Linux holds the rest of its settings too.
pub fn folder(var: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    Some(base(&var)?.join("berth"))
}

/// Where berth-remote kept its server and token, before berth took it in.
pub fn old_folder(var: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    Some(base(&var)?.join("berth-remote"))
}

pub struct Settings {
    dir: Option<PathBuf>,
}

impl Settings {
    /// The settings in `dir`; with `None`, nothing is read or written.
    pub fn new(dir: Option<PathBuf>) -> Settings {
        Settings { dir }
    }

    fn file(&self) -> Option<PathBuf> {
        Some(self.dir.as_ref()?.join("config.json"))
    }

    /// Everything the file holds; nothing when it is missing or unreadable.
    fn read(&self) -> Map<String, Value> {
        self.file()
            .and_then(|file| std::fs::read(file).ok())
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|value| match value {
                Value::Object(map) => Some(map),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// A setting's value, when it is set.
    pub fn get(&self, key: &str) -> Option<String> {
        self.read()
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .filter(|value| !value.is_empty())
    }

    /// Set the settings `changes` names, removing those whose value is
    /// empty. A file that is there but is not settings is not written over.
    pub fn set(&self, changes: &[(&str, &str)]) -> std::io::Result<()> {
        let (Some(dir), Some(file)) = (&self.dir, self.file()) else {
            return Ok(());
        };
        let mut kept = match std::fs::read(&file) {
            Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                Ok(Value::Object(map)) => map,
                _ => {
                    return Err(std::io::Error::other(format!(
                        "{} holds no settings, and is left as it is",
                        file.display()
                    )));
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Map::new(),
            Err(error) => return Err(error),
        };
        let before = kept.clone();
        for (key, value) in changes {
            match value.is_empty() {
                true => kept.remove(*key),
                false => kept.insert((*key).to_owned(), Value::String((*value).to_owned())),
            };
        }
        if kept == before {
            return Ok(());
        }
        private_dir(dir)?;
        let text = serde_json::to_string_pretty(&Value::Object(kept))? + "\n";
        private_write(&file, text.as_bytes())
    }

    /// Take in the server and token berth-remote kept in `old`, once. When
    /// berth has no server, or the same one without a token, they become
    /// berth's and the old file goes. When berth uses another server, or the
    /// same with another token, the file stays, and that is said once.
    pub fn take_in(&self, old: Option<&Path>) -> std::io::Result<TakenIn> {
        let Some(old) = old else {
            return Ok(TakenIn::Nothing);
        };
        let file = old.join("settings.json");
        let Ok(bytes) = std::fs::read(&file) else {
            return Ok(TakenIn::Nothing);
        };
        let kept: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        let text = |key: &str| kept[key].as_str().unwrap_or("").trim().to_owned();
        let (server, token) = (text("server"), text("token"));
        // A file that names no server holds nothing to take in, and is left as it is.
        if server.is_empty() {
            return Ok(TakenIn::Nothing);
        }
        let same = |one: &str, other: &str| one.trim_end_matches('/') == other.trim_end_matches('/');
        let taken = match (self.get(SERVER), self.get(TOKEN)) {
            (None, _) => {
                self.set(&[(SERVER, &server), (TOKEN, &token)])?;
                TakenIn::Taken(server)
            }
            (Some(ours), None) if same(&ours, &server) && !token.is_empty() => {
                self.set(&[(TOKEN, &token)])?;
                TakenIn::Taken(server)
            }
            // Its contents are berth's already.
            (Some(ours), ours_token)
                if same(&ours, &server) && (token.is_empty() || ours_token.as_deref() == Some(&token)) =>
            {
                TakenIn::Same
            }
            (Some(ours), _) => {
                if self.get(NOTED).as_deref() == Some(server.as_str()) {
                    return Ok(TakenIn::Nothing);
                }
                self.set(&[(NOTED, &server)])?;
                return Ok(TakenIn::Left {
                    theirs: server,
                    ours,
                    file,
                });
            }
        };
        for gone in [file.clone(), file.with_extension("json.partial")] {
            match std::fs::remove_file(&gone) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
                _ => {}
            }
        }
        let _ = std::fs::remove_dir(old);
        Ok(taken)
    }
}

/// The setting that remembers which server of berth-remote's settings was
/// said to be left, so it is said once.
const NOTED: &str = "berth_remote_noted";

/// What became of berth-remote's settings.
#[derive(Debug, PartialEq, Eq)]
pub enum TakenIn {
    /// There were none, or they were said to be left before.
    Nothing,
    /// berth uses their server and token now; the old file is gone.
    Taken(String),
    /// berth had them already; the old file is gone.
    Same,
    /// berth uses another server, or the same with another token: the file
    /// stays where it is.
    Left {
        theirs: String,
        ours: String,
        file: PathBuf,
    },
}

/// Make the folder of the settings, which only its owner can open.
fn private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)?;
    // A folder that was there already keeps its mode otherwise.
    #[cfg(unix)]
    std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    Ok(())
}

/// Write a file only its owner can read, since it holds the token. On
/// Windows the file takes the access list of the user's profile, which lets
/// in the user, SYSTEM and the administrators.
fn private_write(file: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let partial = file.with_extension("json.partial");
    // A file left over would keep its own mode.
    match std::fs::remove_file(&partial) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut out = options.open(&partial)?;
    out.write_all(bytes)?;
    out.sync_all()?;
    drop(out);
    std::fs::rename(partial, file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_server_beside_the_other_settings() {
        let scratch = tempfile::tempdir().unwrap();
        let dir = scratch.path().join("config/berth");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.json"), r#"{"store": "/srv/berth.db"}"#).unwrap();
        let settings = Settings::new(Some(dir.clone()));
        assert_eq!(settings.get(SERVER), None);
        settings
            .set(&[(SERVER, "https://studio.example"), (TOKEN, "secret")])
            .unwrap();
        assert_eq!(
            settings.get(SERVER).as_deref(),
            Some("https://studio.example")
        );
        assert_eq!(settings.get(TOKEN).as_deref(), Some("secret"));
        settings.set(&[(TOKEN, "")]).unwrap();
        assert_eq!(settings.get(TOKEN), None);
        let all: Value =
            serde_json::from_slice(&std::fs::read(dir.join("config.json")).unwrap()).unwrap();
        assert_eq!(all["store"], "/srv/berth.db");
        assert!(!dir.join("config.json.partial").exists());
    }

    #[test]
    fn a_file_that_is_not_settings_is_left_alone() {
        let scratch = tempfile::tempdir().unwrap();
        let settings = Settings::new(Some(scratch.path().to_owned()));
        std::fs::write(scratch.path().join("config.json"), "{not json").unwrap();
        assert_eq!(settings.get(SERVER), None);
        assert!(settings.set(&[(SERVER, "https://dock.example")]).is_err());
        let left = std::fs::read_to_string(scratch.path().join("config.json")).unwrap();
        assert_eq!(left, "{not json");
    }

    fn remote(scratch: &Path, text: &str) -> PathBuf {
        let old = scratch.join("berth-remote");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("settings.json"), text).unwrap();
        old
    }

    #[test]
    fn berth_remotes_server_is_taken_in_once() {
        let scratch = tempfile::tempdir().unwrap();
        let old = remote(
            scratch.path(),
            r#"{"server": "https://studio.example", "token": "s3cret"}"#,
        );
        let settings = Settings::new(Some(scratch.path().join("berth")));
        assert_eq!(
            settings.take_in(Some(&old)).unwrap(),
            TakenIn::Taken("https://studio.example".into())
        );
        assert_eq!(settings.get(TOKEN).as_deref(), Some("s3cret"));
        assert!(!old.exists());
        assert_eq!(settings.take_in(Some(&old)).unwrap(), TakenIn::Nothing);
    }

    #[test]
    fn berth_remotes_token_for_berths_own_server_is_taken_in() {
        let scratch = tempfile::tempdir().unwrap();
        let settings = Settings::new(Some(scratch.path().join("berth")));
        settings.set(&[(SERVER, "https://studio.example")]).unwrap();
        let old = remote(
            scratch.path(),
            r#"{"server": "https://studio.example/", "token": "s3cret"}"#,
        );
        assert_eq!(
            settings.take_in(Some(&old)).unwrap(),
            TakenIn::Taken("https://studio.example/".into())
        );
        assert_eq!(settings.get(TOKEN).as_deref(), Some("s3cret"));
        assert!(!old.exists());

        // The same server and token are berth's already: the file goes.
        let old = remote(
            scratch.path(),
            r#"{"server": "https://studio.example", "token": "s3cret"}"#,
        );
        assert_eq!(settings.take_in(Some(&old)).unwrap(), TakenIn::Same);
        assert!(!old.exists());
    }

    #[test]
    fn berth_remotes_other_server_is_left_and_said_once() {
        let scratch = tempfile::tempdir().unwrap();
        let settings = Settings::new(Some(scratch.path().join("berth")));
        settings
            .set(&[(SERVER, "https://studio.example"), (TOKEN, "mine")])
            .unwrap();
        for other in [
            r#"{"server": "https://dock.example", "token": "other"}"#,
            r#"{"server": "https://studio.example", "token": "other"}"#,
        ] {
            let old = remote(scratch.path(), other);
            let file = old.join("settings.json");
            let taken = settings.take_in(Some(&old)).unwrap();
            assert!(matches!(&taken, TakenIn::Left { file: left, ours, .. }
                if *left == file && ours == "https://studio.example"), "{taken:?}");
            // The file stays, and is not spoken of again.
            assert!(file.exists());
            assert_eq!(settings.take_in(Some(&old)).unwrap(), TakenIn::Nothing);
            assert_eq!(settings.get(TOKEN).as_deref(), Some("mine"));
            assert_eq!(
                settings.get(SERVER).as_deref(),
                Some("https://studio.example")
            );
            std::fs::remove_dir_all(&old).unwrap();
        }
    }

    #[test]
    fn a_file_of_berth_remote_with_no_server_is_left_as_it_is() {
        let scratch = tempfile::tempdir().unwrap();
        let settings = Settings::new(Some(scratch.path().join("berth")));
        for text in ["{not json", r#"{"token": "s3cret"}"#] {
            let old = remote(scratch.path(), text);
            assert_eq!(settings.take_in(Some(&old)).unwrap(), TakenIn::Nothing);
            assert!(old.join("settings.json").exists());
        }
        assert_eq!(settings.get(TOKEN), None);
    }

    #[cfg(unix)]
    #[test]
    fn only_the_owner_can_read_them() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = tempfile::tempdir().unwrap();
        let dir = scratch.path().join("berth");
        // A folder made earlier with a looser mode is made private too.
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let settings = Settings::new(Some(dir.clone()));
        settings.set(&[(TOKEN, "secret")]).unwrap();
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("config.json")), 0o600);
    }

    #[test]
    fn the_folder_follows_the_system() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value.to_string())
            }
        };
        if cfg!(windows) {
            assert_eq!(
                folder(env(&[("LOCALAPPDATA", r"C:\Users\ada\AppData\Local")])),
                Some(PathBuf::from(r"C:\Users\ada\AppData\Local").join("berth"))
            );
            assert_eq!(folder(env(&[])), None);
            return;
        }
        assert_eq!(
            folder(env(&[
                ("XDG_CONFIG_HOME", "/scratch/c"),
                ("HOME", "/home/ada")
            ])),
            Some(PathBuf::from("/scratch/c/berth"))
        );
        assert_eq!(
            old_folder(env(&[
                ("XDG_CONFIG_HOME", "/scratch/c"),
                ("HOME", "/home/ada")
            ])),
            Some(PathBuf::from("/scratch/c/berth-remote"))
        );
        let home = match cfg!(target_os = "macos") {
            true => "/home/ada/Library/Application Support/berth",
            false => "/home/ada/.config/berth",
        };
        for relative in ["", "scratch/c"] {
            assert_eq!(
                folder(env(match relative {
                    "" => &[("XDG_CONFIG_HOME", ""), ("HOME", "/home/ada")],
                    _ => &[("XDG_CONFIG_HOME", "scratch/c"), ("HOME", "/home/ada")],
                })),
                Some(PathBuf::from(home))
            );
        }
        assert_eq!(folder(env(&[])), None);
    }
}
