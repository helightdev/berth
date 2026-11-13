//! The tabs of the home screen that set the server up: its settings, its
//! registries, its secrets and its plugins. What they show is what the server
//! last said; what they change goes to the server.

use crate::admin::{Admin, Data};
use crate::browser::Browser;
use berth_tui::berth_key;
use berth_tui::fields::Kind;
use berth_tui::plugins::{self, Asked, Place};
use berth_tui::setup::{valid_name, value_line};
use berth_tui::{Code, Key, event, theme};
use ratatui::{
    style::Style,
    text::{Line, Span},
};
use serde_json::{Value, json};
use tui_input::Input;

const TYPES: &[&str] = &["nexus", "npm", "raw", "docker"];
const FORMATS: &[&str] = &["npm", "raw", "docker"];
const NAME_RULE: &str = "Use lower-case letters, digits, - and _, starting with a letter";

fn type_label(kind: &str) -> &'static str {
    match kind {
        "nexus" => "Sonatype Nexus",
        "npm" => "npm registry",
        "raw" => "File server (HTTP upload)",
        _ => "Docker registry",
    }
}

/// What a tab shows this frame.
pub struct Shown {
    pub rows: Vec<Line<'static>>,
    pub selected: usize,
    /// The title of the editor while something is typed, and whether what is
    /// typed is hidden.
    pub editor: Option<(&'static str, bool)>,
    pub help: String,
    pub keys: &'static str,
}

/// A row of the Registries tab. Registries are named, repositories numbered.
#[derive(Clone, PartialEq)]
enum RegistryRow {
    Name(String),
    Type(String),
    Address(String),
    User(String),
    Password(String),
    Repository(String, usize),
    Format(String, usize),
    DockerAddress(String, usize),
    AddRepository(String),
    AddRegistry,
}

impl RegistryRow {
    fn registry(&self) -> Option<&str> {
        use RegistryRow::*;
        match self {
            Name(name)
            | Type(name)
            | Address(name)
            | User(name)
            | Password(name)
            | Repository(name, _)
            | Format(name, _)
            | DockerAddress(name, _)
            | AddRepository(name) => Some(name),
            AddRegistry => None,
        }
    }

    fn help(&self) -> &'static str {
        use RegistryRow::*;
        match self {
            Name(_) => {
                "A registry: a server packages, files or images come from and go to. Enter renames it, x removes it with its stored login. A host gets a registry on the Secrets page of its set-up."
            }
            Type(_) => {
                "Enter switches the kind. Sonatype Nexus: one server and login with repositories under it. npm registry: any npm-compatible registry at this address. File server: an address files are uploaded to with HTTP PUT, for the release archive. Docker registry: any registry for container images."
            }
            Address(_) => {
                "Where the registry is. With https:// for Nexus, npm and file servers; a Docker registry is HOST or HOST:PORT."
            }
            User(_) => {
                "The account to log in with. Leave it empty when the registry takes a token alone, such as an npm token. x clears it."
            }
            Password(_) => {
                "The password or token. The server keeps it, readable by the user it runs as only, and it is never shown. x forgets it."
            }
            Repository(..) => {
                "A repository on the Nexus server, by its name there. Enter renames it, x removes it from this list."
            }
            Format(..) => {
                "Enter switches the format. npm: becomes the agent's npm registry. raw: a place to publish the release archive. docker: container images."
            }
            DockerAddress(..) => {
                "Leave it empty when the repository uses path based routing in Nexus: images are then at HOST/REPOSITORY/IMAGE. With a connector on a port or host name of its own, enter that as HOST:PORT."
            }
            AddRepository(_) => {
                "Add a repository of this Nexus server. Type its name; it starts as npm and Format changes that."
            }
            AddRegistry => {
                "Add a registry. Type a short name for it (lower-case letters, digits, - and _), then set its kind and address."
            }
        }
    }
}

fn bare(url: &str) -> &str {
    url.split_once("://")
        .map_or(url, |(_, rest)| rest)
        .trim_end_matches('/')
}

