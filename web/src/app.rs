//! The page's screens: the token, the server's hosts, and one host.

use crate::browser::{self, Browser};
use crate::portal::{Link, Portal};
use crate::remote::Remote;
use berth_tui::host::{self, HostView, Pending, Pick, Request, State};
use berth_tui::screen::{
    self, Spots, clear_hints, dialog, draw_arguments, draw_footer, draw_header, draw_question,
    draw_view, ellipsize, hint_click, inside,
};
use berth_tui::{Click, Code, Event, Key, event, markdown, theme};
use ratzilla::ratatui::{
    Frame,
    layout::{Constraint, Layout, Margin, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph, Wrap},
};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use tui_input::Input;
use web_time::{Duration, Instant};

/// How often the list of hosts is asked for again.
const HOSTS_EVERY: Duration = Duration::from_secs(10);
/// How long to wait before opening a lost portal again.
const RETRY: Duration = Duration::from_secs(3);
/// How long something said on the status line stays.
const SAID_FOR: Duration = Duration::from_secs(5);
/// What the status line says while the server is away.
const AWAY: &str = "The berth server does not answer; trying again…";
/// The key that leaves a running session: a browser keeps Ctrl+Q to itself.
const LEAVE: &str = "Alt+Q";

#[derive(PartialEq)]
enum Screen {
    Token,
    Hosts,
    Host,
}

/// What the server last said of its hosts.
#[derive(Default)]
struct Hosts {
    list: Vec<Value>,
    error: Option<String>,
    answered: bool,
}

/// Something a row of the host's page does.
#[derive(Clone, Copy, PartialEq)]
enum Entry {
    Pick(Pick),
    /// Ask the host again.
    Again,
}

struct Row {
    line: Line<'static>,
    entry: Option<Entry>,
}

impl Row {
    fn info(line: impl Into<Line<'static>>) -> Row {
        Row {
            line: line.into(),
            entry: None,
        }
    }
}

pub struct App {
    screen: Screen,
    portal: Option<Portal>,
    /// The portal has been open with this token, so it is the right one.
    proven: bool,
    retry: Option<Instant>,
    /// The server: where the page came from, or the one that was named.
    server: String,
    /// `berth serve` gave this page, so the server is not asked for.
    embedded: bool,
    token: String,
    typing: Input,
    typing_server: Input,
    /// The server is being typed, not the token.
    on_server: bool,
    /// What the token dialog says about the last try.
    refused: String,
    hosts: Rc<RefCell<Hosts>>,
    asked: Option<Instant>,
    cursor: usize,
    /// The host that is open: its name and address.
    name: String,
    address: String,
    host: HostView,
    list: ListState,
    display: ListState,
    as_agent: bool,
    status: String,
    /// What the status line last said, and since when, for letting it go.
    said: (String, Instant),
    links: Spots,
    words: Spots,
    /// The rows of the last frame, for the mouse.
    hits: Vec<(Rect, usize)>,
    started: Instant,
    size: (u16, u16),
}

impl App {
    pub fn new() -> App {
        let embedded = browser::embedded();
        let (linked_server, linked_token) = browser::linked();
        let kept = browser::stored(browser::SERVER).unwrap_or_default();
        let server = match embedded {
            true => browser::origin(),
            false => linked_server
                .map(|server| browser::address(&server))
                .unwrap_or_else(|| kept.clone()),
        };
        // The token that was kept is the one of the server that was kept.
        let token = linked_token
            .or_else(|| {
                (embedded || server == kept)
                    .then(|| browser::stored(browser::TOKEN))
                    .flatten()
            })
            .unwrap_or_default();
        if !embedded {
            browser::store(browser::SERVER, &server);
        }
        browser::store(browser::TOKEN, &token);
        let mut app = App {
            screen: Screen::Token,
            portal: None,
            proven: false,
            retry: None,
            on_server: server.is_empty(),
            typing_server: Input::new(server.clone()),
            server,
            embedded,
            token,
            typing: Input::default(),
            refused: String::new(),
            hosts: Rc::default(),
            asked: None,
            cursor: 0,
            name: String::new(),
            address: String::new(),
            host: HostView::default(),
            list: ListState::default(),
            display: ListState::default(),
            as_agent: true,
            status: String::new(),
            said: (String::new(), Instant::now()),
            links: vec![],
            words: vec![],
            hits: vec![],
            started: Instant::now(),
            size: (100, 30),
        };
        if !app.token.is_empty() && !app.server.is_empty() {
            app.connect();
        }
        app
    }

