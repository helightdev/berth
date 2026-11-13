//! Drawing what every screen has: the top and bottom lines, dialogs, the
//! terminal panel; and the keys and clicks of the panel and the dialogs.

use crate::{
    event::{Click, Code, Event, Key, edit, paste},
    host::{HostView, View, argument_label},
    theme,
};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
};
use tui_input::Input;

/// Places on screen with a text: web addresses, or words to copy.
pub type Spots = Vec<(Rect, String)>;

pub fn inside(rect: Rect, x: u16, y: u16) -> bool {
    x >= rect.x && x < rect.right() && y >= rect.y && y < rect.bottom()
}

pub fn ellipsize(text: &str, width: usize) -> String {
    let text = text.replace(['\n', '\r'], " ");
    if Span::raw(&text).width() <= width {
        return text;
    }
    if width == 0 {
        return String::new();
    }
    let mut result = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let columns = Span::raw(ch.to_string()).width();
        if used + columns >= width {
            break;
        }
        result.push(ch);
        used += columns;
    }
    result.push('…');
    result
}

/// Break text into lines of at most `width` columns, at spaces.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = vec![String::new()];
    for word in text.split_whitespace() {
        let line = lines.last_mut().expect("one line");
        if !line.is_empty()
            && Span::raw(line.as_str()).width() + 1 + Span::raw(word).width() > width
        {
            lines.push(word.to_owned());
        } else {
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
    }
    lines
}

/// The top line: the name, the tabs and what is open. Returns the tabs' areas.
pub fn draw_header(
    frame: &mut Frame,
    area: Rect,
    tabs: &[&str],
    current: usize,
    context: &str,
) -> Vec<Rect> {
    let mut spans = vec![
        Span::styled("▐", Style::new().fg(theme::STEEL)),
        Span::styled("▄", Style::new().fg(theme::ACCENT)),
        Span::styled("▌", Style::new().fg(theme::STEEL)),
        Span::styled(" berth  ", Style::new().fg(theme::TEXT).bold()),
    ];
    let mut x = area.x + 11;
    let mut areas = vec![];
    for (index, tab) in tabs.iter().enumerate() {
        let width = tab.len() as u16 + 4;
        areas.push(Rect::new(
            x,
            area.y,
            width.min(area.right().saturating_sub(x)),
            1,
        ));
        x += width + 1;
        if index == current {
            spans.push(Span::styled(
                format!(" {} {tab} ", index + 1),
                theme::plate(),
            ));
        } else {
            spans.push(Span::styled(
                format!(" {} ", index + 1),
                Style::new().fg(theme::ACCENT),
            ));
            spans.push(Span::styled(
                format!("{tab} "),
                Style::new().fg(theme::MUTED),
            ));
        }
        spans.push(Span::raw(" "));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    let width = (context.chars().count() as u16).min(area.right().saturating_sub(x));
    frame.render_widget(
        Paragraph::new(context)
            .alignment(Alignment::Right)
            .style(Style::new().fg(theme::MUTED)),
        Rect::new(area.right() - width, area.y, width, 1),
    );
    areas
}

thread_local! {
    /// Where the key hints of the last frame are, with the key each stands for.
    static HINTS: std::cell::RefCell<Vec<(Rect, Key)>> = const { std::cell::RefCell::new(vec![]) };
}

/// The key a hint such as `Ctrl+V pastes` names, if it names one key.
pub fn hint_key(name: &str) -> Option<Key> {
    let (ctrl, alt, name) = match (name.strip_prefix("Ctrl+"), name.strip_prefix("Alt+")) {
        (Some(rest), _) => (true, false, rest),
        (_, Some(rest)) => (false, true, rest),
        _ => (false, false, name),
    };
    let code = match name {
        "Enter" => Code::Enter,
        "Esc" => Code::Esc,
        "Tab" => Code::Tab,
        "Space" => Code::Char(' '),
        "Left" => Code::Left,
        "Right" => Code::Right,
        _ => {
            let mut chars = name.chars();
            match (chars.next(), chars.as_str()) {
                (Some(ch), "") if ch.is_ascii_alphanumeric() => Code::Char(ch.to_ascii_lowercase()),
                (Some('F'), number) => Code::F(number.parse().ok()?),
                _ => return None,
            }
        }
    };
    Some(Key {
        ctrl,
        alt,
        ..Key::plain(code)
    })
}

/// Forget the hints of the last frame, before the next is drawn.
pub fn clear_hints() {
    HINTS.with_borrow_mut(Vec::clear);
}

/// Remember where the hints of `keys` are drawn from (x, y) on, so that a click
/// on one presses its key. A panel over the screen replaces the hints under it.
pub fn note_hints(keys: &str, mut x: u16, y: u16, modal: bool) {
    HINTS.with_borrow_mut(|hints| {
        if modal {
            hints.clear();
        }
        for hint in keys.split(" · ") {
            let width = Span::raw(hint).width() as u16;
            let name = hint.split(' ').next().unwrap_or(hint);
            if let Some(key) = hint_key(name)
                && !hint.ends_with('…')
            {
                hints.push((Rect::new(x, y, width, 1), key));
            }
            x += width + 2;
        }
    });
}

/// The key of the hint at a place on screen, if one is there.
pub fn hint_at(column: u16, row: u16) -> Option<Key> {
    HINTS.with_borrow(|hints| {
        hints
            .iter()
            .find(|(rect, _)| inside(*rect, column, row))
            .map(|(_, key)| *key)
    })
}

/// A click on a key hint is that key.
pub fn hint_click(event: Event) -> Event {
    if let Event::Mouse(mouse) = &event
        && mouse.click == Click::Left
        && let Some(key) = hint_at(mouse.column, mouse.row)
    {
        return Event::Key(key);
    }
    event
}

/// The bottom line: key hints on the left, the last message on the right.
pub fn draw_footer(frame: &mut Frame, area: Rect, keys: &str, status: &str, colour: Color) {
    let width = if status.is_empty() {
        0
    } else {
        (status.chars().count() as u16 + 2).min(area.width / 2)
    };
    let [hints, note] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(width)]).areas(area);
    let shown = ellipsize(keys, hints.width as usize);
    note_hints(&shown, hints.x, hints.y, false);
    frame.render_widget(Paragraph::new(theme::hints(&shown)), hints);
    frame.render_widget(
        Paragraph::new(ellipsize(status, note.width as usize))
            .alignment(Alignment::Right)
            .style(Style::new().fg(colour)),
        note,
    );
}

