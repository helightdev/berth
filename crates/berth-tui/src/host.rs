//! A host as the overview knows it: what it can do, its sessions, and the
//! panel onto one of them. How the host is reached is the [`Transport`]'s
//! business; requests go out through it and what happened comes back.

use crate::event::{Key, key_bytes};
use crate::page::{self, Accepted, Begin};
pub use crate::page::{Arguments, Launch, argument_label};
use berth_api as api;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use web_time::{Duration, Instant};

/// The lowest `api` of a host this berth can work with.
pub const NEEDS_API: u32 = 1;
/// How often the overview asks for stats and sessions again.
const POLL: Duration = Duration::from_secs(10);
/// Lines of a process session's transcript kept for scrolling back.
const HISTORY: usize = 5000;

/// The way to one host. Requests are answered later, as events.
pub trait Transport {
    fn send(&self, request: Request);
    /// The next thing that happened, if anything has.
    fn next(&self) -> Option<Event>;
}

pub enum Request {
    /// Version, tasks, sessions and stats.
    Refresh,
    /// Sessions and stats.
    Poll,
    /// Run a task; the answer is `Opened`.
    Run {
        name: String,
        body: Value,
    },
    /// Start a session; the answer is `Opened`.
    Start {
        body: Value,
    },
    /// Follow a session until `Close`.
    Open(Box<api::Session>),
    Input(Vec<u8>),
    Resize(u16, u16),
    Close,
    Delete(String),
    Choices {
        task: String,
        argument: String,
    },
}

pub enum Event {
    Version(api::Version),
    /// How the host's configuration differs from what would be applied now;
    /// nothing when it is current or does not say.
    Behind(Option<api::Behind>),
    /// The host could not be asked. `missing` when it has no controller.
    Failed {
        message: String,
        missing: bool,
    },
    Tasks(Vec<api::Task>),
    Sessions(Vec<api::Session>),
    Stats(Box<api::Stats>),
    Opened(Box<api::Session>),
    /// Session id and bytes, of either stream.
    Output(String, Vec<u8>),
    Exited(String, Option<i32>),
    /// The stream of a session ended without it having exited.
    Closed(String),
    Choices {
        task: String,
        argument: String,
        choices: Vec<String>,
    },
    Error(String),
}

/// A session of the host shown in the terminal panel.
pub struct View {
    pub id: String,
    pub title: String,
    pub terminal: bool,
    parser: vt100::Parser,
    /// It has ended; with its exit code when it has one.
    pub ended: Option<Option<i32>>,
    /// It was started from this panel, so closing it once it has ended forgets it.
    pub ours: bool,
    /// A shell: its panel closes by itself when it is left.
    pub shell: bool,
    /// The last bytes of output, for a question that arrives in two pieces.
    tail: Vec<u8>,
}

impl View {
    pub fn new(session: &api::Session, rows: u16, cols: u16, ours: bool, shell: bool) -> View {
        let terminal = session.kind == api::SessionKind::Terminal;
        View {
            id: session.id.clone(),
            title: session.title.clone(),
            terminal,
            parser: vt100::Parser::new(rows, cols, if terminal { 2000 } else { HISTORY }),
            ended: None,
            ours,
            shell,
            tail: vec![],
        }
    }

