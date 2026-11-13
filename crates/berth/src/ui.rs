//! Drawing and input handling for the wizard.

use crate::{
    Result,
    app::{App, Entry, Item, Row},
    fields::{Action, Field, Kind, Step},
    job::Job,
    paths::Paths,
    registry::Store,
    theme,
};
use berth_tui::screen::{
    Platform, Spots, clear_hints, dialog, draw_arguments, draw_footer, draw_header, draw_panel,
    draw_question, draw_view, ellipsize, hint_at, inside, note_hints, pasted, with_links, words,
};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Layout, Margin, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Clear, Gauge, List, ListItem, ListState, Padding, Paragraph, Wrap},
};
use serde_json::json;
use tui_input::{Input, InputRequest, backend::crossterm::EventHandler};

/// A clickable region of the last frame.
#[derive(Clone, Copy)]
enum Target {
    Step(Step),
    /// Index into the selectable rows.
    Row(usize),
    Suggestion(usize),
    Button(KeyCode),
    /// Text area of the editor, with its horizontal scroll.
    Editor(usize),
}

type Hits = Vec<(Rect, Target)>;

enum Flow {
    Continue,
    Quit,
    Run,
}

fn buttons(app: &App) -> Vec<(&'static str, KeyCode)> {
    let mut buttons = vec![];
    if app.path_selected() {
        buttons.push(("[Browse F3]", KeyCode::F(3)));
    }
    if let Some(group) = app.selected_group() {
        buttons.push(if app.group_all_on(group) {
            ("[All off Space]", KeyCode::Char(' '))
        } else {
            ("[All on Space]", KeyCode::Char(' '))
        });
    }
    if app.selected_entry() == Some(Entry::Task("terminal")) {
        buttons.push(if app.as_agent {
            ("[As the SSH user Space]", KeyCode::Char(' '))
        } else {
            ("[As the agent Space]", KeyCode::Char(' '))
        });
    }
    if let Some(field) = app.selected_field() {
        if field.can_generate() {
            buttons.push(("[Generate F4]", KeyCode::F(4)));
        }
        if field.is_password_hash() {
            buttons.push(("[Set password F5]", KeyCode::F(5)));
        }
        if field.uses_saved_wifi() && !app.privileged {
            buttons.push(("[Unlock saved passwords F5]", KeyCode::F(5)));
        }
        if field == Field::BerthKey && !app.own_public.is_empty() {
            buttons.push(("[Copy the key c]", KeyCode::Char('c')));
        }
    }
    buttons
}

fn draw(frame: &mut Frame, app: &mut App, rows: &[Row], display: &mut ListState, hits: &mut Hits) {
    hits.clear();
    clear_hints();
    frame.render_widget(theme::backdrop(), frame.area());
    let area = frame.area().inner(Margin {
        horizontal: 2,
        vertical: 1,
    });
    let [header, _, panel, _, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(4),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    let host = app.place.title();
    // The overview has no tabs; the set-up flow under it does.
    let setup = app.step != Step::Manage;
    let labels = Step::SETUP.map(Step::label);
    let current = Step::SETUP.iter().position(|step| *step == app.step);
    let tabs = draw_header(
        frame,
        header,
        if setup { &labels[..] } else { &[][..] },
        current.unwrap_or(0),
        &if setup {
            format!("{host} › set-up")
        } else {
            host.to_string()
        },
    );
    hits.extend(
        tabs.into_iter()
            .zip(Step::SETUP)
            .map(|(rect, step)| (rect, Target::Step(step))),
    );
    let [left, _, right] = Layout::horizontal([
        Constraint::Percentage(62),
        Constraint::Length(2),
        Constraint::Min(0),
    ])
    .areas(panel);

    let editing = app.editing.is_some();
    let [body, editor, controls] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(if editing { 3 } else { 0 }),
        Constraint::Length(if editing { 0 } else { 1 }),
    ])
    .areas(left);
    let buttons = buttons(app);
    if !editing {
        let mut x = controls.x;
        for &(label, code) in &buttons {
            let rect = Rect::new(
                x,
                controls.y,
                (label.len() as u16).min(controls.right().saturating_sub(x)),
                1,
            );
            frame.render_widget(
                Paragraph::new(label).style(Style::new().fg(theme::ACCENT)),
                rect,
            );
            hits.push((rect, Target::Button(code)));
            x += label.len() as u16 + 2;
        }
    }

    // Right column: detected values for the selected field above its description.
    let [choices, help] = Layout::vertical([
        Constraint::Length(if app.suggestions.is_empty() {
            0
        } else {
            (app.suggestions.len() as u16 + 2).min(10)
        }),
        Constraint::Min(3),
    ])
    .areas(right);
    if !app.suggestions.is_empty() {
        let block = theme::panel("Suggestions · → to choose", app.suggestion_focus);
        let width = choices.width.saturating_sub(4) as usize;
        let labels = app
            .suggestions
            .iter()
            .map(|(label, _)| ellipsize(label, width))
            .collect::<Vec<_>>();
        frame.render_stateful_widget(
            List::new(labels)
                .block(block)
                .highlight_style(Style::new().bg(theme::RAISED)),
            choices,
            &mut app.suggestion_list,
        );
        for index in app.suggestion_list.offset()..app.suggestions.len() {
            let y = choices.y + 1 + (index - app.suggestion_list.offset()) as u16;
            if y + 1 >= choices.bottom() {
                break;
            }
            hits.push((
                Rect::new(choices.x + 1, y, choices.width.saturating_sub(2), 1),
                Target::Suggestion(index),
            ));
        }
    }
    frame.render_widget(
        Paragraph::new(crate::markdown::render(&app.help()))
            .block(theme::panel("Information", false))
            .wrap(Wrap { trim: false }),
        help,
    );

    // Left column: the rows of the step. The cursor only stops on selectable ones.
    let positions = rows
        .iter()
        .enumerate()
        .filter_map(|(position, row)| row.entry.map(|_| position))
        .collect::<Vec<_>>();
    display.select(
        app.list
            .selected()
            .and_then(|index| positions.get(index).copied()),
    );
    frame.render_stateful_widget(
        List::new(rows.iter().map(|row| ListItem::new(row.line.clone())))
            .highlight_symbol(theme::cursor())
            .highlight_style(Style::new().bg(theme::RAISED)),
        body,
        display,
    );
    for (index, &position) in positions.iter().enumerate() {
        if position < display.offset() {
            continue;
        }
        let y = body.y + (position - display.offset()) as u16;
        if y >= body.bottom() {
            break;
        }
        hits.push((Rect::new(body.x, y, body.width, 1), Target::Row(index)));
    }

    if let Some(input) = &app.editing {
        let secret = app.password_entry
            || app
                .selected_field()
                .is_some_and(|field| field.spec().kind == Kind::Secret);
        let width = editor.width.saturating_sub(2).max(1) as usize;
        let (cursor, scroll, value) = if secret {
            let cursor = input.cursor();
            (
                cursor,
                cursor.saturating_sub(width - 1),
                "•".repeat(input.value().chars().count()),
            )
        } else {
            (
                input.visual_cursor(),
                input.visual_scroll(width),
                input.value().to_owned(),
            )
        };
        let name = match (app.password_entry, app.selected_entry()) {
            (true, _) => " New password ".to_owned(),
            (false, Some(Entry::Machine(pick))) => berth_tui::machine::editor_title(pick, &host),
            _ => " Edit ".to_owned(),
        };
        let mut title = vec![Span::raw(name.clone())];
        let mut x = editor.x + 1 + name.len() as u16;
        for &(label, code) in &buttons {
            title.push(Span::styled(
                format!("{label} "),
                Style::new().fg(theme::ACCENT).bold(),
            ));
            hits.push((
                Rect::new(
                    x,
                    editor.y,
                    (label.len() as u16).min(editor.right().saturating_sub(x + 1)),
                    1,
                ),
                Target::Button(code),
            ));
            x += label.len() as u16 + 1;
        }
        frame.render_widget(
            Paragraph::new(value)
                .scroll((0, scroll.min(u16::MAX as usize) as u16))
                .block(
                    Block::bordered()
                        .title(Line::from(title))
                        .border_style(Style::new().fg(theme::ACCENT)),
                ),
            editor,
        );
        hits.push((
            editor.inner(Margin {
                horizontal: 1,
                vertical: 1,
            }),
            Target::Editor(scroll),
        ));
        if editor.width > 2 && editor.height > 2 && !app.suggestion_focus {
            frame.set_cursor_position((
                editor.x + 1 + (cursor - scroll).min(width - 1) as u16,
                editor.y + 1,
            ));
        }
    }

    let keys = if editing {
        format!(
            "Enter accept · ↑↓ field · Tab next tab · Esc cancel · {}",
            berth_tui::screen::COPY_FIELD
        )
    } else if app.selected_field() == Some(Field::BerthKey) {
        format!(
            "↑↓ select · Space switch · {} · 1-4 tab · Ctrl+S save · Esc overview",
            berth_tui::berth_key::HINT
        )
    } else {
        match app.step {
            Step::Manage => "↑↓ select · Enter choose · x unset · Ctrl+S save · q hosts",
            _ => "↑↓ select · Enter choose · 1-4 tab · x unset · Ctrl+S save · Esc overview",
        }
        .to_owned()
    };
    draw_footer(frame, footer, &keys, &app.status, theme::ACCENT);
    if let Some(picker) = &mut app.picker {
        picker.draw(frame);
    }
    if let Some(job) = &app.job {
        // A job of the machine is named by what it does, not by the install method.
        let action = match app.machine_job {
            true => job.title.clone(),
            false => app.action.label().to_owned(),
        };
        (app.links, app.words) = draw_job(frame, job, &action, &app.status, area);
    }
    if let Some(session) = &app.session {
        (app.links, app.words) = draw_session(frame, session, &app.status);
    }
    if let Some(view) = &app.host.view {
        (app.links, app.words) = draw_view(frame, view, &app.status, "Ctrl+Q");
    }
    if let Some((question, _)) = &app.host.question {
        draw_question(frame, question);
    }
    if let Some(question) = &app.machine.question {
        draw_question(frame, question);
    }
    draw_arguments(frame, &app.host);
    if app.choosing_drive {
        let lines = app
            .usb_drives
            .iter()
            .take(9)
            .enumerate()
            .map(|(index, (label, _))| Line::raw(ellipsize(&format!("{}  {label}", index + 1), 56)))
            .collect();
        dialog(
            frame,
            " Write to which USB drive? ",
            " 1-9 choose · Esc cancel ",
            lines,
        );
    }
    if let Some(question) = &app.confirm {
        let lines = vec![
            Line::raw(ellipsize(question, 56)),
            Line::styled(
                "Everything on that drive will be lost.",
                Style::new().fg(theme::WARN),
            ),
        ];
        dialog(frame, " Write to USB drive ", " y erase · n cancel ", lines);
    }
    if let Some(input) = &app.unlock {
        let local = std::env::var("USER").unwrap_or_else(|_| "this user".into());
        let user = match (app.unlock_remote, app.text(Field::SshUser)) {
            (false, _) => local,
            (true, "") => format!("{local} on {}", app.text(Field::SshHost)),
            (true, user) => format!("{user} on {}", app.text(Field::SshHost)),
        };
        let kept = match app.unlock_remote {
            true => "Kept until berth is closed. It is not stored.",
            false => "Used once for sudo. It is not stored.",
        };
        draw_unlock(
            frame,
            input,
            app.unlock_failed,
            app.unlock_reason,
            &user,
            kept,
        );
    }
    if let Some(qr) = &app.qr {
        qr.draw(frame);
    }
}

