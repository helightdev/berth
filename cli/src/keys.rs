//! A terminal's keys and mouse as the shared screens take them, and back.

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

/// The key as the terminal would have reported it.
pub fn native(key: Key) -> KeyEvent {
    let code = match key.code {
        Code::Char(ch) => KeyCode::Char(ch),
        Code::Enter => KeyCode::Enter,
        Code::Esc => KeyCode::Esc,
        Code::Tab => KeyCode::Tab,
        Code::BackTab => KeyCode::BackTab,
        Code::Backspace => KeyCode::Backspace,
        Code::Delete => KeyCode::Delete,
        Code::Insert => KeyCode::Insert,
        Code::Up => KeyCode::Up,
        Code::Down => KeyCode::Down,
        Code::Left => KeyCode::Left,
        Code::Right => KeyCode::Right,
        Code::Home => KeyCode::Home,
        Code::End => KeyCode::End,
        Code::PageUp => KeyCode::PageUp,
        Code::PageDown => KeyCode::PageDown,
        Code::F(number) => KeyCode::F(number),
    };
    let mut modifiers = KeyModifiers::NONE;
    for (on, modifier) in [
        (key.ctrl, KeyModifiers::CONTROL),
        (key.alt, KeyModifiers::ALT),
        (key.shift, KeyModifiers::SHIFT),
    ] {
        if on {
            modifiers |= modifier;
        }
    }
    KeyEvent::new(code, modifiers)
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
