//! The set-up pages of a host: what its profile holds, the rows each page
//! shows of it, and what changing a row does to it. Nothing here reads a file
//! or runs a command, so berth and its page in a browser share it.

use crate::fields::{Action, Field, Group, Kind, SOURCES, Step};
use crate::theme;
use ratatui::{
    style::Style,
    text::{Line, Span},
};
use serde_json::{Value, json};

/// Something a plugin installs, switched on or off for a host.
#[derive(Clone, Debug, PartialEq)]
pub struct Feature {
    /// `PLUGIN/FEATURE`, as the profile names it.
    pub key: String,
    pub label: String,
    /// The group of the Features page it is listed in.
    pub group: String,
    pub help: String,
}

/// What a selectable row of a set-up page does when activated.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Entry {
    Source(usize),
    Action(Action),
    Field(Field),
    /// The heading of a group on the Features page.
    Group(usize),
    /// A plugin's feature, by index into the features there are.
    Feature(usize),
    /// A registry this host can be given, by index into the registries there are.
    Registry(usize),
    /// A secret this host can be given, by index into the secrets there are.
    Secret(usize),
    /// One of an account's SSH keys by its place among them, or the row that
    /// adds one.
    Key(Account, Option<usize>),
    Run,
}

/// An account of a host that takes SSH keys.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Account {
    Operator,
    Agent,
}

impl Account {
    /// Its name in a profile's fields.
    pub fn name(self) -> &'static str {
        match self {
            Account::Operator => "operator",
            Account::Agent => "agent",
        }
    }

    fn field(self) -> Field {
        match self {
            Account::Operator => Field::OperatorKey,
            Account::Agent => Field::AgentKey,
        }
    }
}

pub use berth_api::profile::account_keys;

/// What the row that adds a key is for.
pub const ADD_KEY_HELP: &str = "Add an OpenSSH public key to the account: the line of a `.pub` file, such as `ssh-ed25519 AAAA… you@machine`. Paste it and press Enter.";

/// Set one of an account's keys, add one (`index` is `None`), or with
/// nothing typed remove it. The message says why it was not taken.
pub fn set_key(
    config: &mut Value,
    account: Account,
    index: Option<usize>,
    value: &str,
) -> Result<(), String> {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut keys = account_keys(config, account.name());
    if value.is_empty() {
        if let Some(index) = index.filter(|index| *index < keys.len()) {
            keys.remove(index);
        }
    } else if !is_public_key(&value) {
        return Err(
            "Not an OpenSSH public key: it starts with ssh-ed25519, ssh-rsa or ecdsa-sha2-nistp256"
                .into(),
        );
    } else if keys
        .iter()
        .enumerate()
        .any(|(place, known)| *known == value && Some(place) != index)
    {
        return Err("The account has that key already".into());
    } else {
        match index.filter(|index| *index < keys.len()) {
            Some(index) => keys[index] = value,
            None => keys.push(value),
        }
    }
    berth_api::profile::set_account_keys(config, account.name(), &keys);
    Ok(())
}

/// Something a group of the Features page switches.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Item {
    Field(Field),
    /// Index into the features there are.
    Feature(usize),
}

pub struct FeatureGroup {
    pub title: String,
    pub items: Vec<Item>,
}

pub struct Row {
    pub line: Line<'static>,
    /// `None` for headings and other rows the cursor skips.
    pub entry: Option<Entry>,
}

impl Row {
    pub fn info(line: impl Into<Line<'static>>) -> Self {
        Row {
            line: line.into(),
            entry: None,
        }
    }

    pub fn entry(line: impl Into<Line<'static>>, entry: Entry) -> Self {
        Row {
            line: line.into(),
            entry: Some(entry),
        }
    }

    pub fn heading(text: &str) -> Self {
        Row::info(Line::styled(
            text.to_owned(),
            Style::new().fg(theme::ACCENT).bold(),
        ))
    }
}

pub fn set_pointer(config: &mut Value, pointer: &str, value: Value) {
    let mut node = config;
    let mut parts = pointer.trim_start_matches('/').split('/').peekable();
    while let Some(part) = parts.next() {
        if !node.is_object() {
            *node = json!({});
        }
        if parts.peek().is_none() {
            node[part] = value;
            return;
        }
        node = &mut node[part];
    }
}

