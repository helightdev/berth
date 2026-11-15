//! The grammar of the command line, `berth THING ACTION [NAME] [FLAGS]`, as
//! one definition that the help, the errors, the shell completions and the
//! reference page of the docs are all made from.

use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

/// What the exit statuses mean, for the help and the reference page.
pub const EXIT_STATUSES: &[(i32, &str)] = &[
    (0, "done"),
    (1, "the work failed: a job failed, or a host did not answer"),
    (
        2,
        "the command line was wrong: an unknown word, a missing value, a bad flag",
    ),
    (
        3,
        "refused: the role is too low, the host or this server cannot do it now, or a confirmation was declined",
    ),
    (4, "the server could not be reached"),
];

#[derive(Parser, Debug)]
#[command(
    name = "berth",
    version = berth_api::VERSION,
    about = "berth sets up and looks after agent workstations.",
    disable_help_subcommand = false,
    propagate_version = true,
    max_term_width = 100
)]
pub struct Cli {
    #[command(flatten)]
    pub global: Global,
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// The flags every command takes.
#[derive(Args, Debug, Clone, Default)]
pub struct Global {
    /// Use this berth server for this command: https://HOST, http://HOST:PORT or unix:/path. berth login keeps one.
    #[arg(long, global = true, value_name = "URL", help_heading = "Global")]
    pub server: Option<String>,
    /// Read the server's token from this file.
    #[arg(long, global = true, value_name = "FILE", help_heading = "Global")]
    pub token_file: Option<PathBuf>,
    /// Read the server's token from standard input.
    #[arg(long, global = true, help_heading = "Global")]
    pub token_stdin: bool,
    /// What is printed on standard output: a table for people, or the API's own JSON.
    #[arg(short = 'o', long, global = true, value_enum, default_value_t = Output::Table, help_heading = "Global")]
    pub output: Output,
    /// Never ask. What is missing is an error that names the flag to pass.
    #[arg(long, global = true, help_heading = "Global")]
    pub no_input: bool,
    /// Answer yes to a confirmation.
    #[arg(short = 'y', long, global = true, help_heading = "Global")]
    pub yes: bool,
    /// Print only the result.
    #[arg(short = 'q', long, global = true, help_heading = "Global")]
    pub quiet: bool,
    /// Colours: on a terminal, always or never. NO_COLOR turns them off as well.
    #[arg(long, global = true, value_enum, default_value_t = Colour::Auto, value_name = "WHEN", help_heading = "Global")]
    pub color: Colour,
}

#[derive(ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Output {
    #[default]
    Table,
    Json,
}

#[derive(ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Colour {
    #[default]
    Auto,
    Always,
    Never,
}

/// The flag of a command that starts a job.
#[derive(Args, Debug, Clone, Default)]
pub struct Detach {
    /// Print the job's id and return; berth job wait ID waits for it. Needs a berth serve.
    #[arg(long)]
    pub detach: bool,
}

