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

#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
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
}
