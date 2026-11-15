//! What berth does on the machine its installation is on, which is Linux:
//! `berth serve`, the server the screens use when no other is set, and the
//! commands that work with the store, the hosts and the provisioning source.

pub mod commands;
mod key_command;
mod profile;
mod provisioner_command;
mod resource_command;
pub mod serve;
mod store_command;

use crate::Result;
use berth_core::{paths, plugins, registry};
use paths::Paths;
use serde_json::json;
use std::process::Command;

/// The server of this machine for the screens: a `berth serve` of this
/// user, or one in this process, whose jobs `finish` waits for.
pub fn server() -> Result<Option<crate::Local>> {
    let (server, here) = serve::here::local(&Paths::locate()?)?;
    if let Some(here) = &here {
        berth_page::net::serve_here(here.dial());
    }
    Ok(Some((server, Box::new(move || serve::here::finish(here)))))
}

/// ssh runs berth as its askpass helper, the program of SSH_ASKPASS, with
/// the prompt as its one argument and the variable that says where the
/// password is; the answer is all that may go to standard output. Whether
/// this run was one.
pub fn askpass(args: &[&str]) -> Result<bool> {
    let Ok(source) = std::env::var(berth_core::keys::askpass::SOURCE) else {
        return Ok(false);
    };
    let prompt = match args {
        ["askpass", prompt @ ..] => prompt.join(" "),
        [prompt] => (*prompt).to_owned(),
        _ => return Ok(false),
    };
    let answer = berth_core::keys::askpass::answer(&prompt, &source, None)?;
    println!("{answer}");
    Ok(true)
}

fn show_config(paths: &Paths) -> Result<()> {
    // The settings of the file are shown when the store does not open.
    let (settings, unopened) = match paths.settings() {
        Ok(settings) => (settings, None),
        Err(error) => (paths.local_settings()?, Some(error)),
    };
    println!(
        "Settings   {}, and the store",
        paths.settings_file().display()
    );
    let store = berth_core::store::Installation::of(paths).target.describe();
    match &unopened {
        None => println!("Store      {store}: settings, hosts, registries and secrets"),
        Some(error) => println!("Store      {store}, which does not open: {error}"),
    }
    match berth_core::server::Server::configured(paths) {
        Some(server) => println!(
            "Hosts      on the berth server at {}, with a copy in the store",
            server.url
        ),
        None => println!("Hosts      in the store, with their passwords and machines"),
    }
    println!("Builds     {}", paths.build_dir().display());
    match paths.source() {
        Ok(source) => println!("Source     {}", source.display()),
        Err(error) => println!("Source     {error}"),
    }
    println!();
    for (key, help) in paths::SETTINGS {
        // What lets someone in is not for a screen.
        let value = match settings.get(*key).and_then(|value| value.as_str()) {
            Some(_) if paths::is_secret(key) => Some("(set)"),
            value => value,
        };
        let value = match (&unopened, paths::is_local(key)) {
            (Some(_), false) => Some("(in the store, which does not open)"),
            _ => value,
        };
        println!("{key} = {}\n    {help}", value.unwrap_or("(not set)"));
    }
    Ok(())
}

/// `berth config KEY VALUE`: the tailnet settings of a berth server this
/// berth uses are the server's, and change there.
fn change_setting(paths: &Paths, key: &str, value: &str) -> Result<()> {
    match berth_core::server::Server::configured(paths) {
        Some(server) if berth_core::tailnet::SETTINGS.contains(&key) => {
            server.call(
                "settings.set",
                json!({"key": key}),
                Some(json!({"value": value})),
            )?;
            println!("Changed {key} on the berth server at {}", server.url);
            Ok(())
        }
        _ => paths.set(key, value),
    }
}

/// Whether a command reads this machine's own plugins: the commands that
/// apply, build or provision with them or list them.
fn loads_plugins(args: &[&str]) -> bool {
    const VERBS: &[&str] = &[
        "apply",
        "plugins",
        "provisioners",
        "provisioner",
        "provision",
        "instance",
        "resource",
        "deploy",
        "seed",
        "iso",
        "publish",
        "host",
    ];
    args.first().is_some_and(|verb| VERBS.contains(verb))
}

