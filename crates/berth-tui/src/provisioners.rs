//! The Provisioners tab of the home screen: the provisioners there are, each
//! with whether it can be used here, and the page of one, built with
//! [`crate::page`]: how it is now, what it says of itself, what it holds by
//! kind, and its actions. A resource berth made with it has a page of its own
//! with its state, its actions and its log: those of the server the screens
//! use. What the tab asks for goes to the server, which tells the tab what
//! came back.
//!
//! What a provisioner holds, such as storage pools, is listed as its
//! collections: each is a table of its items, and each item has a page with
//! the actions on it, whose arguments its key fills.

use crate::event::{Code, Event, Key};
use crate::form::{self, Did, Form};
use crate::page::{self, Accepted, Arguments, Begin, Group, Item, Tone};
use crate::screen::{self, Platform};
use crate::theme;
use berth_api::{
    Action, Collection, CollectionAction, CollectionItem, ProvisionerInfo, ProvisionerState,
    Resource,
};
use ratatui::{
    style::Style,
    text::{Line, Span},
};
use serde_json::Value;
use std::collections::BTreeMap;

/// What the tab shows this frame.
pub struct Shown {
    pub rows: Vec<Line<'static>>,
    pub selected: usize,
    pub help: String,
    pub keys: &'static str,
}

/// What a key on the tab asks of the screen that has it.
#[derive(Debug, PartialEq)]
pub enum Asked {
    /// Nothing more; maybe something to say.
    Nothing(Option<String>),
    /// Read the list again.
    List,
    /// Ask for the types of provisioner, to register one: [`Tab::adding`].
    Add,
    /// Register a provisioner of the type `kind` under `name`, then read the
    /// list again.
    Save { name: String, kind: String },
    /// Ask a provisioner how it is and what it can do, for its page.
    Open(String),
    /// Run an action of a provisioner with these values.
    Run {
        provisioner: String,
        action: Box<Action>,
        values: BTreeMap<String, String>,
    },
    /// Ask a resource how it is and what it can do, for its page.
    OpenResource(String),
    /// Run an action of a resource with these values.
    RunResource {
        resource: String,
        action: Box<Action>,
        values: BTreeMap<String, String>,
    },
    /// Show a resource's log, by its id.
    Logs(String),
    /// Go back to the hosts' list, which what was being made was chosen on.
    Hosts,
    /// A key the tab does not take, such as one that changes tabs.
    Other,
}

/// A row of a provisioner's page that can be chosen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Entry {
    Action(usize),
    /// A collection, by its place among the provisioner's.
    Collection(usize),
    /// An item of the table shown, by its place in it.
    Item(usize),
    /// What the provisioner holds, by its place on the page.
    Resource(usize),
    /// A resource's log.
    Logs,
}

/// What a resource's page knows of it.
pub struct ResourcePart {
    /// The provisioner whose page it was opened from.
    pub provisioner: String,
    /// None while it is asked.
    pub seen: Option<Result<Resource, String>>,
}

/// What is known of an action before it is asked for: the values a key
/// fills, and the provisioner it runs on when it is offered on a resource's
/// page.
#[derive(Clone, Debug, Default, PartialEq)]
struct Filled {
    provisioner: Option<String>,
    values: BTreeMap<String, String>,
    /// What its job is called, with the item it acts on.
    title: String,
}

/// A collection's table, or the page of one of its items, over the
/// provisioner's page.
#[derive(Clone, Debug, PartialEq)]
struct View {
    collection: String,
    /// On an item's page its key; on a table the key of the item it lists
    /// the items of, or nothing for all.
    key: BTreeMap<String, String>,
    item: bool,
    /// The cursor of what it covers, for going back.
    back: usize,
}

/// The page of one provisioner, or of one resource it made, as it was last
/// heard of.
pub struct Page {
    /// The provisioner's name, or the resource's id.
    pub id: String,
    /// Set on a resource's page.
    pub resource: Option<ResourcePart>,
    /// None while it is asked.
    pub state: Option<Result<ProvisionerState, String>>,
    /// The actions of what is shown: of the subject, or of a collection's
    /// table or item with what their keys fill in `filled`.
    pub actions: Vec<Action>,
    filled: Vec<Filled>,
    /// Every action of the subject, as it was given.
    base: Vec<Action>,
    /// On a resource's page, the provisioner's actions on it as an item.
    extra: Vec<(Action, Filled)>,
    /// The tables and items shown over the provisioner's page, the last on top.
    views: Vec<View>,
    /// How many of a table's actions make a new item; they come first.
    making: usize,
    /// It is asked again, and what it said last is shown till it answers.
    asking: bool,
    /// The page it was opened from, which Esc goes back to.
    back: Option<Box<Page>>,
    cursor: usize,
    /// The question of the action chosen, to be answered first.
    question: Option<(String, usize)>,
    arguments: Option<Arguments>,
    /// The form of an action that asks for much, while it is filled in.
    form: Option<Form>,
    /// The form last run, which its action opens again as it was, so a run
    /// that failed is not typed anew.
    kept: Option<Form>,
}

#[derive(Default)]
pub struct Tab {
    pub selected: usize,
    pub page: Option<Page>,
    /// The dialog that registers a provisioner, with its one action.
    adding: Option<(Vec<Action>, Arguments)>,
}

/// What a host's list offers to make besides hosts, as (kind, label).
pub const MAKES: [(&str, &str); 2] = [
    ("instance", "resource from an image"),
    ("deployment", "compose project"),
];

/// The action of the dialog that registers a provisioner: its name, then
/// its type, of `types`.
fn add_action(types: &[ProvisionerInfo]) -> Action {
    let offered: Vec<String> = types.iter().map(|kind| kind.id.clone()).collect();
    let described: Vec<String> = types
        .iter()
        .map(|kind| format!("{} is {}", kind.id, kind.label))
        .collect();
    Action {
        name: "add".into(),
        label: "Add provisioner".into(),
        arguments: vec![
            berth_api::Argument {
                name: "name".into(),
                label: "Name".into(),
                required: true,
                help: "The provisioner's name in berth, such as ovh-incus: lowercase letters, digits, - and _, starting with a letter.".into(),
                ..Default::default()
            },
            berth_api::Argument {
                name: "type".into(),
                label: "Type".into(),
                required: true,
                default: offered.first().cloned().unwrap_or_default(),
                help: format!(
                    "What it is, of the types the plugins bring: {}.",
                    described.join(", ")
                ),
                values: offered,
                ..Default::default()
            },
        ],
        role: berth_api::ops::Role::Admin,
        ..Action::default()
    }
}

/// How a provisioner is, in a few words, with its colour.
fn usable(provisioner: &ProvisionerInfo) -> (String, ratatui::style::Color) {
    match provisioner.problem.is_empty() {
        true => ("● usable".into(), theme::GOOD),
        false => (
            format!("○ {}", screen::ellipsize(&provisioner.problem, 60)),
            theme::WARN,
        ),
    }
}

/// What an action that answered at once said, as lines.
pub fn answer_lines(answer: &Value) -> Vec<String> {
    let mut lines = vec![];
    if let Some(message) = answer["message"].as_str().filter(|text| !text.is_empty()) {
        lines.push(message.to_owned());
    }
    let rows: Vec<(String, String)> = answer["details"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|row| {
            let text = |key: &str| row[key].as_str().unwrap_or("").to_owned();
            (text("label"), text("value"))
        })
        .collect();
    let width = rows
        .iter()
        .map(|(label, _)| label.chars().count())
        .max()
        .unwrap_or(0);
    lines.extend(
        rows.into_iter()
            .map(|(label, value)| format!("  {label:<width$}  {value}")),
    );
    lines
}

/// The rows of the list: one for each provisioner, then the row that
/// registers one.
pub fn rows(provisioners: &[ProvisionerInfo]) -> Vec<Line<'static>> {
    let muted = Style::new().fg(theme::MUTED);
    let mut rows: Vec<Line<'static>> = provisioners
        .iter()
        .map(|provisioner| {
            let (said, colour) = usable(provisioner);
            Line::from(vec![
                Span::styled(
                    format!("{:<20}", provisioner.named()),
                    Style::new().fg(theme::TEXT),
                ),
                Span::styled(format!("{:<28}", provisioner.label), muted),
                Span::styled(said, Style::new().fg(colour)),
            ])
        })
        .collect();
    rows.push(Line::styled(
        "+ Add provisioner",
        Style::new().fg(theme::ACCENT),
    ));
    rows
}

/// What the list says of the selected provisioner. `place` is where they
/// are, such as this machine.
/// A kind's label, which names one, as more than one: `Compose project`
/// makes `Compose projects`.
pub fn plural(label: &str) -> String {
    let label = label.trim();
    let consonant_y = label.ends_with('y')
        && !label
            .chars()
            .rev()
            .nth(1)
            .is_some_and(|before| "aeiou".contains(before));
    match label {
        "" => String::new(),
        _ if label.ends_with('s')
            || label.ends_with('x')
            || label.ends_with("sh")
            || label.ends_with("ch") =>
        {
            match label.ends_with('s') {
                true => label.to_owned(),
                false => format!("{label}es"),
            }
        }
        _ if consonant_y => format!("{}ies", &label[..label.len() - 1]),
        _ => format!("{label}s"),
    }
}

pub fn help(provisioner: Option<&ProvisionerInfo>, place: &str) -> String {
    let Some(provisioner) = provisioner else {
        return format!(
            "A provisioner creates machines for hosts, and other resources such as Compose projects. Plugins bring the types, such as an Incus virtual machine; berth runs each as a provisioner registered under a name of its own, with its own settings, such as the server it uses. Enter registers one on {place}, asking for its name and its type. From the command line: `berth provisioner add NAME TYPE`"
        );
    };
    let mut text = format!(
        "{}, a {}.\n\n{}\n\n",
        provisioner.id, provisioner.kind, provisioner.description
    );
    if !provisioner.problem.is_empty() {
        text += &format!("It cannot be used here: {}\n\n", provisioner.problem);
    }
    if !provisioner.node.is_empty() {
        text += &format!(
            "It lives on the node {}, whose feature offered it when the node was applied.\n\n",
            provisioner.node
        );
    }
    let kinds: Vec<String> = provisioner
        .kinds
        .iter()
        .map(|kind| match kind.id.as_str() {
            "machine" => "machines for hosts".to_owned(),
            _ => plural(&kind.label),
        })
        .collect();
    if !kinds.is_empty() {
        text += &format!("It makes {}.\n\n", kinds.join(" and "));
    }
    let held: Vec<String> = provisioner
        .collections
        .iter()
        .filter(|collection| collection.kinds.is_empty())
        .map(|collection| collection.plural.to_lowercase())
        .collect();
    if !held.is_empty() {
        text += &format!(
            "Its page lists its {}, each with its actions.\n\n",
            held.join(", ")
        );
    }
    let names: Vec<&str> = provisioner
        .actions
        .iter()
        .filter(|action| action.collection.is_empty())
        .map(Action::title)
        .collect();
    if !names.is_empty() {
        text += &format!("Its actions: {}.\n\n", names.join(", "));
    }
    if !provisioner.own.is_empty() {
        let set: Vec<String> = provisioner
            .own
            .iter()
            .map(|name| match provisioner.settings.get(name) {
                Some(value) => format!("{name} is {value}"),
                None => format!("{name} is not set"),
            })
            .collect();
        text += &format!("Its settings: {}.\n\n", set.join(", "));
    }
    text + &format!(
        "Enter opens its page, which asks it how it is.\n\nThe provisioners of {place}. From the command line: `berth provisioner show {}`",
        provisioner.named()
    )
}

impl Page {
    fn new(id: &str) -> Page {
        Page {
            id: id.to_owned(),
            resource: None,
            back: None,
            state: None,
            actions: vec![],
            filled: vec![],
            base: vec![],
            extra: vec![],
            views: vec![],
            making: 0,
            asking: false,
            cursor: 0,
            question: None,
            arguments: None,
            form: None,
            kept: None,
        }
    }

