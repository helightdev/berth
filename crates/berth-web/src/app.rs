//! The page's screens: the token, the server's hosts, and one host.

use crate::admin::{Admin, Arrived, Data};
use crate::browser::{self, Browser};
use crate::home::Home;
use crate::installed::{Installed, Outcome};
use crate::portal::{Link, Portal};
use crate::remote::Remote;
use crate::setup::{Flow, Setup, draw_editor, masked_line};
use berth_tui::host::{self, HostView, Pending, Pick, Request, State, View};
use berth_tui::screen::{
    self, Platform, Spots, clear_hints, dialog, draw_arguments, draw_footer, draw_header,
    draw_panel, draw_question, draw_view, ellipsize, hint_click, inside,
};
use berth_tui::{Click, Code, Event, Key, event, markdown, qr, theme};
use ratatui::{
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
    /// What a setup key in the link opens.
    Installed,
    Hosts,
    Host,
    Setup,
}

/// The tabs of the home screen, for whoever may set the server up.
const TABS: [&str; 5] = ["Hosts", "Settings", "Registries", "Secrets", "Plugins"];

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
    /// Open the set-up pages of the host.
    Setup,
    /// Apply the host's profile to it, from the server.
    Apply,
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
    /// What the server keeps for setting it up, and the calls that change it.
    data: Rc<RefCell<Data>>,
    admin: Option<Admin>,
    /// The tab of the home screen: the hosts, then the ones of `home`.
    tab: usize,
    home: Home,
    tab_hits: Vec<Rect>,
    /// The name of a new host, while it is typed.
    naming: Option<Input>,
    /// Removing a host waits for a second x.
    armed: bool,
    setup: Option<Setup>,
    /// The SSH user's sudo password, asked for to apply a host.
    password: Option<Input>,
    /// A rendered seed, in a panel, and its text for copying.
    seed: Option<(View, String)>,
    /// A password the server made up, shown until it is put away: what it
    /// is for, and the password.
    made: Option<(&'static str, String)>,
    /// The web addresses of a server's job or a seed in its panel.
    over_links: Spots,
    /// A link as a QR code, over everything else.
    qr: Option<qr::Overlay>,
    /// The token collected with a setup key, until Done.
    installed: Option<Installed>,
}

impl App {
    pub fn new() -> App {
        let embedded = browser::embedded();
        let linked = browser::linked();
        let (linked_server, linked_token) = (linked.server, linked.token);
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
            data: Rc::default(),
            admin: None,
            tab: 0,
            home: Home::default(),
            tab_hits: vec![],
            naming: None,
            armed: false,
            setup: None,
            password: None,
            seed: None,
            made: None,
            over_links: vec![],
            qr: None,
            installed: None,
        };
        // A setup key signs in with the token it collects, whatever was kept.
        if let (Some(key), false) = (linked.setup_key, app.server.is_empty()) {
            app.installed = Some(Installed::claim(&app.server, key));
            app.screen = Screen::Installed;
            return app;
        }
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
                // Who the token is here decides what the page offers.
                let admin = Admin::new(portal.clone(), self.data.clone());
                admin.load();
                self.admin = Some(admin);
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
        self.admin = None;
        *self.data.borrow_mut() = Data::default();
        self.setup = None;
        self.installed = None;
        self.tab = 0;
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
        if let Some(installed) = &mut self.installed {
            match installed.tick() {
                Outcome::SignIn(token) => {
                    self.token = token;
                    browser::store(browser::TOKEN, &self.token);
                    self.connect();
                }
                // A token this browser kept still signs it in.
                Outcome::Refused(why) => {
                    self.installed = None;
                    self.screen = Screen::Token;
                    match self.token.is_empty() {
                        true => self.refused = why,
                        false => {
                            self.status = why;
                            self.connect();
                        }
                    }
                }
                Outcome::Stay | Outcome::Done => {}
            }
        }
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
        self.take_in();
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

