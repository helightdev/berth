//! One way to run an action as a job, wherever it runs. The runner writes
//! the job's record, keeps its output in a log with secrets masked, says
//! when it started and ended, and records the exit code. It holds the lock
//! of the job's subject while an action that changes something runs, so two
//! of those do not overlap, and a repeated token finds the job it started.
//!
//! What a job runs is one of three executors: a function of berth (applying
//! a host's profile, a roll-out, a provisioning), a program (ansible, or a
//! plugin's provisioner), or a task of a host's controller. A task runs as
//! a session on the host, and its job copies the session's output while it
//! runs, so the job stays readable when the host is not and after the host
//! has forgotten the session.

use crate::{Result, paths::Paths, profiles::Place, store};
use serde_json::Value;
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

/// What a job is done to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Subject {
    Host(String),
    /// `PLUGIN/ID`.
    Provisioner(String),
    Resource(String),
    /// The installation itself, as for a roll-out over its hosts.
    Installation,
}

impl Subject {
    /// `host:NAME`, `provisioner:PLUGIN/ID`, `resource:ID` or `installation`.
    pub fn key(&self) -> String {
        match self {
            Subject::Host(name) => format!("host:{name}"),
            Subject::Provisioner(id) => format!("provisioner:{id}"),
            Subject::Resource(id) => format!("resource:{id}"),
            Subject::Installation => "installation".into(),
        }
    }

    pub fn parse(key: &str) -> Option<Subject> {
        if key == "installation" {
            return Some(Subject::Installation);
        }
        let (kind, name) = key.split_once(':')?;
        if name.is_empty() {
            return None;
        }
        match kind {
            "host" => Some(Subject::Host(name.into())),
            "provisioner" => Some(Subject::Provisioner(name.into())),
            "resource" => Some(Subject::Resource(name.into())),
            _ => None,
        }
    }

    /// The file whose lock is held while something changes it. A host's is
    /// the one its machine's provisioning holds, so that the two exclude
    /// each other too.
    fn lock_file(&self, paths: &Paths) -> Result<PathBuf> {
        let file = |kind: &str, name: &str| {
            let plain = name
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || "-_.".contains(ch))
                && !name.starts_with('.');
            let name = match plain {
                true => name.to_owned(),
                false => {
                    use sha2::{Digest, Sha256};
                    let hash = Sha256::digest(name.as_bytes());
                    hash.iter()
                        .take(8)
                        .map(|byte| format!("{byte:02x}"))
                        .collect()
                }
            };
            paths
                .data_dir
                .join("locks")
                .join(format!("{kind}.{name}.lock"))
        };
        Ok(match self {
            Subject::Host(name) => Place::resolve(paths, name)?.lock_file(paths),
            Subject::Provisioner(id) => file("provisioner", &id.replace('/', ".")),
            Subject::Resource(id) => file("resource", id),
            Subject::Installation => paths.data_dir.join("locks").join("installation.lock"),
        })
    }
}

impl std::fmt::Display for Subject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Subject::Host(name) => write!(f, "{name}"),
            Subject::Provisioner(id) => write!(f, "the provisioner {id}"),
            Subject::Resource(id) => write!(f, "the resource {id}"),
            Subject::Installation => write!(f, "this installation"),
        }
    }
}

// ── Locks ───────────────────────────────────────────────────────────────────

/// Held while something changes a subject. A second caller is refused rather
/// than kept waiting.
pub struct Lock(fs::File);

