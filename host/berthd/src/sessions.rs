//! Sessions: commands that berthd runs and holds, so they exist while nobody
//! is connected. A process has its three streams apart; a terminal runs on a
//! pseudo-terminal whose screen is kept here.

use crate::error::{E, R};
use crate::state::{Caller, State};
use crate::transcript::{CAP, MERGED, Transcript};
use berth_api::{self as api, Level, SessionKind, code};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Write},
    os::fd::{FromRawFd, OwnedFd},
    os::unix::process::{CommandExt, ExitStatusExt},
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU32, AtomicU64, Ordering},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{broadcast, watch};

/// Sessions that may run at once.
pub const RUNNING: usize = 64;
/// Lines a terminal keeps above its screen.
const SCROLLBACK: usize = 2000;
/// Ended sessions are forgotten after a week.
const KEPT: u64 = 7 * 86400;
/// An ended session's output leaves memory after this long without a reader.
const UNLOAD: u64 = 600;

#[derive(Clone)]
pub enum Event {
    Output {
        stream: u8,
        offset: u64,
        bytes: Arc<Vec<u8>>,
    },
    Exited(Option<i32>),
}

struct Log {
    /// None while an ended session's output is only on disk.
    transcript: Option<Transcript>,
    parser: Option<vt100::Parser>,
    file: Option<File>,
    file_size: usize,
    /// For a question about the cursor that arrives in two pieces.
    tail: Vec<u8>,
}

#[derive(Default)]
struct Control {
    /// A process's standard input.
    input: Option<File>,
    /// Our end of a terminal.
    terminal: Option<File>,
    /// The process group of the step that runs now.
    group: i32,
    /// A signal that came before the command had started.
    pending: Option<i32>,
    cancelled: bool,
}

pub struct Session {
    pub id: String,
    meta: Mutex<api::Session>,
    log: Mutex<Log>,
    control: Mutex<Control>,
    pub events: broadcast::Sender<Event>,
    pub done: watch::Sender<bool>,
    viewers: AtomicU32,
    last_output: AtomicU64,
    last_read: AtomicU64,
    dir: PathBuf,
}

/// What to start.
pub struct Plan {
    pub id: String,
    pub kind: SessionKind,
    pub title: String,
    pub command: Vec<String>,
    pub user: String,
    pub task: Option<String>,
    /// The commands to run one after another, with the step each is. A plain
    /// session has one command and no step.
    pub steps: Vec<(Option<api::Step>, Command)>,
    pub input: Option<Vec<u8>>,
    /// End a process's input after `input`.
    pub close_input: bool,
    pub timeout: Option<u64>,
    pub rows: u16,
    pub cols: u16,
}

pub struct Sessions {
    dir: PathBuf,
    map: Mutex<BTreeMap<String, Arc<Session>>>,
}

/// `s` and twelve lowercase base32 characters.
pub fn new_id() -> String {
    let mut bytes = [0u8; 8];
    if File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .is_err()
    {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        bytes = (nanos as u64 ^ (std::process::id() as u64) << 32).to_le_bytes();
    }
    let mut number = u64::from_le_bytes(bytes);
    let mut id = String::from("s");
    for _ in 0..12 {
        id.push(b"abcdefghijklmnopqrstuvwxyz234567"[(number & 31) as usize] as char);
        number >>= 5;
    }
    id
}

fn pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut ends = [0; 2];
    // SAFETY: pipe2 fills the two descriptors, which are owned from here on.
    unsafe {
        if libc::pipe2(ends.as_mut_ptr(), libc::O_CLOEXEC) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((OwnedFd::from_raw_fd(ends[0]), OwnedFd::from_raw_fd(ends[1])))
    }
}

/// Give output to the terminal emulation. A fault in it on some odd sequence
/// costs that session its screen, not berthd its sessions.
fn feed(parser: &mut vt100::Parser, bytes: &[u8]) {
    let fed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| parser.process(bytes)));
    if fed.is_err() {
        let (rows, cols) = parser.screen().size();
        *parser = vt100::Parser::new(rows.max(2), cols.max(2), SCROLLBACK);
    }
}

fn signal_number(name: &str) -> Option<i32> {
    Some(match name.trim_start_matches("SIG") {
        "INT" => libc::SIGINT,
        "TERM" => libc::SIGTERM,
        "HUP" => libc::SIGHUP,
        "KILL" => libc::SIGKILL,
        _ => return None,
    })
}

