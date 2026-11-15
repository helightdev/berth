//! What berth does on the machine its installation is on, which is Linux:
//! `berth serve`, the server the screens and commands use when no other is
//! set, and what only makes sense where the server runs: its setup key, its
//! store, its API documents, and the commands that need the installation's
//! own files, such as `berth ssh` with its key.

mod deploy;
pub mod serve;
mod store_command;

pub use deploy::server_context;

use crate::Result;
use crate::cli::args::{self, Global};
use crate::cli::fail::{Fail, Result as Done};
use crate::cli::{client, out};
use berth_core::{host::Host, paths::Paths};
use serde_json::{Value, json};

/// The server of this machine: a `berth serve` of this user, or one in this
/// process, whose jobs the second part waits for. With `ask`, a host whose
/// sudo asks for a password has it asked for on the terminal when the
/// server starts, as `berth serve` does.
pub fn server(ask: bool) -> Result<Option<crate::Local>> {
    let (server, here) = serve::here::local(&Paths::locate()?, ask)?;
    if let Some(here) = &here {
        berth_page::net::serve_here(here.dial());
    }
    Ok(Some((server, Box::new(move || serve::here::finish(here)))))
}

/// Give the server in this process a host's sudo password for this run, as
/// `berth serve` takes one on its terminal at the start. Whether there is
/// such a server.
pub fn give_sudo_password(name: &str, password: &str) -> bool {
    serve::here::give_sudo_password(name, password)
}