    /// A panel onto output that is no session of a host: a job of the
    /// server, or something to read.
    pub fn plain(title: &str, rows: u16, cols: u16) -> View {
        View {
            id: String::new(),
            title: title.to_owned(),
            terminal: false,
            parser: vt100::Parser::new(rows, cols, HISTORY),
            ended: None,
            ours: true,
            shell: false,
            tail: vec![],
        }
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    pub fn running(&self) -> bool {
        self.ended.is_none()
    }

    /// Whether it succeeded, once it has ended.
    pub fn outcome(&self) -> Option<bool> {
        self.ended.map(|code| code == Some(0))
    }

    /// Take in output. Returns what a terminal answers by itself: programs
    /// ask where the cursor is and wait, and with a viewer attached the host
    /// leaves the answer to the viewer.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        if self.terminal {
            self.parser.process(bytes);
            self.tail.extend_from_slice(bytes);
            let asked = self
                .tail
                .windows(4)
                .filter(|window| window == b"\x1b[6n")
                .count();
            let keep = self.tail.len().saturating_sub(3);
            self.tail.drain(..keep);
            let (row, col) = self.screen().cursor_position();
            return format!("\x1b[{};{}R", row + 1, col + 1)
                .repeat(asked)
                .into_bytes();
        }
        // A transcript has plain line ends; the screen wants the carriage back too.
        for part in bytes.split_inclusive(|byte| *byte == b'\n') {
            match part.strip_suffix(b"\n") {
                Some(line) => {
                    self.parser.process(line);
                    self.parser.process(b"\r\n");
                }
                None => self.parser.process(part),
            }
        }
        vec![]
    }

    /// Returns whether the size changed.
    pub fn resize(&mut self, rows: u16, cols: u16) -> bool {
        if self.screen().size() == (rows, cols) {
            return false;
        }
        self.parser.screen_mut().set_size(rows, cols);
        true
    }

    pub fn scroll(&mut self, lines: isize) {
        let now = self.screen().scrollback() as isize;
        self.parser
            .screen_mut()
            .set_scrollback((now + lines).max(0) as usize);
    }

    /// What typing `key` sends, in the mode the program has asked for.
    pub fn key(&mut self, key: &Key) -> Vec<u8> {
        let mut bytes = key_bytes(key);
        if self.screen().application_cursor() && bytes.len() == 3 && bytes.starts_with(b"\x1b[") {
            bytes[1] = b'O';
        }
        self.parser.screen_mut().set_scrollback(0);
        bytes
    }

    pub fn paste(&mut self, text: &str) -> Vec<u8> {
        self.parser.screen_mut().set_scrollback(0);
        match self.screen().bracketed_paste() {
            true => format!("\x1b[200~{text}\x1b[201~").into_bytes(),
            false => text.as_bytes().to_vec(),
        }
    }
}

/// Whether the host can be worked with.
#[derive(Debug, PartialEq)]
pub enum State {
    /// Not asked yet.
    Idle,
    Connecting,
    /// The SSH user's sudo password is needed first.
    Password,
    Ready(String),
    /// It has no berthd, or one older than this berth needs.
    Missing,
    Old(String),
    Unreachable(String),
}

/// Something to say yes to before it happens.
#[derive(Clone, Debug, PartialEq)]
pub enum Pending {
    /// Run the action at this index of [`HostView::actions`].
    Action(usize),
    Apply,
}

/// What the overview knows of the host, and the panel onto one of its sessions.
pub struct HostView {
    remote: Option<Box<dyn Transport>>,
    pub state: State,
    pub tasks: Vec<api::Task>,
    pub sessions: Vec<api::Session>,
    pub stats: Option<api::Stats>,
    /// What changed since the host was applied, when berth knows.
    pub behind: Option<api::Behind>,
    pub view: Option<View>,
    /// The SSH user's sudo password, kept for this run of the UI.
    pub password: Option<String>,
    pub question: Option<(String, Pending)>,
    pub arguments: Option<Arguments>,
    polled: Instant,
    /// The next `Opened` is a session started from here, and maybe a shell.
    starting: Option<bool>,
    /// The size new panels get.
    pub size: (u16, u16),
}

impl Default for HostView {
    fn default() -> Self {
        HostView {
            remote: None,
            state: State::Idle,
            tasks: vec![],
            sessions: vec![],
            stats: None,
            behind: None,
            view: None,
            password: None,
            question: None,
            arguments: None,
            polled: Instant::now(),
            starting: None,
            size: (24, 80),
        }
    }
}

impl HostView {
    pub fn connected(&self) -> bool {
        self.remote.is_some()
    }

    /// Start talking to the host through `remote`, forgetting what was known.
    pub fn connect(&mut self, remote: Box<dyn Transport>) {
        self.disconnect();
        remote.send(Request::Refresh);
        self.remote = Some(remote);
        self.state = State::Connecting;
        self.polled = Instant::now();
    }

    /// The host cannot be reached at all.
    pub fn unreachable(&mut self, why: String) {
        self.disconnect();
        self.state = State::Unreachable(why);
    }

