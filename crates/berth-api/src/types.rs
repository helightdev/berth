use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use utoipa::ToSchema;

/// The fixed list of error codes, with the status each travels under.
pub mod code {
    pub const BAD_REQUEST: (u16, &str) = (400, "bad_request");
    pub const UNAUTHORIZED: (u16, &str) = (401, "unauthorized");
    pub const FORBIDDEN: (u16, &str) = (403, "forbidden");
    pub const NOT_FOUND: (u16, &str) = (404, "not_found");
    pub const CONFLICT: (u16, &str) = (409, "conflict");
    pub const PRECONDITION: (u16, &str) = (412, "precondition_failed");
    pub const TOO_LARGE: (u16, &str) = (413, "too_large");
    pub const LIMIT: (u16, &str) = (429, "limit");
    pub const INTERNAL: (u16, &str) = (500, "internal");
    pub const NOT_AVAILABLE: (u16, &str) = (501, "not_available");
    pub const UNREACHABLE: (u16, &str) = (502, "unreachable");
    pub const NOT_SERVED: (u16, &str) = (503, "not_served");
    pub const TIMEOUT: (u16, &str) = (504, "timeout");
    /// An error of a plugin's service, passed on.
    pub const SERVICE: (u16, &str) = (520, "service_error");
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ErrorDetail {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Version {
    pub version: String,
    pub api: u32,
    /// `system` or `single`.
    #[serde(default)]
    pub mode: String,
    /// What the host's configuration was last applied from, when berth
    /// recorded it. An older berthd does not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied: Option<Applied>,
    /// What was installed on the host, by that record: `controller` when
    /// only the controller was, `workstation` for a whole profile. Absent
    /// when the host keeps no record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<HostLevel>,
}

/// How much of berth a host has: what its profile means it to have, or
/// what it was seen to have. Each level can do what the ones before it can.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum HostLevel {
    /// berth logs in over SSH. Nothing of berth is installed.
    Reachable,
    /// berthd and box are installed and answer: sessions, tasks and files.
    Controller,
    /// The whole profile is applied: desktop, agents and plugins.
    #[default]
    Workstation,
}

impl HostLevel {
    pub const ALL: [HostLevel; 3] = [
        HostLevel::Reachable,
        HostLevel::Controller,
        HostLevel::Workstation,
    ];

    pub fn name(self) -> &'static str {
        match self {
            HostLevel::Reachable => "reachable",
            HostLevel::Controller => "controller",
            HostLevel::Workstation => "workstation",
        }
    }

    pub fn parse(text: &str) -> Option<HostLevel> {
        HostLevel::ALL
            .into_iter()
            .find(|level| level.name() == text.trim())
    }

    /// What a host of this level has, in a few words that fit a line
    /// beside the level on a narrow page.
    pub fn describe(self) -> &'static str {
        match self {
            HostLevel::Reachable => "SSH only, nothing of berth installed",
            HostLevel::Controller => "berthd alone: sessions, tasks, files",
            HostLevel::Workstation => "the whole profile",
        }
    }
}

impl std::fmt::Display for HostLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// What to say when a host is seen to be more than its profile here says,
/// with how to adopt it: as when a workstation is added by its address.
pub fn level_mismatch(
    name: &str,
    intended: HostLevel,
    observed: Option<HostLevel>,
) -> Option<String> {
    let observed = observed.filter(|observed| *observed > intended)?;
    Some(match observed {
        HostLevel::Workstation => format!(
            "{name} is a workstation; its profile here says {intended}. To adopt it from the profile it keeps: berth adopt {name}"
        ),
        _ => format!(
            "{name} has berth's controller; its profile here says {intended}. To adopt it: berth host level {name} controller"
        ),
    })
}

/// What a setup key says of the server it is for.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SetupKey {
    /// The version of berth serve.
    pub version: String,
    pub api: u32,
    /// When the key stops working, in RFC 3339.
    pub expires: String,
}

/// The server's token, collected with a setup key.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SetupClaim {
    /// The token of berth serve, as in its serve.token.
    pub token: String,
    pub version: String,
    pub api: u32,
    pub expires: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Host,
    Agent,
}