impl Sessions {
    /// Take up what an earlier berthd left: its sessions are over, their
    /// transcripts stay.
    pub fn open(dir: &Path) -> std::io::Result<Sessions> {
        std::fs::create_dir_all(dir)?;
        let mut map = BTreeMap::new();
        for entry in std::fs::read_dir(dir)?.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|extension| extension != "json") {
                continue;
            }
            let Ok(mut meta) = std::fs::read(&path)
                .map_err(drop)
                .and_then(|data| serde_json::from_slice::<api::Session>(&data).map_err(drop))
            else {
                continue;
            };
            if meta.ended.is_none() {
                meta.ended = Some(api::now());
                meta.lost = true;
            }
            let session = Session::build(dir, meta, None, None);
            session.save();
            map.insert(session.id.clone(), session);
        }
        Ok(Sessions {
            dir: dir.to_owned(),
            map: Mutex::new(map),
        })
    }

    pub fn get(&self, id: &str) -> R<Arc<Session>> {
        self.map
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| E::missing(format!("session {id}")))
    }

    pub fn list(&self) -> Vec<api::Session> {
        let mut sessions: Vec<_> = self
            .map
            .lock()
            .unwrap()
            .values()
            .map(|session| session.state())
            .collect();
        sessions.sort_by(|a, b| a.started.cmp(&b.started));
        sessions
    }

    pub fn running(&self) -> usize {
        self.map
            .lock()
            .unwrap()
            .values()
            .filter(|session| session.running())
            .count()
    }

    /// Forget old sessions and let go of output nobody reads. Called now and then.
    pub fn tidy(&self) {
        let now = api::unix_now();
        let sessions: Vec<_> = self.map.lock().unwrap().values().cloned().collect();
        for session in sessions {
            if session.running() {
                continue;
            }
            let quiet = now.saturating_sub(session.last_read.load(Ordering::Relaxed));
            if session
                .ended_at()
                .is_some_and(|ended| now.saturating_sub(ended) > KEPT)
            {
                self.forget(&session);
            } else if quiet > UNLOAD && session.viewers.load(Ordering::Relaxed) == 0 {
                let mut log = session.log.lock().unwrap();
                log.transcript = None;
                log.parser = None;
            }
        }
    }

    fn forget(&self, session: &Session) {
        self.map.lock().unwrap().remove(&session.id);
        let _ = std::fs::remove_file(session.dir.join(format!("{}.json", session.id)));
        let _ = std::fs::remove_file(session.dir.join(format!("{}.log", session.id)));
    }

    /// End it if it runs, and forget it with its transcript.
    pub async fn delete(&self, session: &Arc<Session>) {
        if session.running() {
            session.control.lock().unwrap().cancelled = true;
            session.signal(libc::SIGHUP);
            let mut done = session.done.subscribe();
            let wait =
                async { while !*done.borrow_and_update() && done.changed().await.is_ok() {} };
            if tokio::time::timeout(Duration::from_secs(5), wait)
                .await
                .is_err()
            {
                session.signal(libc::SIGKILL);
                let mut done = session.done.subscribe();
                let wait =
                    async { while !*done.borrow_and_update() && done.changed().await.is_ok() {} };
                let _ = tokio::time::timeout(Duration::from_secs(2), wait).await;
            }
        }
        self.forget(session);
    }

    /// Start a session. The commands of `plan` have their user and environment
    /// already; this gives them their streams and runs them in order.
    pub fn start(&self, state: &Arc<State>, caller: &Caller, plan: Plan) -> R<Arc<Session>> {
        if self.running() >= RUNNING {
            return Err(E(code::LIMIT, format!("{RUNNING} sessions run already")));
        }
        let terminal = plan.kind == SessionKind::Terminal;
        let meta = api::Session {
            id: plan.id.clone(),
            kind: plan.kind,
            title: plan.title.chars().take(200).collect(),
            command: plan.command,
            user: plan.user,
            started_by: api::From {
                level: caller.level,
                uid: caller.uid,
                session: None,
            },
            task: plan.task,
            started: api::now(),
            ended: None,
            exit_code: None,
            lost: false,
            steps: plan
                .steps
                .iter()
                .filter_map(|(step, _)| step.clone())
                .collect(),
            last_output: None,
            viewers: 0,
            output_size: 0,
            rows: terminal.then_some(plan.rows),
            cols: terminal.then_some(plan.cols),
        };
        let parser = terminal.then(|| vt100::Parser::new(plan.rows, plan.cols, SCROLLBACK));
        let session = Session::build(&self.dir, meta, Some(Transcript::new(CAP)), parser);
        {
            let mut log = session.log.lock().unwrap();
            let mut options = std::fs::OpenOptions::new();
            options.create(true).append(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            log.file = Some(options.open(session.dir.join(format!("{}.log", session.id)))?);
        }
        session.save();

        // The far ends the commands get, kept until the last step has ended.
        let mut far: Vec<OwnedFd> = vec![];
        let (finished, readers) = std::sync::mpsc::channel::<()>();
        let reader = |session: Arc<Session>, stream: u8, mut source: File| {
            let finished = finished.clone();
            std::thread::spawn(move || {
                let mut buffer = [0; 8192];
                while let Ok(count) = source.read(&mut buffer)
                    && count > 0
                {
                    session.append(stream, &buffer[..count]);
                }
                drop(finished);
            });
        };
        if terminal {
            let (ours, theirs) = berth_pty::open(plan.rows, plan.cols)?;
            reader(session.clone(), 1, ours.try_clone()?);
            session.control.lock().unwrap().terminal = Some(ours);
            far.push(theirs);
        } else {
            let (input_far, input) = pipe()?;
            let (output, output_far) = pipe()?;
            let (errors, errors_far) = pipe()?;
            reader(session.clone(), 1, File::from(output));
            reader(session.clone(), 2, File::from(errors));
            session.control.lock().unwrap().input = Some(File::from(input));
            far.extend([input_far, output_far, errors_far]);
        }
        drop(finished);

        let mut steps = vec![];
        for (index, (step, mut command)) in plan.steps.into_iter().enumerate() {
            command.env("BERTH_SESSION", &session.id);
            if terminal {
                berth_pty::attach(&mut command, &far[0])?;
            } else {
                command
                    .stdin(far[0].try_clone()?)
                    .stdout(far[1].try_clone()?)
                    .stderr(far[2].try_clone()?);
                // A group of its own, so a signal reaches everything it started.
                command.process_group(0);
            }
            steps.push((step.map(|_| index), command));
        }

        self.map
            .lock()
            .unwrap()
            .insert(session.id.clone(), session.clone());
        state.channel.internal(
            "berth/session/started",
            serde_json::json!({"id": session.id, "title": session.state().title}),
        );
        if let Some(input) = plan.input {
            session.write(input);
        }
        if plan.close_input && !terminal {
            session.close_input();
        }
        if let Some(seconds) = plan.timeout {
            let session = session.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(seconds)).await;
                if session.running() {
                    session.control.lock().unwrap().cancelled = true;
                    session.signal(libc::SIGTERM);
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    session.signal(libc::SIGKILL);
                }
            });
        }
        let (runner, state) = (session.clone(), state.clone());
        std::thread::spawn(move || runner.run(&state, steps, far, readers));
        Ok(session)
    }
}