    /// The size a panel for output gets.
    fn panel_size(&self) -> (u16, u16) {
        let (width, height) = self.size;
        (
            height.saturating_sub(4).max(4),
            width.saturating_sub(8).max(20),
        )
    }

    /// The server, and the link that opens this page signed in with its
    /// token: at this page's own address, which is the server's when the
    /// server gave it, and told which server otherwise.
    fn connect_link(&self) -> Option<(String, String)> {
        let server = (!self.embedded).then_some(self.server.as_str());
        let link = qr::connect_link(&browser::page(), server, &self.token)?;
        Some((self.server.clone(), link))
    }

    fn is_admin(&self) -> bool {
        self.data.borrow().admin()
    }

    /// Take in what the server answered since the last frame.
    fn take_in(&mut self) {
        let size = self.panel_size();
        let (said, arrived, changed) = {
            let mut data = self.data.borrow_mut();
            if let Some(job) = &mut data.job {
                job.resize(size.0, size.1);
            }
            (
                data.said.take(),
                std::mem::take(&mut data.arrived),
                std::mem::take(&mut data.hosts_changed),
            )
        };
        if let Some(said) = said {
            self.status = said;
        }
        if changed {
            self.ask_hosts();
        }
        for arrived in arrived {
            match arrived {
                Arrived::Profile(profile) => {
                    if let Some(setup) = &mut self.setup {
                        setup.arrived(profile);
                    }
                }
                Arrived::Saved(profile) => {
                    if let Some(setup) = &mut self.setup {
                        setup.saved(profile);
                    }
                }
                Arrived::Generated(field, value, password) => {
                    if let Some(setup) = &mut self.setup {
                        setup.generated(field, &value);
                        self.made = password.map(|password| (field.spec().label, password));
                    }
                }
                Arrived::Seed(text) => {
                    let mut view = View::plain("Cloud-init user data", size.0, size.1);
                    view.feed(text.as_bytes());
                    view.ended = Some(Some(0));
                    self.seed = Some((view, text));
                }
                Arrived::Password => {
                    self.status.clear();
                    self.password = Some(Input::default());
                }
                Arrived::Provisioners(listed) => {
                    if let Some(setup) = &mut self.setup {
                        setup.machine.listed(listed);
                    }
                }
                Arrived::Instance(found) => {
                    if let Some(setup) = &mut self.setup {
                        setup.machine.found(found);
                    }
                }
                Arrived::Session(session) => {
                    if self.screen == Screen::Host
                        && let Ok(session) = serde_json::from_value(session)
                    {
                        self.host.open(&session);
                    }
                }
            }
        }
        if let Some(setup) = &mut self.setup {
            setup.panel = size;
            setup.sync(&self.data.borrow());
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
                if let Some(text) = host::behind_note(&self.host) {
                    rows.push(Row::info(""));
                    rows.push(heading("Configuration"));
                    rows.push(note(text, theme::WARN));
                    if self.is_admin() {
                        rows.push(Row {
                            line: "  ▸ Apply this configuration".into(),
                            entry: Some(Entry::Apply),
                        });
                    }
                }
                for (title, group) in host::groups(&self.host, self.terminal_user()) {
                    rows.push(Row::info(""));
                    rows.push(heading(&title));
                    rows.extend(group.into_iter().map(|(line, pick)| Row {
                        line: line.into(),
                        entry: Some(Entry::Pick(pick)),
                    }));
                }
                rows.extend(self.setup_rows(true));
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
        rows.extend(self.setup_rows(!matches!(
            self.host.state,
            State::Idle | State::Connecting | State::Password
        )));
        rows
    }

    /// What whoever may set the server up can do with the host from here.
    fn setup_rows(&self, apply: bool) -> Vec<Row> {
        if !self.is_admin() {
            return vec![];
        }
        let mut rows = vec![
            Row::info(""),
            Row::info(Line::styled(
                "Set-Up",
                Style::new().fg(theme::ACCENT).bold(),
            )),
            Row {
                line: "  ▸ Change what is installed, accounts or install method  ›".into(),
                entry: Some(Entry::Setup),
            },
        ];
        if apply {
            rows.push(Row {
                line: "  ▸ Apply this configuration".into(),
                entry: Some(Entry::Apply),
            });
        }
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
            Some(Entry::Setup) => "Opens the set-up pages of this host: what is installed, its accounts, and how to install it. Esc comes back here. Changes reach a running host with Apply this configuration.".into(),
            Some(Entry::Apply) => "Run the installer role against the host again, from the server, with the settings of its profile: for example after changing what is installed. It also installs or updates berth's own tools on the host. The output is shown here as it runs, and the job stays in the server's list.".into(),
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
        // Secrets are kept by the server, and travel from there.
        if task.input.as_deref() == Some("secrets") {
            if let (true, Some(admin)) = (self.is_admin(), &self.admin) {
                self.status = format!("Starting {}…", host::task_label(&task));
                admin.send_secrets(&self.name);
                return;
            }
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
            Entry::Setup => {
                if let Some(admin) = &self.admin {
                    self.setup = Some(Setup::open(&self.name, admin));
                    self.screen = Screen::Setup;
                }
            }
            Entry::Apply => {
                self.host.question = Some((
                    format!("Apply the configuration of {} to it now?", self.name),
                    Pending::Apply,
                ))
            }
        }
    }

    fn apply(&mut self, password: Option<&str>) {
        if let Some(admin) = &self.admin {
            admin.apply(&self.name, password, self.panel_size());
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
                Code::Char('y' | 'Y') => match self.host.question.take() {
                    Some((_, Pending::Task(index))) => {
                        if let Some((index, values)) = self.host.begin_task(index, true) {
                            self.launch(index, values);
                        }
                    }
                    Some((_, Pending::Apply)) => self.apply(None),
                    None => {}
                },
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
            Some(Entry::Setup) => "↑↓ select · Enter open · r refresh · q hosts",
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

    // ── The home screen ─────────────────────────────────────────────────────

    fn open_tab(&mut self, tab: usize) {
        if self.is_admin() && tab < TABS.len() {
            self.tab = tab;
            self.home.leave();
            self.naming = None;
            self.data.borrow_mut().report.clear();
        }
    }

    /// The hosts, and for whoever may add one the row that does.
    fn host_rows(&self) -> usize {
        self.hosts.borrow().list.len() + usize::from(self.is_admin())
    }

    fn open_selected(&mut self) {
        let host = self.hosts.borrow().list.get(self.cursor).cloned();
        match host {
            Some(host) => {
                if let Some(name) = host["name"].as_str() {
                    self.open_host(name, host["address"].as_str().unwrap_or(""));
                }
            }
            None if self.is_admin() => self.naming = Some(Input::default()),
            None => {}
        }
    }

    /// Start the set-up of a new host under the name that was typed.
    fn name_host(&mut self, name: &str) {
        let safe = !name.is_empty()
            && !name.starts_with('.')
            && name
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'));
        let known = self
            .hosts
            .borrow()
            .list
            .iter()
            .any(|host| host["name"] == name);
        let template = self.data.borrow().template.clone();
        if !safe || name.ends_with(".json") {
            self.status = "A name of letters, digits, - and _".into();
        } else if known {
            self.status = format!("There is a host {name} already");
        } else if !template.is_object() {
            self.status = "The server has not said yet what a new host starts from".into();
        } else {
            self.naming = None;
            self.name = name.to_owned();
            self.address.clear();
            self.status = format!("{name} is kept on the server once it is saved");
            if let Some(admin) = &self.admin {
                self.setup = Some(Setup::new(name, template, admin));
                self.screen = Screen::Setup;
            }
        }
    }

    fn hosts_key(&mut self, key: Key) {
        let armed = std::mem::take(&mut self.armed);
        if let Some(input) = &mut self.naming {
            match key.code {
                Code::Esc => self.naming = None,
                Code::Enter => {
                    let name = input.value().trim().to_owned();
                    self.name_host(&name);
                }
                _ => event::edit(input, &key),
            }
            return;
        }
        let count = self.host_rows();
        let selected = self
            .hosts
            .borrow()
            .list
            .get(self.cursor)
            .and_then(|host| host["name"].as_str().map(str::to_owned));
        match key.code {
            Code::Up | Code::Char('k') if count > 0 => {
                self.cursor = (self.cursor + count - 1) % count
            }
            Code::Down | Code::Char('j') if count > 0 => self.cursor = (self.cursor + 1) % count,
            Code::Enter => self.open_selected(),
            Code::Char('r') => {
                self.ask_hosts();
                if let (true, Some(admin)) = (self.is_admin(), &self.admin) {
                    admin.refresh();
                }
            }
            Code::Char('n') if self.is_admin() => self.naming = Some(Input::default()),
            Code::Char('d') if self.is_admin() => {
                if let (Some(name), Some(admin)) = (selected, &self.admin) {
                    admin.set_setting("default_host", &name, format!("{name} is the default host"));
                }
            }
            Code::Char('x') | Code::Delete if self.is_admin() => match (selected, &self.admin) {
                (Some(name), Some(admin)) if armed => admin.remove_host(&name),
                (Some(name), _) => {
                    self.armed = true;
                    self.status = format!(
                        "x again removes {name} from the server. The host itself is left as it is."
                    );
                }
                _ => {}
            },
            Code::Char('o') => self.sign_out(""),
            _ => {}
        }
    }

    fn home_event(&mut self, event: Event) {
        let typing = self.naming.is_some() || self.home.editor.is_some();
        if let Event::Mouse(mouse) = &event {
            if mouse.click != Click::Left {
                return;
            }
            let tab = self
                .tab_hits
                .iter()
                .position(|rect| inside(*rect, mouse.column, mouse.row));
            if let Some(tab) = tab {
                self.open_tab(tab);
                return;
            }
            let hit = self
                .hits
                .iter()
                .find(|(rect, _)| inside(*rect, mouse.column, mouse.row))
                .map(|(_, index)| *index);
            let Some(index) = hit else {
                return;
            };
            // A click selects; a click on what is selected chooses it.
            let selected = match self.tab {
                0 => self.cursor,
                tab => self.home.selected(tab - 1),
            };
            if index == selected {
                self.home_event(Event::Key(Key::plain(Code::Enter)));
            } else if !typing {
                match self.tab {
                    0 => self.cursor = index,
                    tab => {
                        self.home.select(tab - 1, index);
                        self.data.borrow_mut().report.clear();
                    }
                }
            }
            return;
        }
        if let Event::Paste(text) = &event {
            if let Some(input) = self.naming.as_mut().or(self.home.editor.as_mut()) {
                event::paste(input, text.trim());
            }
            return;
        }
        let Event::Key(key) = event else {
            return;
        };
        if !typing {
            self.status.clear();
            match key.code {
                Code::Char(digit @ '1'..='5') => {
                    return self.open_tab(digit as usize - '1' as usize);
                }
                Code::Tab if self.is_admin() => return self.open_tab((self.tab + 1) % TABS.len()),
                Code::BackTab if self.is_admin() => {
                    return self.open_tab((self.tab + TABS.len() - 1) % TABS.len());
                }
                Code::Esc | Code::Char('q') if self.tab != 0 => return self.open_tab(0),
                _ => {}
            }
        }
        match (self.tab, self.admin.clone()) {
            (0, _) | (_, None) => self.hosts_key(key),
            (tab, Some(admin)) => {
                if let Some(said) = self.home.key(tab - 1, key, &admin) {
                    self.status = said;
                }
                if let Some(shown) = self.home.qr.take() {
                    self.qr = Some(shown);
                }
            }
        }
    }

    fn draw_home(&mut self, frame: &mut Frame, area: Rect) {
        let [header, _, logo, main, water, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(4),
            Constraint::Min(1),
            Constraint::Length(2),
            Constraint::Length(1),
        ])
        .areas(area);
        let admin = self.is_admin();
        if !admin {
            self.tab = 0;
        }
        self.home.connect = self.connect_link();
        self.home.server = browser::shown(&self.server).to_owned();
        let tabs: &[&str] = if admin { &TABS } else { &[] };
        self.tab_hits = draw_header(frame, header, tabs, self.tab, browser::shown(&self.server));
        frame.render_widget(Paragraph::new(theme::logo()), logo);
        frame.render_widget(
            Paragraph::new(theme::swell(
                water.width,
                self.started.elapsed().as_secs_f32(),
            )),
            water,
        );
        self.hits.clear();
        if self.tab == 0 {
            return self.draw_hosts(frame, main, footer);
        }
        // Rows on the left with the editor under them, what the selected row
        // means on the right.
        let shown = self.home.shown(self.tab - 1, &self.data.borrow());
        let [left, _, right] = Layout::horizontal([
            Constraint::Percentage(64),
            Constraint::Length(2),
            Constraint::Min(0),
        ])
        .areas(main);
        frame.render_widget(
            Paragraph::new(markdown::render(&shown.help))
                .block(theme::panel("Information", false))
                .wrap(Wrap { trim: false }),
            right,
        );
        let [body, edit] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(if shown.editor.is_some() { 3 } else { 0 }),
        ])
        .areas(left);
        let mut state = ListState::default().with_selected(Some(shown.selected));
        let count = shown.rows.len();
        frame.render_stateful_widget(
            List::new(shown.rows)
                .highlight_symbol(theme::cursor())
                .highlight_style(Style::new().bg(theme::RAISED)),
            body,
            &mut state,
        );
        for index in state.offset()..count {
            let y = body.y + (index - state.offset()) as u16;
            if y >= body.bottom() {
                break;
            }
            self.hits.push((Rect::new(body.x, y, body.width, 1), index));
        }
        if let (Some((title, masked)), Some(input)) = (shown.editor, &self.home.editor) {
            draw_editor(frame, edit, title, input, masked);
        }
        draw_footer(frame, footer, shown.keys, &self.status, theme::ACCENT);
    }