    /// The lines that say how a resource is.
    fn resource_lines(&self, part: &ResourcePart) -> Vec<(String, Tone)> {
        let seen = match &part.seen {
            None => return vec![(format!("Asking about {}…", self.id), Tone::Muted)],
            Some(Err(error)) => return vec![(format!("✗ {error}"), Tone::Bad)],
            Some(Ok(seen)) => seen,
        };
        let tone = match seen.state.as_str() {
            "running" => Tone::Plain,
            "missing" => Tone::Bad,
            "stopped" => Tone::Muted,
            _ => Tone::Warn,
        };
        let mark = match tone {
            Tone::Plain => "●",
            _ => "○",
        };
        let mut lines = vec![
            (
                format!("{} · {} of {}", seen.name, seen.kind, seen.provisioner),
                Tone::Plain,
            ),
            (format!("{mark} {}", seen.state), tone),
        ];
        if !seen.problem.is_empty() {
            lines.push((format!("! {}", seen.problem), Tone::Warn));
        }
        let mut row = |label: &str, value: &str| {
            if !value.is_empty() {
                lines.push((format!("  {label:<12}{value}"), Tone::Muted));
            }
        };
        row("Resource", &seen.id);
        row("Made as", &seen.external_id);
        row("Host", seen.host.as_deref().unwrap_or(""));
        row("Address", &seen.address);
        row("Created", &seen.created);
        row("Asked", &seen.checked);
        if !seen.spec_sha256.is_empty() {
            row(
                "Spec",
                &format!(
                    "sha256 {}",
                    &seen.spec_sha256[..12.min(seen.spec_sha256.len())]
                ),
            );
        }
        let secrets: Vec<String> = seen
            .secrets
            .iter()
            .map(|secret| format!("{} as {}", secret.name, secret.variable()))
            .collect();
        row("Secrets", &secrets.join(", "));
        let arguments: Vec<String> = seen
            .arguments
            .iter()
            .filter(|(_, value)| !value.is_empty())
            .map(|(key, value)| format!("{key}={value}"))
            .collect();
        row("Arguments", &arguments.join(" "));
        if !seen.endpoints.is_empty() {
            lines.push((String::new(), Tone::Muted));
            lines.push((
                format!("Endpoints · berth port forward {} ENDPOINT", seen.id),
                Tone::Plain,
            ));
            let width = seen
                .endpoints
                .iter()
                .map(|endpoint| endpoint.name.chars().count())
                .max()
                .unwrap_or(0);
            for endpoint in &seen.endpoints {
                let hint = match endpoint.hint.is_empty() {
                    true => String::new(),
                    false => format!("  {}", endpoint.hint),
                };
                lines.push((
                    format!(
                        "  {:<width$}  {}:{}/{}{hint}",
                        endpoint.name, endpoint.address, endpoint.port, endpoint.protocol
                    ),
                    Tone::Muted,
                ));
            }
        }
        lines
    }

    /// The lines that say how it is.
    fn state_lines(&self) -> Vec<(String, Tone)> {
        if let Some(part) = &self.resource {
            return self.resource_lines(part);
        }
        let state = match &self.state {
            None => return vec![(format!("Asking {}…", self.id), Tone::Muted)],
            Some(Err(error)) => return vec![(format!("✗ {error}"), Tone::Bad)],
            Some(Ok(state)) => state,
        };
        let info = &state.provisioner;
        let tone = match state.state.as_str() {
            "ready" => Tone::Plain,
            "unreachable" => Tone::Bad,
            _ => Tone::Warn,
        };
        let mark = match tone {
            Tone::Plain => "●",
            _ => "○",
        };
        let said = match state.message.is_empty() {
            true => format!("{mark} {}", state.state),
            false => format!("{mark} {} · {}", state.state, state.message),
        };
        let mut lines = vec![
            (format!("{} · {}", info.label, info.named()), Tone::Plain),
            (said, tone),
        ];
        // The settings row is one of them, in the same columns.
        let settings = (!info.own.is_empty() || !info.node.is_empty()).then_some("Settings".len());
        let width = state
            .details
            .iter()
            .map(|row| row.label.chars().count())
            .chain(settings)
            .max()
            .unwrap_or(0);
        lines.extend(state.details.iter().map(|row| {
            (
                format!("  {:<width$}  {}", row.label, row.value),
                Tone::Muted,
            )
        }));
        if !info.node.is_empty() {
            lines.push((
                format!(
                    "  {:<width$}  {}, whose feature offers it",
                    "Node", info.node
                ),
                Tone::Muted,
            ));
        }
        if !info.own.is_empty() {
            let set: Vec<String> = info
                .own
                .iter()
                .map(|name| match info.settings.get(name) {
                    Some(value) => format!("{name}={value}"),
                    None => format!("{name} not set"),
                })
                .collect();
            lines.push((
                format!("  {:<width$}  {}", "Settings", set.join(" · ")),
                Tone::Muted,
            ));
        }
        lines
    }

    fn held(&self) -> &[berth_api::ListedResource] {
        match (&self.resource, &self.state) {
            (None, Some(Ok(state))) => &state.resources,
            _ => &[],
        }
    }

    /// The label of a kind of the provisioner, for its heading.
    fn kind_label(&self, kind: &str) -> String {
        let declared = match &self.state {
            Some(Ok(state)) => state
                .provisioner
                .kinds
                .iter()
                .find(|known| known.id == kind)
                .map(|known| known.label.clone()),
            _ => None,
        };
        match (kind, declared) {
            ("machine", _) => "Machines".into(),
            (_, Some(label)) if !label.is_empty() => label,
            (kind, _) => kind.to_owned(),
        }
    }

    /// The rows: how it is, its actions in their groups, and on a
    /// provisioner's page its collections and what it holds, by kind. Over
    /// it, the table or item shown.
    fn items(&self) -> Vec<Item<Entry>> {
        match self.views.last() {
            Some(view) if view.item => return self.item_items(view),
            Some(view) => return self.table_items(view),
            None => {}
        }
        let mut groups = match self.resource {
            Some(_) => page::groups(&self.actions, Entry::Action),
            None => self.collection_groups(),
        };
        for group in page::groups(&self.actions, Entry::Action) {
            if self.resource.is_some() {
                break;
            }
            match groups
                .iter_mut()
                .find(|known| known.title.eq_ignore_ascii_case(&group.title))
            {
                Some(known) => known.rows.extend(group.rows),
                None => groups.push(group),
            }
        }
        if self.resource.is_some() {
            let logs = matches!(&self.resource, Some(ResourcePart { seen: Some(Ok(seen)), .. }) if seen.verbs.iter().any(|verb| verb == "logs"));
            if logs {
                groups.push(Group {
                    title: "Log".into(),
                    rows: vec![("  ▸ Show its log".into(), Entry::Logs)],
                });
            }
            return page::items(&self.state_lines(), groups);
        }
        let mut kinds: Vec<String> = vec![];
        for held in self.held() {
            if !kinds.contains(&held.kind) {
                kinds.push(held.kind.clone());
            }
        }
        for kind in kinds {
            let rows: Vec<(String, Entry)> = self
                .held()
                .iter()
                .enumerate()
                .filter(|(_, held)| held.kind == kind)
                .map(|(index, held)| {
                    let mut notes = vec![];
                    if !held.state.is_empty() {
                        notes.push(held.state.clone());
                    }
                    if held.resource.is_empty() {
                        notes.push("no record of it".into());
                    }
                    let id = match held.id.as_str() {
                        "" => "being made",
                        id => id,
                    };
                    let name = match (kind.as_str(), held.host.is_empty()) {
                        ("machine", false) => &held.host,
                        _ => &held.name,
                    };
                    (
                        format!("  {name:<18} {id}  {}", notes.join(" · ")),
                        Entry::Resource(index),
                    )
                })
                .collect();
            groups.push(Group {
                title: format!("{} berth made with it", self.kind_label(&kind)),
                rows,
            });
        }
        page::items(&self.state_lines(), groups)
    }

    fn entries(&self) -> Vec<Entry> {
        self.items()
            .into_iter()
            .filter_map(|item| match item {
                Item::Pick(_, entry) => Some(entry),
                _ => None,
            })
            .collect()
    }

    fn selected(&self) -> Option<Entry> {
        let entries = self.entries();
        entries
            .get(self.cursor.min(entries.len().saturating_sub(1)))
            .copied()
    }

    /// The values the provisioner says an argument takes here, else those
    /// it declares.
    fn offer(&self, dialog: &mut Arguments) {
        let Some(argument) = self
            .actions
            .get(dialog.action)
            .and_then(|action| action.arguments.get(dialog.index))
        else {
            return;
        };
        let narrowed = match &self.state {
            Some(Ok(state)) => state.values.get(&argument.name),
            _ => None,
        };
        dialog.choices = narrowed.unwrap_or(&argument.values).clone();
    }

    /// The values the provisioner says its arguments take here, by name.
    fn narrowed(&self) -> &BTreeMap<String, Vec<String>> {
        static NONE: BTreeMap<String, Vec<String>> = BTreeMap::new();
        match &self.state {
            Some(Ok(state)) => &state.values,
            _ => &NONE,
        }
    }

    fn launch(&mut self, (index, mut values): page::Launch) -> Asked {
        let filled = self.filled.get(index).cloned().unwrap_or_default();
        values.extend(filled.values);
        let action = self.actions.get(index).map(|action| {
            let mut action = action.clone();
            if !filled.title.is_empty() {
                action.label = filled.title.clone();
            }
            Box::new(action)
        });
        match (action, filled.provisioner) {
            (Some(action), _) if !action.available => Asked::Nothing(Some(format!(
                "{} cannot run now: {}",
                action.title(),
                action.why
            ))),
            (Some(action), Some(provisioner)) => Asked::Run {
                provisioner,
                action,
                values,
            },
            (Some(action), None) if self.resource.is_some() => Asked::RunResource {
                resource: self.id.clone(),
                action,
                values,
            },
            (Some(action), None) => Asked::Run {
                provisioner: self.id.clone(),
                action,
                values,
            },
            (None, _) => Asked::Nothing(None),
        }
    }

    fn begin(&mut self, index: usize, confirmed: bool) -> Asked {
        if let Some(action) = self.actions.get(index)
            && !action.available
        {
            return Asked::Nothing(Some(format!(
                "{} cannot run now: {}",
                action.title(),
                action.why
            )));
        }
        match page::begin(&self.actions, index, confirmed) {
            Begin::Ask(question) => {
                self.question = Some((question, index));
                Asked::Nothing(None)
            }
            // The form last run opens again as it was.
            Begin::Arguments(_)
                if self
                    .kept
                    .as_ref()
                    .is_some_and(|kept| kept.wanted() == self.actions[index].name) =>
            {
                self.form = self.kept.take();
                if let Some(form) = &mut self.form {
                    form.reopen(index);
                }
                Asked::Nothing(None)
            }
            Begin::Arguments(_) if form::takes(&self.actions[index]) => {
                let form = Form::of(&self.id, index, &self.actions[index], self.narrowed());
                self.form = Some(form);
                Asked::Nothing(None)
            }
            Begin::Arguments(mut dialog) => {
                self.offer(&mut dialog);
                self.arguments = Some(dialog);
                Asked::Nothing(None)
            }
            Begin::Run(launch) => self.launch(launch),
            Begin::Nothing => Asked::Nothing(None),
        }
    }
}

/// An action of a collection where some of its key is known: the arguments
/// that key fills are given, and not asked for again. On an item, it says
/// why it cannot run on that one, and its question names it.
fn derive(
    base: &[Action],
    collection: &Collection,
    used: &CollectionAction,
    known: &BTreeMap<String, String>,
    item: Option<&CollectionItem>,
    group: Option<&str>,
) -> Option<(Action, Filled)> {
    let original = base.iter().find(|action| action.name == used.action)?;
    let mut action = original.clone();
    let mut values = BTreeMap::new();
    for (part, value) in known {
        let argument = used.argument(part);
        if action.arguments.iter().any(|known| known.name == argument) {
            values.insert(argument.to_owned(), value.clone());
            action.arguments.retain(|known| known.name != argument);
        }
    }
    if !used.label.is_empty() {
        action.label = used.label.clone();
    }
    if let Some(group) = group {
        action.group = group.to_owned();
    }
    let mut title = original.title().to_owned();
    if let Some(item) = item {
        title = format!("{title} {}", item.name);
        if let Some(why) = item.unavailable.get(&used.action)
            && action.available
        {
            action.available = false;
            action.why = why.clone();
        }
        let named = format!("{} {}", collection.label, item.name);
        action.confirm = match (&action.confirm, used.action == collection.remove) {
            (Some(question), _) => Some(format!("{named}: {question}")),
            (None, true) => Some(format!("{named}: {}?", original.title())),
            (None, false) => None,
        };
    }
    Some((
        action,
        Filled {
            provisioner: None,
            values,
            title,
        },
    ))
}

/// The line of an action's row over a provisioner's page: one that cannot
/// run says why.
fn view_line(action: &Action) -> String {
    match action.available {
        true => format!("  ▸ {}", action.title()),
        false => format!(
            "  ▸ {} · {}",
            action.title(),
            screen::ellipsize(&action.why, 70)
        ),
    }
}

/// Lines of cells in columns as wide as their widest, at most `limit`.
fn columns(cells: &[Vec<String>], limit: usize) -> Vec<String> {
    let count = cells.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..count)
        .map(|column| {
            cells
                .iter()
                .filter_map(|row| row.get(column))
                .map(|cell| cell.chars().count().min(limit))
                .max()
                .unwrap_or(0)
        })
        .collect();
    cells
        .iter()
        .map(|row| {
            let line: Vec<String> = row
                .iter()
                .enumerate()
                .map(|(column, cell)| {
                    format!(
                        "{:<width$}",
                        screen::ellipsize(cell, limit),
                        width = widths[column]
                    )
                })
                .collect();
            format!("  {}", line.join("  ")).trim_end().to_owned()
        })
        .collect()
}

