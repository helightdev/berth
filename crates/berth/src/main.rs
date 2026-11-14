mod app;
mod commands;
mod fields;
mod key_command;
mod keys;
mod mirror;
mod picker;
mod provisioner_command;
mod provisioner_tab;
mod remote;
mod resource_command;
mod serve;
mod session;
mod store_command;
mod ui;
mod update;

use app::{App, parse_config};
use berth_core::{credentials, discovery, job, paths, plugins, registry};
pub(crate) use berth_tui::{markdown, theme};
use crossterm::{cursor::SetCursorStyle, event};
use fields::{Action, Field, Step};
use paths::{Paths, layout};
use serde_json::json;
use std::{error::Error, fs, io::Write, process::Command};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const USAGE: &str = "berth: set up and look after agent workstations.

Usage:
  berth                      choose a host, or create one
  berth HOST                 open a host (a saved name, or a path to a profile)
  berth list                 saved hosts
  berth host add NAME ADDRESS [--user USER] [--port PORT] [--auth AUTH]
          [--level reachable|controller|workstation] [--no-contact]
                             save a machine that exists already, by its address, and
                             install berth's key there; it is reachable unless told
  berth host install HOST    install berth's controller alone on a host below a
                             workstation: sessions, tasks and files, no desktop
  berth host level HOST reachable|controller|workstation
                             say what the host is, as for one added by its address
                             that berth set up before; nothing on the host changes
  berth iso HOST [--write /dev/sdX]
                             build the unattended installer; write it to a USB drive
  berth seed HOST            render the cloud-init seed
  berth apply HOST           apply the host's profile to it over SSH
  berth apply --behind [--update] [--reboot [--force]]
                             apply every host that is behind, one at a time, stopping
                             at the first that fails; --all takes current ones too,
                             --update runs their update task, --reboot reboots those
                             that need it unless sessions run (--force: even then)
  berth connect URL          use the berth server at URL: give it the setup key made
                             there with berth setup-key, on standard input
  berth remove HOST          forget a host; the machine is left as it is
  berth profile HOST         the host's profile, as JSON on standard output
  berth profile HOST --set FILE
                             save the profile in FILE (- for standard input) as the
                             host's; a host of a berth server keeps it on the server
  berth credentials HOST [--show]
                             the passwords berth generated for the host's accounts;
                             only to a terminal, unless --show
  berth adopt HOST [[USER@]ADDRESS] [--port PORT] [--key FILE] [--force]
                             save a host from the profile it keeps, read over SSH
                             with sudo; without an address, a saved host by its own;
                             --force replaces a host of that name
  berth provisioners         what can create machines, and what each asks for
  berth provisioner          the provisioners, and whether each can be used here
  berth provisioner types    the types of provisioner the plugins bring
  berth provisioner add NAME TYPE [SETTING=VALUE…]
                             register a provisioner of a type, such as incus/vm,
                             under a name of its own, with its settings
  berth provisioner remove NAME
                             forget a provisioner that holds nothing
  berth provisioner NAME [SETTING=VALUE…]
                             how it is now, what berth made with it, and its
                             actions; the values are of its own arguments, this once
  berth provisioner NAME actions
  berth provisioner NAME run ACTION [NAME=VALUE…] [--yes]
                             run one of its actions, such as settings, which keeps
                             where it is, secrets, new-deployment, or the plugin's own
  berth resource [--provisioner NAME] [--kind KIND] [--host HOST]
                             what provisioners made for berth: machines, deployments
  berth resource ID          one of them as its provisioner says it is now
  berth resource ID actions | run ACTION [NAME=VALUE…] [--yes] | remove [--yes]
  berth resource ID logs [--tail N] [--follow]
  berth resource ID change [--spec FILE|-] [--secret NAME[:VARIABLE]]… [NAME=VALUE…]
  berth deploy NAME --provisioner PROVISIONER --spec FILE|- [--secret NAME[:VARIABLE]]…
          [NAME=VALUE…]      a deployment from a specification, such as a Compose
                             file; again with the same name gives it the new one
  berth provision HOST PROVISIONER [NAME=VALUE…] [--yes]
                             create the machine for a saved host and set it up
  berth instance HOST        the host's machine, as its resource
  berth instance HOST start|stop
  berth instance HOST destroy [--yes]
                             delete the machine; the profile stays
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
                             secret values, kept in berth's store (see berth store);
                             tailnet_api_key is the control server's API key, for
                             enrollment keys
  berth tailnet-key HOST     create a one-use enrollment key for HOST, valid for an
                             hour, at the control server of the tailnet_ settings
                             (HOST may also be the path of its profile)
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
  berth setup-key [--remove] make a key, valid for a day, that lets the first user
                             collect the token of the berth serve on this machine;
                             --remove ends it
  berth inbox [HOST] [--topic PREFIX] [--all]
                             messages taken in from the hosts' channels
  berth kv get|set|rm|ls NAMESPACE [KEY] [JSON]
                             the key-value store
  berth store                where berth keeps its state, and whether it is encrypted
  berth store encrypt | rekey | decrypt [--yes]
                             seal the store's values with a new key file, change the
                             key, or write them plain again; berth serve is stopped first
  berth store copy TARGET | - | --target-file FILE [--into-existing]
                             copy the store into another SQLite file or PostgreSQL
                             database, which berth config store then switches to; a
                             URL as TARGET shows up in the process list, so - reads
                             it from standard input
  berth openapi | asyncapi | operations
                             the API documents of berth serve";

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
/// Whether these arguments open the terminal screen: none, or a host's name.
fn opens_screen(args: &[&str]) -> bool {
    // The commands that are one word.
    const ALONE: &[&str] = &[
        "list",
        "registry",
        "secret",
        "plugins",
        "provisioners",
        "provisioner",
        "resource",
        "inventory",
        "inbox",
        "kv",
        "store",
        "help",
    ];
    match args {
        [] => true,
        [one] => !one.starts_with('-') && !ALONE.contains(one),
        _ => false,
    }
}