pub fn value_line(label: &str, value: &str, kind: Kind) -> Line<'static> {
    let (answer, style) = match kind {
        Kind::Toggle if value == "true" => ("● On", Style::new().fg(theme::GOOD)),
        Kind::Toggle => ("○ Off", Style::new().fg(theme::MUTED)),
        _ if value.is_empty() => ("Not set", Style::new().fg(theme::MUTED).italic()),
        Kind::Secret => ("••••••", Style::new().fg(theme::ACCENT)),
        _ => (value, Style::new().fg(theme::ACCENT)),
    };
    Line::from(vec![
        Span::styled(format!("{label}  "), Style::new().fg(theme::TEXT)),
        Span::styled(answer.to_owned(), style),
    ])
}

/// An SSH public key as type, the end of the key and its comment.
pub fn short_key(key: &str) -> String {
    let mut parts = key.split_whitespace();
    match (parts.next(), parts.next()) {
        (Some(kind), Some(data)) if data.len() > 12 && data.is_ascii() => {
            let comment = parts.collect::<Vec<_>>().join(" ");
            format!("{kind} …{} {comment}", &data[data.len() - 8..])
                .trim_end()
                .to_owned()
        }
        _ => key.to_owned(),
    }
}

/// Names Ubuntu already uses for system accounts or groups; useradd refuses them.
const RESERVED_NAMES: &[&str] = &[
    "root", "daemon", "bin", "sys", "adm", "tty", "disk", "lp", "mail", "news", "uucp", "man",
    "proxy", "kmem", "dialout", "fax", "voice", "cdrom", "floppy", "tape", "sudo", "audio", "dip",
    "www-data", "backup", "operator", "list", "irc", "src", "shadow", "utmp", "video", "sasl",
    "plugdev", "staff", "games", "users", "nogroup", "admin", "docker",
];

