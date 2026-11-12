//! Wizard state: the profile being edited, what each step shows, and running the chosen action.

use crate::{
    Result, credentials,
    discovery::{self, Choices, expand_path},
    fields::{Action, Field, Kind, SOURCES, Step, TASKS},
    job::{Job, Stage},
    picker::Picker,
    plugins::{self, Plugin},
    remote::{HostView, State},
    theme,
};
use berth_core::{apply, host::Host};
use berth_tui::host::Pick;
use berth_tui::setup::{self, set_pointer};
pub use berth_tui::setup::{FeatureGroup, Item, value_line};
use ratatui::{style::Style, text::Line, widgets::ListState};
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
};
use tui_input::Input;

/// What a selectable row does when activated.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Entry {
    Source(usize),
    Action(Action),
    Field(Field),
    /// Something the overview of a host offers, by name.
    Task(&'static str),
    /// The heading of a group on the Features page.
    Group(usize),
    /// A plugin's feature, switched on or off for this host: its index among them.
    Feature(usize),
    /// A task of the host, by index into what it listed.
    HostTask(usize),
    /// A session on the host, by index into what it listed.
    HostSession(usize),
    /// A registry this host can be given, by index into `App::registry_names`.
    HostRegistry(usize),
    /// A secret this host can be given, by index into `App::secret_names`.
    HostSecret(usize),
    /// One of an account's SSH keys, or the row that adds one.
    Key(setup::Account, Option<usize>),
    /// Open the set-up flow.
    Setup,
    Run,
}

pub struct Row {
    pub line: Line<'static>,
    /// `None` for headings and other rows the cursor skips.
    pub entry: Option<Entry>,
}

impl Row {
    fn info(line: impl Into<Line<'static>>) -> Self {
        Row {
            line: line.into(),
            entry: None,
        }
    }
    fn entry(line: impl Into<Line<'static>>, entry: Entry) -> Self {
        Row {
            line: line.into(),
            entry: Some(entry),
        }
    }
}

pub struct App {
    pub config: Value,
    /// The profile as last written to disk, for detecting unsaved changes.
    saved: Value,
    pub path: PathBuf,
    pub step: Step,
    /// What Run does: the install method, or `Manage` while on the Manage tab.
    pub action: Action,
    /// Action for the next `execute`, when it differs from the tab's own.
    pub terminal_action: Option<Action>,
    /// A command of this machine in a terminal inside the UI: `ssh` to a host
    /// that is not set up yet.
    pub session: Option<crate::session::Session>,
    /// What the host itself says it can do, its sessions, and the panel onto one.
    pub host: HostView,
    /// The password dialog for the host was shown; it is not opened by itself again.
    password_asked: bool,
    /// The Terminal row opens the agent's shell and not the SSH user's.
    pub as_agent: bool,
    /// Plugins found in the source and in the user's plugin directory.
    pub plugins: Vec<Plugin>,
    /// This berth's own SSH key: the private half's path, and the public half
    /// that hosts take. For a server's host the public half is the server's.
    pub own_key: Option<PathBuf>,
    pub own_public: String,
    /// What those plugins can install, for the Features page.
    catalogue: Vec<setup::Feature>,
    /// Where berth keeps its settings, for the registries and secret values.
    pub config_dir: PathBuf,
    /// The registries and secrets that exist, for the Secrets page.
    pub registry_names: Vec<String>,
    pub secret_names: Vec<String>,
    /// Web addresses in the output on screen, and where they are, for clicking.
    pub links: Vec<(ratatui::layout::Rect, String)>,
    /// The words of that output, for copying.
    pub words: Vec<(ratatui::layout::Rect, String)>,
    /// The install method to return to when leaving the Manage tab.
    method: Action,
    /// Index into the selectable rows of the current step.
    pub list: ListState,
    pub editing: Option<Input>,
    /// The editor holds a plain password to hash rather than a field value.
    pub password_entry: bool,
    pub suggestions: Choices,
    pub suggestion_list: ListState,
    pub suggestion_focus: bool,
    pub public_keys: Choices,
    pub private_keys: Choices,
    pub ssh_hosts: Choices,
    /// Machines on this machine's tailnet, as (description, address).
    pub tailnet_hosts: Choices,
    /// Wi-Fi networks saved on this machine, as (SSID, password).
    pub wifi_networks: Choices,
    pub status: String,
    pub picker: Option<Picker>,
    /// sudo is authorised, so the saved Wi-Fi passwords can be read.
    pub privileged: bool,
    /// The administrator password dialog, while it is open.
    pub unlock: Option<Input>,
    pub unlock_failed: bool,
    /// What the administrator password is being asked for.
    pub unlock_reason: &'static str,
    /// Run the action once the password dialog succeeds.
    pub pending_run: bool,
    /// The password dialog is for sudo on the managed host, not this machine.
    pub unlock_remote: bool,
    /// Where the builder writes archives, seeds and installer images.
    pub build_dir: PathBuf,
    /// Where downloaded installer ISOs are kept.
    pub iso_dir: PathBuf,
    /// The running job downloads the installer ISO; its last line is the path.
    fetching_iso: bool,
    /// Only write the ISO that is already built, without building it again.
    pub write_only: bool,
    /// The dialog for picking one of several USB drives is open.
    pub choosing_drive: bool,
    /// Question that needs a yes before the action runs.
    pub confirm: Option<String>,
    /// The action running inside the UI, and its output once it has ended.
    pub job: Option<Job>,
    /// Connected USB drives, as (description, device path).
    pub usb_drives: Choices,
    /// A link of the panel as a QR code, over everything else.
    pub qr: Option<berth_tui::qr::Overlay>,
}

/// The rows of a set-up page, as rows of this screen.
fn lift(rows: Vec<setup::Row>) -> Vec<Row> {
    rows.into_iter()
        .map(|row| Row {
            line: row.line,
            entry: row.entry.map(|entry| match entry {
                setup::Entry::Source(index) => Entry::Source(index),
                setup::Entry::Action(action) => Entry::Action(action),
                setup::Entry::Field(field) => Entry::Field(field),
                setup::Entry::Group(index) => Entry::Group(index),
                setup::Entry::Feature(index) => Entry::Feature(index),
                setup::Entry::Registry(index) => Entry::HostRegistry(index),
                setup::Entry::Secret(index) => Entry::HostSecret(index),
                setup::Entry::Key(account, index) => Entry::Key(account, index),
                setup::Entry::Run => Entry::Run,
            }),
        })
        .collect()
}

impl App {
    pub fn new(path: PathBuf, config: Value) -> Self {
        let mut app = App {
            saved: Value::Null,
            config,
            path,
            step: Step::Features,
            action: Action::Iso,
            method: Action::Iso,
            terminal_action: None,
            session: None,
            host: HostView::default(),
            password_asked: false,
            as_agent: true,
            links: vec![],
            words: vec![],
            list: ListState::default().with_selected(Some(0)),
            editing: None,
            password_entry: false,
            suggestions: vec![],
            suggestion_list: ListState::default(),
            suggestion_focus: false,
            public_keys: vec![],
            private_keys: vec![],
            ssh_hosts: vec![],
            tailnet_hosts: vec![],
            wifi_networks: vec![],
            plugins: vec![],
            own_key: None,
            own_public: String::new(),
            catalogue: vec![],
            config_dir: PathBuf::new(),
            registry_names: vec![],
            secret_names: vec![],
            status: String::new(),
            picker: None,
            privileged: false,
            unlock: None,
            unlock_failed: false,
            unlock_reason: "",
            pending_run: false,
            unlock_remote: false,
            build_dir: "build".into(),
            iso_dir: "build/iso".into(),
            fetching_iso: false,
            write_only: false,
            choosing_drive: false,
            confirm: None,
            job: None,
            usb_drives: vec![],
            qr: None,
        };
        app.saved = app.config.clone();
        app
    }