/// Who did something, as berthd saw it.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct From {
    pub level: Level,
    pub uid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

// ── Sessions ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum SessionKind {
    Process,
    Terminal,
}

/// A list of arguments, or one string for `sh -c`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum CommandLine {
    Args(Vec<String>),
    Shell(String),
}

fn agent() -> String {
    "agent".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct StartSession {
    pub kind: SessionKind,
    pub command: CommandLine,
    /// `agent`, `operator` (the SSH user) or `root`.
    #[serde(default = "agent")]
    pub user: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Text for standard input; a process then sees the end of its input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_b64: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Seconds after which the command is ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cols: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Step {
    pub name: String,
    pub user: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Session {
    pub id: String,
    pub kind: SessionKind,
    pub title: String,
    pub command: Vec<String>,
    pub user: String,
    pub started_by: From,
    /// The task it was started for, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    pub started: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// It ended with berthd or the host, not by itself.
    #[serde(default)]
    pub lost: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<Step>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_output: Option<String>,
    #[serde(default)]
    pub viewers: u32,
    #[serde(default)]
    pub output_size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cols: Option<u16>,
}

impl Session {
    pub fn running(&self) -> bool {
        self.ended.is_none()
    }
}

/// The answer to a start with `wait=`: the session and, when it has ended in
/// time, what it wrote.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Started {
    #[serde(flatten)]
    pub session: Session,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout_b64: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_b64: Option<String>,
    /// A terminal's bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_b64: Option<String>,
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Output {
    pub data_b64: String,
    /// The same as text, when read by lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<Vec<String>>,
    /// Where what is still kept begins, when the start was dropped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_offset: Option<u64>,
    /// The offset this answer starts at.
    pub offset: u64,
    pub next_offset: u64,
    pub next_line: u64,
    /// Nothing more will come.
    pub ended: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Screen {
    pub rows: u16,
    pub cols: u16,
    pub text: String,
    pub cursor_row: u16,
    pub cursor_col: u16,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct Input {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_b64: Option<String>,
    /// Close a process's standard input.
    #[serde(default)]
    pub eof: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema)]
pub struct Resize {
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Signal {
    /// `INT`, `TERM`, `HUP` or `KILL`.
    pub signal: String,
}

// ── Tasks ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Argument {
    pub name: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: String,
    /// A command whose lines are the values to offer.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub choices: String,
    /// `resource` for an argument of a provisioner that sizes the machine:
    /// its CPUs, memory or disk. Empty for any other argument.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    /// What it is for, in a sentence or two, beside its short label.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub help: String,
    /// The values a provisioner's argument takes, when it is a choice of a
    /// few. Empty for one that takes any text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
    /// What is given is a secret, such as a password: it is not shown as it
    /// is typed, nor kept, nor written to a job's output.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub secret: bool,
}

impl Argument {
    /// The kind that marks a resource of the machine.
    pub const RESOURCE: &'static str = "resource";

    /// Whether it sizes the machine a provisioner creates.
    pub fn is_resource(&self) -> bool {
        self.kind == Self::RESOURCE
    }
}

/// An entry of `/etc/berth/tasks.json`. Listed without `run`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct Task {
    pub name: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub help: String,
    #[serde(default)]
    pub group: String,
    #[serde(default = "agent")]
    pub user: String,
    #[serde(default)]
    pub terminal: bool,
    /// The agent may run it.
    #[serde(default)]
    pub agent: bool,
    /// Tasks it is a step of; it then has no row of its own.
    #[serde(default)]
    pub extends: Vec<String>,
    #[serde(default)]
    pub confirm: Option<String>,
    /// What the workstation sends on standard input: `secrets`.
    #[serde(default)]
    pub input: Option<String>,
    #[serde(default)]
    pub arguments: Vec<Argument>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub run: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct RunTask {
    #[serde(default)]
    pub arguments: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_b64: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cols: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Choices {
    pub choices: Vec<String>,
}

// ── Actions ─────────────────────────────────────────────────────────────────

/// Where an action runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Runs {
    /// Where berth runs, as a function of berth or a plugin's program. A run
    /// that changes something is a job.
    #[default]
    Berth,
    /// On the host, by its controller, as a session there.
    Host,
}

/// What a plugin adds to an action: a step run after it in the same run.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ActionStep {
    pub label: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub help: String,
}

fn yes() -> bool {
    true
}

fn operate() -> crate::ops::Role {
    crate::ops::Role::Operate
}

/// Something that can be done to a subject: a host, the installation, and
/// later a provisioner or a resource. It says enough to build a form from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Action {
    pub name: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub help: String,
    /// The group of the page it is listed in; `System` when empty.
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub arguments: Vec<Argument>,
    /// The question to answer yes to before it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm: Option<String>,
    #[serde(default)]
    pub runs: Runs,
    /// It needs a terminal session attached; only a host runs these.
    #[serde(default)]
    pub terminal: bool,
    /// False when it only reads, and may be called at any time.
    #[serde(default = "yes")]
    pub changes: bool,
    /// The least role that may run it.
    #[serde(default = "operate")]
    pub role: crate::ops::Role,
    /// False, with `why`, when the subject's state rules it out now.
    #[serde(default = "yes")]
    pub available: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub why: String,
    /// What plugins add to it, in the order they run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<ActionStep>,
}

impl Default for Action {
    fn default() -> Action {
        Action {
            name: String::new(),
            label: String::new(),
            help: String::new(),
            group: String::new(),
            arguments: vec![],
            confirm: None,
            runs: Runs::Berth,
            terminal: false,
            changes: true,
            role: crate::ops::Role::Operate,
            available: true,
            why: String::new(),
            steps: vec![],
        }
    }
}

impl Action {
    /// The actions of a host's tasks, in the order the host lists them: each
    /// task that is no step of another, with the steps plugins add to it. The
    /// host keeps the rest of a task (its user, its command, its input), and
    /// runs it by name.
    pub fn of_tasks(tasks: &[Task]) -> Vec<Action> {
        tasks
            .iter()
            .filter(|task| task.extends.is_empty())
            .map(|task| Action {
                name: task.name.clone(),
                label: task.label.clone(),
                help: task.help.clone(),
                group: task.group.clone(),
                arguments: task.arguments.clone(),
                confirm: task.confirm.clone(),
                runs: Runs::Host,
                terminal: task.terminal,
                steps: tasks
                    .iter()
                    .filter(|step| step.extends.contains(&task.name))
                    .map(|step| ActionStep {
                        label: step.label.clone(),
                        help: step.help.clone(),
                    })
                    .collect(),
                ..Action::default()
            })
            .collect()
    }

    /// What it is called on screen.
    pub fn title(&self) -> &str {
        match self.label.is_empty() {
            true => &self.name,
            false => &self.label,
        }
    }
}

/// What running an action is given.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Act {
    #[serde(default)]
    pub arguments: BTreeMap<String, String>,
    /// Any text of the caller's. The same token again for the same subject
    /// and action answers with the job the first call started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// The SSH user's sudo password, for an action that logs in to a host
    /// whose sudo asks for one. It is neither shown nor kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sudo_password: Option<String>,
    /// The size of the terminal, for an action that runs in one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cols: Option<u16>,
}

/// What running an action answers: one of the three.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct Acted {
    /// The job that runs it, for an action of berth that changes something.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<JobInfo>,
    /// The session on the host, for an action the host runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<Session>,
    /// What an action that answers at once and changes nothing said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
}