/// Whether a command reads this machine's own plugins: the screen, and the
/// commands that apply, build or provision with them or list them.
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
    opens_screen(args) || args.first().is_some_and(|verb| VERBS.contains(verb))
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

fn open(paths: &Paths, host: &str) -> Result<App> {
    let place = berth_core::profiles::Place::resolve(paths, host)?;
    paths.prepare()?;
    // Actions call the builder scripts and playbook by relative path.
    std::env::set_current_dir(paths.source()?)?;
    let (data, existing) = match place.load(paths)? {
        Some(profile) => (profile.to_string().into_bytes(), true),
        None => (fs::read(layout::EXAMPLE)?, false),
    };
    let served = place.server_url().is_some();
    let mut app = App::new(paths.clone(), place, parse_config(&data)?);
    // With a server set, a new host is one of the server's.
    if let (false, true, Some(server)) = (
        existing,
        served,
        berth_core::server::Server::configured(paths),
    ) {
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
    // A host is reached without the store; what needs it says why it is missing.
    app.access = registry::Store::new(paths)
        .map_err(|error| format!("berth's store does not open: {error}"));
    // A server's host is set up from the server, with the server's plugins.
    if app.served().is_some() {
        let known = app
            .host_profile()
            .served(paths)
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
    }
    // The keys of the ring that logs in to the host: the server's for its hosts.
    let ring = match app.host_profile().served(paths) {
        Some(server) => key_command::listed(&server).unwrap_or_default(),
        None => key_command::local(paths).unwrap_or_default(),
    };
    app.own_public = ring
        .iter()
        .find(|key| key.name == berth_api::profile::BERTH_KEY)
        .map(|key| key.public.clone())
        .unwrap_or_default();
    app.ring_keys = ring.into_iter().map(|key| key.name).collect();
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
    // A seed and an installer make a whole workstation.
    berth_core::level::guard(&app.host_profile(), berth_core::level::Capability::Apply)?;
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
            .args([layout::BUILDER, "fetch-iso", "--dir"])
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
        .args(["bash", layout::WRITE_USB])
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
                if !registry::Store::new(paths)?
                    .registries()?
                    .contains_key(name)
                {
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
    // ssh runs berth as its askpass helper, the program of SSH_ASKPASS, with
    // the prompt as its one argument and the variable that says where the
    // password is; the answer is all that may go to standard output.
    if let Ok(source) = std::env::var(berth_core::keys::askpass::SOURCE) {
        let prompt = match args[..] {
            ["askpass", ref prompt @ ..] => Some(prompt.join(" ")),
            [prompt] => Some(prompt.to_owned()),
            _ => None,
        };
        if let Some(prompt) = prompt {
            let answer = berth_core::keys::askpass::answer(&prompt, &source, None)?;
            println!("{answer}");
            return Ok(());
        }
    }
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
    // The first open of the store takes in the files of an older berth and
    // says so on standard error, which the screen would hide. A store that
    // does not open is reported where it is needed.
    if opens_screen(&args) {
        let _ = berth_core::store::shared(&paths);
    }
    if loads_plugins(&args) && berth_core::server::Server::configured(&paths).is_none() {
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
    // The screen opens at once on what the last mirror left; the commands
    // that print are given current data.
    let mut mirroring = None;
    if let Some(server) = berth_core::server::Server::configured(&paths)
        && !args.first().is_some_and(|verb| local.contains(verb))
    {
        if opens_screen(&args) {
            // A host the last mirror does not have cannot be opened without one.
            if let [name] = args[..]
                && !paths.hosts().iter().any(|(known, _)| known == name)
            {
                server
                    .mirror(&paths)
                    .map_err(|error| format!("the berth server at {}: {error}", server.url))?;
            }
            mirroring = Some(mirror::Mirroring::of(&paths, server));
        } else {
            server
                .mirror(&paths)
                .map_err(|error| format!("the berth server at {}: {error}", server.url))?;
        }
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
                        if registry::Store::new(&paths)?.curl_login(name)?.is_none() {
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
        // A host's own subcommands start with secret or registry.
        ["host", "add", name, address, ref rest @ ..]
            if !["secret", "registry"].contains(&name) =>
        {
            return commands::add_host(&paths, name, address, rest);
        }
        ["host", "install", host] => return commands::install_controller(&paths, host),
        ["host", "level", host, level] => return commands::set_level(&paths, host, level),
        ["host", host, ref rest @ ..] => return host_command(&paths, host, rest),
        ["--version" | "-V" | "version"] => {
            println!("berth {}", berth_api::VERSION);
            return Ok(());
        }
        ["update", ref rest @ ..] => return update::command(rest),
        ["serve", ref rest @ ..] => return serve::command(&paths, rest),
        ["setup-key", ref rest @ ..] => return serve::setup_key::command(&paths, rest),
        ["connect", url] => return commands::connect(&paths, url),
        ["inbox", ref rest @ ..] => return serve::inbox_command(&paths, rest),
        ["kv", ref rest @ ..] => return serve::kv_command(&paths, rest),
        ["store", ref rest @ ..] => return store_command::command(&paths, rest),
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
                let done = succeed(npm);
                if let Some(line) = plugins::remembered(&paths) {
                    eprintln!("{line}");
                }
                return done;
            }
            for line in manager.remove(name)? {
                println!("{line}");
            }
            return Ok(());
        }
        ["config"] => return show_config(&paths),
        ["config", key] => return change_setting(&paths, key, ""),
        ["config", key, value] => return change_setting(&paths, key, value),
        ["tailnet-key", host] => {
            // An apply runs this on the machine that runs berth, for a host
            // not on the tailnet yet; the key goes to the role on stdout.
            println!("{}", berth_core::tailnet::key_for(&paths, host)?);
            return Ok(());
        }
        ["profile", host] => return commands::show_profile(&paths, host),
        ["profile", host, "--set", file] => return commands::set_profile(&paths, host, file),
        ["credentials", host] => return commands::credentials(&paths, host, false),
        ["credentials", host, "--show"] | ["credentials", "--show", host] => {
            return commands::credentials(&paths, host, true);
        }
        ["iso", host] => return build_installer(&paths, host, None),
        ["iso", host, "--write", device] => return build_installer(&paths, host, Some(device)),
        ["key", ref rest @ ..] => return key_command::command(&paths, rest),
        ["apply", "--behind" | "--all", ..] => return commands::apply_hosts(&paths, &args[1..]),
        ["apply", host] => return commands::apply(&paths, host),
        ["remove", host] => return commands::remove(&paths, host),
        ["adopt", host, ref rest @ ..] => return commands::adopt(&paths, host, rest),
        ["provisioners"] => return commands::provisioners(&paths),
        ["provisioner", ref rest @ ..] => return provisioner_command::command(&paths, rest),
        ["provision", host, provisioner, ref rest @ ..] => {
            return resource_command::provision(&paths, host, provisioner, rest);
        }
        ["instance", host, ref rest @ ..] => return resource_command::instance(&paths, host, rest),
        ["resource", ref rest @ ..] => return resource_command::command(&paths, rest),
        ["deploy", name, ref rest @ ..] => return resource_command::deploy(&paths, name, rest),
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
    if let Some(running) = &mut mirroring {
        running.start();
    }
    let mut terminal = enter()?;
    let outcome = (|| -> Result<()> {
        loop {
            let name = match &host {
                Some(host) => host.clone(),
                None => match ui::home(&mut terminal, &paths, &mut mirroring)? {
                    Some(name) => name,
                    None => return Ok(()),
                },
            };
            let mut app = open(&paths, &name)?;
            app.start();
            // Terminal actions (Ansible) suspend the UI and come back to it.
            while ui::run(&mut terminal, &mut app, &mut mirroring)? {
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
    use super::opens_screen;

    #[test]
    fn only_the_screen_opens_before_the_mirror_is_made() {
        assert!(opens_screen(&[]) && opens_screen(&["studio"]));
        assert!(!opens_screen(&["list"]) && !opens_screen(&["--version"]));
        assert!(!opens_screen(&["run", "studio", "task"]));
    }

    use super::*;
    use crate::fields::{Action, Field, Step};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use tui_input::{Input, backend::crossterm::EventHandler};

    /// Paths of a folder that is never made, for an app that saves nothing.
    fn nowhere() -> Paths {
        Paths {
            config_dir: "/nonexistent/berth-test/config".into(),
            data_dir: "/nonexistent/berth-test/data".into(),
        }
    }

    /// An app of a new profile that is never saved, in a file whose name is
    /// no host name, so that the profile needs a hostname of its own.
    fn app(config: &str) -> App {
        App::new(
            nowhere(),
            berth_core::profiles::Place::File("/nonexistent/berth-test/new profile.json".into()),
            parse_config(config.as_bytes()).unwrap(),
        )
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
            provisioners: vec![],
        }
    }

    #[test]
    fn plugin_features_join_groups() {
        use app::{Entry, Item};
        let mut app = app(r#"{"access":{"address":"box"},"builder":{"shipped_plugins":true}}"#);
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

        // The accounts on the Settings tab start with a heading.
        app.step = Step::Settings;
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
        // What an installer picks the disk and network with has defaults.
        app.action = Action::UsbSeed;
        assert!(!app.fields().contains(&Field::DiskMode));
        app.step = Step::Settings;
        assert!(app.fields().contains(&Field::DiskType));
        assert!(!app.fields().contains(&Field::DiskSerial));
        assert!(!app.fields().contains(&Field::WifiSsid));
        app.commit(Field::DiskMode, "serial");
        app.activate(Field::Wifi);
        let fields = app.fields();
        assert!(fields.contains(&Field::DiskSerial) && !fields.contains(&Field::DiskType));
        assert!(fields.contains(&Field::WifiSsid));
        app.step = Step::Install;
        app.action = Action::Ssh;
        assert!(app.fields().contains(&Field::SshHost));
        assert!(!app.fields().contains(&Field::Hostname));
    }

    #[test]
    fn new_profiles_default_to_auto_with_wifi_off() {
        let config = parse_config(br#"{}"#).unwrap();
        assert_eq!(config["target_disk_mode"], "auto");
        assert_eq!(config["wifi_enabled"], false);
        assert!(config.get("operator_passwordless_sudo").is_none());
        assert!(config["access"]["sudo"].is_null());
        // The switches of a profile from before are dropped, whatever they said.
        let old = parse_config(br#"{"operator_passwordless_sudo": false, "berth_ssh_key": false}"#)
            .unwrap();
        assert!(
            old.get("operator_passwordless_sudo").is_none() && old.get("berth_ssh_key").is_none()
        );
        let legacy = parse_config(
            br#"{"target_disk_serial":"OLD_SSD","wifi":{"ssid":"saved"},
                "release_url":"https://REPLACE.example/x"}"#,
        )
        .unwrap();
        assert_eq!(legacy["target_disk_mode"], "serial");
        assert_eq!(legacy["wifi_enabled"], true);
        assert_eq!(legacy["release_url"], "");
        // A profile of the qemu kind there once was shows as cloud, reached from the QEMU host.
        let qemu = parse_config(br#"{"profile": "qemu"}"#).unwrap();
        assert_eq!(qemu["profile"], "cloud");
        assert_eq!(
            qemu["ansible_vars"]["devbox_bootstrap_ssh_cidr"],
            "10.0.2.2/32"
        );
        let app = app(r#"{"profile": "qemu"}"#);
        assert_eq!(app.text(Field::RecoveryCidr), "10.0.2.2/32");
    }

    #[test]
    fn asking_for_the_sudo_password_is_off_until_chosen() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("studio.json");
        let profile = r#"{"access":{"address":"192.0.2.10","user":"ops"}}"#;
        let paths = Paths {
            config_dir: directory.path().join("config"),
            data_dir: directory.path().join("data"),
        };
        let mut app = App::new(
            paths,
            berth_core::profiles::Place::File(path.clone()),
            parse_config(profile.as_bytes()).unwrap(),
        );
        // An edit elsewhere leaves it unset.
        app.commit(Field::Hostname, "studio");
        app.save().unwrap();
        let host = |path: &std::path::Path| berth_core::host::Host {
            name: "studio".into(),
            place: berth_core::profiles::Place::File(path.to_owned()),
            config: serde_json::from_slice(&fs::read(path).unwrap()).unwrap(),
        };
        assert!(!app.enabled(Field::AskBecomePass));
        assert!(!host(&path).asks_password());
        assert!(app.config["builder"]["ask_become_pass"].is_null());
        // A first apply as an account whose sudo asks for a password.
        app.activate(Field::AskBecomePass);
        app.save().unwrap();
        assert!(app.enabled(Field::AskBecomePass) && host(&path).asks_password());
        // Once chosen, the choice stays.
        app.activate(Field::AskBecomePass);
        app.save().unwrap();
        assert!(!app.enabled(Field::AskBecomePass) && !host(&path).asks_password());
        assert_eq!(app.config["access"]["sudo"], "none");
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
        // The key of this berth is one, which an older profile cannot turn down.
        app.own_public = "ssh-ed25519 AAAA berth@here".into();
        assert!(!app.problems().join("\n").contains("SSH key"));
        app.config["berth_ssh_key"] = json!(false);
        assert!(!app.problems().join("\n").contains("SSH key"));
        app.config.as_object_mut().unwrap().remove("berth_ssh_key");
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

        // Only the USB installer insists on an operator password.
        let hash = app.config["operator_password_hash"].take();
        assert_eq!(app.problems(), ["Operator password is required (F4 or F5)"]);
        app.action = Action::CloudSeed;
        assert!(!app.problems().join("\n").contains("password"));
        app.action = Action::UsbSeed;
        app.config["operator_password_hash"] = hash;

        // A cloud seed needs somewhere to get the release from, but no SSH network.
        app.action = Action::CloudSeed;
        assert_eq!(app.problems().len(), 1);
        let step = std::mem::replace(&mut app.step, Step::Settings);
        assert!(app.fields().contains(&Field::ReleaseUrl));
        app.config["release_store"] = json!("nexus/releases");
        assert!(app.problems().is_empty());
        assert!(app.fields().contains(&Field::ReleasePrivate));
        assert!(!app.fields().contains(&Field::ReleaseUrl));
        app.step = step;
        app.action = Action::Ssh;
        assert_eq!(app.problems().len(), 1);
        app.commit(Field::SshPort, "22");
        app.config["access"]["address"] = json!("devbox.example");
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
}