pub fn valid_username(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= 32
        && !RESERVED_NAMES.contains(&name)
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

pub fn valid_hostname(name: &str) -> bool {
    name.len() <= 63
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && !name.ends_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

pub fn valid_password_hash(hash: &str) -> bool {
    (hash.starts_with("$6$") || hash.starts_with("$y$"))
        && hash.len() >= 40
        && !hash.contains(char::is_whitespace)
}

pub fn is_public_key(value: &str) -> bool {
    let mut parts = value.split_whitespace();
    matches!(
        parts.next(),
        Some("ssh-ed25519" | "ssh-rsa" | "ecdsa-sha2-nistp256")
    ) && parts.next().is_some()
        && !value.contains(['\n', '\r'])
}

/// A name for a secret, a registry or a plugin: safe as a file name and inside
/// a remote command.
pub fn valid_name(name: &str) -> bool {
    name.starts_with(|ch: char| ch.is_ascii_lowercase())
        && name
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' || ch == '_')
}

fn names(config: &Value, key: &str) -> Vec<Value> {
    config[key].as_array().cloned().unwrap_or_default()
}

/// Give the host a registry's login, or take it away again.
pub fn toggle_registry(config: &mut Value, name: &str) {
    let name = json!(name);
    let mut registries = names(config, "registries");
    match registries.iter().position(|known| *known == name) {
        Some(position) => {
            registries.remove(position);
        }
        None => registries.push(name),
    }
    config["registries"] = json!(registries);
}

/// What to type for how a host gets a secret.
pub const SECRET_HOW: &str =
    "A variable name such as GH_TOKEN, or a path such as ~/.config/app/token";

/// Set how the host gets a secret: a variable's name, a file's path, or
/// nothing for not at all. Anything else is refused.
pub fn set_secret(config: &mut Value, name: &str, how: &str) -> Result<(), &'static str> {
    let how = how.trim();
    let mut secrets = names(config, "secrets");
    let known = secrets.iter().position(|entry| entry["name"] == name);
    let variable = how.starts_with(|ch: char| ch.is_ascii_alphabetic() || ch == '_')
        && how
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_');
    let entry = if how.is_empty() {
        None
    } else if variable {
        Some(json!({"name": name, "env": how}))
    } else if how.starts_with('~')
        || how.starts_with('/')
        || how.contains('/')
        || how.starts_with('.')
    {
        // An earlier file keeps the mode given on the command line.
        let mode = known.and_then(|position| secrets[position]["mode"].as_str().map(str::to_owned));
        let mut entry = json!({"name": name, "file": how});
        if let Some(mode) = mode {
            entry["mode"] = json!(mode);
        }
        Some(entry)
    } else {
        return Err(SECRET_HOW);
    };
    if let Some(position) = known {
        secrets.remove(position);
    }
    secrets.extend(entry);
    config["secrets"] = json!(secrets);
    Ok(())
}

/// The plugin features a profile has switched on, as `PLUGIN/FEATURE`.
pub fn chosen_features(config: &Value) -> Vec<String> {
    config["ansible_vars"]["devbox_plugin_features"]
        .as_array()
        .map(|names| {
            names
                .iter()
                .filter_map(|name| name.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Switch a feature. The chosen ones are stored in the order the plugins
/// declare them, which is the order the role applies them in. Ones of plugins
/// not present stay.
pub fn set_feature(config: &mut Value, features: &[Feature], index: usize, on: bool) {
    let key = &features[index].key;
    let mut chosen = chosen_features(config);
    chosen.retain(|known| known != key);
    if on {
        chosen.push(key.clone());
    }
    let mut ordered: Vec<String> = features
        .iter()
        .filter(|feature| chosen.contains(&feature.key))
        .map(|feature| feature.key.clone())
        .collect();
    chosen.retain(|key| !ordered.contains(key));
    ordered.extend(chosen);
    set_pointer(
        config,
        "/ansible_vars/devbox_plugin_features",
        json!(ordered),
    );
}

/// Take what was typed for a field. The message says why it was not taken.
pub fn commit(config: &mut Value, field: Field, value: &str) -> Result<(), String> {
    let spec = field.spec();
    let value = if spec.kind == Kind::Secret {
        value
    } else {
        value.trim()
    };
    if let Kind::Choice(choices) = spec.kind
        && !choices.contains(&value)
    {
        return Err(format!("Choose one of: {}", choices.join(", ")));
    }
    if let Some(name) = spec.pointer.strip_prefix("/ansible_vars/")
        && value.is_empty()
    {
        // An empty override would replace the role default with nothing.
        if let Some(vars) = config["ansible_vars"].as_object_mut() {
            vars.remove(name);
        }
        return Ok(());
    }
    set_pointer(config, spec.pointer, json!(value));
    if spec.pointer.starts_with("/wifi/")
        && config["wifi"]
            .as_object()
            .is_some_and(|wifi| wifi.values().all(|v| v.as_str() == Some("")))
    {
        config["wifi"] = Value::Null;
    }
    Ok(())
}

/// The file servers and raw repositories among the registries, where a
/// release can be published: `NAME` or `NAME/REPOSITORY`.
pub fn release_stores(registries: &serde_json::Map<String, Value>) -> Vec<String> {
    let mut stores = vec![];
    for (name, registry) in registries {
        if registry["type"] == "raw" {
            stores.push(name.clone());
        }
        for repository in registry["repositories"].as_array().into_iter().flatten() {
            if registry["type"] == "nexus" && repository["format"] == "raw" {
                stores.push(format!(
                    "{name}/{}",
                    repository["name"].as_str().unwrap_or("")
                ));
            }
        }
    }
    stores
}

/// What comes after `current` when stepping through `all` and then nothing.
pub fn next_of(all: &[String], current: &str) -> String {
    match all.iter().position(|known| known == current) {
        None if current.is_empty() => all.first(),
        None => None,
        Some(index) => all.get(index + 1),
    }
    .cloned()
    .unwrap_or_default()
}

fn branch(index: usize, count: usize) -> &'static str {
    if index + 1 < count {
        "  ├─ "
    } else {
        "  └─ "
    }
}

/// A profile as its set-up pages see it.
#[derive(Clone, Copy)]
pub struct Profile<'a> {
    pub config: &'a Value,
    pub step: Step,
    /// How it is to be installed, or `Manage` on its overview.
    pub action: Action,
    /// What plugins can install.
    pub features: &'a [Feature],
    /// The registries and secrets there are to give.
    pub registries: &'a [String],
    pub secrets: &'a [String],
    /// The public SSH key of the berth that sets the host up; empty when it
    /// is not known.
    pub own_key: &'a str,
}

impl<'a> Profile<'a> {
    pub fn keys(&self, account: Account) -> Vec<String> {
        account_keys(self.config, account.name())
    }

    pub fn text(&self, field: Field) -> &'a str {
        self.config
            .pointer(field.spec().pointer)
            .and_then(Value::as_str)
            .unwrap_or("")
    }

    /// State of a toggle. Unset feature switches are on, like the role defaults,
    /// and tool categories follow the older single `devbox_install_dev_tools` switch.
    /// Asking for the sudo password follows the operator's sudo.
    pub fn enabled(&self, field: Field) -> bool {
        use berth_api::profile;
        let pointer = field.spec().pointer;
        if field == Field::AskBecomePass {
            return profile::asks_become_pass(self.config);
        }
        let stored = self.config.pointer(pointer).and_then(Value::as_bool);
        stored.unwrap_or_else(|| {
            if matches!(
                field,
                Field::BerthKey | Field::OperatorSudo | Field::AgentSudo
            ) {
                true
            } else if pointer.starts_with("/ansible_vars/devbox_tools_") {
                self.config["ansible_vars"]["devbox_install_dev_tools"]
                    .as_bool()
                    .unwrap_or(true)
            } else {
                pointer.starts_with("/ansible_vars/")
            }
        })
    }

    /// The `berth serve` this host belongs to, when it is not a host of this machine.
    pub fn served(&self) -> Option<&'a str> {
        self.config["builder"]["served"].as_str()
    }

    /// A server's host that may be used here and not set up.
    pub fn locked(&self) -> bool {
        self.config["builder"]["served_only"] == true
    }

    pub fn has_registry(&self, index: usize) -> bool {
        names(self.config, "registries").contains(&json!(self.registries[index]))
    }

    /// How this host gets a secret: `GH_TOKEN` for a variable, a path for a
    /// file, nothing when it does not get it.
    pub fn secret_how(&self, index: usize) -> String {
        let name = &self.secrets[index];
        names(self.config, "secrets")
            .iter()
            .find(|entry| entry["name"] == name.as_str())
            .and_then(|entry| entry["env"].as_str().or(entry["file"].as_str()))
            .unwrap_or("")
            .to_owned()
    }

    /// The rows of the Secrets page.
    pub fn access_rows(&self) -> Vec<Row> {
        let muted = Style::new().fg(theme::MUTED);
        let mut rows = vec![Row::heading("Registries")];
        if self.registries.is_empty() {
            rows.push(Row::info(Line::styled(
                "  none yet · add them on the Registries tab of the home screen",
                muted,
            )));
        }
        for (index, name) in self.registries.iter().enumerate() {
            let mut line = value_line(name, &self.has_registry(index).to_string(), Kind::Toggle);
            line.spans
                .insert(0, Span::styled(branch(index, self.registries.len()), muted));
            rows.push(Row::entry(line, Entry::Registry(index)));
        }
        rows.push(Row::heading("Secrets"));
        if self.secrets.is_empty() {
            rows.push(Row::info(Line::styled(
                "  none yet · add them on the Secrets tab of the home screen",
                muted,
            )));
        }
        for (index, name) in self.secrets.iter().enumerate() {
            let how = self.secret_how(index);
            let mut line = Line::from(vec![
                Span::styled(branch(index, self.secrets.len()), muted),
                Span::styled(format!("{name}  "), Style::new().fg(theme::TEXT)),
            ]);
            line.spans.push(if how.is_empty() {
                Span::styled("○ Not given", muted)
            } else if how.contains('/') || how.starts_with(['~', '.']) {
                Span::styled(format!("● file {how}"), Style::new().fg(theme::GOOD))
            } else {
                Span::styled(format!("● variable {how}"), Style::new().fg(theme::GOOD))
            });
            rows.push(Row::entry(line, Entry::Secret(index)));
        }
        // What the profile names but there is no value for.
        for entry in names(self.config, "secrets") {
            let name = entry["name"].as_str().unwrap_or("");
            if !self.secrets.iter().any(|known| known == name) {
                rows.push(Row::info(Line::styled(
                    format!("✗ {name} is given to this host but has no value stored here"),
                    Style::new().fg(theme::BAD),
                )));
            }
        }
        for name in names(self.config, "registries") {
            if !self.registries.iter().any(|known| name == known.as_str()) {
                rows.push(Row::info(Line::styled(
                    format!(
                        "✗ Registry {} is given to this host but not configured here",
                        name.as_str().unwrap_or("")
                    ),
                    Style::new().fg(theme::BAD),
                )));
            }
        }
        rows.push(Row::info(""));
        rows.push(Row::info(Line::styled(
            "They reach a running host with \"Install secrets and registry logins\" on its overview.",
            muted,
        )));
        rows
    }

    /// The groups of the Features page: the built-in ones, with what plugins add
    /// to them, then groups only plugins have.
    pub fn feature_groups(&self) -> Vec<FeatureGroup> {
        use Field::*;
        // The first two are filled by plugins that come with berth.
        let builtin: [(&str, &[Field]); 5] = [
            ("Coding Agents", &[]),
            ("Agent Orchestrator", &[]),
            (
                "Development Tools",
                &[
                    ToolsWeb,
                    ToolsNative,
                    ToolsJvm,
                    ToolsDotnet,
                    ToolsScripting,
                    ToolsScientific,
                ],
            ),
            ("Browsers", &[ToolsChrome, Playwright]),
            ("Desktop", &[SoftwareRendering]),
        ];
        let mut groups: Vec<FeatureGroup> = builtin
            .iter()
            .map(|(title, fields)| FeatureGroup {
                title: (*title).to_owned(),
                items: fields.iter().map(|field| Item::Field(*field)).collect(),
            })
            .collect();
        for (index, feature) in self.features.iter().enumerate() {
            let item = Item::Feature(index);
            let known = groups
                .iter_mut()
                .find(|group| group.title.eq_ignore_ascii_case(&feature.group));
            match known {
                Some(group) => group.items.push(item),
                None => groups.push(FeatureGroup {
                    title: feature.group.clone(),
                    items: vec![item],
                }),
            }
        }
        groups.retain(|group| !group.items.is_empty());
        groups
    }

    pub fn item_enabled(&self, item: Item) -> bool {
        match item {
            Item::Field(field) => self.enabled(field),
            Item::Feature(index) => {
                chosen_features(self.config).contains(&self.features[index].key)
            }
        }
    }

    /// Whether everything in a group is on, which is when switching it turns it off.
    pub fn group_all_on(&self, index: usize) -> bool {
        self.feature_groups()[index]
            .items
            .iter()
            .all(|item| self.item_enabled(*item))
    }

    pub fn feature_rows(&self) -> Vec<Row> {
        let mut rows = vec![];
        for (index, group) in self.feature_groups().into_iter().enumerate() {
            let on = group
                .items
                .iter()
                .filter(|item| self.item_enabled(**item))
                .count();
            rows.push(Row::entry(
                Line::from(vec![
                    Span::styled(group.title, Style::new().fg(theme::ACCENT).bold()),
                    Span::styled(
                        format!("  {on} of {}", group.items.len()),
                        Style::new().fg(theme::MUTED),
                    ),
                ]),
                Entry::Group(index),
            ));
            for (number, item) in group.items.iter().enumerate() {
                let label = match *item {
                    Item::Field(field) => field.spec().label,
                    Item::Feature(index) => &self.features[index].label,
                };
                let mut line =
                    value_line(label, &self.item_enabled(*item).to_string(), Kind::Toggle);
                line.spans.insert(
                    0,
                    Span::styled(
                        branch(number, group.items.len()),
                        Style::new().fg(theme::MUTED),
                    ),
                );
                rows.push(Row::entry(
                    line,
                    match *item {
                        Item::Field(field) => Entry::Field(field),
                        Item::Feature(index) => Entry::Feature(index),
                    },
                ));
            }
        }
        rows
    }

    /// Fields shown by the current step for the chosen action.
    pub fn fields(&self) -> Vec<Field> {
        use Field::*;
        match self.step {
            Step::Features => vec![
                ToolsWeb,
                ToolsNative,
                ToolsJvm,
                ToolsDotnet,
                ToolsScripting,
                ToolsScientific,
                ToolsChrome,
                Playwright,
                SoftwareRendering,
            ],
            Step::Install | Step::Manage => self.settings(),
            Step::Secrets => vec![],
            // The container only has the agent account.
            Step::Accounts if self.action == Action::Docker => vec![AgentUser],
            Step::Accounts => vec![
                OperatorUser,
                OperatorKey,
                BerthKey,
                OperatorHash,
                OperatorSudo,
                AgentUser,
                AgentHash,
                AgentKey,
                AgentPrivileges,
                AgentSudo,
            ],
        }
    }

    fn settings(&self) -> Vec<Field> {
        use Field::*;
        let action = self.action;
        if matches!(action, Action::Save | Action::Release | Action::Docker) {
            return vec![];
        }
        if action == Action::Manage {
            // How a server's host is reached is for whoever sets the server up.
            if self.locked() {
                return vec![];
            }
            return vec![SshHost, SshUser, SshPort, SshKey, AskBecomePass];
        }
        let mut fields = vec![Profile];
        if action.installer() || action == Action::CloudSeed {
            fields.push(Hostname);
        }
        if action.installer() {
            fields.push(DiskMode);
            let mode = self.text(DiskMode);
            match mode {
                "serial" => fields.push(DiskSerial),
                "model" => fields.push(DiskModel),
                "path" => fields.push(DiskPath),
                _ => {}
            }
            if !matches!(mode, "serial" | "ssd" | "hdd") {
                fields.push(DiskType);
            }
            fields.push(Keyboard);
        }
        fields.push(Geometry);
        if action.installer() {
            fields.push(Wifi);
            if self.enabled(Wifi) {
                fields.extend([WifiSsid, WifiPassword, WifiInterface]);
            }
        }
        if action == Action::CloudSeed {
            fields.push(ReleaseStore);
            fields.push(if self.text(ReleaseStore).is_empty() {
                ReleaseUrl
            } else {
                ReleasePrivate
            });
        }
        fields.push(RecoveryCidr);
        match action {
            Action::Iso => fields.extend([Iso, IsoSha]),
            Action::Apply => fields.extend([Inventory, AskBecomePass]),
            Action::Ssh => {
                fields.extend([SshHost, SshUser, SshPort, SshKey, AskPass, AskBecomePass])
            }
            _ => {}
        }
        fields.extend([HardenSsh, Cockpit, Docker, Tailscale]);
        if self.enabled(Tailscale) {
            fields.push(TailscaleKey);
        }
        fields
    }

    pub fn field_rows(&self) -> Vec<Row> {
        let fields = self.fields();
        let mut rows = Vec::new();
        for (index, &field) in fields.iter().enumerate() {
            let group = field.group();
            if let Some(Group::Heading(heading)) = group
                && (index == 0 || fields[index - 1].group() != group)
            {
                rows.push(Row::heading(heading));
            }
            let prefix = match group {
                None => "",
                Some(_)
                    if fields
                        .get(index + 1)
                        .is_some_and(|next| next.group() == group) =>
                {
                    "  ├─ "
                }
                Some(_) => "  └─ ",
            };
            // An account's keys are a row each, and one more that adds a key.
            let account = [Account::Operator, Account::Agent]
                .into_iter()
                .find(|account| account.field() == field);
            if let Some(account) = account {
                let muted = Style::new().fg(theme::MUTED);
                for (place, key) in self.keys(account).iter().enumerate() {
                    let mut line = value_line("SSH key", &short_key(key), Kind::Text);
                    line.spans.insert(0, Span::styled(prefix, muted));
                    rows.push(Row::entry(line, Entry::Key(account, Some(place))));
                }
                rows.push(Row::entry(
                    Line::from(vec![
                        Span::styled(prefix, muted),
                        Span::styled("+ Add SSH key", Style::new().fg(theme::ACCENT)),
                    ]),
                    Entry::Key(account, None),
                ));
                continue;
            }
            let spec = field.spec();
            let value = if spec.kind == Kind::Toggle {
                self.enabled(field).to_string()
            } else {
                self.text(field).to_owned()
            };
            let (value, kind) = match field {
                // Enough of the key to recognise it, not enough to use it.
                Field::TailscaleKey if value.len() > 24 && value.is_ascii() => (
                    format!(
                        "{}…{}",
                        &value[..value.len().min(16)],
                        &value[value.len() - 4..]
                    ),
                    Kind::Text,
                ),
                _ => (value, spec.kind),
            };
            let mut line = value_line(spec.label, &value, kind);
            line.spans
                .insert(0, Span::styled(prefix, Style::new().fg(theme::MUTED)));
            rows.push(Row::entry(line, Entry::Field(field)));
        }
        if rows.is_empty() {
            rows.push(Row::info("This action needs no settings here."));
        }
        rows
    }

    /// The rows of the Accounts page that fill it in from this machine.
    pub fn source_rows() -> Vec<Row> {
        let mut rows = vec![Row::info(""), Row::heading("Fill In From This Machine")];
        for (index, (label, _)) in SOURCES.iter().enumerate() {
            rows.push(Row::entry(format!("  {label}"), Entry::Source(index)));
        }
        rows
    }

    /// The ways to install on the Install page, with the chosen one marked.
    pub fn method_rows(&self, methods: &[Action]) -> Vec<Row> {
        let mut rows = vec![Row::heading("How To Install")];
        for action in methods.iter().copied() {
            let mark = if action == self.action { "●" } else { "○" };
            rows.push(Row::entry(
                format!("  {mark} {}", action.label()),
                Entry::Action(action),
            ));
        }
        rows
    }

    /// What the Install page warns of when SSH is open to every address.
    pub fn open_ssh_warning(&self) -> Option<&'static str> {
        let cloud = self.action == Action::CloudSeed || self.text(Field::Profile) == "cloud";
        (cloud && self.text(Field::RecoveryCidr).is_empty()).then(|| {
            if self.enabled(Field::HardenSsh) {
                "! No recovery SSH CIDR: SSH is reachable from any address (keys only)"
            } else {
                "! No recovery SSH CIDR and no SSH hardening: SSH with passwords is reachable from any address"
            }
        })
    }

    /// Reasons the chosen action would fail that the profile alone shows.
    /// What needs this machine's files is for whoever has them to add.
    pub fn problems(&self) -> Vec<String> {
        use Field::*;
        let action = self.action;
        let mut problems = Vec::new();
        let mut check = |ok: bool, message: &str| {
            if !ok {
                problems.push(message.to_owned());
            }
        };
        if matches!(action, Action::Save | Action::Release | Action::Manage) {
            return problems;
        }
        let agent = self.text(AgentUser);
        check(
            valid_username(agent),
            "Agent username is invalid or reserved by Ubuntu",
        );
        if action == Action::Docker {
            return problems;
        }
        let operator = self.text(OperatorUser);
        check(
            valid_username(operator),
            "Operator username is invalid or reserved by Ubuntu (operator and admin are)",
        );
        check(
            operator != agent,
            "Operator and agent need different usernames",
        );
        for (field, name) in [(OperatorHash, "Operator"), (AgentHash, "Agent")] {
            let hash = self.text(field);
            check(
                hash.is_empty() || valid_password_hash(hash),
                &format!("{name} password hash is not SHA-512 crypt or yescrypt"),
            );
        }
        for (account, name) in [(Account::Operator, "Operator"), (Account::Agent, "Agent")] {
            check(
                self.keys(account).iter().all(|key| is_public_key(key)),
                &format!("{name} has an SSH public key that is not a supported OpenSSH key"),
            );
        }
        check(
            self.enabled(AgentSudo)
                || !self.text(AgentPrivileges).is_empty()
                || !self.text(AgentHash).is_empty(),
            "Agent needs a password when passwordless sudo is off",
        );
        let geometry = self.text(Geometry);
        check(
            geometry.is_empty()
                || geometry.split_once('x').is_some_and(|(w, h)| {
                    [w, h].iter().all(|n| {
                        (3..=4).contains(&n.len()) && n.chars().all(|c| c.is_ascii_digit())
                    })
                }),
            "Desktop size must look like 1600x900",
        );
        if action.installer() || action == Action::CloudSeed {
            // Seeds are the only way into the new machine, so the operator must be able to log in.
            check(
                valid_hostname(self.text(Hostname)),
                "Hostname is missing or invalid (F4 generates)",
            );
            check(
                !self.keys(Account::Operator).is_empty()
                    || self.enabled(BerthKey) && !self.own_key.is_empty(),
                "Operator needs an SSH key: add one, or let it take this berth's",
            );
            check(
                !self.text(OperatorHash).is_empty(),
                "Operator password is required (F4 or F5)",
            );
        }
        if action.installer() {
            match self.text(DiskMode) {
                "serial" => check(
                    !self.text(DiskSerial).is_empty(),
                    "Serial disk selection needs the target disk serial",
                ),
                "model" => check(
                    !self.text(DiskModel).is_empty(),
                    "Model disk selection needs a model pattern",
                ),
                "path" => check(
                    self.text(DiskPath).starts_with("/dev/"),
                    "Path disk selection needs a /dev/ path or pattern",
                ),
                _ => {}
            }
            if self.enabled(Wifi) {
                check(
                    [WifiSsid, WifiPassword]
                        .iter()
                        .all(|field| !self.text(*field).is_empty()),
                    "Wi-Fi needs an SSID and password",
                );
            }
        }
        if action == Action::CloudSeed {
            check(
                !self.text(ReleaseStore).is_empty()
                    || self.text(ReleaseUrl).starts_with("https://"),
                "Choose where to publish the release, or enter its HTTPS address",
            );
        }
        problems
    }
}

