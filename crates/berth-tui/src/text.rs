//! A text editor of several lines, light like nano: what a Compose file or
//! another value of several lines is typed in. It takes the whole screen
//! while it is open. Ctrl+S keeps the text, Esc leaves it as it was when it
//! was opened.

use crate::event::{Code, Key};
use crate::screen::{draw_footer, draw_header};
use crate::theme;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Margin},
    style::Style,
    text::{Line, Span},
    widgets::{Clear, Paragraph},
};
use std::cell::Cell;

/// The keys the editor takes, for the line under it.
pub const KEYS: &str = "Ctrl+S keep · Esc discard · Tab indent · PgUp PgDn page";

/// How far a page up or down moves the cursor, in lines.
const PAGE: isize = 20;

/// What a key does to the editor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Still editing.
    Editing,
    /// Ctrl+S: keep the text.
    Keep,
    /// Esc: leave it as it was.
    Leave,
}

/// The lines of a text being edited, with the cursor and how far down the
/// text is shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextEditor {
    /// What is edited, for the panel's title: the field's label.
    label: String,
    lines: Vec<Vec<char>>,
    row: usize,
    col: usize,
    /// The first line and the first column shown, kept as the text is drawn
    /// so the cursor stays in view.
    top: Cell<usize>,
    left: Cell<usize>,
}

impl TextEditor {
    /// An editor of `text`, the value of the field `label`, with the cursor
    /// at its end.
    pub fn new(label: &str, text: &str) -> Self {
        let lines: Vec<Vec<char>> = text
            .replace("\r\n", "\n")
            .split('\n')
            .map(|line| line.chars().collect())
            .collect();
        let row = lines.len() - 1;
        let col = lines[row].len();
        TextEditor {
            label: label.to_owned(),
            lines,
            row,
            col,
            top: Cell::new(0),
            left: Cell::new(0),
        }
    }

