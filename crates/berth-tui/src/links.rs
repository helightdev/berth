//! The lines of a terminal's screen, for finding web addresses that run over
//! several rows. A terminal marks the rows it wrapped itself, but programs
//! that draw their own screens, such as the coding agents, break a long link
//! with hard line ends or put each row in place with cursor moves, so a link
//! that fills its row and goes on at the start of the next is joined too.

use crate::screen::with_links;

/// One cell of a row: its column, how many columns it takes, and its text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub col: u16,
    pub width: u16,
    pub text: String,
}

/// A line of the screen: its text, and the (column, row) of each character.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Line {
    pub text: String,
    pub places: Vec<(u16, u16)>,
}

/// Columns a link may stop short of the edge of its row and still be taken
/// as broken there: programs leave a column or two of padding.
const SLACK: u16 = 2;

/// The cells of the top `rows` rows and left `cols` columns of a screen, and
/// whether the terminal wrapped each row.
pub fn grid(screen: &vt100::Screen, rows: u16, cols: u16) -> (Vec<Vec<Cell>>, Vec<bool>) {
    let mut grid = vec![];
    for row in 0..rows {
        let mut cells = vec![];
        for col in 0..cols {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            cells.push(Cell {
                col,
                width: if cell.is_wide() { 2 } else { 1 },
                text: match cell.has_contents() {
                    true => cell.contents().to_owned(),
                    false => " ".to_owned(),
                },
            });
        }
        grid.push(cells);
    }
    let wrapped = (0..rows).map(|row| screen.row_wrapped(row)).collect();
    (grid, wrapped)
}

/// The text of a screen, with the rows of one line joined.
pub fn screen_lines(screen: &vt100::Screen) -> Vec<String> {
    let (rows, cols) = screen.size();
    let (grid, wrapped) = grid(screen, rows, cols);
    lines(&grid, &wrapped, cols)
        .into_iter()
        .map(|line| line.text)
        .collect()
}

/// A side of a box that a program draws around its text.
fn border(text: &str) -> bool {
    matches!(
        text,
        "│" | "┃" | "║" | "╎" | "╏" | "┆" | "┇" | "┊" | "┋" | "▏" | "▕" | "▌" | "▐"
    )
}

fn blank(cell: &Cell) -> bool {
    cell.text.trim().is_empty()
}

/// Where the text of a row is, between the spaces and the sides of a box
/// around it: the index of its first and last cells, and the column of its
/// left side, if it has one.
fn content(cells: &[Cell]) -> Option<(usize, usize, Option<u16>)> {
    let mut first = cells.iter().position(|cell| !blank(cell))?;
    let mut left = None;
    if border(&cells[first].text) {
        left = Some(cells[first].col);
        first += 1 + cells[first + 1..].iter().position(|cell| !blank(cell))?;
    }
    let mut last = cells.iter().rposition(|cell| !blank(cell))?;
    if border(&cells[last].text) && last > first {
        last = cells[..last].iter().rposition(|cell| !blank(cell))?;
    }
    (last >= first).then_some((first, last, left))
}

/// Whether a character can be part of a web address, as `with_links` reads one.
fn in_address(ch: char) -> bool {
    ch.is_ascii_graphic() && !"()<>\"'".contains(ch)
}

/// Whether `text`, the line so far, ends in a web address that may go on.
fn ends_in_link(text: &str) -> bool {
    let parts = with_links(text);
    let [.., (link, true), (rest, false)] = parts.as_slice() else {
        return false;
    };
    // An address that was shortened goes on nowhere; one that ends in a
    // full stop may go on, as the stop can be part of it.
    !link.is_empty() && rest.chars().all(|ch| ".,;:".contains(ch))
}

/// The column the last word of `cells` starts at.
fn last_word(cells: &[Cell]) -> u16 {
    let at = cells
        .iter()
        .rposition(|cell| !cell.text.chars().all(in_address))
        .map_or(0, |index| index + 1);
    cells.get(at).map_or(0, |cell| cell.col)
}

