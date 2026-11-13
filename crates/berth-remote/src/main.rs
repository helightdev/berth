//! berth-remote: the hosts of a berth serve, in this terminal.

use berth_remote::options::{self, Parsed};
use berth_remote::{device, keys, settings};
use crossterm::event::{self, Event as Native, KeyCode, KeyEventKind, KeyModifiers};
use std::time::Duration;

fn main() {
    let options = match options::parse(std::env::args().skip(1), |name| std::env::var(name).ok()) {
        Ok(Parsed::Run(options)) => options,
        Ok(Parsed::Version) => {
            println!("berth-remote {}", berth_api::VERSION);
            return;
        }
        Ok(Parsed::Help) => {
            println!("{}", options::USAGE);
            return;
        }
        Err(error) => {
            eprintln!("berth-remote: {error}\n\n{}", options::USAGE);
            std::process::exit(2);
        }
    };
    let settings = settings::Settings::new(settings::folder(|name| std::env::var(name).ok()));
    match device::Terminal::new(options.linked, settings, options.forget) {
        Ok(terminal) => berth_page::device::install(terminal),
        Err(error) => {
            eprintln!("berth-remote: could not forget the kept sign-in: {error}");
            std::process::exit(1);
        }
    }
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
    let mut app = berth_page::App::new();
    loop {
        berth_remote::pump();
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
