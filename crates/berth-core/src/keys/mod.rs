//! How berth logs in to hosts over SSH. This is the only code that builds
//! ssh's options: for `ssh` itself, for the link to a host's berthd, for the
//! Ansible inventory and for the logins of first contact and rekeying. A
//! host's profile says how it is logged in to in `access.auth`:
//!
//! | `auth` | What ssh is given |
//! | --- | --- |
//! | `berth` | The key `berth` of the ring, and no other |
//! | `key:NAME` | That key of the ring, and no other |
//! | `password` | No key; the password through [`askpass`], from the secret `ssh/HOST` |
//! | `ssh` | Your own SSH configuration and agent, with berth's key offered after them |
//!
//! Every login checks the host's key against the known-hosts file berth
//! writes for it ([`known`]): an unknown host is recorded at first contact
//! and a changed one is refused.

pub mod askpass;
pub mod contact;
pub mod known;
pub mod refusal;
pub mod ring;

use crate::Result;
use crate::link::LinkSpec;
use crate::paths::Paths;
use crate::profiles::Place;
use crate::store::{OWN, Store, kind};
use berth_api::profile::{Access, Auth, BERTH_KEY};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The secret that holds a host's SSH password.
pub fn password_secret(place: &Place) -> String {
    format!("ssh/{}", place.argument())
}

/// The secret that holds the SSH user's sudo password, when it is kept.
pub fn sudo_secret(place: &Place) -> String {
    format!("sudo/{}", place.argument())
}

/// A credential used once, at first contact.
pub enum Once {
    Password(String),
    /// A private key, checked already.
    Key(ring::Halves),
}

/// Everything ssh needs to log in to one host.
#[derive(Debug, Clone)]
pub struct Login {
    /// The host's name, for messages.
    pub name: String,
    pub address: String,
    pub user: String,
    pub port: String,
    pub auth: Auth,
    alias: String,
    known_hosts: PathBuf,
    /// Keys offered, in order.
    identities: Vec<PathBuf>,
    /// Whether ssh offers those keys only, not the agent's or its defaults.
    only: bool,
    /// Where the askpass helper finds the password: `secret:NAME` or `file:PATH`.
    askpass: Option<String>,
    /// Where a refusal of this login is kept; none for a credential used once.
    guard: Option<refusal::Guard>,
}

/// A value of an `-o` option, quoted for ssh when it has spaces.
fn quoted(path: &Path) -> String {
    let text = path.display().to_string();
    match text.contains(char::is_whitespace) {
        true => format!("\"{text}\""),
        false => text,
    }
}

/// A word for a shell, as Ansible splits `ansible_ssh_common_args`.
fn shell_word(word: &str) -> String {
    if !word.is_empty()
        && word
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "-_=./,:@%+".contains(ch))
    {
        return word.to_owned();
    }
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// The keys ssh tries by default, which naming any identity leaves out, so
/// `auth: ssh` names those that exist.
fn default_identities() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return vec![];
    };
    [
        "id_rsa",
        "id_ecdsa",
        "id_ecdsa_sk",
        "id_ed25519",
        "id_ed25519_sk",
    ]
    .iter()
    .map(|name| home.join(".ssh").join(name))
    .filter(|path| path.is_file())
    .collect()
}

/// The files of a ring's key and of its pending successor, for logging in
/// while a rotation runs.
fn key_files(paths: &Paths, store: &Store, name: &str) -> Result<Vec<PathBuf>> {
    let mut files = vec![];
    files.extend(ring::file(paths, store, name, false)?);
    files.extend(ring::file(paths, store, name, true)?);
    Ok(files)
}