impl Drop for Lock {
    /// Unlocked rather than only closed: a process started meanwhile may hold
    /// a copy of the descriptor until it runs its program, and with it the lock.
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // SAFETY: the descriptor is the open file's own.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// Take the lock at `path` for what `doing` names, which is written into the
/// file so that a caller who is refused can be told who holds it. `refused`
/// makes the refusal from that, with ` (…)` around it or empty.
pub fn lock_at(path: &Path, doing: &str, refused: impl FnOnce(&str) -> String) -> Result<Lock> {
    if let Some(dir) = path.parent() {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    use std::os::fd::AsRawFd;
    // SAFETY: the descriptor is the open file's own.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let mut holder = String::new();
        let _ = file.read_to_string(&mut holder);
        let holder = holder.trim();
        let said = match holder.is_empty() {
            true => String::new(),
            false => format!(" ({holder})"),
        };
        return Err(refused(&said).into());
    }
    // Only the holder writes, so what is there is the one holding it.
    file.set_len(0)?;
    let _ = writeln!(file, "{doing}, process {}", std::process::id());
    Ok(Lock(file))
}

/// Take the subject's lock for what `doing` names, or say what else changes
/// it now.
pub fn lock(paths: &Paths, subject: &Subject, doing: &str) -> Result<Lock> {
    lock_at(&subject.lock_file(paths)?, doing, |holder| {
        format!("Something else is being done to {subject}{holder}. Try again when it has ended")
    })
}

// ── Masking ─────────────────────────────────────────────────────────────────

/// The texts that must not be shown: each secret, and each longer line of
/// one, as a key or a certificate is split over lines. The longest first, so
/// a part does not leave the rest of a whole.
pub fn hidden<'a>(secrets: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut texts = vec![];
    for value in secrets {
        texts.push(value.to_owned());
        texts.extend(
            value
                .lines()
                .map(str::trim)
                .filter(|line| line.len() >= 8)
                .map(str::to_owned),
        );
    }
    texts.retain(|text| !text.is_empty());
    texts.sort_by_key(|text| std::cmp::Reverse(text.len()));
    texts.dedup();
    texts
}

/// A line with each hidden text replaced.
pub fn mask(line: &[u8], hidden: &[String]) -> Vec<u8> {
    let mut line = line.to_vec();
    for text in hidden {
        let text = text.as_bytes();
        let mut out = Vec::with_capacity(line.len());
        let mut rest = &line[..];
        while let Some(at) = rest.windows(text.len()).position(|window| window == text) {
            out.extend_from_slice(&rest[..at]);
            out.extend_from_slice(b"[secret]");
            rest = &rest[at + text.len()..];
        }
        out.extend_from_slice(rest);
        line = out;
    }
    line
}

/// Output on its way to a log or a terminal, with the hidden texts masked.
/// With nothing to hide it passes on as it comes; otherwise it passes on a
/// line at a time, since a secret may arrive in two pieces.
pub struct Output<'a> {
    inner: &'a mut dyn Write,
    hidden: Vec<String>,
    pending: Vec<u8>,
    log: Option<PathBuf>,
}

impl<'a> Output<'a> {
    pub fn new(inner: &'a mut dyn Write, hidden: Vec<String>) -> Output<'a> {
        Output {
            inner,
            hidden,
            pending: vec![],
            log: None,
        }
    }

    /// Where the job's output is kept, when it is kept.
    pub fn log(&self) -> Option<&Path> {
        self.log.as_deref()
    }

    fn pass_lines(&mut self) -> std::io::Result<()> {
        while let Some(end) = self
            .pending
            .iter()
            .position(|byte| *byte == b'\n' || *byte == b'\r')
        {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            self.inner.write_all(&mask(&line, &self.hidden))?;
        }
        Ok(())
    }
}

impl Write for Output<'_> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if self.hidden.is_empty() {
            return self.inner.write(data);
        }
        self.pending.extend_from_slice(data);
        self.pass_lines()?;
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.pass_lines()?;
        self.inner.flush()
    }
}

impl Drop for Output<'_> {
    fn drop(&mut self) {
        let rest = std::mem::take(&mut self.pending);
        let _ = self.inner.write_all(&mask(&rest, &self.hidden));
        let _ = self.inner.flush();
    }
}

// ── What runs ───────────────────────────────────────────────────────────────

