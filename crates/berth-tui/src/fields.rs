//! The settings the UI can edit, and the wizard steps and actions around them.

pub const PROFILES: &[&str] = &["laptop", "cloud"];
pub const DISK_MODES: &[&str] = &[
    "auto", "serial", "model", "path", "largest", "smallest", "ssd", "hdd",
];
pub const DISK_TYPES: &[&str] = &["any", "ssd", "hdd"];
/// The ways to log in that are not a key of the ring; `key:NAME` is offered
/// for each of those.
pub const LOGINS: &[&str] = &["berth", "password", "ssh"];
/// The levels a host can be meant to have, the whole profile first.
pub const LEVELS: &[&str] = &["workstation", "controller", "reachable"];

/// The pages of a host: its overview (`Manage`) and the tabs of the set-up
/// flow. Each tab depends only on the ones before it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step {
    /// How the host is installed and reached: what has to be chosen.
    Install,
    Features,
    /// What has a default that follows from the tabs before.
    Settings,
    /// The last few things, every problem, and the buttons.
    Run,
    Manage,
}

impl Step {
    /// The tabs of the set-up flow.
    pub const SETUP: [Step; 4] = [Step::Install, Step::Features, Step::Settings, Step::Run];

    pub fn label(self) -> &'static str {
        match self {
            Step::Install => "Install",
            Step::Features => "Features",
            Step::Settings => "Settings",
            Step::Run => "Run",
            Step::Manage => "Overview",
        }
    }

    /// The tab `delta` places along, wrapping around at the ends.
    pub fn offset(self, delta: isize) -> Step {
        if self == Step::Manage {
            return self;
        }
        let index = (self as isize + delta).rem_euclid(Self::SETUP.len() as isize);
        Self::SETUP[index as usize]
    }
}

/// The rows of a host's overview that are berth's own: name, label and help.
/// Everything else there is a task the host lists itself.
pub const TASKS: &[(&str, &str, &str)] = &[
    (
        "terminal",
        "Terminal",
        "A shell on the host, in a panel here. Space switches between the agent account, with its tools on the PATH and the virtual desktop as its display, and the SSH user. It is a session of the host: Ctrl+Q leaves it running and it can be opened again from Sessions; exit or Ctrl+D ends it. The mouse wheel scrolls back. From the command line: `berth attach HOST`.",
    ),
    (
        "apply",
        "Apply this configuration",
        "Run the installer role against the host again with the settings of this profile, for example after changing what is installed. It also installs or updates berth's own tools on the host. Continues in the terminal.",
    ),
    (
        "connect",
        "Try again",
        "Ask the host again what it can do. A host whose sudo asks for a password needs that password once; it is kept until berth is closed and stored nowhere.",
    ),
    (
        "address",
        "Change the SSH address",
        "Edit the address berth reaches this host at, for a host that has a new one, and try again once it is saved.",
    ),
    (
        "install-controller",
        "Install the controller",
        "Put berthd and box on the host and start berth's controller, so that sessions, tasks and files work there, without the rest of a workstation. The host's level becomes controller. Continues in the terminal. From the command line: `berth host install HOST`.",
    ),
    (
        "ssh",
        "Terminal over SSH",
        "A shell on the host as the SSH user, over plain SSH, for looking at a host whose controller cannot be reached. From the command line: `berth ssh HOST`.",
    ),
];
/// How much the agent account may do with sudo.
pub const PRIVILEGES: &[&str] = &["full", "limited", "none"];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Save,
    Release,
    UsbSeed,
    Iso,
    CloudSeed,
    Machine,
    Apply,
    Docker,
    Ssh,
    Manage,
}

