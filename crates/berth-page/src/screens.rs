//! The screens in a terminal: drawing, and the keys and mouse taken in.

use crate::keys;
use crossterm::event;
use std::time::Duration;

/// Stop the mouse and paste reports, which ratatui's own restore leaves on.
fn stop_reports() {
    let _ = crossterm::execute!(
        std::io::stdout(),
        event::DisableBracketedPaste,
        event::DisableMouseCapture
    );
}

/// The screens in this terminal until they are left, opening the host `open` once
/// the server has listed it.
pub fn run(open: Option<String>) -> std::io::Result<()> {
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
    let outcome = screens(&mut terminal, open);
    stop_reports();
    ratatui::restore();
    outcome
}

fn screens(terminal: &mut ratatui::DefaultTerminal, open: Option<String>) -> std::io::Result<()> {
    let mut app = crate::App::new();
    if let Some(host) = open {
        app.open_when_listed(&host);
    }
    loop {
        crate::pump();
        app.tick();
        terminal.draw(|frame| app.draw(frame))?;
        // Wait a little for input; what the network says is taken in on the next turn.
        if !keys::poll(Duration::from_millis(30))? {
            continue;
        }
        loop {
            if let Some(event) = keys::event(&keys::read()?) {
                app.event(event);
            }
            if app.quit() {
                return Ok(());
            }
            if !keys::poll(Duration::ZERO)? {
                break;
            }
        }
    }
}