/// How to log in to the host whose profile is `config` at `place`.
pub fn login(paths: &Paths, store: &Store, place: &Place, config: &Value) -> Result<Login> {
    let access = Access::of(config);
    let served = config["builder"]["served"].is_string();
    // A berth server's host is reached by the server with the server's keys;
    // ssh from here can only be the user's own.
    let auth = match served {
        true => Auth::Ssh,
        false => access.auth.clone(),
    };
    let mut login = Login {
        name: place.argument(),
        address: access.address.clone(),
        user: access.user.clone(),
        port: access.port.clone(),
        auth: auth.clone(),
        alias: known::alias(place),
        known_hosts: known::prepare(paths, store, place)?,
        identities: vec![],
        only: false,
        askpass: None,
        guard: None,
    };
    match &auth {
        Auth::Berth | Auth::Key(_) => {
            login.identities = key_files(paths, store, auth.key().unwrap_or(BERTH_KEY))?;
            login.only = true;
        }
        Auth::Ssh => {
            login.identities = default_identities();
            if !served {
                login.identities.extend(key_files(paths, store, BERTH_KEY)?);
            }
        }
        Auth::Password => {
            let secret = password_secret(place);
            if store.names(kind::SECRET, OWN)?.contains(&secret) {
                login.askpass = Some(format!("secret:{secret}"));
            }
        }
    }
    login.guard = Some(refusal::Guard {
        paths: paths.clone(),
        place: place.clone(),
        login: login.identity(),
        name: login.name.clone(),
    });
    Ok(login)
}

/// The login of a host through the installation's store.
pub fn login_of(paths: &Paths, host: &crate::host::Host) -> Result<Login> {
    let store = crate::store::shared(paths)?;
    login(paths, &store, &host.place, &host.config)
}

impl Login {
    /// The same host with a credential used once: that password, or that key
    /// alone.
    pub fn with_once(&self, once: &Once, dir: &Path) -> Result<Login> {
        let mut login = self.clone();
        match once {
            Once::Password(password) => {
                let file = dir.join("password");
                crate::paths::write_private(&file, password.as_bytes())?;
                login.auth = Auth::Password;
                login.identities.clear();
                login.only = false;
                login.askpass = Some(format!("file:{}", file.display()));
                login.guard = None;
            }
            Once::Key(halves) => {
                let file = dir.join("once");
                crate::paths::write_private(&file, halves.private.as_bytes())?;
                login.auth = Auth::Key("once".into());
                login.identities = vec![file];
                login.only = true;
                login.askpass = None;
                login.guard = None;
            }
        }
        Ok(login)
    }

    /// The same host logged in to with only these key files.
    pub fn with_keys(&self, files: Vec<PathBuf>) -> Login {
        let mut login = self.clone();
        login.auth = Auth::Key("rotation".into());
        login.identities = files;
        login.only = true;
        login.askpass = None;
        let identity = login.identity();
        if let Some(guard) = &mut login.guard {
            guard.login = identity;
        }
        login
    }