impl Action {
    /// The ways to install a host, offered on the Install tab.
    pub const METHODS: [Action; 6] = [
        Action::Machine,
        Action::Iso,
        Action::CloudSeed,
        Action::Ssh,
        Action::Apply,
        Action::Docker,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Action::Save => "Save configuration",
            Action::Release => "Build release",
            Action::UsbSeed => "Render USB seed",
            Action::Iso => "USB installer",
            Action::CloudSeed => "Cloud-init seed",
            Action::Machine => "Create a machine",
            Action::Apply => "This machine or an inventory",
            Action::Docker => "Container image",
            Action::Ssh => "Existing host over SSH",
            Action::Manage => "Manage an installed host",
        }
    }

    pub fn help(self) -> &'static str {
        match self {
            Action::Save => "Save your selections as a private JSON profile.",
            Action::Release => "Package the provisioning source for distribution.",
            Action::UsbSeed => {
                "Generate the NoCloud files and provisioning bundle for an unattended installer."
            }
            Action::Iso => {
                "For a blank machine: build an installer image from the Ubuntu Server ISO that installs and sets up the host without questions, and write it to a USB drive. F4 on the ISO field downloads the ISO."
            }
            Action::CloudSeed => {
                "Generate cloud-init user data. Upload the generated release archive to the configured URL."
            }
            Action::Machine => {
                "Have a provisioner create the machine, such as a container or a virtual machine on a server, and set it up as this host. Choose the provisioner below; plugins bring them. Its size is on the Settings tab, and Create is on the Run tab."
            }
            Action::Apply => {
                "Provision a dedicated Ubuntu host using Ansible. Blank inventory applies to this machine."
            }
            Action::Docker => {
                "Build the desktop image using Docker Compose. Start it afterwards with docker compose."
            }
            Action::Manage => {
                "Look after a host that is already set up, over SSH: show its status and access links, publish the desktop on the tailnet, run the smoke test, install updates or reboot. Pick the task on the overview."
            }
            Action::Ssh => {
                "Install on an existing Ubuntu host over SSH. Enter a hostname, IP address or SSH config alias, and how berth logs in: its own key by default. A host that does not take that key yet gets it with berth key trust HOST and a password used once. Sudo prompts use the normal terminal."
            }
        }
    }

    /// Actions that write an installer for a blank machine.
    pub fn installer(self) -> bool {
        matches!(self, Action::UsbSeed | Action::Iso)
    }

    /// Actions that make a machine nobody has logged in to, from the profile alone.
    pub fn seeds(self) -> bool {
        matches!(self, Action::CloudSeed | Action::Machine)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Text,
    Secret,
    Path,
    Toggle,
    Choice(&'static [&'static str]),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Group {
    Heading(&'static str),
    Under(Field),
}

pub struct Spec {
    pub label: &'static str,
    /// JSON pointer of the value inside the profile.
    pub pointer: &'static str,
    pub kind: Kind,
    pub help: &'static str,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Field {
    Profile,
    Hostname,
    Keyboard,
    Geometry,
    ReleaseUrl,
    RecoveryCidr,
    ReleaseStore,
    ReleasePrivate,
    TailscaleKey,
    TailscaleAuto,
    TailscaleLoginServer,
    DiskMode,
    DiskSerial,
    DiskModel,
    DiskPath,
    DiskType,
    Wifi,
    WifiInterface,
    WifiSsid,
    WifiPassword,
    OperatorUser,
    OperatorKey,
    OperatorHash,
    AgentUser,
    AgentKey,
    AgentHash,
    AgentSudo,
    AgentPrivileges,
    ToolsWeb,
    ToolsNative,
    ToolsJvm,
    ToolsDotnet,
    ToolsScripting,
    ToolsScientific,
    ToolsChrome,
    Playwright,
    SoftwareRendering,
    HardenSsh,
    Cockpit,
    Docker,
    Tailscale,
    Iso,
    IsoSha,
    UsbDevice,
    Inventory,
    AskBecomePass,
    SshHost,
    SshUser,
    SshPort,
    Login,
    Level,
}

impl Field {
    pub fn spec(self) -> Spec {
        use Field::*;
        use Kind::*;
        let (label, pointer, kind, help) = match self {
            Profile => (
                "Host profile",
                "/profile",
                Choice(PROFILES),
                "Choose laptop or cloud provisioning defaults. Only laptop adds a physical login and power settings. Cloud leaves SSH open to any address when no recovery SSH CIDR is set; for a QEMU guest, set that to 10.0.2.2/32.",
            ),
            Hostname => (
                "Hostname",
                "/hostname",
                Text,
                "Hostname assigned to the installed target. Lowercase letters, numbers and hyphens, starting with a letter. F4 generates one.",
            ),
            Keyboard => (
                "Keyboard layout",
                "/keyboard",
                Text,
                "Keyboard layout used by the installer, such as de or us.",
            ),
            Geometry => (
                "Desktop size",
                "/ansible_vars/devbox_geometry",
                Text,
                "Resolution of the virtual desktop, for example 1600x900. Blank keeps the default.",
            ),
            ReleaseUrl => (
                "Release HTTPS URL",
                "/release_url",
                Text,
                "HTTPS URL where you will publish the generated release archive.",
            ),
            ReleaseStore => (
                "Publish release to",
                "/release_store",
                Text,
                "Where the new machine gets the release archive from. Enter switches between the file servers and raw repositories of the Registries tab of the home screen; berth uploads the archive there when the seed is made. Or upload it yourself and enter its address below.",
            ),
            ReleasePrivate => (
                "Download with login",
                "/release_private",
                Toggle,
                "Off: the machine downloads the archive without a login, so the store has to allow anonymous reads. On: the seed carries the login of that registry in a root-only file that is deleted after the download. Anyone who can read the seed can read that login.",
            ),
            RecoveryCidr => (
                "Recovery SSH CIDR",
                "/ansible_vars/devbox_bootstrap_ssh_cidr",
                Text,
                "Network allowed to use SSH from outside the tailnet, such as your admin IP with /32. Left empty, a cloud host accepts SSH from any address, and other hosts only from the tailnet and the session used for installing.",
            ),
            TailscaleKey => (
                "Tailscale enrollment key",
                "/tailscale_auth_key",
                Secret,
                "Optional enrollment key from Tailscale or Headscale. Prefer a short-lived, one-use key. With the API key of the control server in the secret tailnet_api_key (or a Tailscale token in TS_API_KEY), F4 creates a one-use key valid for 24 hours.",
            ),
            TailscaleAuto => (
                "Automatic enrollment key",
                "/tailscale_auto_key",
                Toggle,
                "berth creates a one-use key whenever this host is about to join the tailnet: when a seed or installer is made (valid for 24 hours) and when an apply finds the host not on the tailnet yet (valid for an hour). It needs the control server's API key in the secret tailnet_api_key and, for Headscale, the tailnet_ settings. A host on the tailnet already needs no key.",
            ),
            TailscaleLoginServer => (
                "Login server",
                "/tailscale_login_server",
                Text,
                "The Headscale server this host joins, such as https://hs.example.org. Leave it empty for berth's tailnet_login_server setting, or Tailscale's own service when that is empty too. It takes effect when the host joins.",
            ),
            DiskMode => (
                "Disk selection",
                "/target_disk_mode",
                Choice(DISK_MODES),
                "How the installer picks the disk it erases. Auto requires exactly one eligible internal disk. Every mode stops when the match is ambiguous.",
            ),
            DiskSerial => (
                "Target disk serial",
                "/target_disk_serial",
                Text,
                "Exact udev ID_SERIAL of the target disk. The unattended installer erases the matching disk.",
            ),
            DiskModel => (
                "Disk model pattern",
                "/target_disk_model",
                Text,
                "Match udev ID_MODEL, with glob patterns such as Samsung*. Multiple matches stop installation.",
            ),
            DiskPath => (
                "Disk path pattern",
                "/target_disk_path",
                Path,
                "Match a /dev/ disk path or pattern. Exact /dev/disk/by-id symlinks are resolved during installation.",
            ),
            DiskType => (
                "Disk type filter",
                "/target_disk_ssd",
                Choice(DISK_TYPES),
                "Optional filter for model, path, auto or size selection.",
            ),
            Wifi => (
                "Wi-Fi",
                "/wifi_enabled",
                Toggle,
                "Configure Wi-Fi in the unattended installer. Wired DHCP stays available either way.",
            ),
            WifiInterface => (
                "Wi-Fi interface",
                "/wifi/interface",
                Text,
                "Leave blank to use the wireless interface found on the target during installation. Set a name such as wlp1s0 only to force one.",
            ),
            WifiSsid => (
                "Wi-Fi SSID",
                "/wifi/ssid",
                Text,
                "Wireless network name. Networks saved on this machine are listed in Suggestions. F5 unlocks their saved passwords, so choosing one also fills in its password.",
            ),
            WifiPassword => (
                "Wi-Fi password",
                "/wifi/password",
                Secret,
                "Wireless network password, saved in the private profile. When this machine knows the network, the Run tab shows whether the password matches.",
            ),
            OperatorUser => (
                "Operator username",
                "/operator_username",
                Text,
                "Account for human login and management. Ubuntu reserves names such as operator, admin and root.",
            ),
            OperatorKey => (
                "SSH key",
                "/operator_ssh_public_keys",
                Text,
                "An OpenSSH public key the operator account takes. Enter changes it, x removes it. The account can have several: one for each person or machine that logs in.",
            ),
            OperatorHash => (
                "Operator password hash",
                "/operator_password_hash",
                Secret,
                "SHA-512 crypt or yescrypt hash of the operator password. F4 generates a password and hash, and berth credentials HOST shows the password later. F5 hashes a password you enter.",
            ),
            AgentUser => (
                "Agent username",
                "/agent_username",
                Text,
                "Account used by coding agents and the virtual desktop. Home is /home/USERNAME.",
            ),
            AgentKey => (
                "SSH key",
                "/agent_ssh_public_keys",
                Text,
                "An OpenSSH public key for SSH access to the agent account. Enter changes it, x removes it. The account can have several, or none.",
            ),
            AgentHash => (
                "Agent password hash",
                "/agent_password_hash",
                Secret,
                "Optional local password for the agent. F4 generates a password and hash, and berth credentials HOST shows the password later. F5 hashes a password you enter. Empty locks password login.",
            ),
            AgentPrivileges => (
                "Agent privileges",
                "/agent_privileges",
                Choice(PRIVILEGES),
                "What the agent may do with sudo. full: everything, for machines you would throw away. limited: install and update packages with apt, and restart its own desktop and services; it cannot touch SSH, Tailscale, the firewall, the SSH user or berth's controller. none: nothing. It guards against accidents, not intent: package scripts run as root.",
            ),
            AgentSudo => (
                "Agent passwordless sudo",
                "/agent_passwordless_sudo",
                Toggle,
                "Allow the agent to use sudo without a password. Turning this off requires an agent password.",
            ),
            ToolsWeb => (
                "JavaScript runtimes",
                "/ansible_vars/devbox_tools_web",
                Toggle,
                "Install Bun/bunx, Deno and pnpm. Base Node and npm support the coding agents.",
            ),
            ToolsNative => (
                "Native development",
                "/ansible_vars/devbox_tools_native",
                Toggle,
                "Install Go, Clang and Rust with Cargo/rustup. Base GCC, CMake and Ninja remain available.",
            ),
            ToolsJvm => (
                "JVM development",
                "/ansible_vars/devbox_tools_jvm",
                Toggle,
                "Install OpenJDK 21, Maven, Gradle, Kotlin and Scala/sbt.",
            ),
            ToolsDotnet => (
                ".NET development",
                "/ansible_vars/devbox_tools_dotnet",
                Toggle,
                "Install the .NET SDK and native runtime dependencies.",
            ),
            ToolsScripting => (
                "Scripting languages",
                "/ansible_vars/devbox_tools_scripting",
                Toggle,
                "Install Ruby/Bundler, PHP/Composer and Erlang/Elixir.",
            ),
            ToolsScientific => (
                "Scientific computing",
                "/ansible_vars/devbox_tools_scientific",
                Toggle,
                "Install R for scientific computing.",
            ),
            ToolsChrome => (
                "Google Chrome",
                "/ansible_vars/devbox_tools_chrome",
                Toggle,
                "Install native Google Chrome from its vendor repository.",
            ),
            Playwright => (
                "Playwright",
                "/ansible_vars/devbox_install_playwright",
                Toggle,
                "Install Playwright and its matching Chromium build.",
            ),
            SoftwareRendering => (
                "Software graphics",
                "/ansible_vars/devbox_software_rendering",
                Toggle,
                "Use Mesa CPU rendering for the virtual desktop and headless jobs. Turn off on a machine with a GPU to render on it instead, for example for the Unity editor.",
            ),
            HardenSsh => (
                "SSH hardening",
                "/ansible_vars/devbox_harden_ssh",
                Toggle,
                "Accept SSH keys only and refuse root logins. Applied only when the operator has an SSH public key or the account used for installing has one, so it cannot lock you out. Console, Cockpit and sudo still use passwords.",
            ),
            Cockpit => (
                "Cockpit web console",
                "/ansible_vars/devbox_install_cockpit",
                Toggle,
                "Install the Cockpit web console for logs, services, storage, updates, files and a terminal. It listens on port 9090 and is reachable over the tailnet or an SSH tunnel. Log in as the operator with its password.",
            ),
            Docker => (
                "Docker on host",
                "/ansible_vars/devbox_install_docker",
                Toggle,
                "Install Docker, Compose and Buildx on the target host.",
            ),
            Tailscale => (
                "Tailscale",
                "/ansible_vars/devbox_install_tailscale",
                Toggle,
                "Install Tailscale on the target host and enable optional enrollment.",
            ),
            Iso => (
                "Source ISO",
                "/builder/iso",
                Path,
                "Ubuntu Server 26.04 amd64 installer ISO to build from. F4 downloads the current one and checks it against Ubuntu's published SHA256SUMS.",
            ),
            IsoSha => (
                "ISO SHA256",
                "/builder/iso_sha256",
                Text,
                "SHA-256 the source ISO must have. Filled in from the file when you choose the ISO, and F4 recomputes it. Compare it with Ubuntu's published SHA256SUMS before trusting the image.",
            ),
            Inventory => (
                "Ansible inventory",
                "/builder/inventory",
                Path,
                "Path to an Ansible inventory file. Blank provisions this machine.",
            ),
            AskBecomePass => (
                "Ask for sudo password",
                "/access/sudo",
                Toggle,
                "Ask for the SSH user's sudo password, for a first apply as an account of the machine's own whose sudo asks for one. The operator never needs it: once the host is applied as its operator, berth stops asking. The password is asked for when it is needed and kept until berth is closed, or kept as the secret sudo/HOST if you store it there.",
            ),
            SshHost => (
                "SSH address",
                "/access/address",
                Text,
                "The host's name or IP address, or a Host alias from ~/.ssh/config.",
            ),
            SshUser => (
                "SSH user",
                "/access/user",
                Text,
                "The account berth logs in as. Leave blank to use your SSH configuration.",
            ),
            SshPort => (
                "SSH port",
                "/access/port",
                Text,
                "SSH port. Leave blank for the SSH configuration default.",
            ),
            Login => (
                "Log in with",
                "/access/auth",
                Choice(LOGINS),
                "How berth logs in. berth: the installation's own key, which hosts it sets up take. key:NAME: another key of berth's key ring (the Settings tab of the home screen lists them). password: a password, kept as the secret ssh/HOST or asked for. ssh: your own SSH configuration and agent, with berth's key offered after them. A host that does not take its key yet gets it with: berth key trust HOST.",
            ),
            Level => (
                "Level",
                "/level",
                Choice(LEVELS),
                "How much of berth the host gets. workstation: the whole profile, with the desktop, the agents and plugins. controller: berthd and box alone, for sessions, tasks and files on a machine that stays as it is. reachable: nothing is installed; berth logs in over SSH. A lower level can be raised later: install the controller, or set workstation and apply.",
            ),
            UsbDevice => (
                "Then write to USB drive",
                "/builder/usb_device",
                Text,
                "Enter switches between not writing and each connected USB drive. The chosen drive is erased and gets the finished ISO; that asks for confirmation and your sudo password. You can also press w after the build instead.",
            ),
        };
        Spec {
            label,
            pointer,
            kind,
            help,
        }
    }

    /// Fields shown as a tree: under a heading, or under the field that reveals them.
    pub fn group(self) -> Option<Group> {
        use Field::*;
        Some(match self {
            OperatorUser | OperatorKey | OperatorHash => Group::Heading("Operator Account"),
            AgentUser | AgentKey | AgentHash | AgentSudo | AgentPrivileges => {
                Group::Heading("Agent Account")
            }
            ToolsWeb | ToolsNative | ToolsJvm | ToolsDotnet | ToolsScripting | ToolsScientific
            | ToolsChrome => Group::Heading("Development Tools"),
            DiskSerial | DiskModel | DiskPath | DiskType => Group::Under(DiskMode),
            WifiInterface | WifiSsid | WifiPassword => Group::Under(Wifi),
            TailscaleKey | TailscaleAuto | TailscaleLoginServer => Group::Under(Tailscale),
            ReleaseUrl | ReleasePrivate => Group::Under(ReleaseStore),
            IsoSha | UsbDevice => Group::Under(Iso),
            SshUser | SshPort | Login => Group::Under(SshHost),
            _ => return None,
        })
    }

    pub fn is_password_hash(self) -> bool {
        matches!(self, Field::OperatorHash | Field::AgentHash)
    }

    /// Fields that use the Wi-Fi logins saved on this machine.
    pub fn uses_saved_wifi(self) -> bool {
        matches!(self, Field::Wifi | Field::WifiSsid | Field::WifiPassword)
    }

    /// Fields whose value can be removed with x.
    pub fn can_clear(self) -> bool {
        matches!(self.spec().kind, Kind::Text | Kind::Secret | Kind::Path)
    }

    /// Fields with a Generate (F4) action.
    pub fn can_generate(self) -> bool {
        matches!(
            self,
            Field::Hostname | Field::Iso | Field::IsoSha | Field::TailscaleKey
        ) || self.is_password_hash()
    }
}
