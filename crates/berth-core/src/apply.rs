//! Applying a host's configuration: the Ansible run of the installer role
//! against it over SSH.

use crate::{
    Result,
    host::Host,
    paths::{Paths, layout},
    plugins,
};
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
    let mut config = config.clone();
    berth_api::profile::retire_qemu(&mut config);
    let config = &config;
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
    ] {
        if let Some(value) = config.get(source) {
            vars[dest] = value.clone();
        }
    }
    // The role writes each account's keys as one block, a key to a line.
    vars["devbox_agent_ssh_public_key"] =
        json!(berth_api::profile::account_keys(config, "agent").join("\n"));
    vars["devbox_operator_ssh_public_key"] =
        json!(berth_api::profile::account_keys(config, "operator").join("\n"));
    // The role keeps this key apart from the profile's, so that no later
    // apply takes it away.
    vars["devbox_berth_ssh_public_key"] = json!(own.map(str::trim).unwrap_or(""));
    vars["devbox_profile"] = config["profile"].clone();
    vars["devbox_plugin_dirs"] = plugin_dirs.clone();
    if let Some(key) = config["tailscale_auth_key"]
        .as_str()
        .filter(|key| !key.is_empty())
    {
        vars["devbox_tailscale_auth_key"] = json!(key);
    }
    if let Some(server) = config["tailscale_login_server"]
        .as_str()
        .filter(|server| !server.is_empty())
    {
        vars["devbox_tailscale_login_server"] = json!(server);
    }
    vars
}

/// Add what joining the tailnet needs from this machine: berth's login
/// server, and for a host that joins automatically the way to a key, which
/// names the host by the path of its `profile`.
pub fn add_tailnet(
    vars: &mut Value,
    paths: &Paths,
    config: &Value,
    profile: &std::path::Path,
) -> Result<()> {
    let berth = std::env::current_exe()?;
    let profile = profile.to_string_lossy();
    if let (Some(vars), Value::Object(more)) = (
        vars.as_object_mut(),
        crate::tailnet::apply_vars(paths, config, &profile, &berth),
    ) {
        vars.extend(more);
    }
    Ok(())
}

/// `ansible-playbook` of the source's own environment when it has one, with
/// the source's Ansible settings.
pub fn playbook(source: &std::path::Path) -> Command {
    let local = source.join(".venv/bin/ansible-playbook");
    let mut command = Command::new(if local.is_file() {
        local
    } else {
        "ansible-playbook".into()
    });
    command.env("ANSIBLE_CONFIG", source.join(layout::ANSIBLE_CONFIG));
    command
}

/// The Ansible run that applies `host`'s profile to it over SSH. With a
/// password it becomes root with it, taken from a private file; without one a
/// host that asks for a password is asked for on the terminal.
pub fn prepare(paths: &Paths, host: &Host, become_password: Option<&str>) -> Result<Prepared> {
    let source = paths.source()?;
    let plugins = plugins::load(&[source.join("plugins"), paths.config_dir.join("plugins")]);
    let dirs = plugin_dirs(&plugins);
    // Without its own key in the operator's authorized_keys, berth could be
    // locked out of the host it sets up, so it does not go on without one.
    let own = paths.own_public_key().ok_or(
        "berth has no SSH key of its own to give the host's operator: check that ssh-keygen works",
    )?;
    let mut vars = ansible_vars(&host.config, &dirs, Some(&own));
    add_tailnet(&mut vars, paths, &host.config, &host.path)?;
    // The role keeps this on the host, for the plugins the host has, so
    // berth can tell later whether the host is behind.
    vars["devbox_berth_applied"] = json!({
        "berth": berth_api::VERSION,
        "plugins": plugins::records(&plugins),
    });
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
        .args([layout::PLAYBOOK, "--become", "-e"])
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
    command.env("BERTH_SSH_PUBLIC_KEY", &own);
    Ok(Prepared {
        command,
        files: vec![extra, inventory],
    })
}

