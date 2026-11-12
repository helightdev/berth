mod app;
mod commands;
mod fields;
mod keys;
mod picker;
mod remote;
mod serve;
mod session;
mod ui;
mod update;

use app::{App, parse_config};
use berth_core::{credentials, discovery, job, paths, plugins, registry};
pub(crate) use berth_tui::{markdown, theme};
use crossterm::{cursor::SetCursorStyle, event};
use fields::{Action, Field, Step};
use paths::Paths;
use serde_json::json;
use std::{error::Error, fs, io::Write, process::Command};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const USAGE: &str = "berth: set up and look after agent workstations.

Usage:
  berth                      choose a host, or create one
  berth HOST                 open a host (a saved name, or a path to a profile)
  berth list                 saved hosts
  berth iso HOST [--write /dev/sdX]
                             build the unattended installer; write it to a USB drive
  berth seed HOST            render the cloud-init seed
  berth apply HOST           apply the host's profile to it over SSH
  berth apply --behind [--update] [--reboot [--force]]
                             apply every host that is behind, one at a time, stopping
                             at the first that fails; --all takes current ones too,
                             --update runs their update task, --reboot reboots those
                             that need it unless sessions run (--force: even then)
  berth login                log in to a berth server that takes access tokens of an
                             OAuth 2.0 issuer; berth logout forgets the login
  berth remove HOST          forget a host; the machine is left as it is
  berth key                  this berth's own SSH public key, which the operator of a
                             host it sets up takes
  berth tasks HOST           what the host can do: its tasks and those of its plugins
  berth run HOST TASK [NAME=VALUE…] [--yes]
                             run one of them and leave with its exit code
                             (berth manage HOST TASK is the same)
  berth exec HOST [--user agent|operator|root] [--keep] -- COMMAND…
                             run a command on the host; its exit code is berth's
  berth attach HOST [ID]     this terminal onto a terminal session of the host, or onto
                             a new shell as the agent; Ctrl+Q leaves it running
  berth sessions HOST        the host's sessions, running and ended
  berth output HOST ID [--tail N] [--follow]
                             what a session wrote
  berth channel HOST [TOPIC-PREFIX] [--since ID]
                             messages on the host's channel as they come
  berth call HOST SERVICE METHOD [JSON]
                             call a service a plugin runs on the host
  berth inventory [HOST…]    the hosts as an Ansible inventory
  berth ssh HOST [COMMAND]   a shell on the host over plain SSH, or run a command there
  berth ssh HOST --agent     the same as the agent account
  berth registry             registries (also the Registries tab of the home screen):
                             Nexus servers with repositories, npm, file and Docker registries
  berth registry add NAME nexus|npm|raw|docker URL [USERNAME]
  berth registry repo NAME npm|raw|docker REPOSITORY [HOST[:PORT]]
  berth registry login NAME  store the password or token of a registry
  berth registry check [NAME]
                             whether a registry answers and accepts its login
  berth registry remove NAME
  berth secret [set NAME | remove NAME]
                             secret values kept on this machine
  berth host HOST            what a host is given
  berth host HOST secret NAME env VARIABLE | file PATH [MODE] | off
  berth host HOST registry NAME on|off
  berth publish REGISTRY[/REPOSITORY] [HOST [--login [REGISTRY]]]
                             upload the release archive to a Nexus raw repository or
                             file server and use it as HOST's release address; with
                             --login the cloud seed carries that registry's login
  berth image push REGISTRY[/REPOSITORY] [TAG]
                             push the container image
  berth plugins              plugins and where they come from
  berth plugins add SPEC     install a plugin with npm: a name, a tarball or a folder
  berth plugins remove NAME  remove one of yours, by its id or npm name
  berth plugins update [ID] [--check] [--apply [--reboot [--force]]]
                             update one plugin or all to their registry's latest
                             version (--check only says which have one), then say
                             which hosts are behind; --apply brings them up to date
  berth --server URL [--token TOKEN] [COMMAND…]
                             this once, use the hosts of a berth serve elsewhere
                             (http://HOST:PORT, https://HOST or unix:/path); the
                             server setting does the same for every run
  berth --version            the version
  berth update [--check]     install the latest release in this program's place; hosts
                             get its berthd and box when they are applied again
  berth config               show where things are kept and the settings
  berth config KEY [VALUE]   change a setting; no value clears it
  berth serve [--listen ADDRESS:PORT [--expose]]
                             an API over all hosts on a socket of this user; with
                             --listen also over HTTP behind a token, on this machine
                             only unless --expose allows another address
  berth inbox [HOST] [--topic PREFIX] [--all]
                             messages taken in from the hosts' channels
  berth kv get|set|rm|ls NAMESPACE [KEY] [JSON]
                             the key-value store
  berth openapi | asyncapi | operations
                             the API documents of berth serve";

fn show_config(paths: &Paths) -> Result<()> {
    let settings = paths.settings()?;
    println!("Settings   {}", paths.settings_file().display());
    println!("Hosts      {}", paths.hosts_dir().display());
    println!("Builds     {}", paths.build_dir().display());
    match paths.source() {
        Ok(source) => println!("Source     {}", source.display()),
        Err(_) => println!("Source     not found"),
    }
    println!();
    for (key, help) in paths::SETTINGS {
        // What lets someone in is not for a screen.
        let value = match settings.get(*key).and_then(|value| value.as_str()) {
            Some(_) if key.ends_with("token") || key.ends_with("secret") => Some("(set)"),
            value => value,
        };
        println!("{key} = {}\n    {help}", value.unwrap_or("(not set)"));
    }
    Ok(())
}

fn enter() -> Result<ratatui::DefaultTerminal> {
    let terminal = ratatui::init();
    crossterm::execute!(
        std::io::stdout(),
        event::EnableBracketedPaste,
        event::EnableMouseCapture,
        SetCursorStyle::BlinkingBar
    )?;
    Ok(terminal)
}

fn leave() {
    let _ = crossterm::execute!(
        std::io::stdout(),
        event::DisableBracketedPaste,
        event::DisableMouseCapture,
        SetCursorStyle::DefaultUserShape
    );
    ratatui::restore();
}

/// Load a host's profile, starting from the example for a new one.
fn open(paths: &Paths, host: &str) -> Result<App> {
    let path = paths.profile(host)?;
    paths.prepare()?;
    // Actions call the builder scripts and playbook by relative path.
    std::env::set_current_dir(paths.source()?)?;
    let (data, existing) = match fs::read(&path) {
        Ok(data) => (data, true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            (fs::read("config/example.json")?, false)
        }
        Err(error) => return Err(error.into()),
    };
    let mut app = App::new(path, parse_config(&data)?);
    // With a server set, a new host is one of the server's.
    if let (false, Some(server)) = (existing, berth_core::server::Server::configured(paths)) {
        if !server.admin(paths) {
            return Err(format!(
                "No host {host} on the berth server at {}. Adding one needs the role admin there.",
                server.url
            )
            .into());
        }
        app.config["builder"]["served"] = json!(server.url);
    }
    app.build_dir = paths.build_dir();
    app.iso_dir = paths.iso_dir();
    app.load_plugins(paths.config_dir.join("plugins"));
    app.config_dir = paths.store_dir();
    // A server's host is set up from the server, and takes the server's key.
    match app.served() {
        Some(_) => {
            let known = app
                .host_profile()
                .served()
                .and_then(|server| server.call("setup.catalogue", json!({}), None).ok());
            // The server's plugins are what its hosts can have.
            if let Some(features) = known
                .as_ref()
                .and_then(|found| found["features"].as_array())
            {
                let text = |feature: &serde_json::Value, key: &str| {
                    feature[key].as_str().unwrap_or("").to_owned()
                };
                app.set_features(
                    features
                        .iter()
                        .map(|feature| berth_tui::setup::Feature {
                            key: text(feature, "key"),
                            label: text(feature, "label"),
                            group: text(feature, "group"),
                            help: text(feature, "help"),
                        })
                        .collect(),
                );
            }
            app.own_public = known
                .and_then(|found| found["key"].as_str().map(str::to_owned))
                .unwrap_or_default();
        }
        None => {
            app.own_key = paths.own_key().ok();
            app.own_public = paths.own_public_key().unwrap_or_default();
        }
    }
    app.load_access();
    app.detect();
    // A host that is installed opens on its maintenance tab.
    if existing && !app.text(Field::SshHost).is_empty() {
        app.set_step(Step::Manage);
    }
    Ok(app)
}

/// Stop with the list of problems when the profile cannot run the action.
fn require_ready(app: &App) -> Result<()> {
    let problems = app.problems();
    if problems.is_empty() {
        return Ok(());
    }
    Err(format!("Not ready:\n  {}", problems.join("\n  ")).into())
}

fn succeed(mut command: Command) -> Result<()> {
    let status = command.status()?;
    if !status.success() {
        let program = command.get_program().to_string_lossy().into_owned();
        return Err(format!("{program} failed ({status})").into());
    }
    Ok(())
}

/// `berth iso HOST [--write DEVICE]`: build the installer without the UI.
fn build_installer(paths: &Paths, host: &str, device: Option<&str>) -> Result<()> {
    let mut app = open(paths, host)?;
    app.action = Action::Iso;
    if app.text(Field::Iso).is_empty() {
        let output = Command::new("python3")
            .args(["bin/devbox.py", "fetch-iso", "--dir"])
            .arg(&app.iso_dir)
            .stderr(std::process::Stdio::inherit())
            .output()?;
        if !output.status.success() {
            return Err("Downloading the Ubuntu installer failed".into());
        }
        app.commit(Field::Iso, String::from_utf8(output.stdout)?.trim());
    }
    require_ready(&app)?;
    app.execute()?;
    let Some(device) = device else {
        return Ok(());
    };
    let name = format!("{}-autoinstall.iso", app.text(Field::Hostname));
    print!("Erase {device} and write the installer to it? [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if !answer.trim().eq_ignore_ascii_case("y") {
        return Err("Not written".into());
    }
    let mut write = Command::new("sudo");
    write
        .args(["bash", "bin/write-usb.sh"])
        .arg(app.build_dir.join(name))
        .arg(device);
    succeed(write)
}

/// `berth host HOST ...`: the secrets and registries a host is given.
fn host_command(paths: &Paths, host: &str, args: &[&str]) -> Result<()> {
    let mut app = open(paths, host)?;
    let list = |value: &serde_json::Value| value.as_array().cloned().unwrap_or_default();
    match *args {
        [] => {
            for secret in list(&app.config["secrets"]) {
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
            for name in list(&app.config["registries"]) {
                println!("registry  {}", name.as_str().unwrap_or(""));
            }
            println!("Send them with: berth run {host} secrets");
            return Ok(());
        }
        ["secret", name, ref how @ ..] => {
            let mut secrets = list(&app.config["secrets"]);
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
            app.config["secrets"] = json!(secrets);
        }
        ["registry", name, switch @ ("on" | "off")] => {
            let mut registries = list(&app.config["registries"]);
            registries.retain(|known| known != name);
            if switch == "on" {
                if !registry::Store::new(paths).registries()?.contains_key(name) {
                    return Err(format!("No registry {name}. See: berth registry").into());
                }
                registries.push(json!(name));
            }
            app.config["registries"] = json!(registries);
        }
        _ => return Err("Usage: berth host HOST [secret NAME ... | registry NAME on|off]".into()),
    }
    app.save()
}

fn main() {
    if let Err(error) = run() {
        eprintln!("berth: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut args: Vec<&str> = args.iter().map(String::as_str).collect();
    // `--server URL [--token TOKEN]` first: this run only, through that berth serve.
    while let [flag @ ("--server" | "--token"), value, ..] = args[..] {
        let variable = if flag == "--server" {
            "BERTH_SERVER"
        } else {
            "BERTH_TOKEN"
        };
        // SAFETY: nothing else runs yet.
        unsafe { std::env::set_var(variable, value) };
        args.drain(..2);
    }
    let paths = Paths::locate()?;
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
        "login",
        "logout",
        "key",
    ];
    if let Some(server) = berth_core::server::Server::configured(&paths)
        && !args.first().is_some_and(|verb| local.contains(verb))
    {
        server
            .mirror(&paths)
            .map_err(|error| format!("the berth server at {}: {error}", server.url))?;
    }
    let host = match args[..] {
        ["list"] => {
            for (name, address) in paths.hosts() {
                println!("{name:<24} {address}");
            }
            return Ok(());
        }
        ["registry", ref rest @ ..] => return registry::registry_command(&paths, rest),
        ["secret", ref rest @ ..] => return registry::secret_command(&paths, rest),
        ["image", "push", destination, ref rest @ ..] => {
            let image =
                registry::push_image(&paths, destination, rest.first().unwrap_or(&"latest"))?;
            println!("Pushed {image}");
            return Ok(());
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
                let mut app = open(&paths, host)?;
                app.config["release_url"] = json!(url);
                app.config["release_store"] = json!(destination);
                app.config["release_private"] = json!(login.is_some());
                match login {
                    Some(name) => {
                        if registry::Store::new(&paths).curl_login(name)?.is_none() {
                            return Err(format!("Registry {name} has no stored login").into());
                        }
                        app.config["release_login"] = json!(name);
                        println!("{host} downloads it from there with the login of {name}");
                    }
                    None => {
                        if let Some(profile) = app.config.as_object_mut() {
                            profile.remove("release_login");
                        }
                        println!("{host} downloads it from there without a login");
                    }
                }
                app.save()?;
            }
            return Ok(());
        }
        ["host", host, ref rest @ ..] => return host_command(&paths, host, rest),
        ["--version" | "-V" | "version"] => {
            println!("berth {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        ["update", ref rest @ ..] => return update::command(rest),
        ["serve", ref rest @ ..] => return serve::command(&paths, rest),
        ["inbox", ref rest @ ..] => return serve::inbox_command(&paths, rest),
        ["kv", ref rest @ ..] => return serve::kv_command(&paths, rest),
        ["openapi"] => {
            println!("{}", serde_json::to_string_pretty(&serve::openapi())?);
            return Ok(());
        }
        ["operations"] => {
            println!("{}", serde_json::to_string_pretty(&serve::operations())?);
            return Ok(());
        }
        ["asyncapi"] => {
            println!("{}", serde_json::to_string_pretty(&serve::asyncapi())?);
            return Ok(());
        }
        ["plugins"] => {
            let listing = plugins::Manager::new(&paths).list()?;
            print!("{}", plugins::describe(&listing.plugins));
            return Ok(());
        }
        ["plugins", "add", spec] => {
            for line in plugins::Manager::new(&paths).add(spec)? {
                println!("{line}");
            }
            return Ok(());
        }
        ["plugins", "update", ref rest @ ..] => return commands::update_plugins(&paths, rest),
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
                return succeed(npm);
            }
            for line in manager.remove(name)? {
                println!("{line}");
            }
            return Ok(());
        }
        ["config"] => return show_config(&paths),
        ["config", key] => return paths.set(key, ""),
        ["config", key, value] => return paths.set(key, value),
        ["iso", host] => return build_installer(&paths, host, None),
        ["iso", host, "--write", device] => return build_installer(&paths, host, Some(device)),
        ["login"] => return commands::login(&paths),
        ["logout"] => return commands::logout(&paths),
        ["key"] => {
            println!(
                "{}",
                fs::read_to_string(paths.own_key()?.with_extension("pub"))?.trim()
            );
            return Ok(());
        }
        ["apply", "--behind" | "--all", ..] => return commands::apply_hosts(&paths, &args[1..]),
        ["apply", host] => return commands::apply(&paths, host),
        ["remove", host] => return commands::remove(&paths, host),
        ["seed", host] if berth_core::server::Server::configured(&paths).is_some() => {
            return commands::seed(&paths, host);
        }
        ["seed", host] => {
            let mut app = open(&paths, host)?;
            app.action = Action::CloudSeed;
            require_ready(&app)?;
            return app.execute();
        }
        ["tasks", host] => return commands::tasks(&paths, host),
        ["run" | "manage", host, task, ref rest @ ..] => {
            return commands::run(&paths, host, task, rest);
        }
        ["exec", host, ref rest @ ..] => return commands::exec(&paths, host, rest),
        ["attach", host] => return commands::attach(&paths, host, None),
        ["attach", host, id] => return commands::attach(&paths, host, Some(id)),
        ["sessions", host] => return commands::sessions(&paths, host),
        ["output", host, id, ref rest @ ..] => return commands::output(&paths, host, id, rest),
        ["channel", host, ref rest @ ..] => return commands::channel(&paths, host, rest),
        ["call", host, service, method] => {
            return commands::call(&paths, host, service, method, None);
        }
        ["call", host, service, method, body] => {
            return commands::call(&paths, host, service, method, Some(body));
        }
        ["inventory", ref hosts @ ..] => return commands::inventory(&paths, hosts),
        ["ssh", host, "--agent"] | ["ssh", "--agent", host] => {
            let status = open(&paths, host)?.agent_terminal()?.status()?;
            std::process::exit(status.code().unwrap_or(1));
        }
        ["ssh", host, ref remote @ ..] => {
            let status = open(&paths, host)?.terminal(remote)?.status()?;
            std::process::exit(status.code().unwrap_or(1));
        }
        [] => None,
        [host] if !host.starts_with('-') && host != "help" => Some(host.to_owned()),
        _ => {
            println!("{USAGE}");
            return Ok(());
        }
    };
    // Find the source before the screen is taken over, so the error is readable.
    paths.source()?;
    let mut terminal = enter()?;
    let outcome = (|| -> Result<()> {
        loop {
            let name = match &host {
                Some(host) => host.clone(),
                None => match ui::home(&mut terminal, &paths)? {
                    Some(name) => name,
                    None => return Ok(()),
                },
            };
            let mut app = open(&paths, &name)?;
            // Terminal actions (Ansible) suspend the UI and come back to it.
            while ui::run(&mut terminal, &mut app)? {
                leave();
                match app.execute() {
                    Ok(()) => println!("\nDone."),
                    Err(error) => println!("\n{error}"),
                }
                println!("Press Enter to return to berth.");
                let _ = std::io::stdin().read_line(&mut String::new());
                terminal = enter()?;
            }
            if host.is_some() {
                return Ok(());
            }
        }
    })();
    leave();
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fields::{Action, Field, Step};
    use berth_core::host::inventory as remote_inventory;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use std::path::PathBuf;
    use tui_input::{Input, backend::crossterm::EventHandler};

    fn app(config: &str) -> App {
        App::new(PathBuf::new(), parse_config(config.as_bytes()).unwrap())
    }

    fn selectable(app: &App) -> Vec<usize> {
        app.rows()
            .iter()
            .enumerate()
            .filter_map(|(position, row)| row.entry.map(|_| position))
            .collect()
    }

    fn demo_plugin() -> plugins::Plugin {
        let feature = |id: &str, group: &str, action: &str| plugins::Feature {
            id: id.into(),
            group: group.into(),
            label: id.into(),
            help: "",
            actions: vec![plugins::PluginAction {
                name: action.into(),
                label: "Say hello".into(),
                help: "",
                run: "echo 'hi'".into(),
                user: "agent".into(),
                terminal: false,
                group: String::new(),
                extends: vec![],
                agent: false,
                arguments: vec![],
                confirm: None,
            }],
            services: vec![],
            channels: vec![],
        };
        plugins::Plugin {
            id: "demo".into(),
            label: "Demo".into(),
            package: "berth-plugin-demo".into(),
            version: "1.0.0".into(),
            description: String::new(),
            dir: "/plugins/demo".into(),
            found: "/plugins/demo".into(),
            features: vec![
                feature("one", "development tools", "hello"),
                feature("two", "Games", "other"),
            ],
        }
    }

    #[test]
    fn plugin_features_join_groups() {
        use app::{Entry, Item};
        let mut app = app(r#"{"builder":{"ssh_host":"box","shipped_plugins":true}}"#);
        app.set_plugins(vec![demo_plugin()]);
        app.step = Step::Features;
        let groups = app.feature_groups();
        // One feature joins a built-in group, the other brings its own. The
        // groups of the coding agents are not shown without their plugins.
        assert_eq!(groups[0].title, "Development Tools");
        assert_eq!(groups[0].items.last(), Some(&Item::Feature(0)));
        assert_eq!(groups.last().unwrap().title, "Games");
        assert_eq!(groups.last().unwrap().items, [Item::Feature(1)]);

        // Chosen out of order, stored in the plugin's order.
        app.set_item(Item::Feature(1), true);
        app.set_item(Item::Feature(0), true);
        assert_eq!(
            app.config["ansible_vars"]["devbox_plugin_features"],
            json!(["demo/one", "demo/two"])
        );
        // What the features can do on the host is the host's to say: the
        // overview has no rows for them until it has been asked.
        app.set_step(Step::Manage);
        assert!(
            !app.entries()
                .iter()
                .any(|entry| matches!(entry, Entry::HostTask(_)))
        );
        app.set_item(Item::Feature(0), false);
        assert_eq!(
            app.config["ansible_vars"]["devbox_plugin_features"],
            json!(["demo/two"])
        );
    }

    #[test]
    fn old_agent_switches_become_plugin_features() {
        // Everything was on unless switched off; T3 followed the agents.
        let features = |config: &str| app(config).config["ansible_vars"].clone();
        assert_eq!(
            features("{}")["devbox_plugin_features"],
            json!([
                "claude/cli",
                "codex/cli",
                "github/cli",
                "pi/cli",
                "t3/server"
            ])
        );
        let old = features(
            r#"{"ansible_vars": {"devbox_agent_codex": false, "devbox_install_t3": false,
                "devbox_plugin_features": ["unity/cli"], "devbox_tools_web": true}}"#,
        );
        assert_eq!(
            old,
            json!({"devbox_tools_web": true,
                   "devbox_plugin_features": ["claude/cli", "github/cli", "pi/cli", "unity/cli"]})
        );
        let none = features(r#"{"ansible_vars": {"devbox_install_agents": false}}"#);
        assert_eq!(none["devbox_plugin_features"], json!(["github/cli"]));
        // Done once: what is switched off afterwards stays off.
        let kept = features(
            r#"{"builder": {"shipped_plugins": true}, "ansible_vars": {"devbox_plugin_features": []}}"#,
        );
        assert_eq!(kept["devbox_plugin_features"], json!([]));
    }

    #[test]
    fn groups_are_selectable_and_switch_everything_in_them() {
        use app::Entry;
        let mut app = app(r#"{"builder":{"shipped_plugins":true}}"#);
        app.set_plugins(vec![demo_plugin()]);
        app.step = Step::Features;
        let rows = app.rows();
        assert!(
            rows[0]
                .line
                .to_string()
                .starts_with("Development Tools  6 of 7")
        );
        assert_eq!(rows[0].entry, Some(Entry::Group(0)));
        assert!(rows[7].line.to_string().starts_with("  └─ one"));

        // Not everything is on, so the group is switched on first, then off.
        assert!(!app.group_all_on(0));
        app.toggle_group(0);
        assert!(app.group_all_on(0));
        assert_eq!(
            app.config["ansible_vars"]["devbox_plugin_features"],
            json!(["demo/one"])
        );
        app.toggle_group(0);
        assert!(!app.enabled(Field::ToolsWeb) && !app.enabled(Field::ToolsScientific));
        assert_eq!(
            app.config["ansible_vars"]["devbox_plugin_features"],
            json!([])
        );

        app.step = Step::Accounts;
        assert_eq!(selectable(&app)[0], 1);
        app.action = Action::Docker;
        assert_eq!(app.fields(), [Field::AgentUser]);
    }

    #[test]
    fn settings_follow_the_action_and_disk_mode() {
        let mut app = app("{}");
        app.step = Step::Install;
        app.action = Action::Docker;
        assert!(app.fields().is_empty());
        app.action = Action::UsbSeed;
        assert!(app.fields().contains(&Field::DiskType));
        assert!(!app.fields().contains(&Field::DiskSerial));
        assert!(!app.fields().contains(&Field::WifiSsid));
        app.commit(Field::DiskMode, "serial");
        app.activate(Field::Wifi);
        let fields = app.fields();
        assert!(fields.contains(&Field::DiskSerial) && !fields.contains(&Field::DiskType));
        assert!(fields.contains(&Field::WifiSsid));
        app.action = Action::Ssh;
        assert!(app.fields().contains(&Field::SshHost));
        assert!(!app.fields().contains(&Field::Hostname));
    }

    #[test]
    fn new_profiles_default_to_auto_with_wifi_off() {
        let config = parse_config(br#"{}"#).unwrap();
        assert_eq!(config["target_disk_mode"], "auto");
        assert_eq!(config["wifi_enabled"], false);
        assert_eq!(config["operator_passwordless_sudo"], true);
        assert!(config["builder"]["ask_become_pass"].is_null());
        let legacy = parse_config(
            br#"{"target_disk_serial":"OLD_SSD","wifi":{"ssid":"saved"},
                "release_url":"https://REPLACE.example/x","builder":{"ask_pass":"true"}}"#,
        )
        .unwrap();
        assert_eq!(legacy["target_disk_mode"], "serial");
        assert_eq!(legacy["wifi_enabled"], true);
        assert_eq!(legacy["release_url"], "");
        assert_eq!(legacy["builder"]["ask_pass"], true);
    }

    #[test]
    fn asking_for_the_sudo_password_follows_the_operator_until_chosen() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("studio.json");
        let profile = r#"{"builder":{"ssh_host":"192.0.2.10","ssh_user":"ops"}}"#;
        let mut app = App::new(path.clone(), parse_config(profile.as_bytes()).unwrap());
        // An edit elsewhere leaves the choice to the operator's sudo.
        app.commit(Field::Hostname, "studio");
        app.save().unwrap();
        let host = |path: &std::path::Path| berth_core::host::Host {
            name: "studio".into(),
            path: path.to_owned(),
            config: serde_json::from_slice(&fs::read(path).unwrap()).unwrap(),
            key: None,
        };
        assert!(!app.enabled(Field::AskBecomePass));
        assert!(!host(&path).asks_password());
        // An operator that types its password for sudo is asked for it.
        app.activate(Field::OperatorSudo);
        app.save().unwrap();
        assert!(app.enabled(Field::AskBecomePass) && host(&path).asks_password());
        // Once chosen, the choice stays.
        app.activate(Field::AskBecomePass);
        app.save().unwrap();
        assert!(!app.enabled(Field::AskBecomePass) && !host(&path).asks_password());
        assert_eq!(app.config["builder"]["ask_become_pass"], false);
    }

    #[test]
    fn edits_land_in_the_profile() {
        let mut app = app(r#"{"ansible_vars":{"devbox_install_dev_tools":false}}"#);
        app.commit(Field::WifiSsid, " home ");
        assert_eq!(app.config["wifi"]["ssid"], "home");
        app.commit(Field::WifiSsid, "");
        assert!(app.config["wifi"].is_null());

        app.commit(Field::Geometry, "1920x1080");
        assert_eq!(app.config["ansible_vars"]["devbox_geometry"], "1920x1080");
        app.commit(Field::Geometry, "");
        assert!(app.config["ansible_vars"].get("devbox_geometry").is_none());

        // Tool categories inherit the older single switch until set themselves.
        assert!(!app.enabled(Field::ToolsJvm));
        assert!(app.enabled(Field::Docker));
        app.activate(Field::ToolsJvm);
        assert_eq!(app.config["ansible_vars"]["devbox_tools_jvm"], true);

        app.activate(Field::Profile);
        assert_eq!(app.text(Field::Profile), "cloud");
        app.commit(Field::Profile, "desktop");
        assert_eq!(app.text(Field::Profile), "cloud");
        assert!(app.unsaved());
    }

    #[test]
    fn review_reports_what_would_fail() {
        let mut app = app("{}");
        app.action = Action::Save;
        assert!(app.problems().is_empty());
        app.action = Action::UsbSeed;
        let problems = app.problems().join("\n");
        assert!(problems.contains("Hostname"));
        assert!(problems.contains("Operator needs an SSH key"));
        assert!(problems.contains("Operator password is required"));
        // The key of this berth is one, unless the profile turns it down.
        app.own_public = "ssh-ed25519 AAAA berth@here".into();
        assert!(!app.problems().join("\n").contains("SSH key"));
        app.config["berth_ssh_key"] = json!(false);
        assert!(
            app.problems()
                .join("\n")
                .contains("Operator needs an SSH key")
        );
        app.config["berth_ssh_key"] = json!(true);
        app.own_public.clear();

        app.config["hostname"] = json!("devbox-1");
        app.config["operator_ssh_public_key"] = json!("ssh-ed25519 AAAA me");
        app.config["operator_password_hash"] = json!(format!("$6$salt${}", "a".repeat(86)));
        assert_eq!(app.problems(), Vec::<String>::new());

        // An agent without a privilege level types a password for sudo, so it needs one.
        app.commit(Field::AgentPrivileges, "limited");
        app.activate(Field::AgentSudo);
        assert_eq!(app.problems(), Vec::<String>::new());
        app.config["agent_privileges"] = json!(null);
        assert_eq!(app.problems().len(), 1);
        app.activate(Field::AgentSudo);
        app.commit(Field::AgentUser, "ops");
        assert_eq!(app.problems().len(), 1);
        // Ubuntu already has a system group with this name, so the account cannot be created.
        app.commit(Field::AgentUser, "operator");
        assert_eq!(app.problems().len(), 1);
        app.commit(Field::AgentUser, "agent");

        // A cloud seed needs somewhere to get the release from, but no SSH network.
        app.action = Action::CloudSeed;
        assert_eq!(app.problems().len(), 1);
        let step = std::mem::replace(&mut app.step, Step::Install);
        assert!(app.fields().contains(&Field::ReleaseUrl));
        app.config["release_store"] = json!("nexus/releases");
        assert!(app.problems().is_empty());
        assert!(app.fields().contains(&Field::ReleasePrivate));
        assert!(!app.fields().contains(&Field::ReleaseUrl));
        app.step = step;
        app.action = Action::Ssh;
        assert_eq!(app.problems().len(), 1);
        app.commit(Field::SshPort, "22");
        app.config["builder"]["ssh_host"] = json!("devbox.example");
        assert!(app.problems().is_empty());
    }

    #[test]
    fn choosing_an_iso_records_its_checksum() {
        let directory = tempfile::tempdir().unwrap();
        let iso = directory.path().join("ubuntu.iso");
        fs::write(&iso, "abc").unwrap();
        let mut app = app("{}");
        app.commit(Field::Iso, iso.to_str().unwrap());
        assert_eq!(
            app.text(Field::IsoSha),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn wifi_logins_come_from_and_are_checked_against_this_machine() {
        let mut app = app("{}");
        app.wifi_networks = vec![("home".into(), "secret".into()), ("cafe".into(), "".into())];
        app.commit(Field::WifiSsid, "cafe");
        assert_eq!(app.text(Field::WifiPassword), "");
        assert_eq!(app.wifi_password_matches(), None);
        app.commit(Field::WifiSsid, "home");
        assert_eq!(app.text(Field::WifiPassword), "secret");
        assert_eq!(app.wifi_password_matches(), Some(true));
        app.commit(Field::WifiPassword, "typo");
        assert_eq!(app.wifi_password_matches(), Some(false));
        assert!(app.status.contains("Differs"));
    }

    #[test]
    fn input_widget_inserts_and_deletes_at_cursor() {
        let mut input = Input::new("abde".into());
        for code in [
            KeyCode::Home,
            KeyCode::Right,
            KeyCode::Right,
            KeyCode::Char('c'),
            KeyCode::Delete,
            KeyCode::End,
            KeyCode::Backspace,
        ] {
            input.handle_event(&Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
        }
        assert_eq!(input.value(), "abc");
        assert_eq!(input.cursor(), 3);
    }

    #[test]
    fn discovers_custom_public_key_names_and_config_includes() {
        let directory = tempfile::tempdir().unwrap();
        let write = |name: &str, content: &str| {
            fs::write(directory.path().join(name), content).unwrap();
        };
        write("storage.pub", "ssh-ed25519 AAAATEST storage\n");
        write("rsa.pub", "ssh-rsa AAAATEST laptop\n");
        write("invalid.pub", "not a key");
        write("config", "Include config.d/*\nHost main * !excluded\n");
        fs::create_dir(directory.path().join("config.d")).unwrap();
        write("config.d/cloud", "Host cloud-server\n  User operator\n");
        let (public, _) = discovery::keys(directory.path());
        assert_eq!(public.len(), 2);
        assert!(public.iter().any(|(_, key)| key.ends_with("storage")));
        assert_eq!(discovery::hosts(&directory.path().join("config")).len(), 2);
    }

    #[test]
    fn ssh_inventory_uses_structured_connection_values() {
        let inventory = remote_inventory("my-cloud", "operator", "2222", "").unwrap();
        let target = &inventory["all"]["hosts"]["devbox_target"];
        assert_eq!(target["ansible_host"], "my-cloud");
        assert_eq!(target["ansible_port"], 2222);
        assert!(remote_inventory("my-cloud", "", "0", "").is_err());
        assert!(remote_inventory("-oProxyCommand=anything", "", "", "").is_err());
        assert!(remote_inventory("", "", "", "").is_err());
    }
}