/// Put text on the clipboard: with the desktop's own tool when there is one,
/// otherwise by asking the terminal (OSC 52).
fn copy_text(text: &str) -> bool {
    use std::io::Write;
    use std::process::{Command, Stdio};
    for (program, args) in [
        ("wl-copy", &[][..]),
        ("xclip", &["-selection", "clipboard"][..]),
        ("xsel", &["--clipboard", "--input"][..]),
    ] {
        if program == "wl-copy" && std::env::var_os("WAYLAND_DISPLAY").is_none() {
            continue;
        }
        let child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(mut child) = child {
            let written = child
                .stdin
                .take()
                .is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
            // xclip keeps running to serve the selection; only wl-copy is waited for.
            if written
                && (program != "wl-copy" || child.wait().is_ok_and(|status| status.success()))
            {
                return true;
            }
        }
    }
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::new();
    for chunk in text.as_bytes().chunks(3) {
        let bits = chunk.iter().enumerate().fold(0u32, |bits, (index, byte)| {
            bits | (*byte as u32) << (16 - 8 * index)
        });
        for index in 0..4 {
            encoded.push(if index <= chunk.len() {
                ALPHABET[(bits >> (18 - 6 * index) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    let mut out = std::io::stdout();
    write!(out, "\x1b]52;c;{encoded}\x07").is_ok() && out.flush().is_ok()
}

/// What is on the desktop's clipboard, if it can be read.
fn paste_text() -> Option<String> {
    for (program, args) in [
        ("wl-paste", &["--no-newline"][..]),
        ("xclip", &["-selection", "clipboard", "-o"][..]),
        ("xsel", &["--clipboard", "--output"][..]),
    ] {
        if program == "wl-paste" && std::env::var_os("WAYLAND_DISPLAY").is_none() {
            continue;
        }
        let output = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output();
        if let Ok(output) = output
            && output.status.success()
        {
            return Some(String::from_utf8_lossy(&output.stdout).trim().to_owned());
        }
    }
    None
}

/// Draw a running or finished job. Returns where its web addresses and its
/// words are, for opening and copying with the mouse.
fn draw_job(frame: &mut Frame, job: &Job, action: &str, note: &str, area: Rect) -> (Spots, Spots) {
    let (keys, color) = match &job.outcome {
        None => (" Esc cancel ", theme::ACCENT),
        Some(Ok(())) if job.offers_usb(action, Action::Iso.label()) => {
            (" w write to USB drive · Enter close ", theme::GOOD)
        }
        Some(Ok(())) => (
            " Enter close · Click opens a link · Right-click copies a value ",
            theme::GOOD,
        ),
        Some(Err(_)) => (" Enter close · Right-click copies a value ", theme::BAD),
    };
    // Output with a web address in it offers its QR code.
    let shown = job.lines.iter().rev().take(area.height as usize);
    let keys = match berth_tui::qr::any_link(shown.map(String::as_str)) {
        true => format!(" {} · {} ", keys.trim(), berth_tui::qr::HINT),
        false => keys.to_owned(),
    };
    let title = if job.title == action {
        format!(" {action} ")
    } else {
        format!(" {action} · {} ", job.title)
    };
    note_hints(
        keys.trim(),
        area.x + 1,
        area.bottom().saturating_sub(1),
        true,
    );
    let block = theme::popup(title.trim(), color)
        .title_bottom(theme::hints(keys.trim()))
        .title_bottom(
            Line::styled(
                if note.is_empty() {
                    String::new()
                } else {
                    format!(" {note} ")
                },
                Style::new().fg(theme::WARN),
            )
            .right_aligned(),
        )
        .padding(Padding::horizontal(1));
    let [meter, _, log] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(block.inner(area));
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    match &job.outcome {
        None => match job.progress {
            Some(ratio) => frame.render_widget(
                Gauge::default()
                    .ratio(ratio.clamp(0.0, 1.0))
                    .gauge_style(Style::new().fg(theme::ACCENT).bg(theme::RAISED)),
                Rect { height: 1, ..meter },
            ),
            None => frame.render_widget(
                Paragraph::new("Working…").style(Style::new().fg(theme::MUTED)),
                meter,
            ),
        },
        Some(Ok(())) => frame.render_widget(
            Paragraph::new(format!("✓ Done. {}", job.next))
                .wrap(Wrap { trim: true })
                .style(Style::new().fg(theme::GOOD)),
            meter,
        ),
        Some(Err(error)) => frame.render_widget(
            Paragraph::new(format!("✗ {error}"))
                .wrap(Wrap { trim: true })
                .style(Style::new().fg(theme::BAD)),
            meter,
        ),
    }
    let first = job.lines.len().saturating_sub(log.height as usize);
    let (mut links, mut copyable) = (vec![], vec![]);
    let lines: Vec<Line> = job.lines[first..]
        .iter()
        .enumerate()
        .map(|(row, line)| {
            let mut x = log.x;
            let shown = ellipsize(line, log.width as usize);
            for (column, word) in words(&shown) {
                if !word.ends_with('…') {
                    let width = Span::raw(word.as_str()).width() as u16;
                    copyable.push((
                        Rect::new(log.x + column, log.y + row as u16, width, 1),
                        word,
                    ));
                }
            }
            let spans = with_links(&shown)
                .into_iter()
                .map(|(text, link)| {
                    let span = if link {
                        Span::styled(text.clone(), Style::new().fg(theme::ACCENT).underlined())
                    } else {
                        Span::raw(text.clone())
                    };
                    let width = span.width() as u16;
                    // A shortened address would open the wrong page.
                    if link && !text.ends_with('…') {
                        links.push((Rect::new(x, log.y + row as u16, width, 1), text));
                    }
                    x += width;
                    span
                })
                .collect::<Vec<_>>();
            Line::from(spans)
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines).style(Style::new().fg(theme::MUTED)),
        log,
    );
    (links, copyable)
}

/// Draw a command's terminal. Returns where its web addresses and its words
/// are, for opening and copying with the mouse.
fn draw_session(
    frame: &mut Frame,
    session: &crate::session::Session,
    note: &str,
) -> (Spots, Spots) {
    let keys = match session.outcome {
        None => {
            " Ctrl+V paste · Ctrl+Q close · Click opens a link · Right-click copies · Wheel scrolls "
        }
        Some(true) => " ✓ Done · Enter close ",
        Some(false) => " ✗ It did not finish · Enter close ",
    };
    draw_panel(
        frame,
        &session.title,
        session.screen(),
        session.outcome,
        keys,
        note,
    )
}

/// The desktop this terminal runs on: its clipboard, and its way to open a link.
struct Desktop;

impl Platform for Desktop {
    fn copy(&mut self, text: &str) -> bool {
        copy_text(text)
    }

    fn paste(&mut self) -> Option<String> {
        paste_text()
    }

    fn open(&mut self, link: &str) -> std::result::Result<(), String> {
        std::process::Command::new("xdg-open")
            .arg(link)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map(drop)
            .map_err(|error| error.to_string())
    }
}

/// A click on a key hint is that key.
fn hint_click(event: Event) -> Event {
    if let Event::Mouse(mouse) = &event
        && mouse.kind == MouseEventKind::Down(MouseButton::Left)
        && let Some(key) = hint_at(mouse.column, mouse.row)
    {
        return Event::Key(crate::keys::native(key));
    }
    event
}

/// Alt+R over a panel shows its links as QR codes, and the overlay takes
/// every key while it is open.
fn qr_event(app: &mut App, event: &Event) -> bool {
    let Some(event) = crate::keys::event(event) else {
        return app.qr.is_some();
    };
    let mut said = None;
    let taken = berth_tui::qr::take(&mut app.qr, &app.links, &event, &mut said);
    if let Some(said) = said {
        app.status = said;
    }
    taken
}

/// A click on a web address opens it here; a right-click on a word copies it.
fn spot_event(app: &mut App, event: &Event) -> bool {
    let said = crate::keys::event(event).and_then(|event| {
        berth_tui::screen::spot_event(&app.links, &app.words, &event, &mut Desktop)
    });
    match said {
        Some(said) => {
            if !said.is_empty() {
                app.status = said;
            }
            true
        }
        None => false,
    }
}

fn session_event(app: &mut App, event: &Event) {
    if qr_event(app, event) || spot_event(app, event) {
        return;
    }
    let Some(session) = &mut app.session else {
        return;
    };
    match event {
        Event::Mouse(mouse) if mouse.kind == MouseEventKind::ScrollUp => session.scroll(3),
        Event::Mouse(mouse) if mouse.kind == MouseEventKind::ScrollDown => session.scroll(-3),
        Event::Paste(text) => {
            session.paste(text);
            app.status = pasted(text);
        }
        Event::Key(key) if key.kind != KeyEventKind::Release => {
            let control = key.modifiers.contains(KeyModifiers::CONTROL);
            if control && key.code == KeyCode::Char('q') {
                app.session = None;
            } else if control && key.code == KeyCode::Char('v') {
                // Terminals that keep Ctrl+V to themselves paste by their own means.
                app.status = match paste_text() {
                    Some(text) => {
                        session.paste(&text);
                        pasted(&text)
                    }
                    None => "Could not read the clipboard".into(),
                };
            } else if session.running() {
                session.key(key);
            } else if matches!(key.code, KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q')) {
                let title = session.title.clone();
                app.status = match session.outcome {
                    Some(true) => format!("{title} finished"),
                    _ => format!("{title} did not finish"),
                };
                app.session = None;
            }
        }
        _ => {}
    }
}

/// Keys and the mouse while a session of the host is in the panel.
fn view_event(app: &mut App, event: &Event) {
    if qr_event(app, event) {
        return;
    }
    if let Some(event) = crate::keys::event(event)
        && let Some(said) = berth_tui::screen::view_event(
            &mut app.host,
            &app.links,
            &app.words,
            &event,
            &mut Desktop,
        )
    {
        app.status = said;
    }
}

/// The yes-or-no question before a task or before applying.
fn question_event(app: &mut App, event: &Event) -> Flow {
    let Event::Key(key) = event else {
        return Flow::Continue;
    };
    match key.code {
        KeyCode::Char('y' | 'Y') => match app.host.question.take() {
            Some((_, crate::remote::Pending::Action(index))) => app.begin_task(index, true),
            Some((_, crate::remote::Pending::Apply)) => return apply(app, true),
            None => {}
        },
        KeyCode::Char('n' | 'N') | KeyCode::Esc => app.host.question = None,
        _ => {}
    }
    Flow::Continue
}

/// The dialog that asks for a task's arguments.
fn arguments_event(app: &mut App, event: &Event, platform: &mut dyn Platform) {
    let Some(event) = crate::keys::event(event) else {
        return;
    };
    let mut said = None;
    let accepted = berth_tui::screen::arguments_event(&mut app.host, &event, platform, &mut said);
    if let Some(said) = said {
        app.status = said;
    }
    if accepted {
        app.accept_argument();
    }
}

/// Applying the configuration is the SSH install, run from the overview. It
/// may replace the host's controller, which ends the sessions running there.
fn apply(app: &mut App, confirmed: bool) -> Flow {
    let running = app
        .host
        .sessions
        .iter()
        .filter(|session| session.running())
        .count();
    if running > 0 && !confirmed {
        let question = format!(
            "{running} session{} on the host. Applying can restart berth's controller there, which ends them. Apply anyway?",
            if running == 1 {
                " is running"
            } else {
                "s are running"
            }
        );
        app.host.question = Some((question, crate::remote::Pending::Apply));
        return Flow::Continue;
    }
    app.action = Action::Ssh;
    let problems = app.problems();
    app.action = Action::Manage;
    if problems.is_empty() {
        app.terminal_action = Some(Action::Ssh);
        // What the host can do may be different afterwards.
        app.host.disconnect();
        return Flow::Run;
    }
    app.status = problems[0].clone();
    Flow::Continue
}

fn job_event(app: &mut App, event: &Event) {
    if qr_event(app, event) || spot_event(app, event) {
        return;
    }
    let (Some(job), Event::Key(key)) = (&mut app.job, event) else {
        return;
    };
    if key.kind == KeyEventKind::Release {
        return;
    }
    let interrupt = key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
    if job.running() {
        if key.code == KeyCode::Esc || interrupt {
            job.cancel();
        }
    } else if key.code == KeyCode::Char('w')
        && job.offers_usb(app.action.label(), Action::Iso.label())
    {
        app.write_built_iso(None);
    } else if matches!(key.code, KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q')) || interrupt {
        app.close_job();
    }
}

fn confirm_event(app: &mut App, event: &Event) {
    let Event::Key(key) = event else {
        return;
    };
    match key.code {
        KeyCode::Char('y' | 'Y') => {
            app.confirm = None;
            app.run(true);
        }
        KeyCode::Char('n' | 'N') | KeyCode::Esc => {
            app.confirm = None;
            app.write_only = false;
        }
        _ => {}
    }
}

fn draw_unlock(
    frame: &mut Frame,
    input: &Input,
    failed: bool,
    reason: &str,
    user: &str,
    kept: &'static str,
) {
    let typed = input.value().chars().count().min(54);
    let note = if failed {
        Line::styled("Wrong password. Try again.", Style::new().fg(theme::BAD))
    } else {
        Line::styled(kept, Style::new().fg(theme::MUTED))
    };
    let lines = vec![
        Line::raw(reason.to_owned()),
        note,
        Line::raw(""),
        Line::from(vec![
            Span::styled("› ", Style::new().fg(theme::ACCENT)),
            Span::raw("•".repeat(typed)),
        ]),
    ];
    let title = ellipsize(&format!(" Password for {user} "), 58);
    let area = dialog(
        frame,
        &title,
        " Enter unlock · Esc cancel · Ctrl+O copy ",
        lines,
    );
    if area.width > 6 && area.height > 5 {
        frame.set_cursor_position((area.x + 4 + typed as u16, area.y + 4));
    }
}

fn unlock_event(app: &mut App, event: &Event, platform: &mut dyn Platform) {
    let Some(input) = &mut app.unlock else {
        return;
    };
    match event {
        Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
            KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.status = berth_tui::screen::copy_field(platform, input);
            }
            KeyCode::Esc => {
                app.unlock = None;
                app.unlock_remote = false;
                app.pending_run = false;
                app.write_only = false;
            }
            KeyCode::Enter => app.submit_unlock(),
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => input.reset(),
            _ => {
                input.handle_event(event);
            }
        },
        Event::Paste(text) => {
            for ch in text.chars().filter(|ch| !ch.is_control()) {
                input.handle(InputRequest::InsertChar(ch));
            }
        }
        _ => {}
    }
}

fn picker_event(app: &mut App, event: &Event) {
    let Some(picker) = &mut app.picker else {
        return;
    };
    let mut choose = false;
    let mut selected = None;
    match event {
        Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
            KeyCode::Esc => {
                app.picker = None;
                return;
            }
            KeyCode::Up => picker.move_by(true),
            KeyCode::Down => picker.move_by(false),
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::CONTROL) => {
                selected = Some(picker.directory.display().to_string())
            }
            KeyCode::Enter => choose = true,
            _ => {}
        },
        Event::Mouse(mouse) if inside(picker.area, mouse.column, mouse.row) => match mouse.kind {
            MouseEventKind::ScrollUp => picker.move_by(true),
            MouseEventKind::ScrollDown => picker.move_by(false),
            MouseEventKind::Down(MouseButton::Left)
                if mouse.row > picker.area.y && mouse.row + 1 < picker.area.bottom() =>
            {
                let index = picker.state.offset() + (mouse.row - picker.area.y - 1) as usize;
                if index < picker.entries.len() {
                    picker.state.select(Some(index));
                    choose = true;
                }
            }
            _ => {}
        },
        _ => {}
    }
    if choose {
        match picker.choose() {
            Ok(value) => selected = value,
            Err(error) => app.status = error.to_string(),
        }
    }
    if let Some(path) = selected {
        app.picker = None;
        app.password_entry = false;
        app.editing = Some(Input::new(path));
        app.accept_edit();
    }
}

fn move_suggestion(app: &mut App, up: bool) {
    let index = app.suggestion_list.selected().unwrap_or(0);
    app.suggestion_list.select(Some(if up {
        index.saturating_sub(1)
    } else {
        (index + 1).min(app.suggestions.len().saturating_sub(1))
    }));
}

/// Handle a mouse event. Clicks on rows and buttons come back as the key they stand for.
fn mouse_event(app: &mut App, mouse: MouseEvent, hits: &Hits) -> Option<KeyEvent> {
    let target = hits
        .iter()
        .rev()
        .find(|(rect, _)| inside(*rect, mouse.column, mouse.row))
        .copied();
    let key = |code| Some(KeyEvent::new(code, KeyModifiers::NONE));
    match mouse.kind {
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            let up = mouse.kind == MouseEventKind::ScrollUp;
            if matches!(target, Some((_, Target::Suggestion(_)))) {
                move_suggestion(app, up);
                app.suggestion_focus = true;
            } else {
                app.accept_edit();
                app.move_selection(up);
            }
            None
        }
        MouseEventKind::Down(MouseButton::Left) => match target? {
            (_, Target::Step(step)) => {
                app.accept_edit();
                app.set_step(step);
                None
            }
            (_, Target::Row(index)) => {
                app.accept_edit();
                app.list.select(Some(index));
                app.suggestion_focus = false;
                key(KeyCode::Enter)
            }
            (_, Target::Suggestion(index)) => {
                app.suggestion_list.select(Some(index));
                app.select_suggestion();
                None
            }
            (_, Target::Button(code)) => key(code),
            (rect, Target::Editor(scroll)) => {
                if let Some(input) = &mut app.editing {
                    let column = (mouse.column - rect.x) as usize + scroll;
                    let mut width = 0;
                    let cursor = input
                        .value()
                        .chars()
                        .take_while(|ch| {
                            width += Span::raw(ch.to_string()).width();
                            width <= column
                        })
                        .count();
                    input.handle(InputRequest::SetCursor(cursor));
                }
                None
            }
        },
        _ => None,
    }
}

fn choose(app: &mut App, entry: Entry) -> Flow {
    match entry {
        Entry::Action(action) => {
            app.action = action;
            berth_tui::setup::fit_profile(&mut app.config, action);
        }
        Entry::Field(field) => app.activate(field),
        Entry::Setup => app.set_step(Step::Install),
        Entry::HostRegistry(index) => app.toggle_host_registry(index),
        Entry::HostSecret(index) => app.editing = Some(Input::new(app.host_secret(index))),
        // A key is pasted whole, not changed letter by letter.
        Entry::Key(..) => app.editing = Some(Input::default()),
        Entry::Group(_) => app.status = "Space switches the whole group".into(),
        Entry::Feature(index) => {
            let item = Item::Feature(index);
            app.set_item(item, !app.item_enabled(item));
        }
        Entry::HostTask(index) => app.begin_task(index, false),
        Entry::HostSession(index) => {
            if let Some(session) = app.host.sessions.get(index).cloned() {
                app.host.open(&session);
            }
        }
        Entry::Task("terminal") => app.start_host_shell(),
        Entry::Task("ssh") => {
            let (rows, cols) = session_size();
            app.start_shell(rows, cols);
        }
        Entry::Task("connect") => app.reconnect_host(),
        Entry::Task("address") => app.edit_address(),
        Entry::Task("apply") => return apply(app, false),
        Entry::Task("install-controller") => {
            let _ = app.save();
            app.own_command = Some(
                ["host", "install", &app.place.argument()]
                    .map(str::to_owned)
                    .to_vec(),
            );
        }
        Entry::Machine(pick) => app.machine_enter(pick),
        Entry::Save => {
            save(app);
        }
        Entry::Task(_) => {}
        Entry::Run => {
            if !app.problems().is_empty() {
                app.status = "Fix the listed problems first".into();
            } else if app.leaves_ui() {
                return Flow::Run;
            } else {
                app.write_only = false;
                app.run(false);
            }
        }
    }
    Flow::Continue
}

/// Save the profile and say how that went.
fn save(app: &mut App) -> bool {
    let saved = app.save();
    let done = saved.is_ok();
    app.status = match saved {
        Ok(()) => app.saved_note(),
        Err(error) => format!("Save failed · {error}"),
    };
    done
}

/// The size of the terminal panel for the window as it is now.
fn session_size() -> (u16, u16) {
    let (width, height) = crossterm::terminal::size().unwrap_or((100, 30));
    (
        height.saturating_sub(4).max(4),
        width.saturating_sub(8).max(20),
    )
}

fn key_event(
    app: &mut App,
    key: KeyEvent,
    quit_armed: &mut bool,
    platform: &mut dyn Platform,
) -> Flow {
    let armed = std::mem::take(quit_armed);
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('s') if control => {
            app.accept_edit();
            save(app);
            return Flow::Continue;
        }
        KeyCode::F(3) => {
            app.open_picker();
            return Flow::Continue;
        }
        KeyCode::F(4) if app.selected_field().is_some_and(Field::can_generate) => {
            app.cancel_edit();
            if let Err(error) = app.generate() {
                app.status = error.to_string();
            }
            return Flow::Continue;
        }
        KeyCode::F(5) if app.selected_field().is_some_and(Field::is_password_hash) => {
            app.password_entry = true;
            app.editing = Some(Input::default());
            return Flow::Continue;
        }
        KeyCode::F(5)
            if !app.privileged && app.selected_field().is_some_and(Field::uses_saved_wifi) =>
        {
            app.accept_edit();
            app.request_unlock("Read the Wi-Fi passwords saved on this machine.");
            return Flow::Continue;
        }
        KeyCode::Tab | KeyCode::BackTab => {
            app.accept_edit();
            let step = app
                .step
                .offset(if key.code == KeyCode::Tab { 1 } else { -1 });
            if step != app.step {
                app.set_step(step);
            }
            return Flow::Continue;
        }
        KeyCode::F(2) if !app.suggestions.is_empty() => {
            app.suggestion_focus = !app.suggestion_focus;
            return Flow::Continue;
        }
        _ => {}
    }
    if app.suggestion_focus {
        match key.code {
            KeyCode::Up => move_suggestion(app, true),
            KeyCode::Down => move_suggestion(app, false),
            KeyCode::Enter => app.select_suggestion(),
            KeyCode::Left | KeyCode::Esc => app.suggestion_focus = false,
            _ => {}
        }
        return Flow::Continue;
    }
    if let Some(input) = &mut app.editing {
        match key.code {
            KeyCode::Esc => {
                app.retry_address = false;
                app.cancel_edit();
            }
            KeyCode::Enter => {
                let retry = std::mem::take(&mut app.retry_address);
                app.accept_edit();
                if retry && save(app) {
                    // Back on the overview, which asks the host at its new address.
                    app.set_step(Step::Manage);
                    app.reconnect_host();
                }
            }
            KeyCode::Up | KeyCode::Down => {
                app.accept_edit();
                app.move_selection(key.code == KeyCode::Up);
            }
            KeyCode::Char('u') if control => input.reset(),
            KeyCode::Char('o') if control => {
                app.status = berth_tui::screen::copy_field(platform, input);
            }
            _ => {
                input.handle_event(&Event::Key(key));
            }
        }
        return Flow::Continue;
    }
    match key.code {
        KeyCode::Up => app.move_selection(true),
        KeyCode::Down => app.move_selection(false),
        KeyCode::Right if !app.suggestions.is_empty() => app.suggestion_focus = true,
        _ if app.selected_field() == Some(Field::BerthKey)
            && crate::keys::key(&key).is_some_and(|key| berth_tui::berth_key::is_copy(&key)) =>
        {
            app.status = berth_tui::berth_key::copy(platform, &app.own_public);
        }
        KeyCode::Char(digit @ '1'..='9') => {
            if app.step != Step::Manage
                && let Some(&step) = Step::SETUP.get(digit as usize - '1' as usize)
            {
                app.set_step(step);
            }
        }
        KeyCode::Char(' ') => match app.selected_entry() {
            Some(Entry::Group(group)) => app.toggle_group(group),
            Some(Entry::Task("terminal")) => app.as_agent = !app.as_agent,
            Some(
                entry @ (Entry::Field(_)
                | Entry::Feature(..)
                | Entry::HostRegistry(_)
                | Entry::Machine(
                    berth_tui::machine::Pick::Provisioner | berth_tui::machine::Pick::Argument(_),
                )),
            ) => {
                return choose(app, entry);
            }
            _ => {}
        },
        KeyCode::Char('x') | KeyCode::Delete => {
            if let Some(Entry::Key(account, Some(index))) = app.selected_entry()
                && berth_tui::setup::set_key(&mut app.config, account, Some(index), "").is_ok()
            {
                app.status = "SSH key removed".into();
            }
            if let Some(Entry::HostSecret(index)) = app.selected_entry() {
                app.set_host_secret(index, "");
            }
            if let Some(Entry::HostSession(index)) = app.selected_entry()
                && let Some(session) = app.host.sessions.get(index)
            {
                if session.running() {
                    app.status = "It is still running. Open it and end it first.".into();
                } else {
                    app.status = format!("Forgot {}", session.title);
                    app.host
                        .send(crate::remote::Request::Delete(session.id.clone()));
                }
            }
            if let Some(field) = app.selected_field() {
                app.clear(field);
            }
        }
        // Leaving the set-up flow returns to the host's overview.
        KeyCode::Char('q') | KeyCode::Esc if app.step != Step::Manage => app.set_step(Step::Manage),
        KeyCode::Char('q') | KeyCode::Esc => {
            if armed || !app.unsaved() {
                return Flow::Quit;
            }
            *quit_armed = true;
            app.status = "Unsaved changes · Ctrl+S saves, q again discards".into();
        }
        KeyCode::Enter => {
            if let Some(entry) = app.selected_entry() {
                return choose(app, entry);
            }
        }
        _ => {}
    }
    Flow::Continue
}

/// What the home screen is showing.
enum HomeMode {
    Hosts,
    Naming(Input),
    /// The selected setting, and its editor while it is being changed.
    Settings(usize, Option<Input>),
    Registries(RegistryView),
    Secrets(SecretView),
    Plugins(PluginView),
    Provisioners(Box<crate::provisioner_tab::View>),
}

/// A clickable region of the home screen.
#[derive(Clone, Copy)]
enum HomeTarget {
    /// Hosts, Settings or Registries.
    Tab(usize),
    Row(usize),
}

/// The Registries tab: the selected row, its editor while something is typed,
/// and whether a registry's removal waits for a second x.
#[derive(Default)]
struct RegistryView {
    selected: usize,
    editor: Option<Input>,
    armed: bool,
    /// The result of the last check, shown until the selection moves.
    report: Vec<String>,
}

/// A row of the Registries tab. Registries are named, repositories numbered.
#[derive(Clone, PartialEq, Debug)]
enum RegistryRow {
    Name(String),
    Type(String),
    Address(String),
    User(String),
    Password(String),
    Repository(String, usize),
    Format(String, usize),
    DockerAddress(String, usize),
    AddRepository(String),
    AddRegistry,
}

impl RegistryRow {
    /// The registry a row belongs to.
    fn registry(&self) -> Option<&str> {
        use RegistryRow::*;
        match self {
            Name(name)
            | Type(name)
            | Address(name)
            | User(name)
            | Password(name)
            | Repository(name, _)
            | Format(name, _)
            | DockerAddress(name, _)
            | AddRepository(name) => Some(name),
            AddRegistry => None,
        }
    }

    fn help(&self) -> &'static str {
        use RegistryRow::*;
        match self {
            Name(_) => {
                "A registry: a server packages, files or images come from and go to. Enter renames it, x removes it with its stored login. Hosts get a registry with: berth host HOST registry NAME on."
            }
            Type(_) => {
                "Enter switches the kind. Sonatype Nexus: one server and login with repositories under it. npm registry: any npm-compatible registry at this address. File server: an address files are uploaded to with HTTP PUT, for the release archive. Docker registry: any registry for container images."
            }
            Address(_) => {
                "Where the registry is. With https:// for Nexus, npm and file servers; a Docker registry is HOST or HOST:PORT."
            }
            User(_) => {
                "The account to log in with. Leave it empty when the registry takes a token alone, such as an npm token. x clears it."
            }
            Password(_) => {
                "The password or token. It is kept in berth's store with the secrets, and is never shown. x forgets it."
            }
            Repository(..) => {
                "A repository on the Nexus server, by its name there. Enter renames it, x removes it from this list."
            }
            Format(..) => {
                "Enter switches the format. npm: becomes the agent's npm registry. raw: a place to publish the release archive. docker: container images."
            }
            DockerAddress(..) => {
                "Leave it empty when the repository uses path based routing in Nexus: images are then at HOST/REPOSITORY/IMAGE. With a connector on a port or host name of its own, enter that as HOST:PORT."
            }
            AddRepository(_) => {
                "Add a repository of this Nexus server. Type its name; it starts as npm and Format changes that."
            }
            AddRegistry => {
                "Add a registry. Type a short name for it (lower-case letters, digits, - and _), then set its kind and address."
            }
        }
    }
}

/// The rows of the Registries tab with what they show.
fn registry_rows(store: Option<&Store>) -> Vec<(RegistryRow, Line<'static>)> {
    use RegistryRow::*;
    let muted = Style::new().fg(theme::MUTED);
    let field = |prefix: &str, label: &str, value: &str| {
        let mut line = crate::app::value_line(label, value, Kind::Text);
        line.spans.insert(0, Span::styled(prefix.to_owned(), muted));
        line
    };
    let mut rows = vec![];
    let registries = store.and_then(|store| store.registries().ok());
    for (name, registry) in registries.unwrap_or_default() {
        let text = |key: &str| registry[key].as_str().unwrap_or("").to_owned();
        let nexus = registry["type"] == "nexus";
        rows.push((
            Name(name.clone()),
            Line::styled(name.clone(), Style::new().fg(theme::ACCENT).bold()),
        ));
        rows.push((
            Type(name.clone()),
            field("  ├─ ", "Kind", crate::registry::type_label(&text("type"))),
        ));
        rows.push((
            Address(name.clone()),
            field("  ├─ ", "Address", &text("url")),
        ));
        rows.push((
            User(name.clone()),
            field("  ├─ ", "User name", &text("username")),
        ));
        let stored = store
            .and_then(|store| store.has_secret(&format!("registry/{name}")).ok())
            .unwrap_or(false);
        let mut password = Line::from(vec![
            Span::styled(if nexus { "  ├─ " } else { "  └─ " }, muted),
            Span::styled("Password or token  ", Style::new().fg(theme::TEXT)),
        ]);
        password.spans.push(match stored {
            true => Span::styled("● stored", Style::new().fg(theme::GOOD)),
            false => Span::styled("○ not set", muted),
        });
        rows.push((Password(name.clone()), password));
        if !nexus {
            continue;
        }
        let repositories = registry["repositories"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        for (index, repository) in repositories.iter().enumerate() {
            let repo = repository["name"].as_str().unwrap_or("");
            let format = repository["format"].as_str().unwrap_or("");
            let mut line = field("  ├─ ", "Repository", repo);
            if format != "docker" {
                line.spans.push(Span::styled(
                    format!("  {}", crate::registry::repository_url(&registry, repo)),
                    muted,
                ));
            }
            rows.push((Repository(name.clone(), index), line));
            let last = if format == "docker" {
                "  │    ├─ "
            } else {
                "  │    └─ "
            };
            rows.push((Format(name.clone(), index), field(last, "Format", format)));
            if format == "docker" {
                let line = match repository["address"]
                    .as_str()
                    .filter(|address| !address.is_empty())
                {
                    Some(address) => field("  │    └─ ", "Docker address", address),
                    None => Line::from(vec![
                        Span::styled("  │    └─ ", muted),
                        Span::styled("Docker address  ", Style::new().fg(theme::TEXT)),
                        Span::styled(
                            format!(
                                "Default · {} (path based routing)",
                                crate::registry::docker_address(&registry, repository)
                            ),
                            muted.italic(),
                        ),
                    ]),
                };
                rows.push((DockerAddress(name.clone(), index), line));
            }
        }
        rows.push((
            AddRepository(name.clone()),
            Line::from(vec![
                Span::styled("  └─ ", muted),
                Span::styled("+ Add repository", Style::new().fg(theme::ACCENT)),
            ]),
        ));
    }
    rows.push((
        AddRegistry,
        Line::styled("+ Add registry", Style::new().fg(theme::ACCENT)),
    ));
    rows
}

/// The Plugins tab: its rows and editor, the plugins of this machine or of
/// the berth server as they were last read, and what the last change said.
#[derive(Default)]
struct PluginView {
    tab: berth_tui::plugins::Tab,
    editor: Option<Input>,
    listing: Option<std::result::Result<berth_api::PluginList, String>>,
    report: Vec<String>,
    /// The lines of a roll-out that runs, as they come.
    rolling: Option<std::sync::mpsc::Receiver<String>>,
}

/// The lines a roll-out writes, sent on one by one; only its own, not what
/// Ansible says.
struct RolloutLines {
    sender: std::sync::mpsc::Sender<String>,
    pending: Vec<u8>,
}

impl std::io::Write for RolloutLines {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.pending.extend_from_slice(data);
        while let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line).trim_end().to_owned();
            if berth_tui::plugins::rollout_line(&line) {
                let _ = self.sender.send(line);
            }
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl PluginView {
    /// Read the plugins, unless they were read since the last change.
    fn load(&mut self, paths: &Paths) {
        if self.listing.is_none() {
            let listed = crate::plugins::Manager::new(paths).list();
            self.listing = Some(listed.map_err(|error| error.to_string()));
        }
    }

    fn plugins(&self) -> &[berth_api::PluginInfo] {
        match &self.listing {
            Some(Ok(listing)) => &listing.plugins,
            _ => &[],
        }
    }

    /// Take in what a roll-out that runs has said since.
    fn pump(&mut self, status: &mut String) {
        let Some(lines) = &self.rolling else {
            return;
        };
        loop {
            match lines.try_recv() {
                Ok(line) => {
                    *status = line.clone();
                    self.report.push(line);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.rolling = None;
                    return;
                }
            }
        }
    }

    /// Start a roll-out in the background, its lines shown as they come.
    fn roll_out(&mut self, paths: &Paths, options: berth_api::Rollout) {
        let (sender, receiver) = std::sync::mpsc::channel();
        let paths = paths.clone();
        self.report = vec!["Rolling out…".into()];
        self.tab.behind.clear();
        self.rolling = Some(receiver);
        std::thread::spawn(move || {
            let mut out = RolloutLines {
                sender: sender.clone(),
                pending: vec![],
            };
            let said = match crate::commands::rollout(&paths, &options, false, &mut out) {
                Ok(report) => crate::commands::rollout_summary(&report),
                Err(error) => format!("✗ {error}"),
            };
            for line in said.lines() {
                let _ = sender.send(line.to_owned());
            }
        });
    }

    /// Show what a change said, and read the plugins again.
    fn changed(&mut self, done: Result<Vec<String>>, status: &mut String) {
        self.report = match done {
            Ok(lines) => lines,
            Err(error) => error.to_string().lines().map(str::to_owned).collect(),
        };
        *status = berth_tui::plugins::status(&self.report);
        self.listing = None;
    }
}

/// Where the plugins of the Plugins tab are: here, or on the berth server.
fn plugin_place(server: &Option<berth_core::server::Server>) -> berth_tui::plugins::Place<'_> {
    match server {
        Some(server) => berth_tui::plugins::Place::Server(&server.url),
        None => berth_tui::plugins::Place::Here,
    }
}