    /// The profile as its set-up pages see it.
    fn view(&self) -> setup::Profile<'_> {
        setup::Profile {
            config: &self.config,
            step: self.step,
            action: self.action,
            features: &self.catalogue,
            registries: &self.registry_names,
            secrets: &self.secret_names,
            own_key: &self.own_public,
        }
    }

    pub fn text(&self, field: Field) -> &str {
        self.view().text(field)
    }

    /// State of a toggle. Unset feature switches are on, like the role defaults.
    pub fn enabled(&self, field: Field) -> bool {
        self.view().enabled(field)
    }

    /// Read which registries and secrets exist. They are kept outside the profile.
    pub fn load_access(&mut self) {
        let store = crate::registry::Store::at(self.config_dir.clone());
        self.registry_names = store
            .registries()
            .unwrap_or_default()
            .keys()
            .cloned()
            .collect();
        self.secret_names = store.secret_names();
    }

    pub fn toggle_host_registry(&mut self, index: usize) {
        setup::toggle_registry(&mut self.config, &self.registry_names[index]);
    }

    /// How this host gets a secret: `GH_TOKEN` for a variable, a path for a
    /// file, nothing when it does not get it.
    pub fn host_secret(&self, index: usize) -> String {
        self.view().secret_how(index)
    }

    pub fn set_host_secret(&mut self, index: usize, how: &str) {
        if let Err(why) = setup::set_secret(&mut self.config, &self.secret_names[index], how) {
            self.status = why.into();
        }
    }

    fn access_rows(&self) -> Vec<Row> {
        lift(self.view().access_rows())
    }

    /// Read the plugins of the source (the working directory) and the user's own.
    pub fn load_plugins(&mut self, user_dir: PathBuf) {
        self.set_plugins(plugins::load(&["plugins".into(), user_dir]));
    }

    pub fn set_plugins(&mut self, plugins: Vec<Plugin>) {
        self.catalogue = plugins
            .iter()
            .flat_map(|plugin| {
                plugin.features.iter().map(|feature| setup::Feature {
                    key: format!("{}/{}", plugin.id, feature.id),
                    label: feature.label.clone(),
                    group: feature.group.clone(),
                    help: feature.help.to_owned(),
                })
            })
            .collect();
        self.plugins = plugins;
    }

    /// What a server's host can have installed: what the server's plugins
    /// offer, as `setup.catalogue` lists it.
    pub fn set_features(&mut self, features: Vec<setup::Feature>) {
        self.catalogue = features;
    }

    /// The groups of the Features page: the built-in ones, with what plugins add
    /// to them, then groups only plugins have.
    pub fn feature_groups(&self) -> Vec<FeatureGroup> {
        self.view().feature_groups()
    }

    pub fn item_enabled(&self, item: Item) -> bool {
        self.view().item_enabled(item)
    }

    pub fn set_item(&mut self, item: Item, on: bool) {
        match item {
            Item::Field(field) => self.set_toggle(field, on),
            Item::Feature(index) => {
                setup::set_feature(&mut self.config, &self.catalogue, index, on)
            }
        }
    }

    /// The group whose heading is selected.
    pub fn selected_group(&self) -> Option<usize> {
        match self.selected_entry() {
            Some(Entry::Group(index)) => Some(index),
            _ => None,
        }
    }

    /// Whether everything in a group is on, which is when switching it turns it off.
    pub fn group_all_on(&self, index: usize) -> bool {
        self.view().group_all_on(index)
    }

    /// Switch everything in a group on, or off when all of it is on already.
    pub fn toggle_group(&mut self, index: usize) {
        let on = !self.group_all_on(index);
        let group = self.feature_groups().swap_remove(index);
        for item in group.items {
            self.set_item(item, on);
        }
        self.status = format!("{}: all {}", group.title, if on { "on" } else { "off" });
    }

    fn feature_rows(&self) -> Vec<Row> {
        lift(self.view().feature_rows())
    }

    /// Set a toggle. The coding agents share the older switch for all of them,
    /// which the role still reads: it is on while any agent is.
    fn set_toggle(&mut self, field: Field, on: bool) {
        self.set(field, json!(on));
    }

    fn set(&mut self, field: Field, value: Value) {
        set_pointer(&mut self.config, field.spec().pointer, value);
        // Another address is another host to ask.
        use Field::*;
        if matches!(field, SshHost | SshUser | SshPort | SshKey | AskBecomePass) {
            self.reconnect_host();
        }
    }

    pub fn unsaved(&self) -> bool {
        self.config != self.saved
    }

    /// Fields shown by the current step for the chosen action.
    #[cfg(test)]
    pub fn fields(&self) -> Vec<Field> {
        self.view().fields()
    }

    fn field_rows(&self) -> Vec<Row> {
        lift(self.view().field_rows())
    }

    fn review_rows(&self) -> Vec<Row> {
        let mut rows = vec![];
        if self.action == Action::Apply && self.text(Field::Inventory).is_empty() {
            rows.push(Row::info(Line::styled(
                "! This installs on THIS MACHINE (no inventory set)",
                Style::new().fg(theme::WARN).bold(),
            )));
        }
        let problems = self.problems();
        for problem in &problems {
            rows.push(Row::info(Line::styled(
                format!("✗ {problem}"),
                Style::new().fg(theme::BAD),
            )));
        }
        if self.action.installer() && self.enabled(Field::Wifi) {
            let (text, color) = match self.wifi_password_matches() {
                Some(true) => (
                    "✓ Wi-Fi password matches the one saved on this machine",
                    theme::GOOD,
                ),
                Some(false) => (
                    "! Wi-Fi password differs from the one saved on this machine",
                    theme::WARN,
                ),
                None => (
                    "· Wi-Fi login not checked: this machine has not saved that network",
                    theme::MUTED,
                ),
            };
            rows.push(Row::info(Line::styled(text, Style::new().fg(color))));
        }
        if let Some(text) = self.view().open_ssh_warning() {
            rows.push(Row::info(Line::styled(text, Style::new().fg(theme::WARN))));
        }
        if problems.is_empty() {
            rows.push(Row::info(Line::styled(
                "✓ Ready",
                Style::new().fg(theme::GOOD),
            )));
        }
        rows.push(Row::info(""));
        if self.action == Action::Iso {
            let device = self.text(Field::UsbDevice);
            let drive = self.usb_drives.iter().find(|(_, path)| path == device);
            let value = match drive {
                Some((label, _)) => label.as_str(),
                None if device.is_empty() => "no",
                None => device,
            };
            rows.push(Row::entry(
                value_line("Then write to USB drive", value, Kind::Text),
                Entry::Field(Field::UsbDevice),
            ));
        }
        rows.push(Row::entry(
            format!("[Run · {}]", self.action.label()),
            Entry::Run,
        ));
        rows
    }

    pub fn rows(&self) -> Vec<Row> {
        let heading = |text: &'static str| {
            Row::info(Line::styled(text, Style::new().fg(theme::ACCENT).bold()))
        };
        let mut rows = vec![];
        match self.step {
            Step::Features => rows = self.feature_rows(),
            Step::Secrets => rows = self.access_rows(),
            Step::Accounts => {
                rows = self.field_rows();
                rows.extend(lift(setup::Profile::source_rows()));
            }
            Step::Install => {
                rows = lift(self.view().method_rows(self.methods()));
                rows.push(Row::info(""));
                rows.extend(self.field_rows());
                rows.push(Row::info(""));
                rows.extend(self.review_rows());
            }
            Step::Manage => {
                if let Some(line) = self.host.stats_line() {
                    rows.push(Row::info(Line::styled(line, Style::new().fg(theme::MUTED))));
                    rows.push(Row::info(""));
                }
                // berthd and box come with berth, so a newer berth has newer ones to give.
                if let State::Ready(version) = &self.host.state
                    && version != berth_api::VERSION
                    && self.served().is_none()
                {
                    rows.push(Row::info(Line::styled(
                        format!(
                            "The host's tools are {version}, this berth is {}. Apply this configuration updates them.",
                            berth_api::VERSION
                        ),
                        Style::new().fg(theme::WARN),
                    )));
                    rows.push(Row::info(""));
                }
                rows.push(heading("Connection"));
                if let Some(server) = self.served() {
                    rows.push(Row::info(format!(
                        "  {} · through the berth server at {server}",
                        self.text(Field::SshHost)
                    )));
                }
                if !self.locked() {
                    rows.extend(self.field_rows());
                }
                for problem in self.problems() {
                    rows.push(Row::info(Line::styled(
                        format!("✗ {problem}"),
                        Style::new().fg(theme::BAD),
                    )));
                }
                for (title, group) in self.overview_groups() {
                    rows.push(Row::info(""));
                    rows.push(Row::info(Line::styled(
                        title,
                        Style::new().fg(theme::ACCENT).bold(),
                    )));
                    rows.extend(group);
                }
                // Set-up and applying it are for whoever may set the server up.
                if !self.locked() {
                    rows.push(Row::info(""));
                    rows.push(heading("Set-Up"));
                    rows.push(Row::entry(
                        "  ▸ Change what is installed, accounts or install method  ›",
                        Entry::Setup,
                    ));
                    // A host without its controller offers Apply above already.
                    if !matches!(self.host.state, State::Missing | State::Old(_)) {
                        rows.push(Row::entry(
                            "  ▸ Apply this configuration",
                            Entry::Task("apply"),
                        ));
                    }
                }
            }
        }
        rows
    }

    /// The groups of the overview under the connection, as the host lists its
    /// tasks: each in the group it names, with the terminal among the first and
    /// the host's sessions after them. A host that cannot be asked says why.
    fn overview_groups(&self) -> Vec<(String, Vec<Row>)> {
        let note = |text: String, colour| {
            Row::info(Line::styled(format!("  {text}"), Style::new().fg(colour)))
        };
        let fixed = |name: &'static str| {
            let label = TASKS
                .iter()
                .find(|(task, ..)| *task == name)
                .map_or(name, |(_, label, _)| label);
            Row::entry(format!("  ▸ {label}"), Entry::Task(name))
        };
        let host = |rows| vec![("Host".to_owned(), rows)];
        match &self.host.state {
            State::Ready(_) => {}
            State::Idle if !self.problems().is_empty() => return vec![],
            State::Idle | State::Connecting => {
                return host(vec![note(
                    "Asking the host what it can do…".into(),
                    theme::MUTED,
                )]);
            }
            State::Password => {
                return host(vec![
                    note(
                        "The SSH user's sudo password is needed to reach the host.".into(),
                        theme::WARN,
                    ),
                    fixed("connect"),
                ]);
            }
            State::Missing => {
                return host(vec![
                    note("berth's controller is not on this host yet. Applying the configuration installs it.".into(), theme::WARN),
                    fixed("apply"),
                ]);
            }
            State::Old(version) => {
                return host(vec![
                    note(
                        format!(
                            "The host's tools ({version}) are older than this berth needs. Applying the configuration brings them up to date."
                        ),
                        theme::WARN,
                    ),
                    fixed("apply"),
                ]);
            }
            State::Unreachable(message) => {
                let first = message.lines().next().unwrap_or("").to_owned();
                return host(vec![
                    note(format!("✗ {first}"), theme::BAD),
                    fixed("connect"),
                    fixed("ssh"),
                ]);
            }
        }
        let user = match self.as_agent {
            true => self.agent_label().to_owned(),
            false => self.operator_label(),
        };
        berth_tui::host::groups(&self.host, &user)
            .into_iter()
            .map(|(title, rows)| {
                let rows = rows
                    .into_iter()
                    .map(|(line, pick)| {
                        let entry = match pick {
                            Pick::Task(index) => Entry::HostTask(index),
                            Pick::Terminal => Entry::Task("terminal"),
                            Pick::Session(index) => Entry::HostSession(index),
                        };
                        Row::entry(line, entry)
                    })
                    .collect();
                (title, rows)
            })
            .collect()
    }

    fn agent_label(&self) -> &str {
        match self.text(Field::AgentUser) {
            "" => "agent",
            user => user,
        }
    }

    fn operator_label(&self) -> String {
        match self.text(Field::SshUser) {
            "" => "the SSH user".into(),
            user => format!("{user}, the SSH user"),
        }
    }

    pub fn entries(&self) -> Vec<Entry> {
        self.rows()
            .into_iter()
            .filter_map(|row| row.entry)
            .collect()
    }

    pub fn selected_entry(&self) -> Option<Entry> {
        self.entries()
            .get(self.list.selected().unwrap_or(0))
            .copied()
    }

    pub fn selected_field(&self) -> Option<Field> {
        match self.selected_entry() {
            Some(Entry::Field(field)) => Some(field),
            _ => None,
        }
    }

    /// The information box for the selected row, as Markdown. A task that has
    /// steps from plugins lists what each of them adds.
    pub fn help(&self) -> String {
        match self.selected_entry() {
            Some(Entry::Feature(index)) => self.catalogue[index].help.clone(),
            // The key itself, to give to a host that was not set up from here.
            Some(Entry::Field(Field::BerthKey)) if !self.own_public.is_empty() => {
                format!("{}\n\n`{}`", Field::BerthKey.spec().help, self.own_public)
            }
            Some(Entry::HostTask(index)) => berth_tui::host::task_help(&self.host, index),
            Some(Entry::HostSession(index)) => berth_tui::host::session_help(&self.host, index),
            _ => self.entry_help().to_owned(),
        }
    }

    fn entry_help(&self) -> &'static str {
        match self.selected_entry() {
            Some(Entry::Source(index)) => SOURCES[index].1,
            Some(Entry::Action(action)) => action.help(),
            Some(Entry::Field(field)) => field.spec().help,
            Some(Entry::Task(name)) => TASKS
                .iter()
                .find(|(task, ..)| *task == name)
                .map_or("", |(.., help)| help),
            Some(Entry::Group(_)) => setup::GROUP_HELP,
            Some(Entry::Feature(_)) => "",
            Some(Entry::HostTask(_) | Entry::HostSession(_)) => "",
            Some(Entry::HostRegistry(_)) => {
                "Give this host the login of this registry. Its npm repositories become the agent's npm registry with the login in ~/.npmrc, and its Docker addresses get the login in ~/.docker/config.json. Registries are set up on the Registries tab of the home screen."
            }
            Some(Entry::Key(account, Some(_))) => match account {
                setup::Account::Operator => Field::OperatorKey.spec().help,
                setup::Account::Agent => Field::AgentKey.spec().help,
            },
            Some(Entry::Key(_, None)) => setup::ADD_KEY_HELP,
            Some(Entry::HostSecret(_)) => {
                "How this host gets the secret. Enter, then type a variable name such as GH_TOKEN to put it in the agent's environment, or a path such as ~/.config/app/token to write it to a file. Empty, or x, means the host does not get it. A secret given as GH_TOKEN also logs git in to GitHub through gh. Values are set on the Secrets tab of the home screen."
            }
            Some(Entry::Setup) => {
                "Opens the set-up pages of this host: what is installed, its accounts, and how to install it. Esc comes back here. Changes reach a running host with Apply this configuration."
            }
            Some(Entry::Run) | None => self.action.help(),
        }
    }

    /// The selected row takes a file path, so Browse applies.
    pub fn path_selected(&self) -> bool {
        match self.selected_entry() {
            Some(Entry::Source(index)) => index > 0,
            Some(Entry::Field(field)) => field.spec().kind == Kind::Path,
            _ => false,
        }
    }

    /// The `berth serve` this host belongs to, when it is not a host of this machine.
    pub fn served(&self) -> Option<&str> {
        self.config["builder"]["served"].as_str()
    }

    /// A server's host that this berth may use and not set up.
    pub fn locked(&self) -> bool {
        self.config["builder"]["served_only"] == true
    }

    /// The server a host is set up on through this berth.
    fn server(&self) -> Option<berth_core::server::Server> {
        self.served()?;
        (!self.locked())
            .then(|| self.host_profile().served())
            .flatten()
    }

    fn name(&self) -> String {
        self.host_profile().name
    }

    /// The ways to install this host: for a server's host those the server
    /// does itself.
    pub fn methods(&self) -> &'static [Action] {
        match self.served() {
            Some(_) => &[Action::CloudSeed, Action::Ssh],
            None => &Action::METHODS,
        }
    }

    pub fn set_step(&mut self, step: Step) {
        if let Some(server) = self.served()
            && self.locked()
            && step != Step::Manage
        {
            self.status = format!(
                "This host is set up on the berth server at {server}, by whoever has the role admin there"
            );
            return;
        }
        self.step = step;
        // The Manage tab acts on the running host; the others on the install method.
        if step == Step::Manage {
            if self.action != Action::Manage {
                self.method = self.action;
            }
            self.action = Action::Manage;
        } else if self.action == Action::Manage {
            self.action = self.method;
        }
        // A host with an address opens on its tasks rather than the address.
        let task = |entry: &Entry| matches!(entry, Entry::Task(_) | Entry::HostTask(_));
        let first = match self.text(Field::SshHost).is_empty() {
            false if step == Step::Manage => self.entries().iter().position(task),
            _ => None,
        };
        if step == Step::Secrets {
            self.load_access();
        }
        self.list = ListState::default().with_selected(Some(first.unwrap_or(0)));
        self.status.clear();
        self.suggestion_focus = false;
    }

    pub fn move_selection(&mut self, up: bool) {
        let index = self.list.selected().unwrap_or(0);
        let last = self.entries().len().saturating_sub(1);
        self.list.select(Some(if up {
            index.saturating_sub(1)
        } else {
            (index + 1).min(last)
        }));
    }

    pub fn open_picker(&mut self) {
        if !self.path_selected() {
            return;
        }
        let entry = self.selected_entry();
        if self.editing.is_none() {
            match entry {
                Some(Entry::Field(field)) => self.activate(field),
                _ => self.editing = Some(Input::default()),
            }
        }
        let value = self.editing.as_ref().map_or("", |input| input.value());
        let wants_key = matches!(entry, Some(Entry::Source(2) | Entry::Field(Field::SshKey)));
        let start = if !value.is_empty() {
            value.to_owned()
        } else if wants_key && let Some(home) = std::env::var_os("HOME") {
            PathBuf::from(home)
                .join(".ssh")
                .to_string_lossy()
                .into_owned()
        } else {
            ".".into()
        };
        match Picker::new(&start) {
            Ok(picker) => self.picker = Some(picker),
            Err(error) => self.status = error.to_string(),
        }
    }

    /// Generate a hostname, or a password whose hash goes into the selected field.
    pub fn generate(&mut self) -> Result<()> {
        let Some(field) = self.selected_field() else {
            return Ok(());
        };
        if field == Field::Iso {
            let mut command = Command::new("python3");
            command
                .args(["bin/devbox.py", "fetch-iso", "--dir"])
                .arg(&self.iso_dir);
            let stage = Stage {
                title: "Download Ubuntu Server ISO".into(),
                command,
                source: None,
                input: None,
            };
            let next = "The ISO path and checksum are filled in.";
            self.job = Some(Job::start(vec![stage], next, vec![])?);
            self.fetching_iso = true;
        } else if field == Field::Hostname {
            self.set(field, json!(credentials::hostname()?));
        } else if field == Field::TailscaleKey {
            let host = self.text(Field::Hostname);
            let description = if host.is_empty() {
                "devbox".to_owned()
            } else {
                format!("devbox {host}")
            };
            let key = credentials::tailscale_auth_key(&description)?;
            self.set(field, json!(key));
            self.status = "One-use Tailscale key created · valid for 24 hours".into();
        } else if field == Field::IsoSha {
            self.hash_iso()?;
        } else if field.is_password_hash() {
            let password = credentials::password()?;
            let hash = credentials::hash(&password)?;
            let user = self.text(if field == Field::OperatorHash {
                Field::OperatorUser
            } else {
                Field::AgentUser
            });
            let user = if user.is_empty() { "account" } else { user }.to_owned();
            let path = self.path.with_extension("credentials.json");
            credentials::save(&path, &user, &password)?;
            self.set(field, json!(hash));
            self.status = format!("Generated password saved privately in {}", path.display());
        }
        Ok(())
    }

    pub fn refresh_discovery(&mut self) {
        if let Some(home) = std::env::var_os("HOME") {
            let ssh = PathBuf::from(home).join(".ssh");
            (self.public_keys, self.private_keys) = discovery::keys(&ssh);
            self.ssh_hosts = discovery::hosts(&ssh.join("config"));
        }
        self.wifi_networks = discovery::wifi_networks(self.privileged);
        self.usb_drives = discovery::usb_drives();
        self.tailnet_hosts = discovery::tailnet_hosts();
        for entry in discovery::agent_keys() {
            let same_key = |(_, key): &(String, String)| {
                key.split_whitespace()
                    .take(2)
                    .eq(entry.1.split_whitespace().take(2))
            };
            if !self.public_keys.iter().any(same_key) {
                self.public_keys.push(entry);
            }
        }
    }

    pub fn update_suggestions(&mut self) {
        let own = |values: &[&str]| -> Choices {
            values
                .iter()
                .map(|value| (value.to_string(), value.to_string()))
                .collect()
        };
        let choices = match self.selected_entry() {
            // Importing a key file wants paths, which ssh-agent keys do not have.
            Some(Entry::Source(2)) => self
                .public_keys
                .iter()
                .filter(|(label, _)| !label.starts_with("ssh-agent"))
                .map(|(label, _)| (label.clone(), label.clone()))
                .collect(),
            Some(Entry::Key(..)) => self.public_keys.clone(),
            Some(Entry::Field(field)) => match (field, field.spec().kind) {
                (_, Kind::Choice(values)) => own(values),
                (Field::SshHost, _) => self
                    .tailnet_hosts
                    .iter()
                    .chain(&self.ssh_hosts)
                    .cloned()
                    .collect(),
                (Field::WifiSsid, _) => self
                    .wifi_networks
                    .iter()
                    .map(|(ssid, _)| (ssid.clone(), ssid.clone()))
                    .collect(),
                (Field::SshKey, _) => self.private_keys.clone(),
                _ => vec![],
            },
            _ => vec![],
        };
        if self.suggestions != choices {
            self.suggestions = choices;
            self.suggestion_list = ListState::default().with_selected(Some(0));
            self.suggestion_focus = false;
        }
    }

    pub fn select_suggestion(&mut self) {
        if let Some((_, value)) = self
            .suggestions
            .get(self.suggestion_list.selected().unwrap_or(0))
        {
            self.editing = Some(Input::new(value.clone()));
            self.password_entry = false;
            self.accept_edit();
        }
        self.suggestion_focus = false;
    }

    pub fn cancel_edit(&mut self) {
        self.editing = None;
        self.password_entry = false;
    }

    pub fn accept_edit(&mut self) {
        let Some(input) = self.editing.take() else {
            return;
        };
        let hashing = std::mem::take(&mut self.password_entry);
        match self.selected_entry() {
            Some(Entry::Field(field)) if hashing => match credentials::hash(input.value()) {
                Ok(hash) => self.set(field, json!(hash)),
                Err(error) => self.status = error.to_string(),
            },
            Some(Entry::Field(field)) => self.commit(field, input.value()),
            Some(Entry::Source(index)) => {
                if let Err(error) = self.import(index, input.value()) {
                    self.status = format!("Import failed · {error}");
                }
            }
            Some(Entry::HostSecret(index)) => self.set_host_secret(index, input.value()),
            Some(Entry::Key(account, index)) => {
                // Nothing typed changes nothing: x is what removes a key.
                if !input.value().trim().is_empty()
                    && let Err(why) =
                        setup::set_key(&mut self.config, account, index, input.value())
                {
                    self.status = why;
                }
            }
            _ => {}
        }
    }

    /// Toggle or cycle the field, or open the editor for free text.
    pub fn activate(&mut self, field: Field) {
        if field == Field::ReleaseStore {
            // Cycle through entering an address and the configured stores.
            let stores = crate::registry::Store::at(self.config_dir.clone()).release_stores();
            let next = match stores.iter().position(|store| store == self.text(field)) {
                None if self.text(field).is_empty() => stores.first(),
                None => None,
                Some(index) => stores.get(index + 1),
            };
            if stores.is_empty() {
                self.status = "No file server or raw repository yet. Add one on the Registries tab of the home screen.".into();
            }
            let value = next.cloned().unwrap_or_default();
            self.set(field, json!(value));
            return;
        }
        if field == Field::UsbDevice {
            // Cycle through "no" and the drives connected right now.
            self.usb_drives = discovery::usb_drives();
            let current = self
                .usb_drives
                .iter()
                .position(|(_, path)| path == self.text(field));
            let next = match current {
                None if self.text(field).is_empty() => self.usb_drives.first(),
                None => None,
                Some(index) => self.usb_drives.get(index + 1),
            };
            let device = next.map_or(String::new(), |(_, path)| path.clone());
            self.status = match device.is_empty() {
                true if self.usb_drives.is_empty() => {
                    "No usable USB drive. Replug one that was ejected.".into()
                }
                _ => String::new(),
            };
            self.set(field, json!(device));
            return;
        }
        match field.spec().kind {
            Kind::Toggle => self.set_toggle(field, !self.enabled(field)),
            Kind::Choice(choices) => {
                let current = choices
                    .iter()
                    .position(|choice| *choice == self.text(field));
                let next = current.map_or(0, |index| (index + 1) % choices.len());
                self.set(field, json!(choices[next]));
            }
            _ => self.editing = Some(Input::new(self.text(field).to_owned())),
        }
    }

    pub fn commit(&mut self, field: Field, value: &str) {
        let spec = field.spec();
        let value = if spec.kind == Kind::Secret {
            value
        } else {
            value.trim()
        };
        if let Kind::Choice(choices) = spec.kind
            && !choices.contains(&value)
        {
            self.status = format!("Choose one of: {}", choices.join(", "));
            return;
        }
        if let Some(name) = spec.pointer.strip_prefix("/ansible_vars/")
            && value.is_empty()
        {
            // An empty override would replace the role default with nothing.
            if let Some(vars) = self.config["ansible_vars"].as_object_mut() {
                vars.remove(name);
            }
            return;
        }
        self.set(field, json!(value));
        if spec.pointer.starts_with("/wifi/")
            && self.config["wifi"]
                .as_object()
                .is_some_and(|wifi| wifi.values().all(|v| v.as_str() == Some("")))
        {
            self.config["wifi"] = Value::Null;
        }
        if field == Field::SshHost && !value.is_empty() {
            self.fill_from_ssh_config(value);
        }
        if field == Field::WifiSsid
            && let Some(password) = self.saved_wifi_password().map(str::to_owned)
        {
            self.set(Field::WifiPassword, json!(password));
            self.status = "Wi-Fi password filled from this machine".into();
        }
        if field == Field::WifiPassword {
            match self.wifi_password_matches() {
                Some(true) => {
                    self.status = "Matches the Wi-Fi password saved on this machine".into()
                }
                Some(false) => {
                    self.status = "Differs from the Wi-Fi password saved on this machine".into()
                }
                None => {}
            }
        }
        if field == Field::Iso
            && !value.is_empty()
            && let Err(error) = self.hash_iso()
        {
            self.status = error.to_string();
        }
    }

    /// Record the checksum of the chosen ISO so the build can detect a changed file.
    fn hash_iso(&mut self) -> Result<()> {
        let path = expand_path(self.text(Field::Iso));
        if !path.is_file() {
            return Err("Choose the source ISO first".into());
        }
        let digest = credentials::sha256(&path)?;
        self.set(Field::IsoSha, json!(digest));
        self.status = "ISO SHA256 computed · compare with Ubuntu's SHA256SUMS".into();
        Ok(())
    }

    /// Get administrator rights for reading saved Wi-Fi passwords, asking for the
    /// password in a dialog unless sudo needs none.
    pub fn request_unlock(&mut self, reason: &'static str) {
        self.unlock_reason = reason;
        if self.privileged || credentials::sudo_unlock(None) {
            self.unlocked();
        } else {
            self.unlock_failed = false;
            self.unlock = Some(Input::default());
        }
    }

    pub fn submit_unlock(&mut self) {
        let Some(input) = self.unlock.take() else {
            return;
        };
        if std::mem::take(&mut self.unlock_remote) {
            self.host.password = Some(input.value().to_owned());
            self.connect_host();
            return;
        }
        if credentials::sudo_unlock(Some(input.value())) {
            self.unlocked();
            if self.pending_run {
                self.run(true);
            }
        } else {
            self.unlock_failed = true;
            self.unlock = Some(Input::default());
        }
    }

    fn unlocked(&mut self) {
        self.privileged = true;
        self.wifi_networks = discovery::wifi_networks(true);
        let known = self
            .wifi_networks
            .iter()
            .filter(|(_, password)| !password.is_empty())
            .count();
        self.status = format!("Unlocked {known} saved Wi-Fi passwords");
        if self.text(Field::WifiPassword).is_empty()
            && let Some(password) = self.saved_wifi_password().map(str::to_owned)
        {
            self.set(Field::WifiPassword, json!(password));
            self.status = "Wi-Fi password filled from this machine".into();
        }
    }

    /// Password this machine has saved for the profile's SSID, if it knows the network.
    fn saved_wifi_password(&self) -> Option<&str> {
        let ssid = self.text(Field::WifiSsid);
        self.wifi_networks
            .iter()
            .find(|(known, password)| known == ssid && !password.is_empty())
            .map(|(_, password)| password.as_str())
    }

    /// Whether the profile's Wi-Fi password is the one this machine uses for that
    /// network. `None` when this machine has nothing to compare against.
    pub fn wifi_password_matches(&self) -> Option<bool> {
        self.saved_wifi_password()
            .map(|saved| saved == self.text(Field::WifiPassword))
    }

    /// Fill blank connection fields with what OpenSSH would use for this host.
    fn fill_from_ssh_config(&mut self, host: &str) {
        let Ok(output) = Command::new("ssh").args(["-G", "--", host]).output() else {
            return;
        };
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let Some((name, value)) = line.split_once(' ') else {
                continue;
            };
            let field = match name {
                "user" => Field::SshUser,
                "port" => Field::SshPort,
                "identityfile" if expand_path(value).is_file() => Field::SshKey,
                _ => continue,
            };
            if self.text(field).is_empty() {
                self.set(field, json!(value));
            }
        }
    }

    /// Fill blank fields from local SSH keys and DEVBOX_* environment variables.
    pub fn detect(&mut self) {
        self.refresh_discovery();
        let only_key = match self.public_keys.as_slice() {
            [(_, key)] => Some(key.clone()),
            _ => None,
        };
        let env = |name: &str| std::env::var(name).ok();
        let mut filled = Vec::new();
        for (label, field, detected) in [
            ("hostname", Field::Hostname, env("DEVBOX_HOSTNAME")),
            (
                "disk serial",
                Field::DiskSerial,
                env("DEVBOX_TARGET_DISK_SERIAL"),
            ),
            ("release URL", Field::ReleaseUrl, env("DEVBOX_RELEASE_URL")),
            ("ISO path", Field::Iso, env("DEVBOX_ISO")),
            ("ISO checksum", Field::IsoSha, env("DEVBOX_ISO_SHA256")),
            ("inventory", Field::Inventory, env("DEVBOX_INVENTORY")),
        ] {
            let Some(value) = detected
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
            else {
                continue;
            };
            if self.text(field).is_empty() {
                self.set(field, json!(value));
                filled.push(label);
            }
        }
        // Without this berth's own key the operator needs one of the user's.
        if let Some(key) = only_key
            && self.view().keys(setup::Account::Operator).is_empty()
            && self.own_public.is_empty()
            && setup::set_key(&mut self.config, setup::Account::Operator, None, &key).is_ok()
        {
            filled.push("public key");
        }
        self.status = if filled.is_empty() {
            format!(
                "Found {} public keys, {} private keys and {} SSH hosts. Select a field to choose.",
                self.public_keys.len(),
                self.private_keys.len(),
                self.ssh_hosts.len()
            )
        } else {
            format!("Review detected values: {}", filled.join(", "))
        };
    }

    fn import(&mut self, source: usize, path: &str) -> Result<()> {
        if source == 1 {
            self.config = parse_config(&fs::read(expand_path(path))?)?;
            self.status = "Profile imported. Save to keep it at the current output path.".into();
        } else {
            let key = fs::read_to_string(expand_path(path))?;
            let key = key.trim();
            if !discovery::is_public_key(key) {
                return Err("Expected a supported OpenSSH public key".into());
            }
            setup::set_key(&mut self.config, setup::Account::Operator, None, key)?;
            self.status = "SSH public key imported".into();
        }
        Ok(())
    }

    /// Reasons the chosen action would fail, checked before leaving the UI.
    pub fn problems(&self) -> Vec<String> {
        use Field::*;
        let action = self.action;
        if action == Action::Manage {
            return self
                .remote_inventory()
                .err()
                .map(|error| error.to_string())
                .into_iter()
                .collect();
        }
        let mut problems = self.view().problems();
        let mut check = |ok: bool, message: &str| {
            if !ok {
                problems.push(message.to_owned());
            }
        };
        match action {
            Action::Iso => {
                check(
                    expand_path(self.text(Iso)).is_file(),
                    "Source ISO file does not exist",
                );
                let usb = self.text(UsbDevice);
                check(
                    usb.is_empty() || self.usb_drives.iter().any(|(_, path)| path == usb),
                    "The USB drive to write to is not connected",
                );
                let sha = self.text(IsoSha);
                check(
                    sha.is_empty()
                        || sha.len() == 64
                            && sha
                                .chars()
                                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
                    "ISO SHA256 must be 64 lowercase hex characters",
                );
            }
            Action::Apply => {
                let inventory = self.text(Inventory);
                check(
                    inventory.is_empty() || expand_path(inventory).exists(),
                    "Ansible inventory file does not exist",
                );
            }
            Action::Ssh => {
                if let Err(error) = self.remote_inventory() {
                    problems.push(error.to_string());
                }
            }
            _ => {}
        }
        problems
    }

    fn remote_inventory(&self) -> Result<Value> {
        self.host_profile().inventory()
    }

    /// This host as its profile describes it, for reaching and applying it.
    pub fn host_profile(&self) -> Host {
        let name = self
            .path
            .file_stem()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        Host {
            name,
            path: self.path.clone(),
            config: self.config.clone(),
            key: self.own_key.clone(),
        }
    }

    /// Start talking to the host's controller. A host whose sudo asks for a
    /// password gets the dialog first, once.
    pub fn connect_host(&mut self) {
        let host = self.host_profile();
        let own_link = host.config["builder"]["link"].is_array() || host.served().is_some();
        if host.asks_password() && !own_link && self.host.password.is_none() {
            self.host.state = State::Password;
            if !std::mem::replace(&mut self.password_asked, true) {
                self.ask_host_password(false);
            }
            return;
        }
        match host.link(self.host.password.as_deref()) {
            Ok(spec) => {
                crate::remote::connect(&mut self.host, spec);
            }
            Err(error) => self.host.state = State::Unreachable(error.to_string()),
        }
    }

    fn ask_host_password(&mut self, failed: bool) {
        self.unlock_remote = true;
        self.unlock_failed = failed;
        self.unlock_reason = "Your sudo password on the host, to reach its controller.";
        self.unlock = Some(Input::default());
    }

    /// Connect again from nothing, asking for the password again if needed.
    pub fn reconnect_host(&mut self) {
        self.password_asked = false;
        self.host.disconnect();
    }

    /// Keep the overview in step with the host: connect when there is an
    /// address, take in what it said. Called between events.
    pub fn watch_host(&mut self) {
        if self.step != Step::Manage {
            return;
        }
        if self.host.state == State::Idle && self.unlock.is_none() && self.problems().is_empty() {
            self.connect_host();
        }
        let had_password = self.host.password.is_some();
        if let Some(note) = self.host.pump() {
            self.status = note;
        }
        // sudo did not take the password: ask again.
        if had_password && self.host.password.is_none() && self.unlock.is_none() {
            self.ask_host_password(true);
        }
        let last = self.entries().len().saturating_sub(1);
        if self.list.selected().unwrap_or(0) > last {
            self.list.select(Some(last));
        }
    }

    /// Run a task of the host: after its question and its arguments, if it has any.
    pub fn begin_task(&mut self, index: usize, confirmed: bool) {
        if let Some((index, values)) = self.host.begin_task(index, confirmed) {
            self.launch_task(index, values);
        }
    }

    /// Take the value typed for the argument being asked and go on to the
    /// next, or run the task after the last.
    pub fn accept_argument(&mut self) {
        match self.host.accept_argument() {
            Ok(Some((index, values))) => self.launch_task(index, values),
            Ok(None) => {}
            Err(missing) => self.status = missing,
        }
    }

    fn launch_task(&mut self, index: usize, values: std::collections::BTreeMap<String, String>) {
        let Some(task) = self.host.tasks.get(index).cloned() else {
            return;
        };
        let mut body = self.host.task_body(&task, &values);
        // A server sends its own secrets, as a session of the host like any other.
        if task.input.as_deref() == Some("secrets")
            && let Some(server) = self.server()
        {
            let _ = self.save();
            let sent = server.call("hosts.secrets", json!({"name": self.name()}), None);
            self.status = match sent {
                Ok(_) => "The berth server is sending the secrets. The session is below.".into(),
                Err(error) => error.to_string(),
            };
            return;
        }
        // Secrets are built here and travel on the task's standard input.
        if task.input.as_deref() == Some("secrets") {
            let payload =
                crate::registry::Store::at(self.config_dir.clone()).host_payload(&self.config);
            match payload {
                Ok(payload) => {
                    body["input_b64"] = json!(berth_api::b64(format!("{payload}\n").as_bytes()))
                }
                Err(error) => {
                    self.status = error.to_string();
                    return;
                }
            }
        }
        let _ = self.save();
        self.status = format!("Starting {}…", berth_tui::host::task_label(&task));
        self.host.run(&task.name, body);
    }

    /// A shell on the host as one of its sessions: the agent's, or the SSH user's.
    pub fn start_host_shell(&mut self) {
        let (user, title) = match self.as_agent {
            true => (
                "agent",
                format!("{} on {}", self.agent_label(), self.text(Field::SshHost)),
            ),
            false => ("operator", self.ssh_target()),
        };
        self.host.shell(user, &title);
        self.status = "Starting a shell…".into();
    }

    pub fn save(&mut self) -> Result<()> {
        let parent = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        // Temporary files are created private, which the profile should stay.
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer_pretty(&mut file, &self.config)?;
        writeln!(file)?;
        file.persist(&self.path)?;
        // A server's host is the server's to keep.
        if let Some(server) = self.server() {
            server
                .save_host(&self.name(), &self.config)
                .map_err(|error| format!("The berth server did not take the profile: {error}"))?;
        }
        self.saved = self.config.clone();
        Ok(())
    }

    /// Where each plugin is on this machine, by id.
    fn plugin_dirs(&self) -> Value {
        apply::plugin_dirs(&self.plugins)
    }

    /// The command for the chosen action, and the private files it reads.
    fn command(&self) -> Result<(Command, Vec<tempfile::NamedTempFile>)> {
        let action = self.action;
        // A server's host is installed by the server: berth itself asks it to.
        if let Some(server) = self.served() {
            let verb = match action {
                Action::Ssh => "apply",
                Action::CloudSeed => "seed",
                _ => {
                    return Err(format!(
                        "{} is done on a machine that keeps the host itself, not through the berth server at {server}",
                        action.label()
                    )
                    .into());
                }
            };
            let mut command = Command::new(std::env::current_exe()?);
            command.arg(verb).arg(self.name());
            return Ok((command, vec![]));
        }
        // Secrets go to private files rather than argv. They live until the command ends.
        let mut extra = tempfile::NamedTempFile::new()?;
        let inventory = tempfile::NamedTempFile::new()?;
        let mut command;
        match action {
            Action::Save | Action::Manage => return Err("Nothing to run".into()),
            Action::Release => {
                command = Command::new("python3");
                command.args(["bin/devbox.py", "release"]);
            }
            Action::UsbSeed | Action::CloudSeed => {
                command = Command::new("python3");
                command
                    .args(["bin/devbox.py", "render", "--kind"])
                    .arg(if action == Action::UsbSeed {
                        "usb"
                    } else {
                        "cloud"
                    })
                    .arg("--config")
                    .arg(&self.path);
                // A release that is not public: the seed carries the login of
                // the registry named for it.
                let store = self.text(Field::ReleaseStore);
                let login = match self.config["release_login"].as_str() {
                    Some(name) => Some(name),
                    None if self.enabled(Field::ReleasePrivate) && !store.is_empty() => {
                        store.split('/').next()
                    }
                    None => None,
                };
                if let (Action::CloudSeed, Some(name)) = (action, login) {
                    let login = crate::registry::Store::at(self.config_dir.clone())
                        .curl_login(name)?
                        .ok_or_else(|| {
                            format!(
                                "Registry {name} has no stored login for downloading the release"
                            )
                        })?;
                    command.env("BERTH_RELEASE_LOGIN", login);
                }
            }
            Action::Iso => {
                command = Command::new("python3");
                command
                    .args(["bin/devbox.py", "iso", "--config"])
                    .arg(&self.path)
                    .arg("--iso")
                    .arg(expand_path(self.text(Field::Iso)))
                    .arg("--overwrite");
                if !self.text(Field::IsoSha).is_empty() {
                    command.args(["--sha256", self.text(Field::IsoSha)]);
                }
            }
            // Applying to a host over SSH is berth-core's; the password the
            // overview was given saves Ansible asking for it again.
            Action::Ssh => {
                let paths = crate::paths::Paths::locate()?;
                let prepared =
                    apply::prepare(&paths, &self.host_profile(), self.host.password.as_deref())?;
                return Ok((prepared.command, prepared.files));
            }
            Action::Apply => {
                command = apply::playbook(Path::new("."));
                serde_json::to_writer(
                    &mut extra,
                    &apply::ansible_vars(
                        &self.config,
                        &self.plugin_dirs(),
                        Some(self.own_public.as_str()),
                    ),
                )?;
                extra.flush()?;
                command
                    .args(["ansible/site.yml", "--become", "-e"])
                    .arg(format!("@{}", extra.path().display()))
                    .arg("-i");
                if self.text(Field::Inventory).is_empty() {
                    command.args(["localhost,", "-c", "local"]);
                } else {
                    command.arg(expand_path(self.text(Field::Inventory)));
                }
                if self.enabled(Field::AskBecomePass) {
                    command.arg("--ask-become-pass");
                }
            }
            Action::Docker => {
                command = Command::new("docker");
                command.args(["compose", "-f", "container/compose.yml", "build"]);
                command.env("OPERATOR_USER", self.text(Field::OperatorUser));
                command.env("AGENT_USER", self.text(Field::AgentUser));
                for (env, field) in [
                    ("INSTALL_PLAYWRIGHT", Field::Playwright),
                    ("INSTALL_WEB_TOOLS", Field::ToolsWeb),
                    ("INSTALL_NATIVE_TOOLS", Field::ToolsNative),
                    ("INSTALL_JVM_TOOLS", Field::ToolsJvm),
                    ("INSTALL_DOTNET_TOOLS", Field::ToolsDotnet),
                    ("INSTALL_SCRIPTING_TOOLS", Field::ToolsScripting),
                    ("INSTALL_SCIENTIFIC_TOOLS", Field::ToolsScientific),
                    ("INSTALL_CHROME_TOOLS", Field::ToolsChrome),
                    ("SOFTWARE_RENDERING", Field::SoftwareRendering),
                ] {
                    command.env(env, self.enabled(field).to_string());
                }
                command.env(
                    "PLUGIN_FEATURES",
                    json!(setup::chosen_features(&self.config)).to_string(),
                );
            }
        }
        command.env("DEVBOX_BUILD_DIR", &self.build_dir);
        // The release archive packs the user's plugins next to the source's.
        command.env("BERTH_PLUGINS", self.plugin_dirs().to_string());
        command.env("BERTH_SSH_PUBLIC_KEY", &self.own_public);
        Ok((command, vec![extra, inventory]))
    }

    fn next_hint(&self) -> &'static str {
        match self.action {
            Action::UsbSeed => {
                "Next: build the ISO from this seed with the Build unattended ISO action."
            }
            Action::Iso => "Booting the installer erases the selected disk.",
            Action::CloudSeed => {
                "Next: upload the devbox-release.tar.gz shown above unchanged to the release URL, then pass the user-data file when creating the server."
            }
            Action::Docker => {
                "Next: docker compose -f container/compose.yml up -d in the source directory, then open http://127.0.0.1:16080/vnc.html"
            }
            Action::Apply | Action::Ssh => {
                "Next: log in as the agent and run `agent-env devbox-smoke`, then sign in to the coding clients."
            }
            _ => "",
        }
    }

    /// Actions that prompt for passwords, so they run in the normal terminal
    /// after the UI has closed. Everything else runs inside the UI.
    pub fn leaves_ui(&self) -> bool {
        matches!(self.action, Action::Save | Action::Apply | Action::Ssh)
    }

    /// Save the profile and run the chosen action in the normal terminal.
    pub fn execute(&mut self) -> Result<()> {
        let tab_action = self.action;
        if let Some(action) = self.terminal_action.take() {
            self.action = action;
        }
        let outcome = self.execute_action();
        self.action = tab_action;
        outcome
    }

    /// For a cloud seed with a store chosen: upload the release there and take
    /// its address, so the seed points at what was just published.
    fn publish_release(&mut self) -> Result<()> {
        let store = self.text(Field::ReleaseStore).to_owned();
        // A server publishes the release itself when it renders the seed.
        if self.action != Action::CloudSeed || store.is_empty() || self.served().is_some() {
            return Ok(());
        }
        let url = crate::registry::publish_from(
            &crate::registry::Store::at(self.config_dir.clone()),
            &self.build_dir,
            &self.plugin_dirs(),
            &store,
        )
        .map_err(|error| format!("Publishing to {store} failed: {error}"))?;
        self.set(Field::ReleaseUrl, json!(url));
        Ok(())
    }

    fn execute_action(&mut self) -> Result<()> {
        self.publish_release()?;
        self.save()?;
        println!("Saved {}", self.path.display());
        if self.action == Action::Save {
            return Ok(());
        }
        let (mut command, _files) = self.command()?;
        let status = command.status().map_err(|error| {
            format!(
                "Could not start {}: {error}",
                command.get_program().to_string_lossy()
            )
        })?;
        if !status.success() {
            return Err(format!("{} failed ({status})", self.action.label()).into());
        }
        println!("{}", self.next_hint());
        Ok(())
    }

    /// Run the chosen action inside the UI. Writing a USB drive first asks for
    /// confirmation and for administrator rights.
    pub fn run(&mut self, confirmed: bool) {
        // The overview has no action of its own: its rows are the host's tasks.
        if self.action == Action::Manage {
            return;
        }
        let mut usb = String::new();
        if self.action == Action::Iso {
            usb = self.text(Field::UsbDevice).to_owned();
        }
        if !usb.is_empty() {
            self.usb_drives = discovery::usb_drives();
            let Some(drive) = self.usb_drives.iter().find(|(_, path)| *path == usb) else {
                self.status = format!("{usb} is not a connected USB drive");
                return;
            };
            if !confirmed {
                self.confirm = Some(format!("Erase {}?", drive.0));
                return;
            }
            if !self.privileged {
                self.pending_run = true;
                self.request_unlock("Write the installer to the USB drive.");
                if !self.privileged {
                    return;
                }
            }
        }
        self.pending_run = false;
        if let Err(error) = self.start_job(&usb) {
            self.status = error.to_string();
        }
    }

    /// Close the finished job's screen and take over what it produced.
    pub fn close_job(&mut self) {
        let Some(job) = self.job.take() else {
            return;
        };
        let succeeded = job.outcome == Some(Ok(()));
        if std::mem::take(&mut self.fetching_iso) {
            if let (true, Some(path)) = (succeeded, job.lines.last()) {
                self.commit(Field::Iso, path);
            }
            return;
        }
        self.status = if succeeded {
            format!("{} finished", job.title)
        } else {
            format!("{} failed", job.title)
        };
    }

    /// A shell over plain SSH in a terminal inside the UI, as the SSH user: for
    /// a host whose controller cannot be reached.
    pub fn start_shell(&mut self, rows: u16, cols: u16) {
        let title = self.ssh_target();
        let started = self.terminal(&[]).and_then(|command| {
            let _ = self.save();
            crate::session::Session::start(&title, command, rows, cols)
        });
        match started {
            Ok(mut session) => {
                session.closes = true;
                self.session = Some(session);
            }
            Err(error) => self.status = error.to_string(),
        }
    }

    /// `ssh` with this host's port and key.
    fn ssh(&self) -> Command {
        self.host_profile().ssh()
    }

    fn ssh_target(&self) -> String {
        self.host_profile().target()
    }

    /// An interactive shell on the host, or `remote` run there.
    pub fn terminal(&self, remote: &[&str]) -> Result<Command> {
        if self.text(Field::SshHost).is_empty() {
            return Err("SSH host is required".into());
        }
        let mut command = self.ssh();
        command.arg(self.ssh_target()).args(remote);
        Ok(command)
    }

    /// An interactive shell as the agent. Root login and the agent's own key are
    /// not a given, so it goes through the SSH user and sudo. `agent-env` adds the
    /// display and tool paths; a host without it still gets a login shell.
    pub fn agent_terminal(&self) -> Result<Command> {
        let agent = self.agent_name()?;
        let mut command = self.terminal(&[])?;
        command.arg("-t").arg(format!(
            "sudo -iu {agent} sh -c 'command -v agent-env >/dev/null && exec agent-env; exec bash -l'"
        ));
        Ok(command)
    }

    /// The agent's account name, checked because it ends up in a remote shell command.
    fn agent_name(&self) -> Result<&str> {
        let agent = match self.text(Field::AgentUser) {
            "" => "agent",
            user => user,
        };
        if !agent
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
            || agent.starts_with('-')
        {
            return Err("Agent username is not a valid account name".into());
        }
        Ok(agent)
    }

    /// Remove a field's value.
    pub fn clear(&mut self, field: Field) {
        if field.can_clear() {
            self.commit(field, "");
            if field == Field::Iso {
                self.commit(Field::IsoSha, "");
            }
            self.status = format!("{} unset", field.spec().label);
        }
    }

    /// Write the ISO that was just built to a USB drive: the only one connected,
    /// the given one, or ask which.
    pub fn write_built_iso(&mut self, device: Option<String>) {
        self.usb_drives = discovery::usb_drives();
        let device = match (device, self.usb_drives.as_slice()) {
            (Some(device), _) => device,
            (None, [(_, only)]) => only.clone(),
            (None, []) => {
                if let Some(job) = &mut self.job {
                    let hint = "No usable USB drive. Plug one in (replug it if it was ejected) and press w.";
                    job.lines.push(hint.into());
                }
                return;
            }
            (None, _) => {
                self.choosing_drive = true;
                return;
            }
        };
        self.choosing_drive = false;
        self.job = None;
        self.set(Field::UsbDevice, json!(device));
        self.write_only = true;
        self.run(false);
    }

    fn start_job(&mut self, usb: &str) -> Result<()> {
        self.publish_release()?;
        if self.action != Action::Release {
            self.save()?;
        }
        let mut stages = vec![];
        let mut files = vec![];
        if !std::mem::take(&mut self.write_only) {
            let command;
            (command, files) = self.command()?;
            stages.push(Stage {
                title: self.action.label().into(),
                command,
                source: None,
                input: None,
            });
        }
        let mut next = self.next_hint();
        if !usb.is_empty() {
            let iso = self
                .build_dir
                .join(format!("{}-autoinstall.iso", self.text(Field::Hostname)));
            let mut write = Command::new("sudo");
            write
                .args(["-n", "bash", "bin/write-usb.sh"])
                .arg(&iso)
                .arg(usb);
            stages.push(Stage {
                title: format!("Write to {usb}"),
                command: write,
                source: Some(iso),
                input: None,
            });
            next = "Next: boot the target from the USB drive. Booting it erases the selected disk.";
        }
        self.job = Some(Job::start(stages, next, files)?);
        Ok(())
    }
}