/// A host's controller as a job asks it, waiting for each answer. The
/// server's link and a test's double are each one.
pub trait Controller: Send {
    fn get(&mut self, path: &str) -> std::result::Result<Value, String>;
}

/// A program and what it is given: its standard output and error are the
/// job's output, its exit code the job's.
pub struct Program {
    pub command: Command,
    /// Written to its standard input, which is otherwise empty.
    pub input: Option<Vec<u8>>,
    /// Private files it reads, removed once it has ended.
    pub files: Vec<tempfile::NamedTempFile>,
}

pub enum Work {
    /// A function of berth: its output goes to the writer, and it says
    /// whether it went well.
    Function(Box<dyn FnOnce(&mut Output) -> bool + Send>),
    Program(Box<Program>),
    /// A session a task of a host's controller runs, by its id there,
    /// followed until it ends.
    Session {
        controller: Box<dyn Controller>,
        id: String,
    },
}

/// What a job is and how it may run.
pub struct Request {
    pub subject: Subject,
    pub action: String,
    /// The hosts it is about, for a reader.
    pub host: String,
    pub token: Option<String>,
    /// Whether it changes the subject: then it holds the subject's lock.
    pub changes: bool,
    /// The subject's lock, taken already by what prepared the work.
    pub held: Option<Lock>,
    /// Texts that never reach the output, such as a password the work is given.
    pub secrets: Vec<String>,
}

impl Request {
    pub fn new(subject: Subject, action: &str) -> Request {
        let host = match &subject {
            Subject::Host(name) => name.clone(),
            _ => String::new(),
        };
        Request {
            subject,
            action: action.to_owned(),
            host,
            token: None,
            changes: true,
            held: None,
            secrets: vec![],
        }
    }

    /// The lock the work needs: held already, taken now, or none for an
    /// action that only reads.
    fn lock(&mut self, paths: &Paths) -> Result<Option<Lock>> {
        match (self.held.take(), self.changes) {
            (Some(held), _) => Ok(Some(held)),
            (None, true) => Ok(Some(lock(paths, &self.subject, &self.action)?)),
            (None, false) => Ok(None),
        }
    }
}

/// How long a session's output is asked for again while there is no news,
/// and how long a host may not answer before its job gives up on it.
const POLL: Duration = Duration::from_millis(500);
const PATIENCE: Duration = Duration::from_secs(300);

/// Run the work to its end, its output to `out`: the exit code.
fn execute(work: Work, out: &mut Output) -> Option<i32> {
    match work {
        Work::Function(function) => Some(if function(out) { 0 } else { 1 }),
        Work::Program(program) => run_program(*program, out),
        Work::Session { controller, id } => follow(controller, &id, out),
    }
}

fn run_program(program: Program, out: &mut Output) -> Option<i32> {
    let Program {
        mut command,
        input,
        files,
    } = program;
    let fail = |out: &mut Output, error: String| {
        let _ = writeln!(out, "✗ {error}");
        None
    };
    let (mut reader, writer) = match std::io::pipe() {
        Ok(pipe) => pipe,
        Err(error) => return fail(out, error.to_string()),
    };
    let copy = match writer.try_clone() {
        Ok(copy) => copy,
        Err(error) => return fail(out, error.to_string()),
    };
    command
        .stdin(match input {
            Some(_) => Stdio::piped(),
            None => Stdio::null(),
        })
        .stdout(writer)
        .stderr(copy);
    let program_name = command.get_program().to_string_lossy().into_owned();
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return fail(out, format!("{program_name}: {error}")),
    };
    // The command holds the pipe's other ends; they close when it is dropped.
    drop(command);
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        // One that does not read its input must not hold this up.
        std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
    }
    let mut buffer = [0; 8192];
    while let Ok(count) = reader.read(&mut buffer)
        && count > 0
    {
        let _ = out.write_all(&buffer[..count]);
        let _ = out.flush();
    }
    let status = child.wait().ok();
    drop(files);
    status.and_then(|status| status.code())
}