/// How a sudo password is given to a command that may need one.
#[derive(Args, Debug, Clone, Default)]
pub struct Sudo {
    /// Read the SSH user's sudo password from standard input, for a host whose sudo asks for one.
    #[arg(long)]
    pub sudo_password_stdin: bool,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Short for berth host list.
    Ls,
    /// Short for berth host apply.
    Apply(HostApply),
    /// Run a command on a host and pass its exit status through.
    Exec(Exec),
    /// A shell on a host over plain SSH, or a command run there. Needs the installation on this machine.
    Ssh(Ssh),
    /// Run one of a host's actions: berth action list HOST names them.
    Run(RunAction),
    /// Open berth's screens: the home screen, or the page of one host.
    Ui {
        /// The host to open.
        host: Option<String>,
    },
    /// The hosts: list, create, add, change, install and remove them.
    #[command(subcommand)]
    Host(Host),
    /// Bring the hosts up to date, one at a time.
    Rollout(Rollout),
    /// What a host can do.
    #[command(subcommand)]
    Action(ActionCommand),
    /// The terminal and process sessions of a host.
    #[command(subcommand)]
    Session(Session),
    /// What can make hosts and resources, such as Incus or Docker.
    #[command(subcommand)]
    Provisioner(Provisioner),
    /// What provisioners made that is not a host, such as a deployment.
    #[command(subcommand)]
    Resource(Resource),
    /// The work the server does: applies, installers, roll-outs.
    #[command(subcommand)]
    Job(Job),
    /// The SSH keys berth logs in to hosts with.
    #[command(subcommand)]
    Key(Key),
    /// Secret values the server keeps and gives to hosts.
    #[command(subcommand)]
    Secret(Secret),
    /// Registries: Nexus servers, npm, file and Docker registries.
    #[command(subcommand)]
    Registry(Registry),
    /// Plugins and what they install.
    #[command(subcommand)]
    Plugin(Plugin),
    /// The settings: the server's, and this berth's own, such as the server it uses.
    #[command(subcommand)]
    Config(Config),
    /// Reach a port of a host, or an endpoint of a resource, from this machine.
    #[command(subcommand)]
    Port(Port),
    /// Messages taken in from the hosts' channels.
    #[command(subcommand)]
    Inbox(Inbox),
    /// The server's key-value store.
    #[command(subcommand)]
    Kv(Kv),
    /// A host's channel.
    #[command(subcommand)]
    Channel(Channel),
    /// Services that plugins run on a host.
    #[command(subcommand)]
    Service(Service),
    /// The berth server of this machine: running it, its setup key, its store and its API documents.
    #[cfg(target_os = "linux")]
    #[command(subcommand)]
    Server(Server),
    /// Short for berth server run.
    #[cfg(target_os = "linux")]
    Serve(ServerRun),
    /// Use the berth server at URL, with a setup key made there.
    Login(Login),
    /// Stop using the berth server berth login kept.
    Logout,
    /// Install the latest release in this program's place.
    Update(Update),
    /// The version of this berth, and of the server it uses.
    Version,
    /// The completions of a shell, to source from its start-up file.
    Completion {
        /// bash, zsh, fish, elvish or powershell.
        shell: clap_complete::Shell,
    },
    /// ssh's askpass helper.
    #[command(hide = true)]
    Askpass { prompt: Vec<String> },
    /// A one-use enrollment key for a host, which an apply asks for.
    #[cfg(target_os = "linux")]
    #[command(hide = true)]
    TailnetKey { host: String },
}

// ── host ────────────────────────────────────────────────────────────────────

#[derive(Subcommand, Debug)]
pub enum Host {
    /// The hosts, with their address and whether each answers.
    #[command(visible_alias = "ls")]
    List,
    /// One host: its type, address, state and actions.
    Show { name: String },
    /// Describe a new host, then install it. Asks what is missing on a terminal.
    #[command(after_long_help = super::setup::keys_help())]
    Create(HostCreate),
    /// Take in a machine that exists, by its address, and log in to it once.
    Add(HostAdd),
    /// Change settings of a host's profile: KEY=VALUE, as berth host create --help lists the keys.
    #[command(after_long_help = super::setup::keys_help())]
    Set(HostSet),
    /// Change a host's profile in $EDITOR.
    Edit { name: String },
    /// A host's profile as JSON on standard output.
    Export { name: String },
    /// Save the profile in FILE (- for standard input) as the host's, new or changed.
    Import {
        name: String,
        #[arg(value_name = "FILE")]
        file: String,
    },
    /// Apply the host's profile to it over SSH, or install its controller alone when it is managed.
    Apply(HostApply),
    /// The cloud-init user data that installs the host on first boot, on standard output.
    Seed {
        name: String,
        /// Write it to FILE instead.
        #[arg(long, value_name = "FILE")]
        file: Option<PathBuf>,
    },
    /// Build the host's unattended USB installer where the server runs, and write it to a drive.
    Iso(HostIso),
    /// Give a host another name in berth. The machine is left as it is.
    Rename {
        name: String,
        new: String,
        /// Make NEW its profile's hostname as well, for the next apply.
        #[arg(long)]
        hostname: bool,
    },
    /// Forget a host. The machine is left as it is, unless --destroy.
    #[command(visible_alias = "rm")]
    Remove {
        name: String,
        /// Delete the machine a provisioner made for it as well.
        #[arg(long)]
        destroy: bool,
    },
    /// Take in a host berth set up before, from the profile it keeps.
    Adopt(HostAdopt),
    /// The passwords berth generated for a host's accounts.
    Credentials {
        name: String,
        /// Print them even when standard output is no terminal.
        #[arg(long)]
        show: bool,
    },
    /// Check the host's SSH host key, and accept a new one.
    Verify {
        name: String,
        /// Accept the key the host shows now.
        #[arg(long)]
        accept: bool,
    },
    /// Start the machine a provisioner made for the host.
    Start(HostPower),
    /// Stop the machine a provisioner made for the host.
    Stop(HostPower),
    /// The hosts as an Ansible inventory. Needs the installation on this machine.
    #[cfg(target_os = "linux")]
    Inventory { names: Vec<String> },
}