/// What a row of the Features and Secrets pages is for.
pub const GROUP_HELP: &str = "A group of things to install. Space switches all of it on, or off when all of it is on. Plugins add their own entries to these groups.";
pub const REGISTRY_HELP: &str = "Give this host the login of this registry. Its npm repositories become the agent's npm registry with the login in ~/.npmrc, and its Docker addresses get the login in ~/.docker/config.json. Registries are set up on the Registries tab of the home screen.";
pub const SECRET_HELP: &str = "How this host gets the secret. Enter, then type a variable name such as GH_TOKEN to put it in the agent's environment, or a path such as ~/.config/app/token to write it to a file. Empty, or x, means the host does not get it. A secret given as GH_TOKEN also logs git in to GitHub through gh. Values are set on the Secrets tab of the home screen.";

#[cfg(test)]
mod tests {
    use super::*;

    fn features() -> Vec<Feature> {
        let feature = |key: &str, group: &str| Feature {
            key: key.into(),
            label: key.into(),
            group: group.into(),
            help: String::new(),
        };
        vec![
            feature("demo/agent", "coding agents"),
            feature("demo/extra", "Extras"),
        ]
    }

    #[test]
    fn features_join_the_groups_and_are_kept_in_declared_order() {
        let features = features();
        let mut config = json!({});
        set_feature(&mut config, &features, 1, true);
        set_feature(&mut config, &features, 0, true);
        assert_eq!(chosen_features(&config), ["demo/agent", "demo/extra"]);
        let profile = Profile {
            config: &config,
            step: Step::Features,
            action: Action::Ssh,
            features: &features,
            registries: &[],
            secrets: &[],
            own_key: "",
        };
        let groups = profile.feature_groups();
        assert_eq!(groups[0].title, "Coding Agents");
        assert_eq!(groups[0].items, [Item::Feature(0)]);
        assert_eq!(groups.last().unwrap().items, [Item::Feature(1)]);
        assert!(profile.item_enabled(Item::Feature(1)));
    }