    pub fn disconnect(&mut self) {
        self.remote = None;
        self.view = None;
        self.tasks.clear();
        self.sessions.clear();
        self.stats = None;
        self.behind = None;
        self.state = State::Idle;
    }

    pub fn send(&self, request: Request) {
        if let Some(remote) = &self.remote {
            remote.send(request);
        }
    }

    /// Run a task; its session opens in the panel when the host has started it.
    pub fn run(&mut self, name: &str, body: Value) {
        self.starting = Some(false);
        self.send(Request::Run {
            name: name.to_owned(),
            body,
        });
    }

    /// What can be done on the host: its tasks as actions, each task that
    /// is no step of another, with the steps plugins add to it.
    pub fn actions(&self) -> Vec<api::Action> {
        api::Action::of_tasks(&self.tasks)
    }

    /// Begin an action: its question first, then its arguments. Returns the
    /// action to run now when it needs neither.
    pub fn begin_task(&mut self, index: usize, confirmed: bool) -> Option<Launch> {
        match page::begin(&self.actions(), index, confirmed) {
            Begin::Ask(question) => self.question = Some((question, Pending::Action(index))),
            Begin::Arguments(dialog) => {
                self.arguments = Some(dialog);
                self.ask_choices();
            }
            Begin::Run(launch) => return Some(launch),
            Begin::Nothing => {}
        }
        None
    }

    fn ask_choices(&self) {
        if let Some((task, argument)) = self
            .arguments
            .as_ref()
            .and_then(|dialog| dialog.wants_choices(&self.actions()))
        {
            self.send(Request::Choices { task, argument });
        }
    }

    /// Take the value typed for the argument being asked and go on to the
    /// next. Returns the action to run after the last, or what is missing.
    pub fn accept_argument(&mut self) -> Result<Option<Launch>, String> {
        let Some(dialog) = self.arguments.take() else {
            return Ok(None);
        };
        match page::accept(&self.actions(), dialog) {
            Accepted::Next(next) => {
                self.arguments = Some(next);
                self.ask_choices();
                Ok(None)
            }
            Accepted::Run(launch) => Ok(Some(launch)),
            Accepted::Missing(dialog, said) => {
                self.arguments = Some(dialog);
                Err(said)
            }
        }
    }

    /// What starting a task sends: its arguments, and the panel's size for a terminal.
    pub fn task_body(&self, action: &api::Action, values: &BTreeMap<String, String>) -> Value {
        let mut body = json!({"arguments": values});
        if action.terminal {
            (body["rows"], body["cols"]) = (json!(self.size.0), json!(self.size.1));
        }
        body
    }

    /// The task an action of the host runs, with what the host keeps of it.
    pub fn task(&self, action: &api::Action) -> Option<&api::Task> {
        self.tasks.iter().find(|task| task.name == action.name)
    }

    /// Start a shell as `user`.
    pub fn shell(&mut self, user: &str, title: &str) {
        self.starting = Some(true);
        let (rows, cols) = self.size;
        let body = json!({
            "kind": "terminal", "command": ["bash", "-l"], "user": user,
            "title": title, "rows": rows, "cols": cols,
        });
        self.send(Request::Start { body });
    }

    /// Show a session of the list in the panel.
    pub fn open(&mut self, session: &api::Session) {
        let (rows, cols) = self.size;
        self.view = Some(View::new(session, rows, cols, false, false));
        self.send(Request::Open(Box::new(session.clone())));
        if session.kind == api::SessionKind::Terminal {
            self.send(Request::Resize(rows, cols));
        }
    }

    /// Leave the panel. The session goes on; `forget` deletes it.
    pub fn close(&mut self, forget: bool) {
        self.send(Request::Close);
        if let Some(view) = self.view.take()
            && forget
        {
            self.send(Request::Delete(view.id));
        } else {
            self.send(Request::Poll);
        }
    }

    /// Type into the session in the panel.
    pub fn input(&self, bytes: Vec<u8>) {
        if !bytes.is_empty() && self.view.as_ref().is_some_and(View::running) {
            self.send(Request::Input(bytes));
        }
    }

