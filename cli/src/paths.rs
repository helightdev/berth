//! Where berth keeps things on this machine, and its own settings.
//!
//! - `~/.config/berth/config.json`: the tool's settings
//! - `~/.config/berth/hosts/NAME.json`: one profile per host, private
//! - `~/.local/share/berth/source/`: the provisioning source, unpacked from the
//!   program itself whenever a new version runs
//! - `~/.local/share/berth/build/`: release archives, seeds and installer images
//! - `~/.local/share/berth/iso/`: downloaded Ubuntu installer ISOs
//!
//! `XDG_CONFIG_HOME` and `XDG_DATA_HOME` are honoured.

use crate::Result;
use serde_json::{Map, Value, json};
use std::{
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
};

/// The provisioning source this program was built with, and its SHA-256.
const SOURCE_ARCHIVE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/devbox-release.tar.gz"));
const SOURCE_DIGEST: &str = include_str!(concat!(env!("OUT_DIR"), "/release.sha256"));

/// Settings in `config.json` that `berth config KEY VALUE` can change.
pub const SETTINGS: &[(&str, &str)] = &[
    (
        "source",
        "directory with a provisioning source to use instead of the built-in one, such as a checkout you are changing",
    ),
    ("default_host", "host selected first on the home screen"),
    (
        "iso_dir",
        "directory for downloaded Ubuntu installer ISOs (default: next to the builds)",
    ),
];

pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
}

fn base(variable: &str, fallback: &str) -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os(variable).filter(|dir| Path::new(dir).is_absolute()) {
        return Ok(PathBuf::from(dir).join("berth"));
    }
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(PathBuf::from(home).join(fallback).join("berth"))
}

impl Paths {
    pub fn locate() -> Result<Paths> {
        Ok(Paths {
            config_dir: base("XDG_CONFIG_HOME", ".config")?,
            data_dir: base("XDG_DATA_HOME", ".local/share")?,
        })
    }

    pub fn settings_file(&self) -> PathBuf {
        self.config_dir.join("config.json")
    }
    pub fn hosts_dir(&self) -> PathBuf {
        self.config_dir.join("hosts")
    }
    pub fn iso_dir(&self) -> PathBuf {
        let configured = self.settings().unwrap_or_default();
        match configured.get("iso_dir").and_then(|dir| dir.as_str()) {
            Some(dir) => crate::discovery::expand_path(dir),
            None => self.data_dir.join("iso"),
        }
    }
    pub fn build_dir(&self) -> PathBuf {
        self.data_dir.join("build")
    }