/// A job of berth: one run of an action, with its output kept where it ran.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct JobInfo {
    pub id: String,
    /// What it was done to: `host:NAME`, `provisioner:PLUGIN/ID`,
    /// `resource:ID` or `installation`. Empty for a job from before 0.5.
    #[serde(default)]
    pub subject: String,
    /// The action it runs, such as `apply`, `rollout` or `provision`.
    #[serde(default)]
    pub action: String,
    /// The hosts it is about, for a reader: a name, or several with commas.
    #[serde(default)]
    pub host: String,
    pub started: String,
    pub ended: Option<String>,
    pub exit_code: Option<i32>,
    /// How a roll-out went, host by host, once it has ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<RolloutReport>,
}

// ── System ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct Memory {
    pub total: u64,
    pub used: u64,
    pub available: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Filesystem {
    pub mount: String,
    pub device: String,
    pub kind: String,
    pub total: u64,
    pub used: u64,
    pub available: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Interface {
    pub name: String,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Gpu {
    pub name: String,
    /// Percent.
    pub utilization: f64,
    pub memory_used: u64,
    pub memory_total: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UnitState {
    pub unit: String,
    pub active: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct Stats {
    pub hostname: String,
    pub load: [f64; 3],
    /// Percent busy over 200 ms, all cores together.
    pub cpu: f64,
    pub cores: Vec<f64>,
    pub memory: Memory,
    pub swap: Memory,
    pub filesystems: Vec<Filesystem>,
    pub network: Vec<Interface>,
    /// Seconds.
    pub uptime: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gpus: Vec<Gpu>,
    pub reboot_required: bool,
    pub units: Vec<UnitState>,
    pub sessions_running: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct FileEntry {
    pub name: String,
    /// `file`, `dir`, `link` or `other`.
    pub kind: String,
    pub size: u64,
    pub modified: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Listing {
    pub path: String,
    pub entries: Vec<FileEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Unit {
    pub unit: String,
    pub load: String,
    pub active: String,
    pub sub: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct LogEntry {
    pub time: String,
    pub unit: String,
    pub priority: u8,
    pub message: String,
    /// Give it back as `since` to go on from here.
    pub cursor: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Process {
    pub pid: u32,
    pub parent: u32,
    pub user: String,
    pub command: String,
    /// Seconds of CPU used so far.
    pub cpu_time: f64,
    /// Resident memory in bytes.
    pub memory: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Done {
    pub ok: bool,
}

// ── Channel ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "lowercase")]
pub enum Keep {
    None,
    Last,
    #[default]
    Log,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Message {
    pub id: u64,
    pub topic: String,
    pub body: Value,
    pub keep: Keep,
    pub from: From,
    pub time: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Publish {
    #[serde(default)]
    pub body: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep: Option<Keep>,
}

/// What a client sends over the channel stream to publish.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PublishFrame {
    pub topic: String,
    #[serde(default)]
    pub body: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep: Option<Keep>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Messages {
    pub messages: Vec<Message>,
    /// Pass as `since` for the rest, when there is more.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<u64>,
}

/// An entry of `/etc/berth/channels.json`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Topic {
    pub topic: String,
    #[serde(default)]
    pub keep: Option<Keep>,
    #[serde(default)]
    pub max: Option<u64>,
    #[serde(default)]
    pub days: Option<u64>,
}

// ── Services ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct Method {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

/// An entry of `/etc/berth/services.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct ServiceSpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// `root` or `agent`.
    #[serde(default = "agent")]
    pub user: String,
    #[serde(default)]
    pub agent: bool,
    #[serde(default)]
    pub run: String,
    /// Seconds a call may take.
    #[serde(default)]
    pub timeout: Option<u64>,
    #[serde(default)]
    pub methods: Vec<Method>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Service {
    pub name: String,
    pub description: String,
    pub agent: bool,
    pub methods: Vec<Method>,
    pub declared: bool,
    pub up: bool,
    /// Who serves it now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<u32>,
}

/// berthd to the serving program.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CallFrame {
    pub id: u64,
    pub method: String,
    pub body: Value,
    pub from: From,
}

/// The serving program's answer.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ReplyFrame {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorDetail>,
}

// ── Configuration ───────────────────────────────────────────────────────────

/// `/etc/berth/berthd.json`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct Config {
    /// The agent account.
    pub agent: String,
    /// The SSH user.
    pub operator: String,
    /// Folders the file calls may touch.
    pub roots: Vec<String>,
    /// Units whose state the stats report.
    pub units: Vec<String>,
    /// Whether the calls that need systemd work. Off in single-user mode
    /// unless set.
    pub systemd: Option<bool>,
    /// A wrapper that gives the agent its environment.
    pub agent_env: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            agent: "agent".into(),
            operator: String::new(),
            roots: vec![],
            units: vec![],
            systemd: None,
            agent_env: "/usr/local/bin/agent-env".into(),
        }
    }
}

// ── Plugins ─────────────────────────────────────────────────────────────────

/// Where a plugin comes from, which says whether it can be removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PluginOrigin {
    /// It is part of the provisioning source and stays.
    #[default]
    Shipped,
    /// Installed with npm into the plugin directory.
    Npm,
    /// A folder copied into the plugin directory.
    Folder,
}

/// A task a feature adds to a host's overview.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct PluginTask {
    /// The task is `PLUGIN:NAME` on the host.
    pub name: String,
    pub label: String,
    /// `agent`, `operator` or `root`.
    pub user: String,
    /// The agent may run it with `box run`.
    #[serde(default)]
    pub agent: bool,
    /// Tasks of berth it runs after, with no row of its own.
    #[serde(default)]
    pub extends: Vec<String>,
    /// The names of the arguments it asks for.
    #[serde(default)]
    pub arguments: Vec<String>,
}

/// A program a feature keeps running on the host to answer calls.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct PluginService {
    /// The service is `PLUGIN/NAME` on the host.
    pub name: String,
    pub user: String,
    #[serde(default)]
    pub methods: Vec<String>,
}

/// Something a plugin installs, switched on per host.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct PluginFeature {
    /// A profile names it `PLUGIN/ID`.
    pub id: String,
    pub label: String,
    /// The group of the Features page it is listed in.
    pub group: String,
    #[serde(default)]
    pub help: String,
    #[serde(default)]
    pub tasks: Vec<PluginTask>,
    #[serde(default)]
    pub services: Vec<PluginService>,
    /// Channel topics, without the plugin's id.
    #[serde(default)]
    pub channels: Vec<String>,
}

/// A plugin that was found, with what it can install.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct PluginInfo {
    pub id: String,
    pub label: String,
    /// Its npm name.
    pub package: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    pub origin: PluginOrigin,
    /// Where it is on the machine that has it.
    pub dir: String,
    #[serde(default)]
    pub features: Vec<PluginFeature>,
    /// What it can create machines with.
    #[serde(default)]
    pub provisioners: Vec<ProvisionerInfo>,
}

/// The plugins a berth serve has, and where it keeps its own.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct PluginList {
    pub plugins: Vec<PluginInfo>,
    /// The directory of its own plugins; a folder copied there is one too.
    pub dir: String,
}