#[derive(Args, Debug, Clone, Default)]
pub struct HostCreate {
    pub name: String,
    /// How it is installed: provisioner, iso, cloud-seed, ssh or ssh-only.
    #[arg(long, value_name = "METHOD")]
    pub method: Option<String>,
    /// Start from a profile: a host's name, a JSON file, or - for standard input.
    #[arg(long, value_name = "PROFILE")]
    pub from: Option<String>,
    /// What berth does with it: workstation, or node for a server of machines, applied over SSH.
    #[arg(long = "type", value_name = "TYPE")]
    pub host_type: Option<String>,
    /// Switch on a plugin's feature, PLUGIN/FEATURE. Again for more.
    #[arg(long, value_name = "FEATURE")]
    pub feature: Vec<String>,
    /// Switch off a plugin's feature.
    #[arg(long, value_name = "FEATURE")]
    pub no_feature: Vec<String>,
    /// Set a field of the profile. Again for more; the keys are listed below.
    #[arg(long = "set", value_name = "KEY=VALUE")]
    pub set: Vec<String>,
    /// Generate the accounts' passwords; berth host credentials NAME shows them.
    #[arg(long)]
    pub generate_passwords: bool,
    /// Make the machine with this provisioner, which also sets it up.
    #[arg(long, value_name = "PROVISIONER")]
    pub provisioner: Option<String>,
    /// The provisioner's arguments, such as cpus=2.
    #[arg(value_name = "NAME=VALUE")]
    pub arguments: Vec<String>,
    /// Install it once it is saved: apply, render the seed or build the installer.
    #[arg(long)]
    pub run: bool,
    #[command(flatten)]
    pub detach: Detach,
    #[command(flatten)]
    pub sudo: Sudo,
}

#[derive(Args, Debug, Clone)]
pub struct HostAdd {
    pub name: String,
    /// Its name or IP address, or a Host alias of the server's SSH configuration.
    pub address: String,
    /// The account berth logs in as.
    #[arg(long)]
    pub user: Option<String>,
    #[arg(long)]
    pub port: Option<String>,
    /// How berth logs in: berth, key:NAME, password or ssh.
    #[arg(long, value_name = "AUTH")]
    pub auth: Option<String>,
    /// What berth does with it: ssh, managed, node or workstation.
    #[arg(long = "type", value_name = "TYPE", default_value = "ssh")]
    pub host_type: String,
    /// Switch on a plugin's feature of a node or a workstation, PLUGIN/FEATURE, such as incus/server. Again for more.
    #[arg(long, value_name = "FEATURE")]
    pub feature: Vec<String>,
    /// Save it without logging in to it.
    #[arg(long)]
    pub no_contact: bool,
    /// Read a password from standard input that installs berth's key on it, once.
    #[arg(long)]
    pub password_stdin: bool,
}

#[derive(Args, Debug, Clone)]
pub struct HostSet {
    pub name: String,
    /// KEY=VALUE, where KEY is a field (berth host create --help), feature.PLUGIN/FEATURE, secret.NAME or registry.NAME.
    #[arg(value_name = "KEY=VALUE", required = true)]
    pub values: Vec<String>,
}

#[derive(Args, Debug, Clone, Default)]
pub struct HostApply {
    pub name: String,
    #[command(flatten)]
    pub sudo: Sudo,
    #[command(flatten)]
    pub detach: Detach,
}

#[derive(Args, Debug, Clone)]
pub struct HostIso {
    pub name: String,
    /// Write it to this USB drive, which is erased, such as /dev/sdb.
    #[arg(long, value_name = "DEVICE")]
    pub write: Option<String>,
    /// Read your sudo password for writing the drive from standard input.
    #[arg(long)]
    pub password_stdin: bool,
    #[command(flatten)]
    pub detach: Detach,
}