impl Session {
    fn build(
        dir: &Path,
        meta: api::Session,
        transcript: Option<Transcript>,
        parser: Option<vt100::Parser>,
    ) -> Arc<Session> {
        let ended = meta.ended.is_some();
        Arc::new(Session {
            id: meta.id.clone(),
            meta: Mutex::new(meta),
            log: Mutex::new(Log {
                transcript,
                parser,
                file: None,
                file_size: 0,
                tail: vec![],
            }),
            control: Mutex::new(Control::default()),
            events: broadcast::channel(256).0,
            done: watch::channel(ended).0,
            viewers: AtomicU32::new(0),
            last_output: AtomicU64::new(0),
            last_read: AtomicU64::new(api::unix_now()),
            dir: dir.to_owned(),
        })
    }

    fn save(&self) {
        let meta = self.meta.lock().unwrap().clone();
        let path = self.dir.join(format!("{}.json", self.id));
        if let Ok(data) = serde_json::to_vec(&meta) {
            let _ = std::fs::write(path, data);
        }
    }

    pub fn running(&self) -> bool {
        self.meta.lock().unwrap().ended.is_none()
    }

    /// berthd is stopping: what still runs did not end by itself.
    pub fn lose(&self) {
        self.meta.lock().unwrap().lost = true;
        self.control.lock().unwrap().cancelled = true;
        self.signal(libc::SIGHUP);
    }

    fn ended_at(&self) -> Option<u64> {
        // Good enough for tidying: when the state file was last written.
        let path = self.dir.join(format!("{}.json", self.id));
        let modified = std::fs::metadata(path).ok()?.modified().ok()?;
        Some(
            modified
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_secs(),
        )
    }

    pub fn kind(&self) -> SessionKind {
        self.meta.lock().unwrap().kind
    }