/// What npm said of a change, a few lines; the first starts with ✓.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct PluginReport {
    pub lines: Vec<String>,
    /// After an update: the hosts whose configuration is now behind.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub behind: Vec<HostBehind>,
}

/// Whether a newer version of a plugin is there, as its registry says.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct PluginUpdate {
    pub id: String,
    pub package: String,
    /// The version installed.
    pub version: String,
    /// The registry's latest version, when it is another one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest: Option<String>,
    /// Why there is nothing to ask, or what asking said: a plugin from a
    /// .tgz or a folder has no registry to ask.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

/// The answer of `plugins.check`: each of the user's plugins.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct PluginUpdates {
    pub plugins: Vec<PluginUpdate>,
}

/// What to update: one plugin by its id or npm name, or all of them.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct PluginUpdateSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

// ── Provisioners ────────────────────────────────────────────────────────────

/// Something a plugin creates machines with, for a saved host's profile.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ProvisionerInfo {
    /// `PLUGIN/ID`.
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub description: String,
    /// `seed`: the machine boots with berth's cloud seed and sets itself up.
    /// `apply`: berth applies the profile over SSH once the machine answers.
    pub setup: String,
    /// What it can do besides `create`, `status` and `destroy`: `start`,
    /// `stop`, `list`, `log`.
    #[serde(default)]
    pub verbs: Vec<String>,
    /// The berth secrets it is given.
    #[serde(default)]
    pub secrets: Vec<String>,
    /// The required ones that have no value. Not looked at in a plugin's listing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_secrets: Vec<String>,
    /// What creating a machine takes: the provisioner's own arguments first,
    /// then those of its machines.
    #[serde(default)]
    pub arguments: Vec<Argument>,
    /// The names of the provisioner's own arguments, which say where it is,
    /// such as an Incus server. Their values are kept as its settings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub own: Vec<String>,
    /// The values of its own arguments that are kept, by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub settings: BTreeMap<String, String>,
    /// What it can make: `machine`, and kinds its plugin names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<ProvisionerKind>,
    /// Its plugin's own actions, such as listing images or pruning.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<Action>,
    /// Why it cannot be used here, as a sentence that says what to change.
    /// Empty when it can.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub problem: String,
}