    /// Give the panel the size the window allows now.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.size = (rows, cols);
        if let Some(view) = &mut self.view
            && view.resize(rows, cols)
            && view.terminal
            && view.running()
        {
            self.send(Request::Resize(rows, cols));
        }
    }

    /// Take in what the host has said. Returns something for the status line,
    /// if there is something to say.
    pub fn pump(&mut self) -> Option<String> {
        let mut note = None;
        while let Some(event) = self.remote.as_ref().and_then(|remote| remote.next()) {
            match event {
                Event::Version(version) if version.api < NEEDS_API => {
                    self.state = State::Old(version.version);
                }
                Event::Version(version) => self.state = State::Ready(version.version),
                Event::Failed { message, missing } => {
                    // A wrong password is asked for again.
                    if self.password.is_some()
                        && (message.contains("incorrect password") || message.contains("try again"))
                    {
                        self.password = None;
                        self.state = State::Password;
                        note = Some("sudo on the host did not take that password".to_owned());
                    } else if missing {
                        self.state = State::Missing;
                    } else if message.contains("a password is required") {
                        self.state = State::Password;
                    } else {
                        self.state = State::Unreachable(message);
                    }
                }
                Event::Behind(behind) => self.behind = behind,
                Event::Tasks(tasks) => self.tasks = tasks,
                Event::Sessions(sessions) => self.sessions = sessions,
                Event::Stats(stats) => self.stats = Some(*stats),
                Event::Opened(session) => {
                    let shell = self.starting.take().unwrap_or(false);
                    let (rows, cols) = self.size;
                    self.view = Some(View::new(&session, rows, cols, true, shell));
                    if session.kind == api::SessionKind::Terminal {
                        self.send(Request::Open(session.clone()));
                        self.send(Request::Resize(rows, cols));
                    } else {
                        self.send(Request::Open(session));
                    }
                }
                Event::Output(id, bytes) => {
                    if let Some(view) = self.view.as_mut().filter(|view| view.id == id) {
                        let answer = view.feed(&bytes);
                        if !answer.is_empty() && view.running() {
                            self.send(Request::Input(answer));
                        }
                    }
                }
                Event::Exited(id, code) => {
                    if let Some(view) = self.view.as_mut().filter(|view| view.id == id) {
                        view.ended = Some(code);
                    }
                }
                Event::Closed(id) => {
                    if let Some(view) = self.view.as_mut().filter(|view| view.id == id)
                        && view.running()
                    {
                        view.ended = Some(None);
                        note =
                            Some("The connection to the session was lost; it may still run".into());
                    }
                }
                Event::Choices {
                    task,
                    argument,
                    choices,
                } => {
                    let actions = self.actions();
                    if let Some(dialog) = &mut self.arguments {
                        dialog.offered(&actions, &task, &argument, choices);
                    }
                }
                Event::Error(message) => {
                    self.starting = None;
                    note = Some(message);
                }
            }
        }
        if self.view.is_none()
            && matches!(self.state, State::Ready(_))
            && self.polled.elapsed() >= POLL
        {
            self.polled = Instant::now();
            self.send(Request::Poll);
        }
        note
    }

    /// One line about the host: load, memory, disk, uptime.
    pub fn stats_line(&self) -> Option<String> {
        let stats = self.stats.as_ref()?;
        let gb = |bytes: u64| bytes as f64 / (1u64 << 30) as f64;
        let mut parts = vec![
            format!("load {:.2}", stats.load[0]),
            format!("cpu {:.0}%", stats.cpu),
            format!(
                "memory {:.1}/{:.0} GB",
                gb(stats.memory.used),
                gb(stats.memory.total)
            ),
        ];
        if let Some(root) = stats
            .filesystems
            .iter()
            .find(|filesystem| filesystem.mount == "/")
            && root.total > 0
        {
            parts.push(format!(
                "/ {:.0}% of {:.0} GB",
                root.used as f64 * 100.0 / root.total as f64,
                gb(root.total)
            ));
        }
        for gpu in &stats.gpus {
            parts.push(format!("gpu {:.0}%", gpu.utilization));
        }
        parts.push(format!("up {}", uptime(stats.uptime)));
        if stats.reboot_required {
            parts.push("reboot required".into());
        }
        Some(parts.join(" · "))
    }
}