/// Copy a session's output as it comes, until it has ended: its exit code.
fn follow(mut controller: Box<dyn Controller>, id: &str, out: &mut Output) -> Option<i32> {
    let (mut since, mut answered) = (0u64, Instant::now());
    loop {
        let asked = controller.get(&format!("/v1/sessions/{id}/output?since={since}"));
        let output: berth_api::Output = match asked
            .and_then(|value| serde_json::from_value(value).map_err(|error| error.to_string()))
        {
            Ok(output) => output,
            Err(_) if answered.elapsed() < PATIENCE => {
                std::thread::sleep(POLL * 2);
                continue;
            }
            Err(error) => {
                let _ = writeln!(
                    out,
                    "\n✗ The host stopped answering ({error}). The session {id} may still run there"
                );
                return None;
            }
        };
        answered = Instant::now();
        let data = berth_api::unb64(&output.data_b64).unwrap_or_default();
        let _ = out.write_all(&data);
        let _ = out.flush();
        let moved = output.next_offset > since;
        since = output.next_offset;
        if output.ended {
            let session = controller.get(&format!("/v1/sessions/{id}"));
            return session
                .ok()
                .and_then(|session| session["exit_code"].as_i64().map(|code| code as i32));
        }
        if !moved {
            std::thread::sleep(POLL);
        }
    }
}

// ── The runner ──────────────────────────────────────────────────────────────

/// Told of a job when it starts and when it ends.
pub type Notify = Arc<dyn Fn(&store::Job) + Send + Sync>;

/// Runs jobs and keeps their records in a store, their output in files.
#[derive(Clone)]
pub struct Runner {
    paths: Paths,
    store: Arc<store::Store>,
    jobs_dir: PathBuf,
    notify: Notify,
}

fn new_id() -> std::io::Result<String> {
    let mut bytes = [0u8; 12];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let letters = b"abcdefghijklmnopqrstuvwxyz234567";
    Ok(format!(
        "j{}",
        bytes
            .iter()
            .map(|byte| letters[(byte & 31) as usize] as char)
            .collect::<String>()
    ))
}

impl Runner {
    pub fn new(
        paths: Paths,
        store: Arc<store::Store>,
        jobs_dir: PathBuf,
        notify: Notify,
    ) -> Runner {
        Runner {
            paths,
            store,
            jobs_dir,
            notify,
        }
    }

    /// Start the work as a job, in a thread of its own, and say which job it
    /// is. A token used before for the same subject and action answers with
    /// that job, and nothing is started. A refusal, such as a lock that is
    /// taken, comes before there is a job.
    pub fn start(&self, request: Request, work: Work) -> Result<store::Job> {
        match self.reserve(request)? {
            Reserved::Found(job) => Ok(job),
            Reserved::New(ticket) => Ok(ticket.run(work)),
        }
    }

    /// Write the job's record before anything runs, with its lock taken. Of
    /// callers with the same token one gets the ticket to run it; the others
    /// find that job.
    pub fn reserve(&self, mut request: Request) -> Result<Reserved> {
        let (subject, action) = (request.subject.key(), request.action.clone());
        if let Some(token) = &request.token
            && let Some(job) = self.store.job_by_token(&subject, &action, token)?
        {
            return Ok(Reserved::Found(job));
        }
        let lock = request.lock(&self.paths)?;
        let id = new_id()?;
        fs::create_dir_all(&self.jobs_dir)?;
        let path = self.jobs_dir.join(format!("{id}.log"));
        let asked = store::Job {
            id: id.clone(),
            host: request.host.clone(),
            what: action.clone(),
            started: berth_api::now(),
            ended: None,
            exit_code: None,
            output_path: path.to_string_lossy().into_owned(),
            subject,
            action,
            token: request.token.clone(),
        };
        let job = self.store.add_job(&asked)?;
        // Another caller with the same token was first.
        if job.id != id {
            return Ok(Reserved::Found(job));
        }
        let file = match fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) => {
                self.store.end_job(&id, None)?;
                return Err(error.into());
            }
        };
        (self.notify)(&job);
        Ok(Reserved::New(Ticket {
            runner: self.clone(),
            job,
            lock,
            file,
            path,
            hidden: hidden(request.secrets.iter().map(String::as_str)),
        }))
    }
}