/// Put back the plugins berth's store has and this machine's plugin
/// directory lacks, as on a machine restored from the store, and say so on
/// standard error.
pub(crate) fn restore_plugins(paths: &Paths) {
    match berth_core::plugin_state::restore(paths) {
        Ok(notices) => notices.iter().for_each(|notice| eprintln!("{notice}")),
        Err(error) => eprintln!("berth: the plugins in berth's store were not checked: {error}"),
    }
}

fn succeed(mut command: Command) -> Result<()> {
    let status = command.status()?;
    if !status.success() {
        let program = command.get_program().to_string_lossy().into_owned();
        return Err(format!("{program} failed ({status})").into());
    }
    Ok(())
}

/// `berth host HOST ...`: the secrets and registries a host is given.
fn host_command(paths: &Paths, host: &str, args: &[&str]) -> Result<()> {
    let (place, mut config) = profile::load(paths, host)?;
    let list = |value: &serde_json::Value| value.as_array().cloned().unwrap_or_default();
    match *args {
        [] => {
            for secret in list(&config["secrets"]) {
                let place = match (secret["env"].as_str(), secret["file"].as_str()) {
                    (Some(variable), _) => format!("env {variable}"),
                    (_, Some(path)) => format!("file {path}"),
                    _ => String::new(),
                };
                println!(
                    "secret    {}  {place}",
                    secret["name"].as_str().unwrap_or("")
                );
            }
            for name in list(&config["registries"]) {
                println!("registry  {}", name.as_str().unwrap_or(""));
            }
            println!("Send them with: berth run {host} secrets");
            return Ok(());
        }
        ["secret", name, ref how @ ..] => {
            let mut secrets = list(&config["secrets"]);
            secrets.retain(|known| known["name"] != name);
            match *how {
                ["env", variable] => secrets.push(json!({"name": name, "env": variable})),
                ["file", path] => secrets.push(json!({"name": name, "file": path})),
                ["file", path, mode] => {
                    secrets.push(json!({"name": name, "file": path, "mode": mode}))
                }
                ["off"] => {}
                _ => {
                    return Err(
                        "Usage: berth host HOST secret NAME env VARIABLE | file PATH [MODE] | off"
                            .into(),
                    );
                }
            }
            config["secrets"] = json!(secrets);
        }
        ["registry", name, switch @ ("on" | "off")] => {
            let mut registries = list(&config["registries"]);
            registries.retain(|known| known != name);
            if switch == "on" {
                if !registry::Store::new(paths)?
                    .registries()?
                    .contains_key(name)
                {
                    return Err(format!("No registry {name}. See: berth registry").into());
                }
                registries.push(json!(name));
            }
            config["registries"] = json!(registries);
        }
        _ => return Err("Usage: berth host HOST [secret NAME ... | registry NAME on|off]".into()),
    }
    profile::save(paths, &place, config)?;
    Ok(())
}