    /// The agent may look at the sessions it started through a task.
    pub fn visible_to(&self, caller: &Caller) -> bool {
        let meta = self.meta.lock().unwrap();
        caller.host()
            || (meta.started_by.level == Level::Agent && meta.started_by.uid == caller.uid)
    }

    pub fn state(&self) -> api::Session {
        let mut meta = self.meta.lock().unwrap().clone();
        meta.viewers = self.viewers.load(Ordering::Relaxed);
        let last = self.last_output.load(Ordering::Relaxed);
        meta.last_output = (last > 0).then(|| api::rfc3339(last));
        meta.output_size = match &self.log.lock().unwrap().transcript {
            Some(transcript) => transcript.written(MERGED),
            None => meta.output_size,
        };
        meta
    }

    /// Take in what the command wrote.
    fn append(&self, stream: u8, bytes: &[u8]) {
        let mut log = self.log.lock().unwrap();
        let log = &mut *log;
        let Some(transcript) = &mut log.transcript else {
            return;
        };
        let offset = transcript.append(stream, bytes);
        if let Some(file) = &mut log.file {
            let record = Transcript::record(stream, bytes);
            log.file_size += record.len();
            let _ = file.write_all(&record);
            // The file only grows; start it over from what is kept when it has doubled.
            if log.file_size > 2 * CAP + (1 << 20) {
                let stored = transcript.store();
                if std::fs::write(self.dir.join(format!("{}.log", self.id)), &stored).is_ok() {
                    log.file_size = stored.len();
                }
            }
        }
        if let Some(parser) = &mut log.parser {
            feed(parser, bytes);
            // A program that asks where the cursor is waits for the answer.
            // With a viewer attached, its terminal answers; without one we do.
            log.tail.extend_from_slice(bytes);
            let asked = log
                .tail
                .windows(4)
                .filter(|window| window == b"\x1b[6n")
                .count();
            let keep = log.tail.len().saturating_sub(3);
            log.tail.drain(..keep);
            if asked > 0 && self.viewers.load(Ordering::Relaxed) == 0 {
                let (row, col) = parser.screen().cursor_position();
                let answer = format!("\x1b[{};{}R", row + 1, col + 1).repeat(asked);
                if let Some(terminal) = &mut self.control.lock().unwrap().terminal {
                    let _ = terminal.write_all(answer.as_bytes());
                }
            }
        }
        self.last_output.store(api::unix_now(), Ordering::Relaxed);
        let _ = self.events.send(Event::Output {
            stream,
            offset,
            bytes: Arc::new(bytes.to_vec()),
        });
    }

