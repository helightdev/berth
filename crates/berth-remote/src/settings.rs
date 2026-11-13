//! The server and token berth-remote keeps between runs: a JSON file that
//! only its user can read, in a folder that only its user can open.

use serde_json::{Map, Value};
use std::io::Write;
use std::path::{Path, PathBuf};

/// The folder of the settings for the environment `var` gives: under
/// `%LOCALAPPDATA%` on Windows, `$XDG_CONFIG_HOME` where it is set, and else
/// `~/Library/Application Support` on macOS and `~/.config` elsewhere.
pub fn folder(var: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let var = |name: &str| var(name).filter(|value| !value.is_empty());
    let base = if cfg!(windows) {
        // Local rather than roaming, so that the token is not copied to
        // other machines with the profile.
        PathBuf::from(var("LOCALAPPDATA")?)
    } else if let Some(dir) = var("XDG_CONFIG_HOME") {
        PathBuf::from(dir)
    } else if cfg!(target_os = "macos") {
        PathBuf::from(var("HOME")?).join("Library/Application Support")
    } else {
        PathBuf::from(var("HOME")?).join(".config")
    };
    Some(base.join("berth-remote"))
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
        Some(self.dir.as_ref()?.join("settings.json"))
    }

    /// What is kept; nothing when the file is missing or unreadable.
    pub fn read(&self) -> Map<String, Value> {
        self.file()
            .and_then(|file| std::fs::read(file).ok())
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|value| match value {
                Value::Object(map) => Some(map),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// Keep `kept`, replacing what was. An empty map removes the file.
    pub fn write(&self, kept: &Map<String, Value>) -> std::io::Result<()> {
        let (Some(dir), Some(file)) = (&self.dir, self.file()) else {
            return Ok(());
        };
        if kept.is_empty() {
            return self.forget();
        }
        private_dir(dir)?;
        private_write(&file, Value::Object(kept.clone()).to_string().as_bytes())
    }

    /// Remove what was kept: the file, and its folder when nothing else is in it.
    pub fn forget(&self) -> std::io::Result<()> {
        let (Some(dir), Some(file)) = (&self.dir, self.file()) else {
            return Ok(());
        };
        for gone in [file.clone(), file.with_extension("json.partial")] {
            match std::fs::remove_file(&gone) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
                _ => {}
            }
        }
        // Not an error: a folder with other files in it stays.
        let _ = std::fs::remove_dir(dir);
        Ok(())
    }
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
    use serde_json::json;

    fn kept(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn keeps_and_forgets() {
        let scratch = tempfile::tempdir().unwrap();
        let dir = scratch.path().join("config/berth-remote");
        let settings = Settings::new(Some(dir.clone()));
        assert!(settings.read().is_empty());
        let both = kept(json!({"server": "https://studio.example", "token": "secret"}));
        settings.write(&both).unwrap();
        assert_eq!(settings.read(), both);
        settings
            .write(&kept(json!({"server": "https://dock.example"})))
            .unwrap();
        assert_eq!(settings.read()["server"], "https://dock.example");
        assert!(!settings.read().contains_key("token"));
        assert!(!dir.join("settings.json.partial").exists());
        settings.forget().unwrap();
        assert!(!dir.exists());
        assert!(settings.read().is_empty());
        // Forgetting twice, or what was never kept, is no error.
        settings.forget().unwrap();
    }

    #[test]
    fn an_empty_map_removes_the_file() {
        let scratch = tempfile::tempdir().unwrap();
        let settings = Settings::new(Some(scratch.path().join("berth-remote")));
        settings.write(&kept(json!({"token": "secret"}))).unwrap();
        settings.write(&Map::new()).unwrap();
        assert!(!scratch.path().join("berth-remote").exists());
    }

    #[test]
    fn a_file_that_is_not_settings_reads_as_nothing() {
        let scratch = tempfile::tempdir().unwrap();
        let settings = Settings::new(Some(scratch.path().to_owned()));
        std::fs::write(scratch.path().join("settings.json"), "[1, 2]").unwrap();
        assert!(settings.read().is_empty());
        std::fs::write(scratch.path().join("settings.json"), "{not json").unwrap();
        assert!(settings.read().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn only_the_owner_can_read_them() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = tempfile::tempdir().unwrap();
        let dir = scratch.path().join("berth-remote");
        // A folder made earlier with a looser mode is made private too.
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        // So is a file left over from a write that did not finish.
        std::fs::write(dir.join("settings.json.partial"), "{}").unwrap();
        std::fs::set_permissions(
            dir.join("settings.json.partial"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        let settings = Settings::new(Some(dir.clone()));
        settings.write(&kept(json!({"token": "secret"}))).unwrap();
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("settings.json")), 0o600);
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
                Some(PathBuf::from(r"C:\Users\ada\AppData\Local").join("berth-remote"))
            );
            assert_eq!(folder(env(&[])), None);
            return;
        }
        assert_eq!(
            folder(env(&[
                ("XDG_CONFIG_HOME", "/scratch/c"),
                ("HOME", "/home/ada")
            ])),
            Some(PathBuf::from("/scratch/c/berth-remote"))
        );
        let home = match cfg!(target_os = "macos") {
            true => "/home/ada/Library/Application Support/berth-remote",
            false => "/home/ada/.config/berth-remote",
        };
        assert_eq!(
            folder(env(&[("XDG_CONFIG_HOME", ""), ("HOME", "/home/ada")])),
            Some(PathBuf::from(home))
        );
        assert_eq!(folder(env(&[])), None);
    }
}