fn uptime(seconds: u64) -> String {
    match seconds {
        0..3600 => format!("{}m", seconds / 60),
        3600..86400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86400),
    }
}

/// How a session of the list is doing, in a word.
pub fn session_state(session: &api::Session) -> String {
    match (session.running(), session.lost, session.exit_code) {
        (true, ..) => "running".to_owned(),
        (_, true, _) => "lost".to_owned(),
        (_, _, Some(0)) => "done".to_owned(),
        (_, _, Some(code)) => format!("failed ({code})"),
        _ => "ended".to_owned(),
    }
}

/// Something the overview of a host offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pick {
    /// An action, by index into [`HostView::actions`].
    Task(usize),
    Terminal,
    /// A session, by index.
    Session(usize),
}

/// The groups of the overview of a host that answers, as it lists its tasks:
/// each in the group it names, with the terminal among the first and the
/// host's sessions after them.
pub fn groups(host: &HostView, terminal_user: &str) -> Vec<(String, Vec<(String, Pick)>)> {
    let mut groups: Vec<(String, Vec<(String, Pick)>)> = page::groups(&host.actions(), Pick::Task)
        .into_iter()
        .map(|group| (group.title, group.rows))
        .collect();
    let terminal = (format!("  ▸ Terminal · as {terminal_user}"), Pick::Terminal);
    match groups.iter_mut().find(|(title, _)| title == "System") {
        Some((_, rows)) => rows.insert(1.min(rows.len()), terminal),
        None => groups.insert(0, ("System".to_owned(), vec![terminal])),
    }
    if !host.sessions.is_empty() {
        let rows = host
            .sessions
            .iter()
            .enumerate()
            .map(|(index, session)| {
                (
                    format!("  ▸ {} · {}", session.title, session_state(session)),
                    Pick::Session(index),
                )
            })
            .collect();
        groups.push(("Sessions".to_owned(), rows));
    }
    groups
}

/// What the overview says of a host whose configuration is behind.
pub fn behind_note(host: &HostView) -> Option<String> {
    let behind = host.behind.as_ref()?;
    Some(format!(
        "Applied before these changed: {}. Applying the configuration again brings the host up to date; sign-ins and data stay.",
        behind.text()
    ))
}

/// A list that comes bare, or under a key of an object.
pub fn list<T: serde::de::DeserializeOwned>(value: Value, key: &str) -> Vec<T> {
    let items = match value {
        Value::Array(items) => items,
        Value::Object(mut object) => match object.remove(key) {
            Some(Value::Array(items)) => items,
            _ => vec![],
        },
        _ => vec![],
    };
    items
        .into_iter()
        .filter_map(|item| serde_json::from_value(item).ok())
        .collect()
}

/// The exit code an `exited` frame of a session carries, when the frame is one.
pub fn exited(frame: &Value) -> Option<Option<i32>> {
    let exited = frame.get("exited")?;
    let code = exited.get("exit_code").unwrap_or(exited);
    Some(code.as_i64().map(|code| code as i32))
}

/// What the information box says of an action of the host.
pub fn task_help(host: &HostView, index: usize) -> String {
    match host.actions().get(index) {
        Some(action) => page::help(action, &format!("berth run HOST {}", action.name)),
        None => String::new(),
    }
}

/// What the information box says of a session of the list.
pub fn session_help(host: &HostView, index: usize) -> String {
    let Some(session) = host.sessions.get(index) else {
        return String::new();
    };
    let state = match (&session.ended, session.exit_code) {
        (None, _) => "still running".to_owned(),
        (Some(ended), Some(code)) => format!("ended {ended} with exit code {code}"),
        (Some(ended), None) => format!("ended {ended}"),
    };
    format!(
        "`{}` as {}, started {}, {state}. It lives on the host and goes on while nobody watches.\n\nEnter opens it here. x forgets one that has ended, with its output. From the command line: `berth attach HOST {id}` or `berth output HOST {id}`.",
        session.command.join(" "),
        session.user,
        session.started,
        id = session.id,
    )
}