impl Page {
    /// What the provisioner is, on its own page once it answered.
    fn info(&self) -> Option<&ProvisionerInfo> {
        match (&self.resource, &self.state) {
            (None, Some(Ok(state))) => Some(&state.provisioner),
            _ => None,
        }
    }

    fn collection(&self, id: &str) -> Option<&Collection> {
        self.info()?
            .collections
            .iter()
            .find(|collection| collection.id == id)
    }

    /// The items of a collection within the item whose key is `within`, or
    /// all; none when the provisioner did not give them.
    fn rows(&self, id: &str, within: &BTreeMap<String, String>) -> Option<Vec<&CollectionItem>> {
        let Some(Ok(state)) = &self.state else {
            return None;
        };
        let all = state.collections.get(id)?;
        Some(
            all.iter()
                .filter(|row| {
                    within
                        .iter()
                        .all(|(part, value)| row.key.get(part) == Some(value))
                })
                .collect(),
        )
    }

    /// The item of a collection with this key.
    fn found(&self, id: &str, key: &BTreeMap<String, String>) -> Option<&CollectionItem> {
        let Some(Ok(state)) = &self.state else {
            return None;
        };
        state
            .collections
            .get(id)?
            .iter()
            .find(|row| &row.key == key)
    }

    /// How many items a collection has within `within`, as its row says it.
    fn count(&self, id: &str, within: &BTreeMap<String, String>) -> String {
        match self.rows(id, within) {
            Some(rows) => rows.len().to_string(),
            None => "?".into(),
        }
    }

    /// The rows of collections, as `(title, entries)` groups: those of the
    /// provisioner's page, or those within the item `within` is the key of.
    fn collection_rows(
        &self,
        parent: &str,
        within: &BTreeMap<String, String>,
    ) -> Vec<Group<Entry>> {
        let Some(info) = self.info() else {
            return vec![];
        };
        let shown: Vec<(usize, &Collection)> = info
            .collections
            .iter()
            .enumerate()
            .filter(|(_, collection)| collection.kinds.is_empty())
            .filter(|(_, collection)| parent.is_empty() || collection.within == parent)
            .collect();
        let width = shown
            .iter()
            .map(|(_, collection)| collection.plural.chars().count())
            .max()
            .unwrap_or(0);
        let mut groups: Vec<Group<Entry>> = vec![];
        for (index, collection) in shown {
            let title = match (parent.is_empty(), collection.group.is_empty()) {
                (false, _) => format!("In this {}", self.label_of(parent).to_lowercase()),
                (true, true) => "Collections".to_owned(),
                (true, false) => collection.group.clone(),
            };
            let row = (
                format!(
                    "  ▸ {:<width$}  {}",
                    collection.plural,
                    self.count(&collection.id, within)
                ),
                Entry::Collection(index),
            );
            match groups
                .iter_mut()
                .find(|known| known.title.eq_ignore_ascii_case(&title))
            {
                Some(group) => group.rows.push(row),
                None => groups.push(Group {
                    title,
                    rows: vec![row],
                }),
            }
        }
        groups
    }

    fn collection_groups(&self) -> Vec<Group<Entry>> {
        self.collection_rows("", &BTreeMap::new())
    }

    fn label_of(&self, id: &str) -> String {
        self.collection(id)
            .map_or_else(|| id.to_owned(), |collection| collection.label.clone())
    }

    /// What the lines over a view say while the provisioner is asked, or
    /// when it could not be.
    fn waiting(&self) -> Option<(String, Tone)> {
        match &self.state {
            _ if self.asking => Some((format!("Asking {}…", self.id), Tone::Muted)),
            None => Some((format!("Asking {}…", self.id), Tone::Muted)),
            Some(Err(error)) => Some((format!("✗ {error}"), Tone::Bad)),
            Some(Ok(_)) => None,
        }
    }

    /// The rows of the actions shown over the provisioner's page, in groups.
    fn view_groups(&self) -> Vec<Group<Entry>> {
        let mut groups: Vec<Group<Entry>> = vec![];
        for (index, action) in self.actions.iter().enumerate() {
            let title = page::group_of(action);
            let row = (view_line(action), Entry::Action(index));
            match groups.iter_mut().find(|known| known.title == title) {
                Some(group) => group.rows.push(row),
                None => groups.push(Group {
                    title: title.to_owned(),
                    rows: vec![row],
                }),
            }
        }
        groups
    }

    /// A collection's table: what it lists, a row for each item, and the
    /// actions that make one or act on them all.
    fn table_items(&self, view: &View) -> Vec<Item<Entry>> {
        let Some(collection) = self.collection(&view.collection) else {
            let line = self.waiting().unwrap_or_else(|| {
                (
                    format!("{} has no {}", self.id, view.collection),
                    Tone::Warn,
                )
            });
            return page::items(&[line], vec![]);
        };
        let mut title = format!("{} · {}", collection.plural, self.id);
        if !view.key.is_empty() {
            let name = self
                .found(&collection.within, &view.key)
                .map(|item| item.name.clone())
                .unwrap_or_else(|| view.key.values().cloned().collect::<Vec<_>>().join("/"));
            title += &format!(
                " · in the {} {name}",
                self.label_of(&collection.within).to_lowercase()
            );
        }
        let mut items = vec![Item::Note(title, Tone::Plain)];
        if let Some(line) = self.waiting() {
            items.push(Item::Note(line.0, line.1));
        }
        let rows = self.rows(&collection.id, &view.key);
        match &rows {
            None if self.waiting().is_none() => items.push(Item::Note(
                "○ The provisioner did not list them. r asks again".into(),
                Tone::Warn,
            )),
            None => {}
            Some(rows) if rows.is_empty() => items.push(Item::Note(
                format!("No {} yet", collection.plural.to_lowercase()),
                Tone::Muted,
            )),
            Some(rows) => {
                // A part of the key that is the table's own is not repeated.
                let shown: Vec<&berth_api::CollectionColumn> = collection
                    .columns
                    .iter()
                    .filter(|column| !view.key.contains_key(&column.name))
                    .collect();
                let mut cells = vec![
                    std::iter::once(collection.label.clone())
                        .chain(
                            shown
                                .iter()
                                .map(|column| collection.column_label(column).to_owned()),
                        )
                        .collect::<Vec<_>>(),
                ];
                for row in rows {
                    cells.push(
                        std::iter::once(row.name.clone())
                            .chain(shown.iter().map(|column| {
                                row.columns.get(&column.name).cloned().unwrap_or_default()
                            }))
                            .collect(),
                    );
                }
                let lines = columns(&cells, 32);
                items.push(Item::Blank);
                items.push(Item::Heading(lines[0].clone()));
                items.extend(
                    lines[1..]
                        .iter()
                        .enumerate()
                        .map(|(index, line)| Item::Pick(line.clone(), Entry::Item(index))),
                );
            }
        }
        for group in self.view_groups() {
            items.push(Item::Blank);
            items.push(Item::Heading(group.title));
            items.extend(
                group
                    .rows
                    .into_iter()
                    .map(|(line, entry)| Item::Pick(line, entry)),
            );
        }
        items
    }

    /// An item's page: its values, its actions, and the collections within it.
    fn item_items(&self, view: &View) -> Vec<Item<Entry>> {
        let (Some(collection), Some(item)) = (
            self.collection(&view.collection),
            self.found(&view.collection, &view.key),
        ) else {
            let line = self.waiting().unwrap_or_else(|| {
                (
                    format!(
                        "{} {} is no longer listed",
                        self.label_of(&view.collection),
                        view.key.values().cloned().collect::<Vec<_>>().join("/")
                    ),
                    Tone::Warn,
                )
            });
            return page::items(&[line], vec![]);
        };
        let mut lines = vec![(
            format!("{} {} · {}", collection.label, item.name, self.id),
            Tone::Plain,
        )];
        if let Some(line) = self.waiting() {
            lines.push(line);
        }
        let width = collection
            .columns
            .iter()
            .map(|column| collection.column_label(column).chars().count())
            .max()
            .unwrap_or(0);
        for column in &collection.columns {
            let value = item.columns.get(&column.name).cloned().unwrap_or_default();
            if !value.is_empty() {
                lines.push((
                    format!("  {:<width$}  {value}", collection.column_label(column)),
                    Tone::Muted,
                ));
            }
        }
        let mut groups = self.view_groups();
        groups.extend(self.collection_rows(&collection.id, &view.key));
        page::items(&lines, groups)
    }

    /// Work out the actions of what is shown from those of the subject: the
    /// loose ones on the provisioner's page, a table's that make an item or
    /// act on all, an item's own with its key, or a resource's with the
    /// provisioner's that act on it.
    fn arrange(&mut self) {
        let (actions, filled, making) = self.arranged();
        self.actions = actions;
        self.filled = filled;
        self.making = making;
    }

    fn arranged(&self) -> (Vec<Action>, Vec<Filled>, usize) {
        if self.resource.is_some() {
            let mut actions = self.base.clone();
            let mut filled = vec![Filled::default(); actions.len()];
            for (action, known) in &self.extra {
                actions.push(action.clone());
                filled.push(known.clone());
            }
            return (actions, filled, 0);
        }
        let Some(view) = self.views.last() else {
            let actions: Vec<Action> = self
                .base
                .iter()
                .filter(|action| action.collection.is_empty())
                .cloned()
                .collect();
            let filled = vec![Filled::default(); actions.len()];
            return (actions, filled, 0);
        };
        let Some(collection) = self.collection(&view.collection) else {
            return (vec![], vec![], 0);
        };
        let derived: Vec<(Action, Filled)> = match view.item {
            true => {
                let item = self.found(&collection.id, &view.key);
                collection
                    .item
                    .iter()
                    .filter_map(|used| {
                        derive(
                            &self.base,
                            collection,
                            used,
                            &view.key,
                            item,
                            Some(&collection.label),
                        )
                    })
                    .collect()
            }
            false => {
                let all = format!("All {}", collection.plural.to_lowercase());
                collection
                    .new
                    .iter()
                    .filter_map(|used| {
                        derive(&self.base, collection, used, &view.key, None, Some("New"))
                    })
                    .chain(collection.list.iter().filter_map(|used| {
                        derive(&self.base, collection, used, &view.key, None, Some(&all))
                    }))
                    .collect()
            }
        };
        let making = match view.item {
            true => 0,
            false => derived
                .iter()
                .filter(|(action, _)| collection.new.iter().any(|used| used.action == action.name))
                .count(),
        };
        let (actions, filled) = derived.into_iter().unzip();
        (actions, filled, making)
    }

    /// Show `view` over what is shown now.
    fn push(&mut self, collection: String, key: BTreeMap<String, String>, item: bool) {
        self.views.push(View {
            collection,
            key,
            item,
            back: self.cursor,
        });
        self.cursor = 0;
        self.kept = None;
        self.arrange();
    }

    /// Go back from the view on top to what it covers.
    fn pop(&mut self) {
        if let Some(view) = self.views.pop() {
            self.cursor = view.back;
        }
        self.kept = None;
        self.arrange();
    }

    /// Leave the pages of items that are no longer listed, as after one is removed.
    fn prune(&mut self) {
        if !matches!(self.state, Some(Ok(_))) {
            return;
        }
        let gone = self.views.iter().position(|view| {
            self.collection(&view.collection).is_none()
                || (view.item && self.found(&view.collection, &view.key).is_none())
        });
        if let Some(gone) = gone {
            self.cursor = self.views[gone].back;
            self.views.truncate(gone);
            self.kept = None;
            // The table it was opened from selects the item that took its place.
            let count = self
                .table()
                .and_then(|view| self.rows(&view.collection, &view.key))
                .map_or(0, |rows| rows.len());
            if count > 0 {
                self.cursor = self.cursor.min(count - 1);
            }
        }
    }

    /// The provisioner's actions on a resource it made of a kind one of its
    /// collections has as items, with the resource's id as their key.
    fn on_resource(&self, held: &berth_api::ListedResource) -> Vec<(Action, Filled)> {
        let Some(info) = self.info() else {
            return vec![];
        };
        let mut found = vec![];
        for collection in info
            .collections
            .iter()
            .filter(|collection| collection.kinds.contains(&held.kind))
        {
            let key: BTreeMap<String, String> = collection
                .key
                .iter()
                .map(|part| (part.clone(), held.id.clone()))
                .collect();
            for used in &collection.item {
                if let Some((action, mut filled)) =
                    derive(&self.base, collection, used, &key, None, None)
                {
                    filled.provisioner = Some(self.id.clone());
                    let name = match held.host.is_empty() {
                        true => &held.name,
                        false => &held.host,
                    };
                    filled.title = format!("{} · {name}", action.title());
                    found.push((action, filled));
                }
            }
        }
        found
    }

