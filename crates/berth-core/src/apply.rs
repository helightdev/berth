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
/// host that asks for a password is asked for on the terminal. Only a
/// workstation is applied: every way to apply comes through here.
pub fn prepare(paths: &Paths, host: &Host, become_password: Option<&str>) -> Result<Prepared> {
    crate::level::guard(host, crate::level::Capability::Apply)?;
    let source = paths.source()?;
    let plugins = plugins::load(&[source.join("plugins"), paths.config_dir.join("plugins")]);
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
    let mut prepared = run(paths, host, layout::PLAYBOOK, vars, become_password)?;
    prepared.command.env("BERTH_PLUGINS", dirs.to_string());
    prepared.command.env("BERTH_SSH_PUBLIC_KEY", own.join("\n"));
    Ok(prepared)
}

/// The Ansible run that puts berth's controller alone on a host below a
/// workstation: berthd and box, what they read, and the record of what was
/// installed. The SSH user is the account sessions run as.
pub fn prepare_controller(
    paths: &Paths,
    host: &Host,
    become_password: Option<&str>,
) -> Result<Prepared> {
    crate::level::guard(host, crate::level::Capability::InstallController)?;
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
        berth_api::profile::Auth::Password if host.served(paths).is_none() => {
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
    serde_json::to_writer(&mut extra, &vars)?;
    extra.flush()?;
    let mut inventory = tempfile::NamedTempFile::new()?;
    serde_json::to_writer(&mut inventory, &login.inventory()?)?;
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
    command.env("DEVBOX_BUILD_DIR", paths.build_dir());
    Ok(Prepared {
        command,
        files: vec![extra, inventory],
    })
}

/// After a successful apply of a workstation that berth reaches as its
/// operator: the role gave the operator sudo without a password, SSH with
/// keys only and berth's keys, so the saved profile says so now. A password
/// kept for the first apply is not needed again and is removed. A host of
/// a berth server is settled by the server, which applies it.
pub fn settle(paths: &Paths, host: &Host) -> Result<()> {
    if host.served(paths).is_some() {
        return Ok(());
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
    secrets.set_secret(&crate::keys::sudo_secret(&host.place), None)?;
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

/// How long a controller just installed has to answer.
const CONTROLLER_PATIENCE: std::time::Duration = std::time::Duration::from_secs(60);

/// Install berth's controller alone on a host from where berth runs, its
/// output to `out`, and wait for it to answer: whether all went well. A
/// reachable host becomes a controller in its profile once the controller
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
    let intended = crate::level::of(&host.config);
    let asked = crate::keys::refusal::now();
    let observed = crate::level::observe(paths, host, become_password);
    // A host that refused the login just now would refuse Ansible's too.
    if observed.is_none()
        && let Ok(login) = crate::keys::login_of(paths, host)
        && let Some(refused) = login.refusal()
        && refused.at >= asked
    {
        return fail(out, &refused.sentence(&host.name, refused.at));
    }
    if let Some(why) = crate::level::refuse_install(&host.name, intended, observed) {
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
    if crate::level::of(&host.config) != crate::level::Level::Controller {
        let mut config = host.config.clone();
        berth_api::profile::set_level(&mut config, crate::level::Level::Controller);
        if let Err(error) = host.place.save(paths, &config) {
            return fail(
                out,
                &format!("The controller is installed, but the profile was not saved: {error}"),
            );
        }
        let _ = writeln!(out, "✓ {} is a controller host now", host.name);
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
            tokio::time::timeout(
                std::time::Duration::from_secs(15),
                link.get::<berth_api::Version>("/v1/version"),
            )
            .await
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
            "profile": "cloud", "agent_username": "bot", "agent_privileges": "limited",
            "tailscale_auth_key": "tskey", "ansible_vars": {"devbox_geometry": "1x1"},
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
        assert_eq!(vars["devbox_tailscale_auth_key"], "tskey");
        // A pasted key without a login server goes to Tailscale, as always.
        assert!(vars.get("devbox_tailscale_login_server").is_none());
        // A profile of the qemu kind there once was is applied as cloud, reached from the QEMU host.
        let qemu = ansible_vars(&json!({"profile": "qemu"}), &json!({}), &[]);
        assert_eq!(qemu["devbox_profile"], "cloud");
        assert_eq!(qemu["devbox_bootstrap_ssh_cidr"], "10.0.2.2/32");
        let headscale = json!({"tailscale_login_server": "https://hs.example.org"});
        assert_eq!(
            ansible_vars(&headscale, &json!({}), &[])["devbox_tailscale_login_server"],
            "https://hs.example.org"
        );
        assert!(
            ansible_vars(&json!({}), &json!({}), &[])
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
        paths.give_own_key();
        paths.set("source", source.to_str().unwrap()).unwrap();
        let host = Host {
            name: "box".into(),
            place: Place::own("box").unwrap(),
            config: json!({"profile": "cloud", "access": {"address": "box", "user": "ops", "sudo": "password"}}),
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
        // The key of this berth goes to the operator, apart from the profile's,
        // and the host keeps its profile without how this berth reaches it.
        assert_eq!(
            extra["devbox_berth_ssh_public_key"],
            paths.own_public_key().unwrap()
        );
        assert_eq!(extra["devbox_berth_profile"]["host"], "box");
        assert_eq!(extra["devbox_berth_profile"]["profile"]["profile"], "cloud");
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
        let place = Place::own("studio").unwrap();
        let host = |config: Value| {
            place.save(&paths, &config).unwrap();
            Host {
                name: "studio".into(),
                place: place.clone(),
                config,
            }
        };
        let saved = || place.load(&paths).unwrap().unwrap();
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
}
