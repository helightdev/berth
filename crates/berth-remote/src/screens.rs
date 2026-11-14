//! The screens in a terminal: drawing, and the keys and mouse taken in.

use crate::keys;
use crossterm::event::{self, Event as Native, KeyCode, KeyEventKind, KeyModifiers};
use std::time::Duration;

/// Stop the mouse and paste reports, which ratatui's own restore leaves on.
fn stop_reports() {
    let _ = crossterm::execute!(
        std::io::stdout(),
        event::DisableBracketedPaste,
        event::DisableMouseCapture
    );
}

/// The screens in this terminal until Ctrl+Q, opening the host `open` once
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
    let mut app = berth_page::App::new();
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
