//! Applying a host's configuration: the Ansible run of the installer role
//! against it over SSH.

use crate::{Result, host::Host, paths::Paths, plugins};
use serde_json::{Value, json};
use std::{io::Write, process::Command};

/// A command ready to run, and the private files it reads. The files live as
/// long as this does, so keep it until the command has ended.
pub struct Prepared {
    pub command: Command,
    pub files: Vec<tempfile::NamedTempFile>,
}

/// Where each plugin is on this machine, by id.
pub fn plugin_dirs(plugins: &[plugins::Plugin]) -> Value {
    let dirs: serde_json::Map<String, Value> = plugins
        .iter()
        .map(|plugin| (plugin.id.clone(), json!(plugin.dir)))
        .collect();
    Value::Object(dirs)
}

/// Variables for a direct Ansible run, matching what the seed builder embeds.
/// `own` is the public key of the berth that applies them.
pub fn ansible_vars(config: &Value, plugin_dirs: &Value, own: Option<&str>) -> Value {
    let mut vars = config["ansible_vars"].clone();
    if !vars.is_object() {
        vars = json!({});
    }
    for (source, dest) in [
        ("agent_username", "devbox_user"),
        ("agent_password_hash", "devbox_agent_password_hash"),
        ("agent_passwordless_sudo", "devbox_agent_passwordless_sudo"),
        ("agent_privileges", "devbox_agent_privileges"),
        ("operator_username", "devbox_operator_user"),
        ("operator_password_hash", "devbox_operator_password_hash"),
        (
            "operator_passwordless_sudo",
            "devbox_operator_passwordless_sudo",
        ),
    ] {
        if let Some(value) = config.get(source) {
            vars[dest] = value.clone();
        }
    }
    // The role writes each account's keys as one block, a key to a line.
    vars["devbox_agent_ssh_public_key"] =
        json!(berth_api::profile::account_keys(config, "agent").join("\n"));
    vars["devbox_operator_ssh_public_key"] =
        json!(berth_api::profile::operator_keys(config, own).join("\n"));
    vars["devbox_profile"] = config["profile"].clone();
    vars["devbox_plugin_dirs"] = plugin_dirs.clone();
    if let Some(key) = config["tailscale_auth_key"]
        .as_str()
        .filter(|key| !key.is_empty())
    {
        vars["devbox_tailscale_auth_key"] = json!(key);
    }
    vars
}

/// `ansible-playbook` of the source's own environment when it has one.
pub fn playbook(source: &std::path::Path) -> Command {
    let local = source.join(".venv/bin/ansible-playbook");
    Command::new(if local.is_file() {
        local
    } else {
        "ansible-playbook".into()
    })
}

/// The Ansible run that applies `host`'s profile to it over SSH. With a
/// password it becomes root with it, taken from a private file; without one a
/// host that asks for a password is asked for on the terminal.
pub fn prepare(paths: &Paths, host: &Host, become_password: Option<&str>) -> Result<Prepared> {
    let source = paths.source()?;
    let plugins = plugins::load(&[source.join("plugins"), paths.config_dir.join("plugins")]);
    let dirs = plugin_dirs(&plugins);
    let own = paths.own_public_key();
    let mut vars = ansible_vars(&host.config, &dirs, own.as_deref());
    if let Some(password) = become_password {
        vars["ansible_become_password"] = json!(password);
    }
    // Secrets go to private files rather than argv.
    let mut extra = tempfile::NamedTempFile::new()?;
    serde_json::to_writer(&mut extra, &vars)?;
    extra.flush()?;
    let mut inventory = tempfile::NamedTempFile::new()?;
    serde_json::to_writer(&mut inventory, &host.inventory()?)?;
    inventory.flush()?;
    let mut command = playbook(&source);
    command
        .current_dir(&source)
        .args(["ansible/site.yml", "--become", "-e"])
        .arg(format!("@{}", extra.path().display()))
        .arg("-i")
        .arg(inventory.path());
    if host.config["builder"]["ask_pass"]
        .as_bool()
        .unwrap_or(false)
    {
        command.arg("--ask-pass");
    }
    if become_password.is_none() && host.asks_password() {
        command.arg("--ask-become-pass");
    }
    command.env("DEVBOX_BUILD_DIR", paths.build_dir());
    command.env("BERTH_PLUGINS", dirs.to_string());
    if let Some(own) = own {
        command.env("BERTH_SSH_PUBLIC_KEY", own);
    }
    Ok(Prepared {
        command,
        files: vec![extra, inventory],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variables_come_from_the_profile() {
        let config = json!({
            "profile": "cloud", "agent_username": "bot", "agent_privileges": "limited",
            "tailscale_auth_key": "tskey", "ansible_vars": {"devbox_geometry": "1x1"},
            "operator_ssh_public_key": "ssh-ed25519 ME",
        });
        let vars = ansible_vars(
            &config,
            &json!({"demo": "/p/demo"}),
            Some("ssh-ed25519 OWN"),
        );
        assert_eq!(
            vars["devbox_operator_ssh_public_key"],
            "ssh-ed25519 ME\nssh-ed25519 OWN"
        );
        assert_eq!(vars["devbox_agent_ssh_public_key"], "");
        assert_eq!(vars["devbox_user"], "bot");
        assert_eq!(vars["devbox_agent_privileges"], "limited");
        assert_eq!(vars["devbox_profile"], "cloud");
        assert_eq!(vars["devbox_geometry"], "1x1");
        assert_eq!(vars["devbox_plugin_dirs"]["demo"], "/p/demo");
        assert_eq!(vars["devbox_tailscale_auth_key"], "tskey");
        assert!(
            ansible_vars(&json!({}), &json!({}), None)
                .get("devbox_tailscale_auth_key")
                .is_none()
        );
    }

    #[test]
    fn a_password_travels_in_the_private_file() {
        let home = tempfile::tempdir().unwrap();
        let source = home.path().join("source");
        std::fs::create_dir_all(source.join("bin")).unwrap();
        std::fs::write(source.join("bin/devbox.py"), "").unwrap();
        let paths = Paths {
            config_dir: home.path().join("config"),
            data_dir: home.path().join("data"),
        };
        paths.prepare().unwrap();
        paths.set("source", source.to_str().unwrap()).unwrap();
        let host = Host {
            name: "box".into(),
            path: "box.json".into(),
            key: None,
            config: json!({"profile": "cloud", "builder": {"ssh_host": "box", "ssh_user": "ops", "ask_become_pass": true}}),
        };
        let prepared = prepare(&paths, &host, Some("hunter2")).unwrap();
        let args: Vec<String> = prepared
            .command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(
            !args
                .iter()
                .any(|arg| arg.contains("hunter2") || arg == "--ask-become-pass")
        );
        assert_eq!(&args[..2], ["ansible/site.yml", "--become"]);
        let extra: Value =
            serde_json::from_slice(&std::fs::read(prepared.files[0].path()).unwrap()).unwrap();
        assert_eq!(extra["ansible_become_password"], "hunter2");
        let inventory: Value =
            serde_json::from_slice(&std::fs::read(prepared.files[1].path()).unwrap()).unwrap();
        assert_eq!(
            inventory["all"]["hosts"]["devbox_target"]["ansible_user"],
            "ops"
        );
        // Without one, Ansible asks.
        let prepared = prepare(&paths, &host, None).unwrap();
        assert!(
            prepared
                .command
                .get_args()
                .any(|arg| arg == "--ask-become-pass")
        );
    }
}