    /// Which login this is, to tell whether a refusal was of it: where it
    /// goes, as whom, and with which keys or password. Another key, address
    /// or user is another login.
    pub fn identity(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut hash = Sha256::new();
        for part in [&self.address, &self.user, &self.port] {
            hash.update(part.as_bytes());
            hash.update([0]);
        }
        hash.update([self.only as u8]);
        for file in &self.identities {
            hash.update(std::fs::read(file).unwrap_or_default());
            hash.update([0]);
        }
        hash.update(self.askpass.as_deref().unwrap_or("").as_bytes());
        hash.finalize()[..12]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// Why nothing automatic logs in with this login now: the host refused
    /// it a short while ago.
    pub fn refused(&self) -> Option<String> {
        self.guard.as_ref()?.refused()
    }

    /// Whether the host refused this login a short while ago.
    pub fn refusal(&self) -> Option<refusal::Refusal> {
        self.guard.as_ref()?.holds()
    }

    /// Note how a login went: what ssh said when it failed, or nothing when
    /// it worked. Returns what to say instead of ssh's words.
    pub fn noted(&self, failed: Option<&str>) -> Option<String> {
        let guard = self.guard.as_ref()?;
        match failed {
            Some(said) => guard.failed(said),
            None => {
                guard.worked();
                None
            }
        }
    }

    /// `user@address`, or the address alone.
    pub fn target(&self) -> String {
        crate::host::target(&self.address, &self.user)
    }

    /// The `-o` options and the port, without the target.
    pub fn options(&self) -> Vec<String> {
        let mut args = vec![];
        if !self.port.is_empty() {
            args.extend(["-p".to_owned(), self.port.clone()]);
        }
        let mut options = vec![
            format!("HostKeyAlias={}", self.alias),
            format!("UserKnownHostsFile={}", quoted(&self.known_hosts)),
            // A host never seen is recorded; one whose key changed is refused.
            "StrictHostKeyChecking=accept-new".to_owned(),
            "HashKnownHosts=no".to_owned(),
            "CheckHostIP=no".to_owned(),
            // berth says itself which host key it recorded, in place of ssh's warning.
            "LogLevel=ERROR".to_owned(),
        ];
        match &self.auth {
            Auth::Password => options.extend([
                "PubkeyAuthentication=no".to_owned(),
                "PreferredAuthentications=keyboard-interactive,password".to_owned(),
                "NumberOfPasswordPrompts=1".to_owned(),
            ]),
            _ if self.only => options.extend([
                "IdentitiesOnly=yes".to_owned(),
                "PasswordAuthentication=no".to_owned(),
                "KbdInteractiveAuthentication=no".to_owned(),
            ]),
            _ => {}
        }
        for file in &self.identities {
            options.push(format!("IdentityFile={}", quoted(file)));
        }
        for option in options {
            args.extend(["-o".to_owned(), option]);
        }
        args
    }

    /// What ssh's environment needs. With a password berth has, ssh asks
    /// berth itself for it; otherwise ssh never asks a program, so no
    /// password dialog of the desktop opens, whatever `DISPLAY` says, and a
    /// prompt can only be on a terminal.
    pub fn env(&self) -> Vec<(String, String)> {
        let Some(source) = &self.askpass else {
            return vec![
                ("SSH_ASKPASS".into(), "/bin/false".into()),
                ("SSH_ASKPASS_REQUIRE".into(), "never".into()),
            ];
        };
        let helper = std::env::current_exe()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| "berth".into());
        vec![
            ("SSH_ASKPASS".into(), helper),
            ("SSH_ASKPASS_REQUIRE".into(), "force".into()),
            (askpass::SOURCE.into(), source.clone()),
        ]
    }

    /// Whether a login needs nobody to type anything: a key, or a password
    /// berth has.
    fn batch(&self) -> bool {
        self.auth != Auth::Password || self.askpass.is_some()
    }

    /// Why logging in cannot work without someone to type a password.
    pub fn needs_password(&self) -> Option<String> {
        (!self.batch()).then(|| {
            format!(
                "{} logs in with a password, and berth has none stored. Store it with: berth secret set ssh/{}, or install a key with: berth key trust {}",
                self.name, self.name, self.name
            )
        })
    }

    /// `ssh` for a shell or a command on the host, interactive or not. The
    /// target and the command come after.
    pub fn ssh(&self) -> Command {
        let mut command = Command::new("ssh");
        command.args(self.options());
        command.envs(self.env());
        command
    }

    /// `ssh` for a command run by berth itself: nobody types anything and no
    /// connection is shared, so each login is tested on its own.
    pub fn batch_ssh(&self) -> Command {
        let mut command = self.ssh();
        command.args(["-o", "ConnectTimeout=10", "-o", "ControlPath=none"]);
        if self.auth != Auth::Password {
            command.args(["-o", "BatchMode=yes"]);
        }
        command.stdin(std::process::Stdio::null());
        command
    }