    /// Run the steps in order, each to its end, then close the session.
    fn run(
        self: Arc<Session>,
        state: &Arc<State>,
        steps: Vec<(Option<usize>, Command)>,
        far: Vec<OwnedFd>,
        readers: std::sync::mpsc::Receiver<()>,
    ) {
        let mut first_failure = None;
        for (step, mut command) in steps {
            if self.control.lock().unwrap().cancelled {
                break;
            }
            let code = match command.spawn() {
                Ok(mut child) => {
                    // The command holds its ends of the streams now.
                    drop(command);
                    let early = {
                        let mut control = self.control.lock().unwrap();
                        control.group = child.id() as i32;
                        control.pending.take()
                    };
                    if let Some(signal) = early {
                        self.signal(signal);
                    }
                    let status = child.wait();
                    self.control.lock().unwrap().group = 0;
                    match status {
                        Ok(status) => status
                            .code()
                            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)),
                        Err(_) => 1,
                    }
                }
                Err(error) => {
                    let program = command.get_program().to_string_lossy().into_owned();
                    self.append(
                        2,
                        format!("berthd: could not start {program}: {error}\n").as_bytes(),
                    );
                    127
                }
            };
            if let Some(index) = step {
                self.meta.lock().unwrap().steps[index].exit_code = Some(code);
                self.save();
            }
            if code != 0 && first_failure.is_none() {
                first_failure = Some(code);
            }
        }
        // Closing our copies of the far ends lets the readers see the end. A
        // program left behind in the background can hold them open; it gets a
        // moment, then the session is over without it.
        drop(far);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
            if readers.recv_timeout(left).is_err() {
                break;
            }
        }
        let code = first_failure.unwrap_or(0);
        {
            let mut control = self.control.lock().unwrap();
            control.input = None;
            control.terminal = None;
        }
        {
            let mut meta = self.meta.lock().unwrap();
            meta.ended = Some(api::now());
            meta.exit_code = Some(code);
            meta.output_size = self
                .log
                .lock()
                .unwrap()
                .transcript
                .as_ref()
                .map_or(0, |log| log.written(MERGED));
        }
        self.save();
        self.log.lock().unwrap().file = None;
        self.last_read.store(api::unix_now(), Ordering::Relaxed);
        let _ = self.done.send(true);
        let _ = self.events.send(Event::Exited(Some(code)));
        state.channel.internal(
            "berth/session/exited",
            serde_json::json!({"id": self.id, "exit_code": code}),
        );
    }

    /// Write to standard input, or type into the terminal. Never recorded.
    pub fn write(&self, bytes: Vec<u8>) {
        let control = self.control.lock().unwrap();
        let target = control
            .terminal
            .as_ref()
            .or(control.input.as_ref())
            .and_then(|file| file.try_clone().ok());
        drop(control);
        if let Some(mut target) = target {
            // A command that does not read must not hold up anyone else.
            std::thread::spawn(move || {
                let _ = target.write_all(&bytes);
            });
        }
    }

    pub fn close_input(&self) {
        self.control.lock().unwrap().input = None;
    }

    pub fn resize(&self, rows: u16, cols: u16) -> R<()> {
        if rows < 2 || cols < 2 || rows > 1000 || cols > 1000 {
            return Err(E::bad("rows and cols are between 2 and 1000"));
        }
        let control = self.control.lock().unwrap();
        let Some(terminal) = &control.terminal else {
            return Err(E::bad("Only a running terminal has a size"));
        };
        berth_pty::resize(terminal, rows, cols);
        drop(control);
        if let Some(parser) = &mut self.log.lock().unwrap().parser {
            parser.screen_mut().set_size(rows, cols);
        }
        let mut meta = self.meta.lock().unwrap();
        (meta.rows, meta.cols) = (Some(rows), Some(cols));
        Ok(())
    }

    /// Send a signal to everything the running step started.
    pub fn signal(&self, signal: i32) {
        let group = {
            let mut control = self.control.lock().unwrap();
            if control.group <= 1 {
                control.pending = Some(signal);
            }
            control.group
        };
        if group > 1 {
            // SAFETY: a plain system call; a group that is gone is not an error.
            unsafe {
                libc::kill(-group, signal);
            }
        }
    }

    /// Run `read` on the transcript, loading it from disk when it had been let go.
    fn with_log<T>(&self, read: impl FnOnce(&Transcript, &mut Option<vt100::Parser>) -> T) -> T {
        self.last_read.store(api::unix_now(), Ordering::Relaxed);
        let mut log = self.log.lock().unwrap();
        if log.transcript.is_none() {
            let data = std::fs::read(self.dir.join(format!("{}.log", self.id))).unwrap_or_default();
            log.transcript = Some(Transcript::load(&data, CAP));
        }
        let log = &mut *log;
        read(log.transcript.as_ref().expect("loaded"), &mut log.parser)
    }

    /// The terminal's screen, built again from the transcript when needed.
    fn with_screen<T>(&self, read: impl FnOnce(&mut vt100::Parser) -> T) -> R<T> {
        let (rows, cols) = {
            let meta = self.meta.lock().unwrap();
            if meta.kind != SessionKind::Terminal {
                return Err(E::bad(
                    "Only a terminal has a screen; read a process with /output",
                ));
            }
            (meta.rows.unwrap_or(24), meta.cols.unwrap_or(80))
        };
        Ok(self.with_log(|transcript, parser| {
            let parser = parser.get_or_insert_with(|| {
                let mut parser = vt100::Parser::new(rows, cols, SCROLLBACK);
                feed(&mut parser, &transcript.read(1, 0, usize::MAX).1);
                parser
            });
            read(parser)
        }))
    }
}

// ── The calls ───────────────────────────────────────────────────────────────

use axum::{
    Json,
    extract::{Extension, Path as UrlPath, Query, State as AppState, ws},
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};

#[derive(serde::Deserialize, Default)]
pub struct Wait {
    wait: Option<String>,
}