/// After a successful apply: a host that berth reaches as its operator no
/// longer asks for a sudo password, so its saved profile stops saying so.
pub fn settle(host: &Host) -> Result<()> {
    let Ok(saved) = std::fs::read(&host.path) else {
        return Ok(());
    };
    let mut profile: Value = serde_json::from_slice(&saved)?;
    if berth_api::profile::settle_after_apply(&mut profile) {
        crate::seed::save_profile(&host.path, &profile)?;
    }
    Ok(())
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
        assert_eq!(vars["devbox_operator_ssh_public_key"], "ssh-ed25519 ME");
        assert_eq!(vars["devbox_berth_ssh_public_key"], "ssh-ed25519 OWN");
        // A profile from before the setting went away cannot turn either off.
        let old = json!({"berth_ssh_key": false, "operator_passwordless_sudo": false});
        let vars = ansible_vars(&old, &json!({}), Some("ssh-ed25519 OWN"));
        assert_eq!(vars["devbox_berth_ssh_public_key"], "ssh-ed25519 OWN");
        assert!(vars.get("devbox_operator_passwordless_sudo").is_none());
        let vars = ansible_vars(
            &config,
            &json!({"demo": "/p/demo"}),
            Some("ssh-ed25519 OWN"),
        );
        assert_eq!(vars["devbox_agent_ssh_public_key"], "");
        assert_eq!(vars["devbox_user"], "bot");
        assert_eq!(vars["devbox_agent_privileges"], "limited");
        assert_eq!(vars["devbox_profile"], "cloud");
        assert_eq!(vars["devbox_geometry"], "1x1");
        assert_eq!(vars["devbox_plugin_dirs"]["demo"], "/p/demo");
        assert_eq!(vars["devbox_tailscale_auth_key"], "tskey");
        // A pasted key without a login server goes to Tailscale, as always.
        assert!(vars.get("devbox_tailscale_login_server").is_none());
        // A profile of the qemu kind there once was is applied as cloud, reached from the QEMU host.
        let qemu = ansible_vars(&json!({"profile": "qemu"}), &json!({}), None);
        assert_eq!(qemu["devbox_profile"], "cloud");
        assert_eq!(qemu["devbox_bootstrap_ssh_cidr"], "10.0.2.2/32");
        let headscale = json!({"tailscale_login_server": "https://hs.example.org"});
        assert_eq!(
            ansible_vars(&headscale, &json!({}), None)["devbox_tailscale_login_server"],
            "https://hs.example.org"
        );
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
        std::fs::create_dir_all(source.join("provision/bin")).unwrap();
        std::fs::write(source.join(layout::BUILDER), "").unwrap();
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
        assert_eq!(&args[..2], [layout::PLAYBOOK, "--become"]);
        let config = prepared
            .command
            .get_envs()
            .find(|(name, _)| *name == "ANSIBLE_CONFIG")
            .and_then(|(_, value)| value);
        let wanted = paths.source().unwrap().join(layout::ANSIBLE_CONFIG);
        assert_eq!(config, Some(wanted.as_os_str()));
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
        // The key of this berth goes to the operator, apart from the profile's.
        assert_eq!(
            extra["devbox_berth_ssh_public_key"],
            paths.own_public_key().unwrap()
        );
    }

    #[test]
    fn a_first_apply_as_the_operator_stops_the_asking() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("studio.json");
        let host = |config: Value| {
            std::fs::write(&path, config.to_string()).unwrap();
            Host {
                name: "studio".into(),
                path: path.clone(),
                key: None,
                config,
            }
        };
        let saved = || -> Value { serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap() };
        let first = json!({"operator_passwordless_sudo": false,
                           "builder": {"ssh_host": "192.0.2.10", "ssh_user": "ops", "ask_become_pass": true}});
        settle(&host(first)).unwrap();
        assert!(!berth_api::profile::asks_become_pass(&saved()));
        assert_eq!(saved()["builder"]["ssh_host"], "192.0.2.10");
        // An account of the machine's own still asks.
        let own = json!({"builder": {"ssh_host": "192.0.2.10", "ssh_user": "admin", "ask_become_pass": true}});
        settle(&host(own.clone())).unwrap();
        assert_eq!(saved(), own);
    }
}