/// Handle a key on the Plugins tab.
fn plugins_key(
    view: &mut PluginView,
    paths: &Paths,
    key: KeyEvent,
    status: &mut String,
) -> Outcome {
    use berth_tui::plugins::Asked;
    view.load(paths);
    let server = berth_core::server::Server::configured(paths);
    let plugins = view.plugins().to_vec();
    let Some(shared) = crate::keys::key(&key) else {
        return Outcome::Stay;
    };
    let place = plugin_place(&server);
    match view
        .tab
        .key(shared, &mut view.editor, &plugins, &mut view.report, &place)
    {
        Asked::Nothing(said) => {
            if let Some(said) = said {
                *status = said;
            }
        }
        Asked::Install(spec) => {
            let done = crate::plugins::Manager::new(paths).add(&spec);
            view.changed(done, status);
        }
        Asked::Remove(id) => {
            let done = crate::plugins::Manager::new(paths).remove(&id);
            view.changed(done, status);
        }
        Asked::Check => match crate::plugins::Manager::new(paths).check() {
            Ok(checked) => {
                view.report = view.tab.checked(checked.plugins);
                *status = berth_tui::plugins::status(&view.report);
            }
            Err(error) => view.changed(Err(error), status),
        },
        Asked::Update(id) => {
            let manager = crate::plugins::Manager::new(paths);
            match manager.update(id.as_deref()) {
                Ok(mut report) => {
                    if manager.server().is_none() {
                        report.behind = berth_core::rollout::survey(paths);
                    }
                    let lines = view.tab.updated(report);
                    view.changed(Ok(lines), status);
                }
                Err(error) => view.changed(Err(error), status),
            }
        }
        Asked::Rollout(_) if view.rolling.is_some() => {
            *status = "A roll-out runs already".into();
        }
        Asked::Rollout(options) => view.roll_out(paths, options),
        Asked::Other => match leave_tab(key, tabs::PLUGINS) {
            Outcome::Stay => {}
            outcome => return outcome,
        },
    }
    Outcome::Stay
}

/// The Secrets tab: the selected row, the editor, the name a typed value is
/// for, and whether a removal waits for a second x.
#[derive(Default)]
struct SecretView {
    selected: usize,
    editor: Option<Input>,
    /// A new secret's name, while its value is being typed.
    naming: Option<String>,
    armed: bool,
}

/// Handle a key on the Secrets tab. The row after the last secret adds one.
fn secrets_key(
    view: &mut SecretView,
    paths: &Paths,
    key: KeyEvent,
    status: &mut String,
) -> Outcome {
    let store = match Store::new(paths) {
        Ok(store) => store,
        Err(error) => {
            *status = error.to_string();
            return leave_tab(key, tabs::SECRETS);
        }
    };
    let names = store.secret_names();
    view.selected = view.selected.min(names.len());
    let selected = names.get(view.selected).cloned();
    if let Some(input) = &mut view.editor {
        match key.code {
            KeyCode::Esc => {
                view.editor = None;
                view.naming = None;
            }
            KeyCode::Enter => {
                let typed = input.value().to_owned();
                // A name was typed for a new secret: its value comes next.
                if selected.is_none() && view.naming.is_none() {
                    let name = typed.trim();
                    if !crate::plugins::valid_name(name) {
                        *status = "Use lower-case letters, digits, - and _, starting with a letter"
                            .into();
                    } else if names.iter().any(|known| known == name) {
                        *status = format!("There is a secret {name} already");
                    } else {
                        view.naming = Some(name.to_owned());
                        view.editor = Some(Input::default());
                        *status = format!("Now the value of {name}");
                    }
                    return Outcome::Stay;
                }
                let name = view.naming.take().or(selected).unwrap_or_default();
                view.editor = None;
                if typed.is_empty() {
                    *status = "Nothing entered, kept as it was".into();
                    return Outcome::Stay;
                }
                *status = match store.set_secret(&name, Some(&typed)) {
                    Ok(()) => format!("Stored {name}"),
                    Err(error) => error.to_string(),
                };
                let listed = store.secret_names();
                view.selected = listed
                    .iter()
                    .position(|known| *known == name)
                    .unwrap_or(view.selected);
            }
            _ => {
                input.handle_event(&Event::Key(key));
            }
        }
        return Outcome::Stay;
    }
    let armed = std::mem::take(&mut view.armed);
    match key.code {
        KeyCode::Up => view.selected = view.selected.saturating_sub(1),
        KeyCode::Down => view.selected = (view.selected + 1).min(names.len()),
        KeyCode::Enter => view.editor = Some(Input::default()),
        KeyCode::Char('x') | KeyCode::Delete => match selected {
            Some(name) if armed => {
                *status = match store.set_secret(&name, None) {
                    Ok(()) => format!("Removed {name}"),
                    Err(error) => error.to_string(),
                }
            }
            Some(name) => {
                view.armed = true;
                *status = format!("x again removes {name}");
            }
            None => {}
        },
        _ => return leave_tab(key, tabs::SECRETS),
    }
    Outcome::Stay
}

/// The tabs of the home screen, in their order; a digit key is one more
/// than the index.
pub(crate) mod tabs {
    pub const HOSTS: usize = 0;
    pub const PROVISIONERS: usize = 1;
    pub const SETTINGS: usize = 2;
    pub const REGISTRIES: usize = 3;
    pub const SECRETS: usize = 4;
    pub const PLUGINS: usize = 5;
    pub const NAMES: [&str; 6] = [
        "Hosts",
        "Provisioners",
        "Settings",
        "Registries",
        "Secrets",
        "Plugins",
    ];
}

/// The keys that leave a tab for another, the same on every tab: a digit,
/// Tab and Shift+Tab, Esc for the hosts, and q. `here` is the tab's index.
fn leave_tab(key: KeyEvent, here: usize) -> Outcome {
    let count = tabs::NAMES.len();
    match key.code {
        KeyCode::Char(digit @ '1'..='6') => Outcome::Tab(digit as usize - '1' as usize),
        KeyCode::Esc => Outcome::Tab(tabs::HOSTS),
        KeyCode::Tab => Outcome::Tab((here + 1) % count),
        KeyCode::BackTab => Outcome::Tab((here + count - 1) % count),
        KeyCode::Char('q') => Outcome::Quit,
        _ => Outcome::Stay,
    }
}

/// Where the registries and secrets of `store` are kept, for the help of their tabs.
fn kept_in(paths: &Paths, store: &Store) -> String {
    match store.server() {
        Some(server) => format!(
            "Kept on the berth server at {}. This berth has the registries and the names of the secrets in its store.",
            server.url
        ),
        None => format!(
            "Kept in berth's store, the {}, {}. See: berth store",
            berth_core::store::Installation::of(paths).target.describe(),
            match store.encrypted() {
                true => "encrypted",
                false => "not encrypted",
            }
        ),
    }
}

/// What a key on the Registries tab leads to.
enum Outcome {
    Stay,
    Tab(usize),
    Quit,
}

/// Take what was typed for a row. The message is shown in the footer.
fn registry_accept(store: &Store, row: &RegistryRow, value: &str) -> crate::Result<String> {
    use RegistryRow::*;
    let mut registries = store.registries()?;
    let valid = crate::plugins::valid_name;
    let message = match row {
        AddRegistry | Name(_) => {
            if !valid(value) {
                return Err(
                    "Use lower-case letters, digits, - and _, starting with a letter".into(),
                );
            }
            if registries.contains_key(value) {
                return Err(format!("There is a registry {value} already").into());
            }
            if let Name(old) = row {
                // The stored login moves with the name, in the same change.
                return Ok(match store.rename_registry(old, value)? {
                    true => format!("Registry {value}"),
                    false => format!(
                        "Registry {value}. The berth server drops a login with the old name: enter its password again"
                    ),
                });
            }
            registries.insert(value.to_owned(), json!({"type": "nexus", "url": ""}));
            format!("Registry {value}")
        }
        Address(name) => {
            let registry = &mut registries[name.as_str()];
            if registry["type"] != "docker"
                && !value.starts_with("https://")
                && !value.starts_with("http://")
            {
                return Err("Start the address with https://".into());
            }
            registry["url"] = json!(value.trim_end_matches('/'));
            "Address saved".into()
        }
        User(name) => {
            match value {
                "" => registries[name.as_str()]
                    .as_object_mut()
                    .and_then(|registry| registry.remove("username")),
                user => registries[name.as_str()]
                    .as_object_mut()
                    .and_then(|registry| registry.insert("username".into(), json!(user))),
            };
            "User name saved".into()
        }
        Password(name) => {
            if value.is_empty() {
                return Ok("Nothing entered, kept as it was".into());
            }
            store.set_secret(&format!("registry/{name}"), Some(value))?;
            return Ok("Stored".into());
        }
        AddRepository(name) | Repository(name, _) => {
            if value.is_empty() || value.contains(['/', ' ']) {
                return Err("The repository's name on the server, without / or spaces".into());
            }
            let registry = &mut registries[name.as_str()];
            let mut repositories = registry["repositories"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            if repositories.iter().any(|known| known["name"] == value) {
                return Err(format!("{value} is listed already").into());
            }
            match row {
                Repository(_, index) => repositories[*index]["name"] = json!(value),
                _ => repositories.push(json!({"format": "npm", "name": value})),
            }
            registry["repositories"] = json!(repositories);
            format!("Repository {value}")
        }
        DockerAddress(name, index) => {
            let address = value.split_once("://").map_or(value, |(_, rest)| rest);
            let repository = &mut registries[name.as_str()]["repositories"][*index];
            match address.trim_end_matches('/') {
                "" => repository
                    .as_object_mut()
                    .and_then(|repository| repository.remove("address")),
                address => repository
                    .as_object_mut()
                    .and_then(|repository| repository.insert("address".into(), json!(address))),
            };
            "Docker address saved".into()
        }
        Type(_) | Format(..) => String::new(),
    };
    store.save_registries(registries)?;
    Ok(message)
}

/// Handle a key on the Registries tab.
fn registries_key(
    view: &mut RegistryView,
    paths: &Paths,
    key: KeyEvent,
    status: &mut String,
) -> Outcome {
    use RegistryRow::*;
    let store = match Store::new(paths) {
        Ok(store) => store,
        Err(error) => {
            *status = error.to_string();
            return leave_tab(key, tabs::REGISTRIES);
        }
    };
    let rows = registry_rows(Some(&store));
    view.selected = view.selected.min(rows.len() - 1);
    let row = rows[view.selected].0.clone();
    if let Some(input) = &mut view.editor {
        match key.code {
            KeyCode::Esc => view.editor = None,
            KeyCode::Enter => {
                let typed = input.value().trim().to_owned();
                match registry_accept(&store, &row, &typed) {
                    Ok(message) => {
                        *status = message;
                        view.editor = None;
                        // A new or renamed registry stays selected, ready for its kind and address.
                        if matches!(row, AddRegistry | Name(_)) {
                            let named = registry_rows(Some(&store))
                                .iter()
                                .position(|(row, _)| *row == Name(typed.clone()));
                            view.selected = named.unwrap_or(view.selected);
                        }
                    }
                    Err(error) => *status = error.to_string(),
                }
            }
            _ => {
                input.handle_event(&Event::Key(key));
            }
        }
        return Outcome::Stay;
    }
    let armed = std::mem::take(&mut view.armed);
    let report = std::mem::take(&mut view.report);
    if key.code == KeyCode::Char('c') {
        match row.registry() {
            Some(name) => {
                view.report = crate::registry::check(&store, name)
                    .unwrap_or_else(|error| vec![format!("✗ {error}")]);
                view.report.insert(0, format!("Checked {name}"));
                *status = format!("Checked {name}");
            }
            None => *status = "Select a registry to check".into(),
        }
        return Outcome::Stay;
    }
    // Moving on to something else drops the report; other keys keep it on screen.
    if !matches!(key.code, KeyCode::Up | KeyCode::Down) {
        view.report = report;
    }
    let change = |edit: &dyn Fn(&mut serde_json::Map<String, serde_json::Value>)| {
        let mut registries = store.registries().unwrap_or_default();
        edit(&mut registries);
        store
            .save_registries(registries)
            .map_or_else(|error| error.to_string(), |()| String::new())
    };
    let cycle = |all: &[&str], current: &str| {
        let index = all
            .iter()
            .position(|known| *known == current)
            .map_or(0, |index| index + 1);
        all[index % all.len()].to_owned()
    };
    match key.code {
        KeyCode::Up => view.selected = view.selected.saturating_sub(1),
        KeyCode::Down => view.selected = (view.selected + 1).min(rows.len() - 1),
        KeyCode::Enter | KeyCode::Char(' ') => match &row {
            Type(name) => {
                *status = change(&|registries| {
                    let next = cycle(
                        crate::registry::TYPES,
                        registries[name.as_str()]["type"].as_str().unwrap_or(""),
                    );
                    registries[name.as_str()]["type"] = json!(next);
                })
            }
            Format(name, index) => {
                *status = change(&|registries| {
                    let repository = &mut registries[name.as_str()]["repositories"][*index];
                    let next = cycle(
                        crate::registry::FORMATS,
                        repository["format"].as_str().unwrap_or(""),
                    );
                    repository["format"] = json!(next);
                })
            }
            // Editing starts from the current value, except for what is secret or new.
            other => {
                let registries = store.registries().unwrap_or_default();
                let current = match other {
                    Name(name) => name.clone(),
                    Address(name) => registries[name.as_str()]["url"]
                        .as_str()
                        .unwrap_or("")
                        .to_owned(),
                    User(name) => registries[name.as_str()]["username"]
                        .as_str()
                        .unwrap_or("")
                        .to_owned(),
                    Repository(name, index) => {
                        registries[name.as_str()]["repositories"][*index]["name"]
                            .as_str()
                            .unwrap_or("")
                            .to_owned()
                    }
                    DockerAddress(name, index) => registries[name.as_str()]["repositories"][*index]
                        ["address"]
                        .as_str()
                        .unwrap_or("")
                        .to_owned(),
                    _ => String::new(),
                };
                view.editor = Some(Input::new(current));
            }
        },
        KeyCode::Char('x') | KeyCode::Delete => match &row {
            Name(name) if !armed => {
                view.armed = true;
                *status = format!("x again removes {name} and its stored login");
            }
            Name(name) => {
                *status = change(&|registries| {
                    registries.remove(name.as_str());
                });
                let _ = store.set_secret(&format!("registry/{name}"), None);
            }
            User(name) => {
                *status = change(&|registries| {
                    if let Some(registry) = registries[name.as_str()].as_object_mut() {
                        registry.remove("username");
                    }
                })
            }
            Password(name) => {
                *status = match store.set_secret(&format!("registry/{name}"), None) {
                    Ok(()) => "Forgotten".into(),
                    Err(error) => error.to_string(),
                }
            }
            Repository(name, index) => {
                *status = change(&|registries| {
                    if let Some(repositories) =
                        registries[name.as_str()]["repositories"].as_array_mut()
                    {
                        repositories.remove(*index);
                    }
                })
            }
            _ => {}
        },
        _ => return leave_tab(key, tabs::REGISTRIES),
    }
    Outcome::Stay
}

/// Open one of the home screen's tabs.
fn open_tab(home: &mut Home, tab: usize) {
    home.mode = match tab {
        tabs::PROVISIONERS => HomeMode::Provisioners(Default::default()),
        tabs::SETTINGS => HomeMode::Settings(0, None),
        tabs::REGISTRIES => HomeMode::Registries(RegistryView::default()),
        tabs::SECRETS => HomeMode::Secrets(SecretView::default()),
        tabs::PLUGINS => HomeMode::Plugins(PluginView::default()),
        _ => HomeMode::Hosts,
    };
}

/// Whether the server's last word is that a host answers: a small marker in
/// front of its name. Nothing without a server, and a hollow one when the
/// server has not said.
fn host_marker(known: Option<&berth_core::server::Known>, name: &str) -> Span<'static> {
    match known.map(|known| known.hosts.get(name)) {
        None => Span::raw(""),
        Some(Some(reach)) if reach.reachable => Span::styled("● ", Style::new().fg(theme::GOOD)),
        Some(Some(_)) => Span::styled("● ", Style::new().fg(theme::BAD)),
        Some(None) => Span::styled("○ ", Style::new().fg(theme::MUTED)),
    }
}

/// What the Information panel says of the selected host on the Hosts tab.
fn host_help(home: &Home) -> String {
    let Some(known) = &home.known else {
        return String::new();
    };
    let mut help = String::new();
    if let Some(note) = server_note(known) {
        help = format!("{note}\n\n");
    }
    if let Some((name, _)) = home.hosts.get(home.selected)
        && known.hosts.get(name).is_some_and(|reach| reach.rotating)
    {
        help.push_str(&format!("A key rotation has not reached {name} yet: it is offered the old key and the new one until it has. Run the rotation again when it answers.\n\n"));
    }
    help.push_str(&match home.hosts.get(home.selected) {
        None => "● answers, ● does not answer, ○ not known yet: what the berth server found when it last asked.".to_owned(),
        Some((name, _)) => match known.hosts.get(name) {
            Some(reach) if reach.reachable => match &reach.version {
                Some(version) => format!("{name} answers, with berthd {version}."),
                None => format!("{name} answers."),
            },
            Some(reach) => format!(
                "{name} does not answer the berth server.\n\n{}",
                reach.error.as_deref().unwrap_or("It gave no reason.")
            ),
            None => format!("The berth server has not said yet whether {name} answers."),
        },
    });
    help
}

struct Home {
    hosts: Vec<(String, String)>,
    selected: usize,
    mode: HomeMode,
    status: String,
    /// The water moves with the time since then.
    started: std::time::Instant,
    /// The link that opens the server's page, as a QR code, while it is shown.
    qr: Option<berth_tui::qr::Overlay>,
    /// The SSH keys of this berth's key ring, or of the berth server it
    /// uses, by name and public half, and that server. Asked for when
    /// Settings is first shown; empty when unknown.
    berth_key: Option<RingKeys>,
    /// The tailnet settings of the berth server this berth uses, which are
    /// the server's to keep. Asked for with the key.
    served: Option<serde_json::Map<String, serde_json::Value>>,
    /// What the last mirror found of the berth server and its hosts, with a
    /// server set.
    known: Option<berth_core::server::Known>,
    /// Whether a mirror is being made now.
    asking: bool,
    /// `r` was pressed on the Hosts tab: the loop reloads, which needs the mirror.
    refresh: bool,
    /// The hosts of this berth a key rotation has not reached yet.
    rotating: Vec<String>,
}

/// The status line while a mirror is being made, if nothing else is said.
const ASKING: &str = "Asking the berth server…";

/// What the last mirror left, for the home screen.
fn known_of(paths: &Paths) -> Option<berth_core::server::Known> {
    berth_core::server::Server::configured(paths).map(|server| server.known(paths))
}

/// The line for a server older than this berth.
fn server_note(known: &berth_core::server::Known) -> Option<String> {
    let version = known.version.as_deref()?;
    crate::update::newer(berth_api::VERSION, version).then(|| {
        format!(
            "The berth server runs {version} and this berth {}. Update the berth server.",
            berth_api::VERSION
        )
    })
}

