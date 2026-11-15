//! Applying a host's configuration: the Ansible run of the installer role
//! against it over SSH.

use crate::{
    Result,
    host::Host,
    paths::{Paths, layout},
    plugins,
    profiles::Place,
};
use serde_json::{Value, json};
use std::{io::Write, process::Command};

/// A command ready to run, and the private files it reads. The files live as
/// long as this does, so keep it until the command has ended.
pub struct Prepared {
    pub command: Command,
    pub files: Vec<tempfile::NamedTempFile>,
    /// Lines for the output before the command's own, such as what of the
    /// profile was left out.
    pub notes: Vec<String>,
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
/// `own` are the public keys of the berth that applies them, as
/// [`crate::keys::operator_keys`] gives them. The role keeps those apart from
/// the profile's, so that no later apply takes them away.
pub fn ansible_vars(config: &Value, plugin_dirs: &Value, own: &[String]) -> Value {
    let mut config = config.clone();
    berth_api::profile::retire_qemu(&mut config);
    let config = &config;
    let mut vars = config["ansible_vars"].clone();
    if !vars.is_object() {
        vars = json!({});
    }
    // How the host joins the tailnet is berth's to say, not the profile's:
    // the key command runs on this machine.
    if let Some(vars) = vars.as_object_mut() {
        for berths in crate::tailnet::ROLE_VARS {
            vars.remove(*berths);
        }
        // Ansible's own variables would say how and where commands run, which
        // berth says from `access`; a profile saved before this was checked
        // may still hold them.
        vars.retain(|name, _| berth_api::profile::role_variable(name));
        if let Some(Value::Array(features)) = vars.get_mut("devbox_plugin_features") {
            features.retain(|feature| {
                feature
                    .as_str()
                    .is_some_and(berth_api::profile::feature_name)
            });
        }
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
    let own: Vec<&str> = own
        .iter()
        .map(|key| key.trim())
        .filter(|key| !key.is_empty())
        .collect();
    vars["devbox_berth_ssh_public_key"] = json!(own.join("\n"));
    // The role names the target by its older name.
    vars["devbox_profile"] = config["target"].clone();
    vars["devbox_plugin_dirs"] = plugin_dirs.clone();
    // A node takes the role's path without the desktop, and only the features its profile names.
    if berth_api::profile::host_type(config) == berth_api::HostType::Node {
        vars["devbox_level"] = json!("node");
        vars["devbox_profile"] = json!(berth_api::profile::node_target(config));
        if !vars["devbox_plugin_features"].is_array() {
            vars["devbox_plugin_features"] = json!([]);
        }
        for account in ["devbox_user", "devbox_operator_user"] {
            if let Some(vars) = vars.as_object_mut() {
                vars.remove(account);
            }
        }
        if let Some(user) = config["access"]["user"]
            .as_str()
            .filter(|user| !user.is_empty())
        {
            vars["devbox_node_user"] = json!(user);
        }
    }
    vars
}

/// Add what joining the tailnet needs from this machine: berth's login
/// server, and the way to a key or why there is none, which
/// names the host as `berth tailnet-key` finds it again: by its name, or by
/// the path of its file. That command runs as a child of the apply, with its
/// environment, so it looks in the same store.
pub fn add_tailnet(vars: &mut Value, paths: &Paths, config: &Value, place: &Place) -> Result<()> {
    let berth = std::env::current_exe()?;
    if let (Some(vars), Value::Object(more)) = (
        vars.as_object_mut(),
        crate::tailnet::apply_vars(paths, config, &place.argument(), &berth),
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
/// host that asks for a password is asked for on the terminal. Only a node
/// or a workstation is applied: every way to apply comes through here.
pub fn prepare(paths: &Paths, host: &Host, become_password: Option<&str>) -> Result<Prepared> {
    crate::host_type::guard(host, crate::host_type::Capability::Apply)?;
    let source = paths.source()?;
    let plugins = plugins::load(&[source.join("plugins"), paths.config_dir.join("plugins")]);
    crate::node::check_features(&plugins, &host.name, &host.config)?;
    crate::node::check_removal(paths, host)?;
    let dirs = plugin_dirs(&plugins);
    // Without its own key in the operator's authorized_keys, berth could be
    // locked out of the host it sets up, so it does not go on without one.
    let own = crate::keys::operator_keys(paths, &host.config)?;
    let mut vars = ansible_vars(&host.config, &dirs, &own);
    add_tailnet(&mut vars, paths, &host.config, &host.place)?;
    // The role keeps this on the host, for the plugins the host has, so
    // berth can tell later whether the host is behind.
    vars["devbox_berth_applied"] = json!({
        "berth": berth_api::VERSION,
        "plugins": plugins::records(&plugins),
    });
    // The role keeps the profile on the host too, for a berth that lacks it.
    vars["devbox_berth_profile"] = crate::kept_profile::for_host(paths, host);
    // Only the certificates go to the host; their keys stay in berth's store.
    let store = crate::registry::Store::own(crate::store::shared(paths)?);
    vars["devbox_client_certificates"] =
        crate::node::client_certificates(&plugins, &store, &host.config)?;
    let mut prepared = run(paths, host, layout::PLAYBOOK, vars, become_password)?;
    let left = berth_api::profile::left_out(&host.config);
    if !left.is_empty() {
        prepared.notes.push(format!(
            "! {}'s ansible_vars hold {}, which the role does not take: it is applied without {}. Remove {} from the profile",
            host.name,
            left.join(", "),
            if left.len() == 1 { "it" } else { "them" },
            if left.len() == 1 { "it" } else { "them" },
        ));
    }
    prepared.command.env("BERTH_PLUGINS", dirs.to_string());
    prepared.command.env("BERTH_SSH_PUBLIC_KEY", own.join("\n"));
    Ok(prepared)
}

/// The Ansible run that puts berth's controller alone on a host that is not
/// a workstation: berthd and box, what they read, and the record of what
/// was installed. The SSH user is the account sessions run as.
pub fn prepare_controller(
    paths: &Paths,
    host: &Host,
    become_password: Option<&str>,
) -> Result<Prepared> {
    crate::host_type::guard(host, crate::host_type::Capability::InstallController)?;
    // The role calls the type managed by its older name.
    let mut vars = json!({
        "devbox_level": "controller",
        "devbox_berth_applied": {"berth": berth_api::VERSION, "plugins": []},
    });
    let user = host.access().user;
    if !user.is_empty() {
        vars["devbox_controller_user"] = json!(user);
    }
    run(
        paths,
        host,
        layout::CONTROLLER_PLAYBOOK,
        vars,
        become_password,
    )
}

/// The run of `playbook` against `host` with these variables, the passwords
/// it needs in them.
fn run(
    paths: &Paths,
    host: &Host,
    playbook_path: &str,
    mut vars: Value,
    become_password: Option<&str>,
) -> Result<Prepared> {
    let source = paths.source()?;
    if let Some(password) = become_password {
        vars["ansible_become_password"] = json!(password);
    }
    // A host logged in to with a password: Ansible hands it to ssh through
    // an askpass helper of its own, and takes it from this private file.
    let login = crate::keys::login_of(paths, host)?;
    let ssh_password = match login.auth {
        berth_api::profile::Auth::Password => {
            crate::registry::Store::own(crate::store::shared(paths)?)
                .secret(&crate::keys::password_secret(&host.place))?
        }
        _ => None,
    };
    if let Some(password) = &ssh_password {
        vars["ansible_password"] = json!(password);
    }
    // Secrets go to private files rather than argv.
    let mut extra = tempfile::NamedTempFile::new()?;
    extra.write_all(untemplated(&vars).as_bytes())?;
    extra.flush()?;
    let mut inventory = tempfile::NamedTempFile::new()?;
    inventory.write_all(untemplated(&login.inventory()?).as_bytes())?;
    inventory.flush()?;
    let mut command = playbook(&source);
    command
        .current_dir(&source)
        .args([playbook_path, "--become", "-e"])
        .arg(format!("@{}", extra.path().display()))
        .arg("-i")
        .arg(inventory.path());
    if login.auth == berth_api::profile::Auth::Password && ssh_password.is_none() {
        command.arg("--ask-pass");
    }
    if become_password.is_none() && host.asks_password() {
        command.arg("--ask-become-pass");
    }
    // A password is Ansible's to hand to ssh, through its own helper; no
    // other program, such as the desktop's dialog, is ever asked for one.
    command.env("SSH_ASKPASS_REQUIRE", "never");
    // A job's output is a log, not a terminal; its colours are kept for
    // whoever shows it on one, and taken out for whoever does not.
    command.env("ANSIBLE_FORCE_COLOR", "1");
    command.env("DEVBOX_BUILD_DIR", paths.build_dir());
    Ok(Prepared {
        command,
        files: vec![extra, inventory],
        notes: vec![],
    })
}

/// `value` as YAML that Ansible reads with every text marked `!unsafe`, so
/// that none of it is a template: Ansible renders `{{ … }}` in variables
/// and inventories, and a lookup in one would run on this machine.
pub fn untemplated(value: &Value) -> String {
    let mut out = String::new();
    write_untemplated(value, &mut out);
    out
}

fn write_untemplated(value: &Value, out: &mut String) {
    match value {
        // Ansible reads an empty text marked unsafe as null, and an empty one is no template.
        Value::String(text) if text.is_empty() => out.push_str("\"\""),
        Value::String(text) => {
            out.push_str("!unsafe ");
            quoted(text, out);
        }
        // YAML 1.1 reads a number such as 1e5 as text unless it is told otherwise.
        Value::Number(number) if number.is_f64() => {
            out.push_str("!!float ");
            out.push_str(&number.to_string());
        }
        Value::Number(_) | Value::Bool(_) | Value::Null => out.push_str(&value.to_string()),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_untemplated(item, out);
            }
            out.push(']');
        }
        Value::Object(fields) => {
            out.push('{');
            for (index, (key, item)) in fields.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                quoted(key, out);
                out.push_str(": ");
                write_untemplated(item, out);
            }
            out.push('}');
        }
    }
}

/// A text in YAML's double quotes, with everything but printable ASCII
/// escaped, as YAML refuses some characters even in quotes.
fn quoted(text: &str, out: &mut String) {
    use std::fmt::Write as _;
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            ' '..='~' => out.push(ch),
            ch if (ch as u32) <= 0xffff => {
                let _ = write!(out, "\\u{:04x}", ch as u32);
            }
            ch => {
                let _ = write!(out, "\\U{:08x}", ch as u32);
            }
        }
    }
    out.push('"');
}

