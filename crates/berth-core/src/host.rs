//! A host as its profile describes it: where it is and how it is reached.

use crate::{Result, discovery::expand_path, link::LinkSpec, paths::Paths, server::Server};
use serde_json::{Value, json};
use std::{fs, path::PathBuf, process::Command};

pub struct Host {
    /// The name it is saved under, or the path it was given by.
    pub name: String,
    pub path: PathBuf,
    pub config: Value,
    /// This berth's own SSH key, offered to the host besides the user's.
    pub key: Option<PathBuf>,
}

impl Host {
    /// A saved host by name, or a profile by path.
    pub fn load(paths: &Paths, name: &str) -> Result<Host> {
        let path = paths.profile(name)?;
        let data = fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        let config: Value = serde_json::from_slice(&data)?;
        Ok(Host {
            name: name.to_owned(),
            path,
            config,
            key: paths.own_key().ok(),
        })
    }

    /// The saved hosts that have an SSH address or a link command.
    pub fn all(paths: &Paths) -> Vec<Host> {
        paths
            .hosts()
            .into_iter()
            .filter_map(|(name, _)| Host::load(paths, &name).ok())
            .filter(|host| {
                !host.builder("ssh_host").is_empty() || host.config["builder"]["link"].is_array()
            })
            .collect()
    }

    fn builder(&self, key: &str) -> &str {
        self.config["builder"][key].as_str().unwrap_or("")
    }

    pub fn address(&self) -> &str {
        self.builder("ssh_host")
    }

    /// The SSH user sudo asks for a password.
    pub fn asks_password(&self) -> bool {
        self.config["builder"]["ask_become_pass"]
            .as_bool()
            .unwrap_or(false)
    }

    /// The `berth serve` this host is reached through, for a profile written
    /// from a server's list of hosts.
    pub fn served(&self) -> Option<Server> {
        let url = self.config["builder"]["served"].as_str()?;
        // The token is a setting, not part of a profile.
        let configured = Paths::locate()
            .ok()
            .and_then(|paths| Server::configured(&paths))
            .filter(|server| server.url == url);
        Some(configured.unwrap_or(Server {
            url: url.to_owned(),
            token: std::env::var("BERTH_TOKEN").ok(),
        }))
    }

    pub fn agent(&self) -> &str {
        match self.config["agent_username"].as_str() {
            None | Some("") => "agent",
            Some(user) => user,
        }
    }

    /// The SSH user's sudo password, for a host that asks for one: from
    /// `BERTH_SUDO_PASSWORD`, or asked for on a terminal. The UI has its own dialog.
    pub fn sudo_password(&self) -> Result<Option<String>> {
        if !self.asks_password() {
            return Ok(None);
        }
        if let Ok(password) = std::env::var("BERTH_SUDO_PASSWORD") {
            return Ok(Some(password));
        }
        use std::io::IsTerminal;
        if !std::io::stdin().is_terminal() {
            // Tried without one: sudo says so itself when it does want it.
            return Ok(None);
        }
        crate::registry::read_secret(&format!("Your sudo password on {}", self.name)).map(Some)
    }

    /// The host as an Ansible inventory.
    pub fn inventory(&self) -> Result<Value> {
        // A server's host is reached with a key on the server.
        let served = self.config["builder"]["served"].is_string();
        let mut inventory = inventory(
            self.builder("ssh_host"),
            self.builder("ssh_user"),
            self.builder("ssh_port"),
            match served {
                true => "",
                false => self.builder("ssh_key"),
            },
        )?;
        let own = self.own_identity();
        if !served && !own.is_empty() {
            inventory["all"]["hosts"]["devbox_target"]["ansible_ssh_extra_args"] =
                json!(own.join(" "));
        }
        Ok(inventory)
    }

    /// The ssh options that offer this berth's own key, when the profile
    /// names no key. Naming an identity makes ssh leave out the ones it tries
    /// by default, so those that exist are named with it.
    fn own_identity(&self) -> Vec<String> {
        let Some(key) = self
            .key
            .as_ref()
            .filter(|_| self.builder("ssh_key").is_empty())
        else {
            return vec![];
        };
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let usual = [
            "id_rsa",
            "id_ecdsa",
            "id_ecdsa_sk",
            "id_ed25519",
            "id_ed25519_sk",
        ]
        .iter()
        .filter_map(|name| Some(home.as_ref()?.join(".ssh").join(name)))
        .filter(|path| path.is_file());
        usual
            .chain([key.clone()])
            .filter_map(|path| path.to_str().map(str::to_owned))
            // Each is one word of a command line.
            .filter(|path| !path.contains(char::is_whitespace))
            .flat_map(|path| ["-o".to_owned(), format!("IdentityFile={path}")])
            .collect()
    }

    /// `user@host`, or the host alone.
    pub fn target(&self) -> String {
        target(self.builder("ssh_host"), self.builder("ssh_user"))
    }

    /// `ssh` with this host's port and key.
    pub fn ssh(&self) -> Command {
        let mut command = ssh(self.builder("ssh_port"), self.builder("ssh_key"));
        command.args(self.own_identity());
        command
    }