/// What reserving a job came to.
pub enum Reserved {
    /// The job a call before with the same token started.
    Found(store::Job),
    /// This call's job, to run or to fail.
    New(Ticket),
}

/// A job whose record is written and whose lock is held, before its work runs.
pub struct Ticket {
    runner: Runner,
    job: store::Job,
    lock: Option<Lock>,
    file: fs::File,
    path: PathBuf,
    hidden: Vec<String>,
}

impl Ticket {
    pub fn job(&self) -> &store::Job {
        &self.job
    }

    /// Run the work in a thread of its own: the job as it started.
    pub fn run(self, work: Work) -> store::Job {
        let job = self.job.clone();
        std::thread::spawn(move || {
            let Ticket {
                runner,
                job,
                lock,
                mut file,
                path,
                hidden,
            } = self;
            let exit_code = {
                let mut out = Output::new(&mut file, hidden);
                out.log = Some(path);
                execute(work, &mut out)
            };
            drop(lock);
            runner.end(&job.id, exit_code);
        });
        job
    }

    /// End the job as failed without running anything, with the reason in
    /// its output: the job as it ended.
    pub fn fail(mut self, reason: &str) -> store::Job {
        let _ = writeln!(self.file, "✗ {reason}");
        drop(self.lock.take());
        self.runner.end(&self.job.id, Some(1));
        self.runner
            .store
            .job(&self.job.id)
            .ok()
            .flatten()
            .unwrap_or_else(|| self.job.clone())
    }
}

impl Runner {
    fn end(&self, id: &str, exit_code: Option<i32>) {
        let _ = self.store.end_job(id, exit_code);
        if let Ok(Some(job)) = self.store.job(id) {
            (self.notify)(&job);
        }
    }
}

/// Run the work here and now, with its output to `out`, for a berth that
/// keeps no record of jobs: the exit code. It holds the subject's lock as a
/// job would. A program with nothing to hide gets the terminal itself, so it
/// sees one and colours what it writes.
pub fn here(
    paths: &Paths,
    mut request: Request,
    work: Work,
    out: &mut dyn Write,
) -> Result<Option<i32>> {
    let _lock = request.lock(paths)?;
    let hidden = hidden(request.secrets.iter().map(String::as_str));
    let work = match work {
        Work::Program(program) if hidden.is_empty() => return Ok(inherit(*program)?),
        work => work,
    };
    let mut out = Output::new(out, hidden);
    Ok(execute(work, &mut out))
}