/// After a successful apply of a workstation that berth reaches as its
/// operator: the role gave the operator sudo without a password, SSH with
/// keys only and berth's keys, so the saved profile says so now. A password
/// kept for the first apply is not needed again and is removed.
pub fn settle(paths: &Paths, host: &Host) -> Result<()> {
    settle_with(paths, host, &mut std::io::sink())
}

/// [`settle`], saying on `out` what it did for a node: the provisioners its
/// features offer are registered, and those they no longer offer forgotten.
/// A node that does not say is named on `out`, and its profile is settled
/// all the same.
pub fn settle_with(paths: &Paths, host: &Host, out: &mut dyn Write) -> Result<()> {
    match settle_node(paths, host) {
        Ok(lines) => {
            for line in lines {
                let _ = writeln!(out, "{line}");
            }
        }
        Err(error) => {
            let _ = writeln!(out, "! {error}");
        }
    }
    for line in settle_tailnet(paths, host) {
        let _ = writeln!(out, "{line}");
    }
    let Some(mut profile) = host.place.load(paths)? else {
        return Ok(());
    };
    let before = berth_api::profile::Access::of(&profile);
    if !berth_api::profile::settle_after_apply(&mut profile) {
        return Ok(());
    }
    host.place.save(paths, &profile)?;
    let store = crate::store::shared(paths)?;
    let secrets = crate::registry::Store::own(store.clone());
    // A node's sudo is as it was; only a workstation's operator was given one that never asks.
    if crate::host_type::of(&profile) != crate::host_type::HostType::Node {
        secrets.set_secret(&crate::keys::sudo_secret(&host.place), None)?;
    }
    if before.auth == berth_api::profile::Auth::Password {
        secrets.set_secret(&crate::keys::password_secret(&host.place), None)?;
    }
    // The role checked that the operator takes the key: a refusal of it
    // remembered from before the apply no longer holds.
    if let Ok(login) = crate::keys::login(paths, &store, &host.place, &profile) {
        login.noted(None);
    }
    Ok(())
}

