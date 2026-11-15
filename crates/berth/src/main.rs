//! berth: set up and look after agent workstations. Its commands and its
//! screens are clients of a berth server's API, on Linux, macOS and Windows.
//! On Linux the program has the server in it as well: `berth serve`, and
//! one in the process for a command when no other is set.

mod cli;
#[cfg(target_os = "linux")]
mod local;
#[cfg(not(target_os = "linux"))]
#[path = "client_only.rs"]
mod local;
mod update;

use std::error::Error;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// A server on this machine, and what to do once it is no longer needed.
type Local = (String, Box<dyn FnOnce()>);

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

/// `berth ui [HOST]`: the screens of a server: the one this berth is set to
/// use, a `berth serve` of this user on this machine, or else one in this
/// process for as long as they are open, which waits for the jobs it runs
/// before berth ends.
fn screens(global: &cli::args::Global, host: Option<&str>) -> cli::fail::Result<()> {
    use berth_page::settings::{self, Settings};
    use cli::fail::Fail;
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(Fail::usage(
            "berth's screens need a terminal, and there is none here. berth help lists the commands that need none",
        ));
    }
    let var = |name: &str| std::env::var(name).ok();
    let settings = Settings::new(settings::folder(var));
    cli::client::take_in_remote(&settings, settings::old_folder(var).as_deref());
    let mut linked = berth_page::device::Linked {
        server: global
            .server
            .clone()
            .or_else(|| var("BERTH_SERVER").filter(|server| !server.trim().is_empty())),
        token: var("BERTH_TOKEN").filter(|token| !token.trim().is_empty()),
        setup_key: None,
    };
    if let Some(file) = &global.token_file {
        linked.token = Some(std::fs::read_to_string(file)?.trim().to_owned());
    }
    let mut finish: Box<dyn FnOnce()> = Box::new(|| {});
    // Without a server here, the screens ask for one and its token.
    if linked.local(&settings)
        && let Some((server, then)) = local::server(true)?
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
    let args: Vec<String> = std::env::args().collect();
    match local::askpass(&args) {
        Ok(true) => return,
        Ok(false) => {}
        Err(error) => {
            eprintln!("berth: {error}");
            std::process::exit(1);
        }
    }
    std::process::exit(cli::main(args));
}
