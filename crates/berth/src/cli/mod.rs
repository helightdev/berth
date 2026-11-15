//! berth's command line: one grammar, `berth THING ACTION [NAME] [FLAGS]`,
//! whose commands are calls of the same API the screens use.

pub mod args;
pub mod ask;
pub mod client;
mod data;
#[cfg(test)]
mod docs;
pub mod fail;
pub mod follow;
mod host;
mod jobs;
pub mod login;
mod old;
pub mod out;
mod pages;
mod setup;
mod things;

#[cfg(target_os = "linux")]
pub use host::{profile_of as host_profile, save as save_host};

use args::{Cli, Command};
use clap::{CommandFactory, FromArgMatches};
use client::Client;
use fail::{Fail, Result};

/// What bare `berth` prints: what it is, the commands most people need
/// first, and how to open the screens.
pub const SHORT_HELP: &[(&str, &[(&str, &str)])] = &[
    (
        "Start here",
        &[
            ("berth ui", "the screens: hosts, set-up, plugins"),
            (
                "berth host create NAME",
                "describe a new host, then install it",
            ),
            (
                "berth host add NAME ADDRESS",
                "take in a machine that exists",
            ),
            ("berth login URL", "use the hosts of a berth server"),
        ],
    ),
    (
        "Every day",
        &[
            ("berth ls", "the hosts and whether each answers"),
            ("berth exec HOST -- COMMAND", "run a command on a host"),
            ("berth ssh HOST", "a shell on a host"),
            ("berth apply HOST", "apply a host's profile to it"),
        ],
    ),
];

fn short_help() {
    use out::{Style, styled};
    println!("berth sets up and looks after agent workstations.");
    let wide = SHORT_HELP
        .iter()
        .flat_map(|(_, lines)| lines.iter().map(|(command, _)| command.len()))
        .max()
        .unwrap_or(0);
    for (heading, lines) in SHORT_HELP {
        println!("\n{}", styled(heading, Style::Bold));
        for (command, what) in *lines {
            println!(
                "  {}{}   {}",
                styled(command, Style::Accent),
                " ".repeat(wide - command.len()),
                styled(what, Style::Dim)
            );
        }
    }
    println!("\nberth help lists every command. berth THING --help explains one.");
}

/// The command tree, as clap and the reference page take it.
pub fn command() -> clap::Command {
    let statuses: Vec<String> = args::EXIT_STATUSES
        .iter()
        .map(|(status, means)| format!("{status} {means}"))
        .collect();
    Cli::command().after_help(format!(
        "berth THING --help explains a thing and its actions.\nExit statuses: {}. A command run on a host passes its own through.",
        statuses.join("; ")
    ))
}

/// Run berth with these arguments: the exit status.
pub fn main(args: Vec<String>) -> i32 {
    if let Some(fail) = old::refuse(&args) {
        out::fail(&fail);
        return fail.status;
    }
    let matches = match command().try_get_matches_from(&args) {
        Ok(matches) => matches,
        Err(error) => return clap_error(error, &args),
    };
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(error) => return clap_error(error, &args),
    };
    out::init(&cli.global);
    match run(cli) {
        Ok(()) => 0,
        Err(fail) => {
            out::fail(&fail);
            fail.status
        }
    }
}