/// A small centred box over the rest of the screen. Returns its area.
pub fn dialog(frame: &mut Frame, title: &str, keys: &str, lines: Vec<Line>) -> Rect {
    let screen = frame.area();
    let width = 62.min(screen.width);
    let height = (lines.len() as u16 + 2).min(screen.height);
    let area = Rect::new(
        screen.x + (screen.width - width) / 2,
        screen.y + (screen.height - height) / 2,
        width,
        height,
    );
    note_hints(
        keys.trim(),
        area.x + 1,
        area.bottom().saturating_sub(1),
        true,
    );
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            theme::popup(title.trim(), theme::ACCENT).title_bottom(theme::hints(keys.trim())),
        ),
        area,
    );
    area
}

/// Split a line of output into text and the web addresses in it.
pub fn with_links(line: &str) -> Vec<(String, bool)> {
    let mut parts = vec![];
    let mut rest = line;
    while let Some(start) = [rest.find("https://"), rest.find("http://")]
        .into_iter()
        .flatten()
        .min()
    {
        let tail = &rest[start..];
        let end = tail
            .find(|ch: char| ch.is_whitespace() || "()<>\"'".contains(ch))
            .unwrap_or(tail.len());
        // Punctuation after an address belongs to the sentence.
        let link = tail[..end].trim_end_matches(['.', ',', ';', ':', '…']);
        parts.push((rest[..start].to_owned(), false));
        parts.push((link.to_owned(), true));
        rest = &tail[link.len()..];
    }
    parts.push((rest.to_owned(), false));
    parts
}