#[derive(Args, Debug, Clone)]
pub struct HostAdopt {
    pub name: String,
    /// [USER@]ADDRESS; without it, the saved host of that name.
    pub address: Option<String>,
    #[arg(long)]
    pub port: Option<String>,
    /// A private key file where the server runs, offered as well for reading the profile only.
    #[arg(long, value_name = "FILE")]
    pub key: Option<String>,
    /// Replace a host of that name.
    #[arg(long)]
    pub force: bool,
    #[command(flatten)]
    pub sudo: Sudo,
}

#[derive(Args, Debug, Clone)]
pub struct HostPower {
    pub name: String,
    #[command(flatten)]
    pub detach: Detach,
}

#[derive(Args, Debug, Clone)]
pub struct Exec {
    pub host: String,
    /// The account it runs as: agent, operator or root.
    #[arg(long, default_value = "agent")]
    pub user: String,
    /// Start it and return; berth session logs HOST ID shows what it writes.
    #[arg(long)]
    pub detach: bool,
    /// Keep the session once it has ended, for berth session logs.
    #[arg(long)]
    pub keep: bool,
    /// The command and its arguments, after --.
    #[arg(last = true, required = true, value_name = "COMMAND")]
    pub command: Vec<String>,
}

#[derive(Args, Debug, Clone)]
pub struct Ssh {
    pub host: String,
    /// Log in as the agent account.
    #[arg(long)]
    pub agent: bool,
    /// A command to run there, after --.
    #[arg(last = true, value_name = "COMMAND")]
    pub command: Vec<String>,
}

#[derive(Args, Debug, Clone)]
pub struct RunAction {
    pub host: String,
    pub action: String,
    /// The action's arguments.
    #[arg(value_name = "NAME=VALUE")]
    pub arguments: Vec<String>,
    #[command(flatten)]
    pub sudo: Sudo,
    #[command(flatten)]
    pub detach: Detach,
}

#[derive(Args, Debug, Clone, Default)]
pub struct Rollout {
    /// Only these hosts. Again for more.
    #[arg(long, value_name = "HOST")]
    pub only: Vec<String>,
    /// Leave these hosts out. Again for more.
    #[arg(long, value_name = "HOST")]
    pub except: Vec<String>,
    /// Take hosts that are up to date as well.
    #[arg(long)]
    pub all: bool,
    /// Run each host's update action as well.
    #[arg(long)]
    pub update: bool,
    /// Reboot the hosts that need it, unless sessions run there.
    #[arg(long)]
    pub reboot: bool,
    /// With --reboot: even when sessions run.
    #[arg(long, requires = "reboot")]
    pub force: bool,
    #[command(flatten)]
    pub detach: Detach,
}

#[derive(Subcommand, Debug)]
pub enum ActionCommand {
    /// What a host can do, or the installation without a host.
    #[command(visible_alias = "ls")]
    List { host: Option<String> },
}

#[derive(Subcommand, Debug)]
pub enum Session {
    /// A host's sessions, running and ended.
    #[command(visible_alias = "ls")]
    List { host: String },
    /// This terminal onto a terminal session, or onto a new shell as the agent. Ctrl+Q leaves it running.
    Attach { host: String, id: Option<String> },
    /// What a session wrote.
    Logs {
        host: String,
        id: String,
        /// Go on as it writes more.
        #[arg(short = 'f', long)]
        follow: bool,
        /// Only the last N lines.
        #[arg(long, value_name = "N")]
        tail: Option<u64>,
    },
    /// End a session.
    Stop { host: String, id: String },
}

// ── provisioners and resources ──────────────────────────────────────────────