/// After a successful apply of a host that joins the tailnet: record its
/// device, and rename it when a rename asked for that. Only with the API
/// key, as berth does nothing with the device without it; a host applied
/// before the key was set is looked up by its address or hostname instead.
fn settle_tailnet(paths: &Paths, host: &Host) -> Vec<String> {
    if !berth_api::profile::joins_tailnet(&host.config)
        || crate::tailnet::Control::configured(paths)
            .device_problem()
            .is_some()
    {
        return vec![];
    }
    let reported = match crate::node::applied(paths, host) {
        Ok(applied) => applied.and_then(|applied| applied.tailnet),
        Err(error) => {
            return vec![format!(
                "! {} did not say which device it is on the tailnet, so berth has not recorded it: {error}",
                host.name
            )];
        }
    };
    crate::tailnet::after_apply(paths, host, reported).unwrap_or_else(|error| {
        vec![format!(
            "! berth did not record {}'s device on the tailnet: {error}",
            host.name
        )]
    })
}

/// After a successful apply of a node: register the provisioners its
/// features offer, and forget those they no longer do. Says what it did, a
/// line each.
pub fn settle_node(paths: &Paths, host: &Host) -> Result<Vec<String>> {
    if crate::host_type::of(&host.config) != crate::host_type::HostType::Node {
        return Ok(vec![]);
    }
    let offers = crate::node::reported(paths, host)
        .map_err(|error| format!("{} did not say what its features offer: {error}", host.name))?;
    let store = crate::registry::Store::own(crate::store::shared(paths)?);
    crate::node::register(paths, &store, host, &offers)
}