fn registry_rows(data: &Data) -> Vec<(RegistryRow, Line<'static>)> {
    use RegistryRow::*;
    let muted = Style::new().fg(theme::MUTED);
    let field = |prefix: &str, label: &str, value: &str| {
        let mut line = value_line(label, value, Kind::Text);
        line.spans.insert(0, Span::styled(prefix.to_owned(), muted));
        line
    };
    let mut rows = vec![];
    for (name, registry) in &data.registries {
        let text = |key: &str| registry[key].as_str().unwrap_or("").to_owned();
        let nexus = registry["type"] == "nexus";
        rows.push((
            Name(name.clone()),
            Line::styled(name.clone(), Style::new().fg(theme::ACCENT).bold()),
        ));
        rows.push((
            Type(name.clone()),
            field("  ├─ ", "Kind", type_label(&text("type"))),
        ));
        rows.push((
            Address(name.clone()),
            field("  ├─ ", "Address", &text("url")),
        ));
        rows.push((
            User(name.clone()),
            field("  ├─ ", "User name", &text("username")),
        ));
        let stored = data
            .secrets
            .iter()
            .any(|secret| *secret == format!("registry/{name}"));
        let mut password = Line::from(vec![
            Span::styled(if nexus { "  ├─ " } else { "  └─ " }, muted),
            Span::styled("Password or token  ", Style::new().fg(theme::TEXT)),
        ]);
        password.spans.push(match stored {
            true => Span::styled("● stored", Style::new().fg(theme::GOOD)),
            false => Span::styled("○ not set", muted),
        });
        rows.push((Password(name.clone()), password));
        if !nexus {
            continue;
        }
        let repositories = registry["repositories"].as_array().into_iter().flatten();
        for (index, repository) in repositories.enumerate() {
            let repo = repository["name"].as_str().unwrap_or("");
            let format = repository["format"].as_str().unwrap_or("");
            let mut line = field("  ├─ ", "Repository", repo);
            if format != "docker" {
                line.spans.push(Span::styled(
                    format!("  {}/repository/{repo}/", text("url").trim_end_matches('/')),
                    muted,
                ));
            }
            rows.push((Repository(name.clone(), index), line));
            let last = if format == "docker" {
                "  │    ├─ "
            } else {
                "  │    └─ "
            };
            rows.push((Format(name.clone(), index), field(last, "Format", format)));
            if format == "docker" {
                let line = match repository["address"]
                    .as_str()
                    .filter(|address| !address.is_empty())
                {
                    Some(address) => field("  │    └─ ", "Docker address", address),
                    None => Line::from(vec![
                        Span::styled("  │    └─ ", muted),
                        Span::styled("Docker address  ", Style::new().fg(theme::TEXT)),
                        Span::styled(
                            format!(
                                "Default · {}/{repo} (path based routing)",
                                bare(&text("url"))
                            ),
                            muted.italic(),
                        ),
                    ]),
                };
                rows.push((DockerAddress(name.clone(), index), line));
            }
        }
        rows.push((
            AddRepository(name.clone()),
            Line::from(vec![
                Span::styled("  └─ ", muted),
                Span::styled("+ Add repository", Style::new().fg(theme::ACCENT)),
            ]),
        ));
    }
    rows.push((
        AddRegistry,
        Line::styled("+ Add registry", Style::new().fg(theme::ACCENT)),
    ));
    rows
}

#[derive(Default)]
pub struct Home {
    selected: [usize; 3],
    pub editor: Option<Input>,
    /// A removal waits for a second x.
    armed: bool,
    /// A new secret's name, while its value is being typed.
    naming: Option<String>,
    /// A new or renamed registry, to select once the rows have it.
    go_to: Option<String>,
    plugins: plugins::Tab,
    /// The server, and the link that opens this page signed in, when there
    /// is one to give: a row of the Settings tab.
    pub connect: Option<(String, String)>,
    /// The address of the server, as the header shows it.
    pub server: String,
    /// A QR code to show, for the page to take.
    pub qr: Option<berth_tui::qr::Overlay>,
}

/// The rows of the Settings tab after the server's own settings.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsRow {
    BerthKey,
    Connect,
}

/// The tabs after Hosts, in their order.
pub const SETTINGS: usize = 0;
pub const REGISTRIES: usize = 1;
pub const SECRETS: usize = 2;
pub const PLUGINS: usize = 3;

fn cycle(all: &[&str], current: &str) -> String {
    let index = all
        .iter()
        .position(|known| *known == current)
        .map_or(0, |index| index + 1);
    all[index % all.len()].to_owned()
}

impl Home {
    pub fn select(&mut self, tab: usize, row: usize) {
        match tab {
            PLUGINS => self.plugins.selected = row,
            tab => self.selected[tab] = row,
        }
    }