/// Run a command of berth that needs this machine's installation.
pub fn command(args: &[&str]) -> Result<()> {
    let paths = Paths::locate()?;
    if loads_plugins(args) && berth_core::server::Server::configured(&paths).is_none() {
        restore_plugins(&paths);
    }
    // With a server set, the hosts are the server's: fetch them before anything looks.
    let local = [
        "config",
        "serve",
        "--version",
        "-V",
        "version",
        "openapi",
        "asyncapi",
        "operations",
        "update",
        "key",
        "askpass",
        "tailnet-key",
        "credentials",
        "connect",
        "setup-key",
        "store",
    ];
    if let Some(server) = berth_core::server::Server::configured(&paths)
        && !args.first().is_some_and(|verb| local.contains(verb))
    {
        server
            .mirror(&paths)
            .map_err(|error| format!("the berth server at {}: {error}", server.url))?;
    }
    match args[..] {
        ["list"] => {
            for (name, address) in paths.hosts() {
                println!("{name:<24} {address}");
            }
            Ok(())
        }
        ["registry", ref rest @ ..] => registry::registry_command(&paths, rest),
        ["secret", ref rest @ ..] => registry::secret_command(&paths, rest),
        ["image", "push", destination, ref rest @ ..] => {
            let image =
                registry::push_image(&paths, destination, rest.first().unwrap_or(&"latest"))?;
            println!("Pushed {image}");
            Ok(())
        }
        ["publish", destination, ref rest @ ..] => {
            std::env::set_current_dir(paths.source()?)?;
            let url = registry::publish(&paths, destination)?;
            println!("Published {url}");
            let (host, login) =
                match *rest {
                    [] => (None, None),
                    [host] => (Some(host), None),
                    [host, "--login"] => (Some(host), destination.split('/').next()),
                    [host, "--login", registry] => (Some(host), Some(registry)),
                    _ => return Err(
                        "Usage: berth publish REGISTRY[/REPOSITORY] [HOST [--login [REGISTRY]]]"
                            .into(),
                    ),
                };
            if let Some(host) = host {
                let (place, mut config) = profile::load(&paths, host)?;
                config["release_url"] = json!(url);
                config["release_store"] = json!(destination);
                config["release_private"] = json!(login.is_some());
                match login {
                    Some(name) => {
                        if registry::Store::new(&paths)?.curl_login(name)?.is_none() {
                            return Err(format!("Registry {name} has no stored login").into());
                        }
                        config["release_login"] = json!(name);
                        println!("{host} downloads it from there with the login of {name}");
                    }
                    None => {
                        if let Some(profile) = config.as_object_mut() {
                            profile.remove("release_login");
                        }
                        println!("{host} downloads it from there without a login");
                    }
                }
                profile::save(&paths, &place, config)?;
            }
            Ok(())
        }
        // A host's own subcommands start with secret or registry.
        ["host", "add", name, address, ref rest @ ..]
            if !["secret", "registry"].contains(&name) =>
        {
            commands::add_host(&paths, name, address, rest)
        }
        ["host", "install", host] => commands::install_controller(&paths, host),
        ["host", "type", host, given] => commands::set_type(&paths, host, given),
        ["host", "rename", host, name] => commands::rename(&paths, host, name, false),
        ["host", "rename", host, name, "--hostname"]
        | ["host", "rename", host, "--hostname", name] => {
            commands::rename(&paths, host, name, true)
        }
        ["host", host, ref rest @ ..] => host_command(&paths, host, rest),
        ["serve", ref rest @ ..] => serve::command(&paths, rest),
        ["setup-key", ref rest @ ..] => serve::setup_key::command(&paths, rest),
        ["inbox", ref rest @ ..] => serve::inbox_command(&paths, rest),
        ["kv", ref rest @ ..] => serve::kv_command(&paths, rest),
        ["store", ref rest @ ..] => store_command::command(&paths, rest),
        ["openapi"] => {
            println!("{}", serde_json::to_string_pretty(&serve::openapi())?);
            Ok(())
        }
        ["operations"] => {
            println!("{}", serde_json::to_string_pretty(&serve::operations())?);
            Ok(())
        }
        ["asyncapi"] => {
            println!("{}", serde_json::to_string_pretty(&serve::asyncapi())?);
            Ok(())
        }
        ["plugins"] => {
            let listing = plugins::Manager::new(&paths).list()?;
            print!("{}", plugins::describe(&listing.plugins));
            Ok(())
        }
        ["plugins", "add", spec] => {
            for line in plugins::Manager::new(&paths).add(spec)? {
                println!("{line}");
            }
            Ok(())
        }
        ["plugins", "update", ref rest @ ..] => commands::update_plugins(&paths, rest),
        ["plugins", "remove", name] => {
            let manager = plugins::Manager::new(&paths);
            // A name npm knows that berth does not, such as a broken package.
            let known = manager
                .list()?
                .plugins
                .iter()
                .any(|plugin| plugin.id == *name || plugin.package == *name);
            if !known && manager.server().is_none() {
                let mut npm = Command::new("npm");
                npm.args([
                    "uninstall",
                    "--ignore-scripts",
                    "--no-audit",
                    "--no-fund",
                    "--prefix",
                ])
                .arg(plugins::own_dir(&paths.config_dir))
                .arg(name);
                let done = succeed(npm);
                if let Some(line) = plugins::remembered(&paths) {
                    eprintln!("{line}");
                }
                return done;
            }
            for line in manager.remove(name)? {
                println!("{line}");
            }
            Ok(())
        }
        ["config"] => show_config(&paths),
        ["config", key] => change_setting(&paths, key, ""),
        ["config", key, value] => change_setting(&paths, key, value),
        ["tailnet-key", host] => {
            // An apply runs this on the machine that runs berth, for a host
            // not on the tailnet yet; the key goes to the role on stdout.
            println!("{}", berth_core::tailnet::key_for(&paths, host)?);
            Ok(())
        }
        ["profile", host] => commands::show_profile(&paths, host),
        ["profile", host, "--set", file] => commands::set_profile(&paths, host, file),
        ["credentials", host] => commands::credentials(&paths, host, false),
        ["credentials", host, "--show"] | ["credentials", "--show", host] => {
            commands::credentials(&paths, host, true)
        }
        ["iso", host] => commands::installer(&paths, host, None),
        ["iso", host, "--write", device] => commands::installer(&paths, host, Some(device)),
        ["key", ref rest @ ..] => key_command::command(&paths, rest),
        ["apply", "--behind" | "--all", ..] => commands::apply_hosts(&paths, &args[1..]),
        ["apply", host] => commands::apply(&paths, host),
        ["remove", host] => commands::remove(&paths, host),
        ["adopt", host, ref rest @ ..] => commands::adopt(&paths, host, rest),
        ["provisioners"] => commands::provisioners(&paths),
        ["provisioner", ref rest @ ..] => provisioner_command::command(&paths, rest),
        ["provision", host, provisioner, ref rest @ ..] => {
            resource_command::provision(&paths, host, provisioner, rest)
        }
        ["instance", host, ref rest @ ..] => resource_command::instance(&paths, host, rest),
        ["resource", ref rest @ ..] => resource_command::command(&paths, rest),
        ["deploy", name, ref rest @ ..] => resource_command::deploy(&paths, name, rest),
        ["seed", host] => commands::seed(&paths, host),
        ["actions", host] => commands::actions(&paths, host),
        ["run" | "manage", host, action, ref rest @ ..] => {
            commands::run(&paths, host, action, rest)
        }
        ["exec", host, ref rest @ ..] => commands::exec(&paths, host, rest),
        ["attach", host] => commands::attach(&paths, host, None),
        ["attach", host, id] => commands::attach(&paths, host, Some(id)),
        ["sessions", host] => commands::sessions(&paths, host),
        ["output", host, id, ref rest @ ..] => commands::output(&paths, host, id, rest),
        ["channel", host, ref rest @ ..] => commands::channel(&paths, host, rest),
        ["call", host, service, method] => commands::call(&paths, host, service, method, None),
        ["call", host, service, method, body] => {
            commands::call(&paths, host, service, method, Some(body))
        }
        ["inventory", ref hosts @ ..] => commands::inventory(&paths, hosts),
        ["ssh", host, "--agent"] | ["ssh", "--agent", host] => {
            let status = commands::agent_terminal(&paths, host)?.status()?;
            std::process::exit(status.code().unwrap_or(1));
        }
        ["ssh", host, ref remote @ ..] => {
            let status = commands::terminal(&paths, host, remote)?.status()?;
            std::process::exit(status.code().unwrap_or(1));
        }
        _ => {
            println!("{}", crate::USAGE);
            Ok(())
        }
    }
}