/// Run a program with the terminal as its output.
fn inherit(program: Program) -> std::io::Result<Option<i32>> {
    let Program {
        mut command,
        input,
        files,
    } = program;
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command.spawn()?;
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        let _ = stdin.write_all(&input);
    }
    let status = child.wait()?;
    drop(files);
    Ok(status.code())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, mpsc};

    struct World {
        _dir: tempfile::TempDir,
        paths: Paths,
        store: Arc<store::Store>,
        runner: Runner,
        told: Arc<Mutex<Vec<store::Job>>>,
    }

    fn world() -> World {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: dir.path().join("config"),
            data_dir: dir.path().join("data"),
        };
        let store = Arc::new(store::Store::open(&dir.path().join("store.db")).unwrap());
        let told = Arc::new(Mutex::new(vec![]));
        let heard = told.clone();
        let notify: Notify =
            Arc::new(move |job: &store::Job| heard.lock().unwrap().push(job.clone()));
        let runner = Runner::new(
            paths.clone(),
            store.clone(),
            paths.data_dir.join("jobs"),
            notify,
        );
        World {
            _dir: dir,
            paths,
            store,
            runner,
            told,
        }
    }

    fn ended(world: &World, id: &str) -> store::Job {
        for _ in 0..500 {
            let job = world.store.job(id).unwrap().unwrap();
            if job.ended.is_some() {
                return job;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("{id} did not end");
    }

    fn log(job: &store::Job) -> String {
        fs::read_to_string(&job.output_path).unwrap()
    }

    #[test]
    fn subjects_are_keys_and_back() {
        for subject in [
            Subject::Host("studio".into()),
            Subject::Provisioner("incus/vm".into()),
            Subject::Resource("r-1".into()),
            Subject::Installation,
        ] {
            assert_eq!(Subject::parse(&subject.key()), Some(subject));
        }
        assert_eq!(Subject::parse("host:"), None);
        assert_eq!(Subject::parse("planet:x"), None);
    }

    #[test]
    fn a_secret_is_masked_also_in_two_pieces() {
        let hidden = hidden(["hunter2-longer", "a\nline of a key\n"]);
        let mut kept = vec![];
        {
            let mut out = Output::new(&mut kept, hidden);
            out.write_all(b"one hunter2-").unwrap();
            out.write_all(b"longer two\nline of a key and the rest")
                .unwrap();
        }
        assert_eq!(
            String::from_utf8(kept).unwrap(),
            "one [secret] two\n[secret] and the rest"
        );
        // Nothing to hide passes on as it comes, a progress meter included.
        let mut plain = vec![];
        Output::new(&mut plain, vec![]).write_all(b"50%\r").unwrap();
        assert_eq!(plain, b"50%\r");
    }

    #[test]
    fn a_function_is_a_job_with_its_record_output_and_events() {
        let world = world();
        let mut request = Request::new(Subject::Installation, "rollout");
        request.host = "studio, lab".into();
        request.secrets = vec!["s3cret-value".into()];
        let work = Work::Function(Box::new(|out: &mut Output| {
            writeln!(out, "using s3cret-value").unwrap();
            assert!(
                out.log()
                    .is_some_and(|log| log.extension() == Some("log".as_ref()))
            );
            false
        }));
        let job = world.runner.start(request, work).unwrap();
        assert_eq!(
            (job.subject.as_str(), job.action.as_str()),
            ("installation", "rollout")
        );
        let job = ended(&world, &job.id);
        assert_eq!(job.exit_code, Some(1));
        assert_eq!(log(&job), "using [secret]\n");
        assert_eq!(job.host, "studio, lab");
        // Told when it started and when it ended.
        let told = world.told.lock().unwrap();
        assert_eq!(told.len(), 2);
        assert!(told[0].ended.is_none() && told[1].ended.is_some());
    }

    #[test]
    fn a_program_is_a_job_with_its_input_and_exit_code() {
        let world = world();
        let mut command = Command::new("sh");
        command.args(["-c", "read line; echo \"got $line\"; echo err >&2; exit 3"]);
        let work = Work::Program(Box::new(Program {
            command,
            input: Some(b"pass-word-1\n".to_vec()),
            files: vec![],
        }));
        let mut request = Request::new(Subject::Resource("r-1".into()), "change");
        request.secrets = vec!["pass-word-1".into()];
        let job = world.runner.start(request, work).unwrap();
        let job = ended(&world, &job.id);
        assert_eq!(job.exit_code, Some(3));
        let text = log(&job);
        assert!(
            text.contains("got [secret]") && text.contains("err"),
            "{text}"
        );

        // One that cannot start says so in its output.
        let work = Work::Program(Box::new(Program {
            command: Command::new("/nonexistent/program"),
            input: None,
            files: vec![],
        }));
        let job = world
            .runner
            .start(
                Request::new(Subject::Resource("r-2".into()), "change"),
                work,
            )
            .unwrap();
        let job = ended(&world, &job.id);
        assert_eq!(job.exit_code, None);
        assert!(log(&job).contains("/nonexistent/program"));
    }

    /// A host's controller that has one session, whose output comes in pieces.
    struct Fake {
        pieces: Vec<&'static str>,
        asked: Arc<Mutex<Vec<String>>>,
        failures: usize,
    }

    impl Controller for Fake {
        fn get(&mut self, path: &str) -> std::result::Result<Value, String> {
            self.asked.lock().unwrap().push(path.to_owned());
            if self.failures > 0 {
                self.failures -= 1;
                return Err("no route to host".into());
            }
            if path == "/v1/sessions/s1" {
                return Ok(serde_json::json!({"exit_code": 4}));
            }
            let since: u64 = path.rsplit('=').next().unwrap().parse().unwrap();
            let all = self.pieces.concat();
            let shown: usize = self.pieces.iter().take(2).map(|piece| piece.len()).sum();
            // The second answer has the rest, and the session has ended then.
            let to = if since == 0 {
                shown.min(all.len())
            } else {
                all.len()
            };
            let to = to.max(since as usize);
            Ok(serde_json::json!({
                "data_b64": berth_api::b64(&all.as_bytes()[since as usize..to]),
                "offset": since, "next_offset": to, "next_line": 0,
                "ended": since > 0 && to == all.len(),
            }))
        }
    }

    #[test]
    fn a_task_of_a_host_is_a_job_that_copies_its_session() {
        let world = world();
        let asked = Arc::new(Mutex::new(vec![]));
        let controller = Box::new(Fake {
            pieces: vec!["one\n", "two\n", "three\n"],
            asked: asked.clone(),
            failures: 1,
        });
        let mut request = Request::new(Subject::Host("studio".into()), "status");
        // A task runs beside the host's other sessions, without the lock.
        request.changes = false;
        let work = Work::Session {
            controller,
            id: "s1".into(),
        };
        let job = world.runner.start(request, work).unwrap();
        let job = ended(&world, &job.id);
        assert_eq!(log(&job), "one\ntwo\nthree\n");
        assert_eq!(job.exit_code, Some(4));
        let asked = asked.lock().unwrap();
        // Once more after the host did not answer, then on from where it was.
        assert_eq!(asked[0], asked[1]);
        assert!(asked.contains(&"/v1/sessions/s1/output?since=8".to_owned()));
        assert_eq!(asked.last().unwrap(), "/v1/sessions/s1");
    }

    #[test]
    fn a_subject_is_changed_by_one_job_at_a_time() {
        let world = world();
        let (go, wait) = mpsc::channel::<()>();
        let blocked = Work::Function(Box::new(move |_: &mut Output| wait.recv().is_ok()));
        let first = world
            .runner
            .start(Request::new(Subject::Installation, "rollout"), blocked)
            .unwrap();
        let again = Work::Function(Box::new(|_: &mut Output| true));
        let refused = world
            .runner
            .start(Request::new(Subject::Installation, "rollout"), again)
            .unwrap_err();
        assert!(
            refused.to_string().contains("this installation"),
            "{refused}"
        );
        // Here too, as a berth without a record runs it.
        let here = here(
            &world.paths,
            Request::new(Subject::Installation, "rollout"),
            Work::Function(Box::new(|_: &mut Output| true)),
            &mut std::io::sink(),
        );
        assert!(here.is_err());
        // What only reads takes no lock, and another subject has its own.
        let mut reading = Request::new(Subject::Installation, "status");
        reading.changes = false;
        let read = world
            .runner
            .start(reading, Work::Function(Box::new(|_: &mut Output| true)))
            .unwrap();
        assert_eq!(ended(&world, &read.id).exit_code, Some(0));
        let other = world
            .runner
            .start(
                Request::new(Subject::Resource("r-1".into()), "change"),
                Work::Function(Box::new(|_: &mut Output| true)),
            )
            .unwrap();
        ended(&world, &other.id);
        go.send(()).unwrap();
        assert_eq!(ended(&world, &first.id).exit_code, Some(0));
        let after = world
            .runner
            .start(
                Request::new(Subject::Installation, "rollout"),
                Work::Function(Box::new(|_: &mut Output| true)),
            )
            .unwrap();
        ended(&world, &after.id);
    }

    #[test]
    fn a_reserved_job_that_cannot_start_fails_once_for_its_token() {
        let world = world();
        let request = || {
            let mut request = Request::new(Subject::Host("studio".into()), "status");
            request.changes = false;
            request.token = Some("t-1".into());
            request
        };
        let Reserved::New(ticket) = world.runner.reserve(request()).unwrap() else {
            panic!("the first caller reserves");
        };
        // Another caller with the token meanwhile finds the same job.
        let Reserved::Found(found) = world.runner.reserve(request()).unwrap() else {
            panic!("the second caller does not reserve");
        };
        assert_eq!(found.id, ticket.job().id);
        let failed = ticket.fail("The host did not start status: no route to host");
        assert_eq!(failed.exit_code, Some(1));
        assert!(log(&failed).contains("✗ The host did not start status"));
        // Asked again, the failure is the answer, and nothing starts.
        let Reserved::Found(again) = world.runner.reserve(request()).unwrap() else {
            panic!("a retry reserves again");
        };
        assert_eq!((again.id, again.exit_code), (failed.id, Some(1)));
        assert_eq!(world.told.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_refused_lock_says_who_holds_it() {
        let world = world();
        let held = lock(&world.paths, &Subject::Installation, "rollout").unwrap();
        let refused = lock(&world.paths, &Subject::Installation, "rollout")
            .err()
            .unwrap();
        let said = refused.to_string();
        assert!(
            said.contains(&format!("(rollout, process {})", std::process::id())),
            "{said}"
        );
        drop(held);
        let again = lock(&world.paths, &Subject::Installation, "apply").unwrap();
        drop(again);
    }

    #[test]
    fn a_repeated_token_finds_the_job_it_started() {
        let world = world();
        let runs = Arc::new(Mutex::new(0));
        let work = |runs: &Arc<Mutex<i32>>| {
            let runs = runs.clone();
            Work::Function(Box::new(move |_: &mut Output| {
                *runs.lock().unwrap() += 1;
                true
            }))
        };
        let request = |token: &str| {
            let mut request = Request::new(Subject::Resource("r-1".into()), "change");
            request.token = Some(token.into());
            request
        };
        let first = world.runner.start(request("t-1"), work(&runs)).unwrap();
        ended(&world, &first.id);
        let again = world.runner.start(request("t-1"), work(&runs)).unwrap();
        assert_eq!(again.id, first.id);
        let other = world.runner.start(request("t-2"), work(&runs)).unwrap();
        assert_ne!(other.id, first.id);
        ended(&world, &other.id);
        assert_eq!(*runs.lock().unwrap(), 2);
    }

    #[test]
    fn here_runs_with_the_output_given_and_masks_it() {
        let world = world();
        let mut request = Request::new(Subject::Installation, "rollout");
        request.secrets = vec!["s3cret-value".into()];
        let mut out = vec![];
        let code = here(
            &world.paths,
            request,
            Work::Function(Box::new(|out: &mut Output| {
                assert!(out.log().is_none());
                writeln!(out, "s3cret-value").is_ok()
            })),
            &mut out,
        )
        .unwrap();
        assert_eq!((code, out), (Some(0), b"[secret]\n".to_vec()));
        assert!(world.store.jobs(10).unwrap().is_empty());
    }
}