/// A kind of thing a provisioner makes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ProvisionerKind {
    /// `machine`, or a kind the plugin names, such as `deployment`.
    pub id: String,
    #[serde(default)]
    pub label: String,
    /// For a machine: `seed` or `apply`, as for the provisioner.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub setup: String,
    /// What can be done to one besides `create`, `status` and `destroy`:
    /// `start`, `stop`, `change`, `logs`.
    #[serde(default)]
    pub verbs: Vec<String>,
    #[serde(default)]
    pub arguments: Vec<Argument>,
    /// The specification one is made from, for a kind that has one: its
    /// `format` and `label`, as the plugin declares them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<Value>,
    /// What its plugin can do to one, each run with the verb `act` and the
    /// resource's id.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<Action>,
}

/// A row of what a provisioner says, shown as it is given.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Detail {
    pub label: String,
    #[serde(default)]
    pub value: String,
}

/// What a provisioner lists of what it holds, beside berth's records.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ListedResource {
    /// The provisioner's id of it; empty for a record whose `create` did not end.
    #[serde(default)]
    pub id: String,
    /// `machine`, or a kind the plugin names, such as `deployment`.
    pub kind: String,
    pub name: String,
    /// The host a machine was made for, when it says.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub host: String,
    /// What the provisioner's list says, or empty when it was not asked.
    #[serde(default)]
    pub state: String,
    /// berth's id of it, when berth has a record of it. One the provisioner
    /// lists without one is not berth's, or berth lost track of it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub resource: String,
}

