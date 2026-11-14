//! Keys and the mouse as the screens take them. A terminal's and a browser's
//! events are both turned into these.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Code {
    Char(char),
    Enter,
    Esc,
    Tab,
    BackTab,
    Backspace,
    Delete,
    Insert,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    F(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    pub code: Code,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Key {
    pub fn plain(code: Code) -> Key {
        Key {
            code,
            ctrl: false,
            alt: false,
            shift: false,
        }
    }

    pub fn ctrl(ch: char) -> Key {
        Key {
            ctrl: true,
            ..Key::plain(Code::Char(ch))
        }
    }

    /// Ctrl and this letter, whatever its case.
    pub fn is_ctrl(&self, ch: char) -> bool {
        self.ctrl && matches!(self.code, Code::Char(given) if given.eq_ignore_ascii_case(&ch))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Click {
    Left,
    Right,
    ScrollUp,
    ScrollDown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mouse {
    pub click: Click,
    pub column: u16,
    pub row: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Key(Key),
    Mouse(Mouse),
    Paste(String),
}

/// What a terminal sends for a key.
pub fn key_bytes(key: &Key) -> Vec<u8> {
    let bytes = match key.code {
        Code::Char(ch) if key.ctrl && ch.is_ascii_alphabetic() => {
            vec![ch.to_ascii_lowercase() as u8 - b'a' + 1]
        }
        Code::Char(' ' | '@') if key.ctrl => vec![0],
        Code::Char(ch @ ('[' | '\\' | ']' | '^' | '_')) if key.ctrl => vec![ch as u8 - b'@'],
        Code::Char(ch) => ch.to_string().into_bytes(),
        Code::BackTab => b"\x1b[Z".to_vec(),
        Code::PageUp => b"\x1b[5~".to_vec(),
        Code::PageDown => b"\x1b[6~".to_vec(),
        Code::Insert => b"\x1b[2~".to_vec(),
        Code::F(number @ 1..=4) => vec![0x1b, b'O', b'P' + number - 1],
        Code::F(number @ 5..=12) => {
            let code = [15, 17, 18, 19, 20, 21, 23, 24][number as usize - 5];
            format!("\x1b[{code}~").into_bytes()
        }
        Code::F(_) => vec![],
        Code::Enter => vec![b'\r'],
        Code::Backspace => vec![0x7f],
        Code::Tab => vec![b'\t'],
        Code::Esc => vec![0x1b],
        Code::Up => b"\x1b[A".to_vec(),
        Code::Down => b"\x1b[B".to_vec(),
        Code::Right => b"\x1b[C".to_vec(),
        Code::Left => b"\x1b[D".to_vec(),
        Code::Home => b"\x1b[H".to_vec(),
        Code::End => b"\x1b[F".to_vec(),
        Code::Delete => b"\x1b[3~".to_vec(),
    };
    // Alt is the key with an escape in front.
    if key.alt && !bytes.is_empty() {
        return [vec![0x1b], bytes].concat();
    }
    bytes
}

/// What a key does to a line being typed, if anything.
pub fn edit(input: &mut tui_input::Input, key: &Key) {
    use tui_input::InputRequest::*;
    let request = match (key.code, key.ctrl, key.alt) {
        (Code::Backspace, false, false) | (Code::Char('h'), true, false) => DeletePrevChar,
        (Code::Delete, false, false) => DeleteNextChar,
        (Code::Left, false, false) | (Code::Char('b'), true, false) => GoToPrevChar,
        (Code::Left, true, false) | (Code::Char('b'), false, true) => GoToPrevWord,
        (Code::Right, false, false) | (Code::Char('f'), true, false) => GoToNextChar,
        (Code::Right, true, false) | (Code::Char('f'), false, true) => GoToNextWord,
        (Code::Char('u'), true, false) => DeleteLine,
        (Code::Char('w'), true, false)
        | (Code::Char('d'), false, true)
        | (Code::Backspace, false, true) => DeletePrevWord,
        (Code::Delete, true, false) => DeleteNextWord,
        (Code::Char('k'), true, false) => DeleteTillEnd,
        (Code::Char('y'), true, false) => Yank,
        (Code::Char('a'), true, false) | (Code::Home, false, false) => GoToStart,
        (Code::Char('e'), true, false) | (Code::End, false, false) => GoToEnd,
        (Code::Char(ch), false, false) | (Code::Char(ch), true, true) => InsertChar(ch),
        _ => return,
    };
    input.handle(request);
}

/// Type pasted text into a line, without what is not text.
pub fn paste(input: &mut tui_input::Input, text: &str) {
    for ch in text.chars().filter(|ch| !ch.is_control()) {
        input.handle(tui_input::InputRequest::InsertChar(ch));
    }
}

/// Type pasted text of several lines into a line: each line break is shown
/// as `newline`, and a tab as two spaces.
pub fn paste_lines(input: &mut tui_input::Input, text: &str, newline: char) {
    for ch in text.replace("\r\n", "\n").chars() {
        let typed: &[char] = match ch {
            '\n' | '\r' => &[newline],
            '\t' => &[' ', ' '],
            ch if ch.is_control() => &[],
            _ => &[ch],
        };
        for &ch in typed {
            input.handle(tui_input::InputRequest::InsertChar(ch));
        }
    }
}