/// Take in a finished mirror: the hosts and what is known of them are read
/// again, and what came from the server on the Settings tab is asked for again.
fn home_mirror(home: &mut Home, paths: &Paths, mirroring: &mut Option<crate::mirror::Mirroring>) {
    if std::mem::take(&mut home.refresh) {
        match mirroring {
            Some(mirroring) => {
                if !mirroring.start() {
                    home.status = "Still asking the berth server".into();
                }
            }
            None => {
                reload_hosts(home, paths);
                home.status = "Read the hosts again".into();
            }
        }
    }
    let Some(mirroring) = mirroring else {
        return;
    };
    if let Some(made) = mirroring.finished() {
        match made {
            Ok(()) => reload_hosts(home, paths),
            Err(error) => home.status = error,
        }
    }
    home.asking = mirroring.running();
}

/// Read the saved hosts and what is known of them again, keeping the selected
/// host selected.
fn reload_hosts(home: &mut Home, paths: &Paths) {
    let chosen = home.hosts.get(home.selected).map(|(name, _)| name.clone());
    home.hosts = paths.hosts();
    home.selected = chosen
        .and_then(|name| home.hosts.iter().position(|(known, _)| *known == name))
        .unwrap_or(home.selected.min(home.hosts.len()));
    home.known = known_of(paths);
    home.rotating = rotating(paths);
    home.berth_key = None;
    home.served = None;
}

/// The hosts of this berth that a key rotation has not reached yet: they
/// are offered both keys until it has. A server's hosts are the server's.
fn rotating(paths: &Paths) -> Vec<String> {
    if berth_core::server::Server::configured(paths).is_some() {
        return vec![];
    }
    let Ok(store) = berth_core::store::shared(paths) else {
        return vec![];
    };
    // Nothing to look at while no key is being replaced.
    let pending = berth_core::keys::ring::names(&store).is_ok_and(|names| {
        names.iter().any(|name| {
            berth_core::keys::ring::read(&store, name, true).is_ok_and(|next| next.is_some())
        })
    });
    if !pending {
        return vec![];
    }
    berth_core::host::Host::all(paths)
        .into_iter()
        .filter(|host| {
            berth_core::keys::contact::waiting(&store, &host.place, &host.config)
                .is_ok_and(|keys| !keys.is_empty())
        })
        .map(|host| host.name)
        .collect()
}

/// The keys of a key ring by name and public half, and the berth server
/// whose ring it is.
type RingKeys = (Vec<(String, String)>, Option<String>);

/// The rows of the Settings tab after berth's own settings.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SettingsRow {
    /// A key of the ring, by its place in the list.
    Key(usize),
    Connect,
}

fn settings_extras(home: &Home, paths: &Paths) -> Vec<SettingsRow> {
    let mut extras = vec![];
    if let Some((keys, _)) = &home.berth_key {
        extras.extend((0..keys.len()).map(SettingsRow::Key));
    }
    if connect_link(paths).is_some() {
        extras.push(SettingsRow::Connect);
    }
    extras
}

/// The SSH keys hosts set up from here are logged in to with: the server's
/// for a berth that uses one, which only someone who may set the server up
/// is told.
fn berth_key(paths: &Paths) -> RingKeys {
    let server = berth_core::server::Server::configured(paths);
    let keys = match &server {
        Some(server) => crate::key_command::listed(server),
        None => crate::key_command::local(paths),
    };
    let keys = keys
        .unwrap_or_default()
        .into_iter()
        .map(|key| (key.name, key.public))
        .collect();
    (keys, server.map(|server| server.url))
}

/// The tailnet settings of the berth server this berth uses, by key.
fn served_settings(paths: &Paths) -> Option<serde_json::Map<String, serde_json::Value>> {
    let server = berth_core::server::Server::configured(paths)?;
    let listed = server.call("settings.list", json!({}), None).ok()?;
    Some(
        listed
            .as_array()?
            .iter()
            .filter_map(|setting| {
                let key = setting["key"].as_str()?;
                berth_core::tailnet::SETTINGS
                    .contains(&key)
                    .then(|| (key.to_owned(), setting["value"].clone()))
            })
            .collect(),
    )
}

/// Change a setting: on the berth server this berth uses for one that is
/// the server's, here otherwise.
fn change_setting(home: &mut Home, paths: &Paths, key: &str, value: &str) -> crate::Result<()> {
    match (
        berth_core::server::Server::configured(paths),
        &mut home.served,
    ) {
        (Some(server), Some(served)) if berth_core::tailnet::SETTINGS.contains(&key) => {
            server.call(
                "settings.set",
                json!({"key": key}),
                Some(json!({"value": value})),
            )?;
            served.insert(key.into(), json!(value));
            Ok(())
        }
        _ => paths.set(key, value),
    }
}

/// What a setting is called on screen.
fn setting_label(key: &str) -> &str {
    match key {
        "source" => "Provisioning source",
        "default_host" => "Default host",
        "iso_dir" => "Installer ISO folder",
        other => other,
    }
}

/// A setting's stored value, and what applies when it is not set. `served`
/// has the settings a berth server keeps.
fn setting(
    paths: &Paths,
    served: Option<&serde_json::Map<String, serde_json::Value>>,
    key: &str,
) -> (String, String) {
    // One at a time, so a store that does not open leaves the settings of the file.
    let stored = match served {
        Some(served) if berth_core::tailnet::SETTINGS.contains(&key) => served
            .get(key)
            .and_then(|value| value.as_str())
            .map(str::to_owned),
        _ => paths.setting_with(key, None),
    };
    let fallback = match key {
        "source" => paths
            .source()
            .map(|dir| dir.display().to_string())
            .unwrap_or_else(|_| "not found".into()),
        "iso_dir" => paths.iso_dir().display().to_string(),
        _ => "none".into(),
    };
    (stored.unwrap_or_default(), fallback)
}

/// The berth server this berth uses, and the link that opens its page signed
/// in with its token. None without a token: a login is not handed on.
fn connect_link(paths: &Paths) -> Option<(String, String)> {
    let server = berth_core::server::Server::configured(paths)?;
    let link = berth_tui::qr::served_link(&server.url, server.token.as_deref()?)?;
    Some((server.url, link))
}

/// Clears a status line once it has been showing for a while.
#[derive(Default)]
struct Fade {
    text: String,
    since: Option<std::time::Instant>,
}

impl Fade {
    const SHOWN: std::time::Duration = std::time::Duration::from_secs(6);

    /// Clear `status` when its time is up. Otherwise, how long it has left.
    fn check(&mut self, status: &mut String) -> Option<std::time::Duration> {
        if *status != self.text {
            self.text = status.clone();
            self.since = Some(std::time::Instant::now());
        }
        if status.is_empty() {
            return None;
        }
        let shown = self.since.map_or(Self::SHOWN, |since| since.elapsed());
        if shown >= Self::SHOWN {
            status.clear();
            self.text.clear();
            return None;
        }
        Some(Self::SHOWN - shown)
    }
}

/// While the mirror runs the host's own screen says so, and a failure is told.
/// What the host shows is read again unless it holds changes not yet saved.
fn host_mirror(app: &mut App, mirroring: &mut Option<crate::mirror::Mirroring>) {
    let Some(mirroring) = mirroring else {
        return;
    };
    if let Some(made) = mirroring.finished() {
        match made {
            Ok(()) => {
                if app.status == ASKING {
                    app.status.clear();
                }
                if let Some(said) = app.take_in_mirror() {
                    app.status = said;
                }
            }
            Err(error) => app.status = error,
        }
    } else if app.status.is_empty() {
        app.status = ASKING.into();
    }
}

/// The home screen: pick a saved host, name a new one or change berth's own
/// settings. `None` quits.
pub fn home(
    terminal: &mut DefaultTerminal,
    paths: &Paths,
    mirroring: &mut Option<crate::mirror::Mirroring>,
) -> Result<Option<String>> {
    // A blinking cursor restarts its blink with every frame of the water, which
    // reads as flicker; a steady one does not.
    use crossterm::cursor::SetCursorStyle;
    crossterm::execute!(std::io::stdout(), SetCursorStyle::SteadyBar)?;
    let chosen = home_loop(terminal, paths, mirroring);
    crossterm::execute!(std::io::stdout(), SetCursorStyle::BlinkingBar)?;
    chosen
}

fn home_loop(
    terminal: &mut DefaultTerminal,
    paths: &Paths,
    mirroring: &mut Option<crate::mirror::Mirroring>,
) -> Result<Option<String>> {
    let hosts = paths.hosts();
    let preferred = setting(paths, None, "default_host").0;
    let mut home = Home {
        selected: hosts
            .iter()
            .position(|(name, _)| *name == preferred)
            .unwrap_or(0),
        hosts,
        mode: HomeMode::Hosts,
        status: String::new(),
        started: std::time::Instant::now(),
        qr: None,
        berth_key: None,
        served: None,
        known: None,
        asking: false,
        refresh: false,
        rotating: rotating(paths),
    };
    let mut hits = vec![];
    let mut fade = Fade::default();
    home.known = known_of(paths);
    loop {
        home_mirror(&mut home, paths, mirroring);
        fade.check(&mut home.status);
        if matches!(home.mode, HomeMode::Settings(..)) && home.berth_key.is_none() {
            home.berth_key = Some(berth_key(paths));
            home.served = served_settings(paths);
        }
        if let HomeMode::Plugins(view) = &mut home.mode {
            view.load(paths);
            view.pump(&mut home.status);
        }
        if let HomeMode::Provisioners(view) = &mut home.mode {
            view.load(paths);
            view.pump(paths, &mut home.status);
        }
        // The water redraws many times a second. Each frame is applied at once,
        // so the text cursor is not seen travelling to the water and back.
        crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::BeginSynchronizedUpdate
        )?;
        let drawn = terminal.draw(|frame| draw_home(frame, paths, &home, &mut hits));
        crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::EndSynchronizedUpdate
        )?;
        drawn?;
        if !event::poll(std::time::Duration::from_millis(50))? {
            continue;
        }
        let event = hint_click(event::read()?);
        if home.qr.is_some() {
            if let Some(event) = crate::keys::event(&event) {
                berth_tui::qr::take(&mut home.qr, &vec![], &event, &mut None);
            }
            continue;
        }
        let left = match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    return Ok(None);
                }
                home_key(&mut home, paths, key, &mut Desktop)
            }
            Event::Mouse(mouse) => home_mouse(&mut home, paths, mouse, &hits, &mut Desktop),
            Event::Paste(text) => {
                if let Some(input) = home_editor(&mut home.mode) {
                    for ch in text.chars().filter(|ch| !ch.is_control()) {
                        input.handle(InputRequest::InsertChar(ch));
                    }
                }
                None
            }
            _ => None,
        };
        if let Some(host) = left {
            return Ok(host);
        }
    }
}

/// The text field of the home screen that is being edited, if one is.
fn home_editor(mode: &mut HomeMode) -> Option<&mut Input> {
    match mode {
        HomeMode::Naming(input) => Some(input),
        HomeMode::Settings(_, editor) => editor.as_mut(),
        HomeMode::Registries(view) => view.editor.as_mut(),
        HomeMode::Secrets(view) => view.editor.as_mut(),
        HomeMode::Plugins(view) => view.editor.as_mut(),
        HomeMode::Hosts | HomeMode::Provisioners(_) => None,
    }
}

/// Handle the mouse on the home screen: the wheel moves the selection, a click
/// switches tab or chooses a row.
fn home_mouse(
    home: &mut Home,
    paths: &Paths,
    mouse: MouseEvent,
    hits: &[(Rect, HomeTarget)],
    platform: &mut dyn Platform,
) -> Option<Option<String>> {
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    match mouse.kind {
        MouseEventKind::ScrollUp => home_key(home, paths, key(KeyCode::Up), platform),
        MouseEventKind::ScrollDown => home_key(home, paths, key(KeyCode::Down), platform),
        MouseEventKind::Down(MouseButton::Left) => {
            let (_, target) = hits
                .iter()
                .find(|(rect, _)| inside(*rect, mouse.column, mouse.row))?;
            match *target {
                HomeTarget::Tab(1) if matches!(home.mode, HomeMode::Settings(..)) => None,
                HomeTarget::Tab(2) if matches!(home.mode, HomeMode::Registries(..)) => None,
                HomeTarget::Tab(3) if matches!(home.mode, HomeMode::Secrets(..)) => None,
                HomeTarget::Tab(4) if matches!(home.mode, HomeMode::Plugins(..)) => None,
                HomeTarget::Tab(5) if matches!(home.mode, HomeMode::Provisioners(..)) => None,
                HomeTarget::Tab(tab) => {
                    open_tab(home, tab);
                    None
                }
                HomeTarget::Row(index) => {
                    // A click elsewhere drops what was being typed.
                    match &mut home.mode {
                        HomeMode::Registries(view) => {
                            view.selected = index;
                            view.editor = None;
                        }
                        HomeMode::Secrets(view) => {
                            view.selected = index;
                            view.editor = None;
                            view.naming = None;
                        }
                        HomeMode::Plugins(view) => {
                            view.tab.selected = index;
                            view.editor = None;
                        }
                        // A click selects a row, and on the row selected chooses it.
                        HomeMode::Provisioners(view) => {
                            if view.typing() {
                                return None;
                            }
                            let before = view.tab.selected_row();
                            view.tab.select_row(index);
                            if view.tab.page.is_some() && before != index {
                                return None;
                            }
                        }
                        HomeMode::Settings(selected, editor) => {
                            *selected = index;
                            *editor = None;
                        }
                        mode => {
                            *mode = HomeMode::Hosts;
                            home.selected = index;
                        }
                    }
                    home_key(home, paths, key(KeyCode::Enter), platform)
                }
            }
        }
        _ => None,
    }
}

/// Handle a key on the home screen. `Some` leaves it: with a host, or to quit.
fn home_key(
    home: &mut Home,
    paths: &Paths,
    key: KeyEvent,
    platform: &mut dyn Platform,
) -> Option<Option<String>> {
    let extras = match home.mode {
        HomeMode::Settings(..) => settings_extras(home, paths),
        _ => vec![],
    };
    let extra = |selected: usize| {
        let index = selected.checked_sub(crate::paths::SETTINGS.len())?;
        extras.get(index).copied()
    };
    if let Some(typed) = crate::keys::key(&key)
        && berth_tui::screen::is_copy_field(&typed)
        && let Some(input) = home_editor(&mut home.mode)
    {
        home.status = berth_tui::screen::copy_field(platform, input);
        return None;
    }
    let outcome = match &mut home.mode {
        HomeMode::Registries(view) => Some(registries_key(view, paths, key, &mut home.status)),
        HomeMode::Secrets(view) => Some(secrets_key(view, paths, key, &mut home.status)),
        HomeMode::Plugins(view) => Some(plugins_key(view, paths, key, &mut home.status)),
        HomeMode::Provisioners(view) => {
            Some(match view.key(paths, key, &mut home.status, platform) {
                crate::provisioner_tab::Left::Stay => Outcome::Stay,
                crate::provisioner_tab::Left::Tab(tab) => Outcome::Tab(tab),
                crate::provisioner_tab::Left::Quit => Outcome::Quit,
            })
        }
        _ => None,
    };
    match outcome {
        Some(Outcome::Stay) => return None,
        Some(Outcome::Tab(tab)) => {
            open_tab(home, tab);
            return None;
        }
        Some(Outcome::Quit) => return Some(None),
        None => {}
    }
    match &mut home.mode {
        HomeMode::Registries(_)
        | HomeMode::Secrets(_)
        | HomeMode::Plugins(_)
        | HomeMode::Provisioners(_) => {}
        HomeMode::Naming(input) => match key.code {
            KeyCode::Esc => home.mode = HomeMode::Hosts,
            KeyCode::Enter => {
                let name = input.value().trim().to_owned();
                if name.contains('/') || name.ends_with(".json") {
                    home.status = "Use letters, digits, - and _".into();
                } else if let Err(error) = berth_core::profiles::Place::resolve(paths, &name) {
                    home.status = error.to_string();
                } else {
                    return Some(Some(name));
                }
            }
            _ => {
                input.handle_event(&Event::Key(key));
            }
        },
        HomeMode::Settings(selected, editor @ Some(_)) => match key.code {
            KeyCode::Esc => *editor = None,
            KeyCode::Enter => {
                let (name, _) = crate::paths::SETTINGS[*selected];
                let value = editor.take().unwrap_or_default();
                home.status = match change_setting(home, paths, name, value.value().trim()) {
                    Ok(()) => format!("Saved {}", setting_label(name)),
                    Err(error) => error.to_string(),
                };
            }
            _ => {
                if let Some(input) = editor {
                    input.handle_event(&Event::Key(key));
                }
            }
        },
        HomeMode::Settings(selected, editor) => match key.code {
            KeyCode::Up => *selected = selected.saturating_sub(1),
            KeyCode::Down => {
                // The key of berth and the row that opens the server's page
                // come after the settings.
                let last = crate::paths::SETTINGS.len() + extras.len() - 1;
                *selected = (*selected + 1).min(last);
            }
            KeyCode::Enter if *selected < crate::paths::SETTINGS.len() => {
                let (name, _) = crate::paths::SETTINGS[*selected];
                *editor = Some(Input::new(setting(paths, home.served.as_ref(), name).0));
            }
            KeyCode::Enter | KeyCode::Char('c')
                if matches!(extra(*selected), Some(SettingsRow::Key(_))) =>
            {
                if let (Some(SettingsRow::Key(index)), Some((keys, _))) =
                    (extra(*selected), &home.berth_key)
                    && let Some((_, public)) = keys.get(index)
                {
                    home.status = berth_tui::berth_key::copy(platform, public);
                }
            }
            KeyCode::Enter | KeyCode::Char('r')
                if key.code == KeyCode::Enter || key.modifiers.contains(KeyModifiers::ALT) =>
            {
                if extra(*selected) == Some(SettingsRow::Connect)
                    && let Some((_, link)) = connect_link(paths)
                {
                    home.qr = Some(berth_tui::qr::Overlay::connect(link));
                }
            }
            _ => match leave_tab(key, tabs::SETTINGS) {
                Outcome::Tab(tab) => open_tab(home, tab),
                Outcome::Quit => return Some(None),
                Outcome::Stay => {}
            },
        },
        HomeMode::Hosts => match key.code {
            KeyCode::Up => home.selected = home.selected.saturating_sub(1),
            KeyCode::Down => home.selected = (home.selected + 1).min(home.hosts.len()),
            KeyCode::Char('n') => home.mode = HomeMode::Naming(Input::default()),
            KeyCode::Char('s') => open_tab(home, tabs::SETTINGS),
            KeyCode::Char('r') => home.refresh = true,
            KeyCode::Char('1'..='6') | KeyCode::Tab | KeyCode::BackTab => {
                if let Outcome::Tab(tab) = leave_tab(key, tabs::HOSTS) {
                    open_tab(home, tab);
                }
            }
            KeyCode::Char('d') => {
                if let Some((name, _)) = home.hosts.get(home.selected) {
                    home.status = match paths.set("default_host", name) {
                        Ok(()) => format!("{name} is selected first from now on"),
                        Err(error) => error.to_string(),
                    };
                }
            }
            KeyCode::Enter => match home.hosts.get(home.selected) {
                Some((name, _)) => return Some(Some(name.clone())),
                None => home.mode = HomeMode::Naming(Input::default()),
            },
            KeyCode::Char('q') | KeyCode::Esc => return Some(None),
            _ => {}
        },
    }
    None
}