/// With `wait=`, hold the answer until the command has ended or the time is
/// up, and include what it wrote.
pub async fn started(session: &Arc<Session>, wait: &Wait) -> R<api::Started> {
    let mut answer = api::Started {
        session: session.state(),
        stdout_b64: None,
        stderr_b64: None,
        output_b64: None,
        truncated: false,
    };
    let Some(wait) = &wait.wait else {
        return Ok(answer);
    };
    let seconds = api::duration(wait)
        .filter(|seconds| *seconds <= 600)
        .ok_or_else(|| E::bad("wait is a duration of at most 10m, such as 30s"))?;
    let mut done = session.done.subscribe();
    let ended = async { while !*done.borrow_and_update() && done.changed().await.is_ok() {} };
    if tokio::time::timeout(Duration::from_secs(seconds), ended)
        .await
        .is_ok()
    {
        const INLINE: usize = 1 << 20;
        let read = |selection| {
            session.with_log(|log, _| {
                let (_, data) = log.read(selection, 0, INLINE);
                (
                    api::b64(&data),
                    log.written(selection) - log.first(selection) > data.len() as u64
                        || log.first(selection) > 0,
                )
            })
        };
        if session.kind() == SessionKind::Terminal {
            let (data, more) = read(1);
            (answer.output_b64, answer.truncated) = (Some(data), more);
        } else {
            let ((out, more_out), (err, more_err)) = (read(1), read(2));
            (answer.stdout_b64, answer.stderr_b64, answer.truncated) =
                (Some(out), Some(err), more_out || more_err);
        }
    }
    answer.session = session.state();
    Ok(answer)
}

pub async fn start(
    AppState(state): AppState<Arc<State>>,
    Extension(caller): Extension<Caller>,
    Query(wait): Query<Wait>,
    Json(request): Json<api::StartSession>,
) -> R<Json<api::Started>> {
    let command = crate::users::command(
        &state,
        &request.user,
        &request.command,
        request.cwd.as_deref(),
        &request.env,
    )?;
    let words = match &request.command {
        api::CommandLine::Args(args) => args.clone(),
        api::CommandLine::Shell(script) => vec![script.clone()],
    };
    let input = match (&request.input_b64, &request.input) {
        (Some(data), _) => Some(api::unb64(data).ok_or_else(|| E::bad("input_b64 is not base64"))?),
        (None, Some(text)) => Some(text.clone().into_bytes()),
        (None, None) => None,
    };
    let plan = Plan {
        id: new_id(),
        kind: request.kind,
        title: request.title.clone().unwrap_or_else(|| words.join(" ")),
        command: words,
        user: request.user.clone(),
        task: None,
        steps: vec![(None, command)],
        // Nobody can type into a command that is waited for in one request.
        close_input: input.is_some() || wait.wait.is_some(),
        input,
        timeout: request.timeout,
        rows: request.rows.unwrap_or(24).clamp(2, 1000),
        cols: request.cols.unwrap_or(80).clamp(2, 1000),
    };
    let session = state.sessions.start(&state, &caller, plan)?;
    Ok(Json(started(&session, &wait).await?))
}

pub async fn list(AppState(state): AppState<Arc<State>>) -> Json<Vec<api::Session>> {
    Json(state.sessions.list())
}

fn visible(state: &State, caller: &Caller, id: &str) -> R<Arc<Session>> {
    let session = state.sessions.get(id)?;
    if !session.visible_to(caller) {
        return Err(E::forbidden(
            "The agent sees only the sessions it started through a task",
        ));
    }
    Ok(session)
}

/// One session's state, or with a WebSocket handshake the session live.
pub async fn one(
    AppState(state): AppState<Arc<State>>,
    Extension(caller): Extension<Caller>,
    UrlPath(id): UrlPath<String>,
    Query(query): Query<BTreeMap<String, String>>,
    upgrade: Result<ws::WebSocketUpgrade, ws::rejection::WebSocketUpgradeRejection>,
) -> R<Response> {
    let session = visible(&state, &caller, &id)?;
    let Ok(upgrade) = upgrade else {
        return Ok(Json(session.state()).into_response());
    };
    let since = match query.get("since") {
        Some(since) => Some(
            since
                .parse::<u64>()
                .map_err(|_| E::bad("since is a byte offset"))?,
        ),
        None => None,
    };
    Ok(upgrade.on_upgrade(move |socket| live(session, socket, since)))
}

/// The frame a viewer gets for a piece of output.
fn frame(terminal: bool, stream: u8, bytes: &[u8]) -> ws::Message {
    match terminal {
        true => ws::Message::Binary(bytes.to_vec().into()),
        false => ws::Message::Binary([&[stream], bytes].concat().into()),
    }
}