    pub fn selected(&self, tab: usize) -> usize {
        match tab {
            PLUGINS => self.plugins.selected,
            tab => self.selected[tab],
        }
    }

    /// Leave what is being typed, on changing tabs.
    pub fn leave(&mut self) {
        self.editor = None;
        self.naming = None;
        self.armed = false;
        self.plugins.leave();
    }

    pub fn shown(&mut self, tab: usize, data: &Data) -> Shown {
        match tab {
            SETTINGS => self.settings_shown(data),
            REGISTRIES => self.registries_shown(data),
            PLUGINS => self.plugins_shown(data),
            _ => self.secrets_shown(data),
        }
    }

    /// A key on one of the tabs. Returns what to say.
    pub fn key(&mut self, tab: usize, key: Key, admin: &Admin) -> Option<String> {
        match tab {
            SETTINGS => self.settings_key(key, admin),
            REGISTRIES => self.registries_key(key, admin),
            PLUGINS => self.plugins_key(key, admin),
            _ => self.secrets_key(key, admin),
        }
    }

    // ── Settings ────────────────────────────────────────────────────────────

    /// The rows of the Settings tab after the server's settings, once it has
    /// said what they are.
    fn settings_extras(&self, data: &Data) -> Vec<SettingsRow> {
        let mut extras = vec![];
        if data.settings.is_empty() {
            return extras;
        }
        if !data.own_key.is_empty() {
            extras.push(SettingsRow::BerthKey);
        }
        if self.connect.is_some() {
            extras.push(SettingsRow::Connect);
        }
        extras
    }

    fn settings_shown(&mut self, data: &Data) -> Shown {
        let muted = Style::new().fg(theme::MUTED);
        let extras = self.settings_extras(data);
        let count = (data.settings.len() + extras.len()).max(1);
        self.selected[SETTINGS] = self.selected[SETTINGS].min(count - 1);
        let mut rows: Vec<Line> = data
            .settings
            .iter()
            .map(|setting| {
                let key = setting["key"].as_str().unwrap_or("");
                let value = setting["value"].as_str().unwrap_or("");
                let mut line = match value.is_empty() {
                    true => Line::from(vec![
                        Span::styled(format!("{key}  "), Style::new().fg(theme::TEXT)),
                        Span::styled("Not set", muted.italic()),
                    ]),
                    false => value_line(key, value, Kind::Text),
                };
                if setting["open"] != true {
                    line.spans
                        .push(Span::styled("  · set on the server", muted));
                }
                line
            })
            .collect();
        for extra in &extras {
            rows.push(match extra {
                SettingsRow::BerthKey => berth_key::row(&data.own_key),
                SettingsRow::Connect => Line::from(vec![
                    Span::styled("Open on another device  ", Style::new().fg(theme::TEXT)),
                    Span::styled("QR code and link, with the token", muted),
                ]),
            });
        }
        let extra = self.selected[SETTINGS]
            .checked_sub(data.settings.len())
            .and_then(|index| extras.get(index).copied());
        let help = match (
            data.settings.get(self.selected[SETTINGS]),
            extra,
            &self.connect,
        ) {
            (Some(setting), ..) => {
                let meaning = setting["meaning"].as_str().unwrap_or("");
                match setting["open"] == true {
                    true => format!("The {meaning}.\n\nLeave it empty to use the default."),
                    false => format!(
                        "The {meaning}.\n\nIt says who gets in or where the server's files are, so it is set on the server itself: `berth config {} VALUE`. A token is never shown.",
                        setting["key"].as_str().unwrap_or("")
                    ),
                }
            }
            (None, Some(SettingsRow::BerthKey), _) => {
                berth_key::help(&data.own_key, Some(&self.server))
            }
            (None, Some(SettingsRow::Connect), Some((server, _))) => {
                berth_tui::qr::connect_help(server, false)
            }
            _ => "Asking the server for its settings…".into(),
        };
        Shown {
            rows,
            selected: self.selected[SETTINGS],
            editor: self.editor.as_ref().map(|_| (" Edit ", false)),
            help,
            keys: match (&self.editor, extra) {
                (Some(_), _) => "Enter accept · Esc cancel",
                (None, Some(SettingsRow::BerthKey)) => {
                    "↑↓ select · Enter copy the key · Tab registries · Esc hosts"
                }
                (None, Some(SettingsRow::Connect)) => {
                    "↑↓ select · Enter show the QR code · Tab registries · Esc hosts"
                }
                (None, None) => "↑↓ select · Enter change · Tab registries · Esc hosts",
            },
        }
    }

