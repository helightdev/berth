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
    let host = app.path.file_stem().unwrap_or_default().to_string_lossy();
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
        let name = if app.password_entry {
            " New password "
        } else {
            " Edit "
        };
        let mut title = vec![Span::raw(name)];
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
        "Enter accept · ↑↓ field · Tab next tab · Esc cancel"
    } else {
        match app.step {
            Step::Manage => "↑↓ select · Enter choose · x unset · Ctrl+S save · q hosts",
            _ => "↑↓ select · Enter choose · 1-4 tab · x unset · Ctrl+S save · Esc overview",
        }
    };
    draw_footer(frame, footer, keys, &app.status, theme::ACCENT);
    if let Some(picker) = &mut app.picker {
        picker.draw(frame);
    }
    if let Some(job) = &app.job {
        (app.links, app.words) = draw_job(frame, job, app.action.label(), &app.status, area);
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
    if spot_event(app, event) {
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
            Some((_, crate::remote::Pending::Task(index))) => app.begin_task(index, true),
            Some((_, crate::remote::Pending::Apply)) => return apply(app, true),
            None => {}
        },
        KeyCode::Char('n' | 'N') | KeyCode::Esc => app.host.question = None,
        _ => {}
    }
    Flow::Continue
}

/// The dialog that asks for a task's arguments.
fn arguments_event(app: &mut App, event: &Event) {
    if let Some(event) = crate::keys::event(event)
        && berth_tui::screen::arguments_event(&mut app.host, &event)
    {
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
    if spot_event(app, event) {
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
    let area = dialog(frame, &title, " Enter unlock · Esc cancel ", lines);
    if area.width > 6 && area.height > 5 {
        frame.set_cursor_position((area.x + 4 + typed as u16, area.y + 4));
    }
}

fn unlock_event(app: &mut App, event: &Event) {
    let Some(input) = &mut app.unlock else {
        return;
    };
    match event {
        Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
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
        Entry::Source(0) => app.detect(),
        Entry::Source(_) => app.editing = Some(Input::default()),
        Entry::Action(action) => {
            app.action = action;
            // A laptop profile would add a display manager and power policy to a server.
            if action == Action::CloudSeed && app.text(Field::Profile) == "laptop" {
                app.config["profile"] = json!("cloud");
            }
        }
        Entry::Field(field) => app.activate(field),
        Entry::Setup => app.set_step(Step::Features),
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
        Entry::Task("apply") => return apply(app, false),
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

/// The size of the terminal panel for the window as it is now.
fn session_size() -> (u16, u16) {
    let (width, height) = crossterm::terminal::size().unwrap_or((100, 30));
    (
        height.saturating_sub(4).max(4),
        width.saturating_sub(8).max(20),
    )
}

fn key_event(app: &mut App, key: KeyEvent, quit_armed: &mut bool) -> Flow {
    let armed = std::mem::take(quit_armed);
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('s') if control => {
            app.accept_edit();
            app.status = match app.save() {
                Ok(()) => format!("Saved {}", app.path.display()),
                Err(error) => format!("Save failed · {error}"),
            };
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
            KeyCode::Esc => app.cancel_edit(),
            KeyCode::Enter => app.accept_edit(),
            KeyCode::Up | KeyCode::Down => {
                app.accept_edit();
                app.move_selection(key.code == KeyCode::Up);
            }
            KeyCode::Char('u') if control => input.reset(),
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
            Some(entry @ (Entry::Field(_) | Entry::Feature(..) | Entry::HostRegistry(_))) => {
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
                "The password or token. It is kept in secrets.json on this machine, readable by you only, and is never shown. x forgets it."
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
fn registry_rows(store: &Store) -> Vec<(RegistryRow, Line<'static>)> {
    use RegistryRow::*;
    let muted = Style::new().fg(theme::MUTED);
    let field = |prefix: &str, label: &str, value: &str| {
        let mut line = crate::app::value_line(label, value, Kind::Text);
        line.spans.insert(0, Span::styled(prefix.to_owned(), muted));
        line
    };
    let mut rows = vec![];
    for (name, registry) in store.registries().unwrap_or_default() {
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
            .secret(&format!("registry/{name}"))
            .ok()
            .flatten()
            .is_some();
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

/// The Plugins tab: the selected row, the editor for what to install, whether a
/// removal waits for a second x, and what npm said last.
#[derive(Default)]
struct PluginView {
    selected: usize,
    editor: Option<Input>,
    armed: bool,
    report: Vec<String>,
}

/// Where the user's own plugins are.
fn plugin_dir(paths: &Paths) -> std::path::PathBuf {
    paths.config_dir.join("plugins")
}

/// The plugins of the source and the user's own.
fn found_plugins(paths: &Paths) -> Vec<crate::plugins::Plugin> {
    let shipped = paths
        .source()
        .map(|dir| dir.join("plugins"))
        .unwrap_or_default();
    crate::plugins::load(&[shipped, plugin_dir(paths)])
}

/// The rows of the Plugins tab: each line with the plugin it belongs to.
fn plugin_rows(
    paths: &Paths,
    plugins: &[crate::plugins::Plugin],
) -> Vec<(Option<usize>, Line<'static>)> {
    let muted = Style::new().fg(theme::MUTED);
    let own = plugin_dir(paths);
    let mut rows = vec![];
    for (index, plugin) in plugins.iter().enumerate() {
        let origin = if plugin.dir.starts_with(&own) {
            "yours"
        } else {
            "comes with berth"
        };
        rows.push((
            Some(index),
            Line::from(vec![
                Span::styled(plugin.label.clone(), Style::new().fg(theme::ACCENT).bold()),
                Span::styled(
                    format!("  {} {}  · {origin}", plugin.package, plugin.version),
                    muted,
                ),
            ]),
        ));
        for (number, feature) in plugin.features.iter().enumerate() {
            let branch = if number + 1 < plugin.features.len() {
                "  ├─ "
            } else {
                "  └─ "
            };
            rows.push((
                Some(index),
                Line::from(vec![
                    Span::styled(branch, muted),
                    Span::styled(feature.label.clone(), Style::new().fg(theme::TEXT)),
                    Span::styled(format!("  in {}", feature.group), muted),
                ]),
            ));
        }
    }
    rows.push((
        None,
        Line::styled("+ Add plugin", Style::new().fg(theme::ACCENT)),
    ));
    rows
}

/// Run npm for the user's plugin directory and return what it printed.
/// Nothing of a plugin runs here: install scripts are off.
fn npm(paths: &Paths, verb: &str, what: &str) -> Vec<String> {
    let dir = plugin_dir(paths);
    if let Err(error) = std::fs::create_dir_all(&dir) {
        return vec![format!("✗ {error}")];
    }
    let output = std::process::Command::new("npm")
        .arg(verb)
        .args([
            "--ignore-scripts",
            "--no-audit",
            "--no-fund",
            "--no-progress",
            "--prefix",
        ])
        .arg(&dir)
        .arg(what)
        .stdin(std::process::Stdio::null())
        .output();
    match output {
        Err(error) => vec![format!("✗ npm could not be started: {error}")],
        Ok(output) => {
            let mut lines = vec![if output.status.success() {
                format!("✓ npm {verb} {what}")
            } else {
                format!("✗ npm {verb} {what} failed")
            }];
            let text = [output.stdout, output.stderr].concat();
            lines.extend(
                String::from_utf8_lossy(&text)
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .take(12)
                    .map(str::to_owned),
            );
            lines
        }
    }
}

/// Handle a key on the Plugins tab.
fn plugins_key(
    view: &mut PluginView,
    paths: &Paths,
    key: KeyEvent,
    status: &mut String,
) -> Outcome {
    let plugins = found_plugins(paths);
    let rows = plugin_rows(paths, &plugins);
    view.selected = view.selected.min(rows.len() - 1);
    let selected = rows[view.selected].0.map(|index| &plugins[index]);
    if let Some(input) = &mut view.editor {
        match key.code {
            KeyCode::Esc => view.editor = None,
            KeyCode::Enter => {
                let spec = input.value().trim().to_owned();
                view.editor = None;
                if !spec.is_empty() {
                    let spec = crate::discovery::expand_path(&spec)
                        .to_str()
                        .filter(|_| spec.starts_with(['~', '.', '/']))
                        .map_or(spec.clone(), str::to_owned);
                    view.report = npm(paths, "install", &spec);
                    *status = view.report[0].clone();
                }
            }
            _ => {
                input.handle_event(&Event::Key(key));
            }
        }
        return Outcome::Stay;
    }
    let armed = std::mem::take(&mut view.armed);
    match key.code {
        KeyCode::Up => {
            view.selected = view.selected.saturating_sub(1);
            view.report.clear();
        }
        KeyCode::Down => {
            view.selected = (view.selected + 1).min(rows.len() - 1);
            view.report.clear();
        }
        KeyCode::Enter if selected.is_none() => view.editor = Some(Input::default()),
        KeyCode::Char('x') | KeyCode::Delete => match selected {
            Some(plugin) if !plugin.dir.starts_with(plugin_dir(paths)) => {
                *status = format!("{} comes with berth and stays", plugin.label);
            }
            Some(plugin) if !armed => {
                view.armed = true;
                *status = format!("x again removes {} from this machine", plugin.label);
            }
            // Installed with npm, or a folder that was copied there.
            Some(plugin)
                if plugin
                    .dir
                    .starts_with(plugin_dir(paths).join("node_modules")) =>
            {
                view.report = npm(paths, "uninstall", &plugin.package);
                *status = view.report[0].clone();
            }
            Some(plugin) => {
                *status = match std::fs::remove_dir_all(&plugin.dir) {
                    Ok(()) => format!("Removed {}", plugin.label),
                    Err(error) => error.to_string(),
                }
            }
            None => {}
        },
        KeyCode::Char('1') | KeyCode::Tab | KeyCode::Esc => return Outcome::Tab(0),
        KeyCode::Char('2') => return Outcome::Tab(1),
        KeyCode::Char('3') => return Outcome::Tab(2),
        KeyCode::Char('4') | KeyCode::BackTab => return Outcome::Tab(3),
        KeyCode::Char('q') => return Outcome::Quit,
        _ => {}
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
    let store = Store::new(paths);
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
                *status = format!("x again removes {name} from this machine");
            }
            None => {}
        },
        KeyCode::Char('1') | KeyCode::Esc => return Outcome::Tab(0),
        KeyCode::Char('2') => return Outcome::Tab(1),
        KeyCode::Char('3') | KeyCode::BackTab => return Outcome::Tab(2),
        KeyCode::Char('5') | KeyCode::Tab => return Outcome::Tab(4),
        KeyCode::Char('q') => return Outcome::Quit,
        _ => {}
    }
    Outcome::Stay
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
            let registry = match row {
                Name(old) => {
                    // The stored login moves with the name.
                    let secret = store.secret(&format!("registry/{old}"))?;
                    store.set_secret(&format!("registry/{old}"), None)?;
                    store.set_secret(&format!("registry/{value}"), secret.as_deref())?;
                    registries.remove(old).unwrap_or_default()
                }
                _ => json!({"type": "nexus", "url": ""}),
            };
            registries.insert(value.to_owned(), registry);
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
    let store = Store::new(paths);
    let rows = registry_rows(&store);
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
                            let named = registry_rows(&store)
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
        KeyCode::Char('1') | KeyCode::Esc => return Outcome::Tab(0),
        KeyCode::Char('2') | KeyCode::BackTab => return Outcome::Tab(1),
        KeyCode::Char('4') | KeyCode::Tab => return Outcome::Tab(3),
        KeyCode::Char('5') => return Outcome::Tab(4),
        KeyCode::Char('q') => return Outcome::Quit,
        _ => {}
    }
    Outcome::Stay
}

/// Open one of the home screen's tabs.
fn open_tab(home: &mut Home, tab: usize) {
    home.mode = match tab {
        1 => HomeMode::Settings(0, None),
        2 => HomeMode::Registries(RegistryView::default()),
        3 => HomeMode::Secrets(SecretView::default()),
        4 => HomeMode::Plugins(PluginView::default()),
        _ => HomeMode::Hosts,
    };
}

struct Home {
    hosts: Vec<(String, String)>,
    selected: usize,
    mode: HomeMode,
    status: String,
    /// The water moves with the time since then.
    started: std::time::Instant,
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

/// A setting's stored value, and what applies when it is not set.
fn setting(paths: &Paths, key: &str) -> (String, String) {
    let stored = paths.settings().unwrap_or_default();
    let stored = stored.get(key).and_then(|value| value.as_str());
    let fallback = match key {
        "source" => paths
            .source()
            .map(|dir| dir.display().to_string())
            .unwrap_or_else(|_| "not found".into()),
        "iso_dir" => paths.iso_dir().display().to_string(),
        _ => "none".into(),
    };
    (stored.unwrap_or_default().to_owned(), fallback)
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

/// The home screen: pick a saved host, name a new one or change berth's own
/// settings. `None` quits.
pub fn home(terminal: &mut DefaultTerminal, paths: &Paths) -> Result<Option<String>> {
    // A blinking cursor restarts its blink with every frame of the water, which
    // reads as flicker; a steady one does not.
    use crossterm::cursor::SetCursorStyle;
    crossterm::execute!(std::io::stdout(), SetCursorStyle::SteadyBar)?;
    let chosen = home_loop(terminal, paths);
    crossterm::execute!(std::io::stdout(), SetCursorStyle::BlinkingBar)?;
    chosen
}

fn home_loop(terminal: &mut DefaultTerminal, paths: &Paths) -> Result<Option<String>> {
    let hosts = paths.hosts();
    let preferred = setting(paths, "default_host").0;
    let mut home = Home {
        selected: hosts
            .iter()
            .position(|(name, _)| *name == preferred)
            .unwrap_or(0),
        hosts,
        mode: HomeMode::Hosts,
        status: String::new(),
        started: std::time::Instant::now(),
    };
    let mut hits = vec![];
    let mut fade = Fade::default();
    loop {
        fade.check(&mut home.status);
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
        let left = match hint_click(event::read()?) {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    return Ok(None);
                }
                home_key(&mut home, paths, key)
            }
            Event::Mouse(mouse) => home_mouse(&mut home, paths, mouse, &hits),
            Event::Paste(text) => {
                let editor = match &mut home.mode {
                    HomeMode::Naming(input) => Some(input),
                    HomeMode::Settings(_, editor) => editor.as_mut(),
                    HomeMode::Registries(view) => view.editor.as_mut(),
                    HomeMode::Secrets(view) => view.editor.as_mut(),
                    HomeMode::Plugins(view) => view.editor.as_mut(),
                    HomeMode::Hosts => None,
                };
                if let Some(input) = editor {
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

/// Handle the mouse on the home screen: the wheel moves the selection, a click
/// switches tab or chooses a row.
fn home_mouse(
    home: &mut Home,
    paths: &Paths,
    mouse: MouseEvent,
    hits: &[(Rect, HomeTarget)],
) -> Option<Option<String>> {
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    match mouse.kind {
        MouseEventKind::ScrollUp => home_key(home, paths, key(KeyCode::Up)),
        MouseEventKind::ScrollDown => home_key(home, paths, key(KeyCode::Down)),
        MouseEventKind::Down(MouseButton::Left) => {
            let (_, target) = hits
                .iter()
                .find(|(rect, _)| inside(*rect, mouse.column, mouse.row))?;
            match *target {
                HomeTarget::Tab(1) if matches!(home.mode, HomeMode::Settings(..)) => None,
                HomeTarget::Tab(2) if matches!(home.mode, HomeMode::Registries(..)) => None,
                HomeTarget::Tab(3) if matches!(home.mode, HomeMode::Secrets(..)) => None,
                HomeTarget::Tab(4) if matches!(home.mode, HomeMode::Plugins(..)) => None,
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
                            view.selected = index;
                            view.editor = None;
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
                    home_key(home, paths, key(KeyCode::Enter))
                }
            }
        }
        _ => None,
    }
}

/// Handle a key on the home screen. `Some` leaves it: with a host, or to quit.
fn home_key(home: &mut Home, paths: &Paths, key: KeyEvent) -> Option<Option<String>> {
    let outcome = match &mut home.mode {
        HomeMode::Registries(view) => Some(registries_key(view, paths, key, &mut home.status)),
        HomeMode::Secrets(view) => Some(secrets_key(view, paths, key, &mut home.status)),
        HomeMode::Plugins(view) => Some(plugins_key(view, paths, key, &mut home.status)),
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
        HomeMode::Registries(_) | HomeMode::Secrets(_) | HomeMode::Plugins(_) => {}
        HomeMode::Naming(input) => match key.code {
            KeyCode::Esc => home.mode = HomeMode::Hosts,
            KeyCode::Enter => {
                let name = input.value().trim().to_owned();
                if name.contains('/') || name.ends_with(".json") {
                    home.status = "Use letters, digits, - and _".into();
                } else if let Err(error) = paths.profile(&name) {
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
                home.status = match paths.set(name, value.value().trim()) {
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
            KeyCode::Down => *selected = (*selected + 1).min(crate::paths::SETTINGS.len() - 1),
            KeyCode::Enter => {
                let (name, _) = crate::paths::SETTINGS[*selected];
                *editor = Some(Input::new(setting(paths, name).0));
            }
            KeyCode::Char('3' | 'r') | KeyCode::Tab => {
                home.mode = HomeMode::Registries(RegistryView::default())
            }
            KeyCode::Char('4') => home.mode = HomeMode::Secrets(SecretView::default()),
            KeyCode::Char('5') => home.mode = HomeMode::Plugins(PluginView::default()),
            KeyCode::Char('1') | KeyCode::BackTab | KeyCode::Esc => home.mode = HomeMode::Hosts,
            KeyCode::Char('q') => return Some(None),
            _ => {}
        },
        HomeMode::Hosts => match key.code {
            KeyCode::Up => home.selected = home.selected.saturating_sub(1),
            KeyCode::Down => home.selected = (home.selected + 1).min(home.hosts.len()),
            KeyCode::Char('n') => home.mode = HomeMode::Naming(Input::default()),
            KeyCode::Char('2' | 's') | KeyCode::Tab => home.mode = HomeMode::Settings(0, None),
            KeyCode::Char('3' | 'r') => home.mode = HomeMode::Registries(RegistryView::default()),
            KeyCode::Char('4') => home.mode = HomeMode::Secrets(SecretView::default()),
            KeyCode::Char('5') | KeyCode::BackTab => {
                home.mode = HomeMode::Plugins(PluginView::default())
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
        HomeMode::Settings(..) => 1,
        HomeMode::Registries(..) => 2,
        HomeMode::Secrets(..) => 3,
        HomeMode::Plugins(..) => 4,
        _ => 0,
    };
    let settings = tab != 0;
    let tabs = draw_header(
        frame,
        header,
        &["Hosts", "Settings", "Registries", "Secrets", "Plugins"],
        tab,
        "",
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
            let rows = crate::paths::SETTINGS
                .iter()
                .map(|(name, _)| {
                    let (stored, fallback) = setting(paths, name);
                    if stored.is_empty() {
                        Line::from(vec![
                            Span::styled(
                                format!("{}  ", setting_label(name)),
                                Style::new().fg(theme::TEXT),
                            ),
                            Span::styled(format!("Default · {fallback}"), muted.italic()),
                        ])
                    } else {
                        crate::app::value_line(setting_label(name), &stored, Kind::Text)
                    }
                })
                .collect();
            let (name, help) = crate::paths::SETTINGS[*selected];
            (
                rows,
                *selected,
                editor.as_ref().map(|input| (" Edit ", input)),
                format!(
                    "The {help}.\n\nLeave it empty to use the default. Also: berth config {name} VALUE\n\nKept in {}",
                    paths.settings_file().display()
                ),
                if editor.is_some() {
                    "Enter accept · Esc cancel"
                } else {
                    "↑↓ select · Enter change · Tab registries · q quit"
                },
            )
        }
        HomeMode::Registries(view) => {
            let listed = registry_rows(&Store::new(paths));
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
                    "{}\n\nc checks that this registry answers and accepts its login.\n\nKept in {}",
                    row.help(),
                    paths.settings_file().display()
                ),
                false => view.report.join("\n\n"),
            };
            (
                listed.iter().map(|(_, line)| line.clone()).collect(),
                selected,
                view.editor.as_ref().map(|input| (title, input)),
                help,
                if view.editor.is_some() {
                    "Enter accept · Esc cancel"
                } else {
                    "↑↓ select · Enter change · c check · x remove · Tab secrets · q quit"
                },
            )
        }
        HomeMode::Secrets(view) => {
            let store = Store::new(paths);
            let names = store.secret_names();
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
                Some(_) => {
                    "A token, password or key file kept on this machine. Enter replaces its value, x twice removes it. Values are never shown."
                }
                None => {
                    "Add a secret: first a short name (lower-case letters, digits, - and _), then its value. Paste works."
                }
            };
            let help = format!(
                "{about}\n\nWhich host gets a secret, and whether as an environment variable or a file, is chosen on the Secrets page of that host.\n\nKept in {}, readable by you only, not encrypted.",
                paths.config_dir.join("secrets.json").display()
            );
            (
                rows,
                selected,
                view.editor.as_ref().map(|input| (title, input)),
                help,
                if view.editor.is_some() {
                    "Enter accept · Esc cancel"
                } else {
                    "↑↓ select · Enter set value · x remove · Tab plugins · q quit"
                },
            )
        }
        HomeMode::Plugins(view) => {
            let plugins = found_plugins(paths);
            let listed = plugin_rows(paths, &plugins);
            let selected = view.selected.min(listed.len() - 1);
            let help = match (
                view.report.is_empty(),
                listed[selected].0.map(|index| &plugins[index]),
            ) {
                (false, _) => view.report.join("\n"),
                (true, Some(plugin)) => {
                    let mut text = format!("{}\n\n", plugin.description);
                    for feature in &plugin.features {
                        text += &format!("{}: {}\n\n", feature.label, feature.help);
                    }
                    text + &format!(
                        "Its features are switched on per host, on the Features page.\n\nFrom {}",
                        plugin.dir.display()
                    )
                }
                (true, None) => format!(
                    "Install a plugin with npm: a package name from your registry, a .tgz from npm pack, or a folder. Registries and logins come from your npm configuration. Nothing of a plugin runs on this machine.\n\nA folder copied to {} works as well.",
                    plugin_dir(paths).display()
                ),
            };
            (
                listed.into_iter().map(|(_, line)| line).collect(),
                selected,
                view.editor
                    .as_ref()
                    .map(|input| (" Package, .tgz or folder ", input)),
                help,
                if view.editor.is_some() {
                    "Enter install · Esc cancel"
                } else {
                    "↑↓ select · Enter add · x remove · Tab hosts · q quit"
                },
            )
        }
        mode => {
            let preferred = setting(paths, "default_host").0;
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
                        Span::styled(format!("{name:<24}"), Style::new().fg(theme::TEXT)),
                        Span::styled(format!("{address:<28}"), muted),
                    ];
                    if *name == preferred {
                        line.push(Span::styled("default", Style::new().fg(theme::STEEL)));
                    }
                    Line::from(line)
                })
                .collect();
            rows.push(Line::styled("+ New host", Style::new().fg(theme::ACCENT)));
            let help = String::new();
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
                    "Enter create · Esc cancel"
                } else {
                    "↑↓ select · Enter open · n new host · d default · 2 settings · 3 registries · 4 secrets · 5 plugins · q quit"
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
    draw_footer(frame, footer, keys, &home.status, theme::WARN);
}

/// Run a host's screen. Returns true when the chosen action has to run in the
/// normal terminal, and false when the user left the host.
pub fn run(terminal: &mut DefaultTerminal, app: &mut App) -> Result<bool> {
    let mut display = ListState::default();
    let mut hits: Hits = vec![];
    let mut quit_armed = false;
    let mut fade = Fade::default();
    loop {
        // "x again to quit" and the like stay until the next key.
        let fading = match quit_armed {
            true => None,
            false => fade.check(&mut app.status),
        };
        app.update_suggestions();
        app.watch_host();
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
        if let Some(left) = fading
            && !event::poll(left)?
        {
            continue;
        }
        let event = hint_click(event::read()?);
        if app.unlock.is_some() {
            unlock_event(app, &event);
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
        if app.host.question.is_some() {
            if let Flow::Run = question_event(app, &event) {
                return Ok(true);
            }
            continue;
        }
        if app.host.arguments.is_some() {
            arguments_event(app, &event);
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
        match key_event(app, key, &mut quit_armed) {
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
    use ratatui::{Terminal, backend::TestBackend};

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
        let mut app = App::new("x.json".into(), parse_config(b"{}").unwrap());
        // The coding agents are plugins that come with berth.
        app.set_plugins(crate::plugins::load(&["../plugins".into()]));
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
        };
        let keys = |home: &mut Home, codes: &[KeyCode]| {
            for &code in codes {
                home_key(home, &paths, KeyEvent::new(code, KeyModifiers::NONE));
            }
        };
        let typed = |text: &str| text.chars().map(KeyCode::Char).collect::<Vec<_>>();
        // The Secrets tab: a name, then the value, which the screen does not show.
        keys(&mut home, &[KeyCode::Char('4'), KeyCode::Enter]);
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
        let store = Store::new(&paths);
        assert_eq!(
            store.secret("github").unwrap().as_deref(),
            Some("ghp_value")
        );
        store.set_secret("registry/nexus", Some("pw")).unwrap();
        assert_eq!(store.secret_names(), ["github"]);

        // On a host: its Secrets page lists it, and takes how the host gets it.
        let mut app = App::new("x.json".into(), parse_config(b"{}").unwrap());
        app.config_dir = paths.config_dir.clone();
        app.set_step(Step::Secrets);
        assert_eq!(app.entries(), [Entry::HostSecret(0)]);
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
        };
        let key = |home: &mut Home, code| {
            home_key(home, &paths, KeyEvent::new(code, KeyModifiers::NONE));
        };
        key(&mut home, KeyCode::Char('5'));
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
        };
        let keys = |home: &mut Home, codes: &[KeyCode]| {
            for &code in codes {
                home_key(home, &paths, KeyEvent::new(code, KeyModifiers::NONE));
            }
        };
        let typed = |text: &str| text.chars().map(KeyCode::Char).collect::<Vec<_>>();
        use KeyCode::{Down, Enter, Up};
        // Hosts, then the Registries tab: only "+ Add registry" is there.
        keys(&mut home, &[KeyCode::Char('3'), Enter]);
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
        let store = Store::new(&paths);
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
            if matches!(key_event(app, key, &mut armed), Flow::Run) {
                return true;
            }
        }
        false
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
        };
        let press =
            |home: &mut Home, code| home_key(home, &paths, KeyEvent::new(code, KeyModifiers::NONE));
        assert!(press(&mut home, KeyCode::Char('d')).is_none());
        assert_eq!(paths.settings().unwrap()["default_host"], "laptop");
        for code in [
            KeyCode::Char('2'),
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
            home_mouse(home, &paths, mouse, &hits)
        };
        assert!(click(&mut home, 15, 1).is_none());
        assert!(matches!(home.mode, HomeMode::Hosts));
        assert_eq!(click(&mut home, 10, 7), Some(Some("laptop".into())));
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

    #[test]
    fn install_tab_goes_from_method_to_a_blocked_run() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = App::new(
            directory.path().join("profile.json"),
            parse_config(b"{}").unwrap(),
        );
        use KeyCode::*;
        // Install: the USB installer is the default method; its settings follow.
        press(&mut app, &[Tab, Tab, Tab, Down, Down, Down, Down, Down]);
        assert_eq!(app.action, Action::Iso);
        let install = screen(&mut app);
        assert!(install.contains("● USB installer"));
        assert!(install.contains("▌ Host profile  laptop"));
        assert!(install.contains("Suggestions"));

        // Type a hostname, then cycle the disk mode from the suggestions.
        press(
            &mut app,
            &[Down, Enter, Char('b'), Char('o'), Char('x'), Down],
        );
        assert_eq!(app.text(Field::Hostname), "box");
        press(&mut app, &[Right, Down, Enter]);
        assert_eq!(app.text(Field::DiskMode), "serial");
        assert!(screen(&mut app).contains("  └─ Target disk serial  Not set"));

        // Run sits below the list of what is missing and refuses to start.
        app.list.select(Some(app.entries().len() - 1));
        assert!(!press(&mut app, &[Enter]));
        let review = screen(&mut app);
        assert!(review.contains("✗ Serial disk selection needs the target disk serial"));
        assert!(review.contains("[Run · USB installer]"));
        assert!(app.status.contains("Fix the listed problems"));

        // Unsaved changes need a second q; Ctrl+S clears that.
        let mut armed = false;
        let q = KeyEvent::new(Char('q'), KeyModifiers::NONE);
        // The first q only leaves the set-up pages for the host's overview.
        assert!(matches!(key_event(&mut app, q, &mut armed), Flow::Continue));
        assert!(!armed && app.step == Step::Manage);
        assert!(matches!(key_event(&mut app, q, &mut armed), Flow::Continue));
        assert!(armed);
        let save = KeyEvent::new(Char('s'), KeyModifiers::CONTROL);
        key_event(&mut app, save, &mut armed);
        assert!(app.path.is_file() && !app.unsaved());
        assert!(matches!(key_event(&mut app, q, &mut armed), Flow::Quit));
    }

    #[test]
    fn unlock_dialog_masks_the_password_and_closes_on_escape() {
        let mut app = App::new("x.json".into(), parse_config(b"{}").unwrap());
        app.unlock = Some(Input::new("hunter2".into()));
        app.unlock_failed = true;
        let text = screen(&mut app);
        assert!(text.contains("Password for "));
        assert!(text.contains("› •••••••"));
        assert!(text.contains("Wrong password"));
        assert!(!text.contains("hunter2"));
        let escape = Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        unlock_event(&mut app, &escape);
        assert!(app.unlock.is_none());
    }

    #[test]
    fn a_running_action_shows_its_progress_and_output() {
        let mut app = App::new("x.json".into(), parse_config(b"{}").unwrap());
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
        let mut app = App::new("x.json".into(), parse_config(b"{}").unwrap());
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
    fn the_overview_is_what_the_host_lists() {
        use crate::remote::{Pending, State};
        let mut app = App::new("unused.json".into(), parse_config(b"{}").unwrap());
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
        select(&mut app, Entry::HostTask(2));
        let help = app.help();
        assert!(
            help.contains("### From plugins") && help.contains("**Claude Code sign-in**: Whether")
        );
        assert!(help.contains("berth run HOST sign-in:status"));

        // A task with a question waits for the yes.
        select(&mut app, Entry::HostTask(4));
        assert!(!press(&mut app, &[KeyCode::Enter]));
        assert_eq!(
            app.host.question,
            Some(("Restart the host?".into(), Pending::Task(4)))
        );
        assert!(screen_text(&mut app).contains("Restart the host?"));
        question_event(
            &mut app,
            &Event::Key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
        );
        assert!(app.host.question.is_none());

        // A task with arguments asks for each; a needed one cannot be left empty.
        select(&mut app, Entry::HostTask(5));
        press(&mut app, &[KeyCode::Enter]);
        let key = |app: &mut App, code| {
            arguments_event(app, &Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
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
        let command = format!("{:?}", app.agent_terminal().unwrap());
        assert!(command.contains("devbox.example") && command.contains("sudo -iu robot "));
        // The set-up pages are a flow under the overview, with the install method.
        select(&mut app, Entry::Setup);
        press(&mut app, &[KeyCode::Enter, KeyCode::Char('4')]);
        assert_eq!((app.step, app.action), (Step::Install, Action::Iso));
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
}