/// A provisioner's page: what it is, what it says of itself now, and what
/// it holds.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ProvisionerState {
    pub provisioner: ProvisionerInfo,
    /// `ready`, `unreachable` or `unusable`, or what the provisioner said.
    pub state: String,
    /// What it said of itself, or why it could not be asked.
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub details: Vec<Detail>,
    /// The values an argument takes here, by its name, where the
    /// provisioner narrows what it declares, such as the pools there are.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub values: BTreeMap<String, Vec<String>>,
    /// berth's records of what it made, with what its own list says.
    #[serde(default)]
    pub resources: Vec<ListedResource>,
}

/// A berth secret a resource is given in its environment, by name. Its
/// value is resolved when the provisioner is called and never kept with
/// the resource.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ResourceSecret {
    /// The name of the berth secret.
    pub name: String,
    /// The environment variable it becomes; the secret's name when empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub env: String,
}

impl ResourceSecret {
    /// The variable it is given as.
    pub fn variable(&self) -> &str {
        match self.env.is_empty() {
            true => &self.name,
            false => &self.env,
        }
    }
}

/// Something a provisioner made for berth: a machine, which may be a
/// host's, or a kind its plugin names, such as a deployment.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Resource {
    /// berth's id of it, stable for the life of the record.
    pub id: String,
    /// `PLUGIN/ID`.
    pub provisioner: String,
    /// `machine`, or a kind the plugin declares.
    pub kind: String,
    /// Unique per provisioner and kind.
    pub name: String,
    /// `creating`, `running`, `stopped`, `missing` or `unknown`, as last seen.
    pub state: String,
    /// The provisioner's id of it; empty until `create` said it.
    #[serde(default)]
    pub external_id: String,
    /// The host a machine belongs to, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub address: String,
    #[serde(default)]
    pub arguments: BTreeMap<String, String>,
    /// The berth secrets it is given, by name; never their values.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<ResourceSecret>,
    /// The SHA-256 of its stored specification, for a kind that has one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub spec_sha256: String,
    /// When it was asked for, RFC 3339.
    #[serde(default)]
    pub created: String,
    /// When its provisioner was last asked about it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub checked: String,
    /// What can be done to it besides being removed: `start`, `stop`,
    /// `change`, `logs`, as its kind declares.
    #[serde(default)]
    pub verbs: Vec<String>,
    /// Why its state is `unknown`, or what is wrong with it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub problem: String,
}