    #[test]
    fn a_secret_is_a_variable_a_file_or_not_given() {
        let mut config = json!({});
        set_secret(&mut config, "token", "GH_TOKEN").unwrap();
        assert_eq!(config["secrets"][0]["env"], "GH_TOKEN");
        set_secret(&mut config, "token", "~/.config/app/token").unwrap();
        assert_eq!(config["secrets"][0]["file"], "~/.config/app/token");
        assert!(set_secret(&mut config, "token", "not a name").is_err());
        set_secret(&mut config, "token", "").unwrap();
        assert_eq!(config["secrets"], json!([]));
    }

    #[test]
    fn a_cloud_seed_needs_a_way_in() {
        let config = json!({"agent": {"user": "agent"}, "operator": {"user": "ops"}});
        let profile = Profile {
            config: &config,
            step: Step::Install,
            action: Action::CloudSeed,
            features: &[],
            registries: &[],
            secrets: &[],
            own_key: "",
        };
        let problems = profile.problems();
        assert!(problems.iter().any(|problem| problem.contains("Hostname")));
        let lacks_key = |problems: Vec<String>| {
            problems
                .iter()
                .any(|problem| problem.contains("needs an SSH key"))
        };
        assert!(lacks_key(problems));
        // This berth's own key is a way in.
        let own = Profile {
            own_key: "ssh-ed25519 AAAA berth@here",
            ..profile
        };
        assert!(!lacks_key(own.problems()));
    }

    #[test]
    fn an_account_takes_several_keys() {
        let mut config = json!({"operator_ssh_public_key": "ssh-ed25519 AAAA one"});
        let operator = Account::Operator;
        set_key(&mut config, operator, None, "ssh-ed25519 BBBB two").unwrap();
        assert!(set_key(&mut config, operator, None, "ssh-ed25519 AAAA one").is_err());
        assert!(set_key(&mut config, Account::Agent, None, "not a key").is_err());
        set_key(&mut config, operator, Some(0), "").unwrap();
        assert_eq!(
            config,
            json!({"operator_ssh_public_keys": ["ssh-ed25519 BBBB two"]})
        );
    }
}
