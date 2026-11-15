//! What a command prints: results on standard output, as an aligned table
//! or sentence for people and the API's own JSON with `-o json`, and what
//! goes on, hints and warnings on standard error.

use super::args::{Colour, Global, Output};
use super::fail::Fail;
use berth_tui::theme::{self, Depth, Role};
use serde_json::Value;
use std::io::{IsTerminal, Write};
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy)]
pub struct Out {
    pub json: bool,
    pub quiet: bool,
    /// Colours on standard output and on standard error.
    pub colour: (bool, bool),
    /// How many colours the terminal shows, when there are any.
    pub depth: Depth,
    /// Whether each of the two is a terminal.
    pub terminal: (bool, bool),
    /// Whether someone can be asked: a terminal, and no --no-input.
    pub ask: bool,
    pub yes: bool,
    /// `--sudo-password-stdin`: a host's sudo password is read from standard input.
    pub sudo_stdin: bool,
}

static OUT: OnceLock<Out> = OnceLock::new();

/// Set how this run prints, from its flags and its terminal.
pub fn init(global: &Global) {
    let terminal = (
        std::io::stdout().is_terminal(),
        std::io::stderr().is_terminal(),
    );
    let no_colour = std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty());
    let colour = match global.color {
        Colour::Always => (true, true),
        Colour::Never => (false, false),
        Colour::Auto if no_colour => (false, false),
        Colour::Auto => terminal,
    };
    let depth = Depth::of_terminal(|name| std::env::var(name).ok());
    let colour = match depth {
        // A dumb terminal shows the escapes as text, unless asked for.
        Depth::None if global.color == Colour::Auto => (false, false),
        _ => colour,
    };
    let out = Out {
        json: global.output == Output::Json,
        quiet: global.quiet,
        colour,
        depth: match depth {
            Depth::None => Depth::Basic,
            depth => depth,
        },
        terminal,
        ask: !global.no_input && std::io::stdin().is_terminal() && terminal.1,
        yes: global.yes,
        sudo_stdin: global.sudo_password_stdin,
    };
    let _ = OUT.set(out);
}

pub fn get() -> Out {
    *OUT.get_or_init(|| Out {
        json: false,
        quiet: false,
        colour: (false, false),
        depth: Depth::Basic,
        terminal: (false, false),
        ask: false,
        yes: false,
        sudo_stdin: false,
    })
}

/// A piece of text in a style, when the stream it goes to takes colours.
#[derive(Clone, Copy)]
pub enum Style {
    Good,
    Bad,
    Warn,
    Dim,
    Bold,
    Accent,
}

fn paint(text: &str, style: Style, on: bool) -> String {
    if !on || text.is_empty() {
        return text.to_owned();
    }
    let code = match style {
        Style::Bold => "1".into(),
        Style::Good => theme::sgr(Role::Good, get().depth),
        Style::Bad => theme::sgr(Role::Bad, get().depth),
        Style::Warn => theme::sgr(Role::Warn, get().depth),
        Style::Dim => theme::sgr(Role::Muted, get().depth),
        Style::Accent => theme::sgr(Role::Accent, get().depth),
    };
    format!("\x1b[{code}m{text}\x1b[0m")
}

/// A style of ratatui in `role`'s colour, for what the command line draws
/// with it on standard error: prompts and progress. Plain without colours.
pub fn look(role: Role) -> ratatui::style::Style {
    let out = get();
    match out.colour.1 {
        true => theme::style(role, out.depth),
        false if role == Role::Muted => ratatui::style::Style::new().dim(),
        false => ratatui::style::Style::new(),
    }
}

/// `text` styled for standard output.
pub fn styled(text: &str, style: Style) -> String {
    paint(text, style, get().colour.0)
}

/// `text` styled for standard error.
pub fn styled_err(text: &str, style: Style) -> String {
    paint(text, style, get().colour.1)
}

/// A result for people, unless only JSON is wanted.
pub fn line(text: impl AsRef<str>) {
    if !get().json {
        println!("{}", text.as_ref());
    }
}

/// A document of the API as the result.
pub fn json(value: &Value) {
    let text = serde_json::to_string_pretty(value).unwrap_or_default();
    println!("{text}");
}

/// The result of a command: its JSON with `-o json`, else what `people`
/// prints.
pub fn data(value: &Value, people: impl FnOnce()) {
    match get().json {
        true => json(value),
        false => people(),
    }
}

/// That something was done: a sentence on standard output, which `-q` and
/// `-o json` leave out.
pub fn done(text: impl AsRef<str>) {
    let out = get();
    if !out.quiet && !out.json {
        println!("{} {}", styled("✓", Style::Good), text.as_ref());
    }
}

/// What to do next, on standard error.
pub fn hint(text: impl AsRef<str>) {
    let out = get();
    if !out.quiet && !out.json {
        eprintln!("{}", styled_err(text.as_ref(), Style::Dim));
    }
}