fn draw_home(frame: &mut Frame, paths: &Paths, home: &Home, hits: &mut Vec<(Rect, HomeTarget)>) {
    hits.clear();
    clear_hints();
    frame.render_widget(theme::backdrop(), frame.area());
    let area = frame.area().inner(Margin {
        horizontal: 2,
        vertical: 1,
    });
    let [header, _, logo, main, water, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(4),
        Constraint::Min(1),
        Constraint::Length(2),
        Constraint::Length(1),
    ])
    .areas(area);
    let tab = match home.mode {
        HomeMode::Provisioners(..) => tabs::PROVISIONERS,
        HomeMode::Settings(..) => tabs::SETTINGS,
        HomeMode::Registries(..) => tabs::REGISTRIES,
        HomeMode::Secrets(..) => tabs::SECRETS,
        HomeMode::Plugins(..) => tabs::PLUGINS,
        _ => tabs::HOSTS,
    };
    // The Information panel is for the settings, and for a host with a server's word on it.
    let settings = tab != 0 || home.known.is_some();
    let tabs = draw_header(
        frame,
        header,
        &tabs::NAMES,
        tab,
        home.known
            .as_ref()
            .and_then(server_note)
            .as_deref()
            .unwrap_or(""),
    );
    hits.extend(
        tabs.into_iter()
            .enumerate()
            .map(|(tab, rect)| (rect, HomeTarget::Tab(tab))),
    );
    // What is typed for a password is not shown.
    let mut masked = false;
    frame.render_widget(Paragraph::new(theme::logo()), logo);
    frame.render_widget(
        Paragraph::new(theme::swell(
            water.width,
            home.started.elapsed().as_secs_f32(),
        )),
        water,
    );

    // Settings use the same arrangement as a host's tabs: rows on the left with
    // the editor under them, what the selected row means on the right.
    let [left, right] = Layout::horizontal([
        Constraint::Percentage(if settings { 64 } else { 100 }),
        Constraint::Min(0),
    ])
    .areas(main);
    let muted = Style::new().fg(theme::MUTED);
    let (rows, selected, editor, help, keys): (Vec<Line>, usize, _, String, _) = match &home.mode {
        HomeMode::Settings(selected, editor) => {
            let connect = connect_link(paths);
            let extras = settings_extras(home, paths);
            let (keys, server) = home.berth_key.clone().unwrap_or_default();
            let mut rows: Vec<Line> = crate::paths::SETTINGS
                .iter()
                .map(|(name, _)| {
                    let (stored, fallback) = setting(paths, home.served.as_ref(), name);
                    if stored.is_empty() {
                        Line::from(vec![
                            Span::styled(
                                format!("{}  ", setting_label(name)),
                                Style::new().fg(theme::TEXT),
                            ),
                            Span::styled(format!("Default · {fallback}"), muted.italic()),
                        ])
                    } else if crate::paths::is_secret(name) {
                        // A secret is not shown whole in a list; its field shows it.
                        crate::app::value_line(
                            setting_label(name),
                            &berth_tui::screen::short_secret(&stored),
                            Kind::Text,
                        )
                    } else {
                        crate::app::value_line(setting_label(name), &stored, Kind::Text)
                    }
                })
                .collect();
            for extra in &extras {
                rows.push(match extra {
                    SettingsRow::Key(index) => {
                        let (name, public) = &keys[*index];
                        berth_tui::berth_key::row(name, public)
                    }
                    SettingsRow::Connect => Line::from(vec![
                        Span::styled("Open on another device  ", Style::new().fg(theme::TEXT)),
                        Span::styled("QR code and link, with the token", muted),
                    ]),
                });
            }
            let selected = (*selected).min(rows.len() - 1);
            let extra = selected
                .checked_sub(crate::paths::SETTINGS.len())
                .and_then(|index| extras.get(index));
            let (help, keys) = match (crate::paths::SETTINGS.get(selected), extra, &connect) {
                (Some((name, help)), ..) => (
                    format!(
                        "{}.\n\nLeave it empty to use the default. Also: berth config {name} VALUE\n\nKept {}",
                        berth_tui::screen::describe_setting(help),
                        match (&home.served, berth_core::tailnet::SETTINGS.contains(name)) {
                            (Some(_), true) => format!(
                                "on the berth server at {server}",
                                server = server.as_deref().unwrap_or("")
                            ),
                            _ if crate::paths::is_local(name) => {
                                format!("in {}", paths.settings_file().display())
                            }
                            _ => format!(
                                "in berth's store, the {}",
                                berth_core::store::Installation::of(paths).target.describe()
                            ),
                        }
                    ),
                    "↑↓ select · Enter change · Tab registries · q quit",
                ),
                (None, Some(SettingsRow::Key(index)), _) => (
                    berth_tui::berth_key::help(&keys[*index].0, &keys[*index].1, server.as_deref()),
                    "↑↓ select · Enter copy the key · Tab registries · q quit",
                ),
                (None, Some(SettingsRow::Connect), Some((url, _))) => (
                    berth_tui::qr::connect_help(url, true),
                    "↑↓ select · Enter show the QR code · Tab registries · q quit",
                ),
                _ => (String::new(), ""),
            };
            (
                rows,
                selected,
                editor.as_ref().map(|input| (" Edit ", input)),
                help,
                if editor.is_some() {
                    "Enter accept · Esc cancel · Ctrl+O copy"
                } else {
                    keys
                },
            )
        }
        HomeMode::Registries(view) => {
            let store = Store::new(paths);
            let listed = registry_rows(store.as_ref().ok());
            let selected = view.selected.min(listed.len() - 1);
            let row = &listed[selected].0;
            masked = matches!(row, RegistryRow::Password(_));
            let title = match row {
                RegistryRow::AddRegistry => " Name of the new registry ",
                RegistryRow::AddRepository(_) => " Name of the repository on the server ",
                RegistryRow::Password(_) => " Password or token ",
                _ => " Edit ",
            };
            let help = match view.report.is_empty() {
                true => format!(
                    "{}\n\nc checks that this registry answers and accepts its login.\n\n{}",
                    row.help(),
                    match &store {
                        Ok(store) => kept_in(paths, store),
                        Err(error) => error.to_string(),
                    }
                ),
                false => view.report.join("\n\n"),
            };
            (
                listed.iter().map(|(_, line)| line.clone()).collect(),
                selected,
                view.editor.as_ref().map(|input| (title, input)),
                help,
                if view.editor.is_some() {
                    "Enter accept · Esc cancel · Ctrl+O copy"
                } else {
                    "↑↓ select · Enter change · c check · x remove · Tab secrets · q quit"
                },
            )
        }
        HomeMode::Secrets(view) => {
            let store = Store::new(paths);
            let names = store
                .as_ref()
                .map(|store| store.secret_names())
                .unwrap_or_default();
            let selected = view.selected.min(names.len());
            let mut rows: Vec<Line> = names
                .iter()
                .map(|name| {
                    Line::from(vec![
                        Span::styled(format!("{name}  "), Style::new().fg(theme::TEXT)),
                        Span::styled("● stored", Style::new().fg(theme::GOOD)),
                    ])
                })
                .collect();
            rows.push(Line::styled("+ Add secret", Style::new().fg(theme::ACCENT)));
            // A value is not shown while it is typed; a new secret's name is.
            masked = view.naming.is_some() || selected < names.len();
            let title = match (&view.naming, names.get(selected)) {
                (Some(_), _) => " Value of the new secret ",
                (None, Some(_)) => " New value ",
                (None, None) => " Name of the new secret ",
            };
            let about = match names.get(selected) {
                Some(name) if let Some(help) = berth_tui::setup::own_secret_help(name) => help,
                Some(_) => {
                    "A token, password or key file. Enter replaces its value, x twice removes it. Values are never shown."
                }
                None => {
                    "Add a secret: first a short name (lower-case letters, digits, - and _), then its value. Paste works. The name tailnet_api_key is for the API key of the tailnet's control server, which berth keeps for itself."
                }
            };
            let help = format!(
                "{about}\n\nWhich host gets a secret, and whether as an environment variable or a file, is chosen on the Secrets page of that host.\n\n{}",
                match &store {
                    Ok(store) => kept_in(paths, store),
                    Err(error) => error.to_string(),
                }
            );
            (
                rows,
                selected,
                view.editor.as_ref().map(|input| (title, input)),
                help,
                if view.editor.is_some() {
                    "Enter accept · Esc cancel · Ctrl+O copy"
                } else {
                    "↑↓ select · Enter set value · x remove · Tab plugins · q quit"
                },
            )
        }
        HomeMode::Provisioners(view) => {
            let shown = view.shown(paths);
            (shown.rows, shown.selected, None, shown.help, shown.keys)
        }
        HomeMode::Plugins(view) => {
            let plugins = view.plugins();
            let listed = berth_tui::plugins::rows(plugins, &view.tab.updates);
            let selected = view.tab.selected.min(listed.len() - 1);
            let server = berth_core::server::Server::configured(paths);
            let place = plugin_place(&server);
            let report = match &view.listing {
                Some(Err(error)) if view.report.is_empty() => vec![format!("✗ {error}")],
                _ => view.report.clone(),
            };
            let dir = match &view.listing {
                Some(Ok(listing)) => listing.dir.clone(),
                _ => crate::plugins::own_dir(&paths.config_dir)
                    .display()
                    .to_string(),
            };
            let help = berth_tui::plugins::help(
                listed[selected].0.map(|index| &plugins[index]),
                &report,
                &place.adding(&dir),
            );
            (
                listed.into_iter().map(|(_, line)| line).collect(),
                selected,
                view.editor
                    .as_ref()
                    .map(|input| (place.editor_title(), input)),
                help,
                if view.editor.is_some() {
                    "Enter install · Esc cancel · Ctrl+O copy"
                } else {
                    "↑↓ select · Enter add · c check · U update · A all · x remove · Tab provisioners · q quit"
                },
            )
        }
        mode => {
            let preferred = setting(paths, None, "default_host").0;
            let mut rows: Vec<Line> = home
                .hosts
                .iter()
                .map(|(name, address)| {
                    let address = if address.is_empty() {
                        "no address yet"
                    } else {
                        address
                    };
                    let mut line = vec![
                        host_marker(home.known.as_ref(), name),
                        Span::styled(format!("{name:<24}"), Style::new().fg(theme::TEXT)),
                        Span::styled(format!("{address:<28}"), muted),
                    ];
                    if *name == preferred {
                        line.push(Span::styled("default ", Style::new().fg(theme::STEEL)));
                    }
                    let served = home
                        .known
                        .as_ref()
                        .and_then(|known| known.hosts.get(name))
                        .is_some_and(|reach| reach.rotating);
                    if served || home.rotating.contains(name) {
                        line.push(Span::styled(
                            "key rotation pending",
                            Style::new().fg(theme::WARN),
                        ));
                    }
                    Line::from(line)
                })
                .collect();
            rows.push(Line::styled("+ New host", Style::new().fg(theme::ACCENT)));
            let help = host_help(home);
            let naming = match mode {
                HomeMode::Naming(input) => Some((" New host name ", input)),
                _ => None,
            };
            (
                rows,
                home.selected,
                naming,
                help,
                if naming.is_some() {
                    "Enter create · Esc cancel · Ctrl+O copy"
                } else {
                    "↑↓ select · Enter open · n new host · d default · r refresh · 2 provisioners · 3 settings · 4 registries · 5 secrets · 6 plugins · q quit"
                },
            )
        }
    };
    let [body, edit] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(if editor.is_some() { 3 } else { 0 }),
    ])
    .areas(left);
    for index in 0..(rows.len() as u16).min(body.height) {
        hits.push((
            Rect::new(body.x, body.y + index, body.width, 1),
            HomeTarget::Row(index as usize),
        ));
    }
    frame.render_stateful_widget(
        List::new(rows)
            .highlight_symbol(theme::cursor())
            .highlight_style(Style::new().bg(theme::RAISED)),
        body,
        &mut ListState::default().with_selected(Some(selected)),
    );
    if settings {
        frame.render_widget(
            Paragraph::new(crate::markdown::render(&help))
                .block(theme::panel("Information", false))
                .wrap(Wrap { trim: false }),
            right,
        );
    }
    if let Some((title, input)) = editor {
        // The panel has a border and one column of padding on each side.
        let width = edit.width.saturating_sub(4).max(1) as usize;
        let scroll = input.visual_scroll(width);
        frame.render_widget(
            Paragraph::new(match masked {
                true => "•".repeat(input.value().chars().count()),
                false => input.value().to_owned(),
            })
            .scroll((0, scroll.min(u16::MAX as usize) as u16))
            .block(theme::panel(title.trim(), true)),
            edit,
        );
        frame.set_cursor_position((
            edit.x + 2 + (input.visual_cursor() - scroll).min(width) as u16,
            edit.y + 1,
        ));
    }
    let status = match home.status.is_empty() && home.asking {
        true => ASKING,
        false => &home.status,
    };
    draw_footer(frame, footer, keys, status, theme::WARN);
    if let HomeMode::Provisioners(view) = &home.mode {
        view.draw_over(frame);
    }
    if let Some(qr) = &home.qr {
        qr.draw(frame);
    }
}

