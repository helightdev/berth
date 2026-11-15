//! A terminal's keys and mouse as the shared screens take them: the same as
//! berth's own terminal UI.

use berth_tui::{Click, Code, Event, Key, Mouse};
use crossterm::event::{
    Event as Native, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};

pub fn key(key: &KeyEvent) -> Option<Key> {
    let code = match key.code {
        KeyCode::Char(ch) => Code::Char(ch),
        KeyCode::Enter => Code::Enter,
        KeyCode::Esc => Code::Esc,
        KeyCode::Tab => Code::Tab,
        KeyCode::BackTab => Code::BackTab,
        KeyCode::Backspace => Code::Backspace,
        KeyCode::Delete => Code::Delete,
        KeyCode::Insert => Code::Insert,
        KeyCode::Up => Code::Up,
        KeyCode::Down => Code::Down,
        KeyCode::Left => Code::Left,
        KeyCode::Right => Code::Right,
        KeyCode::Home => Code::Home,
        KeyCode::End => Code::End,
        KeyCode::PageUp => Code::PageUp,
        KeyCode::PageDown => Code::PageDown,
        KeyCode::F(number) => Code::F(number),
        _ => return None,
    };
    Some(Key {
        code,
        ctrl: key.modifiers.contains(KeyModifiers::CONTROL),
        alt: key.modifiers.contains(KeyModifiers::ALT),
        shift: key.modifiers.contains(KeyModifiers::SHIFT),
    })
}

/// How soon a key has to follow Esc to be read as that key with Alt. A
/// terminal sends Alt+R as Esc and `r` together, but a relay on the way, as
/// between Windows and WSL, can pass them on in two writes.
const ALT_WAIT: std::time::Duration = std::time::Duration::from_millis(25);

thread_local! {
    /// An event read while waiting to see whether Esc was Alt.
    static AHEAD: std::cell::RefCell<Option<Native>> = const { std::cell::RefCell::new(None) };
}

/// Whether an event is waiting, for up to `timeout`.
pub fn poll(timeout: std::time::Duration) -> std::io::Result<bool> {
    if AHEAD.with_borrow(Option::is_some) {
        return Ok(true);
    }
    crossterm::event::poll(timeout)
}

/// The next event of the terminal, with Esc and a character that come right
/// after it taken as that character with Alt.
pub fn read() -> std::io::Result<Native> {
    if let Some(event) = AHEAD.take() {
        return Ok(event);
    }
    read_from(crossterm::event::read, || crossterm::event::poll(ALT_WAIT))
}

fn read_from(
    mut read: impl FnMut() -> std::io::Result<Native>,
    mut soon: impl FnMut() -> std::io::Result<bool>,
) -> std::io::Result<Native> {
    let event = read()?;
    let escape = matches!(&event, Native::Key(key)
        if key.code == KeyCode::Esc && key.modifiers.is_empty() && key.kind == KeyEventKind::Press);
    if !escape || !soon()? {
        return Ok(event);
    }
    let next = read()?;
    if let Native::Key(key) = &next
        && let KeyCode::Char(_) = key.code
        && key.kind == KeyEventKind::Press
        && (key.modifiers - KeyModifiers::SHIFT).is_empty()
    {
        return Ok(Native::Key(KeyEvent::new_with_kind(
            key.code,
            key.modifiers | KeyModifiers::ALT,
            key.kind,
        )));
    }
    AHEAD.set(Some(next));
    Ok(event)
}

/// The event, when it is one the screens take: a key going down, a click, the
/// wheel, a paste.
pub fn event(event: &Native) -> Option<Event> {
    match event {
        Native::Key(pressed) if pressed.kind != KeyEventKind::Release => {
            key(pressed).map(Event::Key)
        }
        Native::Mouse(mouse) => {
            let click = match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => Click::Left,
                MouseEventKind::Down(MouseButton::Right) => Click::Right,
                MouseEventKind::ScrollUp => Click::ScrollUp,
                MouseEventKind::ScrollDown => Click::ScrollDown,
                _ => return None,
            };
            Some(Event::Mouse(Mouse {
                click,
                column: mouse.column,
                row: mouse.row,
            }))
        }
        Native::Paste(text) => Some(Event::Paste(text.clone())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode, modifiers: KeyModifiers) -> Native {
        Native::Key(KeyEvent::new(code, modifiers))
    }

    /// What reading gives when the terminal has `events`, each after the
    /// first coming soon after the one before when `soon` is set.
    fn reads(events: &[Native], soon: bool) -> Vec<Native> {
        let queue = std::cell::RefCell::new(events.to_vec());
        let mut got = vec![];
        while AHEAD.with_borrow(Option::is_some) || !queue.borrow().is_empty() {
            let event = match AHEAD.take() {
                Some(event) => event,
                None => read_from(
                    || Ok(queue.borrow_mut().remove(0)),
                    || Ok(soon && !queue.borrow().is_empty()),
                )
                .unwrap(),
            };
            got.push(event);
        }
        got
    }

    #[test]
    fn esc_and_a_letter_right_after_it_are_alt_and_the_letter() {
        let esc = press(KeyCode::Esc, KeyModifiers::NONE);
        let r = press(KeyCode::Char('r'), KeyModifiers::NONE);
        let alt_r = press(KeyCode::Char('r'), KeyModifiers::ALT);
        assert_eq!(reads(&[esc.clone(), r.clone()], true), [alt_r]);
        let shifted = press(KeyCode::Char('R'), KeyModifiers::SHIFT);
        let alt_shift_r = press(KeyCode::Char('R'), KeyModifiers::SHIFT | KeyModifiers::ALT);
        assert_eq!(reads(&[esc.clone(), shifted], true), [alt_shift_r]);
        // A letter typed later is a key of its own.
        assert_eq!(reads(&[esc.clone(), r.clone()], false), [esc.clone(), r]);
        // So is anything but a letter, and it is not lost.
        let up = press(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(reads(&[esc.clone(), up.clone()], true), [esc.clone(), up]);
        let ctrl_c = press(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(reads(&[esc.clone(), ctrl_c.clone()], true), [esc, ctrl_c]);
    }
}