#[derive(Subcommand, Debug)]
pub enum Provisioner {
    /// The provisioners, and whether each can be used here.
    #[command(visible_alias = "ls")]
    List,
    /// The types of provisioner the plugins bring.
    Types,
    /// Register a provisioner of a type, such as incus/vm, under a name of its own.
    Add {
        name: String,
        #[arg(value_name = "TYPE")]
        kind: String,
        /// Its settings.
        #[arg(value_name = "NAME=VALUE")]
        settings: Vec<String>,
    },
    /// How it is now and what it holds. NAME=VALUE gives one of its settings another value for this once.
    Show {
        name: String,
        #[arg(value_name = "NAME=VALUE")]
        settings: Vec<String>,
    },
    /// Change its settings.
    Set {
        name: String,
        #[arg(value_name = "NAME=VALUE", required = true)]
        settings: Vec<String>,
    },
    /// Forget a provisioner that holds nothing.
    #[command(visible_alias = "rm")]
    Remove { name: String },
    /// What it can do.
    Actions { name: String },
    /// Run one of its actions.
    Run {
        name: String,
        action: String,
        #[arg(value_name = "NAME=VALUE")]
        arguments: Vec<String>,
        #[command(flatten)]
        detach: Detach,
    },
}

#[derive(Subcommand, Debug)]
pub enum Resource {
    /// What provisioners made.
    #[command(visible_alias = "ls")]
    List {
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        provisioner: Option<String>,
        #[arg(long)]
        host: Option<String>,
    },
    /// One of them as its provisioner says it is now.
    Show { id: String },
    /// Make a resource, such as a deployment from a Compose file.
    Create(ResourceCreate),
    /// Change a resource: other arguments, a new specification, other secrets.
    Set(ResourceSet),
    /// Remove a resource with its provisioner.
    #[command(visible_alias = "rm")]
    Remove {
        id: String,
        #[command(flatten)]
        detach: Detach,
    },
    /// What it wrote.
    Logs {
        id: String,
        #[arg(short = 'f', long)]
        follow: bool,
        #[arg(long, value_name = "N")]
        tail: Option<u64>,
    },
    /// Run a command in it, for a kind that has exec, and exit with its exit status.
    Exec {
        id: String,
        /// Give the command what berth reads on standard input, to its end.
        #[arg(long)]
        stdin: bool,
        /// The command and its arguments, after --.
        #[arg(last = true, required = true, value_name = "COMMAND")]
        command: Vec<String>,
        #[command(flatten)]
        detach: Detach,
    },
    /// What it can do.
    Actions { id: String },
    /// Run one of its actions.
    Run {
        id: String,
        action: String,
        #[arg(value_name = "NAME=VALUE")]
        arguments: Vec<String>,
        #[command(flatten)]
        detach: Detach,
    },
}

#[derive(Args, Debug, Clone)]
pub struct ResourceCreate {
    pub name: String,
    #[arg(long, required = true)]
    pub provisioner: String,
    /// Its kind, where the provisioner makes several.
    #[arg(long)]
    pub kind: Option<String>,
    /// The specification, such as a Compose file; - for standard input.
    #[arg(long, value_name = "FILE")]
    pub spec: Option<String>,
    /// A secret it gets, NAME or NAME:VARIABLE. Again for more.
    #[arg(long, value_name = "NAME[:VARIABLE]")]
    pub secret: Vec<String>,
    #[arg(value_name = "NAME=VALUE")]
    pub arguments: Vec<String>,
    #[command(flatten)]
    pub detach: Detach,
}

#[derive(Args, Debug, Clone)]
pub struct ResourceSet {
    pub id: String,
    #[arg(long, value_name = "FILE")]
    pub spec: Option<String>,
    #[arg(long, value_name = "NAME[:VARIABLE]")]
    pub secret: Vec<String>,
    #[arg(value_name = "NAME=VALUE")]
    pub arguments: Vec<String>,
    #[command(flatten)]
    pub detach: Detach,
}

// ── jobs ────────────────────────────────────────────────────────────────────