/// What to create a resource with.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct NewResource {
    /// `PLUGIN/ID`.
    pub provisioner: String,
    /// `machine` when empty.
    #[serde(default)]
    pub kind: String,
    /// The host's name for a host's machine when empty.
    #[serde(default)]
    pub name: String,
    /// The saved host a machine is made for, and set up as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Values of the provisioner's and the kind's arguments; the others
    /// take their settings or defaults.
    #[serde(default)]
    pub arguments: BTreeMap<String, String>,
    /// The specification, for a kind that has one, as its plugin reads it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<ResourceSecret>,
    /// Any text of the caller's. The same token again answers with the
    /// resource and the job the first call started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

/// What to change of a resource: what is given replaces what it had.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ResourceChange {
    /// Values of the kind's arguments, over those it has.
    #[serde(default)]
    pub arguments: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets: Option<Vec<ResourceSecret>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

/// What creating a resource answers: its record and the job that makes it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct Created {
    pub resource: Resource,
    pub job: JobInfo,
}

// ── What a host was applied from ────────────────────────────────────────────

/// A plugin as it was when a host's configuration was applied.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AppliedPlugin {
    pub id: String,
    pub package: String,
    pub version: String,
    /// SHA-256 of its files, so a change without a new version shows too.
    pub digest: String,
}

/// What berth recorded on a host when it applied the configuration: its own
/// version and the plugins whose features the host has.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Applied {
    pub berth: String,
    #[serde(default)]
    pub plugins: Vec<AppliedPlugin>,
}

/// The profile a host keeps of itself, at `/etc/berth/profile.json`: the
/// profile as berth holds it, without the enrollment key and without its
/// `access`, how the berth that wrote it reaches the host. A berth that does
/// not have the host can take it over from this.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct KeptProfile {
    /// The format of this document: 2, or 1 for one berth 0.4.1 wrote, whose
    /// profile has the layout before `access`.
    pub format: u64,
    /// The version of the berth that wrote it.
    pub berth: String,
    /// When it was written, in RFC 3339.
    #[serde(default)]
    pub written: String,
    /// The name the host has in the berth that wrote it.
    pub host: String,
    /// The profile.
    pub profile: Value,
    /// The record of the machine berth created for the host, as a resource,
    /// when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<Value>,
    /// The same in a document of format 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<Value>,
}

/// Something that changed since a host was applied: berth itself or a plugin.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Change {
    /// `berth`, or a plugin's id.
    pub id: String,
    /// The version the host was applied with.
    pub from: String,
    /// The version there is now; empty for a plugin that is gone, the same
    /// as `from` for one whose files changed.
    pub to: String,
}

impl Change {
    /// `claude 1.0.0 → 1.1.0`, `claude 1.0.0 (changed)`, `claude 1.0.0 (gone)`.
    pub fn text(&self) -> String {
        match self.to.as_str() {
            "" => format!("{} {} (gone)", self.id, self.from),
            to if to == self.from => format!("{} {} (changed)", self.id, self.from),
            to => format!("{} {} → {to}", self.id, self.from),
        }
    }
}

/// How a host's configuration differs from what berth would apply now.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Behind {
    /// berth first when its version changed, then the plugins.
    pub changes: Vec<Change>,
}

impl Behind {
    /// Compare what was applied with berth's version and plugins now.
    /// Nothing when they are the same.
    pub fn compare(applied: &Applied, berth: &str, now: &[AppliedPlugin]) -> Option<Behind> {
        let mut changes = vec![];
        if applied.berth != berth {
            changes.push(Change {
                id: "berth".into(),
                from: applied.berth.clone(),
                to: berth.into(),
            });
        }
        for was in &applied.plugins {
            match now.iter().find(|plugin| plugin.id == was.id) {
                Some(is) if is.digest == was.digest => {}
                is => changes.push(Change {
                    id: was.id.clone(),
                    from: was.version.clone(),
                    to: is.map(|is| is.version.clone()).unwrap_or_default(),
                }),
            }
        }
        (!changes.is_empty()).then_some(Behind { changes })
    }

