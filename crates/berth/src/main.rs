//! berth: set up and look after agent workstations. On Linux the program
//! has its installation in it: `berth serve`, the commands and a server for
//! the screens. Elsewhere it is a client of a `berth serve` on another
//! machine, with the same screens.

mod connect;
#[cfg(target_os = "linux")]
mod local;
#[cfg(not(target_os = "linux"))]
#[path = "client_only.rs"]
mod local;
mod update;

use std::error::Error;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// A server on this machine for the screens, and what to do once they close.
type Local = (String, Box<dyn FnOnce()>);

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
                             tailnet_api_key is the control server's API key, or a
                             Tailscale OAuth client as CLIENT_ID:CLIENT_SECRET, with
                             which berth makes the keys hosts join the tailnet with
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
    let mut finish: Box<dyn FnOnce()> = Box::new(|| {});
    // Without a server here, the screens ask for one and its token.
    if linked.local(&settings)
        && let Some((server, then)) = local::server()?
    {
        linked.server = Some(server);
        finish = then;
    }
    berth_page::device::install(linked, settings);
    let shown = berth_page::screens::run(host.map(str::to_owned));
    finish();
    Ok(shown?)
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
    if local::askpass(&args)? {
        return Ok(());
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
    if let Some(host) = opens_screens(&args) {
        return screens(host);
    }
    match args[..] {
        ["--version" | "-V" | "version"] => {
            println!("berth {}", berth_api::VERSION);
            Ok(())
        }
        ["update", ref rest @ ..] => update::command(rest),
        ["connect", url] => connect::command(url),
        _ => local::command(&args),
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