/// Help and versions as clap prints them, and usage errors as berth says
/// them: one sentence, with what was meant when that is clear.
fn clap_error(error: clap::Error, args: &[String]) -> i32 {
    use clap::error::{ContextKind, ContextValue, ErrorKind};
    match error.kind() {
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
            let _ = error.print();
            return 0;
        }
        ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
            let _ = error.print();
            return fail::USAGE;
        }
        _ => {}
    }
    let json = args
        .windows(2)
        .any(|pair| (pair[0] == "-o" || pair[0] == "--output") && pair[1] == "json")
        || args
            .iter()
            .any(|arg| arg == "--output=json" || arg == "-ojson");
    let value = |kind| match error.get(kind) {
        Some(ContextValue::String(text)) => Some(text.clone()),
        Some(ContextValue::Strings(texts)) => texts.first().cloned(),
        _ => None,
    };
    let message = match error.kind() {
        ErrorKind::InvalidSubcommand => {
            let word = value(ContextKind::InvalidSubcommand).unwrap_or_default();
            let before = before_word(args, &word);
            match value(ContextKind::SuggestedSubcommand) {
                Some(meant) => {
                    let after = after_word(args, &word);
                    format!(
                        "no command \"{word}\". Did you mean: {}",
                        [before.as_str(), meant.as_str(), after.as_str()]
                            .iter()
                            .filter(|part| !part.is_empty())
                            .copied()
                            .collect::<Vec<_>>()
                            .join(" ")
                    )
                }
                None => format!("no command \"{word}\". {before} --help lists the commands"),
            }
        }
        _ => {
            let text = error.render().to_string();
            let mut lines = text.lines().filter(|line| !line.trim().is_empty());
            let first = lines
                .next()
                .unwrap_or("")
                .trim_start_matches("error: ")
                .to_owned();
            let tip = lines
                .find(|line| line.trim_start().starts_with("tip:"))
                .map(|tip| format!(". {}", tip.trim().trim_start_matches("tip: ")))
                .unwrap_or_default();
            let usage = text
                .lines()
                .find(|line| line.starts_with("Usage:"))
                .map(|usage| format!(". {}", usage.trim()))
                .unwrap_or_default();
            format!("{first}{tip}{usage}")
        }
    };
    let fail = Fail::usage(message);
    if json {
        let body = serde_json::json!({"error": {"code": fail.code, "message": fail.message, "exit": fail.status}});
        eprintln!("{body}");
    } else {
        eprintln!("berth: {}", fail.message);
    }
    fail.status
}

/// The command up to an unknown word, as `berth host`.
fn before_word(args: &[String], word: &str) -> String {
    let at = args
        .iter()
        .position(|arg| arg == word)
        .unwrap_or(args.len());
    let mut words: Vec<&str> = vec!["berth"];
    words.extend(
        args[1..at.max(1)]
            .iter()
            .filter(|arg| !arg.starts_with('-'))
            .map(String::as_str),
    );
    words.join(" ")
}

/// What came after an unknown word.
fn after_word(args: &[String], word: &str) -> String {
    let at = args
        .iter()
        .position(|arg| arg == word)
        .map_or(args.len(), |at| at + 1);
    args[at..].join(" ")
}

fn run(cli: Cli) -> Result<()> {
    let global = cli.global;
    let Some(command) = cli.command else {
        short_help();
        return Ok(());
    };
    let open = || Client::open(&global);
    match command {
        Command::Ls => host::list(&open()?),
        Command::Apply(apply) => host::apply(&open()?, &apply),
        Command::Exec(exec) => things::exec(&open()?, &exec),
        Command::Ssh(ssh) => crate::local::ssh(&global, &ssh),
        Command::Run(run) => things::run_action(&open()?, &run),
        Command::Ui { host } => crate::screens(&global, host.as_deref()),
        Command::Host(command) => host::command(&global, command),
        Command::Rollout(rollout) => jobs::rollout(&open()?, &rollout),
        Command::Action(args::ActionCommand::List { host }) => {
            things::actions(&open()?, host.as_deref())
        }
        Command::Session(command) => things::session(&open()?, command),
        Command::Provisioner(command) => things::provisioner(&open()?, command),
        Command::Resource(command) => things::resource(&open()?, command),
        Command::Job(command) => jobs::command(&open()?, command),
        Command::Key(command) => setup::key(&open()?, command),
        Command::Secret(command) => setup::secret(&open()?, command),
        Command::Registry(command) => setup::registry(&global, command),
        Command::Plugin(command) => setup::plugin(&open()?, command),
        Command::Config(command) => setup::config(&open()?, command),
        Command::Port(args::Port::Forward {
            target,
            ports,
            resource,
            address,
        }) => data::forward(&open()?, &target, &ports, resource, &address),
        Command::Inbox(command) => data::inbox(&open()?, command),
        Command::Kv(command) => data::kv(&open()?, command),
        Command::Channel(command) => data::channel(&open()?, command),
        Command::Service(command) => data::service(&open()?, command),
        #[cfg(target_os = "linux")]
        Command::Server(command) => crate::local::server_command(command),
        #[cfg(target_os = "linux")]
        Command::Serve(serve) => crate::local::server_command(args::Server::Run(serve)),
        Command::Login(login) => login::login(&global, &login),
        Command::Logout => login::logout(),
        Command::Update(update) => crate::update::command(&update),
        Command::Version => version(&global),
        Command::Completion { shell } => {
            let mut tree = self::command();
            clap_complete::generate(shell, &mut tree, "berth", &mut std::io::stdout());
            Ok(())
        }
        Command::Askpass { .. } => Err(Fail::usage(
            "berth askpass is run by ssh, with what it needs in its environment",
        )),
        #[cfg(target_os = "linux")]
        Command::TailnetKey { host } => crate::local::tailnet_key(&host),
    }
}