    fn settings_key(&mut self, key: Key, admin: &Admin) -> Option<String> {
        let data = admin.data.borrow();
        let settings = data.settings.len();
        let extras = self.settings_extras(&data);
        let count = settings + extras.len();
        let selected = self.selected[SETTINGS].min(count.saturating_sub(1));
        let setting = data.settings.get(selected).cloned().unwrap_or_default();
        let own_key = data.own_key.clone();
        drop(data);
        let extra = selected
            .checked_sub(settings)
            .and_then(|index| extras.get(index).copied());
        let name = setting["key"].as_str().unwrap_or("").to_owned();
        if let Some(input) = &mut self.editor {
            match key.code {
                Code::Esc => self.editor = None,
                Code::Enter => {
                    let typed = input.value().trim().to_owned();
                    self.editor = None;
                    admin.set_setting(&name, &typed, format!("Saved {name}"));
                }
                _ => event::edit(input, &key),
            }
            return None;
        }
        if extra == Some(SettingsRow::BerthKey)
            && (key.code == Code::Enter || berth_key::is_copy(&key))
        {
            return Some(berth_key::copy(&mut Browser, &own_key));
        }
        // The last row shows the link that opens this page.
        if extra == Some(SettingsRow::Connect)
            && (key.code == Code::Enter || berth_tui::qr::is_key(&key))
            && let Some((_, link)) = &self.connect
        {
            self.qr = Some(berth_tui::qr::Overlay::connect(link.clone()));
            return None;
        }
        match key.code {
            Code::Up => self.selected[SETTINGS] = selected.saturating_sub(1),
            Code::Down => self.selected[SETTINGS] = (selected + 1).min(count.saturating_sub(1)),
            Code::Enter if setting["open"] == true => {
                self.editor = Some(Input::new(
                    setting["value"].as_str().unwrap_or("").to_owned(),
                ));
            }
            Code::Enter if !name.is_empty() => {
                return Some(format!(
                    "{name} is set on the server itself, with berth config"
                ));
            }
            _ => {}
        }
        None
    }

    // ── Secrets ─────────────────────────────────────────────────────────────

    fn secrets_shown(&mut self, data: &Data) -> Shown {
        let names = data.secret_names();
        self.selected[SECRETS] = self.selected[SECRETS].min(names.len());
        let selected = self.selected[SECRETS];
        let mut rows: Vec<Line> = names
            .iter()
            .map(|name| {
                Line::from(vec![
                    Span::styled(format!("{name}  "), Style::new().fg(theme::TEXT)),
                    Span::styled("● stored", Style::new().fg(theme::GOOD)),
                ])
            })
            .collect();
        rows.push(Line::styled("+ Add secret", Style::new().fg(theme::ACCENT)));
        let adding = selected == names.len();
        let title = match (&self.naming, adding) {
            (Some(_), _) => " Value ",
            (None, true) => " Name of the new secret ",
            (None, false) => " New value ",
        };
        let own = names
            .get(selected)
            .and_then(|name| berth_tui::setup::own_secret_help(name));
        let help = match (adding, own) {
            (true, _) => "Add a secret: a token or password for the agent on a host. Type a short name for it (lower-case letters, digits, - and _), then its value. The name tailnet_api_key is for the API key of the tailnet's control server, which the server keeps for itself.\n\nThe server keeps the values, readable by the user it runs as only. They are never shown, and leave the server only to a host that is given them.".into(),
            (false, Some(own)) => own.to_owned(),
            (false, None) => "Enter sets a new value, x removes the secret from the server.\n\nA host gets a secret on the Secrets page of its set-up, as a variable of the agent's environment or as a file, and \"Install secrets and registry logins\" on its overview sends it.".into(),
        };
        Shown {
            rows,
            selected,
            editor: self
                .editor
                .as_ref()
                .map(|_| (title, self.naming.is_some() || !adding)),
            help,
            keys: match self.editor {
                Some(_) => "Enter accept · Esc cancel",
                None => "↑↓ select · Enter set · x remove · Tab plugins · Esc hosts",
            },
        }
    }

