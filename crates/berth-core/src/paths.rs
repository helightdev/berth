//! Where berth keeps things on this machine, and its own settings.
//!
//! - `~/.config/berth/config.json`: the settings of this machine alone and
//!   those needed to open the store ([`is_local`]). The others, with the
//!   registries and secrets, are in berth's store
//! - `~/.local/share/berth/berth.db`: the store, unless the `store` setting
//!   names another
//! - the hosts' profiles, their generated passwords and the records of the
//!   machines berth created are in the store too ([`crate::profiles`])
//! - the SSH keys of the key ring and the token of `berth serve` are in the
//!   store, and written from it to `keys/NAME` and `serve.token` of the data
//!   directory when they are needed as files ([`crate::keys::ring`],
//!   [`crate::kept`]); each host's known-hosts file is in `known_hosts/`,
//!   written from the store ([`crate::keys::known`]); the user's own plugins are in `plugins/` of the
//!   configuration directory, which is rebuilt from the store
//!   ([`crate::plugin_state`])
//! - `~/.local/share/berth/locks/`: the locks held while something is done
//!   to a host's machine
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
    /// The playbook that installs berth's controller alone.
    pub const CONTROLLER_PLAYBOOK: &str = "provision/ansible/controller.yml";
    /// Ansible's settings for that playbook.
    pub const ANSIBLE_CONFIG: &str = "provision/ansible.cfg";
    /// The profile a new host starts from.
    pub const EXAMPLE: &str = "provision/config/example.json";
    /// Where a source of berth 0.2 and earlier had its builder.
    pub(crate) const OLD_BUILDER: &str = "bin/devbox.py";
}

/// Settings that `berth config KEY VALUE` can change. Those [`is_local`]
/// names are in `config.json`, the others in the store.
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
        "oauth_client_id",
        "the client berth is at the issuer, for getting access tokens of its own: berth login, or with oauth_client_secret",
    ),
    (
        "oauth_client_secret",
        "the client's secret: with it berth gets access tokens by itself, as a service does",
    ),
    (
        "oauth_scope",
        "the scope berth asks the issuer for, such as: openid offline_access berth:operate",
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
    (
        "store",
        "where berth keeps its state: a SQLite file, or a PostgreSQL URL such as postgres://berth@db.example.org/berth?sslmode=require (default: berth.db in the data directory)",
    ),
    (
        "store_key",
        "the key file that seals the store's values, made by berth store encrypt (default: store.key in the configuration directory)",
    ),
    (
        "store_ca",
        "a PEM file of certificate authorities to trust as well for a PostgreSQL store with sslmode=require",
    ),
];

/// Whether the value of a setting lets someone in, so that no list shows it
/// whole. `serve_token` is a switch that holds `off`, not a secret.
pub fn is_secret(key: &str) -> bool {
    // A PostgreSQL URL in `store` can hold a password.
    matches!(key, "server_token" | "oauth_client_secret" | "store")
}

/// Whether a setting is kept in `config.json` rather than in the store: it
/// belongs to this machine alone, or it is needed before the store can be
/// opened.
pub fn is_local(key: &str) -> bool {
    matches!(
        key,
        "store"
            | "store_key"
            | "store_ca"
            | "server"
            | "server_token"
            | "source"
            | "iso_dir"
            | "oauth_client_id"
            | "oauth_client_secret"
            | "oauth_scope"
    )
}

/// How long the settings read from the store are taken as they are. The
/// Settings tab redraws many times a second and reads each setting apart, so
/// without this every frame would ask the store once per setting.
const STORED_FOR: std::time::Duration = std::time::Duration::from_millis(500);

/// The stored settings of an installation, by its directories, and when they were read.
type StoredSettings = std::sync::Mutex<
    std::collections::HashMap<
        (PathBuf, PathBuf),
        (
            std::time::Instant,
            std::result::Result<Map<String, Value>, String>,
        ),
    >,
>;

fn stored_cache() -> &'static StoredSettings {
    static CACHE: std::sync::OnceLock<StoredSettings> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Write a file private to this user in one step: a reader sees the old