/// Where the next row goes on with the address that `row` ends in, if it
/// does: the index of the cell of `next` that the address resumes at.
///
/// It does when the address reaches the edge of its row, or of the box it is
/// in, and the next row, with the same indentation and box, starts with
/// characters of an address. That next part is not taken when it starts an
/// address of its own, is a rule of dashes or a shell prompt, or is followed
/// by more text without having anything only an address has.
fn resumes(row: &[Cell], from: usize, next: &[Cell], cols: u16) -> Option<usize> {
    let (first, last, left) = content(row)?;
    // A row that goes on with a link is read from where the link resumed.
    let first = first.max(from);
    if first > last {
        return None;
    }
    // The edge is the right side of the box, or the end of the screen.
    let edge = match row[last + 1..].iter().find(|cell| !blank(cell)) {
        Some(side) => side.col,
        None => cols,
    };
    let end = row[last].col + row[last].width;
    if edge.saturating_sub(end) > SLACK {
        return None;
    }
    let (start, next_last, next_left) = content(next)?;
    if next_left != left {
        return None;
    }
    // A program that breaks a line goes on at its left margin, or under the
    // start of the address.
    let column = next[start].col;
    if column < row[first].col || column > last_word(&row[first..=last]) {
        return None;
    }
    let part: String = next[start..=next_last]
        .iter()
        .map(|cell| cell.text.as_str())
        .collect::<String>()
        .chars()
        .take_while(|ch| in_address(*ch))
        .collect();
    if part.is_empty() || part.starts_with("https://") || part.starts_with("http://") {
        return None;
    }
    let mut chars = part.chars();
    let lead = chars.next()?;
    if lead.is_ascii_punctuation() && chars.all(|ch| ch == lead) {
        return None;
    }
    let alone = part.chars().count()
        == next[start..=next_last]
            .iter()
            .map(|cell| cell.text.chars().count())
            .sum::<usize>();
    // A prompt ends in one of these; an address seldom does, except for a
    // `%` cut off from the two digits after it.
    let prompt =
        part.ends_with(['$', '#']) || (part.ends_with('%') && part.matches('%').count() == 1);
    if alone && prompt {
        return None;
    }
    if !alone && !part.contains(['/', '?', '#', '&', '=', '%']) {
        return None;
    }
    Some(start)
}