/// What goes on, on standard error.
pub fn note(text: impl AsRef<str>) {
    if !get().quiet {
        eprintln!("{}", text.as_ref());
    }
}

/// Something to look out for, on standard error even with `-q`.
pub fn warn(text: impl AsRef<str>) {
    eprintln!("{} {}", styled_err("!", Style::Warn), text.as_ref());
}

/// Why the command failed, on standard error: a sentence, or with `-o json`
/// an object with the API's error code.
pub fn fail(fail: &Fail) {
    if fail.message.is_empty() {
        return;
    }
    if get().json {
        let body = serde_json::json!({"error": {"code": fail.code, "message": fail.message, "exit": fail.status}});
        eprintln!("{body}");
        return;
    }
    eprintln!("{} {}", styled_err("berth:", Style::Bad), fail.message);
}

/// The width of text as it is shown, without its colours.
pub fn width(text: &str) -> usize {
    strip_ansi(text).chars().count()
}

/// Text without the escape sequences of colours and cursor movements.
pub fn strip_ansi(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            plain.push(ch);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                // Up to the final byte of the sequence.
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            Some(']') => {
                // An operating system command ends with BEL or ESC \.
                while let Some(next) = chars.next() {
                    if next == '\x07' || (next == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                        break;
                    }
                }
            }
            _ => {
                chars.next();
            }
        }
    }
    plain
}

/// A table for people: a header and rows, in columns as wide as their
/// widest cell.
pub struct Table {
    header: Vec<String>,
    rows: Vec<Vec<String>>,
}

impl Table {
    pub fn new(header: &[&str]) -> Table {
        Table {
            header: header.iter().map(|cell| cell.to_uppercase()).collect(),
            rows: vec![],
        }
    }

    pub fn row(&mut self, cells: Vec<String>) {
        self.rows.push(cells);
    }

    /// The lines of the table, without a newline at the end of the last.
    pub fn render(&self, colour: bool) -> Vec<String> {
        let columns = self.header.len();
        let mut widths = vec![0; columns];
        for row in std::iter::once(&self.header).chain(&self.rows) {
            for (index, cell) in row.iter().enumerate().take(columns) {
                widths[index] = widths[index].max(width(cell));
            }
        }
        let line = |row: &[String]| {
            let mut text = String::new();
            for (index, cell) in row.iter().enumerate().take(columns) {
                text.push_str(cell);
                if index + 1 < columns {
                    let pad = widths[index] - width(cell) + 3;
                    text.push_str(&" ".repeat(pad));
                }
            }
            text.trim_end().to_owned()
        };
        let mut lines = vec![paint(&line(&self.header), Style::Dim, colour)];
        lines.extend(self.rows.iter().map(|row| line(row)));
        lines
    }

    /// Print it, or `empty` when it has no rows.
    pub fn print(&self, empty: &str) {
        if self.rows.is_empty() {
            hint(empty);
            return;
        }
        let mut stdout = std::io::stdout().lock();
        for line in self.render(get().colour.0) {
            let _ = writeln!(stdout, "{line}");
        }
    }
}

/// Name and value pairs, as `berth host show` prints them.
pub fn fields(pairs: &[(&str, String)]) {
    let wide = pairs.iter().map(|(name, _)| name.len()).max().unwrap_or(0);
    for (name, value) in pairs {
        if value.is_empty() {
            continue;
        }
        println!(
            "{}{}  {value}",
            styled(name, Style::Dim),
            " ".repeat(wide - name.len())
        );
    }
}

/// Put the cursor at the start of an inline viewport that was cleared, so
/// that what is printed next takes its place.
pub fn to_line_of<B: ratatui::backend::Backend>(terminal: &mut ratatui::Terminal<B>) {
    let top = terminal.get_frame().area().y;
    let _ = terminal.set_cursor_position((0, top));
    let _ = terminal.show_cursor();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_table_lines_up_its_columns_whatever_their_colours() {
        let mut table = Table::new(&["name", "address", "state"]);
        table.row(vec![
            "studio".into(),
            "192.0.2.10".into(),
            "\x1b[32manswers\x1b[0m".into(),
        ]);
        table.row(vec!["dock".into(), "".into(), "no address".into()]);
        let lines = table.render(false);
        assert_eq!(
            lines,
            [
                "NAME     ADDRESS      STATE",
                "studio   192.0.2.10   \x1b[32manswers\x1b[0m",
                "dock                  no address",
            ]
        );
    }

    #[test]
    fn colours_and_titles_are_taken_out() {
        assert_eq!(strip_ansi("\x1b[1;32mok\x1b[0m done"), "ok done");
        assert_eq!(strip_ansi("\x1b]0;title\x07text"), "text");
        assert_eq!(width("\x1b[31m✗\x1b[0m failed"), 8);
    }
}