/// The words of one line of output and the column each starts at.
pub fn words(line: &str) -> Vec<(u16, String)> {
    let mut found = vec![];
    let (mut column, mut start, mut word) = (0u16, 0u16, String::new());
    for ch in line.chars().chain([' ']) {
        if ch.is_whitespace() {
            // Brackets and sentence punctuation around a value are not part of it.
            let trimmed = word
                .trim_start_matches(['(', '"', '\''])
                .trim_end_matches([')', '"', '\'', '.', ',', ';', ':']);
            if !trimmed.is_empty() {
                let lead = word.len() - word.trim_start_matches(['(', '"', '\'']).len();
                found.push((start + lead as u16, trimmed.to_owned()));
            }
            word.clear();
        } else {
            if word.is_empty() {
                start = column;
            }
            word.push(ch);
        }
        column += Span::raw(ch.to_string()).width() as u16;
    }
    found
}

/// Draw a session of the host: the same panel, with what leaving it means
/// there. `leave` is the key that leaves a running one: `Ctrl+Q`, or `Alt+Q`
/// where Ctrl+Q is not the page's to take.
pub fn draw_view(frame: &mut Frame, view: &View, note: &str, leave: &str) -> (Spots, Spots) {
    let running = match view.terminal {
        true => format!(
            " Ctrl+V paste · {leave} leave it running · Click opens a link · Right-click copies · Wheel scrolls "
        ),
        false => format!(
            " {leave} leave it running · Click opens a link · Right-click copies · Wheel scrolls "
        ),
    };
    let keys = match (view.outcome(), view.ours) {
        (None, _) => running.as_str(),
        (Some(true), true) => " ✓ Done · Enter close · k keep it in Sessions ",
        (Some(false), true) => " ✗ It did not finish · Enter close · k keep it in Sessions ",
        (Some(true), false) => " ✓ Done · Enter close · d forget it ",
        (Some(false), false) => " ✗ It did not finish · Enter close · d forget it ",
    };
    draw_panel(
        frame,
        &view.title,
        view.screen(),
        view.outcome(),
        keys,
        note,
    )
}

/// The terminal panel: a screen, in a box coloured by how the command fared.
pub fn draw_panel(
    frame: &mut Frame,
    title: &str,
    screen: &vt100::Screen,
    outcome: Option<bool>,
    keys: &str,
    note: &str,
) -> (Spots, Spots) {
    let (rows, cols) = screen.size();
    let colour = match outcome {
        None => theme::ACCENT,
        Some(true) => theme::GOOD,
        Some(false) => theme::BAD,
    };
    // A panel with a web address in it offers its QR code.
    let keys = match crate::qr::any_link(
        crate::links::screen_lines(screen)
            .iter()
            .map(String::as_str),
    ) {
        true => format!(" {} · {} ", keys.trim(), crate::qr::HINT),
        false => keys.to_owned(),
    };
    let full = frame.area();
    let (width, height) = ((cols + 4).min(full.width), (rows + 2).min(full.height));
    let area = Rect::new(
        full.x + (full.width - width) / 2,
        full.y + (full.height - height) / 2,
        width,
        height,
    );
    note_hints(
        keys.trim(),
        area.x + 1,
        area.bottom().saturating_sub(1),
        true,
    );
    let block = theme::popup(title, colour)
        .title_bottom(theme::hints(keys.trim()))
        .title_bottom(Line::styled(note.to_owned(), Style::new().fg(theme::WARN)).right_aligned());
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let colour_of = |colour: vt100::Color| match colour {
        vt100::Color::Default => None,
        vt100::Color::Idx(index) => Some(Color::Indexed(index)),
        vt100::Color::Rgb(r, g, b) => Some(Color::Rgb(r, g, b)),
    };
    let (mut links, mut copyable): (Spots, Spots) = (vec![], vec![]);
    let (shown_rows, shown_cols) = (rows.min(inner.height), cols.min(inner.width));
    for row in 0..shown_rows {
        let mut text = String::new();
        for col in 0..shown_cols {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            let symbol = if cell.has_contents() {
                cell.contents()
            } else {
                " "
            };
            let mut style = Style::new();
            if let Some(colour) = colour_of(cell.fgcolor()) {
                style = style.fg(colour);
            }
            if let Some(colour) = colour_of(cell.bgcolor()) {
                style = style.bg(colour);
            }
            for (on, modifier) in [
                (cell.bold(), Modifier::BOLD),
                (cell.dim(), Modifier::DIM),
                (cell.italic(), Modifier::ITALIC),
                (cell.underline(), Modifier::UNDERLINED),
                (cell.inverse(), Modifier::REVERSED),
            ] {
                if on {
                    style = style.add_modifier(modifier);
                }
            }
            if let Some(shown) = frame.buffer_mut().cell_mut((inner.x + col, inner.y + row)) {
                shown.set_symbol(symbol).set_style(style);
            }
            text.push_str(symbol);
        }
        for (column, word) in words(&text) {
            let width = Span::raw(word.as_str()).width() as u16;
            copyable.push((Rect::new(inner.x + column, inner.y + row, width, 1), word));
        }
    }
    // A long address runs over several rows, so the rows of one line are read
    // together, with the place on screen of each character kept.
    let (grid, wrapped) = crate::links::grid(screen, shown_rows, shown_cols);
    for line in crate::links::lines(&grid, &wrapped, cols) {
        let mut at = 0;
        for (part, link) in with_links(&line.text) {
            let count = part.chars().count();
            if link {
                for &(col, row) in &line.places[at..at + count] {
                    let (x, y) = (inner.x + col, inner.y + row);
                    if let Some(shown) = frame.buffer_mut().cell_mut((x, y)) {
                        shown.set_style(Style::new().fg(theme::ACCENT).underlined());
                    }
                    links.push((Rect::new(x, y, 1, 1), part.clone()));
                }
            }
            at += count;
        }
    }
    if outcome.is_none() && !screen.hide_cursor() {
        let (row, col) = screen.cursor_position();
        if row < inner.height && col < inner.width {
            frame.set_cursor_position((inner.x + col, inner.y + row));
        }
    }
    (links, copyable)
}