    /// The table shown, if one is.
    fn table(&self) -> Option<&View> {
        self.views.last().filter(|view| !view.item)
    }

    /// `n` on a table: its one way to make an item, or the rows of those
    /// there are to choose from.
    fn make(&mut self) -> Asked {
        let Some(collection) = self
            .table()
            .and_then(|view| self.collection(&view.collection))
            .cloned()
        else {
            return Asked::Other;
        };
        match self.making {
            0 => Asked::Nothing(Some(format!("{} are not made here", collection.plural))),
            1 => self.begin(0, false),
            _ => {
                if let Some(at) = self
                    .entries()
                    .iter()
                    .position(|entry| *entry == Entry::Action(0))
                {
                    self.cursor = at;
                }
                let ways: Vec<&str> = self.actions[..self.making]
                    .iter()
                    .map(Action::title)
                    .collect();
                Asked::Nothing(Some(format!(
                    "Choose how to make one: {}",
                    ways.join(" or ")
                )))
            }
        }
    }

    /// `x`: remove the item selected in a table, or the item shown, after
    /// a question that names it.
    fn remove(&mut self) -> Asked {
        let Some(view) = self.views.last().cloned() else {
            return Asked::Other;
        };
        let Some(collection) = self.collection(&view.collection).cloned() else {
            return Asked::Nothing(None);
        };
        if collection.remove.is_empty() {
            return Asked::Nothing(Some(format!("{} are not removed here", collection.plural)));
        }
        if view.item {
            let index = self
                .actions
                .iter()
                .position(|action| action.name == collection.remove);
            return match index {
                Some(index) => self.begin(index, false),
                None => Asked::Nothing(None),
            };
        }
        let Some(Entry::Item(index)) = self.selected() else {
            return Asked::Nothing(Some(format!(
                "Choose the {} to remove",
                collection.label.to_lowercase()
            )));
        };
        let Some(item) = self
            .rows(&collection.id, &view.key)
            .and_then(|rows| rows.get(index).map(|row| (*row).clone()))
        else {
            return Asked::Nothing(None);
        };
        let Some(used) = collection
            .item
            .iter()
            .find(|used| used.action == collection.remove)
        else {
            return Asked::Nothing(None);
        };
        let Some((action, filled)) = derive(
            &self.base,
            &collection,
            used,
            &item.key,
            Some(&item),
            Some(&collection.label),
        ) else {
            return Asked::Nothing(None);
        };
        self.arrange();
        self.actions.push(action);
        self.filled.push(filled);
        self.begin(self.actions.len() - 1, false)
    }

    /// Take in what the provisioner or the resource said, and its actions.
    fn take(
        &mut self,
        state: Result<ProvisionerState, String>,
        actions: Result<Vec<Action>, String>,
    ) {
        self.asking = false;
        self.state = Some(match (state, &actions) {
            (Ok(state), Err(error)) => Err(format!(
                "{} answered, but not with its actions: {error}",
                state.provisioner.named()
            )),
            (state, _) => state,
        });
        self.base = actions.unwrap_or_default();
        self.prune();
        // An action being asked for keeps its place until it runs.
        if self.question.is_none() && self.arguments.is_none() || self.views.is_empty() {
            self.arrange();
        }
    }
}