/// ssh runs berth as its askpass helper, the program of SSH_ASKPASS, with
/// the prompt as its one argument and the variable that says where the
/// password is; the answer is all that may go to standard output. Whether
/// this run was one.
pub fn askpass(args: &[String]) -> Result<bool> {
    let Ok(source) = std::env::var(berth_core::keys::askpass::SOURCE) else {
        return Ok(false);
    };
    let prompt = match args {
        [_, word, prompt @ ..] if word == "askpass" => prompt.join(" "),
        [_, prompt] => prompt.clone(),
        _ => return Ok(false),
    };
    let answer = berth_core::keys::askpass::answer(&prompt, &source, None)?;
    println!("{answer}");
    Ok(true)
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

/// What only works with the installation on this machine is refused when
/// berth is set to use a server elsewhere.
fn here_only(global: &Global, what: &str) -> Done<Paths> {
    if let Some(server) = client::named(global)
        && !berth_page::device::local(&server)
    {
        return Err(Fail::refused(format!(
            "{what} works where the berth server runs, and this berth uses the one at {server}. Run it there"
        )));
    }
    Ok(Paths::locate()?)
}

/// `berth ssh HOST [--agent] [-- COMMAND…]`: plain SSH with the
/// installation's key, for a host whose controller cannot be reached.
pub fn ssh(global: &Global, ssh: &args::Ssh) -> Done<()> {
    let paths = here_only(
        global,
        "berth ssh, which logs in with the server's own key,",
    )
    .map_err(|fail| {
        Fail::refused(format!(
            "{} berth session attach {} reaches a host through any server",
            fail.message, ssh.host
        ))
    })?;
    let host = Host::load(&paths, &ssh.host)?;
    if !host.access().reachable() {
        return Err(Fail::failed(format!("{} has no SSH address yet", ssh.host)));
    }
    let mut command = host.ssh(&paths)?;
    command.arg(host.target());
    if ssh.agent {
        // Root login and the agent's own key are not a given, so it goes
        // through the SSH user and sudo; agent-env adds the display and tools.
        let agent = host.agent();
        if agent.starts_with('-')
            || !agent
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
        {
            return Err(Fail::failed("The agent's account name is not a valid one"));
        }
        command.arg("-t").arg(format!(
            "sudo -iu {agent} sh -c 'command -v agent-env >/dev/null && exec agent-env; exec bash -l'"
        ));
    }
    command.args(&ssh.command);
    let status = command.status()?;
    match status.code() {
        Some(0) => Ok(()),
        code => Err(Fail::exit(code.unwrap_or(1))),
    }
}

/// Several hosts' inventories as one.
pub fn merge_inventory(hosts: &[(String, Value)]) -> Value {
    let mut all = serde_json::Map::new();
    for (name, inventory) in hosts {
        all.insert(
            name.clone(),
            inventory["all"]["hosts"]["devbox_target"].clone(),
        );
    }
    json!({"all": {"hosts": all}})
}

/// `berth host inventory [NAME…]`: the hosts as an Ansible inventory, with
/// the installation's key as a file of this machine.
pub fn inventory(global: &Global, names: &[String]) -> Done<()> {
    let paths = here_only(global, "berth host inventory")?;
    let hosts = match names {
        [] => Host::all(&paths)
            .into_iter()
            .filter(|host| !host.address().is_empty())
            .collect(),
        names => names
            .iter()
            .map(|name| Host::load(&paths, name))
            .collect::<Result<Vec<_>>>()?,
    };
    let mut entries = vec![];
    for host in hosts {
        let inventory = host
            .inventory(&paths)
            .map_err(|error| format!("{}: {error}", host.name))?;
        entries.push((host.name, inventory));
    }
    out::json(&merge_inventory(&entries));
    Ok(())
}

/// The enrollment key an apply asks for, for a host not on the tailnet yet;
/// it goes to the role on standard output.
pub fn tailnet_key(host: &str) -> Done<()> {
    let paths = Paths::locate()?;
    println!("{}", berth_core::tailnet::key_for(&paths, host)?);
    Ok(())
}

/// `berth registry publish`: upload the release archive built here, and
/// make it the release address of a host through the server.
pub fn publish(
    global: &Global,
    destination: &str,
    host: Option<&str>,
    login: Option<&str>,
) -> Done<()> {
    let paths = here_only(
        global,
        "berth registry publish, which builds the release here,",
    )?;
    std::env::set_current_dir(paths.source()?)?;
    let url = berth_core::registry::publish(&paths, destination)?;
    out::done(format!("Published {url}"));
    let Some(host) = host else {
        return Ok(());
    };
    let client = client::Client::open(global)?;
    let mut profile = crate::cli::host_profile(&client, host)?;
    profile["release_url"] = json!(url);
    profile["release_store"] = json!(destination);
    let login = login.map(|name| match name {
        "" => destination.split('/').next().unwrap_or(""),
        name => name,
    });
    profile["release_private"] = json!(login.is_some());
    match login {
        Some(name) => {
            profile["release_login"] = json!(name);
            out::done(format!(
                "{host} downloads it from there with the login of {name}"
            ));
        }
        None => {
            if let Some(profile) = profile.as_object_mut() {
                profile.remove("release_login");
            }
            out::done(format!("{host} downloads it from there without a login"));
        }
    }
    crate::cli::save_host(&client, host, &profile)
}

/// `berth registry push-image`: push berth's container image from here.
pub fn push_image(global: &Global, destination: &str, tag: &str) -> Done<()> {
    let paths = here_only(global, "berth registry push-image, which pushes from here,")?;
    let image = berth_core::registry::push_image(&paths, destination, tag)?;
    out::done(format!("Pushed {image}"));
    Ok(())
}

/// What `berth serve --web` says, as berth 0.5 had a page in the browser.
pub const WEB_GONE: &str = "--web does nothing any more: berth has no page in the browser, and its screens are berth ui. Take --web out of this command line";

/// `berth server …`: the server of this machine itself.
pub fn server_command(command: args::Server) -> Done<()> {
    let paths = Paths::locate()?;
    match command {
        args::Server::Run(run) => {
            let mut words = vec![];
            if let Some(listen) = &run.listen {
                words.extend(["--listen", listen.as_str()]);
            }
            if run.expose {
                words.push("--expose");
            }
            if run.web {
                out::warn(WEB_GONE);
            }
            Ok(serve::command(&paths, &words)?)
        }
        args::Server::SetupKey { revoke } => {
            let words: &[&str] = if revoke { &["--remove"] } else { &[] };
            Ok(serve::setup_key::command(&paths, words)?)
        }
        args::Server::Store(store) => {
            let target;
            let file;
            let words: Vec<&str> = match &store {
                args::Store::Status => vec![],
                args::Store::Encrypt => vec!["encrypt"],
                args::Store::Rekey => vec!["rekey"],
                args::Store::Decrypt => {
                    crate::cli::ask::confirm("This writes the store's values plain again.")?;
                    vec!["decrypt", "--yes"]
                }
                args::Store::Copy {
                    target: given,
                    target_file,
                    into_existing,
                } => {
                    let mut words = vec!["copy"];
                    match (given, target_file) {
                        (Some(given), None) => {
                            target = given.clone();
                            words.push(&target);
                        }
                        (None, Some(path)) => {
                            file = path.display().to_string();
                            words.extend(["--target-file", file.as_str()]);
                        }
                        _ => {
                            return Err(Fail::usage(
                                "berth server store copy takes a TARGET or --target-file FILE",
                            ));
                        }
                    }
                    if *into_existing {
                        words.push("--into-existing");
                    }
                    words
                }
            };
            Ok(store_command::command(&paths, &words)?)
        }
        args::Server::Api(api) => {
            let document = match api {
                args::Api::Openapi => serde_json::to_value(serve::openapi())?,
                args::Api::Asyncapi => serve::asyncapi(),
                args::Api::Operations => serve::operations(),
            };
            out::json(&document);
            Ok(())
        }
        args::Server::Image { tag } => deploy::image(tag),
        args::Server::Deploy(deploy) => deploy::deploy(&deploy),
    }
}