    /// The text as it is now, its lines joined with line breaks.
    pub fn text(&self) -> String {
        self.lines
            .iter()
            .map(|line| line.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Type pasted or typed text: line breaks start new lines, and a tab is
    /// two spaces.
    pub fn paste(&mut self, text: &str) {
        for ch in text.replace("\r\n", "\n").chars() {
            match ch {
                '\n' | '\r' => self.enter(),
                '\t' => {
                    self.insert(' ');
                    self.insert(' ');
                }
                ch if ch.is_control() => {}
                ch => self.insert(ch),
            }
        }
    }

    fn insert(&mut self, ch: char) {
        self.lines[self.row].insert(self.col, ch);
        self.col += 1;
    }

    fn enter(&mut self) {
        let rest = self.lines[self.row].split_off(self.col);
        self.row += 1;
        self.lines.insert(self.row, rest);
        self.col = 0;
    }

    fn move_row(&mut self, step: isize) {
        let last = (self.lines.len() - 1) as isize;
        self.row = (self.row as isize + step).clamp(0, last) as usize;
        self.col = self.col.min(self.lines[self.row].len());
    }

    /// What a key does to the text: typing, moving, and whether to keep it.
    pub fn handle(&mut self, key: &Key) -> Outcome {
        if key.is_ctrl('s') {
            return Outcome::Keep;
        }
        if key.code == Code::Esc {
            return Outcome::Leave;
        }
        let length = self.lines[self.row].len();
        match key.code {
            Code::Char('a') if key.ctrl => self.col = 0,
            Code::Char('e') if key.ctrl => self.col = length,
            Code::Char(ch) if !key.ctrl && !key.alt => self.insert(ch),
            Code::Enter => self.enter(),
            Code::Tab => self.paste("\t"),
            Code::Backspace => {
                if self.col > 0 {
                    self.col -= 1;
                    self.lines[self.row].remove(self.col);
                } else if self.row > 0 {
                    let line = self.lines.remove(self.row);
                    self.row -= 1;
                    self.col = self.lines[self.row].len();
                    self.lines[self.row].extend(line);
                }
            }
            Code::Delete => {
                if self.col < length {
                    self.lines[self.row].remove(self.col);
                } else if self.row + 1 < self.lines.len() {
                    let next = self.lines.remove(self.row + 1);
                    self.lines[self.row].extend(next);
                }
            }
            Code::Left => {
                if self.col > 0 {
                    self.col -= 1;
                } else if self.row > 0 {
                    self.row -= 1;
                    self.col = self.lines[self.row].len();
                }
            }
            Code::Right => {
                if self.col < length {
                    self.col += 1;
                } else if self.row + 1 < self.lines.len() {
                    self.row += 1;
                    self.col = 0;
                }
            }
            Code::Up => self.move_row(-1),
            Code::Down => self.move_row(1),
            Code::PageUp => self.move_row(-PAGE),
            Code::PageDown => self.move_row(PAGE),
            Code::Home => self.col = 0,
            Code::End => self.col = length,
            _ => {}
        }
        Outcome::Editing
    }

    /// Draw the editor over the whole screen: the top line with `context`,
    /// what is open, then the numbered lines in a panel, then the keys.
    pub fn draw(&self, frame: &mut Frame, context: &str, status: &str) {
        frame.render_widget(Clear, frame.area());
        frame.render_widget(theme::backdrop(), frame.area());
        let area = frame.area().inner(Margin {
            horizontal: 2,
            vertical: 1,
        });
        let [header, _, body, _, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(area);
        draw_header(frame, header, &[], 0, context);
        let place = format!(
            "{} · line {} of {}, column {}",
            self.label,
            self.row + 1,
            self.lines.len(),
            self.col + 1
        );
        let block = theme::panel(&place, true);
        let inner = block.inner(body);
        frame.render_widget(block, body);
        draw_footer(frame, footer, KEYS, status, theme::ACCENT);
        let gutter = self.lines.len().to_string().len().max(2) + 1;
        let (height, width) = (
            inner.height as usize,
            (inner.width as usize).saturating_sub(gutter),
        );
        if height == 0 || width == 0 {
            return;
        }
        // Both ways the text moves only as far as keeps the cursor in view.
        let top = self
            .top
            .get()
            .clamp((self.row + 1).saturating_sub(height), self.row);
        self.top.set(top);
        let left = self
            .left
            .get()
            .clamp((self.col + 1).saturating_sub(width), self.col);
        self.left.set(left);
        let muted = Style::new().fg(theme::MUTED);
        let end = (top + height).min(self.lines.len());
        let rows: Vec<Line<'static>> = (top..end)
            .map(|index| {
                let text: String = self.lines[index].iter().skip(left).take(width).collect();
                Line::from(vec![
                    Span::styled(format!("{:>1$} ", index + 1, gutter - 1), muted),
                    Span::styled(text, Style::new().fg(theme::TEXT)),
                ])
            })
            .collect();
        frame.render_widget(Paragraph::new(rows), inner);
        let before: String = self.lines[self.row][left..self.col].iter().collect();
        let column = (gutter + Span::raw(before).width()).min(inner.width as usize - 1);
        frame.set_cursor_position((inner.x + column as u16, inner.y + (self.row - top) as u16));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(editor: &mut TextEditor, code: Code) -> Outcome {
        editor.handle(&Key::plain(code))
    }

    fn type_text(editor: &mut TextEditor, text: &str) {
        for ch in text.chars() {
            press(editor, Code::Char(ch));
        }
    }

    #[test]
    fn typing_and_enter_make_lines() {
        let mut editor = TextEditor::new("Text", "");
        type_text(&mut editor, "ab");
        press(&mut editor, Code::Enter);
        type_text(&mut editor, "c");
        assert_eq!(editor.text(), "ab\nc");
    }

    #[test]
    fn backspace_at_a_line_start_joins_the_lines() {
        let mut editor = TextEditor::new("Text", "ab\ncd");
        press(&mut editor, Code::Home);
        press(&mut editor, Code::Backspace);
        assert_eq!(editor.text(), "abcd");
    }

    #[test]
    fn delete_at_a_line_end_joins_the_next_line() {
        let mut editor = TextEditor::new("Text", "ab\ncd");
        press(&mut editor, Code::Up);
        press(&mut editor, Code::End);
        press(&mut editor, Code::Delete);
        assert_eq!(editor.text(), "abcd");
    }

    #[test]
    fn a_paste_keeps_its_lines_and_turns_tabs_into_spaces() {
        let mut editor = TextEditor::new("Text", "");
        editor.paste("services:\r\n\tweb:\n\t\timage: nginx");
        assert_eq!(editor.text(), "services:\n  web:\n    image: nginx");
    }

    #[test]
    fn ctrl_s_keeps_and_esc_leaves() {
        let mut editor = TextEditor::new("Text", "x");
        assert_eq!(editor.handle(&Key::ctrl('s')), Outcome::Keep);
        assert_eq!(press(&mut editor, Code::Esc), Outcome::Leave);
        assert_eq!(press(&mut editor, Code::Char('y')), Outcome::Editing);
    }

    #[test]
    fn the_cursor_stays_on_the_text_when_moving_between_lines() {
        let mut editor = TextEditor::new("Text", "long line\nab");
        press(&mut editor, Code::Up);
        press(&mut editor, Code::End);
        press(&mut editor, Code::Down);
        type_text(&mut editor, "!");
        assert_eq!(editor.text(), "long line\nab!");
    }
}