impl Tab {
    /// The selected provisioner of the list.
    /// None on the row that registers one.
    pub fn selected<'a>(&mut self, list: &'a [ProvisionerInfo]) -> Option<&'a ProvisionerInfo> {
        self.selected = self.selected.min(list.len());
        list.get(self.selected)
    }

    /// Open the dialog that registers a provisioner, with the types there
    /// are; what to say when there is none.
    pub fn adding(&mut self, types: Result<Vec<ProvisionerInfo>, String>) -> Option<String> {
        let types = match types {
            Ok(types) if types.is_empty() => {
                return Some("No plugin brings a provisioner. See the Plugins tab".into());
            }
            Ok(types) => types,
            Err(error) => return Some(format!("✗ {error}")),
        };
        let actions = vec![add_action(&types)];
        let Begin::Arguments(mut dialog) = page::begin(&actions, 0, true) else {
            return None;
        };
        dialog.choices = actions[0].arguments[0].values.clone();
        self.adding = Some((actions, dialog));
        None
    }

    /// Make a resource of `kind` that is no host, such as an instance: the
    /// form of the provisioners that make one, of which the first is asked
    /// for its action.
    pub fn create(&mut self, list: &[ProvisionerInfo], kind: &str) -> Asked {
        let label = MAKES
            .iter()
            .find(|(known, _)| *known == kind)
            .map_or(kind, |(_, label)| label);
        let makers: Vec<(String, String)> = list
            .iter()
            .filter(|provisioner| provisioner.kinds.iter().any(|known| known.id == kind))
            .map(|provisioner| (provisioner.named().to_owned(), provisioner.label.clone()))
            .collect();
        let Some((first, _)) = makers.first().cloned() else {
            return Asked::Nothing(Some(format!(
                "No provisioner makes a {label}. Add one on the Provisioners tab"
            )));
        };
        let form = Form::making(&format!("new {label}"), &format!("new-{kind}"), makers);
        self.page = Some(Page {
            form: Some(form),
            ..Page::new(&first)
        });
        Asked::Open(first)
    }

    /// Leave a dialog, on changing tabs.
    pub fn leave(&mut self) {
        self.adding = None;
        if let Some(page) = &mut self.page {
            page.question = None;
            page.arguments = None;
            page.form = None;
        }
    }

    /// Put pasted text into the argument being asked for, if one is.
    pub fn paste(&mut self, text: &str) {
        if let Some(form) = self.page.as_mut().and_then(|page| page.form.as_mut()) {
            return form.paste(text);
        }
        let dialog = match &mut self.adding {
            Some((_, dialog)) => Some(dialog),
            None => self.page.as_mut().and_then(|page| page.arguments.as_mut()),
        };
        match dialog {
            Some(dialog) if dialog.text => {
                crate::event::paste_lines(&mut dialog.input, text, page::NEWLINE)
            }
            Some(dialog) => crate::event::paste(&mut dialog.input, text),
            None => {}
        }
    }

    /// Whether keys go to a dialog or a form, so none of them changes tabs.
    pub fn typing(&self) -> bool {
        self.adding.is_some()
            || self.page.as_ref().is_some_and(|page| {
                page.question.is_some() || page.arguments.is_some() || page.form.is_some()
            })
    }

    /// The form being filled in, for the screen to draw over everything.
    pub fn form(&self) -> Option<&Form> {
        self.page.as_ref()?.form.as_ref()
    }

    /// A click while the form is open: it selects a row of it, or chooses
    /// the row selected.
    pub fn click(
        &mut self,
        column: u16,
        row: u16,
        list: &[ProvisionerInfo],
        platform: &mut dyn Platform,
    ) -> Asked {
        let key = self
            .page
            .as_mut()
            .and_then(|page| page.form.as_mut())
            .and_then(|form| form.click(column, row));
        match key {
            Some(key) => self.key(key, list, platform),
            None => Asked::Nothing(None),
        }
    }

    /// Take in what a provisioner said for its page.
    pub fn opened(
        &mut self,
        id: &str,
        state: Result<ProvisionerState, String>,
        actions: Result<Vec<Action>, String>,
    ) {
        // A resource's page keeps its provisioner's page, as after an action of
        // the provisioner run there.
        if let Some(back) = self
            .page
            .as_mut()
            .filter(|page| page.id != id)
            .and_then(|page| page.back.as_mut())
            .filter(|back| back.id == id)
        {
            return back.take(state, actions);
        }
        let Some(page) = self.page.as_mut().filter(|page| page.id == id) else {
            return;
        };
        page.take(state, actions);
        // A form that waits for the provisioner's action takes it now.
        let narrowed = match &page.state {
            Some(Ok(state)) => state.values.clone(),
            _ => BTreeMap::new(),
        };
        if let Some(form) = page.form.as_mut().filter(|form| form.waiting()) {
            let found = page
                .actions
                .iter()
                .enumerate()
                .find(|(_, action)| action.name == form.wanted());
            match (found, &page.state) {
                (Some((index, action)), _) if action.available => {
                    form.fill(index, action, &narrowed)
                }
                (Some((_, action)), _) => {
                    form.fail(format!("{} cannot run now: {}", action.title(), action.why))
                }
                (None, Some(Err(error))) => form.fail(error.clone()),
                (None, _) => form.fail(format!("{id} does not make these")),
            }
        }
    }

    /// Leave the page of the resource `id` for its provisioner's, as when it
    /// is gone: the provisioner, to ask again.
    pub fn back_from(&mut self, id: &str) -> Option<String> {
        let page = self.page.as_mut().filter(|page| page.id == id)?;
        let back = page.back.take()?;
        let provisioner = back.id.clone();
        self.page = Some(*back);
        Some(provisioner)
    }

    /// Take in what a resource said for its page.
    pub fn opened_resource(
        &mut self,
        id: &str,
        seen: Result<Resource, String>,
        actions: Result<Vec<Action>, String>,
    ) {
        let Some(page) = self.page.as_mut().filter(|page| page.id == id) else {
            return;
        };
        let Some(part) = page.resource.as_mut() else {
            return;
        };
        part.seen = Some(match (seen, &actions) {
            (Ok(seen), Err(error)) => Err(format!(
                "{} answered, but not with its actions: {error}",
                seen.id
            )),
            (seen, _) => seen,
        });
        page.base = actions.unwrap_or_default();
        page.arrange();
    }

    /// The question waiting for an answer, for the screen to draw.
    pub fn question(&self) -> Option<&str> {
        self.page
            .as_ref()
            .and_then(|page| page.question.as_ref())
            .map(|(question, _)| question.as_str())
    }

    /// The arguments being asked for, with the actions they are of.
    pub fn arguments(&self) -> Option<(&[Action], &Arguments)> {
        if let Some((actions, dialog)) = &self.adding {
            return Some((actions, dialog));
        }
        let page = self.page.as_ref()?;
        Some((&page.actions, page.arguments.as_ref()?))
    }

    /// Choose the row at `row` of what was shown, as a click does.
    pub fn select_row(&mut self, row: usize) {
        match &mut self.page {
            None => self.selected = row,
            Some(page) => {
                let picks = page
                    .items()
                    .iter()
                    .take(row + 1)
                    .filter(|item| matches!(item, Item::Pick(..)))
                    .count();
                if matches!(page.items().get(row), Some(Item::Pick(..))) {
                    page.cursor = picks.saturating_sub(1);
                }
            }
        }
    }

    /// The row shown as selected.
    pub fn selected_row(&self) -> usize {
        match &self.page {
            None => self.selected,
            Some(page) => {
                let mut picks = 0;
                for (row, item) in page.items().iter().enumerate() {
                    if let Item::Pick(..) = item {
                        if picks == page.cursor {
                            return row;
                        }
                        picks += 1;
                    }
                }
                0
            }
        }
    }

    pub fn shown(&self, list: &[ProvisionerInfo], place: &str) -> Shown {
        let Some(page) = &self.page else {
            let selected = self.selected.min(list.len());
            return Shown {
                rows: rows(list),
                selected,
                help: help(list.get(selected), place),
                keys: "↑↓ select · Enter open · a add · r refresh · Esc hosts",
            };
        };
        let items = page.items();
        let muted = Style::new().fg(theme::MUTED);
        let rows = items
            .iter()
            .map(|item| match item {
                Item::Heading(title) => {
                    Line::styled(title.clone(), Style::new().fg(theme::ACCENT).bold())
                }
                Item::Note(text, tone) => Line::styled(
                    text.clone(),
                    Style::new().fg(match tone {
                        Tone::Plain => theme::TEXT,
                        Tone::Muted => theme::MUTED,
                        Tone::Warn => theme::WARN,
                        Tone::Bad => theme::BAD,
                    }),
                ),
                Item::Blank => Line::raw(""),
                Item::Pick(text, Entry::Resource(index))
                    if page
                        .held()
                        .get(*index)
                        .is_some_and(|held| held.resource.is_empty()) =>
                {
                    Line::styled(text.clone(), muted)
                }
                // What cannot run now is greyed over a provisioner's page, with why.
                Item::Pick(text, Entry::Action(index))
                    if !page.views.is_empty()
                        && page
                            .actions
                            .get(*index)
                            .is_some_and(|action| !action.available) =>
                {
                    Line::styled(text.clone(), muted)
                }
                Item::Pick(text, _) => Line::styled(text.clone(), Style::new().fg(theme::TEXT)),
            })
            .collect();
        let help = match page.selected() {
            Some(Entry::Action(index)) => {
                let filled = page.filled.get(index).cloned().unwrap_or_default();
                let given: String = filled
                    .values
                    .iter()
                    .map(|(key, value)| format!(" {key}={value}"))
                    .collect();
                let name = &page.actions[index].name;
                page::help(
                    &page.actions[index],
                    &match (&page.resource, &filled.provisioner) {
                        (_, Some(provisioner)) => {
                            format!("berth provisioner run {provisioner} {name}{given}")
                        }
                        (Some(_), None) => format!("berth resource run {} {name}", page.id),
                        (None, None) => format!("berth provisioner run {} {name}{given}", page.id),
                    },
                )
            }
            Some(Entry::Collection(index)) => {
                let collection = &page.info().expect("a collection is listed").collections[index];
                let mut text = collection.help.clone();
                if !text.is_empty() {
                    text += "\n\n";
                }
                text += "Enter opens them as a table, and each of them with its actions.";
                let mut keys = vec![];
                if !collection.new.is_empty() {
                    keys.push("n makes one");
                }
                if !collection.remove.is_empty() {
                    keys.push("x removes the one selected");
                }
                if !keys.is_empty() {
                    text += &format!(" There, {}.", keys.join(" and "));
                }
                text += &format!(
                    "\n\nFrom the command line: `berth provisioner show {}`",
                    page.id
                );
                if !collection.show.is_empty() {
                    text += &format!(
                        ", or `berth provisioner run {} {}` for these alone",
                        page.id, collection.show
                    );
                }
                text
            }
            Some(Entry::Item(index)) => {
                let view = page.table().expect("an item is in a table");
                let collection = page.collection(&view.collection).expect("it is listed");
                let row = page
                    .rows(&view.collection, &view.key)
                    .and_then(|rows| rows.get(index).copied());
                match row {
                    Some(row) => {
                        let mut text =
                            format!("The {} {}.", collection.label.to_lowercase(), row.name);
                        for (action, why) in &row.unavailable {
                            let title = page
                                .base
                                .iter()
                                .find(|known| &known.name == action)
                                .map_or(action.as_str(), Action::title);
                            text += &format!("\n\n{title} cannot run on it: {why}");
                        }
                        text + "\n\nEnter opens it with its actions."
                    }
                    None => String::new(),
                }
            }
            Some(Entry::Resource(index)) => {
                let held = &page.held()[index];
                match (held.resource.is_empty(), held.kind.as_str()) {
                    (false, "machine") if !held.host.is_empty() => format!(
                        "The machine of {}, {} as the provisioner calls it. Enter opens its page, with its state, its actions and its log. It is looked after as that host too, on the host's page. From the command line: `berth resource show {}`",
                        held.host, held.id, held.resource
                    ),
                    (false, _) => format!(
                        "The {} {}, {} as the provisioner calls it. Enter opens its page, with its state, its actions and its log. From the command line: `berth resource show {}`",
                        held.kind, held.name, held.id, held.resource
                    ),
                    (true, _) => format!(
                        "{} is listed by the provisioner, but berth has no record of it: it was made otherwise, or berth lost track of it. berth does not remove it; the provisioner's own tools do.",
                        held.id
                    ),
                }
            }
            Some(Entry::Logs) => format!(
                "Its log as its provisioner gives it, the last lines and then what follows, with secrets masked. From the command line: `berth resource logs {} -f`",
                page.id
            ),
            None if page.resource.is_some() => format!(
                "Asking about the resource. From the command line: `berth resource show {}`",
                page.id
            ),
            None => format!(
                "Asking the provisioner how it is. From the command line: `berth provisioner show {}`",
                page.id
            ),
        };
        let collection = page
            .views
            .last()
            .and_then(|view| page.collection(&view.collection));
        let removes = collection.is_some_and(|collection| !collection.remove.is_empty());
        let keys = match (page.views.last(), page.making, removes) {
            (None, _, _) if page.resource.is_none() => {
                "↑↓ select · Enter open or run · r ask again · Esc back"
            }
            (None, _, _) => "↑↓ select · Enter run · r ask again · Esc back",
            (Some(view), _, true) if view.item => {
                "↑↓ select · Enter run · x remove · r refresh · Esc back"
            }
            (Some(view), _, false) if view.item => "↑↓ select · Enter run · r refresh · Esc back",
            (Some(_), 0, true) => "↑↓ select · Enter open · x remove · r refresh · Esc back",
            (Some(_), 0, false) => "↑↓ select · Enter open · r refresh · Esc back",
            (Some(_), _, true) => {
                "↑↓ select · Enter open · n new · x remove · r refresh · Esc back"
            }
            (Some(_), _, false) => "↑↓ select · Enter open · n new · r refresh · Esc back",
        };
        Shown {
            rows,
            selected: self.selected_row(),
            help,
            keys,
        }
    }

    /// A key on the tab. `platform` copies what is typed in a dialog.
    pub fn key(
        &mut self,
        key: Key,
        list: &[ProvisionerInfo],
        platform: &mut dyn Platform,
    ) -> Asked {
        if let Some((actions, dialog)) = self.adding.take() {
            let mut said = None;
            let mut open = Some(dialog);
            let accepted =
                screen::action_arguments_event(&mut open, &Event::Key(key), platform, &mut said);
            if accepted && let Some(dialog) = open.take() {
                return match page::accept(&actions, dialog) {
                    Accepted::Next(mut next) => {
                        next.choices = actions[0].arguments[next.index].values.clone();
                        self.adding = Some((actions, next));
                        Asked::Nothing(None)
                    }
                    Accepted::Missing(dialog, said) => {
                        self.adding = Some((actions, dialog));
                        Asked::Nothing(Some(said))
                    }
                    Accepted::Run((_, values)) => Asked::Save {
                        name: values.get("name").cloned().unwrap_or_default(),
                        kind: values.get("type").cloned().unwrap_or_default(),
                    },
                };
            }
            // Esc closes it; any other key leaves it open.
            self.adding = open.map(|dialog| (actions, dialog));
            return Asked::Nothing(said);
        }
        let Some(page) = &mut self.page else {
            // The provisioners, then the row that registers one.
            let count = list.len() + 1;
            match key.code {
                Code::Up | Code::Char('k') => {
                    self.selected = (self.selected.min(count - 1) + count - 1) % count
                }
                Code::Down | Code::Char('j') => {
                    self.selected = (self.selected.min(count - 1) + 1) % count
                }
                Code::Enter => {
                    if let Some(provisioner) = self.selected(list) {
                        let id = provisioner.named().to_owned();
                        self.page = Some(Page::new(&id));
                        return Asked::Open(id);
                    }
                    return Asked::Add;
                }
                Code::Char('a') => return Asked::Add,
                Code::Char('r') => return Asked::List,
                _ => return Asked::Other,
            }
            return Asked::Nothing(None);
        };
        if let Some(mut form) = page.form.take() {
            return match form.key(key, platform) {
                Did::Stay(said) => {
                    page.form = Some(form);
                    Asked::Nothing(said)
                }
                // What was chosen on the hosts' list goes back there.
                Did::Leave if form.makes() => {
                    self.page = None;
                    Asked::Hosts
                }
                Did::Leave => Asked::Nothing(None),
                Did::Switch(id) => {
                    self.page = Some(Page {
                        form: Some(form),
                        ..Page::new(&id)
                    });
                    Asked::Open(id)
                }
                Did::Run(launch) => {
                    page.kept = Some(form);
                    page.launch(launch)
                }
            };
        }
        if let Some((_, index)) = page.question.clone() {
            page.question = None;
            return match key.code {
                Code::Char('y') | Code::Enter => page.begin(index, true),
                _ => Asked::Nothing(Some("Not run".into())),
            };
        }
        if page.arguments.is_some() {
            let mut said = None;
            let event = Event::Key(key);
            if screen::action_arguments_event(&mut page.arguments, &event, platform, &mut said)
                && let Some(dialog) = page.arguments.take()
            {
                return match page::accept(&page.actions, dialog) {
                    Accepted::Next(mut next) => {
                        page.offer(&mut next);
                        page.arguments = Some(next);
                        Asked::Nothing(None)
                    }
                    Accepted::Missing(dialog, said) => {
                        page.arguments = Some(dialog);
                        Asked::Nothing(Some(said))
                    }
                    Accepted::Run(launch) => page.launch(launch),
                };
            }
            return Asked::Nothing(said);
        }
        let count = page.entries().len();
        match key.code {
            Code::Up | Code::Char('k') if count > 0 => {
                page.cursor = (page.cursor.min(count - 1) + count - 1) % count
            }
            Code::Down | Code::Char('j') if count > 0 => {
                page.cursor = (page.cursor.min(count - 1) + 1) % count
            }
            Code::Enter => match page.selected() {
                Some(Entry::Action(index)) => {
                    return page.begin(index, false);
                }
                Some(Entry::Resource(index)) => {
                    let held = page.held()[index].clone();
                    if held.resource.is_empty() {
                        return Asked::Nothing(Some(format!("berth has no record of {}", held.id)));
                    }
                    let extra = page.on_resource(&held);
                    let back = self.page.take().map(Box::new);
                    let provisioner = back
                        .as_ref()
                        .map(|page| page.id.clone())
                        .unwrap_or_default();
                    self.page = Some(Page {
                        resource: Some(ResourcePart {
                            provisioner,
                            seen: None,
                        }),
                        back,
                        extra,
                        ..Page::new(&held.resource)
                    });
                    return Asked::OpenResource(held.resource);
                }
                Some(Entry::Logs) => return Asked::Logs(page.id.clone()),
                Some(Entry::Collection(index)) => {
                    let Some(collection) = page.info().and_then(|info| info.collections.get(index))
                    else {
                        return Asked::Nothing(None);
                    };
                    let id = collection.id.clone();
                    // Within an item, the table lists what that item holds.
                    let within = page
                        .views
                        .last()
                        .filter(|view| view.item)
                        .map(|view| view.key.clone())
                        .unwrap_or_default();
                    page.push(id, within, false);
                }
                Some(Entry::Item(index)) => {
                    let Some(view) = page.table().cloned() else {
                        return Asked::Nothing(None);
                    };
                    let key = page
                        .rows(&view.collection, &view.key)
                        .and_then(|rows| rows.get(index).map(|row| row.key.clone()));
                    if let Some(key) = key {
                        page.push(view.collection, key, true);
                    }
                }
                None => {}
            },
            Code::Char('n') if page.table().is_some() => return page.make(),
            Code::Char('x') | Code::Delete if !page.views.is_empty() => return page.remove(),
            // Over the provisioner's page, what it said stays till it answers.
            Code::Char('r') if !page.views.is_empty() => {
                page.asking = true;
                return Asked::Open(page.id.clone());
            }
            Code::Char('r') => {
                let id = page.id.clone();
                page.state = None;
                if let Some(part) = &mut page.resource {
                    part.seen = None;
                    return Asked::OpenResource(id);
                }
                return Asked::Open(id);
            }
            Code::Esc | Code::Backspace | Code::Left if !page.views.is_empty() => page.pop(),
            Code::Esc | Code::Backspace | Code::Left => {
                // A resource's page goes back to its provisioner's, which is asked again.
                if let Some(back) = page.back.take() {
                    let id = back.id.clone();
                    self.page = Some(*back);
                    return Asked::Open(id);
                }
                self.page = None;
                return Asked::List;
            }
            _ => return Asked::Other,
        }
        Asked::Nothing(None)
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_kinds_label_is_made_plural_where_the_sentence_counts_them() {
        assert_eq!(plural("Compose project"), "Compose projects");
        assert_eq!(plural("Proxy"), "Proxies");
        assert_eq!(plural("Gateway"), "Gateways");
        assert_eq!(plural("Box"), "Boxes");
        assert_eq!(plural("Volumes"), "Volumes");
    }

    use super::*;
    use berth_api::{Argument, Detail, ListedResource};

    struct Nowhere;

    impl Platform for Nowhere {
        fn copy(&mut self, _: &str) -> bool {
            false
        }
        fn paste(&mut self) -> Option<String> {
            None
        }
        fn open(&mut self, _: &str) -> Result<(), String> {
            Ok(())
        }
    }

    fn listed() -> Vec<ProvisionerInfo> {
        vec![
            ProvisionerInfo {
                id: "docker/container".into(),
                label: "Docker container".into(),
                description: "A container.".into(),
                own: vec!["host".into()],
                ..Default::default()
            },
            ProvisionerInfo {
                id: "lab/old".into(),
                label: "Old".into(),
                problem: "lab/old speaks version 1".into(),
                ..Default::default()
            },
        ]
    }

    fn actions() -> Vec<Action> {
        vec![
            Action {
                name: "images".into(),
                label: "Images".into(),
                group: "Engine".into(),
                changes: false,
                ..Action::default()
            },
            Action {
                name: "prune".into(),
                label: "Prune".into(),
                group: "Storage".into(),
                confirm: Some("Prune?".into()),
                arguments: vec![Argument {
                    name: "volumes".into(),
                    default: "no".into(),
                    values: vec!["no".into(), "yes".into()],
                    ..Argument::default()
                }],
                ..Action::default()
            },
            Action {
                name: "settings".into(),
                label: "Settings".into(),
                group: "Set-Up".into(),
                arguments: vec![Argument {
                    name: "runtime".into(),
                    values: vec!["privileged".into(), "sysbox".into()],
                    ..Argument::default()
                }],
                ..Action::default()
            },
        ]
    }

    fn key(code: Code) -> Key {
        Key::plain(code)
    }

    #[test]
    fn a_provisioner_is_registered_with_a_name_and_a_type() {
        let mut tab = Tab::default();
        let none: Vec<ProvisionerInfo> = vec![];
        // An empty list has the row that adds one; Enter on it and a both ask for the types.
        assert_eq!(
            text(&tab.shown(&none, "this machine")),
            ["+ Add provisioner"]
        );
        assert_eq!(tab.key(key(Code::Enter), &none, &mut Nowhere), Asked::Add);
        // Under those there are, it is the last row, and Up from the first reaches it.
        let some = listed();
        let rows = text(&tab.shown(&some, "this machine"));
        assert_eq!(rows.len(), some.len() + 1);
        assert_eq!(rows.last().unwrap(), "+ Add provisioner");
        assert!(!rows[0].contains('('), "{rows:?}");
        tab.key(key(Code::Up), &some, &mut Nowhere);
        assert_eq!(tab.shown(&some, "this machine").selected, some.len());
        assert_eq!(tab.key(key(Code::Enter), &some, &mut Nowhere), Asked::Add);
        tab.key(key(Code::Down), &some, &mut Nowhere);
        assert_eq!(tab.selected, 0);
        assert_eq!(
            tab.key(key(Code::Char('a')), &none, &mut Nowhere),
            Asked::Add
        );
        assert_eq!(
            tab.adding(Ok(vec![])).as_deref(),
            Some("No plugin brings a provisioner. See the Plugins tab")
        );
        assert!(tab.adding(Ok(listed())).is_none() && tab.typing());
        // A name is needed, then the type is one of those offered.
        assert_eq!(
            tab.key(key(Code::Enter), &none, &mut Nowhere),
            Asked::Nothing(Some("Name is needed".into()))
        );
        for ch in "lab".chars() {
            tab.key(key(Code::Char(ch)), &none, &mut Nowhere);
        }
        tab.key(key(Code::Enter), &none, &mut Nowhere);
        let (actions, asked) = tab.arguments().unwrap();
        assert_eq!(actions[0].arguments[asked.index].name, "type");
        assert_eq!(asked.choices, ["docker/container", "lab/old"]);
        assert_eq!(asked.input.value(), "docker/container");
        tab.key(key(Code::Down), &none, &mut Nowhere);
        tab.key(key(Code::Down), &none, &mut Nowhere);
        assert_eq!(
            tab.key(key(Code::Enter), &none, &mut Nowhere),
            Asked::Save {
                name: "lab".into(),
                kind: "lab/old".into()
            }
        );
        assert!(!tab.typing());
        // Esc leaves it.
        tab.adding(Ok(listed()));
        tab.key(key(Code::Esc), &none, &mut Nowhere);
        assert!(!tab.typing());
    }

    #[test]
    fn an_instance_is_made_in_a_form_that_chooses_its_provisioner() {
        let kind = |id: &str| berth_api::ProvisionerKind {
            id: id.into(),
            ..Default::default()
        };
        let mut some = listed();
        let mut tab = Tab::default();
        // None makes one: it says where to add one.
        assert!(matches!(
            tab.create(&some, "instance"),
            Asked::Nothing(Some(said)) if said.contains("Provisioners tab")
        ));
        assert!(tab.form().is_none());
        // The form opens at once, and the first that makes one is asked for its action.
        some[0].kinds = vec![kind("machine"), kind("instance")];
        some[1].kinds = vec![kind("instance")];
        let (first, second) = (some[0].id.clone(), some[1].id.clone());
        assert_eq!(tab.create(&some, "instance"), Asked::Open(first.clone()));
        assert!(tab.typing());
        assert!(tab.form().unwrap().waiting());
        let made = Action {
            name: "new-instance".into(),
            available: true,
            arguments: vec![
                Argument {
                    name: "name".into(),
                    required: true,
                    ..Argument::default()
                },
                Argument {
                    name: "image".into(),
                    ..Argument::default()
                },
            ],
            ..Action::default()
        };
        tab.opened(
            &first,
            Err("not asked".into()),
            Ok(vec![Action::default(), made.clone()]),
        );
        let form = tab.form().unwrap();
        assert_eq!(
            form.context(),
            format!("{first} › new resource from an image")
        );
        assert!(!form.waiting());
        // Enter on its first row goes on to the next provisioner, which is asked.
        assert_eq!(
            tab.key(key(Code::Enter), &some, &mut Nowhere),
            Asked::Open(second.clone())
        );
        assert!(tab.form().unwrap().waiting());
        // One without the action says so in the form.
        tab.opened(&second, Err("it does not answer".into()), Ok(vec![]));
        assert!(!tab.form().unwrap().waiting());
        assert_eq!(
            tab.key(key(Code::Enter), &some, &mut Nowhere),
            Asked::Open(first.clone())
        );
        tab.opened(&first, Err("not asked".into()), Ok(vec![made]));
        // Run needs the name; with it the provisioner's action is run.
        for code in [Code::Down, Code::Enter, Code::Char('a'), Code::Enter] {
            tab.key(key(code), &some, &mut Nowhere);
        }
        tab.key(key(Code::Up), &some, &mut Nowhere);
        tab.key(key(Code::Up), &some, &mut Nowhere);
        let Asked::Run {
            provisioner,
            action,
            values,
        } = tab.key(key(Code::Enter), &some, &mut Nowhere)
        else {
            panic!("not run");
        };
        assert_eq!(
            (provisioner, action.name.as_str(), values["name"].as_str()),
            (first.clone(), "new-instance", "a")
        );
        // Its page is under the form; the action there opens the form as it was.
        assert!(tab.form().is_none() && tab.page.is_some());
        tab.key(key(Code::Enter), &some, &mut Nowhere);
        assert!(tab.form().is_some());
        // Esc leaves for the hosts' list, where it was chosen.
        assert_eq!(tab.key(key(Code::Esc), &some, &mut Nowhere), Asked::Hosts);
        assert!(tab.page.is_none());
    }

    #[test]
    fn a_text_argument_keeps_the_lines_that_are_pasted() {
        let actions = vec![Action {
            name: "new-deployment".into(),
            arguments: vec![Argument {
                name: "spec".into(),
                required: true,
                kind: Argument::TEXT.into(),
                ..Argument::default()
            }],
            ..Action::default()
        }];
        let Begin::Arguments(dialog) = page::begin(&actions, 0, true) else {
            panic!("it asks for its spec");
        };
        assert!(dialog.text);
        let mut open = Some(dialog);
        let mut said = None;
        let pasted = Event::Paste("services:\r\n  wiki:\n\timage: wiki\n".into());
        screen::action_arguments_event(&mut open, &pasted, &mut Nowhere, &mut said);
        let dialog = open.unwrap();
        assert_eq!(dialog.input.value(), "services:⏎  wiki:⏎  image: wiki⏎");
        match page::accept(&actions, dialog) {
            Accepted::Run((_, values)) => {
                assert_eq!(values["spec"], "services:\n  wiki:\n  image: wiki\n")
            }
            _ => panic!("all is there"),
        }
    }

    fn text(shown: &Shown) -> Vec<String> {
        shown
            .rows
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn the_list_says_which_can_be_used_and_opens_a_page() {
        let list = listed();
        let mut tab = Tab::default();
        let shown = tab.shown(&list, "this machine");
        let rows = text(&shown);
        assert!(rows[0].contains("● usable"), "{rows:?}");
        assert!(rows[1].contains("○ lab/old speaks version 1"), "{rows:?}");
        assert!(shown.help.contains("host is not set"), "{}", shown.help);
        assert_eq!(
            tab.key(key(Code::Down), &list, &mut Nowhere),
            Asked::Nothing(None)
        );
        assert!(
            tab.shown(&list, "here")
                .help
                .contains("It cannot be used here")
        );
        assert_eq!(
            tab.key(key(Code::Char('r')), &list, &mut Nowhere),
            Asked::List
        );
        assert_eq!(
            tab.key(key(Code::Char('2')), &list, &mut Nowhere),
            Asked::Other
        );
        tab.selected = 0;
        assert_eq!(
            tab.key(key(Code::Enter), &list, &mut Nowhere),
            Asked::Open("docker/container".into())
        );
        let shown = tab.shown(&list, "here");
        assert!(text(&shown)[0].starts_with("Asking docker/container"));
        // Back to the list.
        assert_eq!(tab.key(key(Code::Esc), &list, &mut Nowhere), Asked::List);
        assert!(tab.page.is_none());
    }

    #[test]
    fn a_page_shows_its_state_and_runs_its_actions_after_their_questions() {
        let list = listed();
        let mut tab = Tab::default();
        tab.key(key(Code::Enter), &list, &mut Nowhere);
        let state = ProvisionerState {
            provisioner: list[0].clone(),
            state: "ready".into(),
            message: "Docker 27.1 answers".into(),
            details: vec![Detail {
                label: "Runtimes".into(),
                value: "runc".into(),
            }],
            values: [("runtime".to_owned(), vec!["privileged".to_owned()])].into(),
            resources: vec![
                ListedResource {
                    id: "berth-studio".into(),
                    kind: "machine".into(),
                    name: "studio".into(),
                    host: "studio".into(),
                    state: "running".into(),
                    resource: "r-studio".into(),
                },
                ListedResource {
                    id: "wiki".into(),
                    kind: "deployment".into(),
                    name: "wiki".into(),
                    state: "running".into(),
                    resource: "r-wiki".into(),
                    ..ListedResource::default()
                },
            ],
            ..ProvisionerState::default()
        };
        // What arrives for another page is not taken.
        tab.opened("lab/old", Err("no".into()), Ok(vec![]));
        tab.opened("docker/container", Ok(state), Ok(actions()));
        let shown = tab.shown(&list, "here");
        let rows = text(&shown);
        for line in [
            "Docker container · docker/container",
            "● ready · Docker 27.1 answers",
            "  Runtimes  runc",
            "  Settings  host not set",
            "Engine",
            "  ▸ Images",
            "Machines berth made with it",
            "deployment berth made with it",
        ] {
            assert!(rows.iter().any(|row| row == line), "{line:?} in {rows:?}");
        }
        assert!(rows.iter().any(|row| row.contains("berth-studio  running")));
        // The first action is selected; one that only reads runs at once.
        assert_eq!(rows[shown.selected], "  ▸ Images");
        assert!(
            shown
                .help
                .contains("berth provisioner run docker/container images")
        );
        let Asked::Run { action, values, .. } = tab.key(key(Code::Enter), &list, &mut Nowhere)
        else {
            panic!("not run");
        };
        assert_eq!((action.name.as_str(), values.len()), ("images", 0));
        // A switch opens the form, which asks the question when it is run.
        tab.key(key(Code::Down), &list, &mut Nowhere);
        assert_eq!(
            tab.key(key(Code::Enter), &list, &mut Nowhere),
            Asked::Nothing(None)
        );
        assert!(tab.question().is_none() && tab.typing());
        let form = tab.form().unwrap();
        assert_eq!(form.context(), "docker/container › prune");
        tab.key(key(Code::Char(' ')), &list, &mut Nowhere);
        tab.key(key(Code::Down), &list, &mut Nowhere);
        tab.key(key(Code::Enter), &list, &mut Nowhere);
        assert_eq!(tab.form().unwrap().asking(), Some("Prune? volumes is on."));
        assert_eq!(
            tab.key(key(Code::Char('n')), &list, &mut Nowhere),
            Asked::Nothing(Some("Not run".into()))
        );
        tab.key(key(Code::Enter), &list, &mut Nowhere);
        let Asked::Run { action, values, .. } = tab.key(key(Code::Char('y')), &list, &mut Nowhere)
        else {
            panic!("not run");
        };
        assert_eq!(
            (action.name.as_str(), values["volumes"].as_str()),
            ("prune", "yes")
        );
        // What the provisioner says it takes here narrows what is offered.
        tab.key(key(Code::Down), &list, &mut Nowhere);
        tab.key(key(Code::Enter), &list, &mut Nowhere);
        assert_eq!(tab.arguments().unwrap().1.choices, ["privileged"]);
        tab.key(key(Code::Esc), &list, &mut Nowhere);
        assert!(!tab.typing());
        // A click on a row selects it.
        let rows = text(&tab.shown(&list, "here"));
        let at = rows.iter().position(|row| row == "  ▸ Images").unwrap();
        tab.select_row(at);
        assert_eq!(tab.selected_row(), at);
        tab.select_row(0);
        assert_eq!(tab.selected_row(), at);
        // Asking again forgets what it said.
        assert_eq!(
            tab.key(key(Code::Char('r')), &list, &mut Nowhere),
            Asked::Open("docker/container".into())
        );
        assert!(text(&tab.shown(&list, "here"))[0].starts_with("Asking"));
    }

    #[test]
    fn an_action_that_cannot_run_says_why_and_a_failure_is_shown() {
        let list = listed();
        let mut tab = Tab::default();
        tab.key(key(Code::Enter), &list, &mut Nowhere);
        let mut blocked = actions();
        blocked[0].available = false;
        blocked[0].why = "needs the secret api_token".into();
        tab.opened("docker/container", Err("engine gone".into()), Ok(blocked));
        let shown = tab.shown(&list, "here");
        assert_eq!(text(&shown)[0], "✗ engine gone");
        let Asked::Nothing(Some(said)) = tab.key(key(Code::Enter), &list, &mut Nowhere) else {
            panic!("it ran");
        };
        assert!(said.contains("needs the secret api_token"));
    }

    #[test]
    fn a_resource_has_a_page_with_its_actions_and_its_log() {
        let list = listed();
        let mut tab = Tab::default();
        tab.key(key(Code::Enter), &list, &mut Nowhere);
        let state = ProvisionerState {
            provisioner: ProvisionerInfo {
                kinds: vec![berth_api::ProvisionerKind {
                    id: "deployment".into(),
                    label: "Compose project".into(),
                    ..Default::default()
                }],
                ..list[0].clone()
            },
            state: "ready".into(),
            resources: vec![
                ListedResource {
                    id: "wiki".into(),
                    kind: "deployment".into(),
                    name: "wiki".into(),
                    state: "running".into(),
                    resource: "r-wiki".into(),
                    ..ListedResource::default()
                },
                ListedResource {
                    id: "stray".into(),
                    kind: "deployment".into(),
                    name: "stray".into(),
                    ..ListedResource::default()
                },
            ],
            ..ProvisionerState::default()
        };
        tab.opened("docker/container", Ok(state), Ok(vec![]));
        let rows = text(&tab.shown(&list, "here"));
        assert!(
            rows.contains(&"Compose project berth made with it".to_owned()),
            "{rows:?}"
        );
        assert!(rows.iter().any(|row| row.contains("no record of it")));
        // One without a record has no page.
        tab.key(key(Code::Down), &list, &mut Nowhere);
        assert!(matches!(
            tab.key(key(Code::Enter), &list, &mut Nowhere),
            Asked::Nothing(Some(_))
        ));
        tab.key(key(Code::Up), &list, &mut Nowhere);
        assert_eq!(
            tab.key(key(Code::Enter), &list, &mut Nowhere),
            Asked::OpenResource("r-wiki".into())
        );
        let seen = Resource {
            id: "r-wiki".into(),
            provisioner: "docker/container".into(),
            kind: "deployment".into(),
            name: "wiki".into(),
            state: "running".into(),
            external_id: "wiki".into(),
            verbs: vec!["logs".into()],
            ..Resource::default()
        };
        let remove = Action {
            name: "remove".into(),
            label: "Remove the deployment".into(),
            group: "Resource".into(),
            confirm: Some("Remove the deployment wiki with what it holds?".into()),
            ..Action::default()
        };
        tab.opened_resource("r-wiki", Ok(seen), Ok(vec![remove]));
        let shown = tab.shown(&list, "here");
        let rows = text(&shown);
        assert_eq!(rows[0], "wiki · deployment of docker/container");
        assert!(rows.contains(&"  ▸ Show its log".to_owned()), "{rows:?}");
        assert!(shown.help.contains("berth resource run r-wiki remove"));
        // Removing asks first, and then runs as the resource's.
        tab.key(key(Code::Enter), &list, &mut Nowhere);
        assert_eq!(
            tab.question(),
            Some("Remove the deployment wiki with what it holds?")
        );
        let Asked::RunResource {
            resource, action, ..
        } = tab.key(key(Code::Char('y')), &list, &mut Nowhere)
        else {
            panic!("not run");
        };
        assert_eq!(
            (resource.as_str(), action.name.as_str()),
            ("r-wiki", "remove")
        );
        tab.key(key(Code::Down), &list, &mut Nowhere);
        assert_eq!(
            tab.key(key(Code::Enter), &list, &mut Nowhere),
            Asked::Logs("r-wiki".into())
        );
        assert_eq!(
            tab.key(key(Code::Char('r')), &list, &mut Nowhere),
            Asked::OpenResource("r-wiki".into())
        );
        // Back to the provisioner's page, which is asked again.
        assert_eq!(
            tab.key(key(Code::Esc), &list, &mut Nowhere),
            Asked::Open("docker/container".into())
        );
        assert!(
            tab.page
                .as_ref()
                .is_some_and(|page| page.resource.is_none())
        );
    }

    // ── Collections ─────────────────────────────────────────────────────────

    fn argument(name: &str) -> Argument {
        Argument {
            name: name.into(),
            label: name.into(),
            ..Argument::default()
        }
    }

    fn act(name: &str, label: &str, collection: &str, arguments: &[&str]) -> Action {
        Action {
            name: name.into(),
            label: label.into(),
            group: "Storage".into(),
            collection: collection.into(),
            arguments: arguments.iter().map(|name| argument(name)).collect(),
            ..Action::default()
        }
    }

    fn used(action: &str, label: &str, arguments: &[(&str, &str)]) -> CollectionAction {
        CollectionAction {
            action: action.into(),
            label: label.into(),
            arguments: arguments
                .iter()
                .map(|(part, argument)| (part.to_string(), argument.to_string()))
                .collect(),
        }
    }

    fn row(name: &str, key: &[(&str, &str)], columns: &[(&str, &str)]) -> CollectionItem {
        let pairs = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect()
        };
        CollectionItem {
            name: name.into(),
            key: pairs(key),
            columns: pairs(columns),
            unavailable: BTreeMap::new(),
        }
    }

    fn column(name: &str, label: &str) -> berth_api::CollectionColumn {
        berth_api::CollectionColumn {
            name: name.into(),
            label: label.into(),
        }
    }

    /// The actions of a made-up Incus server, those of its collections among them.
    fn server_actions() -> Vec<Action> {
        let mut remove = act("remove-pool", "Remove storage pool", "pools", &["name"]);
        remove.confirm = Some("Remove this storage pool?".into());
        vec![
            Action {
                changes: false,
                ..act("server", "Check the server", "", &[])
            },
            Action {
                changes: false,
                ..act("pools", "Storage pools", "pools", &[])
            },
            act(
                "create-pool",
                "New storage pool",
                "pools",
                &["name", "driver"],
            ),
            act(
                "resize-pool",
                "Grow storage pool",
                "pools",
                &["pool", "size"],
            ),
            act(
                "set-pool",
                "Change storage pool",
                "pools",
                &["pool", "key", "value"],
            ),
            remove,
            act(
                "create-volume",
                "New volume",
                "volumes",
                &["pool", "name", "size"],
            ),
            act(
                "remove-volume",
                "Remove volume",
                "volumes",
                &["pool", "name"],
            ),
            act(
                "resize-disk",
                "Grow root disk",
                "instances",
                &["instance", "size"],
            ),
        ]
    }

    fn server_collections() -> Vec<Collection> {
        vec![
            Collection {
                id: "pools".into(),
                label: "Storage pool".into(),
                plural: "Storage pools".into(),
                group: "Storage".into(),
                key: vec!["pool".into()],
                columns: vec![column("driver", "Driver"), column("size", "Size")],
                new: vec![used("create-pool", "", &[])],
                item: vec![
                    used("resize-pool", "Grow", &[]),
                    used("set-pool", "Change a setting", &[]),
                    used("remove-pool", "Remove", &[("pool", "name")]),
                ],
                remove: "remove-pool".into(),
                show: "pools".into(),
                ..Collection::default()
            },
            Collection {
                id: "volumes".into(),
                label: "Volume".into(),
                plural: "Volumes".into(),
                group: "Storage".into(),
                key: vec!["pool".into(), "volume".into()],
                within: "pools".into(),
                columns: vec![column("pool", "Pool"), column("size", "Size")],
                new: vec![used("create-volume", "", &[])],
                item: vec![used("remove-volume", "Remove", &[("volume", "name")])],
                remove: "remove-volume".into(),
                ..Collection::default()
            },
            Collection {
                id: "instances".into(),
                label: "Instance".into(),
                plural: "Instances".into(),
                key: vec!["instance".into()],
                kinds: vec!["machine".into()],
                item: vec![used("resize-disk", "", &[])],
                ..Collection::default()
            },
        ]
    }

    fn server_state(pools: &[&str]) -> ProvisionerState {
        let mut fast = row(
            "fast",
            &[("pool", "fast")],
            &[("driver", "dir"), ("size", "200.0 GiB")],
        );
        fast.unavailable.insert(
            "resize-pool".into(),
            "A dir pool has no size of its own".into(),
        );
        let all = vec![
            row(
                "default",
                &[("pool", "default")],
                &[("driver", "zfs"), ("size", "100.0 GiB")],
            ),
            fast,
        ];
        ProvisionerState {
            provisioner: ProvisionerInfo {
                id: "incus-vm".into(),
                label: "Incus virtual machine".into(),
                collections: server_collections(),
                ..ProvisionerInfo::default()
            },
            state: "ready".into(),
            collections: [
                (
                    "pools".to_owned(),
                    all.into_iter()
                        .filter(|row| pools.contains(&row.name.as_str()))
                        .collect(),
                ),
                (
                    "volumes".to_owned(),
                    vec![
                        row(
                            "data",
                            &[("pool", "default"), ("volume", "data")],
                            &[("pool", "default"), ("size", "10GiB")],
                        ),
                        row(
                            "scratch",
                            &[("pool", "fast"), ("volume", "scratch")],
                            &[("pool", "fast"), ("size", "no set size")],
                        ),
                    ],
                ),
            ]
            .into(),
            resources: vec![ListedResource {
                id: "berth-studio".into(),
                kind: "machine".into(),
                name: "studio".into(),
                host: "studio".into(),
                state: "running".into(),
                resource: "r-studio".into(),
            }],
            ..ProvisionerState::default()
        }
    }

    /// A tab on the page of the made-up server, as it answered.
    fn on_server() -> (Tab, Vec<ProvisionerInfo>) {
        let list = vec![ProvisionerInfo {
            id: "incus-vm".into(),
            ..ProvisionerInfo::default()
        }];
        let mut tab = Tab::default();
        assert_eq!(
            tab.key(key(Code::Enter), &list, &mut Nowhere),
            Asked::Open("incus-vm".into())
        );
        tab.opened(
            "incus-vm",
            Ok(server_state(&["default", "fast"])),
            Ok(server_actions()),
        );
        (tab, list)
    }

    /// Select the row that reads `line`.
    fn choose(tab: &mut Tab, list: &[ProvisionerInfo], line: &str) {
        let rows = text(&tab.shown(list, "here"));
        let at = rows
            .iter()
            .position(|row| row.trim_end() == line)
            .unwrap_or_else(|| panic!("{line:?} in {rows:?}"));
        tab.select_row(at);
        assert_eq!(tab.selected_row(), at, "{line:?} can be chosen");
    }

    fn typed(tab: &mut Tab, list: &[ProvisionerInfo], words: &str) {
        for ch in words.chars() {
            tab.key(key(Code::Char(ch)), list, &mut Nowhere);
        }
    }

    #[test]
    fn a_collection_is_one_row_with_its_count_and_its_actions_are_not_loose() {
        let (tab, list) = on_server();
        let shown = tab.shown(&list, "here");
        let rows = text(&shown);
        let storage = rows.iter().position(|row| row == "Storage").unwrap();
        assert_eq!(
            rows[storage + 1..storage + 4],
            [
                "  ▸ Storage pools  2",
                "  ▸ Volumes        2",
                "  ▸ Check the server"
            ]
        );
        // Neither the actions of a collection nor the collection of resources is a row.
        for gone in [
            "Grow",
            "New storage pool",
            "Grow root disk",
            "Instances",
            "▸ Storage pools  ",
        ] {
            assert!(
                !rows
                    .iter()
                    .any(|row| row.contains(gone) && !row.ends_with("pools  2")),
                "{gone:?} in {rows:?}"
            );
        }
        assert_eq!(rows[shown.selected], "  ▸ Storage pools  2");
        assert!(
            shown.help.contains("x removes the one selected"),
            "{}",
            shown.help
        );
        assert!(shown.help.contains("berth provisioner run incus-vm pools"));
        assert!(shown.keys.contains("Enter open"));
    }

    #[test]
    fn a_table_opens_an_item_whose_actions_have_its_key_and_say_why_not() {
        let (mut tab, list) = on_server();
        choose(&mut tab, &list, "  ▸ Storage pools  2");
        assert_eq!(
            tab.key(key(Code::Enter), &list, &mut Nowhere),
            Asked::Nothing(None)
        );
        let shown = tab.shown(&list, "here");
        let rows = text(&shown);
        assert_eq!(rows[0], "Storage pools · incus-vm");
        for line in [
            "  Storage pool  Driver  Size",
            "  default       zfs     100.0 GiB",
            "  fast          dir     200.0 GiB",
            "New",
            "  ▸ New storage pool",
        ] {
            assert!(rows.iter().any(|row| row == line), "{line:?} in {rows:?}");
        }
        // The first item is selected, and Enter opens it.
        assert_eq!(rows[shown.selected], "  default       zfs     100.0 GiB");
        assert!(shown.keys.contains("n new") && shown.keys.contains("x remove"));
        tab.key(key(Code::Down), &list, &mut Nowhere);
        tab.key(key(Code::Enter), &list, &mut Nowhere);
        let shown = tab.shown(&list, "here");
        let rows = text(&shown);
        assert_eq!(rows[0], "Storage pool fast · incus-vm");
        assert!(rows.contains(&"  Driver  dir".to_owned()), "{rows:?}");
        assert!(
            rows.contains(&"  ▸ Grow · A dir pool has no size of its own".to_owned()),
            "{rows:?}"
        );
        // What cannot run on it is greyed, and says why when it is chosen.
        let grow = rows
            .iter()
            .position(|row| row.starts_with("  ▸ Grow"))
            .unwrap();
        assert_eq!(shown.rows[grow].style.fg, Some(theme::MUTED));
        assert_eq!(rows[shown.selected], rows[grow]);
        assert_eq!(
            tab.key(key(Code::Enter), &list, &mut Nowhere),
            Asked::Nothing(Some(
                "Grow cannot run now: A dir pool has no size of its own".into()
            ))
        );
        // Its other action asks only for what the pool does not fill.
        tab.key(key(Code::Down), &list, &mut Nowhere);
        let help = tab.shown(&list, "here").help;
        assert!(
            help.contains("berth provisioner run incus-vm set-pool pool=fast"),
            "{help}"
        );
        tab.key(key(Code::Enter), &list, &mut Nowhere);
        let (actions, dialog) = tab.arguments().unwrap();
        assert_eq!(actions[dialog.action].arguments[dialog.index].name, "key");
        typed(&mut tab, &list, "description");
        tab.key(key(Code::Enter), &list, &mut Nowhere);
        typed(&mut tab, &list, "fast disks");
        let Asked::Run {
            provisioner,
            action,
            values,
        } = tab.key(key(Code::Enter), &list, &mut Nowhere)
        else {
            panic!("not run");
        };
        assert_eq!(
            (provisioner.as_str(), action.name.as_str(), action.title()),
            ("incus-vm", "set-pool", "Change storage pool fast")
        );
        assert_eq!(
            values,
            [
                ("key".to_owned(), "description".to_owned()),
                ("pool".to_owned(), "fast".to_owned()),
                ("value".to_owned(), "fast disks".to_owned())
            ]
            .into()
        );
        // Esc goes back to the table, with its row selected, and then to the page.
        tab.key(key(Code::Esc), &list, &mut Nowhere);
        let shown = tab.shown(&list, "here");
        assert_eq!(
            text(&shown)[shown.selected],
            "  fast          dir     200.0 GiB"
        );
        tab.key(key(Code::Esc), &list, &mut Nowhere);
        let shown = tab.shown(&list, "here");
        assert_eq!(text(&shown)[shown.selected], "  ▸ Storage pools  2");
    }

    #[test]
    fn x_removes_the_item_selected_after_a_question_that_names_it() {
        let (mut tab, list) = on_server();
        choose(&mut tab, &list, "  ▸ Storage pools  2");
        tab.key(key(Code::Enter), &list, &mut Nowhere);
        tab.key(key(Code::Down), &list, &mut Nowhere);
        assert_eq!(
            tab.key(key(Code::Char('x')), &list, &mut Nowhere),
            Asked::Nothing(None)
        );
        assert_eq!(
            tab.question(),
            Some("Storage pool fast: Remove this storage pool?")
        );
        let Asked::Run { action, values, .. } = tab.key(key(Code::Char('y')), &list, &mut Nowhere)
        else {
            panic!("not run");
        };
        assert_eq!(action.name, "remove-pool");
        assert_eq!(values, [("name".to_owned(), "fast".to_owned())].into());
        // After its job the provisioner is asked again: the table has one fewer.
        tab.opened(
            "incus-vm",
            Ok(server_state(&["default"])),
            Ok(server_actions()),
        );
        let rows = text(&tab.shown(&list, "here"));
        assert!(
            !rows.iter().any(|row| row.starts_with("  fast")),
            "{rows:?}"
        );
        // On a row that is no item, x says what to choose.
        choose(&mut tab, &list, "  ▸ New storage pool");
        assert_eq!(
            tab.key(key(Code::Char('x')), &list, &mut Nowhere),
            Asked::Nothing(Some("Choose the storage pool to remove".into()))
        );
    }

    #[test]
    fn the_page_of_an_item_that_is_gone_goes_back_to_its_table_when_asked_again() {
        let (mut tab, list) = on_server();
        choose(&mut tab, &list, "  ▸ Storage pools  2");
        tab.key(key(Code::Enter), &list, &mut Nowhere);
        tab.key(key(Code::Down), &list, &mut Nowhere);
        tab.key(key(Code::Enter), &list, &mut Nowhere);
        // r asks the provisioner again and keeps what is shown.
        assert_eq!(
            tab.key(key(Code::Char('r')), &list, &mut Nowhere),
            Asked::Open("incus-vm".into())
        );
        assert_eq!(text(&tab.shown(&list, "here"))[1], "Asking incus-vm…");
        tab.opened(
            "incus-vm",
            Ok(server_state(&["default"])),
            Ok(server_actions()),
        );
        let shown = tab.shown(&list, "here");
        let rows = text(&shown);
        assert_eq!(rows[0], "Storage pools · incus-vm");
        assert_eq!(rows[shown.selected], "  default       zfs     100.0 GiB");
    }

    #[test]
    fn the_volumes_of_a_pool_are_listed_on_its_page_and_made_there_without_asking_for_it() {
        let (mut tab, list) = on_server();
        choose(&mut tab, &list, "  ▸ Storage pools  2");
        tab.key(key(Code::Enter), &list, &mut Nowhere);
        tab.key(key(Code::Enter), &list, &mut Nowhere);
        let rows = text(&tab.shown(&list, "here"));
        assert_eq!(rows[0], "Storage pool default · incus-vm");
        let at = rows
            .iter()
            .position(|row| row == "In this storage pool")
            .unwrap();
        assert_eq!(rows[at + 1], "  ▸ Volumes  1");
        tab.select_row(at + 1);
        tab.key(key(Code::Enter), &list, &mut Nowhere);
        let rows = text(&tab.shown(&list, "here"));
        assert_eq!(rows[0], "Volumes · incus-vm · in the storage pool default");
        // The pool is the table's own, so it is no column, and the other pool's volume is not there.
        assert!(rows.contains(&"  Volume  Size".to_owned()), "{rows:?}");
        assert!(rows.contains(&"  data    10GiB".to_owned()), "{rows:?}");
        assert!(!rows.iter().any(|row| row.contains("scratch")));
        // n asks for the name and size, not the pool.
        tab.key(key(Code::Char('n')), &list, &mut Nowhere);
        let (actions, dialog) = tab.arguments().unwrap();
        assert_eq!(actions[dialog.action].arguments[dialog.index].name, "name");
        typed(&mut tab, &list, "logs");
        tab.key(key(Code::Enter), &list, &mut Nowhere);
        let Asked::Run { action, values, .. } = tab.key(key(Code::Enter), &list, &mut Nowhere)
        else {
            panic!("not run");
        };
        assert_eq!(action.name, "create-volume");
        assert_eq!(
            (values["pool"].as_str(), values["name"].as_str()),
            ("default", "logs")
        );
        // A volume's page removes it with both parts of its key.
        tab.key(key(Code::Enter), &list, &mut Nowhere);
        assert_eq!(text(&tab.shown(&list, "here"))[0], "Volume data · incus-vm");
        tab.key(key(Code::Char('x')), &list, &mut Nowhere);
        assert_eq!(tab.question(), Some("Volume data: Remove volume?"));
        let Asked::Run { values, .. } = tab.key(key(Code::Char('y')), &list, &mut Nowhere) else {
            panic!("not run");
        };
        assert_eq!(
            values,
            [
                ("name".to_owned(), "data".to_owned()),
                ("pool".to_owned(), "default".to_owned())
            ]
            .into()
        );
    }

    #[test]
    fn all_volumes_are_a_table_too_and_a_new_one_there_asks_for_its_pool() {
        let (mut tab, list) = on_server();
        choose(&mut tab, &list, "  ▸ Volumes        2");
        tab.key(key(Code::Enter), &list, &mut Nowhere);
        let rows = text(&tab.shown(&list, "here"));
        assert_eq!(rows[0], "Volumes · incus-vm");
        for line in [
            "  Volume   Pool     Size",
            "  data     default  10GiB",
            "  scratch  fast     no set size",
        ] {
            assert!(rows.contains(&line.to_owned()), "{line:?} in {rows:?}");
        }
        // With the pool to ask for too it takes a form, which starts with the pool.
        tab.key(key(Code::Char('n')), &list, &mut Nowhere);
        let form = tab.form().expect("a form");
        assert_eq!(form.wanted(), "create-volume");
        assert!(tab.arguments().is_none());
    }

    #[test]
    fn n_on_a_table_with_several_ways_to_make_one_selects_them() {
        let (mut tab, list) = on_server();
        let mut actions = server_actions();
        actions.push(act("build", "Build the image", "pools", &[]));
        let mut state = server_state(&["default"]);
        state.provisioner.collections[0]
            .new
            .push(used("build", "Build one", &[]));
        state.provisioner.collections[0].remove.clear();
        tab.opened("incus-vm", Ok(state), Ok(actions));
        choose(&mut tab, &list, "  ▸ Storage pools  1");
        tab.key(key(Code::Enter), &list, &mut Nowhere);
        assert_eq!(
            tab.key(key(Code::Char('n')), &list, &mut Nowhere),
            Asked::Nothing(Some(
                "Choose how to make one: New storage pool or Build one".into()
            ))
        );
        let shown = tab.shown(&list, "here");
        assert_eq!(text(&shown)[shown.selected], "  ▸ New storage pool");
        assert!(shown.keys.contains("n new") && !shown.keys.contains("x remove"));
        tab.key(key(Code::Down), &list, &mut Nowhere);
        let Asked::Run { action, .. } = tab.key(key(Code::Enter), &list, &mut Nowhere) else {
            panic!("not run");
        };
        assert_eq!(action.name, "build");
        tab.key(key(Code::Up), &list, &mut Nowhere);
        tab.key(key(Code::Up), &list, &mut Nowhere);
        assert_eq!(
            tab.key(key(Code::Char('x')), &list, &mut Nowhere),
            Asked::Nothing(Some("Storage pools are not removed here".into()))
        );
    }

    #[test]
    fn a_machine_has_the_provisioners_actions_on_it_on_its_page() {
        let (mut tab, list) = on_server();
        choose(
            &mut tab,
            &list,
            "  studio             berth-studio  running",
        );
        assert_eq!(
            tab.key(key(Code::Enter), &list, &mut Nowhere),
            Asked::OpenResource("r-studio".into())
        );
        let seen = Resource {
            id: "r-studio".into(),
            provisioner: "incus-vm".into(),
            kind: "machine".into(),
            name: "studio".into(),
            state: "running".into(),
            ..Resource::default()
        };
        tab.opened_resource("r-studio", Ok(seen), Ok(vec![]));
        let rows = text(&tab.shown(&list, "here"));
        assert!(rows.contains(&"  ▸ Grow root disk".to_owned()), "{rows:?}");
        choose(&mut tab, &list, "  ▸ Grow root disk");
        assert!(
            tab.shown(&list, "here")
                .help
                .contains("berth provisioner run incus-vm resize-disk instance=berth-studio")
        );
        tab.key(key(Code::Enter), &list, &mut Nowhere);
        let (actions, dialog) = tab.arguments().unwrap();
        assert_eq!(actions[dialog.action].arguments[dialog.index].name, "size");
        typed(&mut tab, &list, "80GiB");
        let Asked::Run {
            provisioner,
            action,
            values,
        } = tab.key(key(Code::Enter), &list, &mut Nowhere)
        else {
            panic!("not run");
        };
        assert_eq!(
            (provisioner.as_str(), action.title()),
            ("incus-vm", "Grow root disk · studio")
        );
        assert_eq!(values["instance"], "berth-studio");
        // The provisioner's answer after its job is kept for its page.
        tab.opened(
            "incus-vm",
            Ok(server_state(&["default"])),
            Ok(server_actions()),
        );
        assert_eq!(
            tab.key(key(Code::Esc), &list, &mut Nowhere),
            Asked::Open("incus-vm".into())
        );
        assert!(text(&tab.shown(&list, "here")).contains(&"  ▸ Storage pools  1".to_owned()));
    }
}