/// What the status bar says after a paste. Not the text: it may be a secret.
pub fn pasted(text: &str) -> String {
    match text.chars().count() {
        0 => "The clipboard is empty".into(),
        1 => "Pasted 1 character".into(),
        count => format!("Pasted {count} characters"),
    }
}

/// The yes-or-no question before something happens.
pub fn draw_question(frame: &mut Frame, question: &str) {
    let lines = wrap(question, 56).into_iter().map(Line::raw).collect();
    dialog(frame, " Are you sure? ", " y yes · n no ", lines);
}

/// The dialog that asks for the arguments of a task, one after another.
pub fn draw_arguments(frame: &mut Frame, host: &HostView) {
    let Some(asked) = &host.arguments else {
        return;
    };
    let Some(task) = host.tasks.get(asked.task) else {
        return;
    };
    let Some(argument) = task.arguments.get(asked.index) else {
        return;
    };
    let mut lines = vec![Line::raw(ellipsize(argument_label(argument), 56))];
    for (index, choice) in asked.choices.iter().enumerate().take(8) {
        let mark = if asked.chosen == Some(index) {
            "●"
        } else {
            "○"
        };
        lines.push(Line::styled(
            ellipsize(&format!("  {mark} {choice}"), 56),
            Style::new().fg(theme::MUTED),
        ));
    }
    lines.push(Line::raw(""));
    let typed = ellipsize(asked.input.value(), 52);
    lines.push(Line::from(vec![
        Span::styled("› ", Style::new().fg(theme::ACCENT)),
        Span::raw(typed.clone()),
    ]));
    let title = ellipsize(
        &format!(
            " {} · {} of {} ",
            task.label,
            asked.index + 1,
            task.arguments.len()
        ),
        58,
    );
    let keys = match asked.choices.is_empty() {
        true => " Enter accept · Esc cancel ",
        false => " ↑↓ choose · Enter accept · Esc cancel ",
    };
    let rows = lines.len() as u16;
    let area = dialog(frame, &title, keys, lines);
    if area.width > 6 && area.height > rows {
        let column = Span::raw(typed).width() as u16;
        frame.set_cursor_position((area.x + 4 + column, area.y + rows));
    }
}

/// What the screens need of where they run: the clipboard, and opening a link.
pub trait Platform {
    fn copy(&mut self, text: &str) -> bool;
    /// What is on the clipboard, where it can be read right away.
    fn paste(&mut self) -> Option<String>;
    fn open(&mut self, link: &str) -> Result<(), String>;
}