/// The lines of a screen's rows: rows the terminal wrapped, and rows a link
/// was broken over, are one line. `cols` is the width the program draws for.
pub fn lines(grid: &[Vec<Cell>], wrapped: &[bool], cols: u16) -> Vec<Line> {
    let mut lines = vec![];
    let mut line = Line::default();
    // The cell the row is read from: past the indentation of a link's next part.
    let mut from = 0;
    for (row, cells) in grid.iter().enumerate() {
        let from_here = from.min(cells.len());
        // Where the row is read to, and the cell the next row is read from.
        let join = match grid.get(row + 1) {
            None => None,
            Some(_) if wrapped.get(row).copied().unwrap_or(false) => Some((cells.len(), 0)),
            Some(next) => {
                let mut text = line.text.clone();
                text.extend(cells[from_here..].iter().map(|cell| cell.text.as_str()));
                let text =
                    text.trim_end_matches(|ch: char| ch.is_whitespace() || border(&ch.to_string()));
                match ends_in_link(text) {
                    true => resumes(cells, from_here, next, cols).map(|start| {
                        let to = content(cells).map_or(cells.len(), |(_, last, _)| last + 1);
                        (to, start)
                    }),
                    false => None,
                }
            }
        };
        let to = join.map_or(cells.len(), |(to, _)| to);
        for cell in &cells[from_here.min(to)..to] {
            for ch in cell.text.chars() {
                line.text.push(ch);
                line.places.push((cell.col, row as u16));
            }
        }
        match join {
            Some((_, start)) => from = start,
            None => {
                from = 0;
                lines.push(std::mem::take(&mut line));
            }
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        event::{Click, Event, Mouse},
        screen::{Platform, draw_panel, spot_event},
    };
    use ratatui::{Terminal, backend::TestBackend};

    /// A sign-in link of the length the coding agents print.
    fn login() -> String {
        format!(
            "https://auth.example.com/oauth/authorize?code=true&client_id={}&redirect_uri=https%3A%2F%2Fauth.example.com%2Fcallback&scope=user%3Aprofile&state={}",
            "9d1c250a-e61b-44d9-88ed-5944d1962f5e", "Xy7aB2kQ-pL0_wR4tN8vM1sD6fG3hJ5zC9bV"
        )
    }

    fn screen(rows: u16, cols: u16, bytes: &[u8]) -> vt100::Parser {
        let mut parser = vt100::Parser::new(rows, cols, 0);
        parser.process(bytes);
        parser
    }

    /// The addresses found on a screen, each once.
    fn links(parser: &vt100::Parser) -> Vec<String> {
        let mut found = vec![];
        for line in screen_lines(parser.screen()) {
            for (part, link) in with_links(&line) {
                if link && !found.contains(&part) {
                    found.push(part);
                }
            }
        }
        found
    }

    /// `text` in pieces of `width` characters.
    fn pieces(text: &str, width: usize) -> Vec<String> {
        text.chars()
            .collect::<Vec<_>>()
            .chunks(width)
            .map(|chunk| chunk.iter().collect())
            .collect()
    }

    #[test]
    fn rows_the_terminal_wrapped_are_one_line() {
        let parser = screen(8, 40, format!("Open {} now\r\n", login()).as_bytes());
        assert!(parser.screen().row_wrapped(0));
        assert_eq!(links(&parser), [login()]);
    }

    #[test]
    fn a_wrapped_link_is_still_one_in_the_screen_a_viewer_is_sent() {
        // A viewer that opens a running session gets the host's screen as a
        // redraw of it, which keeps the rows that were wrapped.
        let host = screen(8, 40, format!("Open {} now\r\n", login()).as_bytes());
        let viewer = screen(8, 40, &host.screen().state_formatted());
        assert!(viewer.screen().row_wrapped(0));
        assert_eq!(links(&viewer), [login()]);
    }

    #[test]
    fn a_link_broken_with_line_ends_is_joined() {
        // As a program that wraps its own text writes it: every row ends with
        // a line end, so the terminal wrapped none of them.
        let mut bytes = b"Use the url below to sign in:\r\n\r\n".to_vec();
        for piece in pieces(&login(), 50) {
            bytes.extend(format!("{piece}\r\n").as_bytes());
        }
        bytes.extend(b"\r\nPaste code here if prompted > ");
        let parser = screen(12, 50, &bytes);
        assert!((0..12).all(|row| !parser.screen().row_wrapped(row)));
        assert_eq!(links(&parser), [login()]);
    }

    #[test]
    fn a_link_drawn_row_by_row_with_cursor_moves_is_joined() {
        // As a screen redrawn row by row, each put in place with a cursor
        // move, indented and with a column of padding at the right.
        let mut bytes = b"\x1b[2J\x1b[1;3HSign in at:".to_vec();
        for (index, piece) in pieces(&login(), 45).iter().enumerate() {
            bytes.extend(format!("\x1b[{};3H{piece}", index + 3).as_bytes());
        }
        bytes.extend(b"\x1b[10;3HPress Enter to go on");
        let parser = screen(10, 48, &bytes);
        assert_eq!(links(&parser), [login()]);
        // Its label and what comes after stay out of it.
        let lines = screen_lines(parser.screen());
        assert!(lines.iter().any(|line| line.trim() == "Sign in at:"));
        assert!(
            lines
                .iter()
                .any(|line| line.trim() == "Press Enter to go on")
        );
    }

    #[test]
    fn a_link_in_a_box_is_joined_without_the_sides() {
        let width = 40;
        let mut bytes = format!("╭{}╮\r\n", "─".repeat(width)).into_bytes();
        let mut rows = vec!["Log in with this link:".to_owned()];
        rows.extend(pieces(&login(), width - 2));
        for row in rows {
            bytes.extend(format!("│ {row:<w$} │\r\n", w = width - 2).as_bytes());
        }
        bytes.extend(format!("╰{}╯", "─".repeat(width)).as_bytes());
        let parser = screen(12, width as u16 + 2, &bytes);
        assert_eq!(links(&parser), [login()]);
    }

    #[test]
    fn a_link_after_a_label_goes_on_at_the_margin() {
        let first = 50 - "  Open: ".len();
        let link = login();
        let mut bytes = format!("  Open: {}\r\n", &link[..first]).into_bytes();
        for piece in pieces(&link[first..], 48) {
            bytes.extend(format!("  {piece}\r\n").as_bytes());
        }
        let parser = screen(10, 50, &bytes);
        assert_eq!(links(&parser), [link]);
    }

    #[test]
    fn a_full_stop_at_the_break_stays_in_the_link() {
        let parser = screen(4, 25, b"https://studio.example/a.\r\nb/c\r\n");
        assert_eq!(links(&parser), ["https://studio.example/a.b/c"]);
    }

    #[test]
    fn rows_after_a_link_that_are_not_part_of_it_stay_apart() {
        let link = "https://studio.example/abcdefg";
        let edge = |next: &str| {
            let parser = screen(4, 30, format!("{link}\r\n{next}\r\n").as_bytes());
            links(&parser)
        };
        // Words of a sentence.
        assert_eq!(edge("Then press Enter"), [link]);
        // A link of its own.
        let other = "https://other.example/abcdefgh";
        assert_eq!(edge(other), [link, other]);
        // A shell prompt, a rule, a list.
        assert_eq!(edge("ops@studio:~$ "), [link]);
        assert_eq!(edge("------------------------------"), [link]);
        assert_eq!(edge("- the next item"), [link]);
        // Further in than where the link starts, or in a box of its own.
        assert_eq!(edge("    abc"), [link]);
        assert_eq!(edge("│ abc"), [link]);
        // A link that ends short of the edge ends there.
        let parser = screen(4, 34, format!("{link}\r\nabc\r\n").as_bytes());
        assert_eq!(links(&parser), [link]);
        // Plain text is never joined.
        let parser = screen(4, 30, b"no address here at the edge xy\r\nabc\r\n");
        assert_eq!(
            screen_lines(parser.screen())[..2]
                .iter()
                .map(|line| line.trim_end())
                .collect::<Vec<_>>(),
            ["no address here at the edge xy", "abc"]
        );
    }

    #[test]
    fn a_link_may_stop_a_column_or_two_short_of_the_edge() {
        let link = "https://studio.example/abcdefg";
        for (cols, joined) in [(31, true), (32, true), (33, false)] {
            let parser = screen(4, cols, format!("{link}\r\nhij/k\r\n").as_bytes());
            let expected = match joined {
                true => vec![format!("{link}hij/k")],
                false => vec![link.to_owned()],
            };
            assert_eq!(links(&parser), expected, "{cols} columns");
        }
    }

    #[test]
    fn the_end_of_a_link_may_have_text_after_it_when_it_looks_like_an_address() {
        let link = "https://studio.example/abcdefg";
        let parser = screen(
            4,
            30,
            format!("{link}\r\nh?state=x1 to sign in\r\n").as_bytes(),
        );
        assert_eq!(links(&parser), [format!("{link}h?state=x1")]);
    }

    #[derive(Default)]
    struct Desk {
        copied: Vec<String>,
        opened: Vec<String>,
    }

    impl Platform for Desk {
        fn copy(&mut self, text: &str) -> bool {
            self.copied.push(text.to_owned());
            true
        }
        fn paste(&mut self) -> Option<String> {
            None
        }
        fn open(&mut self, link: &str) -> Result<(), String> {
            self.opened.push(link.to_owned());
            Ok(())
        }
    }

    #[test]
    fn the_panel_opens_and_copies_a_broken_link_whole() {
        let mut bytes = vec![];
        for (index, piece) in pieces(&login(), 40).iter().enumerate() {
            bytes.extend(format!("\x1b[{};1H{piece}", index + 1).as_bytes());
        }
        let parser = screen(8, 40, &bytes);
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        let (mut links, mut words) = (vec![], vec![]);
        terminal
            .draw(|frame| {
                (links, words) = draw_panel(frame, "demo", parser.screen(), None, " Esc close ", "")
            })
            .unwrap();
        assert_eq!(links.len(), login().len());
        assert!(links.iter().all(|(_, link)| *link == login()));
        // The QR code is offered for it.
        let shown = terminal.backend().buffer().clone();
        let drawn: String = (0..12)
            .flat_map(|y| (0..60).map(move |x| (x, y)))
            .map(|place| shown[place].symbol().to_owned())
            .collect();
        assert!(drawn.contains("QR code"), "{drawn}");
        // A click on its last row opens it, and a right-click there copies it.
        let (rect, _) = links.last().unwrap();
        let mut desk = Desk::default();
        for click in [Click::Left, Click::Right] {
            let event = Event::Mouse(Mouse {
                click,
                column: rect.x,
                row: rect.y,
            });
            spot_event(&links, &words, &event, &mut desk);
        }
        assert_eq!(desk.opened, [login()]);
        assert_eq!(desk.copied, [login()]);
    }
}