#[derive(Subcommand, Debug)]
pub enum Job {
    /// The jobs, newest first.
    #[command(visible_alias = "ls")]
    List {
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// One job's state.
    Show { id: String },
    /// What a job wrote.
    Logs {
        id: String,
        /// Go on as it writes more, until it ends.
        #[arg(short = 'f', long)]
        follow: bool,
    },
    /// Wait for a job to end and exit with its result.
    Wait { id: String },
    /// Stop a running job: its programs end, and its record says who stopped it.
    Stop { id: String },
}

// ── keys, secrets, registries, plugins, settings ────────────────────────────

#[derive(Subcommand, Debug)]
pub enum Key {
    /// The keys of berth's key ring, by their public halves.
    #[command(visible_alias = "ls")]
    List,
    /// One key's public half, which a host's operator takes.
    Show {
        #[arg(default_value = "berth")]
        name: String,
    },
    /// Make a new ed25519 key.
    Create { name: String },
    /// Add an OpenSSH private key without a passphrase from FILE (- for standard input).
    Import {
        name: String,
        #[arg(value_name = "FILE")]
        file: String,
    },
    /// Remove a key no host logs in with.
    #[command(visible_alias = "rm")]
    Remove { name: String },
    /// Replace a key on every host that takes it.
    Rotate {
        name: String,
        #[command(flatten)]
        detach: Detach,
    },
    /// Install a key on a host that does not take it yet, with a password used once.
    Trust {
        host: String,
        /// Log in once with this key of the ring, to install the one the host logs in with.
        #[arg(long, value_name = "NAME")]
        key: Option<String>,
        /// Read the password from standard input.
        #[arg(long)]
        password_stdin: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum Secret {
    /// The names of the secrets.
    #[command(visible_alias = "ls")]
    List,
    /// Set a secret's value: asked for, or read from a file or standard input.
    Set {
        name: String,
        /// Read the value from FILE.
        #[arg(long, value_name = "FILE")]
        value_file: Option<PathBuf>,
    },
    /// Remove a secret.
    #[command(visible_alias = "rm")]
    Remove { name: String },
}

#[derive(Subcommand, Debug)]
pub enum Registry {
    /// The registries.
    #[command(visible_alias = "ls")]
    List,
    /// Add a registry, or change its address or user.
    Add {
        name: String,
        /// nexus, npm, raw or docker.
        #[arg(value_name = "TYPE")]
        kind: String,
        url: String,
        /// The user of its login; without one the login is a token.
        #[arg(long)]
        user: Option<String>,
    },
    /// Add a repository of a Nexus registry.
    AddRepo {
        name: String,
        /// npm, raw or docker.
        format: String,
        repository: String,
        /// Its own address, HOST[:PORT], for a Docker repository.
        #[arg(long)]
        address: Option<String>,
    },
    /// Keep the password or token of a registry.
    Login {
        name: String,
        /// Read it from standard input.
        #[arg(long)]
        password_stdin: bool,
    },
    /// Whether a registry answers and takes its login.
    Check { name: Option<String> },
    /// Remove a registry with its login.
    #[command(visible_alias = "rm")]
    Remove { name: String },
    /// Upload the release archive to a raw repository or file server, and use it as HOST's release address.
    #[cfg(target_os = "linux")]
    Publish {
        /// REGISTRY or REGISTRY/REPOSITORY.
        destination: String,
        #[arg(long)]
        host: Option<String>,
        /// The cloud seed carries this registry's login; the destination's without a name.
        #[arg(long, value_name = "REGISTRY", num_args = 0..=1, default_missing_value = "")]
        login: Option<String>,
    },
    /// Push berth's container image.
    #[cfg(target_os = "linux")]
    PushImage {
        /// REGISTRY or REGISTRY/REPOSITORY.
        destination: String,
        #[arg(default_value = "latest")]
        tag: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum Plugin {
    /// The plugins and where they come from.
    #[command(visible_alias = "ls")]
    List,
    /// What a plugin brings: its features with their actions, services and channels, and its provisioners.
    Show { id: String },
    /// Install a plugin with npm: a name, or the address of a tarball.
    Add { spec: String },
    /// Remove one of the server's own plugins.
    #[command(visible_alias = "rm")]
    Remove { id: String },
    /// Update one plugin or all to their registry's latest version.
    Update {
        id: Option<String>,
        /// Only say which have a newer version.
        #[arg(long)]
        check: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum Config {
    /// The settings, with what each means and where it is kept.
    #[command(visible_alias = "ls")]
    List,
    /// One setting's value.
    Get { key: String },
    /// Change a setting.
    Set { key: String, value: String },
    /// Set a setting back to its default.
    Unset { key: String },
}

#[derive(Subcommand, Debug)]
pub enum Port {
    /// Listen on this machine and pass each connection to a port of a host, or to an endpoint of a resource.
    ///
    /// TARGET is a host's name. A resource's id or name is taken when no host has that name, or always with --resource.
    Forward {
        #[arg(value_name = "TARGET")]
        target: String,
        /// [LOCAL:]PORT for a host's port. For a resource, [LOCAL:]ENDPOINT, an endpoint's name or port; LOCAL is the endpoint's port unless given.
        #[arg(value_name = "[LOCAL:]PORT|ENDPOINT", required = true)]
        ports: Vec<String>,
        /// TARGET is a resource, even where a host has the same name.
        #[arg(long)]
        resource: bool,
        /// The address to listen on.
        #[arg(long, default_value = "127.0.0.1")]
        address: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum Inbox {
    /// The messages, oldest first.
    #[command(visible_alias = "ls")]
    List {
        host: Option<String>,
        /// Only topics that start with PREFIX.
        #[arg(long, value_name = "PREFIX")]
        topic: Option<String>,
        /// Done ones as well.
        #[arg(long)]
        all: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum Kv {
    /// The keys of a namespace with their values.
    #[command(visible_alias = "ls")]
    List { namespace: String },
    /// One key's value.
    Get { namespace: String, key: String },
    /// Write a key: JSON, or else text.
    Set {
        namespace: String,
        key: String,
        value: String,
    },
    /// Remove a key.
    #[command(visible_alias = "rm")]
    Remove { namespace: String, key: String },
}

#[derive(Subcommand, Debug)]
pub enum Channel {
    /// Messages on a host's channel as they come, one JSON object per line.
    Listen {
        host: String,
        /// Only topics that start with PREFIX.
        #[arg(long, value_name = "PREFIX")]
        topic: Option<String>,
        /// From after this message on.
        #[arg(long, value_name = "ID")]
        since: Option<u64>,
    },
}

#[derive(Subcommand, Debug)]
pub enum Service {
    /// Call a method of a service a plugin runs on the host.
    Call {
        host: String,
        service: String,
        method: String,
        /// The body, as JSON.
        body: Option<String>,
    },
}

// ── the server of this machine ──────────────────────────────────────────────

#[cfg(target_os = "linux")]
#[derive(Subcommand, Debug)]
pub enum Server {
    /// Serve the API over all hosts on a socket of this user, and over HTTP with --listen.
    Run(ServerRun),
    /// Make a key, valid for a day, that lets the first user collect the server's token.
    SetupKey {
        /// End the key that is open.
        #[arg(long)]
        revoke: bool,
    },
    /// Where the server keeps its state, and sealing it.
    #[command(subcommand)]
    Store(Store),
    /// The server's API documents.
    #[command(subcommand)]
    Api(Api),
}

#[cfg(target_os = "linux")]
#[derive(Args, Debug, Clone, Default)]
pub struct ServerRun {
    /// Also listen over HTTP, behind a token.
    #[arg(long, value_name = "ADDRESS:PORT")]
    pub listen: Option<String>,
    /// Allow an address other than this machine's own.
    #[arg(long, requires = "listen")]
    pub expose: bool,
}

#[cfg(target_os = "linux")]
#[derive(Subcommand, Debug)]
pub enum Store {
    /// Where the store is, and whether it is sealed.
    Status,
    /// Seal the store's values with a new key file.
    Encrypt,
    /// Seal them with another key.
    Rekey,
    /// Write them plain again.
    Decrypt,
    /// Copy the store into another SQLite file or PostgreSQL database, and switch to it.
    Copy {
        /// A file, a PostgreSQL URL, or - to read it from standard input.
        target: Option<String>,
        /// Read the target from FILE.
        #[arg(long, value_name = "FILE")]
        target_file: Option<PathBuf>,
        /// Copy into a database that holds a store already.
        #[arg(long)]
        into_existing: bool,
    },
}

#[cfg(target_os = "linux")]
#[derive(Subcommand, Debug)]
pub enum Api {
    /// The OpenAPI document.
    Openapi,
    /// The AsyncAPI document of the streams.
    Asyncapi,
    /// The operations, with their routes and roles.
    Operations,
}

#[derive(Args, Debug, Clone)]
pub struct Login {
    /// https://HOST or http://HOST:PORT.
    pub url: String,
    /// Read the setup key from standard input.
    #[arg(long)]
    pub setup_key_stdin: bool,
}

#[derive(Args, Debug, Clone)]
pub struct Update {
    /// Only say whether there is a newer release.
    #[arg(long)]
    pub check: bool,
    /// Where releases are, instead of berth's own.
    #[arg(long, value_name = "URL")]
    pub from: Option<String>,
}
