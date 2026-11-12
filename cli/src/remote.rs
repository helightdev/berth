//! The host's own API, for the UI: one background thread owns the runtime and
//! the link, takes requests and sends back what happened. The UI never waits
//! for the network.

use berth_api::{
    self as api,
    client::{Error, Frame},
};
use berth_core::link::{Link, LinkSpec};
use futures_util::{SinkExt, StreamExt};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// The lowest `api` of a host this berth can work with.
pub const NEEDS_API: u32 = 1;
/// How often the overview asks for stats and sessions again.
const POLL: Duration = Duration::from_secs(10);
/// Lines of a process session's transcript kept for scrolling back.
const HISTORY: usize = 5000;

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
    /// The host could not be asked.
    Failed(Error),
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

/// A list that comes bare, or under a key of an object.
pub fn list<T: DeserializeOwned>(value: Value, key: &str) -> Vec<T> {
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

/// The exit code an `exited` frame carries, when the text is one.
pub fn exited(text: &str) -> Option<Option<i32>> {
    let frame: Value = serde_json::from_str(text).ok()?;
    let exited = frame.get("exited")?;
    let code = exited.get("exit_code").unwrap_or(exited);
    Some(code.as_i64().map(|code| code as i32))
}

enum Out {
    Input(Vec<u8>),
    Resize(u16, u16),
}

async fn refresh(link: Arc<Link>, events: mpsc::Sender<Event>, all: bool) {
    if all {
        match link.get::<api::Version>("/v1/version").await {
            Ok(version) => {
                let old = version.api < NEEDS_API;
                let _ = events.send(Event::Version(version));
                if old {
                    return;
                }
            }
            Err(error) => {
                let _ = events.send(Event::Failed(error));
                return;
            }
        }
        if let Ok(tasks) = link.get::<Value>("/v1/tasks").await {
            let _ = events.send(Event::Tasks(list(tasks, "tasks")));
        }
    }
    if let Ok(sessions) = link.get::<Value>("/v1/sessions").await {
        let _ = events.send(Event::Sessions(list(sessions, "sessions")));
    }
    if let Ok(stats) = link.get::<api::Stats>("/v1/stats").await {
        let _ = events.send(Event::Stats(Box::new(stats)));
    }
}

async fn follow(
    link: Arc<Link>,
    session: api::Session,
    events: mpsc::Sender<Event>,
    mut out: UnboundedReceiver<Out>,
) {
    let terminal = session.kind == api::SessionKind::Terminal;
    let id = session.id.clone();
    // A terminal replays its screen; a process is read from the start.
    let path = match terminal {
        true => format!("/v1/sessions/{id}"),
        false => format!("/v1/sessions/{id}?since=0"),
    };
    let mut stream = match link.stream(&path).await {
        Ok(stream) => stream,
        Err(error) => {
            // An ended session can still be read back.
            let output = link
                .get::<api::Output>(&format!("/v1/sessions/{id}/output"))
                .await;
            match output.ok().and_then(|output| api::unb64(&output.data_b64)) {
                Some(bytes) if !session.running() => {
                    let _ = events.send(Event::Output(id.clone(), bytes));
                    let _ = events.send(Event::Exited(id, session.exit_code));
                }
                _ => {
                    let _ = events.send(Event::Error(error.to_string()));
                    let _ = events.send(Event::Closed(id));
                }
            }
            return;
        }
    };
    loop {
        tokio::select! {
            frame = stream.next() => match frame {
                Some(Ok(Frame::Binary(bytes))) => {
                    let event = match (terminal, bytes.split_first()) {
                        (true, _) => Event::Output(id.clone(), bytes.to_vec()),
                        (false, Some((_, rest))) => Event::Output(id.clone(), rest.to_vec()),
                        (false, None) => continue,
                    };
                    if events.send(event).is_err() {
                        return;
                    }
                }
                Some(Ok(Frame::Text(text))) => {
                    if let Some(code) = exited(text.as_str()) {
                        let _ = events.send(Event::Exited(id.clone(), code));
                    }
                }
                Some(Ok(_)) => {}
                _ => {
                    let _ = events.send(Event::Closed(id));
                    return;
                }
            },
            message = out.recv() => {
                let frame = match message {
                    Some(Out::Input(bytes)) if terminal => Frame::Binary(bytes.into()),
                    Some(Out::Input(bytes)) => Frame::Binary([vec![0], bytes].concat().into()),
                    Some(Out::Resize(rows, cols)) => {
                        Frame::Text(json!({"resize": {"rows": rows, "cols": cols}}).to_string().into())
                    }
                    None => {
                        let _ = stream.close().await;
                        return;
                    }
                };
                if stream.send(frame).await.is_err() {
                    let _ = events.send(Event::Closed(id));
                    return;
                }
            }
        }
    }
}

async fn worker(
    link: Arc<Link>,
    mut requests: UnboundedReceiver<Request>,
    events: mpsc::Sender<Event>,
) {
    let mut stream: Option<UnboundedSender<Out>> = None;
    while let Some(request) = requests.recv().await {
        let (link, events) = (link.clone(), events.clone());
        match request {
            Request::Refresh => drop(tokio::spawn(refresh(link, events, true))),
            Request::Poll => drop(tokio::spawn(refresh(link, events, false))),
            Request::Run { name, body } => drop(tokio::spawn(async move {
                let path = format!("/v1/tasks/{}", api::client::encode(&name));
                let _ = events.send(match link.post::<api::Session>(&path, &body).await {
                    Ok(session) => Event::Opened(Box::new(session)),
                    Err(error) => Event::Error(error.to_string()),
                });
            })),
            Request::Start { body } => drop(tokio::spawn(async move {
                let _ = events.send(
                    match link.post::<api::Session>("/v1/sessions", &body).await {
                        Ok(session) => Event::Opened(Box::new(session)),
                        Err(error) => Event::Error(error.to_string()),
                    },
                );
            })),
            Request::Open(session) => {
                let (sender, receiver) = unbounded_channel();
                stream = Some(sender);
                tokio::spawn(follow(link, *session, events, receiver));
            }
            Request::Input(bytes) => {
                if let Some(stream) = &stream {
                    let _ = stream.send(Out::Input(bytes));
                }
            }
            Request::Resize(rows, cols) => {
                if let Some(stream) = &stream {
                    let _ = stream.send(Out::Resize(rows, cols));
                }
            }
            Request::Close => stream = None,
            Request::Delete(id) => drop(tokio::spawn(async move {
                let path = format!("/v1/sessions/{id}");
                if let Err(error) = link.call::<Value>("DELETE", &path, None).await {
                    let _ = events.send(Event::Error(error.to_string()));
                }
                refresh(link, events, false).await;
            })),
            Request::Choices { task, argument } => drop(tokio::spawn(async move {
                let path = format!(
                    "/v1/tasks/{}/choices/{}",
                    api::client::encode(&task),
                    api::client::encode(&argument)
                );
                let choices = match link.get::<Value>(&path).await {
                    Ok(value) => list(value, "choices"),
                    Err(_) => vec![],
                };
                let _ = events.send(Event::Choices {
                    task,
                    argument,
                    choices,
                });
            })),
        }
    }
}

/// The thread that talks to one host.
pub struct Remote {
    requests: UnboundedSender<Request>,
    events: mpsc::Receiver<Event>,
}

impl Remote {
    pub fn start(spec: LinkSpec) -> std::io::Result<Remote> {
        let runtime = berth_core::link::runtime()?;
        let (requests, receiver) = unbounded_channel();
        let (sender, events) = mpsc::channel();
        std::thread::spawn(move || {
            runtime.block_on(worker(Arc::new(Link::new(spec)), receiver, sender));
        });
        Ok(Remote { requests, events })
    }

    pub fn send(&self, request: Request) {
        let _ = self.requests.send(request);
    }
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
    fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
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
    pub fn key(&mut self, key: &crossterm::event::KeyEvent) -> Vec<u8> {
        let mut bytes = crate::session::key_bytes(key);
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
    /// Run the task at this index.
    Task(usize),
    Apply,
}

/// The dialog that asks for a task's arguments, one after another.
pub struct Arguments {
    pub task: usize,
    pub index: usize,
    pub values: std::collections::BTreeMap<String, String>,
    pub input: tui_input::Input,
    pub choices: Vec<String>,
    /// The choice last put into the input, for stepping through them.
    pub chosen: Option<usize>,
}

/// What the overview knows of the host, and the panel onto one of its sessions.
pub struct HostView {
    remote: Option<Remote>,
    pub state: State,
    pub tasks: Vec<api::Task>,
    pub sessions: Vec<api::Session>,
    pub stats: Option<api::Stats>,
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

    /// Start talking to the host over `spec`, forgetting what was known.
    pub fn connect(&mut self, spec: LinkSpec) -> Option<String> {
        self.disconnect();
        match Remote::start(spec) {
            Ok(remote) => {
                remote.send(Request::Refresh);
                self.remote = Some(remote);
                self.state = State::Connecting;
                self.polled = Instant::now();
                None
            }
            Err(error) => {
                self.state = State::Unreachable(error.to_string());
                Some(error.to_string())
            }
        }
    }

    pub fn disconnect(&mut self) {
        self.remote = None;
        self.view = None;
        self.tasks.clear();
        self.sessions.clear();
        self.stats = None;
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
        while let Some(event) = self
            .remote
            .as_ref()
            .and_then(|remote| remote.events.try_recv().ok())
        {
            match event {
                Event::Version(version) if version.api < NEEDS_API => {
                    self.state = State::Old(version.version);
                }
                Event::Version(version) => self.state = State::Ready(version.version),
                Event::Failed(error) => {
                    let message = error.to_string();
                    // A wrong password is asked for again.
                    if self.password.is_some()
                        && (message.contains("incorrect password") || message.contains("try again"))
                    {
                        self.password = None;
                        self.state = State::Password;
                        note = Some("sudo on the host did not take that password".to_owned());
                    } else if Link::missing(&error) {
                        self.state = State::Missing;
                    } else if message.contains("a password is required") {
                        self.state = State::Password;
                    } else {
                        self.state = State::Unreachable(message);
                    }
                }
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
                    if let Some(dialog) = &mut self.arguments
                        && let Some(known) = self.tasks.get(dialog.task)
                        && known.name == task
                        && known
                            .arguments
                            .get(dialog.index)
                            .is_some_and(|known| known.name == argument)
                    {
                        dialog.choices = choices;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_come_bare_or_under_a_key() {
        let bare: Vec<api::Task> = list(json!([{"name": "status"}]), "tasks");
        let keyed: Vec<api::Task> = list(
            json!({"tasks": [{"name": "status"}, {"no": "name"}]}),
            "tasks",
        );
        assert_eq!((bare.len(), keyed.len()), (1, 1));
        assert_eq!(exited(r#"{"exited":{"exit_code":3}}"#), Some(Some(3)));
        assert_eq!(exited(r#"{"exited":{}}"#), Some(None));
        assert_eq!(exited(r#"{"dropped":{"frames":1}}"#), None);
        assert_eq!(uptime(7200), "2h");
    }

    #[test]
    fn a_transcript_is_shown_line_by_line() {
        let session: api::Session = serde_json::from_value(json!({
            "id": "s1", "kind": "process", "title": "demo", "command": ["true"], "user": "agent",
            "started_by": {"level": "host", "uid": 0}, "started": "2026-11-12T00:00:00Z",
        }))
        .unwrap();
        let mut view = View::new(&session, 4, 20, true, false);
        assert!(view.feed(b"one\ntwo").is_empty());
        view.feed(b"!\nthree\n");
        assert_eq!(view.screen().contents(), "one\ntwo!\nthree");
        assert!(view.running() && view.outcome().is_none());
        view.ended = Some(Some(2));
        assert_eq!(view.outcome(), Some(false));
        // A terminal answers a question about the cursor, also one in two pieces.
        let mut terminal = View::new(&session, 4, 20, true, false);
        terminal.terminal = true;
        assert!(terminal.feed(b"ab\x1b[6").is_empty());
        assert_eq!(terminal.feed(b"n"), b"\x1b[1;3R");
    }

    /// The overview against a real controller, when one has been built
    /// (`cargo build -p berthd`): connect, list, run tasks, type into one.
    #[test]
    fn the_overview_talks_to_a_controller() {
        let built = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("berthd");
        if !built.is_file() {
            eprintln!("skipped: {} is not built", built.display());
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("etc")).unwrap();
        let tasks = json!([
            {"name": "status", "label": "Status", "group": "System", "user": "agent", "run": "echo up; echo warn >&2"},
            {"name": "demo:status", "label": "Demo", "extends": ["status"], "user": "agent", "run": "echo demo; exit 4"},
            {"name": "greet", "label": "Greet", "user": "agent", "run": "echo \"hello $BERTH_ARG_WHO\"",
             "arguments": [{"name": "who", "required": true, "choices": "printf 'ann\\nbob\\n'"}]},
            {"name": "ask", "label": "Ask", "user": "agent", "terminal": true, "run": "printf 'code? '; read c; echo got $c"},
        ]);
        std::fs::write(dir.path().join("etc/tasks.json"), tasks.to_string()).unwrap();
        let mut daemon = std::process::Command::new(&built)
            .arg("--single")
            .arg(dir.path())
            .spawn()
            .unwrap();
        struct Stop<'a>(&'a mut std::process::Child);
        impl Drop for Stop<'_> {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let _stop = Stop(&mut daemon);
        for _ in 0..100 {
            if dir.path().join("control.sock").exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let spec = LinkSpec {
            program: built.to_string_lossy().into_owned(),
            args: vec![
                "connect".into(),
                "--dir".into(),
                dir.path().to_string_lossy().into_owned(),
            ],
            first_line: None,
            via: None,
        };
        let mut host = HostView::default();
        assert!(host.connect(spec).is_none());
        fn until(host: &mut HostView, what: &str, done: impl Fn(&HostView) -> bool) {
            for _ in 0..400 {
                if let Some(note) = host.pump() {
                    eprintln!("note: {note}");
                }
                if done(host) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            panic!("never: {what}; state {:?}", host.state);
        }
        until(&mut host, "ready with tasks", |host| {
            matches!(host.state, State::Ready(_)) && host.tasks.len() == 4
        });
        until(&mut host, "stats", |host| host.stats.is_some());
        assert!(host.stats_line().unwrap().contains("load "));
        assert!(host.tasks[0].run.is_empty() && host.tasks[1].extends == ["status"]);

        // A task and the step a plugin adds, as one transcript; the first
        // failing step is the outcome.
        host.run("status", json!({}));
        until(&mut host, "status ended", |host| {
            host.view.as_ref().is_some_and(|view| !view.running())
        });
        let view = host.view.as_ref().unwrap();
        assert!(view.ours && !view.terminal && view.title == "Status");
        let text = view.screen().contents();
        assert!(
            text.contains("up") && text.contains("warn") && text.contains("demo"),
            "{text}"
        );
        assert_eq!(view.ended, Some(Some(4)));
        let id = view.id.clone();
        // Closing a finished one that was started here forgets it.
        host.close(true);
        until(&mut host, "forgotten", |host| {
            host.sessions.iter().all(|session| session.id != id)
        });

        // Arguments reach the command; the host offers choices for one.
        host.send(Request::Choices {
            task: "greet".into(),
            argument: "who".into(),
        });
        host.arguments = Some(Arguments {
            task: 2,
            index: 0,
            values: Default::default(),
            input: Default::default(),
            choices: vec![],
            chosen: None,
        });
        until(&mut host, "choices", |host| {
            host.arguments.as_ref().unwrap().choices == ["ann", "bob"]
        });
        host.arguments = None;
        host.run("greet", json!({"arguments": {"who": "a 'quoted' b"}}));
        until(&mut host, "greet ended", |host| {
            host.view.as_ref().is_some_and(|view| !view.running())
        });
        assert!(
            host.view
                .as_ref()
                .unwrap()
                .screen()
                .contents()
                .contains("hello a 'quoted' b")
        );
        host.close(false);
        until(&mut host, "kept", |host| {
            host.sessions.iter().any(|session| session.title == "Greet")
        });

        // A terminal task takes typing; left with Ctrl+Q it runs on and can be
        // opened again.
        host.run("ask", json!({"rows": 10, "cols": 40}));
        until(&mut host, "the question", |host| {
            host.view
                .as_ref()
                .is_some_and(|view| view.screen().contents().contains("code?"))
        });
        assert!(host.view.as_ref().unwrap().terminal);
        host.close(false);
        until(&mut host, "listed as running", |host| {
            host.sessions
                .iter()
                .any(|session| session.title == "Ask" && session.running())
        });
        let session = host
            .sessions
            .iter()
            .find(|session| session.title == "Ask")
            .unwrap()
            .clone();
        host.open(&session);
        until(&mut host, "the question again", |host| {
            host.view
                .as_ref()
                .is_some_and(|view| view.screen().contents().contains("code?"))
        });
        host.input(b"42\r".to_vec());
        until(&mut host, "the answer", |host| {
            host.view.as_ref().is_some_and(|view| !view.running())
        });
        let view = host.view.as_ref().unwrap();
        assert!(view.screen().contents().contains("got 42") && view.outcome() == Some(true));
        assert!(!view.ours);

        // A task the host does not have says so.
        host.close(true);
        host.run("nothing", json!({}));
        let mut said = None;
        for _ in 0..200 {
            said = host.pump();
            if said.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(said.is_some() && host.view.is_none());
    }

    #[test]
    fn a_host_without_a_controller_is_told_apart() {
        let spec = |script: &str| LinkSpec {
            program: "sh".into(),
            args: vec!["-c".into(), script.into()],
            first_line: None,
            via: None,
        };
        let state = |script: &str, password: Option<&str>| {
            let mut host = HostView {
                password: password.map(str::to_owned),
                ..Default::default()
            };
            host.connect(spec(script));
            for _ in 0..400 {
                host.pump();
                if host.state != State::Connecting {
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            (host.state, host.password)
        };
        assert_eq!(
            state("echo 'sudo: berthd: command not found' >&2; exit 1", None).0,
            State::Missing
        );
        assert_eq!(
            state("echo 'sudo: a password is required' >&2; exit 1", None).0,
            State::Password
        );
        // A password sudo did not take is forgotten, to be asked for again.
        let wrong = state(
            "echo 'Sorry, try again.' >&2; echo 'sudo: 1 incorrect password attempt' >&2; exit 1",
            Some("x"),
        );
        assert_eq!(wrong, (State::Password, None));
        let (other, _) = state(
            "echo 'ssh: connect to host box port 22: Connection timed out' >&2; exit 255",
            None,
        );
        assert!(matches!(other, State::Unreachable(message) if message.contains("timed out")));
    }
}