    /// Create the private directories berth writes to.
    pub fn prepare(&self) -> Result<()> {
        for dir in [self.hosts_dir(), self.build_dir()] {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        Ok(())
    }

    pub fn settings(&self) -> Result<Map<String, Value>> {
        match fs::read(self.settings_file()) {
            Ok(data) => Ok(serde_json::from_slice(&data)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn set(&self, key: &str, value: &str) -> Result<()> {
        if !SETTINGS.iter().any(|(name, _)| *name == key) {
            return Err(format!("Unknown setting: {key}").into());
        }
        let mut settings = self.settings()?;
        if value.is_empty() {
            settings.remove(key);
        } else if key == "source" {
            let source = fs::canonicalize(crate::discovery::expand_path(value))?;
            if !is_source(&source) {
                return Err(format!("{} has no bin/devbox.py", source.display()).into());
            }
            settings.insert(key.into(), json!(source));
        } else {
            settings.insert(key.into(), json!(value));
        }
        self.prepare()?;
        let file = self.settings_file();
        fs::write(&file, serde_json::to_string_pretty(&settings)? + "\n")?;
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600))?;
        Ok(())
    }

    /// The provisioning source: `BERTH_SOURCE`, the `source` setting, the current
    /// directory when berth is run from a checkout, and otherwise berth's own
    /// copy in its data directory.
    pub fn source(&self) -> Result<PathBuf> {
        let configured = self.settings()?;
        let candidates = [
            std::env::var_os("BERTH_SOURCE").map(PathBuf::from),
            configured
                .get("source")
                .and_then(|dir| dir.as_str())
                .map(PathBuf::from),
            std::env::current_dir().ok(),
        ];
        match candidates.into_iter().flatten().find(|dir| is_source(dir)) {
            Some(dir) => Ok(dir),
            None => self.own_source(),
        }
    }

    /// berth's own copy of the provisioning source, unpacked from the program
    /// when it is missing or from another build.
    fn own_source(&self) -> Result<PathBuf> {
        let dir = self.data_dir.join("source");
        let marker = dir.join(".berth-release");
        let digest = SOURCE_DIGEST.trim();
        if fs::read_to_string(&marker).is_ok_and(|known| known.trim() == digest) {
            return Ok(dir);
        }
        // Unpack next to it and swap, so an interrupted run leaves the old copy.
        let fresh = self.data_dir.join("source.new");
        let _ = fs::remove_dir_all(&fresh);
        fs::create_dir_all(&fresh)?;
        let mut tar = std::process::Command::new("tar")
            .args(["-xzf", "-", "-C"])
            .arg(&fresh)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .map_err(|error| format!("tar is needed to unpack the provisioning source: {error}"))?;
        if let Some(mut stdin) = tar.stdin.take() {
            use std::io::Write;
            stdin.write_all(SOURCE_ARCHIVE)?;
        }
        if !tar.wait()?.success() {
            return Err("Unpacking the provisioning source failed".into());
        }
        fs::write(fresh.join(".berth-release"), format!("{digest}\n"))?;
        let _ = fs::remove_dir_all(&dir);
        fs::rename(&fresh, &dir)?;
        Ok(dir)
    }

    /// A host given by name lives in the hosts directory; anything that looks
    /// like a path is used as it is.
    pub fn profile(&self, host: &str) -> Result<PathBuf> {
        if host.contains('/') || host.ends_with(".json") {
            return Ok(std::path::absolute(host)?);
        }
        let valid = !host.is_empty()
            && !host.starts_with(['-', '.'])
            && host
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || "-_.".contains(ch));
        if !valid {
            return Err(format!("Not a host name: {host}").into());
        }
        Ok(self.hosts_dir().join(format!("{host}.json")))
    }

    /// Saved hosts, as (name, SSH address or empty).
    pub fn hosts(&self) -> Vec<(String, String)> {
        let Ok(entries) = fs::read_dir(self.hosts_dir()) else {
            return vec![];
        };
        let mut hosts: Vec<_> = entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().into_string().ok()?;
                let name = name.strip_suffix(".json")?;
                if name.ends_with(".credentials") {
                    return None;
                }
                let profile: Value = serde_json::from_slice(&fs::read(entry.path()).ok()?).ok()?;
                let address = profile["builder"]["ssh_host"].as_str().unwrap_or("");
                Some((name.to_owned(), address.to_owned()))
            })
            .collect();
        hosts.sort();
        hosts
    }
}

fn is_source(dir: &Path) -> bool {
    dir.join("bin/devbox.py").is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_are_named_files_and_paths_stay_paths() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: home.path().join("config"),
            data_dir: home.path().join("data"),
        };
        paths.prepare().unwrap();
        assert_eq!(
            paths.profile("laptop").unwrap(),
            paths.hosts_dir().join("laptop.json")
        );
        assert!(paths.profile("../etc/passwd").unwrap().is_absolute());
        assert!(paths.profile("-x").is_err());
        fs::write(
            paths.profile("laptop").unwrap(),
            r#"{"builder":{"ssh_host":"100.64.0.1"}}"#,
        )
        .unwrap();
        fs::write(paths.hosts_dir().join("laptop.credentials.json"), "{}").unwrap();
        assert_eq!(paths.hosts(), [("laptop".into(), "100.64.0.1".into())]);

        assert!(paths.set("colour", "blue").is_err());
        paths.set("default_host", "laptop").unwrap();
        assert_eq!(paths.settings().unwrap()["default_host"], "laptop");
        assert!(paths.set("source", home.path().to_str().unwrap()).is_err());
    }
}