    /// Open the portal with the token there is.
    fn connect(&mut self) {
        self.host.disconnect();
        self.asked = None;
        self.retry = None;
        match Portal::open(&browser::portal_url(&self.server), &self.token) {
            Ok(portal) => {
                self.portal = Some(portal);
                if self.screen == Screen::Token {
                    self.screen = Screen::Hosts;
                }
            }
            Err(_) => {
                self.portal = None;
                // A browser does not let a page over https open plain ws.
                self.refused = match self.server.starts_with("http://") {
                    true => {
                        "The browser did not open it: from a page over https, use an https address."
                            .into()
                    }
                    false => "The browser did not open that address.".into(),
                };
                self.screen = Screen::Token;
            }
        }
    }

    /// Back to the token dialog, forgetting the token.
    fn sign_out(&mut self, why: &str) {
        self.portal = None;
        self.proven = false;
        self.retry = None;
        self.host.disconnect();
        *self.hosts.borrow_mut() = Hosts::default();
        self.token.clear();
        browser::store(browser::TOKEN, "");
        self.typing = Input::default();
        self.refused = why.to_owned();
        self.screen = Screen::Token;
    }

    fn ask_hosts(&mut self) {
        let Some(portal) = &self.portal else {
            return;
        };
        self.asked = Some(Instant::now());
        let hosts = self.hosts.clone();
        portal.call("hosts.list", None, json!({}), Value::Null, move |result| {
            let mut hosts = hosts.borrow_mut();
            hosts.answered = true;
            match result {
                Ok(list) => {
                    hosts.list = list.as_array().cloned().unwrap_or_default();
                    hosts.error = None;
                }
                Err(error) => hosts.error = Some(error),
            }
        });
    }

    /// Keep in step with the server between frames.
    pub fn tick(&mut self) {
        if self.status != self.said.0 {
            self.said = (self.status.clone(), Instant::now());
        } else if !self.status.is_empty() && self.said.1.elapsed() >= SAID_FOR {
            self.status.clear();
        }
        match self.portal.as_ref().map(Portal::link) {
            Some(Link::Open) => {
                self.proven = true;
                if self.status == AWAY {
                    self.status.clear();
                }
            }
            // A token that never opened the portal is asked for again; one that
            // has is the right one, and the server is only away.
            Some(Link::Refused) if !self.proven => {
                self.sign_out("The server did not open its portal for that token.");
                return;
            }
            Some(Link::Refused | Link::Lost) => {
                self.portal = None;
                self.host.disconnect();
                self.retry = Some(Instant::now());
                self.status = AWAY.into();
            }
            _ => {}
        }
        if self.portal.is_none() {
            if self.retry.is_some_and(|since| since.elapsed() >= RETRY) {
                self.connect();
            }
            return;
        }
        if self
            .asked
            .is_none_or(|asked| asked.elapsed() >= HOSTS_EVERY)
            && self.host.view.is_none()
        {
            self.ask_hosts();
        }
        if self.screen != Screen::Host {
            return;
        }
        if self.host.state == State::Idle
            && let Some(portal) = &self.portal
        {
            self.host
                .connect(Box::new(Remote::new(portal.clone(), &self.name)));
        }
        if let Some(note) = self.host.pump() {
            self.status = note;
        }
        // A shell that was left needs no key to close its panel.
        if self
            .host
            .view
            .as_ref()
            .is_some_and(|view| view.shell && view.outcome() == Some(true))
        {
            self.host.close(true);
        }
        let (width, height) = self.size;
        self.host.resize(
            height.saturating_sub(4).max(4),
            width.saturating_sub(8).max(20),
        );
        let last = self.entries().len().saturating_sub(1);
        if self.list.selected().unwrap_or(0) > last {
            self.list.select(Some(last));
        }
    }