/// contents or the new, never a part, even when the writer is killed.
pub(crate) fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = path.parent().ok_or("a file without a directory")?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let mut name = std::ffi::OsString::from(".");
    name.push(path.file_name().unwrap_or_default());
    name.push(format!(".{}.tmp", std::process::id()));
    let temporary = dir.join(name);
    let written = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(data)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    written
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
    pub fn iso_dir(&self) -> PathBuf {
        let configured = self.local_settings().unwrap_or_default();
        match configured.get("iso_dir").and_then(|dir| dir.as_str()) {
            Some(dir) => crate::discovery::expand_path(dir),
            None => self.data_dir.join("iso"),
        }
    }
    pub fn build_dir(&self) -> PathBuf {
        self.data_dir.join("build")
    }

    /// The private half of this berth's own SSH key, the key `berth` of the
    /// key ring, as a file: an ed25519 key made the first time it is asked
    /// for. Hosts it sets up take the public half for their operator, so
    /// this berth reaches them without a key of the user's.
    pub fn own_key(&self) -> Result<PathBuf> {
        let store = crate::store::shared(self)?;
        crate::keys::ring::file(self, &store, berth_api::profile::BERTH_KEY, false)?
            .ok_or_else(|| "berth's own key is missing".into())
    }

    /// The public half of this berth's own SSH key.
    pub fn own_public_key(&self) -> Option<String> {
        let store = crate::store::shared(self).ok()?;
        let key = crate::keys::ring::public(self, &store, berth_api::profile::BERTH_KEY).ok()?;
        Some(key.public).filter(|public| !public.is_empty())
    }

    /// Create the private directories berth writes to.
    pub fn prepare(&self) -> Result<()> {
        for dir in [&self.config_dir, &self.build_dir()] {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        Ok(())
    }

    /// What `config.json` holds: the settings [`is_local`] names, and keys
    /// this berth does not know. Reading it never opens the store.
    pub fn local_settings(&self) -> Result<Map<String, Value>> {
        match fs::read(self.settings_file()) {
            Ok(data) => Ok(serde_json::from_slice(&data)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
            Err(error) => Err(error.into()),
        }
    }

    /// The settings kept in the store, as text, read again at most every [`STORED_FOR`].
    fn stored_settings(&self) -> Result<Map<String, Value>> {
        let key = (self.config_dir.clone(), self.data_dir.clone());
        if let Some((read, settings)) = stored_cache()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&key)
            && read.elapsed() < STORED_FOR
        {
            return settings.clone().map_err(Into::into);
        }
        let settings = (|| -> Result<Map<String, Value>> {
            let store = crate::store::shared(self)?;
            let entries = store.entries(crate::store::kind::SETTING, crate::store::OWN)?;
            Ok(entries
                .into_iter()
                .map(|(name, value)| (name, json!(String::from_utf8_lossy(&value))))
                .collect())
        })()
        .map_err(|error| error.to_string());
        stored_cache()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key, (std::time::Instant::now(), settings.clone()));
        settings.map_err(Into::into)
    }

    /// The saved settings, those of `config.json` and those of the store, without the environment.
    pub fn settings(&self) -> Result<Map<String, Value>> {
        let mut settings = self.local_settings()?;
        settings.extend(self.stored_settings()?);
        Ok(settings)
    }

    /// A setting from the environment, as `BERTH_` and its name in capitals,
    /// or as it is saved: for a container, or inside another program.
    pub fn setting(&self, key: &str) -> Option<String> {
        let variable = format!("BERTH_{}", key.to_uppercase());
        self.setting_with(key, std::env::var(variable).ok())
    }

    /// A setting as [`Paths::setting`] gives it, with `environment` as the
    /// value of its variable. A local setting never opens the store.
    pub fn setting_with(&self, key: &str, environment: Option<String>) -> Option<String> {
        let saved = |settings: Result<Map<String, Value>>| {
            Some(settings.ok()?.get(key)?.as_str()?.to_owned())
        };
        environment
            .or_else(|| match is_local(key) {
                true => saved(self.local_settings()),
                false => saved(self.stored_settings()),
            })
            .filter(|value| !value.is_empty())
    }

    pub fn set(&self, key: &str, value: &str) -> Result<()> {
        if !SETTINGS.iter().any(|(name, _)| *name == key) {
            return Err(format!("Unknown setting: {key}").into());
        }
        if !value.is_empty()
            && key == "tailnet_login_server"
            && !(value.starts_with("https://") || value.starts_with("http://"))
        {
            return Err("tailnet_login_server is an address such as https://hs.example.org".into());
        }
        // What was read before is not taken any more, whichever setting changes.
        stored_cache()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&(self.config_dir.clone(), self.data_dir.clone()));
        if !is_local(key) {
            let store = crate::store::shared(self)?;
            let (kind, scope) = (crate::store::kind::SETTING, crate::store::OWN);
            match value.is_empty() {
                true => store.forget(kind, scope, key).map(|_| ())?,
                false => store.write(kind, scope, key, value.as_bytes())?,
            };
            // A read since the lock above may have taken the old value.
            stored_cache()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&(self.config_dir.clone(), self.data_dir.clone()));
            return Ok(());
        }
        let mut settings = self.local_settings()?;
        if value.is_empty() {
            settings.remove(key);
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
        let text = serde_json::to_string_pretty(&settings)? + "\n";
        write_private(&self.settings_file(), text.as_bytes())
    }

    /// The provisioning source: `BERTH_SOURCE`, the `source` setting, the current
    /// directory when berth is run from a checkout, and otherwise berth's own
    /// copy in its data directory.
    ///
    /// A source named in the environment or the settings that is laid out as
    /// an older berth's is an error rather than passed over, so changes made in
    /// it are not left out without a word.
    pub fn source(&self) -> Result<PathBuf> {
        let configured = self.local_settings()?;
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
        // Two threads that unpack at once would each remove the other's copy.
        static UNPACKING: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _one = UNPACKING
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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

    /// Saved hosts, as (name, SSH address or empty): this installation's, or
    /// those of the berth server it uses.
    pub fn hosts(&self) -> Vec<(String, String)> {
        crate::profiles::list(self, &crate::profiles::scope(self))
            .unwrap_or_default()
            .into_iter()
            .map(|(name, profile)| (name, berth_api::profile::Access::of(&profile).address))
            .collect()
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
    fn hosts_are_listed_from_the_store_and_settings_checked() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: home.path().join("config"),
            data_dir: home.path().join("data"),
        };
        paths.prepare().unwrap();
        crate::profiles::Place::own("laptop")
            .unwrap()
            .save(&paths, &json!({"access": {"address": "100.64.0.1"}}))
            .unwrap();
        crate::profiles::Place::own("attic")
            .unwrap()
            .save(&paths, &json!({}))
            .unwrap();
        assert_eq!(
            paths.hosts(),
            [
                ("attic".into(), "".into()),
                ("laptop".into(), "100.64.0.1".into())
            ]
        );

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
        let mut settings = paths.local_settings().unwrap();
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

    fn scratch() -> (tempfile::TempDir, Paths) {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: home.path().join("config"),
            data_dir: home.path().join("data"),
        };
        (home, paths)
    }

    #[test]
    fn settings_live_in_the_file_or_in_the_store() {
        let (_home, paths) = scratch();
        paths.set("default_host", "studio").unwrap();
        paths.set("server", "http://192.0.2.7:7777").unwrap();
        assert_eq!(
            paths.setting_with("default_host", None).as_deref(),
            Some("studio")
        );
        let file = paths.local_settings().unwrap();
        assert_eq!(file.keys().collect::<Vec<_>>(), ["server"]);
        let store = crate::store::shared(&paths).unwrap();
        assert_eq!(
            store
                .read(
                    crate::store::kind::SETTING,
                    crate::store::OWN,
                    "default_host"
                )
                .unwrap()
                .unwrap(),
            b"studio"
        );
        let all = paths.settings().unwrap();
        assert_eq!(all["default_host"], "studio");
        assert_eq!(all["server"], "http://192.0.2.7:7777");
        // The environment wins.
        assert_eq!(
            paths
                .setting_with("default_host", Some("attic".into()))
                .as_deref(),
            Some("attic")
        );
        // An empty value clears it.
        paths.set("default_host", "").unwrap();
        assert_eq!(paths.setting_with("default_host", None), None);
        assert!(!paths.settings().unwrap().contains_key("default_host"));
        let mode = fs::metadata(paths.settings_file())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn a_local_setting_never_opens_the_store() {
        let (home, paths) = scratch();
        // A store that cannot be opened: its directory is a file.
        fs::write(home.path().join("file"), "").unwrap();
        let unopenable = home.path().join("file/berth.db");
        paths.set("store", unopenable.to_str().unwrap()).unwrap();
        paths.set("server", "http://192.0.2.7:7777").unwrap();
        assert_eq!(
            paths.setting_with("server", None).as_deref(),
            Some("http://192.0.2.7:7777")
        );
        assert!(crate::server::Server::configured(&paths).is_some());
        assert_eq!(
            crate::profiles::scope(&paths),
            "served:http://192.0.2.7:7777"
        );
        assert_eq!(paths.setting_with("default_host", None), None);
        assert!(paths.settings().is_err());
        assert!(paths.set("default_host", "studio").is_err());
        // And the store can be set back.
        paths.set("store", "").unwrap();
        paths.set("default_host", "studio").unwrap();
        assert_eq!(
            paths.setting_with("default_host", None).as_deref(),
            Some("studio")
        );
    }

    #[test]
    fn setting_drops_what_was_read_before() {
        let (_home, paths) = scratch();
        paths.set("tailnet_user", "agents").unwrap();
        let read = std::time::Instant::now();
        assert_eq!(
            paths.setting_with("tailnet_user", None).as_deref(),
            Some("agents")
        );
        // Written past the settings, the change is seen only once they are read again.
        let store = crate::store::shared(&paths).unwrap();
        let (kind, own) = (crate::store::kind::SETTING, crate::store::OWN);
        store.write(kind, own, "tailnet_user", b"others").unwrap();
        let seen = paths.setting_with("tailnet_user", None);
        if read.elapsed() < STORED_FOR / 2 {
            assert_eq!(seen.as_deref(), Some("agents"));
        }
        paths.set("tailnet_tags", "tag:berth").unwrap();
        assert_eq!(
            paths.setting_with("tailnet_user", None).as_deref(),
            Some("others")
        );
        assert_eq!(
            paths.setting_with("tailnet_tags", None).as_deref(),
            Some("tag:berth")
        );
    }
}