/// Run a host's screen. Returns true when the chosen action has to run in the
/// normal terminal, and false when the user left the host.
pub fn run(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    mirroring: &mut Option<crate::mirror::Mirroring>,
) -> Result<bool> {
    let mut display = ListState::default();
    let mut hits: Hits = vec![];
    let mut quit_armed = false;
    let mut fade = Fade::default();
    loop {
        host_mirror(app, mirroring);
        // "x again to quit" and the like stay until the next key.
        let fading = match quit_armed {
            true => None,
            false => fade.check(&mut app.status),
        };
        app.update_suggestions();
        app.watch_host();
        app.watch_machine();
        // One of berth's own actions on the host runs in the normal terminal.
        if app.own_command.is_some() {
            return Ok(true);
        }
        let (panel_rows, panel_cols) = session_size();
        app.host.resize(panel_rows, panel_cols);
        let rows = app.rows();
        terminal.draw(|frame| draw(frame, app, &rows, &mut display, &mut hits))?;
        if let Some(job) = &mut app.job {
            let running = job.running();
            job.tick();
            if running && !event::poll(std::time::Duration::from_millis(100))? {
                continue;
            }
        }
        if let Some(session) = &mut app.session {
            session.tick();
            let (rows, cols) = session_size();
            session.resize(rows, cols);
            // A shell that was left needs no key to close its panel.
            if session.closes && session.outcome == Some(true) {
                app.session = None;
                continue;
            }
            if !event::poll(std::time::Duration::from_millis(40))? {
                continue;
            }
            let event = hint_click(event::read()?);
            session_event(app, &event);
            continue;
        }
        if let Some(view) = &app.host.view {
            // A shell that was left needs no key to close its panel.
            if view.shell && view.outcome() == Some(true) {
                app.host.close(true);
                continue;
            }
            if !event::poll(std::time::Duration::from_millis(40))? {
                continue;
            }
            let event = hint_click(event::read()?);
            view_event(app, &event);
            continue;
        }
        // While the overview talks to the host, what it says is taken in
        // without waiting for a key.
        if app.step == Step::Manage
            && app.host.connected()
            && !event::poll(std::time::Duration::from_millis(200))?
        {
            continue;
        }
        // The machine's state is taken in when it comes, too.
        if app.machine_awaited() && !event::poll(std::time::Duration::from_millis(200))? {
            continue;
        }
        if let Some(left) = fading
            && !event::poll(left)?
        {
            continue;
        }
        let event = hint_click(event::read()?);
        // The overlay can outlast the panel it came from.
        if app.qr.is_some() {
            qr_event(app, &event);
            continue;
        }
        if app.unlock.is_some() {
            unlock_event(app, &event, &mut Desktop);
            continue;
        }
        if app.choosing_drive {
            if let Event::Key(key) = event {
                let chosen = match key.code {
                    KeyCode::Char(digit @ '1'..='9') => {
                        app.usb_drives.get(digit as usize - '1' as usize).cloned()
                    }
                    KeyCode::Esc => {
                        app.choosing_drive = false;
                        None
                    }
                    _ => None,
                };
                if let Some((_, device)) = chosen {
                    app.write_built_iso(Some(device));
                }
            }
            continue;
        }
        if app.job.is_some() {
            job_event(app, &event);
            continue;
        }
        if app.confirm.is_some() {
            confirm_event(app, &event);
            continue;
        }
        if app.machine.question.is_some() {
            if let Event::Key(key) = &event {
                match key.code {
                    KeyCode::Char('y' | 'Y') => app.machine_answer(true),
                    KeyCode::Char('n' | 'N') | KeyCode::Esc => app.machine_answer(false),
                    _ => {}
                }
            }
            continue;
        }
        if app.host.question.is_some() {
            if let Flow::Run = question_event(app, &event) {
                return Ok(true);
            }
            continue;
        }
        if app.host.arguments.is_some() {
            arguments_event(app, &event, &mut Desktop);
            continue;
        }
        if app.picker.is_some() {
            picker_event(app, &event);
            continue;
        }
        let key = match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => key,
            Event::Mouse(mouse) => match mouse_event(app, mouse, &hits) {
                Some(key) => key,
                None => continue,
            },
            Event::Paste(text) => {
                if let Some(input) = &mut app.editing {
                    for ch in text.chars().filter(|ch| !ch.is_control()) {
                        input.handle(InputRequest::InsertChar(ch));
                    }
                }
                continue;
            }
            _ => continue,
        };
        match key_event(app, key, &mut quit_armed, &mut Desktop) {
            Flow::Continue => {}
            Flow::Quit => return Ok(false),
            Flow::Run => return Ok(true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::parse_config;
    use berth_core::profiles::Place;
    use ratatui::{Terminal, backend::TestBackend};

    /// Paths of a folder that is never made, for an app that saves nothing.
    fn nowhere() -> Paths {
        Paths {
            config_dir: "/nonexistent/berth-test/config".into(),
            data_dir: "/nonexistent/berth-test/data".into(),
        }
    }

    /// An app of a new profile that is never saved.
    fn unsaved_app(config: &[u8]) -> App {
        App::new(
            nowhere(),
            Place::File("/nonexistent/berth-test/studio.json".into()),
            parse_config(config).unwrap(),
        )
    }

    /// An app of a profile in the file `name` of `directory`, which also
    /// holds the installation.
    fn file_app(directory: &std::path::Path, name: &str, config: &[u8]) -> App {
        App::new(
            Paths {
                config_dir: directory.join("config"),
                data_dir: directory.join("data"),
            },
            Place::File(directory.join(name)),
            parse_config(config).unwrap(),
        )
    }

    #[test]
    fn applying_from_the_ui_holds_the_hosts_lock() {
        use berth_core::runner::{Subject, lock};
        use std::process::Command;
        let dir = tempfile::tempdir().unwrap();
        let mut app = file_app(dir.path(), "studio.json", b"{}");
        app.action = Action::Ssh;
        let subject = Subject::Host(app.place.argument());
        let held = lock(&app.paths, &subject, "rollout").unwrap();
        let refused = app
            .run_command(Command::new("true"), vec![])
            .unwrap_err()
            .to_string();
        assert!(refused.contains("(rollout, process "), "{refused}");
        drop(held);
        app.run_command(Command::new("true"), vec![]).unwrap();
        let failed = app.run_command(Command::new("false"), vec![]).unwrap_err();
        assert!(failed.to_string().contains("exit status: 1"), "{failed}");
        // Once it has ended, the lock is free for the next one.
        lock(&app.paths, &subject, "apply").unwrap();
    }

    /// A clipboard of the test's own, so none of the desktop's is touched.
    #[derive(Default)]
    struct Clipboard(Option<String>);

    impl Platform for Clipboard {
        fn copy(&mut self, text: &str) -> bool {
            self.0 = Some(text.to_owned());
            true
        }

        fn paste(&mut self) -> Option<String> {
            self.0.clone()
        }

        fn open(&mut self, _: &str) -> std::result::Result<(), String> {
            Err("no browser in tests".into())
        }
    }

    fn screen(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        app.update_suggestions();
        let rows = app.rows();
        let (mut display, mut hits) = (ListState::default(), vec![]);
        terminal
            .draw(|frame| draw(frame, app, &rows, &mut display, &mut hits))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..24)
            .map(|y| {
                (0..100)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn space_on_a_group_heading_switches_the_group() {
        let mut app = unsaved_app(b"{}");
        // The coding agents are plugins that come with berth.
        app.set_plugins(crate::plugins::load(&["../../plugins".into()]));
        let chosen = |app: &App| app.config["ansible_vars"]["devbox_plugin_features"].to_string();
        app.set_step(Step::Features);
        // The heading itself is selected first and offers the key.
        assert_eq!(app.selected_group(), Some(0));
        let shown = screen(&mut app);
        println!("{shown}");
        assert!(shown.contains("Coding Agents  3 of 3"));
        assert!(shown.contains("[All off Space]"));
        press(&mut app, &[KeyCode::Char(' ')]);
        assert!(!chosen(&app).contains("claude/cli") && !chosen(&app).contains("pi/cli"));
        assert!(chosen(&app).contains("t3/server"));
        assert!(screen(&mut app).contains("[All on Space]"));
        // Enter leaves a heading alone; on an entry Space and Enter both switch it.
        press(&mut app, &[KeyCode::Enter]);
        assert!(!chosen(&app).contains("claude/cli"));
        press(&mut app, &[KeyCode::Down, KeyCode::Char(' ')]);
        assert!(chosen(&app).contains("claude/cli") && !chosen(&app).contains("codex/cli"));
    }

    #[test]
    fn secrets_are_stored_on_the_home_screen_and_given_on_a_host_page() {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: directory.path().join("config"),
            data_dir: directory.path().join("data"),
        };
        let mut home = Home {
            hosts: vec![],
            selected: 0,
            mode: HomeMode::Hosts,
            status: String::new(),
            started: std::time::Instant::now(),
            qr: None,
            berth_key: None,
            served: None,
            known: None,
            asking: false,
            refresh: false,
            rotating: vec![],
        };
        let keys = |home: &mut Home, codes: &[KeyCode]| {
            for &code in codes {
                home_key(
                    home,
                    &paths,
                    KeyEvent::new(code, KeyModifiers::NONE),
                    &mut Clipboard::default(),
                );
            }
        };
        let typed = |text: &str| text.chars().map(KeyCode::Char).collect::<Vec<_>>();
        // The Secrets tab: a name, then the value, which the screen does not show.
        keys(&mut home, &[KeyCode::Char('5'), KeyCode::Enter]);
        keys(&mut home, &typed("github"));
        keys(&mut home, &[KeyCode::Enter]);
        keys(&mut home, &typed("ghp_value"));
        let mut terminal = Terminal::new(TestBackend::new(110, 24)).unwrap();
        terminal
            .draw(|frame| draw_home(frame, &paths, &home, &mut vec![]))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let screen: String = (0..24)
            .flat_map(|y| (0..110).map(move |x| (x, y)))
            .map(|position| buffer[position].symbol())
            .collect();
        assert!(screen.contains("•••••••••") && !screen.contains("ghp_value"));
        keys(&mut home, &[KeyCode::Enter]);
        let store = Store::new(&paths).unwrap();
        assert_eq!(
            store.secret("github").unwrap().as_deref(),
            Some("ghp_value")
        );
        store.set_secret("registry/nexus", Some("pw")).unwrap();
        assert_eq!(store.secret_names(), ["github"]);

        // On a host: its Settings tab lists it, and takes how the host gets it.
        let mut app = unsaved_app(b"{}");
        app.access = Ok(store.clone());
        app.set_step(Step::Settings);
        let secrets: Vec<Entry> = app
            .entries()
            .into_iter()
            .filter(|entry| matches!(entry, Entry::HostSecret(_)))
            .collect();
        assert_eq!(secrets, [Entry::HostSecret(0)]);
        let at = app
            .entries()
            .iter()
            .position(|entry| *entry == Entry::HostSecret(0));
        app.list.select(at);
        press(&mut app, &[KeyCode::Enter]);
        for code in typed("GH_TOKEN") {
            press(&mut app, &[code]);
        }
        press(&mut app, &[KeyCode::Enter]);
        assert_eq!(
            app.config["secrets"],
            json!([{"name": "github", "env": "GH_TOKEN"}])
        );
        assert!(screen_text(&mut app).contains("github  ● variable GH_TOKEN"));
        app.set_host_secret(0, "~/.config/gh/token");
        assert_eq!(
            app.config["secrets"],
            json!([{"name": "github", "file": "~/.config/gh/token"}])
        );
        app.set_host_secret(0, "not a name or path");
        assert_eq!(app.config["secrets"][0]["file"], "~/.config/gh/token");
        press(&mut app, &[KeyCode::Char('x')]);
        assert_eq!(app.config["secrets"], json!([]));

        // Removing the secret takes x twice.
        keys(&mut home, &[KeyCode::Char('x')]);
        assert!(store.secret("github").unwrap().is_some());
        keys(&mut home, &[KeyCode::Char('x')]);
        assert!(store.secret("github").unwrap().is_none());
    }

    #[test]
    fn plugins_tab_lists_and_removes_a_copied_plugin() {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: directory.path().join("config"),
            data_dir: directory.path().join("data"),
        };
        let own = paths.config_dir.join("plugins/aa-hello");
        std::fs::create_dir_all(&own).unwrap();
        std::fs::write(
            own.join("package.json"),
            r#"{"name":"berth-plugin-aa-hello","version":"1.2.3","displayName":"Hello",
                "berth":{"features":[{"id":"cow","label":"cowsay","group":"Development Tools"}]}}"#,
        )
        .unwrap();
        let mut home = Home {
            hosts: vec![],
            selected: 0,
            mode: HomeMode::Hosts,
            status: String::new(),
            started: std::time::Instant::now(),
            qr: None,
            berth_key: None,
            served: None,
            known: None,
            asking: false,
            refresh: false,
            rotating: vec![],
        };
        let key = |home: &mut Home, code| {
            home_key(
                home,
                &paths,
                KeyEvent::new(code, KeyModifiers::NONE),
                &mut Clipboard::default(),
            );
        };
        key(&mut home, KeyCode::Char('6'));
        if let HomeMode::Plugins(view) = &mut home.mode {
            view.load(&paths);
        }
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|frame| draw_home(frame, &paths, &home, &mut vec![]))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text: String = (0..40)
            .flat_map(|y| (0..120).map(move |x| (x, y)))
            .map(|position| buffer[position].symbol())
            .collect();
        // The plugin of the source this program carries, and the copied one.
        assert!(text.contains("Unity") && text.contains("comes with berth"));
        assert!(text.contains("Hello  berth-plugin-aa-hello 1.2.3  · yours"));
        assert!(text.contains("cowsay  in Development Tools") && text.contains("+ Add plugin"));
        // Hello sorts first. Shipped plugins stay; a copied one goes after x twice.
        key(&mut home, KeyCode::Char('x'));
        assert!(own.exists());
        key(&mut home, KeyCode::Char('x'));
        assert!(!own.exists());
        key(&mut home, KeyCode::Char('x'));
        assert!(home.status.contains("comes with berth"));
    }

    #[test]
    fn the_provisioners_tab_opens_a_page_and_runs_its_actions_here() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: directory.path().join("config"),
            data_dir: directory.path().join("data"),
        };
        paths.prepare().unwrap();
        // The checkout is the provisioning source, so none is unpacked behind the test's back.
        let checkout = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        paths
            .set("source", checkout.canonicalize().unwrap().to_str().unwrap())
            .unwrap();
        let own = paths.config_dir.join("plugins/aa-lab");
        std::fs::create_dir_all(own.join("bin")).unwrap();
        std::fs::write(
            own.join("bin/provision"),
            "#!/bin/sh\ncat > /dev/null\n[ -f \"say.$1\" ] && cat \"say.$1\" >&2\n[ -f \"answer.$1\" ] && cat \"answer.$1\"\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(
            own.join("bin/provision"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::fs::write(
            own.join("package.json"),
            r#"{"name": "berth-plugin-aa-lab", "berth": {"provisioners": [
                {"id": "box", "label": "Lab box", "command": "bin/provision", "protocol": 2,
                 "kinds": [{"id": "machine", "setup": "apply"}],
                 "actions": [{"name": "look", "label": "Look around", "changes": false},
                             {"name": "sweep", "label": "Sweep", "confirm": "Sweep the lab?"}]}]}}"#,
        )
        .unwrap();
        std::fs::write(
            own.join("answer.status"),
            r#"{"message": "The lab answers", "details": [{"label": "Benches", "value": "3"}]}"#,
        )
        .unwrap();
        std::fs::write(own.join("answer.act"), r#"{"message": "All clean"}"#).unwrap();
        std::fs::write(own.join("say.act"), "sweeping\n").unwrap();
        let mut home = Home {
            hosts: vec![],
            selected: 0,
            mode: HomeMode::Hosts,
            status: String::new(),
            started: std::time::Instant::now(),
            qr: None,
            berth_key: None,
            served: None,
            known: None,
            asking: false,
            refresh: false,
            rotating: vec![],
        };
        let key = |home: &mut Home, code| {
            home_key(
                home,
                &paths,
                KeyEvent::new(code, KeyModifiers::NONE),
                &mut Clipboard::default(),
            );
        };
        // What runs in a thread is taken in as the screen would, until `done` says so.
        let settle = |home: &mut Home, done: &dyn Fn(&str) -> bool| -> String {
            for _ in 0..500 {
                if let HomeMode::Provisioners(view) = &mut home.mode {
                    view.load(&paths);
                    view.pump(&paths, &mut home.status);
                }
                let screen = home_screen(&paths, home).join("\n");
                if done(&screen) {
                    return screen;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            panic!(
                "the screen did not settle:\n{}",
                home_screen(&paths, home).join("\n")
            );
        };
        key(&mut home, KeyCode::Char('2'));
        let screen = settle(&mut home, &|screen| screen.contains("Lab box"));
        assert!(screen.contains("Provisioners") && screen.contains("aa-lab/box"));
        // The lab sorts first; its page asks it how it is.
        key(&mut home, KeyCode::Enter);
        let screen = settle(&mut home, &|screen| screen.contains("The lab answers"));
        assert!(
            screen.contains("● ready") && screen.contains("Benches  3"),
            "{screen}"
        );
        assert!(screen.contains("▸ Look around") && screen.contains("▸ Sweep"));
        // One that only reads answers on the page.
        key(&mut home, KeyCode::Enter);
        let screen = settle(&mut home, &|screen| screen.contains("All clean"));
        assert!(!screen.contains("sweeping"), "{screen}");
        // One that changes something asks first, and is a job in a panel.
        key(&mut home, KeyCode::Down);
        key(&mut home, KeyCode::Enter);
        assert!(
            home_screen(&paths, &home)
                .join("\n")
                .contains("Sweep the lab?")
        );
        key(&mut home, KeyCode::Char('y'));
        let screen = settle(&mut home, &|screen| screen.contains("✓ Done"));
        assert!(
            screen.contains("sweeping") && screen.contains("✓ All clean"),
            "{screen}"
        );
        // Keys go to the panel until it is closed, and then back to the page.
        key(&mut home, KeyCode::Char('1'));
        assert!(matches!(home.mode, HomeMode::Provisioners(_)));
        key(&mut home, KeyCode::Enter);
        key(&mut home, KeyCode::Esc);
        key(&mut home, KeyCode::Esc);
        assert!(matches!(home.mode, HomeMode::Hosts));
    }

    #[test]
    fn hints_can_be_clicked() {
        let keys =
            "↑↓ select · Enter open · Ctrl+V paste · x remove · F3 browse · Click opens a link";
        note_hints(keys, 4, 9, true);
        let click = |column| {
            hint_click(Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row: 9,
                modifiers: KeyModifiers::NONE,
            }))
        };
        let key = |code, modifiers| Event::Key(KeyEvent::new(code, modifiers));
        // The arrows are not one key, so a click there stays a click.
        assert!(matches!(click(5), Event::Mouse(_)));
        assert_eq!(click(15), key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(click(27), key(KeyCode::Char('v'), KeyModifiers::CONTROL));
        assert_eq!(click(45), key(KeyCode::Char('x'), KeyModifiers::NONE));
        // Between two hints.
        assert!(matches!(click(49), Event::Mouse(_)));
        assert_eq!(click(51), key(KeyCode::F(3), KeyModifiers::NONE));
        assert!(matches!(click(70), Event::Mouse(_)));
        assert_eq!(pasted("AB12-CD34"), "Pasted 9 characters");
    }

    #[test]
    fn a_session_draws_its_terminal_and_finds_a_wrapped_link() {
        let address = format!("https://example.com/login?code={}", "a".repeat(40));
        let mut command = std::process::Command::new("printf");
        command
            .arg("Open %s now\\r\\ncode: AB12-CD34\\r\\n")
            .arg(&address);
        let mut session = crate::session::Session::start("demo", command, 6, 30).unwrap();
        for _ in 0..200 {
            session.tick();
            if !session.running() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // The last output can arrive a moment after the process has ended.
        for _ in 0..5 {
            std::thread::sleep(std::time::Duration::from_millis(10));
            session.tick();
        }
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        let (mut links, mut words) = (vec![], vec![]);
        terminal
            .draw(|frame| (links, words) = draw_session(frame, &session, ""))
            .unwrap();
        // Every cell of the address opens the whole of it, over three rows.
        assert_eq!(links.len(), address.len());
        assert!(links.iter().all(|(_, link)| *link == address));
        assert_eq!(links.last().unwrap().0.y - links[0].0.y, 2);
        assert!(words.iter().any(|(_, word)| word == "AB12-CD34"));
    }

    fn screen_text(app: &mut App) -> String {
        screen(app)
    }

    #[test]
    fn registries_are_edited_on_their_own_tab() {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: directory.path().join("config"),
            data_dir: directory.path().join("data"),
        };
        let mut home = Home {
            hosts: vec![],
            selected: 0,
            mode: HomeMode::Hosts,
            status: String::new(),
            started: std::time::Instant::now(),
            qr: None,
            berth_key: None,
            served: None,
            known: None,
            asking: false,
            refresh: false,
            rotating: vec![],
        };
        let keys = |home: &mut Home, codes: &[KeyCode]| {
            for &code in codes {
                home_key(
                    home,
                    &paths,
                    KeyEvent::new(code, KeyModifiers::NONE),
                    &mut Clipboard::default(),
                );
            }
        };
        let typed = |text: &str| text.chars().map(KeyCode::Char).collect::<Vec<_>>();
        use KeyCode::{Down, Enter, Up};
        // Hosts, then the Registries tab: only "+ Add registry" is there.
        keys(&mut home, &[KeyCode::Char('4'), Enter]);
        keys(&mut home, &typed("Bad Name"));
        keys(&mut home, &[Enter]);
        assert!(home.status.starts_with("Use lower-case"));
        for _ in 0..8 {
            keys(&mut home, &[KeyCode::Backspace]);
        }
        keys(&mut home, &typed("nexus"));
        keys(&mut home, &[Enter, Down, Down, Enter]);
        keys(&mut home, &typed("https://nexus.example/"));
        // User name, then the password, which the screen does not show.
        keys(&mut home, &[Enter, Down, Enter]);
        keys(&mut home, &typed("ci"));
        keys(&mut home, &[Enter, Down, Enter]);
        keys(&mut home, &typed("hunter2"));
        let mut terminal = Terminal::new(TestBackend::new(110, 30)).unwrap();
        let mut shown = |home: &Home| {
            terminal
                .draw(|frame| draw_home(frame, &paths, home, &mut vec![]))
                .unwrap();
            let buffer = terminal.backend().buffer();
            (0..30)
                .map(|y| {
                    (0..110)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let screen = shown(&home);
        assert!(screen.contains("•••••••") && !screen.contains("hunter2"));
        keys(&mut home, &[Enter]);
        // A repository: added as npm, switched to raw and on to docker, which asks for an address.
        keys(&mut home, &[Down, Enter]);
        keys(&mut home, &typed("images"));
        keys(&mut home, &[Enter, Down, Enter, Enter, Down, Enter]);
        keys(&mut home, &typed("nexus.example:8083"));
        keys(&mut home, &[Enter]);
        let store = Store::new(&paths).unwrap();
        assert_eq!(
            store.registries().unwrap()["nexus"],
            json!({"type": "nexus", "url": "https://nexus.example", "username": "ci", "repositories": [
                {"format": "docker", "name": "images", "address": "nexus.example:8083"}]})
        );
        assert_eq!(
            store.secret("registry/nexus").unwrap().as_deref(),
            Some("hunter2")
        );
        let screen = shown(&home);
        println!("{screen}");
        assert!(screen.contains("Registries") && screen.contains("● stored"));
        assert!(screen.contains("Docker address  nexus.example:8083"));

        // A second registry of another kind: a plain npm registry with a token.
        keys(&mut home, &[Down, Down, Enter]);
        keys(&mut home, &typed("verdaccio"));
        keys(&mut home, &[Enter, Down, Enter]);
        assert_eq!(store.registries().unwrap()["verdaccio"]["type"], "npm");
        // Removing a registry takes x twice and forgets its login.
        for _ in 0..12 {
            keys(&mut home, &[Up]);
        }
        keys(&mut home, &[KeyCode::Char('x')]);
        assert!(store.registries().unwrap().contains_key("nexus"));
        keys(&mut home, &[KeyCode::Char('x')]);
        assert!(!store.registries().unwrap().contains_key("nexus"));
        assert_eq!(store.secret("registry/nexus").unwrap(), None);
    }

    fn press(app: &mut App, codes: &[KeyCode]) -> bool {
        let mut armed = false;
        for &code in codes {
            app.update_suggestions();
            let key = KeyEvent::new(code, KeyModifiers::NONE);
            if matches!(
                key_event(app, key, &mut armed, &mut Clipboard::default()),
                Flow::Run
            ) {
                return true;
            }
        }
        false
    }

    #[test]
    fn settings_offer_the_link_to_the_servers_page_as_a_qr_code() {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: directory.path().join("config"),
            data_dir: directory.path().join("data"),
        };
        let mut home = Home {
            hosts: vec![],
            selected: 0,
            mode: HomeMode::Settings(0, None),
            status: String::new(),
            started: std::time::Instant::now(),
            qr: None,
            berth_key: None,
            served: None,
            known: None,
            asking: false,
            refresh: false,
            rotating: vec![],
        };
        let last = berth_core::paths::SETTINGS.len();
        let down = |home: &mut Home| {
            for _ in 0..last + 2 {
                home_key(
                    home,
                    &paths,
                    KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
                    &mut Clipboard::default(),
                );
            }
        };
        // Without a server, or with one reached through a socket, there is no such row.
        down(&mut home);
        assert!(matches!(home.mode, HomeMode::Settings(index, _) if index == last - 1));
        paths.set("server", "https://berth.example.ts.net").unwrap();
        paths.set("server_token", "a1b2").unwrap();
        down(&mut home);
        assert!(matches!(home.mode, HomeMode::Settings(index, _) if index == last));
        let mut terminal = Terminal::new(TestBackend::new(110, 50)).unwrap();
        let mut shown = |home: &Home| {
            terminal
                .draw(|frame| draw_home(frame, &paths, home, &mut vec![]))
                .unwrap();
            let buffer = terminal.backend().buffer();
            (0..50)
                .map(|y| {
                    (0..110)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let screen = shown(&home);
        assert!(screen.contains("Open on another device") && screen.contains("server's token"));
        home_key(
            &mut home,
            &paths,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut Clipboard::default(),
        );
        let overlay = home.qr.clone().unwrap();
        assert_eq!(overlay.link(), "https://berth.example.ts.net/#token=a1b2");
        let screen = shown(&home);
        assert!(
            screen.contains('▀') && screen.contains("#token=a1b2"),
            "{screen}"
        );
    }

    #[test]
    fn alt_r_shows_the_links_of_a_panel_as_qr_codes() {
        let mut app = unsaved_app(b"{}");
        app.links = vec![(
            Rect::new(0, 0, 1, 1),
            "https://studio.example.ts.net/vnc.html".into(),
        )];
        let alt_r = Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT));
        assert!(qr_event(&mut app, &alt_r));
        assert_eq!(
            app.qr.as_ref().map(|qr| qr.link()),
            Some("https://studio.example.ts.net/vnc.html")
        );
        assert!(qr_event(
            &mut app,
            &Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        ));
        assert!(app.qr.is_none());
        // A plain r is the panel's.
        let r = Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        assert!(!qr_event(&mut app, &r));
    }

    #[test]
    fn settings_are_edited_from_the_home_screen() {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: directory.path().join("config"),
            data_dir: directory.path().join("data"),
        };
        let mut home = Home {
            hosts: vec![("laptop".into(), "100.64.0.1".into())],
            selected: 0,
            mode: HomeMode::Hosts,
            status: String::new(),
            started: std::time::Instant::now(),
            qr: None,
            berth_key: None,
            served: None,
            known: None,
            asking: false,
            refresh: false,
            rotating: vec![],
        };
        let press = |home: &mut Home, code| {
            home_key(
                home,
                &paths,
                KeyEvent::new(code, KeyModifiers::NONE),
                &mut Clipboard::default(),
            )
        };
        assert!(press(&mut home, KeyCode::Char('d')).is_none());
        assert_eq!(paths.settings().unwrap()["default_host"], "laptop");
        for code in [
            KeyCode::Char('3'),
            KeyCode::Down,
            KeyCode::Down,
            KeyCode::Enter,
        ] {
            press(&mut home, code);
        }
        for ch in "/srv/iso".chars() {
            press(&mut home, KeyCode::Char(ch));
        }
        press(&mut home, KeyCode::Enter);
        assert_eq!(paths.settings().unwrap()["iso_dir"], "/srv/iso");
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal
            .draw(|frame| draw_home(frame, &paths, &home, &mut vec![]))
            .unwrap();
        let text = format!("{:?}", terminal.backend().buffer());
        assert!(text.contains("█ ▄▄ █") && text.contains("▂") && text.contains("/srv/iso"));
        // Clicking the Hosts tab and then the host opens it.
        let mut hits = vec![];
        let mut click = |home: &mut Home, column, row| {
            terminal
                .draw(|frame| draw_home(frame, &paths, home, &mut hits))
                .unwrap();
            let mouse = MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                modifiers: KeyModifiers::NONE,
            };
            home_mouse(home, &paths, mouse, &hits, &mut Clipboard::default())
        };
        assert!(click(&mut home, 15, 1).is_none());
        assert!(matches!(home.mode, HomeMode::Hosts));
        assert_eq!(click(&mut home, 10, 7), Some(Some("laptop".into())));
    }

    /// A home screen on the Settings tab of fresh scratch directories, and the
    /// text of the screen after drawing it.
    fn settings_home(directory: &std::path::Path) -> (Paths, Home) {
        let paths = Paths {
            config_dir: directory.join("config"),
            data_dir: directory.join("data"),
        };
        let home = Home {
            hosts: vec![],
            selected: 0,
            mode: HomeMode::Settings(0, None),
            status: String::new(),
            started: std::time::Instant::now(),
            qr: None,
            berth_key: None,
            served: None,
            known: None,
            asking: false,
            refresh: false,
            rotating: vec![],
        };
        (paths, home)
    }

    fn home_screen(paths: &Paths, home: &Home) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(140, 30)).unwrap();
        terminal
            .draw(|frame| draw_home(frame, paths, home, &mut vec![]))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..30)
            .map(|y| (0..140).map(|x| buffer[(x, y)].symbol()).collect())
            .collect()
    }

    #[test]
    fn the_text_of_a_field_is_copied_with_its_hint_or_its_key() {
        let directory = tempfile::tempdir().unwrap();
        let (paths, mut home) = settings_home(directory.path());
        let mut clipboard = Clipboard::default();
        home.mode = HomeMode::Settings(0, Some(Input::new("/srv/provision".into())));
        let rows = home_screen(&paths, &home);
        let last = rows
            .iter()
            .position(|row| row.contains("Ctrl+O copy"))
            .expect("the hint is shown");
        let column = rows[last].find("Ctrl+O copy").unwrap();
        assert!(rows[last].contains("Enter accept") && rows[last].contains("Esc cancel"));
        // The rows hold only one-cell characters up to the hint, so a byte is a column.
        let click = |column: usize| {
            hint_click(Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: column as u16,
                row: last as u16,
                modifiers: KeyModifiers::NONE,
            }))
        };
        // Enter and Esc are hints like it.
        assert_eq!(
            click(rows[last].find("Esc cancel").unwrap()),
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        );
        let Event::Key(copy) = click(column + 2) else {
            panic!("a click on the hint is a key");
        };
        assert_eq!(
            copy,
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)
        );
        assert!(home_key(&mut home, &paths, copy, &mut clipboard).is_none());
        assert_eq!(clipboard.0.as_deref(), Some("/srv/provision"));
        assert_eq!(home.status, "Copied the text of the field");
        // The field is still open and unchanged, and the status does not say the text.
        assert!(
            matches!(&home.mode, HomeMode::Settings(0, Some(input)) if input.value() == "/srv/provision")
        );
    }

    #[test]
    fn a_secret_setting_is_shortened_in_the_list_and_whole_in_its_field() {
        let directory = tempfile::tempdir().unwrap();
        let (paths, mut home) = settings_home(directory.path());
        let token = "tok-0123456789abcdefghijklmnopqrstuvwxyz";
        paths.set("server_token", token).unwrap();
        paths.set("default_host", "studio").unwrap();
        let rows = home_screen(&paths, &home).join("\n");
        assert!(!rows.contains(token) && !rows.contains("0123456789abcdef"));
        assert!(rows.contains("server_token  …wxyz"));
        // What is not a secret stays whole.
        assert!(rows.contains("Default host  studio"));
        // The description reads right, with no "The" in front.
        let at = crate::paths::SETTINGS
            .iter()
            .position(|(name, _)| *name == "server_token")
            .unwrap();
        home.mode = HomeMode::Settings(at, None);
        home_key(
            &mut home,
            &paths,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut Clipboard::default(),
        );
        let edited = home_screen(&paths, &home).join("\n");
        assert!(edited.contains(token));
        // The description reads right, with no "The" in front of "the token".
        assert!(!edited.contains("The the") && edited.contains("The token of that berth"));
        assert!(crate::paths::is_secret("oauth_client_secret"));
        assert!(!crate::paths::is_secret("serve_token"));
    }

    #[test]
    fn the_home_screen_draws_while_the_mirror_has_not_returned() {
        let directory = tempfile::tempdir().unwrap();
        let (paths, mut home) = settings_home(directory.path());
        home.mode = HomeMode::Hosts;
        // A mirror that waits for the test to let it end.
        let (release, wait) = std::sync::mpsc::channel::<crate::mirror::Made>();
        let wait = std::sync::Mutex::new(wait);
        let mut mirroring = Some(crate::mirror::Mirroring::new(move || {
            wait.lock().unwrap().recv().unwrap()
        }));
        mirroring.as_mut().unwrap().start();
        home_mirror(&mut home, &paths, &mut mirroring);
        assert!(home.asking);
        assert!(home_screen(&paths, &home).join("\n").contains(ASKING));
        // Starting another while one runs starts nothing.
        assert!(!mirroring.as_mut().unwrap().start());
        // A host the mirror brings is listed once it is there, and not before.
        Place::own("studio")
            .unwrap()
            .save(&paths, &json!({"access": {"address": "192.0.2.10"}}))
            .unwrap();
        assert!(home.hosts.is_empty());
        release.send(Ok(())).unwrap();
        while mirroring.as_ref().unwrap().running() {
            home_mirror(&mut home, &paths, &mut mirroring);
            std::thread::yield_now();
        }
        assert!(!home.asking);
        assert_eq!(home.hosts, [("studio".to_owned(), "192.0.2.10".to_owned())]);
        assert!(!home_screen(&paths, &home).join("\n").contains(ASKING));
        // A mirror that fails says why and keeps what was there.
        mirroring.as_mut().unwrap().start();
        release
            .send(Err(
                "the berth server at http://192.0.2.7:7777: timed out".into()
            ))
            .unwrap();
        while mirroring.as_ref().unwrap().running() {
            home_mirror(&mut home, &paths, &mut mirroring);
            std::thread::yield_now();
        }
        assert!(home.status.contains("timed out") && home.hosts.len() == 1);
    }

    #[test]
    fn hosts_show_whether_the_server_reaches_them() {
        let directory = tempfile::tempdir().unwrap();
        let (paths, mut home) = settings_home(directory.path());
        home.mode = HomeMode::Hosts;
        paths.set("server", "http://192.0.2.7:7777").unwrap();
        let server = berth_core::server::Server::configured(&paths).unwrap();
        for (name, address) in [
            ("attic", "192.0.2.20"),
            ("studio", "192.0.2.10"),
            ("yard", "192.0.2.30"),
        ] {
            Place::stored(&server.scope(), name)
                .unwrap()
                .save(&paths, &json!({"access": {"address": address}}))
                .unwrap();
        }
        let state = |version: &str| {
            format!(
                r#"{{"version":"{version}","hosts":{{"studio":{{"reachable":true,"error":null,"version":"0.3.0"}},"attic":{{"reachable":false,"error":"ssh: connect to host 192.0.2.20: timed out","version":null}}}}}}"#
            )
        };
        // What a mirror keeps of the server's state is in the store, under its scope.
        let store = berth_core::store::shared(&paths).unwrap();
        let mirrored = |state: String| {
            store
                .write(
                    berth_core::store::kind::MIRROR,
                    &server.scope(),
                    "state",
                    state.as_bytes(),
                )
                .unwrap()
        };
        mirrored(state("0.3.0"));
        home.hosts = paths.hosts();
        home.known = known_of(&paths);
        let rows = home_screen(&paths, &home);
        let line = |name: &str| rows.iter().find(|row| row.contains(name)).unwrap().clone();
        assert!(line("studio").contains("● studio"));
        assert!(line("attic").contains("● attic"));
        assert!(line("yard").contains("○ yard"));
        // The selected host, the first, is attic: its error is in the panel.
        let text = rows.join("\n");
        assert!(text.contains("does not answer") && text.contains("timed out"));
        // The server is older than this berth, so the screen says so.
        assert!(text.contains("The berth server runs 0.3.0"));
        assert!(text.contains("Update the berth server"));
        for same in [berth_api::VERSION.to_owned(), "99.0.0".to_owned()] {
            mirrored(state(&same));
            home.known = known_of(&paths);
            let text = home_screen(&paths, &home).join("\n");
            assert!(!text.contains("The berth server runs"));
        }
        // With no server there is no marker and no panel.
        paths.set("server", "").unwrap();
        home.known = known_of(&paths);
        assert!(home.known.is_none());
        assert!(!home_screen(&paths, &home).join("\n").contains('●'));
    }

    #[test]
    fn an_unreachable_host_can_have_its_address_changed() {
        use crate::remote::State;
        let directory = tempfile::tempdir().unwrap();
        let mut app = file_app(directory.path(), "studio.json", b"{}");
        app.set_step(Step::Manage);
        app.commit(Field::SshHost, "192.0.2.20");
        let error = "ssh: connect to host 192.0.2.20 port 22: Connection timed out and a long text that goes on past the width of the list so that it has to be wrapped over lines\nsecond line of the error";
        app.host.state = State::Unreachable(error.into());
        let text = screen_text(&mut app);
        assert!(text.contains("This machine could not reach the host"));
        assert!(text.contains("second line of the error") && text.contains("wrapped over lines"));
        assert!(text.contains("Change the SSH address") && text.contains("Try again"));
        // A host of the berth server is reached by the server.
        app.config["builder"]["served"] = json!("http://192.0.2.7:7777");
        let text = screen_text(&mut app);
        assert!(text.contains("The berth server at http://192.0.2.7:7777 could not"));
        app.config["builder"]
            .as_object_mut()
            .unwrap()
            .remove("served");
        // The action opens the address for editing.
        choose(&mut app, Entry::Task("address"));
        assert!(app.retry_address && app.editing.is_some(), "{}", app.status);
        assert_eq!(app.selected_field(), Some(Field::SshHost));
        let mut armed = false;
        let mut clipboard = Clipboard::default();
        for code in [KeyCode::Char('9'), KeyCode::Enter] {
            let event = KeyEvent::new(code, KeyModifiers::NONE);
            key_event(&mut app, event, &mut armed, &mut clipboard);
        }
        // Saved, and the overview asks the host again.
        assert!(!app.retry_address && app.step == Step::Manage);
        assert!(app.text(Field::SshHost).ends_with('9'));
        assert!(directory.path().join("studio.json").is_file());
        assert!(matches!(app.host.state, State::Idle | State::Connecting));
    }

    #[test]
    fn the_hosts_tab_refreshes_with_r_and_the_hint_is_clickable() {
        let directory = tempfile::tempdir().unwrap();
        let (paths, mut home) = settings_home(directory.path());
        home.mode = HomeMode::Hosts;
        let press = |home: &mut Home, ch| {
            home_key(
                home,
                &paths,
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                &mut Clipboard::default(),
            )
        };
        // Without a server it reads the saved hosts again.
        Place::own("studio")
            .unwrap()
            .save(&paths, &json!({"access": {"address": "192.0.2.10"}}))
            .unwrap();
        let rows = home_screen(&paths, &home);
        let last = rows
            .iter()
            .position(|row| row.contains("r refresh"))
            .unwrap();
        let column = rows[last].find("r refresh").unwrap();
        let Event::Key(clicked) = hint_click(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: column as u16,
            row: last as u16,
            modifiers: KeyModifiers::NONE,
        })) else {
            panic!("a click on the hint is a key");
        };
        assert_eq!(clicked.code, KeyCode::Char('r'));
        assert!(home.hosts.is_empty());
        home_key(&mut home, &paths, clicked, &mut Clipboard::default());
        home_mirror(&mut home, &paths, &mut None);
        assert_eq!(home.hosts.len(), 1);
        // With a server it starts the mirror, and not a second while one runs.
        let (release, wait) = std::sync::mpsc::channel::<crate::mirror::Made>();
        let wait = std::sync::Mutex::new(wait);
        let mut mirroring = Some(crate::mirror::Mirroring::new(move || {
            wait.lock().unwrap().recv().unwrap()
        }));
        press(&mut home, 'r');
        home_mirror(&mut home, &paths, &mut mirroring);
        assert!(home.asking);
        press(&mut home, 'r');
        home_mirror(&mut home, &paths, &mut mirroring);
        assert!(home.status.contains("Still asking"));
        release.send(Ok(())).unwrap();
        while mirroring.as_ref().unwrap().running() {
            home_mirror(&mut home, &paths, &mut mirroring);
            std::thread::yield_now();
        }
        // Typing a name is not a refresh, and r is no longer the Registries tab.
        press(&mut home, 'n');
        press(&mut home, 'r');
        assert!(matches!(&home.mode, HomeMode::Naming(input) if input.value() == "r"));
        assert!(!home.refresh);
        home.mode = HomeMode::Hosts;
        press(&mut home, 'r');
        assert!(matches!(home.mode, HomeMode::Hosts));
        home.mode = HomeMode::Settings(0, None);
        press(&mut home, 'r');
        assert!(matches!(home.mode, HomeMode::Settings(..)));
    }

    #[test]
    fn the_copy_hint_of_a_host_edit_can_be_clicked() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = file_app(directory.path(), "studio.json", b"{}");
        app.commit(Field::SshHost, "192.0.2.20");
        app.editing = Some(Input::new("192.0.2.20".into()));
        let rows: Vec<String> = screen(&mut app).lines().map(str::to_owned).collect();
        let at = rows
            .iter()
            .position(|row| row.contains("Ctrl+O copy"))
            .unwrap();
        let column = rows[at].find("Ctrl+O copy").unwrap();
        let click = |column: usize| {
            hint_click(Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: column as u16,
                row: at as u16,
                modifiers: KeyModifiers::NONE,
            }))
        };
        let Event::Key(copy) = click(column + 1) else {
            panic!("a click on the hint is a key");
        };
        let mut clipboard = Clipboard::default();
        key_event(&mut app, copy, &mut false, &mut clipboard);
        assert_eq!(clipboard.0.as_deref(), Some("192.0.2.20"));
        assert!(app.editing.is_some());
        // Enter is a hint of the line too.
        let enter = rows[at].find("Enter accept").unwrap();
        assert!(matches!(click(enter), Event::Key(key) if key.code == KeyCode::Enter));
    }

    #[test]
    fn a_mirror_is_taken_in_unless_the_host_has_unsaved_changes() {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: directory.path().join("config"),
            data_dir: directory.path().join("data"),
        };
        let place = Place::stored("served:http://192.0.2.7:7777", "studio").unwrap();
        // What a mirror writes: the profile under the server's scope.
        let write = |address: &str| {
            place
                .save(&paths, &json!({"access": {"address": address}}))
                .unwrap()
        };
        write("192.0.2.20");
        let stored = place.load(&paths).unwrap().unwrap();
        let mut app = App::new(
            paths.clone(),
            place.clone(),
            parse_config(stored.to_string().as_bytes()).unwrap(),
        );
        assert_eq!(app.take_in_mirror(), None);
        // A new address in the mirror is taken in.
        write("192.0.2.21");
        assert!(app.take_in_mirror().unwrap().contains("Took in"));
        assert_eq!(app.text(Field::SshHost), "192.0.2.21");
        assert!(!app.unsaved());
        // With a change of its own the screen keeps it and says so.
        app.commit(Field::SshHost, "192.0.2.99");
        write("192.0.2.22");
        let said = app.take_in_mirror().unwrap();
        assert!(said.contains("Saving sends this one"));
        assert_eq!(app.text(Field::SshHost), "192.0.2.99");
    }

    #[test]
    fn what_a_command_saved_of_the_host_is_not_overwritten_by_the_screen() {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: directory.path().join("config"),
            data_dir: directory.path().join("data"),
        };
        let place = Place::own("studio").unwrap();
        place
            .save(&paths, &json!({"access": {"user": "ops"}}))
            .unwrap();
        let open = || {
            let stored = place.load(&paths).unwrap().unwrap();
            App::new(
                paths.clone(),
                place.clone(),
                parse_config(stored.to_string().as_bytes()).unwrap(),
            )
        };
        // What `berth provision` does from the screen: the machine's address
        // and the cloud profile go into the stored profile.
        let provisioned = |port: &str| {
            let mut kept = place.load(&paths).unwrap().unwrap();
            kept["access"]["address"] = json!("192.0.2.40");
            kept["access"]["port"] = json!(port);
            kept["profile"] = json!("cloud");
            place.save(&paths, &kept).unwrap();
        };
        let mut app = open();
        assert_eq!(app.take_in_command(), None);
        provisioned("2222");
        assert!(app.take_in_command().unwrap().contains("Took in"));
        assert!(!app.unsaved());
        app.save().unwrap();
        let kept = place.load(&paths).unwrap().unwrap();
        assert_eq!(kept["access"]["address"], "192.0.2.40");

        // With changes not saved yet the address joins them, and saving keeps both.
        let mut app = open();
        app.commit(Field::Keyboard, "us");
        provisioned("2200");
        let said = app.take_in_command().unwrap();
        assert!(said.contains("not saved yet"), "{said}");
        assert!(app.unsaved());
        assert_eq!(app.text(Field::SshPort), "2200");
        app.save().unwrap();
        let kept = place.load(&paths).unwrap().unwrap();
        assert_eq!(
            (&kept["access"]["port"], &kept["keyboard"], &kept["profile"]),
            (&json!("2200"), &json!("us"), &json!("cloud"))
        );
    }

    #[test]
    fn words_of_output_can_be_copied() {
        assert_eq!(
            words("App: code      K79HUTDTLL75"),
            [
                (0, "App".to_owned()),
                (5, "code".to_owned()),
                (15, "K79HUTDTLL75".to_owned())
            ]
        );
        // The address inside brackets, without them or the semicolon.
        assert_eq!(
            words("(or https://100.64.0.1:9090; log in)")[1],
            (4, "https://100.64.0.1:9090".to_owned())
        );
    }

    #[test]
    fn web_addresses_in_output_are_found() {
        let parts =
            with_links("Cockpit   https://box.ts.net:9090  (or https://100.64.0.1:9090; log in)");
        let links: Vec<_> = parts
            .iter()
            .filter(|part| part.1)
            .map(|part| &part.0)
            .collect();
        assert_eq!(
            links,
            ["https://box.ts.net:9090", "https://100.64.0.1:9090"]
        );
        let text: String = parts.iter().map(|part| part.0.as_str()).collect();
        assert!(text.ends_with("; log in)") && text.starts_with("Cockpit   "));
        assert_eq!(
            with_links("ssh ops@host"),
            [("ssh ops@host".to_owned(), false)]
        );
    }

    /// The text of the rows of the current tab.
    fn lines(app: &App) -> Vec<String> {
        app.rows().iter().map(|row| row.line.to_string()).collect()
    }

    fn select(app: &mut App, entry: Entry) {
        let at = app.entries().iter().position(|known| *known == entry);
        app.list
            .select(Some(at.unwrap_or_else(|| panic!("no {entry:?}"))));
    }

    #[test]
    fn the_set_up_tabs_go_from_install_to_a_blocked_run() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = file_app(directory.path(), "profile.json", b"{}");
        use KeyCode::*;
        // A new host opens on Install; the tabs say nothing but their names.
        assert_eq!(app.step, Step::Install);
        let install = screen(&mut app);
        let header = install.lines().nth(1).unwrap();
        let place = |text: &str| header.find(text).unwrap();
        assert!(place("Install") < place("Features") && place("Features") < place("Settings"));
        assert!(place("Settings") < place("Run") && !header.contains('✗'));
        assert!(install.contains("1-4 tab"));
        // The USB installer is the default method; what it needs chosen follows.
        press(&mut app, &[Down, Enter, Down, Down, Down, Down, Down]);
        assert_eq!(app.action, Action::Iso);
        let install = screen(&mut app);
        assert!(install.contains("● USB installer"));
        assert!(install.contains("▌ Host profile  laptop"), "{install}");
        assert!(install.contains("Suggestions"));
        assert!(!install.contains("Disk selection") && !install.contains("Source ISO"));

        // Type a hostname here, then cycle the disk mode on Settings from the suggestions.
        press(
            &mut app,
            &[Down, Enter, Char('b'), Char('o'), Char('x'), Enter],
        );
        assert_eq!(app.text(Field::Hostname), "box");
        press(&mut app, &[Char('3')]);
        assert_eq!(app.step, Step::Settings);
        select(&mut app, Entry::Field(Field::DiskMode));
        press(&mut app, &[Right, Down, Enter]);
        assert_eq!(app.text(Field::DiskMode), "serial");
        let settings = lines(&app);
        assert!(settings.contains(&"  │  └─ Target disk serial  Not set".to_owned()));
        assert_eq!(
            settings.last().unwrap(),
            "✗ Serial disk selection needs the target disk serial"
        );

        // Run has the ISO, every problem under its tab, and a button that refuses.
        press(&mut app, &[Char('4')]);
        let run = lines(&app);
        assert!(run[0].starts_with("Source ISO"));
        let at = |text: &str| run.iter().position(|row| row.contains(text)).unwrap();
        assert!(at("Settings") < at("  ✗ Serial disk selection needs the target disk serial"));
        assert!(at("Run") < at("  ✗ Source ISO file does not exist"));
        select(&mut app, Entry::Run);
        assert!(!press(&mut app, &[Enter]));
        assert!(screen(&mut app).contains("[Run · USB installer]"));
        assert!(app.status.contains("Fix the listed problems"));
        // Save does what Ctrl+S does.
        select(&mut app, Entry::Save);
        press(&mut app, &[Enter]);
        assert!(directory.path().join("profile.json").is_file() && !app.unsaved());
        app.commit(Field::Keyboard, "us");

        // Unsaved changes need a second q; Ctrl+S clears that.
        let mut armed = false;
        let q = KeyEvent::new(Char('q'), KeyModifiers::NONE);
        // The first q only leaves the set-up pages for the host's overview.
        assert!(matches!(
            key_event(&mut app, q, &mut armed, &mut Clipboard::default()),
            Flow::Continue
        ));
        assert!(!armed && app.step == Step::Manage);
        assert!(matches!(
            key_event(&mut app, q, &mut armed, &mut Clipboard::default()),
            Flow::Continue
        ));
        assert!(armed);
        let save = KeyEvent::new(Char('s'), KeyModifiers::CONTROL);
        key_event(&mut app, save, &mut armed, &mut Clipboard::default());
        assert!(directory.path().join("profile.json").is_file() && !app.unsaved());
        assert!(matches!(
            key_event(&mut app, q, &mut armed, &mut Clipboard::default()),
            Flow::Quit
        ));
    }

    #[test]
    fn unlock_dialog_masks_the_password_and_closes_on_escape() {
        let mut app = unsaved_app(b"{}");
        app.unlock = Some(Input::new("hunter2".into()));
        app.unlock_failed = true;
        let text = screen(&mut app);
        assert!(text.contains("Password for "));
        assert!(text.contains("› •••••••"));
        assert!(text.contains("Wrong password"));
        assert!(!text.contains("hunter2"));
        let escape = Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        unlock_event(&mut app, &escape, &mut Desktop);
        assert!(app.unlock.is_none());
    }

    #[test]
    fn a_running_action_shows_its_progress_and_output() {
        let mut app = unsaved_app(b"{}");
        app.action = Action::Iso;
        let mut command = std::process::Command::new("sh");
        command.args([
            "-c",
            "echo copying; echo 'xorriso : UPDATE : Writing: 1s 46.4% fifo'; sleep 5",
        ]);
        let stage = crate::job::Stage {
            title: "Build unattended ISO".into(),
            command,
            source: None,
            input: None,
        };
        app.job = Some(Job::start(vec![stage], "", vec![]).unwrap());
        let mut text = String::new();
        for _ in 0..200 {
            app.job.as_mut().unwrap().tick();
            text = screen(&mut app);
            if text.contains("46%") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(text.contains("46%") && text.contains("copying"), "{text}");
        assert!(text.contains("Esc cancel"));
        let escape = Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        job_event(&mut app, &escape);
        for _ in 0..200 {
            app.job.as_mut().unwrap().tick();
            if !app.job.as_ref().unwrap().running() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(screen(&mut app).contains("Enter close"));
        job_event(
            &mut app,
            &Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        assert!(app.job.is_none());
    }

    #[test]
    fn writing_a_usb_drive_asks_before_erasing() {
        let mut app = unsaved_app(b"{}");
        app.confirm = Some("Erase /dev/sdz · Stick · 14.6G?".into());
        let text = screen(&mut app);
        assert!(text.contains("Erase /dev/sdz") && text.contains("y erase"));
        confirm_event(
            &mut app,
            &Event::Key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
        );
        assert!(app.confirm.is_none() && app.job.is_none());
    }

    /// A host that has answered, with a few tasks as it would list them.
    fn host_tasks() -> Vec<berth_api::Task> {
        serde_json::from_value(json!([
            {"name": "status", "label": "Status", "group": "System", "user": "root", "help": "How the host is."},
            {"name": "claude:status", "label": "Claude Code sign-in", "extends": ["sign-in:status"], "help": "Whether Claude is signed in."},
            {"name": "sign-in:status", "label": "Show who is signed in", "group": "Sign-In", "help": "Accounts."},
            {"name": "claude:login", "label": "Sign in to Claude Code", "group": "sign-in", "terminal": true},
            {"name": "reboot", "label": "Reboot", "group": "System", "user": "root", "confirm": "Restart the host?"},
            {"name": "backup:restore", "label": "Restore", "group": "Backup",
             "arguments": [{"name": "snapshot", "label": "Snapshot", "required": true, "choices": "restic snapshots"},
                           {"name": "target", "default": "/"}]},
        ]))
        .unwrap()
    }

    #[test]
    fn a_host_below_a_workstation_is_offered_the_controller_and_no_apply() {
        use crate::remote::State;
        let mut app = unsaved_app(br#"{"level": "reachable", "access": {"address": "192.0.2.4"}}"#);
        app.set_step(Step::Manage);
        app.host.level = berth_api::HostLevel::Reachable;
        app.host.state = State::Missing;
        app.host.seen = Some(berth_api::HostLevel::Reachable);
        let text = screen_text(&mut app);
        assert!(text.contains("Level · reachable"), "{text}");
        assert!(text.contains("▸ Install the controller") && text.contains("▸ Terminal over SSH"));
        assert!(!app.entries().contains(&Entry::Task("apply")));
        // Choosing it runs berth host install in the normal terminal.
        let at = app
            .entries()
            .iter()
            .position(|entry| *entry == Entry::Task("install-controller"))
            .unwrap();
        app.list.select(Some(at));
        press(&mut app, &[KeyCode::Enter]);
        let words = app.own_command.clone().unwrap();
        assert_eq!(words[..2], ["host".to_owned(), "install".to_owned()]);

        // A controller host lists berth's own beside its tasks; one it cannot
        // run is marked, and its reason is in the information box.
        app.own_command = None;
        app.config["level"] = json!("controller");
        app.host.level = berth_api::HostLevel::Controller;
        app.host.state = State::Ready(berth_api::VERSION.into());
        app.host.tasks = host_tasks();
        app.host.own = vec![
            berth_core::level::install_controller_action(
                "devbox",
                berth_api::HostLevel::Controller,
                None,
            ),
            berth_api::Action {
                name: "host-key".into(),
                label: "Host key".into(),
                group: "Access".into(),
                available: false,
                why: "devbox has no profile on the server".into(),
                ..berth_api::Action::default()
            },
        ];
        let rows: Vec<String> = app.rows().iter().map(|row| row.line.to_string()).collect();
        assert!(
            rows.iter()
                .any(|row| row.contains("▸ Update the controller"))
        );
        assert!(rows.iter().any(|row| row.contains("▸ Host key · not now")));
        assert_eq!(
            rows.iter().filter(|row| row.as_str() == "Set-Up").count(),
            1,
            "{rows:?}"
        );
        assert!(!app.entries().contains(&Entry::Task("apply")));
        let index = app
            .host
            .actions()
            .iter()
            .position(|action| action.name == "host-key")
            .unwrap();
        app.begin_task(index, false);
        assert!(app.status.contains("cannot run now: devbox has no profile"));
        assert!(app.own_command.is_none());
    }

    #[test]
    fn the_overview_is_what_the_host_lists() {
        use crate::remote::{Pending, State};
        let mut app = unsaved_app(b"{}");
        app.set_step(Step::Manage);
        assert_eq!(app.action, Action::Manage);
        // Without an address there is nothing to ask.
        let manage = screen(&mut app);
        assert!(manage.contains("✗ Enter an SSH hostname") && !manage.contains("▸ Status"));
        assert!(app.entries().contains(&Entry::Task("apply")));

        app.commit(Field::SshHost, "devbox.example");
        app.config["agent_username"] = json!("robot");
        assert!(screen_text(&mut app).contains("Asking the host"));
        // A host without berth's controller offers to apply, once.
        app.host.state = State::Missing;
        let applies = |app: &App| {
            app.entries()
                .iter()
                .filter(|entry| **entry == Entry::Task("apply"))
                .count()
        };
        assert!(screen_text(&mut app).contains("not on this host yet") && applies(&app) == 1);
        app.host.state =
            State::Unreachable("ssh: connect to host devbox.example: timed out\nmore".into());
        let text = screen_text(&mut app);
        assert!(text.contains("✗ ssh: connect to host") && text.contains("▸ Terminal over SSH"));
        assert!(app.entries().contains(&Entry::Task("connect")));

        // One that answered lists its tasks in their groups.
        app.host.state = State::Ready("0.2.0".into());
        app.host.tasks = host_tasks();
        let rows: Vec<String> = app.rows().iter().map(|row| row.line.to_string()).collect();
        let place = |text: &str| rows.iter().position(|row| row.contains(text)).unwrap();
        assert!(
            place("System") < place("▸ Status")
                && place("▸ Status") < place("▸ Terminal · as robot")
        );
        assert!(place("▸ Terminal") < place("▸ Reboot"));
        // Groups are matched whatever their case; a step has no row of its own.
        assert!(place("Sign-In") < place("▸ Show who is signed in"));
        assert!(place("▸ Show who is signed in") < place("▸ Sign in to Claude Code"));
        assert!(place("Backup") > place("▸ Sign in to Claude Code"));
        assert!(!rows.iter().any(|row| row.contains("Claude Code sign-in")));
        assert!(!rows.iter().any(|row| row.contains("Sessions")));
        let select = |app: &mut App, entry: Entry| {
            let at = app.entries().iter().position(|known| *known == entry);
            app.list.select(Some(at.unwrap()));
        };
        // The information box says what the plugins' steps add.
        select(&mut app, Entry::HostTask(1));
        let help = app.help();
        assert!(
            help.contains("### From plugins") && help.contains("**Claude Code sign-in**: Whether")
        );
        assert!(help.contains("berth run HOST sign-in:status"));

        // A task with a question waits for the yes.
        select(&mut app, Entry::HostTask(3));
        assert!(!press(&mut app, &[KeyCode::Enter]));
        assert_eq!(
            app.host.question,
            Some(("Restart the host?".into(), Pending::Action(3)))
        );
        assert!(screen_text(&mut app).contains("Restart the host?"));
        question_event(
            &mut app,
            &Event::Key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
        );
        assert!(app.host.question.is_none());

        // A task with arguments asks for each; a needed one cannot be left empty.
        select(&mut app, Entry::HostTask(4));
        press(&mut app, &[KeyCode::Enter]);
        let key = |app: &mut App, code| {
            arguments_event(
                app,
                &Event::Key(KeyEvent::new(code, KeyModifiers::NONE)),
                &mut Desktop,
            )
        };
        assert!(screen_text(&mut app).contains("Restore · 1 of 2"));
        key(&mut app, KeyCode::Enter);
        assert!(app.status.contains("Snapshot is needed"));
        app.host.arguments.as_mut().unwrap().choices = vec!["abc123".into(), "def456".into()];
        assert!(screen_text(&mut app).contains("○ def456"));
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Enter);
        let asked = app.host.arguments.as_ref().unwrap();
        assert_eq!((asked.index, asked.input.value()), (1, "/"));
        assert_eq!(asked.values["snapshot"], "def456");
        key(&mut app, KeyCode::Esc);
        assert!(app.host.arguments.is_none());

        // Sessions of the host get a group; a running one is not forgotten with x.
        app.host.sessions = serde_json::from_value(json!([
            {"id": "sabc", "kind": "terminal", "title": "robot on devbox", "command": ["bash", "-l"], "user": "agent",
             "started_by": {"level": "host", "uid": 0}, "started": "2026-11-12T10:00:00Z"},
            {"id": "sdef", "kind": "process", "title": "Update", "command": ["update"], "user": "root",
             "started_by": {"level": "host", "uid": 0}, "started": "2026-11-12T10:00:00Z",
             "ended": "2026-11-12T10:05:00Z", "exit_code": 2},
        ]))
        .unwrap();
        let text: String = app
            .rows()
            .iter()
            .map(|row| row.line.to_string() + "\n")
            .collect();
        assert!(
            text.contains("▸ robot on devbox · running") && text.contains("▸ Update · failed (2)")
        );
        select(&mut app, Entry::HostSession(0));
        assert!(
            app.help().contains("still running") && app.help().contains("berth attach HOST sabc")
        );
        press(&mut app, &[KeyCode::Char('x')]);
        assert!(app.status.contains("still running"));
        // Opening one puts it in the panel; a transcript takes its lines.
        select(&mut app, Entry::HostSession(1));
        press(&mut app, &[KeyCode::Enter]);
        let view = app.host.view.as_ref().unwrap();
        assert!(view.id == "sdef" && !view.terminal && !view.ours);
        assert!(screen_text(&mut app).contains("Ctrl+Q leave it running"));
        view_event(
            &mut app,
            &Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        );
        assert!(app.host.view.is_none());

        // Applying with a session running asks first; then it is the SSH install.
        app.config["hostname"] = json!("devbox-1");
        app.config["operator_ssh_public_key"] = json!("ssh-ed25519 AAAA me");
        select(&mut app, Entry::Task("apply"));
        assert!(!press(&mut app, &[KeyCode::Enter]));
        assert!(matches!(app.host.question, Some((_, Pending::Apply))));
        let yes = Event::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(matches!(question_event(&mut app, &yes), Flow::Run));
        assert_eq!(app.terminal_action, Some(Action::Ssh));
        assert_eq!(app.host.state, State::Idle);
        app.terminal_action = None;

        // Space switches the Terminal row between the two accounts.
        app.host.state = State::Ready("0.2.0".into());
        select(&mut app, Entry::Task("terminal"));
        press(&mut app, &[KeyCode::Char(' ')]);
        assert!(!app.as_agent && screen_text(&mut app).contains("the SSH user"));
        press(&mut app, &[KeyCode::Char(' ')]);
        // ssh's options come from berth's key ring, which is in a store.
        let scratch = tempfile::tempdir().unwrap();
        app.paths = Paths {
            config_dir: scratch.path().join("config"),
            data_dir: scratch.path().join("data"),
        };
        let command = format!("{:?}", app.agent_terminal().unwrap());
        assert!(command.contains("devbox.example") && command.contains("sudo -iu robot "));
        // The set-up tabs are a flow under the overview, opening on Install.
        select(&mut app, Entry::Setup);
        press(&mut app, &[KeyCode::Enter]);
        assert_eq!((app.step, app.action), (Step::Install, Action::Iso));
        press(&mut app, &[KeyCode::Char('4')]);
        assert_eq!(app.step, Step::Run);
        press(&mut app, &[KeyCode::Esc]);
        assert_eq!(app.step, Step::Manage);
        // x removes the value of the selected field, and with the address
        // gone what was known of the host is too.
        app.host.state = State::Ready("0.2.0".into());
        select(&mut app, Entry::Field(Field::SshHost));
        press(&mut app, &[KeyCode::Char('x')]);
        assert_eq!(app.text(Field::SshHost), "");
        assert_eq!(app.host.state, State::Idle);
    }

    const BERTH_KEY: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExampleKeyOfBerth1234 berth@studio";

    #[test]
    fn settings_show_the_key_of_berth_and_copy_it() {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: directory.path().join("config"),
            data_dir: directory.path().join("data"),
        };
        paths.set("server", "https://192.0.2.7:8443").unwrap();
        paths.set("server_token", "a1b2").unwrap();
        let mut home = Home {
            hosts: vec![],
            selected: 0,
            mode: HomeMode::Settings(0, None),
            status: String::new(),
            started: std::time::Instant::now(),
            qr: None,
            berth_key: Some((
                vec![("berth".into(), BERTH_KEY.into())],
                Some("https://192.0.2.7:8443".into()),
            )),
            served: None,
            known: None,
            asking: false,
            refresh: false,
            rotating: vec![],
        };
        let mut clipboard = Clipboard::default();
        let settings = berth_core::paths::SETTINGS.len();
        for _ in 0..settings {
            home_key(
                &mut home,
                &paths,
                KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
                &mut clipboard,
            );
        }
        // The key comes after the settings, before the row with the QR code.
        assert!(matches!(home.mode, HomeMode::Settings(index, _) if index == settings));
        let mut terminal = Terminal::new(TestBackend::new(130, 50)).unwrap();
        terminal
            .draw(|frame| draw_home(frame, &paths, &home, &mut vec![]))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let screen = (0..50)
            .map(|y| {
                (0..130)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            screen.contains("This berth's SSH key  ssh-ed25519 …erth1234 berth@studio"),
            "{screen}"
        );
        assert!(screen.contains("Enter copy the key") && screen.contains("berth server at"));
        assert!(screen.contains("Open on another device"));
        home_key(
            &mut home,
            &paths,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut clipboard,
        );
        assert_eq!(clipboard.0.as_deref(), Some(BERTH_KEY));
        assert_eq!(home.status, "Copied this berth's SSH public key");
        assert!(home.qr.is_none());
        // The row after it still shows the QR code.
        home_key(
            &mut home,
            &paths,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            &mut clipboard,
        );
        home_key(
            &mut home,
            &paths,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut clipboard,
        );
        assert!(home.qr.is_some());
        // Without a key to give there is no row for it.
        home.berth_key = Some((vec![], None));
        home.mode = HomeMode::Settings(settings, None);
        home.qr = None;
        home_key(
            &mut home,
            &paths,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut clipboard,
        );
        assert!(home.qr.is_some());
    }

    #[test]
    fn the_field_of_berths_key_copies_it() {
        let mut app = unsaved_app(b"{}");
        app.own_public = BERTH_KEY.into();
        app.set_step(Step::Settings);
        let at = app
            .entries()
            .iter()
            .position(|entry| *entry == Entry::Field(Field::BerthKey));
        app.list.select(at);
        let mut clipboard = Clipboard::default();
        let mut armed = false;
        let c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE);
        key_event(&mut app, c, &mut armed, &mut clipboard);
        assert_eq!(clipboard.0.as_deref(), Some(BERTH_KEY));
        assert_eq!(app.status, "Copied this berth's SSH public key");
        assert!(app.enabled(Field::BerthKey));
        let screen = screen_text(&mut app);
        assert!(screen.contains("[Copy the key c]") && screen.contains("c copy the key"));
        assert!(app.help().contains(BERTH_KEY));
        // Space still switches it.
        press(&mut app, &[KeyCode::Char(' ')]);
        assert!(!app.enabled(Field::BerthKey));
        // Elsewhere c is nothing of the kind.
        clipboard.0 = None;
        press(&mut app, &[KeyCode::Up]);
        key_event(&mut app, c, &mut armed, &mut clipboard);
        assert!(clipboard.0.is_none());
    }

    fn lab(id: &str, label: &str, resources: &[&str]) -> berth_api::ProvisionerInfo {
        let mut arguments = vec![berth_api::Argument {
            name: "server".into(),
            label: "Lab server".into(),
            required: true,
            ..Default::default()
        }];
        arguments.extend(resources.iter().map(|name| berth_api::Argument {
            name: (*name).into(),
            label: format!("Lab {name}"),
            default: "4".into(),
            kind: berth_api::Argument::RESOURCE.into(),
            ..Default::default()
        }));
        berth_api::ProvisionerInfo {
            id: id.into(),
            label: label.into(),
            setup: "apply".into(),
            arguments,
            ..Default::default()
        }
    }

    #[test]
    fn the_tabs_create_and_delete_the_machine() {
        use berth_tui::machine::Pick;
        let profile = format!(
            r#"{{"hostname": "studio", "operator_password_hash": "$6${}",
                "operator_ssh_public_keys": ["ssh-ed25519 AAAA studio"]}}"#,
            "a".repeat(40)
        );
        let mut app = unsaved_app(profile.as_bytes());
        app.set_step(Step::Install);
        app.action = Action::Machine;
        let machine_rows = |app: &App| -> Vec<Entry> {
            app.entries()
                .into_iter()
                .filter(|entry| matches!(entry, Entry::Machine(_)))
                .collect()
        };
        // Nothing was asked without berth's own folders, so nothing shows.
        assert!(machine_rows(&app).is_empty());
        app.machine.asking();
        app.machine.listed(Ok(vec![
            lab("lab/vm", "Lab machine", &["cpus", "memory"]),
            lab("lab/box", "Lab box", &["cpus"]),
        ]));
        app.machine.found(Ok(None));
        // The provisioner is one row, and what it requires is under it.
        assert!(lines(&app).contains(
            &"Provisioner  Lab machine  · berth applies the profile over SSH".to_owned()
        ));
        assert_eq!(
            machine_rows(&app),
            [
                Entry::Machine(Pick::Provisioner),
                Entry::Machine(Pick::Argument(0))
            ]
        );
        let install = lines(&app);
        assert!(install.contains(&"  └─ Lab server *  Not set".to_owned()));
        assert_eq!(install.last().unwrap(), "✗ Lab server is required");
        // Its size is under Machine on Settings, and goes with the provisioner.
        press(&mut app, &[KeyCode::Char('3')]);
        let settings = lines(&app);
        assert_eq!(
            settings[..3],
            ["Machine", "  ├─ Lab cpus  4", "  └─ Lab memory  4"]
        );
        press(&mut app, &[KeyCode::Char('1')]);
        select(&mut app, Entry::Machine(Pick::Provisioner));
        press(&mut app, &[KeyCode::Enter, KeyCode::Char('3')]);
        assert_eq!(lines(&app)[..2], ["Machine", "  └─ Lab cpus  4"]);
        press(&mut app, &[KeyCode::Char('1')]);
        select(&mut app, Entry::Machine(Pick::Provisioner));
        press(&mut app, &[KeyCode::Enter]);
        assert!(
            lines(&app)
                .iter()
                .any(|row| row.contains("Provisioner  Lab machine"))
        );

        // Creating with a required argument empty is refused before asking.
        press(&mut app, &[KeyCode::Char('4')]);
        let run = lines(&app);
        assert!(run.contains(&"  ✗ Lab server is required".to_owned()));
        select(&mut app, Entry::Machine(Pick::Create));
        press(&mut app, &[KeyCode::Enter]);
        assert!(app.machine.question.is_none() && app.status.contains("Fix the listed problems"));
        press(&mut app, &[KeyCode::Char('1')]);
        select(&mut app, Entry::Machine(Pick::Argument(0)));
        press(&mut app, &[KeyCode::Enter]);
        for ch in "192.0.2.10".chars() {
            press(&mut app, &[KeyCode::Char(ch)]);
        }
        press(&mut app, &[KeyCode::Enter]);
        assert_eq!(app.machine.values["server"], "192.0.2.10");
        // A size that is no size is a problem of Settings, and blocks Create.
        press(&mut app, &[KeyCode::Char('3')]);
        select(&mut app, Entry::Machine(Pick::Argument(2)));
        press(
            &mut app,
            &[KeyCode::Enter, KeyCode::Char('x'), KeyCode::Enter],
        );
        let settings = lines(&app);
        assert_eq!(
            settings.last().unwrap(),
            "✗ Lab memory must be a size with its unit, such as 8GiB, 512MiB or 4g"
        );
        press(&mut app, &[KeyCode::Char('4')]);
        assert!(
            lines(&app)
                .iter()
                .any(|row| row.contains("  ✗ Lab memory must be a size"))
        );
        select(&mut app, Entry::Machine(Pick::Create));
        press(&mut app, &[KeyCode::Enter]);
        assert!(app.machine.question.is_none() && app.status.contains("Fix the listed problems"));
        press(&mut app, &[KeyCode::Char('3')]);
        select(&mut app, Entry::Machine(Pick::Argument(2)));
        press(&mut app, &[KeyCode::Enter, KeyCode::Backspace]);
        for ch in "8GiB".chars() {
            press(&mut app, &[KeyCode::Char(ch)]);
        }
        press(&mut app, &[KeyCode::Enter, KeyCode::Char('4')]);
        assert!(lines(&app).contains(&"✓ Ready".to_owned()));
        select(&mut app, Entry::Machine(Pick::Create));
        press(&mut app, &[KeyCode::Enter]);
        let shown = screen_text(&mut app);
        assert!(
            shown.contains("Create a machine for studio with Lab machine?"),
            "{shown}"
        );
        app.machine_answer(false);
        assert!(app.machine.question.is_none() && app.job.is_none());

        // Another way to install has no provisioner to choose.
        app.action = Action::Ssh;
        app.set_step(Step::Install);
        assert!(machine_rows(&app).is_empty());
        // With a machine, its state and what can be done to it are on Run, whatever the way.
        app.machine.found(Ok(Some(berth_api::Resource {
            provisioner: "lab/vm".into(),
            id: "r-studio".into(),
            external_id: "studio".into(),
            state: "stopped".into(),
            verbs: vec!["start".into()],
            ..Default::default()
        })));
        app.set_step(Step::Run);
        assert_eq!(
            machine_rows(&app),
            [Entry::Machine(Pick::Start), Entry::Machine(Pick::Delete)]
        );
        select(&mut app, Entry::Machine(Pick::Delete));
        press(&mut app, &[KeyCode::Enter]);
        assert!(screen_text(&mut app).contains("Type studio to delete its machine"));
        for ch in "studi".chars() {
            press(&mut app, &[KeyCode::Char(ch)]);
        }
        press(&mut app, &[KeyCode::Enter]);
        assert!(app.status.starts_with("Not deleted") && app.job.is_none());
    }

    #[test]
    fn the_provisioner_is_a_choice_like_the_others() {
        use KeyCode::*;
        use berth_tui::machine::Pick;
        let mut app = unsaved_app(br#"{"profile": "laptop"}"#);
        app.machine.asking();
        app.machine.listed(Ok(vec![
            lab("lab/vm", "Lab machine", &["cpus"]),
            lab("lab/box", "Lab box", &[]),
        ]));
        app.machine.found(Ok(None));
        // Choosing the way makes the profile a cloud one, and offers none to choose.
        select(&mut app, Entry::Action(Action::Machine));
        press(&mut app, &[Enter]);
        assert_eq!(app.config["profile"], "cloud");
        assert!(!lines(&app).iter().any(|row| row.contains("Host profile")));
        // The row offers the provisioners in the panel, at the chosen one.
        select(&mut app, Entry::Machine(Pick::Provisioner));
        let shown = screen(&mut app);
        assert!(
            shown.contains("Suggestions") && shown.contains("Lab box"),
            "{shown}"
        );
        assert_eq!(
            app.suggestions,
            [
                ("Lab machine".to_owned(), "lab/vm".to_owned()),
                ("Lab box".to_owned(), "lab/box".to_owned())
            ]
        );
        assert_eq!(app.suggestion_list.selected(), Some(0));
        // Space goes on to the next, and the panel follows it.
        press(&mut app, &[Char(' ')]);
        assert_eq!(app.machine.chosen.as_deref(), Some("lab/box"));
        app.update_suggestions();
        assert_eq!(app.suggestion_list.selected(), Some(1));
        // → enters the panel, and Enter there chooses.
        press(&mut app, &[Right, Up, Enter]);
        assert_eq!(app.machine.chosen.as_deref(), Some("lab/vm"));
        assert!(!app.suggestion_focus);
        assert!(
            app.help()
                .contains("Space or Enter switches to the next provisioner")
        );
    }
}
