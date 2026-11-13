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
    /// `stop`, `list`.
    #[serde(default)]
    pub verbs: Vec<String>,
    /// The berth secrets it is given.
    #[serde(default)]
    pub secrets: Vec<String>,
    /// The required ones that have no value. Not looked at in a plugin's listing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_secrets: Vec<String>,
    #[serde(default)]
    pub arguments: Vec<Argument>,
}

/// What to create a host's machine with.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Provision {
    /// `PLUGIN/ID`.
    pub provisioner: String,
    /// Values of its arguments; the others take their defaults.
    #[serde(default)]
    pub arguments: BTreeMap<String, String>,
}

/// The machine berth created for a host.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct Instance {
    /// `PLUGIN/ID`.
    pub provisioner: String,
    /// The provisioner's id of the machine; empty until `create` said it.
    pub id: String,
    /// `running`, `stopped`, `gone`, `unknown` or `creating`.
    pub state: String,
    #[serde(default)]
    pub address: String,
    /// When it was asked for, RFC 3339.
    #[serde(default)]
    pub created: String,
    #[serde(default)]
    pub arguments: BTreeMap<String, String>,
    /// What can be done to it besides `destroy`: `start`, `stop`.
    #[serde(default)]
    pub verbs: Vec<String>,
    /// Why the state is `unknown`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub problem: String,
}

/// A machine a provisioner has that berth created.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ListedInstance {
    pub id: String,
    /// The host it was created for, as its labels say.
    pub host: String,
    pub state: String,
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

/// How one host of a roll-out went.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RolloutState {
    /// Applied, and healthy afterwards.
    Done,
    /// Nothing to do: it was current.
    Current,
    /// Not started, because an earlier host stopped the roll-out.
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