/// A click on a web address opens it; a right-click copies it, or the word
/// it is on when it is on no address.
/// Returns what to say when the click was one of these.
pub fn spot_event(
    links: &Spots,
    words: &Spots,
    event: &Event,
    platform: &mut dyn Platform,
) -> Option<String> {
    let Event::Mouse(mouse) = event else {
        return None;
    };
    let at = |spots: &Spots| {
        spots
            .iter()
            .find(|(rect, _)| inside(*rect, mouse.column, mouse.row))
            .map(|(_, text)| text.clone())
    };
    match mouse.click {
        // A link is copied whole, though its rows hold a word each.
        Click::Right => at(links)
            .or_else(|| at(words))
            .map(|word| match platform.copy(&word) {
                true => format!("Copied {}", ellipsize(&word, 40)),
                false => "Could not reach the clipboard".into(),
            }),
        Click::Left => at(links).map(|link| match platform.open(&link) {
            Ok(()) => String::new(),
            Err(error) => format!("Could not open the link: {error}"),
        }),
        _ => None,
    }
}

/// Keys and the mouse while a session of the host is in the panel. Returns
/// something for the status line, if there is something to say.
pub fn view_event(
    host: &mut HostView,
    links: &Spots,
    words: &Spots,
    event: &Event,
    platform: &mut dyn Platform,
) -> Option<String> {
    if let Some(said) = spot_event(links, words, event, platform) {
        return (!said.is_empty()).then_some(said);
    }
    let view = host.view.as_mut()?;
    match event {
        Event::Mouse(mouse) if mouse.click == Click::ScrollUp => view.scroll(3),
        Event::Mouse(mouse) if mouse.click == Click::ScrollDown => view.scroll(-3),
        Event::Paste(text) if view.terminal => {
            let bytes = view.paste(text);
            host.input(bytes);
            return Some(pasted(text));
        }
        Event::Key(key) => {
            if key.is_ctrl('q') || (key.alt && key.code == Code::Char('q')) {
                let running = view.running();
                host.close(false);
                return running.then(|| "It runs on; open it again from Sessions".into());
            } else if !view.running() {
                let (ours, title, outcome) = (view.ours, view.title.clone(), view.outcome());
                let forget = match key.code {
                    Code::Enter | Code::Esc | Code::Char('q') => ours,
                    Code::Char('k') if ours => false,
                    Code::Char('d') if !ours => true,
                    _ => return None,
                };
                host.close(forget);
                return Some(match outcome {
                    Some(true) => format!("{title} finished"),
                    _ => format!("{title} did not finish"),
                });
            } else if !view.terminal {
                // A transcript takes no typing.
            } else if key.is_ctrl('v') {
                // Terminals that keep Ctrl+V to themselves paste by their own means.
                return Some(match platform.paste() {
                    Some(text) => {
                        let bytes = view.paste(&text);
                        host.input(bytes);
                        pasted(&text)
                    }
                    None => "Could not read the clipboard".into(),
                });
            } else {
                let bytes = view.key(key);
                host.input(bytes);
            }
        }
        _ => {}
    }
    None
}

/// Keys of the dialog that asks for a task's arguments. Returns whether the
/// value was accepted, for the caller to go on with.
pub fn arguments_event(host: &mut HostView, event: &Event) -> bool {
    let Some(asked) = &mut host.arguments else {
        return false;
    };
    match event {
        Event::Key(key) => match key.code {
            Code::Esc => host.arguments = None,
            Code::Enter => return true,
            Code::Up | Code::Down if !asked.choices.is_empty() => {
                let count = asked.choices.len().min(8);
                let next = match (asked.chosen, key.code) {
                    (None, Code::Down) => 0,
                    (None, _) => count - 1,
                    (Some(index), Code::Down) => (index + 1) % count,
                    (Some(index), _) => (index + count - 1) % count,
                };
                asked.chosen = Some(next);
                asked.input = Input::new(asked.choices[next].clone());
            }
            _ => {
                asked.chosen = None;
                edit(&mut asked.input, key);
            }
        },
        Event::Paste(text) => paste(&mut asked.input, text),
        _ => {}
    }
    false
}
