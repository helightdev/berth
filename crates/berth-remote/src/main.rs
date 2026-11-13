//! berth-remote: the page of `berth serve --web` in a terminal, for a
//! machine that only looks after the hosts of a berth serve elsewhere. It
//! runs on Linux, macOS and Windows: the screens are the page's own, and
//! this program gives them a terminal and a network in place of a browser.

#[path = "../../berth-web/src/admin.rs"]
mod admin;
#[path = "../../berth-web/src/app.rs"]
mod app;
mod browser;
#[path = "../../berth-web/src/home.rs"]
mod home;
#[path = "../../berth-web/src/installed.rs"]
mod installed;
mod keys;
mod net;
mod portal;
#[path = "../../berth-web/src/remote.rs"]
mod remote;
#[path = "../../berth-web/src/setup.rs"]
mod setup;

use crossterm::event::{self, Event as Native, KeyCode, KeyEventKind, KeyModifiers};
use std::time::Duration;

const USAGE: &str = "\
berth-remote: the hosts of a berth serve, in this terminal

Usage: berth-remote [--server ADDRESS] [--forget]

  --server ADDRESS   the berth serve to use, such as https://berth.example
                     or http://192.0.2.10:7878 (also BERTH_SERVER)
  --forget           keep neither the server nor the token for the next run
  --version          say the version

The token comes from BERTH_TOKEN, or is asked for. A setup key from
`berth setup-key` comes from BERTH_SETUP_KEY. Neither goes on the command
line, where other users of the machine could read it.

A plugin package is chosen in the file dialog of the system: on Linux that
of zenity or kdialog. Without one, BERTH_REMOTE_FILE names the file.

The server and the token are kept in the settings of this user:
%APPDATA%\\berth-remote on Windows, ~/.config/berth-remote elsewhere.

Ctrl+Q quits.";

fn options() -> Result<browser::Options, String> {
    let variable = |name: &str| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
    };
    let mut options = browser::Options {
        linked: browser::Linked {
            server: variable("BERTH_SERVER"),
            token: variable("BERTH_TOKEN").map(|token| token.trim().to_owned()),
            setup_key: variable("BERTH_SETUP_KEY").map(|key| key.trim().to_owned()),
        },
        ephemeral: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--server" | "-s" => {
                options.linked.server = Some(args.next().ok_or("--server needs an address")?)
            }
            "--forget" => options.ephemeral = true,
            "--version" | "-V" => {
                println!("berth-remote {}", berth_api::VERSION);
                std::process::exit(0);
            }
            "--help" | "-h" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("{other} is no option of berth-remote\n\n{USAGE}")),
        }
    }
    Ok(options)
}

fn main() {
    let options = match options() {
        Ok(options) => options,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    };
    browser::set_options(options);
    if let Err(error) = run() {
        eprintln!("berth-remote: {error}");
        std::process::exit(1);
    }
}

/// Stop the mouse and paste reports, which ratatui's own restore leaves on.
fn stop_reports() {
    let _ = crossterm::execute!(
        std::io::stdout(),
        event::DisableBracketedPaste,
        event::DisableMouseCapture
    );
}

fn run() -> std::io::Result<()> {
    let mut terminal = ratatui::init();
    let restore = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        stop_reports();
        restore(info);
    }));
    // Not every terminal of Windows knows bracketed paste: there a paste
    // arrives as keys.
    let _ = crossterm::execute!(
        std::io::stdout(),
        event::EnableMouseCapture,
        event::EnableBracketedPaste
    );
    let outcome = screens(&mut terminal);
    stop_reports();
    ratatui::restore();
    outcome
}

fn screens(terminal: &mut ratatui::DefaultTerminal) -> std::io::Result<()> {
    let mut app = app::App::new();
    loop {
        net::pump();
        portal::pump();
        app.tick();
        terminal.draw(|frame| app.draw(frame))?;
        // Wait a little for input; what the network says is taken in on the next turn.
        if !keys::poll(Duration::from_millis(30))? {
            continue;
        }
        loop {
            match keys::read()? {
                Native::Key(key)
                    if key.kind != KeyEventKind::Release
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                        && matches!(key.code, KeyCode::Char('q' | 'Q')) =>
                {
                    return Ok(());
                }
                native => {
                    if let Some(event) = keys::event(&native) {
                        app.event(event);
                    }
                }
            }
            if !keys::poll(Duration::ZERO)? {
                break;
            }
        }
    }
}