/// Load a profile and fill in everything the UI expects to be present.
pub fn parse_config(data: &[u8]) -> Result<Value> {
    let mut config: Value = serde_json::from_slice(data)?;
    if !config.is_object() {
        return Err("Configuration must be a JSON object".into());
    }
    for field in ["wifi", "ansible_vars", "builder"] {
        if !config[field].is_null() && !config[field].is_object() {
            return Err(format!("{field} must be a JSON object or null").into());
        }
    }
    for key in ["ansible_vars", "builder"] {
        if config[key].is_null() {
            config[key] = json!({});
        }
    }
    for (key, value) in [
        ("profile", json!("laptop")),
        ("keyboard", json!("de")),
        ("operator_username", json!("ops")),
        ("agent_username", json!("agent")),
        ("operator_passwordless_sudo", json!(false)),
        ("agent_passwordless_sudo", json!(true)),
        ("target_disk_ssd", json!("any")),
    ] {
        if config[key].is_null() {
            config[key] = value;
        }
    }
    // Placeholders from older example profiles count as unset.
    for key in [
        "operator_password_hash",
        "target_disk_serial",
        "release_url",
    ] {
        if config[key].as_str().is_some_and(|v| v.contains("REPLACE")) {
            config[key] = json!("");
        }
    }
    if config["target_disk_mode"].is_null() {
        let has_serial = config["target_disk_serial"]
            .as_str()
            .is_some_and(|serial| !serial.is_empty());
        config["target_disk_mode"] = json!(if has_serial { "serial" } else { "auto" });
    }
    if config["wifi_enabled"].is_null() {
        let configured = config["wifi"].as_object().is_some_and(|w| !w.is_empty());
        config["wifi_enabled"] = json!(configured);
    }
    // The coding agents, T3 Code and GitHub's CLI were switches once and are
    // features of plugins that come with berth now. They go first: later
    // plugins, such as Unity's, set themselves up for the agents they find.
    if config["builder"]["shipped_plugins"] != true {
        let vars = &config["ansible_vars"];
        let on = |key: &str, default: bool| vars[key].as_bool().unwrap_or(default);
        let agents = on("devbox_install_agents", true);
        let mut features: Vec<Value> = [
            ("claude/cli", agents && on("devbox_agent_claude", true)),
            ("codex/cli", agents && on("devbox_agent_codex", true)),
            ("github/cli", true),
            ("pi/cli", agents && on("devbox_agent_pi", true)),
            ("t3/server", on("devbox_install_t3", agents)),
        ]
        .into_iter()
        .filter(|(_, chosen)| *chosen)
        .map(|(feature, _)| json!(feature))
        .collect();
        for feature in vars["devbox_plugin_features"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if !features.contains(feature) {
                features.push(feature.clone());
            }
        }
        if !config["ansible_vars"].is_object() {
            config["ansible_vars"] = json!({});
        }
        let vars = config["ansible_vars"].as_object_mut().unwrap();
        for old in [
            "devbox_install_agents",
            "devbox_agent_claude",
            "devbox_agent_codex",
            "devbox_agent_pi",
            "devbox_install_t3",
        ] {
            vars.remove(old);
        }
        vars.insert("devbox_plugin_features".into(), json!(features));
        config["builder"]["shipped_plugins"] = json!(true);
    }
    // Profiles from before the privilege levels keep what they had: sudo for
    // everything without a password is `full`. One whose agent types a
    // password stays as it is until a level is chosen.
    if config["agent_privileges"].is_null() && config["agent_passwordless_sudo"] == true {
        config["agent_privileges"] = json!("full");
    }
    // The task last run from the overview was kept once; the host lists its own now.
    if let Some(builder) = config["builder"].as_object_mut() {
        builder.remove("manage_task");
    }
    // Prompt switches were once stored as the strings "true" and "false".
    // Device names change between sessions, so the drive to write is chosen each time.
    if let Some(builder) = config["builder"].as_object_mut() {
        builder.remove("usb_device");
    }
    for (key, default) in [("ask_become_pass", true), ("ask_pass", false)] {
        let value = &config["builder"][key];
        let enabled = value
            .as_bool()
            .or_else(|| value.as_str().map(|text| text == "true"))
            .unwrap_or(default);
        config["builder"][key] = json!(enabled);
    }
    Ok(config)
}