/// `berth version`: this berth's, and that of the server it is set to use,
/// which an older server still answers.
fn version(global: &args::Global) -> Result<()> {
    use serde_json::json;
    let server = client::named(global).map(|url| {
        match Client::open_any(global, &client::settings())
            .and_then(|client| client.server_version())
        {
            Ok((version, api)) => json!({"url": url, "version": version, "api": api}),
            Err(fail) => json!({"url": url, "error": fail.message}),
        }
    });
    let mut said = json!({"version": berth_api::VERSION, "api": berth_api::API});
    if let Some(server) = &server {
        said["server"] = server.clone();
    }
    out::data(&said, || {
        println!("berth {} (API {})", berth_api::VERSION, berth_api::API);
        if let Some(server) = &server {
            let url = server["url"].as_str().unwrap_or("");
            match server["version"].as_str() {
                Some(version) => {
                    println!("server {url}: berth {version} (API {})", server["api"]);
                    let api = server["api"].as_u64().unwrap_or(0) as u32;
                    if let Some(older) = berth_api::compat::older_server(url, version, api) {
                        out::warn(older);
                    }
                }
                None => println!(
                    "server {url}: not reached ({})",
                    server["error"].as_str().unwrap_or("")
                ),
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> std::result::Result<Cli, clap::Error> {
        let args: Vec<&str> = std::iter::once("berth")
            .chain(line.split_whitespace())
            .collect();
        let matches = command().try_get_matches_from(args)?;
        Cli::from_arg_matches(&matches)
    }

    #[test]
    fn the_definition_holds_together() {
        command().debug_assert();
    }

    #[test]
    fn the_short_help_names_commands_there_are() {
        for (_, lines) in SHORT_HELP {
            for (command, _) in *lines {
                let line = command
                    .trim_start_matches("berth")
                    .replace("NAME", "studio")
                    .replace("HOST", "studio")
                    .replace("ADDRESS", "192.0.2.10")
                    .replace("URL", "https://berth.example")
                    .replace("COMMAND", "true");
                assert!(parse(&line).is_ok(), "{command}");
            }
        }
    }

    #[test]
    fn flags_go_before_or_after_the_names_in_both_forms() {
        for line in [
            "host show studio -o json",
            "-o json host show studio",
            "host show --output=json studio",
            "--no-input host apply studio --detach",
        ] {
            assert!(parse(line).is_ok(), "{line}");
        }
        let cli = parse("session logs studio s1 --tail 50").unwrap();
        let cli2 = parse("session logs --tail=50 studio s1").unwrap();
        assert_eq!(format!("{:?}", cli.command), format!("{:?}", cli2.command));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_command_line_of_a_server_of_0_5_still_starts_it() {
        // Containers and units of berth 0.5 run the server with --web.
        let cli = parse("serve --listen 0.0.0.0:7777 --expose --web").unwrap();
        assert!(matches!(cli.command, Some(Command::Serve(serve)) if serve.web && serve.expose));
        let help = command()
            .find_subcommand_mut("serve")
            .unwrap()
            .render_help()
            .to_string();
        assert!(!help.contains("--web"), "{help}");
    }

    #[test]
    fn a_host_may_have_the_name_of_an_action() {
        let cli = parse("host show list").unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Host(args::Host::Show { name })) if name == "list"
        ));
        let cli = parse("exec add -- uname -a").unwrap();
        assert!(
            matches!(cli.command, Some(Command::Exec(exec)) if exec.host == "add" && exec.command == ["uname", "-a"])
        );
    }

    #[test]
    fn an_unknown_word_is_a_usage_error_with_what_was_meant() {
        let args: Vec<String> = ["berth", "aply", "studio"].map(String::from).to_vec();
        assert_eq!(main(args), fail::USAGE);
        let error = parse("aply studio").unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::InvalidSubcommand);
        assert_eq!(
            after_word(&["berth".into(), "aply".into(), "studio".into()], "aply"),
            "studio"
        );
        assert_eq!(
            before_word(&["berth".into(), "host".into(), "shwo".into()], "shwo"),
            "berth host"
        );
    }
}
