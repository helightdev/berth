//! Where berth keeps things on this machine, and its own settings.
//!
//! - `~/.config/berth/config.json`: the tool's settings
//! - `~/.config/berth/hosts/NAME.json`: one profile per host, private, with
//!   `NAME.credentials.json` and `NAME.instance.json` beside it
//! - `~/.local/share/berth/source/`: the provisioning source, unpacked from the
//!   program itself whenever a new version runs. It is laid out as a checkout
//!   is, with the paths in [`layout`]
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

/// Where things are in a provisioning source, a checkout or berth's own copy,
/// relative to its root. The plugins that come with berth are in `plugins/`.
pub mod layout {
    /// Builds the release archive, seeds and installer images.
    pub const BUILDER: &str = "provision/bin/devbox.py";
    /// Writes an installer image to a USB stick.
    pub const WRITE_USB: &str = "provision/bin/write-usb.sh";
    /// The playbook that applies a profile.
    pub const PLAYBOOK: &str = "provision/ansible/site.yml";
    /// Ansible's settings for that playbook.
    pub const ANSIBLE_CONFIG: &str = "provision/ansible.cfg";
    /// The profile a new host starts from.
    pub const EXAMPLE: &str = "provision/config/example.json";
    /// Where a source of berth 0.2 and earlier had its builder.
    pub(crate) const OLD_BUILDER: &str = "bin/devbox.py";
}

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
    (
        "server",
        "address of a berth serve to use instead of hosts of this machine: http://HOST:PORT, https://HOST or unix:/path/to/serve.sock",
    ),
    (
        "server_token",
        "the token of that berth serve (its serve.token), for an address over the network",
    ),
    (
        "retention_days",
        "days berth serve keeps messages and jobs in its store (default: 90)",
    ),
    (
        "serve_setup",
        "off: berth serve lets nobody over the network change hosts, secrets, registries or settings",
    ),
    (
        "serve_token",
        "off: berth serve takes no serve.token over the network, only OAuth 2.0 access tokens",
    ),
    (
        "oauth_issuer",
        "berth serve takes access tokens (JWTs) of this OAuth 2.0 issuer: its address, as in the tokens' iss",
    ),
    (
        "oauth_jwks",
        "address of the issuer's keys, when its metadata is not at the usual place",
    ),
    (
        "oauth_audience",
        "the aud an access token needs for berth serve to take it",
    ),
    (
        "oauth_claim",
        "the claim with the roles berth:view, berth:operate and berth:admin (default: scope; dots go into objects, as in realm_access.roles)",
    ),
    (
        "tailnet_login_server",
        "address of the Headscale server hosts join, such as https://hs.example.org (empty: Tailscale's own service)",
    ),
    (
        "tailnet_user",
        "the Headscale user that new hosts belong to, by name or number",
    ),
    (
        "tailnet_tags",
        "tags new hosts get, comma separated, such as tag:berth (for Tailscale also DEVBOX_TAILSCALE_TAGS)",
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
    /// The profiles: of this machine, or those written for a server's hosts.
    pub fn hosts_dir(&self) -> PathBuf {
        match crate::server::Server::configured(self) {
            Some(server) => server.hosts_dir(self),
            None => self.config_dir.join("hosts"),
        }
    }
    /// Where the registries and secrets are: of this machine, or what is
    /// known of a server's.
    pub fn store_dir(&self) -> PathBuf {
        match crate::server::Server::configured(self) {
            Some(server) => server.dir(self),
            None => self.config_dir.clone(),
        }
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

    /// The private half of this berth's own SSH key, an ed25519 one made the
    /// first time it is asked for. Hosts it sets up take the public half for
    /// their operator, so this berth reaches them without a key of the user's.
    pub fn own_key(&self) -> Result<PathBuf> {
        let key = self.data_dir.join("id_ed25519");
        if key.is_file() && key.with_extension("pub").is_file() {
            return Ok(key);
        }
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.data_dir)?;
        // A half without the other is of no use, and ssh-keygen asks before it overwrites.
        let _ = fs::remove_file(&key);
        let _ = fs::remove_file(key.with_extension("pub"));
        // The comment only tells the keys of several berths apart in authorized_keys.
        let machine = fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
        let machine = machine.trim().split('.').next().unwrap_or("");
        let comment = match machine {
            "" => "berth".to_owned(),
            machine => format!("berth@{machine}"),
        };
        let made = std::process::Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C"])
            .arg(comment)
            .arg("-f")
            .arg(&key)
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|error| format!("ssh-keygen: {error}"))?;
        if !made.status.success() {
            return Err(String::from_utf8_lossy(&made.stderr)
                .trim()
                .to_owned()
                .into());
        }
        Ok(key)
    }

    /// Give a test's berth an own key of its own, so that nothing runs
    /// ssh-keygen, which needs an account in the password database.
    #[cfg(test)]
    pub fn give_own_key(&self) {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.data_dir)
            .unwrap();
        let key = self.data_dir.join("id_ed25519");
        fs::write(&key, "not a real private key\n").unwrap();
        fs::write(key.with_extension("pub"), "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAABAgMEBQYHCAkKCwwNDg8QERITFBUWFxgZGhscHR4f berth@studio\n").unwrap();
    }

    /// The public half of this berth's own SSH key.
    pub fn own_public_key(&self) -> Option<String> {
        let key = self.own_key().ok()?;
        let public = fs::read_to_string(key.with_extension("pub")).ok()?;
        Some(public.trim().to_owned()).filter(|public| !public.is_empty())
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

    /// A setting from the environment, as `BERTH_` and its name in capitals,
    /// or as it is saved: for a container, or inside another program.
    pub fn setting(&self, key: &str) -> Option<String> {
        let variable = format!("BERTH_{}", key.to_uppercase());
        std::env::var(variable)
            .ok()
            .or_else(|| Some(self.settings().ok()?.get(key)?.as_str()?.to_owned()))
            .filter(|value| !value.is_empty())
    }

    pub fn set(&self, key: &str, value: &str) -> Result<()> {
        if !SETTINGS.iter().any(|(name, _)| *name == key) {
            return Err(format!("Unknown setting: {key}").into());
        }
        let mut settings = self.settings()?;
        if value.is_empty() {
            settings.remove(key);
        } else if key == "tailnet_login_server"
            && !(value.starts_with("https://") || value.starts_with("http://"))
        {
            return Err("tailnet_login_server is an address such as https://hs.example.org".into());
        } else if key == "source" {
            let source = fs::canonicalize(crate::discovery::expand_path(value))?;
            older_source(&source)?;
            if !is_source(&source) {
                return Err(format!("{} has no {}", source.display(), layout::BUILDER).into());
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
    ///
    /// A source named in the environment or the settings that is laid out as
    /// an older berth's is an error rather than passed over, so changes made in
    /// it are not left out without a word.
    pub fn source(&self) -> Result<PathBuf> {
        let configured = self.settings()?;
        let named = [
            std::env::var_os("BERTH_SOURCE").map(PathBuf::from),
            configured
                .get("source")
                .and_then(|dir| dir.as_str())
                .map(PathBuf::from),
        ];
        for dir in named.into_iter().flatten() {
            if is_source(&dir) {
                return Ok(dir);
            }
            older_source(&dir)?;
        }
        match std::env::current_dir().ok().filter(|dir| is_source(dir)) {
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
                if name.ends_with(".credentials") || name.ends_with(".instance") {
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
    dir.join(layout::BUILDER).is_file()
}

/// An error for a source laid out as before `provision/` held the installer's side.
fn older_source(dir: &Path) -> Result<()> {
    if dir.join(layout::OLD_BUILDER).is_file() && !is_source(dir) {
        return Err(format!(
            "{} is a provisioning source of an older berth, with {} where this berth looks for {}. Update that checkout, or stop using it with: berth config source \"\" (and unset BERTH_SOURCE)",
            dir.display(),
            layout::OLD_BUILDER,
            layout::BUILDER
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_of_earlier_versions_are_left_alone() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: home.path().join("config"),
            data_dir: home.path().join("data"),
        };
        paths.prepare().unwrap();
        fs::write(
            paths.settings_file(),
            r#"{"oauth_client_id":"berth-cli","oauth_scope":"openid","server":"http://192.0.2.1:7777"}"#,
        )
        .unwrap();
        fs::write(paths.data_dir.join("oauth.json"), "{}").unwrap();
        assert_eq!(paths.settings().unwrap()["server"], "http://192.0.2.1:7777");
        paths.set("default_host", "studio").unwrap();
        assert_eq!(paths.settings().unwrap()["oauth_client_id"], "berth-cli");
    }

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
        fs::write(paths.hosts_dir().join("laptop.instance.json"), "{}").unwrap();
        assert_eq!(paths.hosts(), [("laptop".into(), "100.64.0.1".into())]);

        assert!(paths.set("colour", "blue").is_err());
        paths.set("default_host", "laptop").unwrap();
        assert!(paths.set("tailnet_login_server", "hs.example.org").is_err());
        assert_eq!(paths.settings().unwrap()["default_host"], "laptop");
        assert!(paths.set("source", home.path().to_str().unwrap()).is_err());

        // A checkout of an older berth is named as one, not passed over.
        let old = home.path().join("old");
        fs::create_dir_all(old.join("bin")).unwrap();
        fs::write(old.join(layout::OLD_BUILDER), "").unwrap();
        let said = paths.set("source", old.to_str().unwrap()).unwrap_err();
        assert!(said.to_string().contains("older berth"), "{said}");
        let mut settings = paths.settings().unwrap();
        settings.insert("source".into(), json!(old));
        fs::write(paths.settings_file(), Value::Object(settings).to_string()).unwrap();
        assert!(
            paths
                .source()
                .unwrap_err()
                .to_string()
                .contains("older berth")
        );
        fs::create_dir_all(old.join("provision/bin")).unwrap();
        fs::write(old.join(layout::BUILDER), "").unwrap();
        assert_eq!(paths.source().unwrap(), old);
    }
}
