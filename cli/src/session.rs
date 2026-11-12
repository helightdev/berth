//! A command that needs a terminal, run inside the UI: a shell on a host, or a
//! sign-in that shows a link and waits for a code. It gets a pseudo-terminal of
//! its own; what it draws there is kept here and shown in a panel.

use crate::Result;
use crossterm::event::KeyEvent;
use std::{
    fs::File,
    io::{Read, Write},
    process::{Child, Command},
    sync::mpsc::{Receiver, channel},
};

/// Lines kept above the screen, for scrolling back.
const HISTORY: usize = 2000;

pub struct Session {
    pub title: String,
    /// Close the panel when the command ends well, as for a shell.
    pub closes: bool,
    parser: vt100::Parser,
    child: Child,
    /// Our end of the pseudo-terminal.
    terminal: File,
    receiver: Receiver<Vec<u8>>,
    /// The last bytes of output, for a question that arrives in two pieces.
    tail: Vec<u8>,
    /// Whether it succeeded, once it has ended.
    pub outcome: Option<bool>,
}

impl Session {
    pub fn start(title: &str, command: Command, rows: u16, cols: u16) -> Result<Session> {
        let (ours, child) = berth_pty::spawn(command, rows, cols)?;
        let (sender, receiver) = channel();
        let mut reader = ours.try_clone()?;
        std::thread::spawn(move || {
            let mut buffer = [0; 4096];
            while let Ok(count) = reader.read(&mut buffer)
                && count > 0
                && sender.send(buffer[..count].to_vec()).is_ok()
            {}
        });
        Ok(Session {
            title: title.to_owned(),
            closes: false,
            parser: vt100::Parser::new(rows, cols, HISTORY),
            child,
            terminal: ours,
            receiver,
            tail: vec![],
            outcome: None,
        })
    }

    /// Take in what the command has written and notice when it has ended.
    pub fn tick(&mut self) {
        while let Ok(bytes) = self.receiver.try_recv() {
            self.parser.process(&bytes);
            // Prompts ask where the cursor is and wait for the answer.
            self.tail.extend_from_slice(&bytes);
            let asked = self
                .tail
                .windows(4)
                .filter(|window| window == b"\x1b[6n")
                .count();
            let keep = self.tail.len().saturating_sub(3);
            self.tail.drain(..keep);
            for _ in 0..asked {
                let (row, col) = self.screen().cursor_position();
                self.send(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
            }
        }
        if self.outcome.is_none()
            && let Ok(Some(status)) = self.child.try_wait()
        {
            self.outcome = Some(status.success());
        }
    }

    pub fn running(&self) -> bool {
        self.outcome.is_none()
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// Give the terminal a new size; the command is told by the system.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        if self.screen().size() != (rows, cols) {
            self.parser.screen_mut().set_size(rows, cols);
            berth_pty::resize(&self.terminal, rows, cols);
        }
    }

    /// Look further back (positive) or forward again, by `lines`.
    pub fn scroll(&mut self, lines: isize) {
        let now = self.screen().scrollback() as isize;
        self.parser
            .screen_mut()
            .set_scrollback((now + lines).max(0) as usize);
    }

    pub fn send(&mut self, bytes: &[u8]) {
        if self.running() && !bytes.is_empty() {
            self.parser.screen_mut().set_scrollback(0);
            let _ = self.terminal.write_all(bytes);
        }
    }

    pub fn paste(&mut self, text: &str) {
        if self.screen().bracketed_paste() {
            self.send(format!("\x1b[200~{text}\x1b[201~").as_bytes());
        } else {
            self.send(text.as_bytes());
        }
    }

    /// What a terminal sends for a key, in the mode the command has asked for.
    pub fn key(&mut self, key: &KeyEvent) {
        let mut bytes = key_bytes(key);
        if self.screen().application_cursor() && bytes.len() == 3 && bytes.starts_with(b"\x1b[") {
            bytes[1] = b'O';
        }
        self.send(&bytes);
    }
}

impl Drop for Session {
    /// Interrupt what still runs, as Ctrl+C would: through sudo on a host a
    /// hang-up alone does not reach it. Then end the command.
    fn drop(&mut self) {
        if self.running() {
            self.send(&[3]);
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// What a terminal sends for a key.
pub fn key_bytes(key: &KeyEvent) -> Vec<u8> {
    crate::keys::key(key).map_or(vec![], |key| berth_tui::event::key_bytes(&key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    #[test]
    fn a_session_shows_output_and_takes_input() {
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "printf 'code? '; read code; printf 'got %s\\r\\n' \"$code\"",
        ]);
        let mut session = Session::start("demo", command, 5, 40).unwrap();
        let wait = |session: &mut Session, text: &str| {
            for _ in 0..200 {
                session.tick();
                if session.screen().contents().contains(text) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            panic!("no {text:?} in {:?}", session.screen().contents());
        };
        wait(&mut session, "code?");
        assert!(session.running());
        for key in [KeyCode::Char('4'), KeyCode::Char('2'), KeyCode::Enter] {
            session.key(&KeyEvent::new(key, KeyModifiers::NONE));
        }
        wait(&mut session, "got 42");
        for _ in 0..200 {
            session.tick();
            if !session.running() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(session.outcome, Some(true));
        // A question about the cursor position is answered.
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "stty raw -echo; printf 'ab\\033[6n'; dd bs=1 count=6 2>/dev/null | od -c",
        ]);
        let mut session = Session::start("demo", command, 5, 40).unwrap();
        wait(&mut session, "[   1   ;   3");
        // The command sees the size, and a new one.
        let mut command = Command::new("sh");
        command.args(["-c", "stty size; read go; stty size"]);
        let mut session = Session::start("demo", command, 5, 40).unwrap();
        wait(&mut session, "5 40");
        session.resize(7, 50);
        session.send(b"\r");
        wait(&mut session, "7 50");
        let alt = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT);
        assert_eq!(key_bytes(&alt), b"\x1bb");
        let control = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(key_bytes(&control), vec![3]);
    }
}