    /// How to open a connection to the host's berthd. `password` is the SSH
    /// user's sudo password, for a host that asks for one.
    pub fn link(&self, password: Option<&str>) -> Result<LinkSpec> {
        if let Some(server) = self.served() {
            return Ok(LinkSpec {
                via: Some((server, self.name.clone())),
                ..LinkSpec::default()
            });
        }
        // A profile can name the command itself, as for a container:
        // ["docker", "exec", "-i", "NAME", "berthd", "connect"].
        if let Some(command) = self.config["builder"]["link"].as_array() {
            let mut words = command
                .iter()
                .filter_map(|word| word.as_str().map(str::to_owned));
            let program = words.next().ok_or("builder.link is empty")?;
            return Ok(LinkSpec {
                program,
                args: words.collect(),
                first_line: None,
                via: None,
            });
        }
        self.inventory()?;
        let mut spec = link(
            self.builder("ssh_host"),
            self.builder("ssh_user"),
            self.builder("ssh_port"),
            self.builder("ssh_key"),
            password,
        );
        spec.args.splice(0..0, self.own_identity());
        Ok(spec)
    }
}

pub fn target(host: &str, user: &str) -> String {
    match user {
        "" => host.to_owned(),
        user => format!("{user}@{host}"),
    }
}

pub fn ssh(port: &str, key: &str) -> Command {
    let mut command = Command::new("ssh");
    if !port.is_empty() {
        command.args(["-p", port]);
    }
    if !key.is_empty() {
        command.arg("-i").arg(expand_path(key));
    }
    command
}

/// The link to a host over SSH. Connections share one SSH connection, so only
/// the first pays for the handshake.
pub fn link(host: &str, user: &str, port: &str, key: &str, password: Option<&str>) -> LinkSpec {
    let mut args: Vec<String> = vec![];
    if !port.is_empty() {
        args.extend(["-p".into(), port.into()]);
    }
    if !key.is_empty() {
        args.extend(["-i".into(), expand_path(key).to_string_lossy().into_owned()]);
    }
    let control = crate::link::runtime_dir().join("ssh-%C");
    for option in [
        "BatchMode=yes".to_owned(),
        "ConnectTimeout=10".to_owned(),
        "ControlMaster=auto".to_owned(),
        format!("ControlPath={}", control.display()),
        "ControlPersist=60".to_owned(),
    ] {
        args.extend(["-o".into(), option]);
    }
    args.extend(["-T".into(), target(host, user)]);
    // The line sent first is read by the shell, never by berthd, and handed to
    // sudo only when sudo asks: applying the role lets the SSH user off the
    // password, and the profile still says to ask for it.
    args.push(match password {
        Some(_) => PASSWORD_LINK.into(),
        None => "sudo -n berthd connect".into(),
    });
    LinkSpec {
        program: "ssh".into(),
        args,
        first_line: password.map(str::to_owned),
        via: None,
    }
}

const PASSWORD_LINK: &str = r#"sh -c 'IFS= read -r p; if sudo -n true 2>/dev/null; then exec sudo -n berthd connect; fi; { printf "%s\n" "$p"; exec cat; } | sudo -S -k -p "" berthd connect'"#;

pub fn inventory(host: &str, user: &str, port: &str, key: &str) -> Result<Value> {
    if host.is_empty()
        || host.starts_with('-')
        || host.chars().any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return Err("Enter an SSH hostname, IP address or config alias".into());
    }
    let mut target = json!({"ansible_host": host, "ansible_connection": "ssh"});
    if !user.is_empty() {
        target["ansible_user"] = json!(user);
    }
    if !port.is_empty() {
        let port = port
            .parse::<u16>()
            .ok()
            .filter(|port| *port != 0)
            .ok_or("SSH port must be between 1 and 65535")?;
        target["ansible_port"] = json!(port);
    }
    if !key.is_empty() {
        let key = expand_path(key);
        if !key.is_file() {
            return Err("SSH private key file does not exist".into());
        }
        target["ansible_ssh_private_key_file"] = json!(key);
    }
    Ok(json!({"all": {"hosts": {"devbox_target": target}}}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_link_goes_through_sudo_and_shares_the_connection() {
        let spec = link("box", "ops", "2222", "", None);
        assert_eq!(spec.program, "ssh");
        assert_eq!(spec.args.last().unwrap(), "sudo -n berthd connect");
        assert!(spec.args.windows(2).any(|pair| pair == ["-p", "2222"]));
        assert!(spec.args.contains(&"ops@box".to_owned()));
        assert!(spec.args.iter().any(|arg| arg.starts_with("ControlPath=")));
        let spec = link("box", "", "", "", Some("secret"));
        assert_eq!(spec.first_line.as_deref(), Some("secret"));
        assert!(spec.args.last().unwrap().contains("sudo -S -k"));
        assert!(!spec.args.iter().any(|arg| arg.contains("secret")));
    }

    #[test]
    fn a_profile_can_name_its_own_link() {
        let host = Host {
            name: "c".into(),
            path: "c.json".into(),
            key: None,
            config: json!({"builder": {"link": ["docker", "exec", "-i", "c", "berthd", "connect"]}}),
        };
        let spec = host.link(None).unwrap();
        assert_eq!((spec.program.as_str(), spec.args.len()), ("docker", 5));
        assert!(inventory("-oProxyCommand=x", "", "", "").is_err());
        assert!(inventory("box", "", "0", "").is_err());
    }
}