    fn secrets_key(&mut self, key: Key, admin: &Admin) -> Option<String> {
        let names = admin.data.borrow().secret_names();
        let at = self.selected[SECRETS].min(names.len());
        let selected = names.get(at).cloned();
        if let Some(input) = &mut self.editor {
            match key.code {
                Code::Esc => {
                    self.editor = None;
                    self.naming = None;
                }
                Code::Enter => {
                    let typed = input.value().to_owned();
                    // A name was typed for a new secret: its value comes next.
                    if selected.is_none() && self.naming.is_none() {
                        let name = typed.trim();
                        if !valid_name(name) {
                            return Some(NAME_RULE.into());
                        }
                        if names.iter().any(|known| known == name) {
                            return Some(format!("There is a secret {name} already"));
                        }
                        self.naming = Some(name.to_owned());
                        self.editor = Some(Input::default());
                        return Some(format!("Now the value of {name}"));
                    }
                    let name = self.naming.take().or(selected).unwrap_or_default();
                    self.editor = None;
                    if typed.is_empty() {
                        return Some("Nothing entered, kept as it was".into());
                    }
                    admin.set_secret(&name, &typed, format!("Stored {name}"));
                }
                _ => event::edit(input, &key),
            }
            return None;
        }
        let armed = std::mem::take(&mut self.armed);
        match key.code {
            Code::Up => self.selected[SECRETS] = at.saturating_sub(1),
            Code::Down => self.selected[SECRETS] = (at + 1).min(names.len()),
            Code::Enter => self.editor = Some(Input::default()),
            Code::Char('x') | Code::Delete => match selected {
                Some(name) if armed => admin.remove_secret(&name, format!("Removed {name}")),
                Some(name) => {
                    self.armed = true;
                    return Some(format!("x again removes {name} from the server"));
                }
                None => {}
            },
            _ => {}
        }
        None
    }

    // ── Registries ──────────────────────────────────────────────────────────

    fn registries_shown(&mut self, data: &Data) -> Shown {
        let listed = registry_rows(data);
        if let Some(name) = &self.go_to
            && let Some(position) = listed
                .iter()
                .position(|(row, _)| *row == RegistryRow::Name(name.clone()))
        {
            self.selected[REGISTRIES] = position;
            self.go_to = None;
        }
        self.selected[REGISTRIES] = self.selected[REGISTRIES].min(listed.len() - 1);
        let selected = self.selected[REGISTRIES];
        let row = &listed[selected].0;
        let title = match row {
            RegistryRow::AddRegistry => " Name of the new registry ",
            RegistryRow::AddRepository(_) => " Name of the repository on the server ",
            RegistryRow::Password(_) => " Password or token ",
            _ => " Edit ",
        };
        let help = match data.report.is_empty() {
            true => format!(
                "{}\n\nc checks, from the server, that this registry answers and accepts its login.",
                row.help()
            ),
            false => data.report.join("\n\n"),
        };
        Shown {
            selected,
            editor: self
                .editor
                .as_ref()
                .map(|_| (title, matches!(row, RegistryRow::Password(_)))),
            help,
            keys: match self.editor {
                Some(_) => "Enter accept · Esc cancel",
                None => "↑↓ select · Enter change · x remove · c check · Tab secrets · Esc hosts",
            },
            rows: listed.into_iter().map(|(_, line)| line).collect(),
        }
    }