/// How long a controller just installed has to answer.
const CONTROLLER_PATIENCE: std::time::Duration = std::time::Duration::from_secs(60);

/// Install berth's controller alone on a host from where berth runs, its
/// output to `out`, and wait for it to answer: whether all went well. A
/// host of the type ssh becomes managed in its profile once the controller
/// is there.
pub fn install_controller(
    paths: &Paths,
    host: &Host,
    become_password: Option<&str>,
    out: &mut crate::runner::Output,
) -> bool {
    let fail = |out: &mut crate::runner::Output, error: &dyn std::fmt::Display| {
        let _ = writeln!(out, "✗ {error}");
        false
    };
    // The profile here may say less than the host is, as for a workstation
    // added by its address; the role refuses it too.
    let intended = crate::host_type::of(&host.config);
    let asked = crate::keys::refusal::now();
    let observed = crate::host_type::observe(paths, host, become_password);
    // A host that refused the login just now would refuse Ansible's too.
    if observed.is_none()
        && let Ok(login) = crate::keys::login_of(paths, host)
        && let Some(refused) = login.refusal()
        && refused.at >= asked
    {
        return fail(out, &refused.sentence(&host.name, refused.at));
    }
    if let Some(why) = crate::host_type::refuse_install(&host.name, intended, observed) {
        return fail(out, &why);
    }
    let prepared = match prepare_controller(paths, host, become_password) {
        Ok(prepared) => prepared,
        Err(error) => return fail(out, &error),
    };
    let program = crate::runner::Program {
        command: prepared.command,
        input: None,
        files: prepared.files,
    };
    match crate::runner::run_program(program, out) {
        Some(0) => {}
        Some(code) => {
            return fail(
                out,
                &format!("Installing the controller failed (ansible-playbook ended with {code})"),
            );
        }
        None => return fail(out, &"Installing the controller was ended by a signal"),
    }
    if crate::host_type::of(&host.config) != crate::host_type::HostType::Managed {
        let mut config = host.config.clone();
        berth_api::profile::set_host_type(&mut config, crate::host_type::HostType::Managed);
        if let Err(error) = host.place.save(paths, &config) {
            return fail(
                out,
                &format!("The controller is installed, but the profile was not saved: {error}"),
            );
        }
        let _ = writeln!(out, "✓ {} is a managed host now", host.name);
    }
    match answers(paths, host, become_password) {
        Ok(version) => {
            let _ = writeln!(
                out,
                "✓ berth's controller answers on {}: berthd {version}",
                host.name
            );
            true
        }
        Err(error) => fail(
            out,
            &format!(
                "berthd is installed on {}, but it does not answer ({error}). On a host without systemd something there has to start /usr/local/bin/berthd",
                host.name
            ),
        ),
    }
}