async fn live(session: Arc<Session>, socket: ws::WebSocket, since: Option<u64>) {
    let terminal = session.kind() == SessionKind::Terminal;
    let (mut sink, mut source) = socket.split();
    // What was written so far and the subscription are taken under one lock,
    // so nothing is missed and nothing comes twice.
    let (replay, mut next, mut events) = session.with_log(|log, parser| {
        let replay = match (since, parser, terminal) {
            (None, Some(parser), true) => vec![(1, parser.screen().state_formatted())],
            (since, _, _) => log.frames(since.unwrap_or(0)),
        };
        (replay, log.written(MERGED), session.events.subscribe())
    });
    session.viewers.fetch_add(1, Ordering::Relaxed);
    let mut open = true;
    for (stream, bytes) in replay {
        open = open && sink.send(frame(terminal, stream, &bytes)).await.is_ok();
    }
    let exited = |code: Option<i32>| {
        ws::Message::Text(
            serde_json::json!({"exited": {"exit_code": code}})
                .to_string()
                .into(),
        )
    };
    if !session.running() && open {
        // Nothing more will come; what was written after the snapshot is in it or lost with berthd.
        let _ = sink.send(exited(session.state().exit_code)).await;
        open = false;
    }
    while open {
        tokio::select! {
            event = events.recv() => match event {
                Ok(Event::Output { stream, offset, bytes }) => {
                    if offset >= next {
                        next = offset + bytes.len() as u64;
                        open = sink.send(frame(terminal, stream, &bytes)).await.is_ok();
                    }
                }
                Ok(Event::Exited(code)) => {
                    let _ = sink.send(exited(code)).await;
                    open = false;
                }
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    let note = serde_json::json!({"dropped": {"frames": missed}}).to_string();
                    open = sink.send(ws::Message::Text(note.into())).await.is_ok();
                    next = 0;
                }
                Err(broadcast::error::RecvError::Closed) => open = false,
            },
            message = source.next() => match message {
                Some(Ok(ws::Message::Binary(bytes))) => {
                    if terminal {
                        session.write(bytes.to_vec());
                    } else if bytes.first() == Some(&0) {
                        session.write(bytes[1..].to_vec());
                    }
                }
                Some(Ok(ws::Message::Text(text))) => {
                    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
                    if let Ok(size) = serde_json::from_value::<api::Resize>(value["resize"].clone()) {
                        let _ = session.resize(size.rows, size.cols);
                    }
                    if let Some(signal) = value["signal"].as_str().and_then(signal_number) {
                        session.signal(signal);
                    }
                    if value["eof"] == true {
                        session.close_input();
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => open = false,
            },
        }
    }
    let _ = sink.close().await;
    session.viewers.fetch_sub(1, Ordering::Relaxed);
}

#[derive(serde::Deserialize)]
pub struct OutputQuery {
    stream: Option<String>,
    since: Option<u64>,
    limit: Option<usize>,
    line: Option<u64>,
    count: Option<usize>,
    tail: Option<usize>,
}

pub async fn output(
    AppState(state): AppState<Arc<State>>,
    Extension(caller): Extension<Caller>,
    UrlPath(id): UrlPath<String>,
    Query(query): Query<OutputQuery>,
) -> R<Json<api::Output>> {
    let session = visible(&state, &caller, &id)?;
    let terminal = session.kind() == SessionKind::Terminal;
    let selection = match query.stream.as_deref() {
        _ if terminal => 1,
        None | Some("merged") => MERGED,
        Some("stdout") => 1,
        Some("stderr") => 2,
        Some(_) => return Err(E::bad("stream is stdout, stderr or merged")),
    };
    let by_lines = query.line.is_some() || query.tail.is_some();
    if by_lines && terminal {
        return Err(E::bad(
            "A terminal's bytes are cursor movement and redraws; read its lines from /screen",
        ));
    }
    let ended = !session.running();
    Ok(Json(session.with_log(|log, _| {
        let first_offset = (log.first(selection) > 0).then(|| log.first(selection));
        if by_lines {
            let count = query.tail.or(query.count).unwrap_or(100).min(100_000);
            let from = if query.tail.is_some() {
                None
            } else {
                query.line
            };
            let lines = log.read_lines(selection, from, count, ended);
            let data = lines.lines.join(&b'\n');
            api::Output {
                data_b64: api::b64(&data),
                next_line: lines.first + lines.lines.len() as u64,
                lines: Some(
                    lines
                        .lines
                        .iter()
                        .map(|line| String::from_utf8_lossy(line).into_owned())
                        .collect(),
                ),
                first_offset,
                offset: lines.offset,
                next_offset: lines.next_offset,
                ended: ended && lines.next_offset >= log.written(selection),
            }
        } else {
            let limit = query.limit.unwrap_or(1 << 20).min(8 << 20);
            let (offset, data) = log.read(selection, query.since.unwrap_or(0), limit);
            let next_offset = offset + data.len() as u64;
            api::Output {
                data_b64: api::b64(&data),
                lines: None,
                first_offset,
                offset,
                next_offset,
                next_line: log.lines(selection),
                ended: ended && next_offset >= log.written(selection),
            }
        }
    })))
}

#[derive(serde::Deserialize)]
pub struct ScreenQuery {
    format: Option<String>,
    tail: Option<usize>,
}

pub async fn screen(
    AppState(state): AppState<Arc<State>>,
    Extension(caller): Extension<Caller>,
    UrlPath(id): UrlPath<String>,
    Query(query): Query<ScreenQuery>,
) -> R<Json<api::Screen>> {
    let session = visible(&state, &caller, &id)?;
    let ansi = match query.format.as_deref() {
        None | Some("text") => false,
        Some("ansi") => true,
        Some(_) => return Err(E::bad("format is text or ansi")),
    };
    session
        .with_screen(|parser| {
            let (rows, cols) = parser.screen().size();
            let (cursor_row, cursor_col) = parser.screen().cursor_position();
            let text = match query.tail {
                // Screen and scrollback as lines, the last `tail` of them.
                Some(tail) => {
                    parser.screen_mut().set_scrollback(usize::MAX);
                    let above = parser.screen().scrollback();
                    let mut lines: Vec<String> = vec![];
                    let mut back = above as isize;
                    loop {
                        parser.screen_mut().set_scrollback(back.max(0) as usize);
                        // The last page overlaps the one before by what did not fit.
                        let skip = (-back).max(0) as usize;
                        lines.extend(parser.screen().rows(0, cols).skip(skip));
                        if back <= 0 {
                            break;
                        }
                        back -= rows as isize;
                    }
                    parser.screen_mut().set_scrollback(0);
                    while lines.last().is_some_and(|line| line.trim().is_empty()) {
                        lines.pop();
                    }
                    lines[lines.len().saturating_sub(tail)..].join("\n")
                }
                None if ansi => {
                    String::from_utf8_lossy(&parser.screen().contents_formatted()).into_owned()
                }
                None => parser.screen().contents(),
            };
            api::Screen {
                rows,
                cols,
                text,
                cursor_row,
                cursor_col,
            }
        })
        .map(Json)
}

fn done() -> Json<api::Done> {
    Json(api::Done { ok: true })
}

fn alive(state: &State, id: &str) -> R<Arc<Session>> {
    let session = state.sessions.get(id)?;
    if !session.running() {
        return Err(E(code::CONFLICT, format!("Session {id} has ended")));
    }
    Ok(session)
}

pub async fn input(
    AppState(state): AppState<Arc<State>>,
    UrlPath(id): UrlPath<String>,
    Json(input): Json<api::Input>,
) -> R<Json<api::Done>> {
    let session = alive(&state, &id)?;
    let bytes = match (&input.data_b64, &input.text) {
        (Some(data), _) => api::unb64(data).ok_or_else(|| E::bad("data_b64 is not base64"))?,
        (None, Some(text)) => text.clone().into_bytes(),
        (None, None) => vec![],
    };
    if !bytes.is_empty() {
        session.write(bytes);
    }
    if input.eof {
        // Let what was just written go first.
        tokio::time::sleep(Duration::from_millis(20)).await;
        session.close_input();
    }
    Ok(done())
}

pub async fn resize(
    AppState(state): AppState<Arc<State>>,
    UrlPath(id): UrlPath<String>,
    Json(size): Json<api::Resize>,
) -> R<Json<api::Done>> {
    alive(&state, &id)?.resize(size.rows, size.cols)?;
    Ok(done())
}

pub async fn signal(
    AppState(state): AppState<Arc<State>>,
    UrlPath(id): UrlPath<String>,
    Json(signal): Json<api::Signal>,
) -> R<Json<api::Done>> {
    let number =
        signal_number(&signal.signal).ok_or_else(|| E::bad("signal is INT, TERM, HUP or KILL"))?;
    alive(&state, &id)?.signal(number);
    Ok(done())
}

pub async fn delete(
    AppState(state): AppState<Arc<State>>,
    UrlPath(id): UrlPath<String>,
) -> R<Json<api::Done>> {
    let session = state.sessions.get(&id)?;
    state.sessions.delete(&session).await;
    Ok(done())
}