    /// Take what was typed for a row. The message is what to say.
    fn registry_accept(
        &mut self,
        admin: &Admin,
        row: &RegistryRow,
        value: &str,
    ) -> Result<String, String> {
        use RegistryRow::*;
        let registries = admin.data.borrow().registries.clone();
        let of = |name: &str| registries.get(name).cloned().unwrap_or_else(|| json!({}));
        match row {
            AddRegistry | Name(_) => {
                if !valid_name(value) {
                    return Err(NAME_RULE.into());
                }
                if registries.contains_key(value) {
                    return Err(format!("There is a registry {value} already"));
                }
                self.go_to = Some(value.to_owned());
                match row {
                    // The server never gives a login back, so it cannot move.
                    Name(old) => {
                        admin.save_registry(value, &of(old), "");
                        admin.remove_registry(
                            old,
                            format!(
                                "Registry {value} · its password or token needs entering again"
                            ),
                        );
                    }
                    _ => {
                        // A registry needs an address to be one; until it has
                        // one it is only on this page.
                        admin
                            .data
                            .borrow_mut()
                            .registries
                            .insert(value.to_owned(), json!({"type": "nexus", "url": ""}));
                    }
                }
                Ok(format!("Registry {value} · now its address"))
            }
            Address(name) => {
                let mut registry = of(name);
                if registry["type"] != "docker"
                    && !value.starts_with("https://")
                    && !value.starts_with("http://")
                {
                    return Err("Start the address with https://".into());
                }
                registry["url"] = json!(value.trim_end_matches('/'));
                admin.save_registry(name, &registry, "Address saved");
                Ok(String::new())
            }
            User(name) => {
                let mut registry = of(name);
                if let Some(fields) = registry.as_object_mut() {
                    match value {
                        "" => fields.remove("username"),
                        user => fields.insert("username".into(), json!(user)),
                    };
                }
                Ok(self
                    .put(admin, name, registry, "User name saved")
                    .unwrap_or_default())
            }
            Password(name) => {
                if value.is_empty() {
                    return Ok("Nothing entered, kept as it was".into());
                }
                admin.set_secret(&format!("registry/{name}"), value, "Stored".into());
                Ok(String::new())
            }
            AddRepository(name) | Repository(name, _) => {
                if value.is_empty() || value.contains(['/', ' ']) {
                    return Err("The repository's name on the server, without / or spaces".into());
                }
                let mut registry = of(name);
                let mut repositories = registry["repositories"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                if repositories.iter().any(|known| known["name"] == value) {
                    return Err(format!("{value} is listed already"));
                }
                match row {
                    Repository(_, index) => repositories[*index]["name"] = json!(value),
                    _ => repositories.push(json!({"format": "npm", "name": value})),
                }
                registry["repositories"] = json!(repositories);
                Ok(self
                    .put(admin, name, registry, &format!("Repository {value}"))
                    .unwrap_or_default())
            }
            DockerAddress(name, index) => {
                let address = value.split_once("://").map_or(value, |(_, rest)| rest);
                let mut registry = of(name);
                if let Some(repository) = registry["repositories"][*index].as_object_mut() {
                    match address.trim_end_matches('/') {
                        "" => repository.remove("address"),
                        address => repository.insert("address".into(), json!(address)),
                    };
                }
                Ok(self
                    .put(admin, name, registry, "Docker address saved")
                    .unwrap_or_default())
            }
            Type(_) | Format(..) => Ok(String::new()),
        }
    }

    /// Send a changed registry to the server, which takes one with an address.
    /// Returns what to say when it is not sent yet.
    fn put(&mut self, admin: &Admin, name: &str, registry: Value, said: &str) -> Option<String> {
        if registry["url"].as_str().unwrap_or("").is_empty() {
            admin
                .data
                .borrow_mut()
                .registries
                .insert(name.to_owned(), registry);
            return Some("The server keeps it once it has an address".into());
        }
        admin.save_registry(name, &registry, said);
        None
    }

    fn registries_key(&mut self, key: Key, admin: &Admin) -> Option<String> {
        use RegistryRow::*;
        let rows = registry_rows(&admin.data.borrow());
        let at = self.selected[REGISTRIES].min(rows.len() - 1);
        let row = rows[at].0.clone();
        if let Some(input) = &mut self.editor {
            match key.code {
                Code::Esc => self.editor = None,
                Code::Enter => {
                    let typed = match row {
                        Password(_) => input.value().to_owned(),
                        _ => input.value().trim().to_owned(),
                    };
                    return match self.registry_accept(admin, &row, &typed) {
                        Ok(said) => {
                            self.editor = None;
                            (!said.is_empty()).then_some(said)
                        }
                        Err(why) => Some(why),
                    };
                }
                _ => event::edit(input, &key),
            }
            return None;
        }
        let armed = std::mem::take(&mut self.armed);
        if key.code == Code::Char('c') {
            return match row.registry() {
                Some(name) => {
                    admin.check_registry(name);
                    None
                }
                None => Some("Select a registry to check".into()),
            };
        }
        // Moving on to something else drops the report of the last check.
        if matches!(key.code, Code::Up | Code::Down) {
            admin.data.borrow_mut().report.clear();
        }
        let registries = admin.data.borrow().registries.clone();
        let of = |name: &str| registries.get(name).cloned().unwrap_or_else(|| json!({}));
        match key.code {
            Code::Up => self.selected[REGISTRIES] = at.saturating_sub(1),
            Code::Down => self.selected[REGISTRIES] = (at + 1).min(rows.len() - 1),
            Code::Enter | Code::Char(' ') => match &row {
                Type(name) => {
                    let mut registry = of(name);
                    registry["type"] = json!(cycle(TYPES, registry["type"].as_str().unwrap_or("")));
                    return self.put(admin, name, registry, "");
                }
                Format(name, index) => {
                    let mut registry = of(name);
                    let repository = &mut registry["repositories"][*index];
                    repository["format"] =
                        json!(cycle(FORMATS, repository["format"].as_str().unwrap_or("")));
                    return self.put(admin, name, registry, "");
                }
                // Editing starts from the current value, except for what is secret or new.
                other => {
                    let current = match other {
                        Name(name) => name.clone(),
                        Address(name) => of(name)["url"].as_str().unwrap_or("").to_owned(),
                        User(name) => of(name)["username"].as_str().unwrap_or("").to_owned(),
                        Repository(name, index) => of(name)["repositories"][*index]["name"]
                            .as_str()
                            .unwrap_or("")
                            .to_owned(),
                        DockerAddress(name, index) => of(name)["repositories"][*index]["address"]
                            .as_str()
                            .unwrap_or("")
                            .to_owned(),
                        _ => String::new(),
                    };
                    self.editor = Some(Input::new(current));
                }
            },
            Code::Char('x') | Code::Delete => match &row {
                Name(name) if !armed => {
                    self.armed = true;
                    return Some(format!("x again removes {name} and its stored login"));
                }
                Name(name) => {
                    // One that never got an address is only on this page.
                    match of(name)["url"].as_str().unwrap_or("").is_empty() {
                        true => {
                            admin.data.borrow_mut().registries.remove(name);
                        }
                        false => admin.remove_registry(name, format!("Removed {name}")),
                    }
                }
                User(name) => {
                    let mut registry = of(name);
                    if let Some(fields) = registry.as_object_mut() {
                        fields.remove("username");
                    }
                    return self.put(admin, name, registry, "User name cleared");
                }
                Password(name) => {
                    admin.remove_secret(&format!("registry/{name}"), "Forgotten".into())
                }
                Repository(name, index) => {
                    let mut registry = of(name);
                    if let Some(repositories) = registry["repositories"].as_array_mut() {
                        repositories.remove(*index);
                    }
                    return self.put(admin, name, registry, "Repository removed");
                }
                _ => {}
            },
            _ => {}
        }
        None
    }

    // ── Plugins ─────────────────────────────────────────────────────────────

    fn plugins_shown(&mut self, data: &Data) -> Shown {
        self.plugins.updates = data.updates.clone();
        self.plugins.behind = data.behind.clone();
        let listed = plugins::rows(&data.plugins, &data.updates);
        let selected = self.plugins.selected(&data.plugins);
        let plugin = listed[selected].0.map(|index| &data.plugins[index]);
        let help = plugins::help(plugin, &data.report, &Place::Page.adding(&data.plugin_dir));
        Shown {
            rows: listed.into_iter().map(|(_, line)| line).collect(),
            selected,
            editor: self
                .editor
                .as_ref()
                .map(|_| (Place::Page.editor_title(), false)),
            help,
            keys: match self.editor {
                Some(_) => "Enter install · Esc cancel",
                None => {
                    "↑↓ select · Enter add · u send a .tgz · c check · U update · A all · x remove · Esc hosts"
                }
            },
        }
    }

    fn plugins_key(&mut self, key: Key, admin: &Admin) -> Option<String> {
        let (listed, mut report) = {
            let data = admin.data.borrow();
            self.plugins.updates = data.updates.clone();
            self.plugins.behind = data.behind.clone();
            (data.plugins.clone(), data.report.clone())
        };
        let asked = self
            .plugins
            .key(key, &mut self.editor, &listed, &mut report, &Place::Page);
        admin.data.borrow_mut().report = report;
        match asked {
            Asked::Nothing(said) => return said,
            Asked::Install(spec) => admin.add_plugin(&spec),
            Asked::Remove(id) => admin.remove_plugin(&id),
            Asked::Check => admin.check_plugins(),
            Asked::Update(id) => admin.update_plugins(id.as_deref()),
            Asked::Rollout(options) => admin.roll_out(&options),
            Asked::Other if key.code == Code::Char('u') => {
                let admin = admin.clone();
                crate::browser::choose_file(".tgz,.tar.gz", move |name, package| {
                    admin.upload_plugin(&name, &package)
                });
            }
            Asked::Other => {}
        }
        None
    }
}