/// The version of the host's berthd, asked until it answers or the
/// patience is spent.
fn answers(paths: &Paths, host: &Host, password: Option<&str>) -> Result<String> {
    let link = crate::link::Link::new(host.link(paths, password)?);
    let runtime = crate::link::runtime()?;
    let started = std::time::Instant::now();
    loop {
        let asked = runtime.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(15), link.version()).await
        });
        match asked {
            Ok(Ok(version)) => return Ok(version.version),
            Ok(Err(error)) if started.elapsed() >= CONTROLLER_PATIENCE => {
                return Err(error.to_string().into());
            }
            Err(_) if started.elapsed() >= CONTROLLER_PATIENCE => {
                return Err("no answer in time".into());
            }
            _ => std::thread::sleep(std::time::Duration::from_secs(1)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variables_come_from_the_profile() {
        let config = json!({
            "target": "cloud", "agent_username": "bot", "agent_privileges": "limited",
            "ansible_vars": {"devbox_geometry": "1x1", "devbox_tailscale_key_command": ["sh", "-c", "id"]},
            "operator_ssh_public_key": "ssh-ed25519 ME",
        });
        let vars = ansible_vars(
            &config,
            &json!({"demo": "/p/demo"}),
            &["ssh-ed25519 OWN".to_owned(), "ssh-ed25519 NEXT".to_owned()],
        );
        assert_eq!(vars["devbox_operator_ssh_public_key"], "ssh-ed25519 ME");
        assert_eq!(
            vars["devbox_berth_ssh_public_key"],
            "ssh-ed25519 OWN\nssh-ed25519 NEXT"
        );
        // A profile from before the settings went away cannot turn either off.
        let old = json!({"berth_ssh_key": false, "operator_passwordless_sudo": false});
        let vars_old = ansible_vars(&old, &json!({}), &["ssh-ed25519 OWN".to_owned()]);
        assert_eq!(vars_old["devbox_berth_ssh_public_key"], "ssh-ed25519 OWN");
        assert!(vars_old.get("devbox_operator_passwordless_sudo").is_none());
        assert_eq!(vars["devbox_agent_ssh_public_key"], "");
        assert_eq!(vars["devbox_user"], "bot");
        assert_eq!(vars["devbox_agent_privileges"], "limited");
        assert_eq!(vars["devbox_profile"], "cloud");
        assert_eq!(vars["devbox_geometry"], "1x1");
        assert_eq!(vars["devbox_plugin_dirs"]["demo"], "/p/demo");
        // A profile of the qemu target there once was is applied as cloud, reached from the QEMU host.
        let qemu = ansible_vars(&json!({"target": "qemu"}), &json!({}), &[]);
        assert_eq!(qemu["devbox_profile"], "cloud");
        assert_eq!(qemu["devbox_bootstrap_ssh_cidr"], "10.0.2.2/32");
        // The tailnet's variables are berth's, not the profile's.
        assert!(vars.get("devbox_tailscale_key_command").is_none());
        assert!(vars.get("devbox_tailscale_auth_key").is_none());
        assert!(vars.get("devbox_tailscale_login_server").is_none());
    }

    #[test]
    fn a_node_takes_the_path_without_the_desktop() {
        let node = json!({"type": "node", "access": {"address": "192.0.2.20", "user": "root"},
            "agent_username": "bot", "ansible_vars": {"devbox_user": "x"}});
        let vars = ansible_vars(&node, &json!({}), &["ssh-ed25519 OWN".to_owned()]);
        assert_eq!(vars["devbox_level"], "node");
        assert_eq!(vars["devbox_profile"], "cloud");
        assert_eq!(vars["devbox_node_user"], "root");
        // A node has only the features its profile names, and no accounts of a workstation.
        assert_eq!(vars["devbox_plugin_features"], json!([]));
        assert!(vars.get("devbox_user").is_none() && vars.get("devbox_operator_user").is_none());
        assert_eq!(vars["devbox_berth_ssh_public_key"], "ssh-ed25519 OWN");
        let contained = json!({"type": "node", "target": "container",
            "ansible_vars": {"devbox_plugin_features": ["docker/engine"]}});
        let vars = ansible_vars(&contained, &json!({}), &[]);
        assert_eq!(vars["devbox_profile"], "container");
        assert_eq!(vars["devbox_plugin_features"], json!(["docker/engine"]));
        assert!(
            ansible_vars(&json!({"builder": {}}), &json!({}), &[])
                .get("devbox_level")
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
        paths.give_own_key();
        paths.set("source", source.to_str().unwrap()).unwrap();
        let host = Host {
            name: "box".into(),
            place: Place::own(&paths, "box").unwrap(),
            config: json!({"target": "cloud", "access": {"address": "box", "user": "ops", "sudo": "password"}}),
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
        let extra = read_back(prepared.files[0].path());
        assert_eq!(extra["ansible_become_password"], "hunter2");
        let inventory = read_back(prepared.files[1].path());
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
        // The key of this berth goes to the operator, apart from the profile's,
        // and the host keeps its profile without how this berth reaches it.
        assert_eq!(
            extra["devbox_berth_ssh_public_key"],
            paths.own_public_key().unwrap()
        );
        assert_eq!(extra["devbox_berth_profile"]["host"], "box");
        assert_eq!(extra["devbox_berth_profile"]["profile"]["target"], "cloud");
        assert!(
            extra["devbox_berth_profile"]["profile"]
                .get("access")
                .is_none()
        );
    }

    #[test]
    fn an_apply_as_the_operator_settles_the_profile_and_forgets_the_passwords() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: home.path().join("config"),
            data_dir: home.path().join("data"),
        };
        paths.prepare().unwrap();
        paths.give_own_key();
        let place = Place::own(&paths, "studio").unwrap();
        let host = |config: Value| {
            place.save(&paths, &config).unwrap();
            Host {
                name: "studio".into(),
                place: place.clone(),
                config,
            }
        };
        let saved = || place.load(&paths).unwrap().unwrap();
        place.save(&paths, &json!({})).unwrap();
        let secrets = crate::registry::Store::own(crate::store::shared(&paths).unwrap());
        secrets.set_secret("sudo/studio", Some("hunter2")).unwrap();
        secrets.set_secret("ssh/studio", Some("hunter2")).unwrap();
        let first = json!({"operator_username": "ops", "builder": {},
            "access": {"address": "192.0.2.10", "user": "ops", "auth": "password", "sudo": "password"}});
        settle(&paths, &host(first)).unwrap();
        let access = berth_api::profile::Access::of(&saved());
        assert_eq!(access.auth, berth_api::profile::Auth::Berth);
        assert_eq!(access.sudo, Some(false));
        assert_eq!(access.address, "192.0.2.10");
        assert_eq!(secrets.secret("sudo/studio").unwrap(), None);
        assert_eq!(secrets.secret("ssh/studio").unwrap(), None);
        // A key of the ring the role installed stays how berth logs in.
        let keyed = json!({"builder": {}, "access": {"address": "192.0.2.10", "user": "ops", "auth": "key:admin"}});
        settle(&paths, &host(keyed)).unwrap();
        assert_eq!(
            berth_api::profile::Access::of(&saved()).auth,
            berth_api::profile::Auth::Key("admin".into())
        );
        // An account of the machine's own keeps a sudo that asks, and its password.
        secrets.set_secret("sudo/studio", Some("hunter2")).unwrap();
        let own = json!({"builder": {}, "access": {"address": "192.0.2.10", "user": "admin", "auth": "password", "sudo": "password"}});
        settle(&paths, &host(own.clone())).unwrap();
        assert_eq!(saved(), own);
        assert_eq!(
            secrets.secret("sudo/studio").unwrap().as_deref(),
            Some("hunter2")
        );
    }

    #[test]
    fn a_profile_says_nothing_of_how_ansible_runs() {
        let config = json!({"builder": {}, "ansible_vars": {
            "devbox_geometry": "1x1",
            "ansible_connection": "local",
            "ansible_ssh_common_args": "-o ProxyCommand=\"sh -c id\"",
            "ansible_python_interpreter": "/tmp/x",
            "devbox_plugin_features": ["claude/cli", "../../tmp/x", 7]}});
        let vars = ansible_vars(&config, &json!({}), &[]);
        assert_eq!(vars["devbox_geometry"], "1x1");
        for gone in [
            "ansible_connection",
            "ansible_ssh_common_args",
            "ansible_python_interpreter",
        ] {
            assert!(vars.get(gone).is_none(), "{gone}");
        }
        assert_eq!(vars["devbox_plugin_features"], json!(["claude/cli"]));
        assert_eq!(
            berth_api::profile::left_out(&config),
            [
                "ansible_connection",
                "ansible_python_interpreter",
                "ansible_ssh_common_args",
                "the feature \"../../tmp/x\"",
                "the feature 7"
            ]
        );
    }

    /// A file [`untemplated`] wrote, without its tags, of texts without any.
    fn read_back(path: &std::path::Path) -> Value {
        let text = std::fs::read_to_string(path).unwrap();
        serde_json::from_str(&text.replace("!unsafe ", "").replace("!!float ", "")).unwrap()
    }

    #[test]
    fn what_goes_to_ansible_is_never_a_template() {
        let value = json!({"devbox_x": "{{ lookup('pipe', 'id') }}", "n": 3, "f": 1e20, "b": true,
            "z": null, "e": "", "l": ["a\"b\\c", "é\u{7f}\u{1F600}"], "d": {"{{ k }}": "v"}});
        assert_eq!(
            untemplated(&value),
            r#"{"b": true, "d": {"{{ k }}": !unsafe "v"}, "devbox_x": !unsafe "{{ lookup('pipe', 'id') }}", "e": "", "f": !!float 1e+20, "l": [!unsafe "a\"b\\c", !unsafe "\u00e9\u007f\U0001f600"], "n": 3, "z": null}"#
        );
        if let Ok(dir) = std::env::var("BERTH_UNTEMPLATED_TO") {
            std::fs::write(format!("{dir}/vars.yml"), untemplated(&value)).unwrap();
            let inventory = json!({"all": {"hosts": {"devbox_target": {
                "ansible_host": "{{ lookup('pipe', 'echo RAN') }}", "ansible_connection": "local",
                "ansible_ssh_common_args": "-o ControlMaster=auto"}}}});
            std::fs::write(format!("{dir}/inventory"), untemplated(&inventory)).unwrap();
        }
    }
}