    fn draw_hosts(&mut self, frame: &mut Frame, main: Rect, footer: Rect) {
        let admin = self.is_admin();
        let preferred = self.data.borrow().setting("default_host").to_owned();
        let hosts = self.hosts.borrow();
        let count = hosts.list.len() + usize::from(admin);
        self.cursor = self.cursor.min(count.saturating_sub(1));
        let widest = hosts
            .list
            .iter()
            .filter_map(|host| host["name"].as_str())
            .map(|name| name.chars().count())
            .max()
            .unwrap_or(0);
        let muted = Style::new().fg(theme::MUTED);
        let mut lines: Vec<Line> = vec![];
        for host in hosts.list.iter() {
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
            let address = match host["address"].as_str().unwrap_or("") {
                "" => "no address yet",
                address => address,
            };
            let mut line = vec![
                Span::styled(format!("{name:<widest$}  "), Style::new().fg(theme::TEXT)),
                Span::styled(format!("{address}  "), muted),
                state,
            ];
            if host["behind"].is_object() {
                line.push(Span::styled("  behind", Style::new().fg(theme::WARN)));
            }
            if !preferred.is_empty() && name == preferred {
                line.push(Span::styled("  default", Style::new().fg(theme::STEEL)));
            }
            lines.push(Line::from(line));
        }
        if admin {
            lines.push(Line::styled("+ New host", Style::new().fg(theme::ACCENT)));
        }
        let [body, edit] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(if self.naming.is_some() { 3 } else { 0 }),
        ])
        .areas(main);
        for index in 0..lines.len() {
            let y = body.y + index as u16;
            if y < body.bottom() {
                self.hits.push((Rect::new(body.x, y, body.width, 1), index));
            }
        }
        if lines.is_empty() {
            let text = match (&hosts.error, hosts.answered) {
                (Some(error), _) => format!("✗ {error}"),
                (None, false) => "Asking the server for its hosts…".into(),
                (None, true) => {
                    "The server has no hosts yet. They are set up with berth where it runs, or from here with the role admin.".into()
                }
            };
            frame.render_widget(Paragraph::new(text).style(muted), body);
        } else {
            let mut state = ListState::default().with_selected(Some(self.cursor));
            frame.render_stateful_widget(
                List::new(lines)
                    .highlight_symbol(theme::cursor())
                    .highlight_style(Style::new().bg(theme::RAISED)),
                body,
                &mut state,
            );
        }
        drop(hosts);
        if let Some(input) = &self.naming {
            draw_editor(frame, edit, " New host name ", input, false);
        }
        let keys = match (admin, self.naming.is_some()) {
            (_, true) => "Enter create · Esc cancel",
            (true, false) => {
                "↑↓ select · Enter open · n new host · d default · x remove · 2 settings · 3 registries · 4 secrets · 5 plugins · r refresh · o sign out"
            }
            (false, false) => "↑↓ select · Enter open · r refresh · o sign out",
        };
        draw_footer(frame, footer, keys, &self.status, theme::ACCENT);
    }

    // ── What lies over a screen ─────────────────────────────────────────────

    /// The panels and dialogs that take every key while they are open.
    /// Returns whether the event was one of theirs.
    fn over_event(&mut self, event: &Event) -> bool {
        if let Some(input) = &mut self.password {
            match event {
                Event::Key(key) => match key.code {
                    Code::Esc => self.password = None,
                    Code::Enter => {
                        let password = input.value().to_owned();
                        self.password = None;
                        if !password.is_empty() {
                            self.apply(Some(&password));
                        }
                    }
                    _ => event::edit(input, key),
                },
                Event::Paste(text) => event::paste(input, text.trim()),
                Event::Mouse(_) => {}
            }
            return true;
        }
        if let Some((_, password)) = &self.made {
            if let Event::Key(key) = event {
                match key.code {
                    Code::Char('c') => {
                        self.status = match Browser.copy(password) {
                            true => "Copied the password".into(),
                            false => "The browser did not let the page copy".into(),
                        };
                    }
                    Code::Enter | Code::Esc => self.made = None,
                    _ => {}
                }
            }
            return true;
        }
        let scroll = |view: &mut View, event: &Event| {
            if let Event::Mouse(mouse) = event {
                match mouse.click {
                    Click::ScrollUp => view.scroll(3),
                    Click::ScrollDown => view.scroll(-3),
                    _ => {}
                }
            }
        };
        if let Some((view, text)) = &mut self.seed {
            scroll(view, event);
            if let Event::Key(key) = event {
                match key.code {
                    Code::Char('c') => {
                        self.status = match Browser.copy(text) {
                            true => "Copied the user data".into(),
                            false => "The browser did not let the page copy".into(),
                        };
                    }
                    Code::Enter | Code::Esc | Code::Char('q') => self.seed = None,
                    Code::PageUp => view.scroll(10),
                    Code::PageDown => view.scroll(-10),
                    _ => {}
                }
            }
            return true;
        }
        let mut data = self.data.borrow_mut();
        if let Some(view) = &mut data.job {
            scroll(view, event);
            if let Event::Key(key) = event {
                match key.code {
                    Code::Enter | Code::Esc | Code::Char('q') => data.job = None,
                    Code::PageUp => view.scroll(10),
                    Code::PageDown => view.scroll(-10),
                    _ => {}
                }
            }
            return true;
        }
        false
    }

    fn draw_over(&mut self, frame: &mut Frame) {
        self.over_links.clear();
        if let Some(view) = &self.data.borrow().job {
            let keys = match view.outcome() {
                None => " It runs on the server · Esc hides it · Wheel scrolls ",
                Some(true) => " ✓ Done · Enter close ",
                Some(false) => " ✗ It did not finish · Enter close ",
            };
            (self.over_links, _) = draw_panel(
                frame,
                &view.title,
                view.screen(),
                view.outcome(),
                keys,
                &self.status,
            );
        }
        if let Some((view, _)) = &self.seed {
            (self.over_links, _) = draw_panel(
                frame,
                &view.title,
                view.screen(),
                None,
                " c copy all of it · Wheel scrolls · Enter close ",
                &self.status,
            );
        }
        if let Some((label, password)) = &self.made {
            let lines = vec![
                Line::raw(format!("The password behind the {}:", label.to_lowercase())),
                Line::raw(""),
                Line::styled(password.clone(), Style::new().fg(theme::ACCENT)),
                Line::raw(""),
                Line::raw("It is shown this once. The server keeps a copy beside the"),
                Line::raw("host's profile, readable by the user it runs as."),
            ];
            dialog(
                frame,
                " Generated password ",
                " c copy · Enter close ",
                lines,
            );
        }
        if let Some(input) = &self.password {
            let lines = vec![
                Line::raw(format!("The SSH user's sudo password on {}.", self.name)),
                Line::raw("The server keeps it until it stops, and stores it nowhere."),
                Line::raw(""),
                masked_line("Password  ", input.value(), true),
            ];
            dialog(frame, " Password ", " Enter apply · Esc cancel ", lines);
        }
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

    fn installed_event(&mut self, event: Event) {
        let Some(installed) = &mut self.installed else {
            self.screen = Screen::Token;
            return;
        };
        if let Outcome::Done = installed.event(event, &mut self.status) {
            let data = self.data.clone();
            installed.done(move |problem| data.borrow_mut().said = problem);
            self.installed = None;
            self.screen = match self.portal {
                Some(_) => Screen::Hosts,
                None => Screen::Token,
            };
        }
    }

    /// The header, the logo and the water, behind a dialog.
    fn draw_backdrop(&mut self, frame: &mut Frame, area: Rect) {
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
    }

    fn draw_token(&mut self, frame: &mut Frame, area: Rect) {
        self.draw_backdrop(frame, area);
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
        // Alt+R over a panel shows its links as QR codes.
        let over = self.seed.is_some() || self.data.borrow().job.is_some();
        let links = match (
            over,
            self.screen == Screen::Host && self.host.view.is_some(),
        ) {
            (true, _) => Some(&self.over_links),
            (false, true) => Some(&self.links),
            (false, false) => None,
        };
        if let Some(links) = links.or(self.qr.as_ref().map(|_| &self.over_links)) {
            let mut said = None;
            if qr::take(&mut self.qr, links, &event, &mut said) {
                if let Some(said) = said {
                    self.status = said;
                }
                return;
            }
        }
        if self.over_event(&event) {
            return;
        }
        match self.screen {
            Screen::Token => self.token_event(event),
            Screen::Installed => self.installed_event(event),
            Screen::Hosts => self.home_event(event),
            Screen::Host => self.host_event(event),
            Screen::Setup => {
                let (Some(setup), Some(admin)) = (&mut self.setup, &self.admin) else {
                    self.screen = Screen::Hosts;
                    return;
                };
                if let Flow::Leave = setup.event(event, admin, &mut self.status) {
                    // A host the server never got has no overview to go back to.
                    let kept = setup.kept();
                    self.setup = None;
                    self.asked = None;
                    match kept {
                        true => {
                            let (name, address) = (self.name.clone(), self.address.clone());
                            self.open_host(&name, &address);
                        }
                        false => self.screen = Screen::Hosts,
                    }
                }
            }
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
            Screen::Installed => {
                self.draw_backdrop(frame, area);
                if let Some(installed) = &self.installed {
                    installed.draw(frame);
                }
            }
            Screen::Hosts => self.draw_home(frame, area),
            Screen::Host => self.draw_host(frame, area),
            Screen::Setup => match &mut self.setup {
                Some(setup) => setup.draw(frame, area, &self.status),
                None => self.draw_home(frame, area),
            },
        }
        self.draw_over(frame);
        if let Some(shown) = &self.qr {
            shown.draw(frame);
        }
    }
}