    /// `claude 1.0.0 → 1.1.0, berth 0.3.0 → 0.4.0`.
    pub fn text(&self) -> String {
        self.changes
            .iter()
            .map(Change::text)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// A host whose configuration is behind, and how.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct HostBehind {
    pub name: String,
    #[serde(flatten)]
    pub behind: Behind,
}

// ── Rolling out ─────────────────────────────────────────────────────────────

/// What a roll-out does: apply the hosts that are behind one after another,
/// stopping at the first that fails.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Rollout {
    /// These hosts, in this order; all saved hosts by name when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<String>,
    /// Also the hosts that are current.
    #[serde(default)]
    pub all: bool,
    /// Run each host's `update` task after applying.
    #[serde(default)]
    pub update: bool,
    /// Reboot a host that says it needs it, and wait for it to come back.
    #[serde(default)]
    pub reboot: bool,
    /// Reboot although sessions run, which ends them.
    #[serde(default)]
    pub force: bool,
}

impl Rollout {
    /// The arguments of the `rollout` action that ask for this.
    pub fn arguments(&self) -> BTreeMap<String, String> {
        let word = |on: bool| if on { "yes" } else { "no" }.to_owned();
        let mut arguments = BTreeMap::from([
            ("all".to_owned(), word(self.all)),
            ("update".to_owned(), word(self.update)),
            ("reboot".to_owned(), word(self.reboot)),
            ("force".to_owned(), word(self.force)),
        ]);
        if !self.hosts.is_empty() {
            arguments.insert("hosts".into(), self.hosts.join(","));
        }
        arguments
    }

    /// What the arguments of the `rollout` action ask for.
    pub fn from_arguments(arguments: &BTreeMap<String, String>) -> Result<Rollout, String> {
        let flag = |name: &str| match arguments.get(name).map(|value| value.trim()) {
            None | Some("" | "no" | "false") => Ok(false),
            Some("yes" | "true") => Ok(true),
            Some(other) => Err(format!("{name} is yes or no, not {other}")),
        };
        let hosts = arguments.get("hosts").map_or(vec![], |hosts| {
            hosts
                .split(',')
                .map(str::trim)
                .filter(|host| !host.is_empty())
                .map(str::to_owned)
                .collect()
        });
        let rollout = Rollout {
            hosts,
            all: flag("all")?,
            update: flag("update")?,
            reboot: flag("reboot")?,
            force: flag("force")?,
        };
        if rollout.force && !rollout.reboot {
            return Err("force only goes with reboot".into());
        }
        Ok(rollout)
    }
}

/// How one host of a roll-out went.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RolloutState {
    /// Applied, and healthy afterwards.
    Done,
    /// Nothing to do: it was current.
    Current,
    /// Not started, because an earlier host stopped the roll-out, or left
    /// because something else was changing it, as its `error` says.
    #[default]
    Skipped,
    /// Something went wrong; the roll-out stopped here.
    Failed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RolloutHost {
    pub name: String,
    pub state: RolloutState,
    /// What had changed, as it was before applying.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<Change>,
    /// It was rebooted.
    #[serde(default)]
    pub rebooted: bool,
    /// What went wrong, for a failed host.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    /// What did not go well without being a failure, such as an update step
    /// that ended badly. One stops the roll-out after this host.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Whether a line of a roll-out's output is one of its own, rather than
/// what Ansible or a task wrote: those start with ▸, ✓, ! or ✗.
pub fn rollout_line(line: &str) -> bool {
    ["▸ ", "✓ ", "! ", "✗ "]
        .iter()
        .any(|start| line.starts_with(start))
}

/// The result of a roll-out, host by host in the order they were taken.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RolloutReport {
    pub hosts: Vec<RolloutHost>,
    /// Every host is done or current.
    pub ok: bool,
}