    /// The link to the host's berthd over SSH. Connections share one SSH
    /// connection, so only the first pays for the handshake. `password` is
    /// the SSH user's sudo password.
    pub fn link(&self, password: Option<&str>) -> Result<LinkSpec> {
        if let Some(why) = self.needs_password() {
            return Err(why.into());
        }
        let mut args = self.options();
        let control = crate::link::runtime_dir().join("ssh-%C");
        let mut options = vec![
            "ConnectTimeout=10".to_owned(),
            "ControlMaster=auto".to_owned(),
            format!("ControlPath={}", quoted(&control)),
            "ControlPersist=60".to_owned(),
        ];
        // BatchMode would keep ssh from asking the helper for the password.
        if self.auth != Auth::Password {
            options.insert(0, "BatchMode=yes".to_owned());
        }
        for option in options {
            args.extend(["-o".into(), option]);
        }
        args.extend(["-T".into(), self.target()]);
        // The line sent first is read by the shell, never by berthd, and handed to
        // sudo only when sudo asks: applying the role lets the SSH user off the
        // password, and the profile still says to ask for it.
        args.push(match password {
            Some(_) => PASSWORD_LINK.into(),
            None => "sudo -n berthd connect".into(),
        });
        Ok(LinkSpec {
            program: "ssh".into(),
            args,
            first_line: password.map(str::to_owned),
            via: None,
            env: self.env(),
            name: self.name.clone(),
            guard: self.guard.clone(),
        })
    }

    /// The host as an Ansible inventory. A password is not in it: an apply
    /// gives it to Ansible in a private file.
    pub fn inventory(&self) -> Result<Value> {
        let port = check(&self.address, &self.port)?;
        let mut target = json!({"ansible_host": self.address, "ansible_connection": "ssh"});
        if !self.user.is_empty() {
            target["ansible_user"] = json!(self.user);
        }
        if let Some(port) = port {
            target["ansible_port"] = json!(port);
        }
        // The port is Ansible's own variable; the rest goes to every ssh it runs.
        let options = self.options();
        let options = match options.first().map(String::as_str) {
            Some("-p") => &options[2..],
            _ => &options[..],
        };
        target["ansible_ssh_common_args"] = json!(
            options
                .iter()
                .map(|word| shell_word(word))
                .collect::<Vec<_>>()
                .join(" ")
        );
        Ok(json!({"all": {"hosts": {"devbox_target": target}}}))
    }
}

/// Whether an address and a port can be given to ssh: the port, if there is one.
pub fn check(address: &str, port: &str) -> Result<Option<u16>> {
    if address.is_empty()
        || address.starts_with('-')
        || address
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return Err("Enter an SSH hostname, IP address or config alias".into());
    }
    if port.is_empty() {
        return Ok(None);
    }
    port.parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .map(Some)
        .ok_or_else(|| "SSH port must be between 1 and 65535".into())
}

const PASSWORD_LINK: &str = r#"sh -c 'IFS= read -r p; if sudo -n true 2>/dev/null; then exec sudo -n berthd connect; fi; { printf "%s\n" "$p"; exec cat; } | sudo -S -k -p "" berthd connect'"#;

/// The saved hosts of this installation that log in with a key of the ring.
pub fn users(store: &Store, key: &str) -> Result<Vec<String>> {
    Ok(store
        .entries(kind::PROFILE, OWN)?
        .into_iter()
        .filter_map(|(name, value)| {
            let config: Value = serde_json::from_slice(&value).ok()?;
            (Access::of(&config).auth.key() == Some(key)).then_some(name)
        })
        .collect())
}

/// The public halves a workstation's operator takes for this berth: the key
/// `berth`, and its successor while one is pending, so an apply during a
/// rotation keeps both.
pub fn operator_keys(paths: &Paths) -> Vec<String> {
    let Ok(store) = crate::store::shared(paths) else {
        return vec![];
    };
    let Ok(key) = ring::public(paths, &store, BERTH_KEY) else {
        return vec![];
    };
    let mut keys = vec![key.public];
    keys.extend(key.pending.map(|next| next.public));
    keys
}

#[cfg(test)]
mod tests;
