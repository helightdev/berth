//! A host as its profile describes it: where it is and how it is reached.

use crate::{Result, link::LinkSpec, paths::Paths, profiles::Place, server::Server};
use berth_api::profile::Access;
use serde_json::Value;
use std::process::Command;

pub struct Host {
    /// The name it is saved under, or the path of its file.
    pub name: String,
    /// Where its profile is.
    pub place: Place,
    pub config: Value,
}

impl Host {
    /// A saved host by name, or a profile by path.
    pub fn load(paths: &Paths, name: &str) -> Result<Host> {
        Host::load_at(paths, Place::resolve(paths, name)?)
    }

    /// The host whose profile is at `place`.
    pub fn load_at(paths: &Paths, place: Place) -> Result<Host> {
        let config = place
            .load(paths)?
            .ok_or_else(|| format!("No host {}", place.argument()))?;
        Ok(Host {
            name: place.argument(),
            place,
            config,
        })
    }

    /// The saved hosts that have an SSH address or a link command.
    pub fn all(paths: &Paths) -> Vec<Host> {
        let scope = crate::profiles::scope(paths);
        crate::profiles::list(paths, &scope)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(name, config)| {
                Some(Host {
                    place: Place::stored(&scope, &name).ok()?,
                    name,
                    config,
                })
            })
            .filter(|host| host.access().reachable())
            .collect()
    }

    /// Where the host is and how berth logs in to it.
    pub fn access(&self) -> Access {
        Access::of(&self.config)
    }

    pub fn address(&self) -> String {
        self.access().address
    }

    /// The SSH user sudo asks for a password.
    pub fn asks_password(&self) -> bool {
        berth_api::profile::asks_become_pass(&self.config)
    }

    /// The `berth serve` this host is reached through, for a profile written
    /// from a server's list of hosts. The token is a setting of `paths`, not
    /// part of a profile.
    pub fn served(&self, paths: &Paths) -> Option<Server> {
        let url = self.config["builder"]["served"].as_str()?;
        let configured = Server::configured(paths).filter(|server| server.url == url);
        Some(configured.unwrap_or(Server {
            url: url.to_owned(),
            token: std::env::var("BERTH_TOKEN").ok(),
            paths: Some(paths.clone()),
        }))
    }

    pub fn agent(&self) -> &str {
        match self.config["agent_username"].as_str() {
            None | Some("") => "agent",
            Some(user) => user,
        }
    }

    /// The SSH user's sudo password, for a host that asks for one: the
    /// secret `sudo/HOST` when it is kept, `BERTH_SUDO_PASSWORD`, or asked
    /// for on a terminal. The UI has its own dialog.
    pub fn sudo_password(&self, paths: &Paths) -> Result<Option<String>> {
        if !self.asks_password() {
            return Ok(None);
        }
        if let Some(kept) = self.kept_sudo_password(paths) {
            return Ok(Some(kept));
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

    /// The sudo password kept as the secret `sudo/HOST`, for a host whose
    /// user chose to keep it.
    pub fn kept_sudo_password(&self, paths: &Paths) -> Option<String> {
        if self.served(paths).is_some() {
            return None;
        }
        let store = crate::store::shared(paths).ok()?;
        crate::registry::Store::own(store)
            .secret(&crate::keys::sudo_secret(&self.place))
            .ok()
            .flatten()
    }

    /// The host as an Ansible inventory.
    pub fn inventory(&self, paths: &Paths) -> Result<Value> {
        crate::keys::login_of(paths, self)?.inventory()
    }

    /// `user@host`, or the host alone.
    pub fn target(&self) -> String {
        let access = self.access();
        target(&access.address, &access.user)
    }

    /// `ssh` with this host's options; the target and command come after.
    pub fn ssh(&self, paths: &Paths) -> Result<Command> {
        Ok(crate::keys::login_of(paths, self)?.ssh())
    }

    /// How to open a connection to the host's berthd. `password` is the SSH
    /// user's sudo password, for a host that asks for one.
    pub fn link(&self, paths: &Paths, password: Option<&str>) -> Result<LinkSpec> {
        if let Some(server) = self.served(paths) {
            return Ok(LinkSpec {
                via: Some((server, self.name.clone())),
                name: self.name.clone(),
                ..LinkSpec::default()
            });
        }
        // A profile can name the command itself, as for a container:
        // ["docker", "exec", "-i", "NAME", "berthd", "connect"].
        if let Some(command) = self.access().link {
            let mut words = command.into_iter();
            let program = words.next().ok_or("access.link is empty")?;
            return Ok(LinkSpec {
                program,
                args: words.collect(),
                name: self.name.clone(),
                ..LinkSpec::default()
            });
        }
        crate::keys::login_of(paths, self)?.link(password)
    }
}

pub fn target(host: &str, user: &str) -> String {
    match user {
        "" => host.to_owned(),
        user => format!("{user}@{host}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_profile_can_name_its_own_link() {
        let host = Host {
            name: "c".into(),
            place: Place::File("/c.json".into()),
            config: json!({"access": {"link": ["docker", "exec", "-i", "c", "berthd", "connect"]}}),
        };
        let paths = Paths {
            config_dir: "/nonexistent/config".into(),
            data_dir: "/nonexistent/data".into(),
        };
        let spec = host.link(&paths, None).unwrap();
        assert_eq!((spec.program.as_str(), spec.args.len()), ("docker", 5));
        assert!(spec.env.is_empty());
    }
}