    // ── One host ────────────────────────────────────────────────────────────

    fn terminal_user(&self) -> &'static str {
        match self.as_agent {
            true => "agent",
            false => "the SSH user",
        }
    }

    fn rows(&self) -> Vec<Row> {
        let heading = |text: &str| {
            Row::info(Line::styled(
                text.to_owned(),
                Style::new().fg(theme::ACCENT).bold(),
            ))
        };
        let note = |text: String, colour| {
            Row::info(Line::styled(format!("  {text}"), Style::new().fg(colour)))
        };
        let again = || Row {
            line: "  ▸ Try again".into(),
            entry: Some(Entry::Again),
        };
        let mut rows = vec![];
        if let Some(line) = self.host.stats_line() {
            rows.push(Row::info(Line::styled(line, Style::new().fg(theme::MUTED))));
            rows.push(Row::info(""));
        }
        rows.push(heading("Connection"));
        rows.push(Row::info(format!(
            "  {} · through the berth server at {}",
            self.address,
            browser::shown(&self.server)
        )));
        let said = match &self.host.state {
            State::Ready(_) => {
                for (title, group) in host::groups(&self.host, self.terminal_user()) {
                    rows.push(Row::info(""));
                    rows.push(heading(&title));
                    rows.extend(group.into_iter().map(|(line, pick)| Row {
                        line: line.into(),
                        entry: Some(Entry::Pick(pick)),
                    }));
                }
                return rows;
            }
            State::Idle | State::Connecting | State::Password => {
                vec![note("Asking the host what it can do…".into(), theme::MUTED)]
            }
            State::Missing => vec![
                note(
                    "berth's controller is not on this host yet. Applying its configuration, where berth serve runs, installs it.".into(),
                    theme::WARN,
                ),
                again(),
            ],
            State::Old(version) => vec![
                note(
                    format!(
                        "The host's tools ({version}) are older than this page needs. Applying its configuration, where berth serve runs, brings them up to date."
                    ),
                    theme::WARN,
                ),
                again(),
            ],
            State::Unreachable(message) => vec![
                note(
                    format!("✗ {}", message.lines().next().unwrap_or("")),
                    theme::BAD,
                ),
                again(),
            ],
        };
        rows.push(Row::info(""));
        rows.push(heading("Host"));
        rows.extend(said);
        rows
    }

    fn entries(&self) -> Vec<Entry> {
        self.rows()
            .into_iter()
            .filter_map(|row| row.entry)
            .collect()
    }

    fn selected(&self) -> Option<Entry> {
        self.entries()
            .get(self.list.selected().unwrap_or(0))
            .copied()
    }

    fn help(&self) -> String {
        match self.selected() {
            Some(Entry::Pick(Pick::Task(index))) => host::task_help(&self.host, index),
            Some(Entry::Pick(Pick::Session(index))) => host::session_help(&self.host, index),
            Some(Entry::Pick(Pick::Terminal)) => format!(
                "A shell on the host, as one of its sessions. Space switches between the agent and the SSH user.\n\nLeaving the shell closes it; {LEAVE} leaves it running, to be opened again from Sessions."
            ),
            Some(Entry::Again) => "Ask the host again what it can do.".into(),
            None => String::new(),
        }
    }

    fn open_host(&mut self, name: &str, address: &str) {
        self.name = name.to_owned();
        self.address = address.to_owned();
        self.host.disconnect();
        self.list.select(Some(0));
        self.display = ListState::default();
        self.status.clear();
        self.screen = Screen::Host;
    }

    fn launch(&mut self, index: usize, values: BTreeMap<String, String>) {
        let Some(task) = self.host.tasks.get(index).cloned() else {
            return;
        };
        // Secrets are kept where berth is installed, and travel from there.
        if task.input.as_deref() == Some("secrets") {
            self.status = format!(
                "{} sends secrets, which this page does not hold: berth run {} {}",
                host::task_label(&task),
                self.name,
                task.name
            );
            return;
        }
        let body = self.host.task_body(&task, &values);
        self.status = format!("Starting {}…", host::task_label(&task));
        self.host.run(&task.name, body);
    }

    fn choose(&mut self, entry: Entry) {
        match entry {
            Entry::Pick(Pick::Task(index)) => {
                if let Some((index, values)) = self.host.begin_task(index, false) {
                    self.launch(index, values);
                }
            }
            Entry::Pick(Pick::Session(index)) => {
                if let Some(session) = self.host.sessions.get(index).cloned() {
                    self.host.open(&session);
                }
            }
            Entry::Pick(Pick::Terminal) => {
                let (user, title) = match self.as_agent {
                    true => ("agent", format!("agent on {}", self.name)),
                    false => ("operator", self.name.clone()),
                };
                self.host.shell(user, &title);
                self.status = "Starting a shell…".into();
            }
            Entry::Again => self.host.disconnect(),
        }
    }

    fn move_selection(&mut self, up: bool) {
        let count = self.entries().len();
        if count == 0 {
            return;
        }
        let now = self.list.selected().unwrap_or(0);
        self.list.select(Some(match up {
            true => (now + count - 1) % count,
            false => (now + 1) % count,
        }));
    }

    fn host_key(&mut self, key: Key) {
        self.status.clear();
        match key.code {
            Code::Up | Code::Char('k') => self.move_selection(true),
            Code::Down | Code::Char('j') => self.move_selection(false),
            Code::Enter => {
                if let Some(entry) = self.selected() {
                    self.choose(entry);
                }
            }
            Code::Char(' ') if self.selected() == Some(Entry::Pick(Pick::Terminal)) => {
                self.as_agent = !self.as_agent;
            }
            Code::Char('r') => {
                self.host.send(Request::Refresh);
                self.status = "Asking the host again…".into();
            }
            Code::Char('x') | Code::Delete => {
                if let Some(Entry::Pick(Pick::Session(index))) = self.selected()
                    && let Some(session) = self.host.sessions.get(index)
                {
                    if session.running() {
                        self.status = "It is still running. Open it and end it first.".into();
                    } else {
                        self.status = format!("Forgot {}", session.title);
                        self.host.send(Request::Delete(session.id.clone()));
                    }
                }
            }
            Code::Char('q') | Code::Esc => {
                self.host.disconnect();
                self.screen = Screen::Hosts;
                self.asked = None;
            }
            _ => {}
        }
    }

    fn host_event(&mut self, event: Event) {
        if self.host.view.is_some() {
            if let Some(said) = screen::view_event(
                &mut self.host,
                &self.links,
                &self.words,
                &event,
                &mut Browser,
            ) {
                self.status = said;
            }
            return;
        }
        if self.host.question.is_some() {
            let Event::Key(key) = event else {
                return;
            };
            match key.code {
                Code::Char('y' | 'Y') => {
                    if let Some((_, Pending::Task(index))) = self.host.question.take()
                        && let Some((index, values)) = self.host.begin_task(index, true)
                    {
                        self.launch(index, values);
                    }
                }
                Code::Char('n' | 'N') | Code::Esc => self.host.question = None,
                _ => {}
            }
            return;
        }
        if self.host.arguments.is_some() {
            if screen::arguments_event(&mut self.host, &event) {
                match self.host.accept_argument() {
                    Ok(Some((index, values))) => self.launch(index, values),
                    Ok(None) => {}
                    Err(missing) => self.status = missing,
                }
            }
            return;
        }
        match event {
            Event::Key(key) => self.host_key(key),
            Event::Mouse(mouse) => match mouse.click {
                Click::ScrollUp => self.move_selection(true),
                Click::ScrollDown => self.move_selection(false),
                Click::Left => {
                    let hit = self
                        .hits
                        .iter()
                        .find(|(rect, _)| inside(*rect, mouse.column, mouse.row))
                        .map(|(_, index)| *index);
                    if let Some(index) = hit {
                        // A click selects; a click on what is selected chooses it.
                        if self.list.selected() == Some(index) {
                            self.host_key(Key::plain(Code::Enter));
                        } else {
                            self.list.select(Some(index));
                        }
                    }
                }
                Click::Right => {}
            },
            Event::Paste(_) => {}
        }
    }

    fn draw_host(&mut self, frame: &mut Frame, area: Rect) {
        let [header, _, panel, _, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(4),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(area);
        draw_header(frame, header, &[], 0, &self.name);
        let [left, _, right] = Layout::horizontal([
            Constraint::Percentage(62),
            Constraint::Length(2),
            Constraint::Min(0),
        ])
        .areas(panel);
        frame.render_widget(
            Paragraph::new(markdown::render(&self.help()))
                .block(theme::panel("Information", false))
                .wrap(Wrap { trim: false }),
            right,
        );
        // The cursor only stops on rows that do something.
        let rows = self.rows();
        let positions: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter_map(|(position, row)| row.entry.map(|_| position))
            .collect();
        self.display.select(
            self.list
                .selected()
                .and_then(|index| positions.get(index).copied()),
        );
        frame.render_stateful_widget(
            List::new(rows.iter().map(|row| ListItem::new(row.line.clone())))
                .highlight_symbol(theme::cursor())
                .highlight_style(Style::new().bg(theme::RAISED)),
            left,
            &mut self.display,
        );
        self.hits.clear();
        for (index, &position) in positions.iter().enumerate() {
            if position < self.display.offset() {
                continue;
            }
            let y = left.y + (position - self.display.offset()) as u16;
            if y >= left.bottom() {
                break;
            }
            self.hits.push((Rect::new(left.x, y, left.width, 1), index));
        }
        let keys = match self.selected() {
            Some(Entry::Pick(Pick::Terminal)) => {
                "↑↓ select · Enter open · Space switch user · r refresh · q hosts"
            }
            Some(Entry::Pick(Pick::Session(_))) => {
                "↑↓ select · Enter open · x forget · r refresh · q hosts"
            }
            _ => "↑↓ select · Enter choose · r refresh · q hosts",
        };
        draw_footer(frame, footer, keys, &self.status, theme::ACCENT);
        (self.links, self.words) = match &self.host.view {
            Some(view) => draw_view(frame, view, &self.status, LEAVE),
            None => (vec![], vec![]),
        };
        if let Some((question, _)) = &self.host.question {
            draw_question(frame, question);
        }
        draw_arguments(frame, &self.host);
    }

    // ── The hosts ───────────────────────────────────────────────────────────

    fn hosts_event(&mut self, event: Event) {
        let count = self.hosts.borrow().list.len();
        let open = |app: &mut App, index: usize| {
            let host = app.hosts.borrow().list.get(index).cloned();
            if let Some(host) = host
                && let Some(name) = host["name"].as_str()
            {
                app.open_host(name, host["address"].as_str().unwrap_or(""));
            }
        };
        match event {
            Event::Key(key) => match key.code {
                Code::Up | Code::Char('k') if count > 0 => {
                    self.cursor = (self.cursor + count - 1) % count
                }
                Code::Down | Code::Char('j') if count > 0 => {
                    self.cursor = (self.cursor + 1) % count
                }
                Code::Enter => open(self, self.cursor),
                Code::Char('r') => self.ask_hosts(),
                Code::Char('x') => self.sign_out(""),
                _ => {}
            },
            Event::Mouse(mouse) if mouse.click == Click::Left => {
                let hit = self
                    .hits
                    .iter()
                    .find(|(rect, _)| inside(*rect, mouse.column, mouse.row))
                    .map(|(_, index)| *index);
                match hit {
                    Some(index) if index == self.cursor => open(self, index),
                    Some(index) => self.cursor = index,
                    None => {}
                }
            }
            _ => {}
        }
    }

    fn draw_hosts(&mut self, frame: &mut Frame, area: Rect) {
        let [header, _, logo, main, water, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(4),
            Constraint::Min(1),
            Constraint::Length(2),
            Constraint::Length(1),
        ])
        .areas(area);
        draw_header(frame, header, &[], 0, browser::shown(&self.server));
        frame.render_widget(Paragraph::new(theme::logo()), logo);
        frame.render_widget(
            Paragraph::new(theme::swell(
                water.width,
                self.started.elapsed().as_secs_f32(),
            )),
            water,
        );
        let hosts = self.hosts.borrow();
        self.cursor = self.cursor.min(hosts.list.len().saturating_sub(1));
        let widest = hosts
            .list
            .iter()
            .filter_map(|host| host["name"].as_str())
            .map(|name| name.chars().count())
            .max()
            .unwrap_or(0);
        let muted = Style::new().fg(theme::MUTED);
        let mut lines: Vec<Line> = vec![];
        self.hits.clear();
        for (index, host) in hosts.list.iter().enumerate() {
            let name = host["name"].as_str().unwrap_or("");
            let state = match (host["reachable"].as_bool(), host["version"].as_str()) {
                (Some(true), Some(version)) => {
                    Span::styled(format!("● {version}"), Style::new().fg(theme::GOOD))
                }
                (Some(true), None) => Span::styled("●", Style::new().fg(theme::GOOD)),
                _ => Span::styled(
                    format!(
                        "○ {}",
                        host["error"]
                            .as_str()
                            .and_then(|error| error.lines().next())
                            .unwrap_or("does not answer")
                    ),
                    Style::new().fg(theme::WARN),
                ),
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{name:<widest$}  "), Style::new().fg(theme::TEXT)),
                Span::styled(
                    format!("{}  ", host["address"].as_str().unwrap_or("")),
                    muted,
                ),
                state,
            ]));
            let y = main.y + index as u16;
            if y < main.bottom() {
                self.hits.push((Rect::new(main.x, y, main.width, 1), index));
            }
        }
        if lines.is_empty() {
            let text = match (&hosts.error, hosts.answered) {
                (Some(error), _) => format!("✗ {error}"),
                (None, false) => "Asking the server for its hosts…".into(),
                (None, true) => {
                    "The server has no hosts yet. They are set up with berth where it runs.".into()
                }
            };
            frame.render_widget(Paragraph::new(text).style(muted), main);
        } else {
            let mut state = ListState::default().with_selected(Some(self.cursor));
            frame.render_stateful_widget(
                List::new(lines)
                    .highlight_symbol(theme::cursor())
                    .highlight_style(Style::new().bg(theme::RAISED)),
                main,
                &mut state,
            );
        }
        drop(hosts);
        draw_footer(
            frame,
            footer,
            "↑↓ select · Enter open · r refresh · x forget the token",
            &self.status,
            theme::ACCENT,
        );
    }

    // ── The token ───────────────────────────────────────────────────────────

    fn token_event(&mut self, event: Event) {
        match event {
            Event::Key(key)
                if !self.embedded
                    && matches!(key.code, Code::Tab | Code::BackTab | Code::Up | Code::Down) =>
            {
                self.on_server = !self.on_server;
            }
            Event::Key(key) if key.code == Code::Enter && self.on_server => self.on_server = false,
            Event::Key(key) if key.code == Code::Enter => {
                let token = self.typing.value().trim().to_owned();
                let server = match self.embedded {
                    true => self.server.clone(),
                    false => browser::address(self.typing_server.value()),
                };
                if server.is_empty() {
                    self.on_server = true;
                } else if !token.is_empty() {
                    self.token = token;
                    self.server = server;
                    if !self.embedded {
                        browser::store(browser::SERVER, &self.server);
                        self.typing_server = Input::new(self.server.clone());
                    }
                    browser::store(browser::TOKEN, &self.token);
                    self.typing = Input::default();
                    self.refused.clear();
                    self.status.clear();
                    self.connect();
                }
            }
            Event::Key(key) if self.on_server => event::edit(&mut self.typing_server, &key),
            Event::Key(key) => event::edit(&mut self.typing, &key),
            Event::Paste(text) if self.on_server => {
                event::paste(&mut self.typing_server, text.trim())
            }
            Event::Paste(text) => event::paste(&mut self.typing, text.trim()),
            Event::Mouse(_) => {}
        }
    }

    fn draw_token(&mut self, frame: &mut Frame, area: Rect) {
        let [header, _, logo, _, water] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(4),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .areas(area);
        draw_header(frame, header, &[], 0, browser::shown(&self.server));
        frame.render_widget(Paragraph::new(theme::logo()), logo);
        frame.render_widget(
            Paragraph::new(theme::swell(
                water.width,
                self.started.elapsed().as_secs_f32(),
            )),
            water,
        );
        let masked = ellipsize(&"•".repeat(self.typing.value().chars().count()), 44);
        let named = ellipsize(self.typing_server.value(), 44);
        let mut lines = match self.embedded {
            true => vec![
                Line::raw("The token of this berth server. It is in serve.token,"),
                Line::raw("in the data directory of the berth that serves."),
            ],
            false => vec![
                Line::raw("The address of a berth serve, and its token. The token"),
                Line::raw("is in serve.token, in that berth's data directory."),
            ],
        };
        if !self.refused.is_empty() {
            for line in screen::wrap(&self.refused, 56) {
                lines.push(Line::styled(line, Style::new().fg(theme::WARN)));
            }
        }
        lines.push(Line::raw(""));
        let field = |label: &'static str, value: &str, active: bool| {
            Line::from(vec![
                Span::styled(label, Style::new().fg(theme::MUTED)),
                Span::styled(
                    "› ",
                    Style::new().fg(if active { theme::ACCENT } else { theme::LINE }),
                ),
                Span::raw(value.to_owned()),
            ])
        };
        if !self.embedded {
            lines.push(field("Server  ", &named, self.on_server));
        }
        let server_row = lines.len() as u16;
        lines.push(field("Token   ", &masked, !self.on_server));
        let rows = lines.len() as u16;
        let keys = match self.embedded {
            true => " Enter connect ",
            false => " Tab switch · Enter connect ",
        };
        let area = dialog(frame, " Connect ", keys, lines);
        if area.width > 14 && area.height > rows {
            let (typed, row) = match self.on_server {
                true => (named, server_row),
                false => (masked, rows),
            };
            let column = Span::raw(typed).width() as u16;
            frame.set_cursor_position((area.x + 12 + column, area.y + row));
        }
    }

    // ── Either ──────────────────────────────────────────────────────────────

    pub fn event(&mut self, event: Event) {
        let event = hint_click(event);
        match self.screen {
            Screen::Token => self.token_event(event),
            Screen::Hosts => self.hosts_event(event),
            Screen::Host => self.host_event(event),
        }
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        clear_hints();
        let full = frame.area();
        self.size = (full.width, full.height);
        frame.render_widget(theme::backdrop(), full);
        let area = full.inner(Margin {
            horizontal: 2,
            vertical: 1,
        });
        match self.screen {
            Screen::Token => self.draw_token(frame, area),
            Screen::Hosts => self.draw_hosts(frame, area),
            Screen::Host => self.draw_host(frame, area),
        }
    }
}
