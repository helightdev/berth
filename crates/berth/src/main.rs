mod commands;
mod key_command;
mod profile;
mod provisioner_command;
mod resource_command;
mod serve;
mod store_command;
mod update;

use berth_core::{paths, plugins, registry};
use paths::Paths;
use serde_json::json;
use std::{error::Error, process::Command};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const USAGE: &str = "berth: set up and look after agent workstations.

Usage:
  berth [HOST]               berth's screens: the hosts, and the host HOST
  berth ui [HOST]            the same: of the server this berth uses, of a berth
                             serve of yours on this machine, or of one that runs in
                             berth itself until the screens close
  berth list                 saved hosts
  berth host add NAME ADDRESS [--user USER] [--port PORT] [--auth AUTH]
          [--type ssh|managed|workstation] [--no-contact]
                             save a machine that exists already, by its address, and
                             install berth's key there; its type is ssh unless told
  berth host install HOST    install berth's controller alone on a host that is not
                             a workstation: sessions, actions and files, no desktop
  berth host type HOST ssh|managed|workstation
                             say what the host is, as for one added by its address
                             that berth set up before; nothing on the host changes
  berth host rename HOST NAME [--hostname]
                             give a host another name in berth, with what berth keeps
                             of it; the machine is left as it is, and --hostname makes
                             NAME its profile's hostname for the next apply
  berth iso HOST [--write /dev/sdX]
                             build the unattended installer; write it to a USB drive
  berth seed HOST            render the cloud-init seed
  berth apply HOST           apply the host's profile to it over SSH
  berth apply --behind [--update] [--reboot [--force]]
                             apply every host that is behind, one at a time, stopping
                             at the first that fails; --all takes current ones too,
                             --update runs their update action, --reboot reboots those
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
  berth actions HOST         what the host can do: its controller's actions, those of
                             its plugins and berth's own
  berth run HOST ACTION [NAME=VALUE…] [--yes]
                             run one of them and leave with its exit code
                             (berth manage HOST ACTION is the same)
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

/// End quietly when what reads berth's output has gone, as `head` does after
/// its lines: printing then panics, which is no fault of berth's to report.
fn end_quietly_on_a_closed_pipe() {
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic| {
        let said = panic.payload().downcast_ref::<String>();
        if said.is_some_and(|said| said.starts_with("failed printing to std")) {
            std::process::exit(141);
        }
        report(panic);
    }));
}

/// Whether these arguments open the screens: none, `ui`, or one word that is
/// no command, taken as the host to open.
fn opens_screens<'a>(args: &[&'a str]) -> Option<Option<&'a str>> {
    match args {
        [] | ["ui"] => Some(None),
        ["ui", host] => Some(Some(host)),
        [host] if !host.starts_with('-') && !COMMANDS.contains(host) => Some(Some(host)),
        _ => None,
    }
}

/// The commands that are one word, which no host name can stand for.
const COMMANDS: &[&str] = &[
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
    "config",
    "serve",
    "version",
    "openapi",
    "asyncapi",
    "operations",
    "update",
    "key",
    "setup-key",
];

/// `berth [ui] [HOST]`: the screens of a server: the one this berth is set
/// to use, a `berth serve` of this user on this machine, or else one in
/// this process for as long as they are open, which waits for the jobs it
/// runs before berth ends.
fn screens(host: Option<&str>) -> Result<()> {
    use berth_page::settings::{self, Settings};
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        let said = match host {
            Some(word) => format!(
                "there is no command `{word}`, and no terminal for the screens of host {word}. `berth help` lists the commands"
            ),
            None => "berth's screens need a terminal, and there is none here. `berth help` lists the commands that need none".into(),
        };
        return Err(said.into());
    }
    let var = |name: &str| std::env::var(name).ok();
    let settings = Settings::new(settings::folder(var));
    match settings.take_in(settings::old_folder(var).as_deref()) {
        Ok(Some(server)) => eprintln!("berth: berth-remote's server is berth's now: {server}"),
        Ok(None) => {}
        Err(error) => eprintln!("berth: berth-remote's settings were not taken in: {error}"),
    }
    let mut linked = berth_page::device::Linked::from(var);
    let mut here = None;
    if linked.local(&settings) {
        let (server, started) = serve::here::local(&Paths::locate()?)?;
        if let Some(started) = &started {
            berth_page::net::serve_here(started.dial());
        }
        linked.server = Some(server);
        here = started;
    }
    berth_page::device::install(linked, settings);
    let shown = berth_page::screens::run(host.map(str::to_owned));
    serve::here::finish(here);
    Ok(shown?)
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

fn main() {
    end_quietly_on_a_closed_pipe();
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
    // The screens go through a server, and take nothing from this machine's own files.
    if let Some(host) = opens_screens(&args) {
        return screens(host);
    }
    let paths = Paths::locate()?;
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
        ["--version" | "-V" | "version"] => {
            println!("berth {}", berth_api::VERSION);
            Ok(())
        }
        ["update", ref rest @ ..] => update::command(rest),
        ["serve", ref rest @ ..] => serve::command(&paths, rest),
        ["setup-key", ref rest @ ..] => serve::setup_key::command(&paths, rest),
        ["connect", url] => commands::connect(&paths, url),
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
            println!("{USAGE}");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::opens_screens;

    #[test]
    fn the_screens_open_with_no_command_or_a_host() {
        assert_eq!(opens_screens(&[]), Some(None));
        assert_eq!(opens_screens(&["ui"]), Some(None));
        assert_eq!(opens_screens(&["studio"]), Some(Some("studio")));
        assert_eq!(opens_screens(&["ui", "dock"]), Some(Some("dock")));
        for command in [
            &["list"][..],
            &["--version"],
            &["run", "studio", "status"],
            &["help"],
        ] {
            assert_eq!(opens_screens(command), None, "{command:?}");
        }
    }
}
