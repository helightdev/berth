//! The Provisioners tab of the home screen: the provisioners there are, each
//! with whether it can be used here, and the page of one, built with
//! [`crate::page`]: how it is now, what it says of itself, what it holds by
//! kind, and its actions. A resource berth made with it has a page of its own
//! with its state, its actions and its log. berth shows those of this machine or of its
//! berth server, the page in a browser those of the server it came from. What
//! the tab asks for, each of them does its own way, and tells the tab what
//! came back.

use crate::event::{Code, Event, Key};
use crate::page::{self, Accepted, Arguments, Begin, Group, Item, Tone};
use crate::screen::{self, Platform};
use crate::theme;
use berth_api::{Action, ProvisionerInfo, ProvisionerState, Resource};
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
    /// A key the tab does not take, such as one that changes tabs.
    Other,
}

/// A row of a provisioner's page that can be chosen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Entry {
    Action(usize),
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

/// The page of one provisioner, or of one resource it made, as it was last
/// heard of.
pub struct Page {
    /// The provisioner's `PLUGIN/ID`, or the resource's id.
    pub id: String,
    /// Set on a resource's page.
    pub resource: Option<ResourcePart>,
    /// None while it is asked.
    pub state: Option<Result<ProvisionerState, String>>,
    pub actions: Vec<Action>,
    /// The page it was opened from, which Esc goes back to.
    back: Option<Box<Page>>,
    cursor: usize,
    /// The question of the action chosen, to be answered first.
    question: Option<(String, usize)>,
    arguments: Option<Arguments>,
    /// What the last action that answered at once said.
    said: Vec<String>,
}

#[derive(Default)]
pub struct Tab {
    pub selected: usize,
    pub page: Option<Page>,
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

/// The rows of the list: one for each provisioner.
pub fn rows(provisioners: &[ProvisionerInfo]) -> Vec<Line<'static>> {
    let muted = Style::new().fg(theme::MUTED);
    if provisioners.is_empty() {
        return vec![Line::styled("No plugin brings a provisioner", muted)];
    }
    provisioners
        .iter()
        .map(|provisioner| {
            let (said, colour) = usable(provisioner);
            Line::from(vec![
                Span::styled(
                    format!("{:<28}", provisioner.label),
                    Style::new().fg(theme::TEXT),
                ),
                Span::styled(format!("{:<20}", provisioner.id), muted),
                Span::styled(said, Style::new().fg(colour)),
            ])
        })
        .collect()
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
            "A plugin brings provisioners, which create machines for hosts and may have actions of their own. None of the plugins on {place} does. See the Plugins tab."
        );
    };
    let mut text = format!("{}\n\n", provisioner.description);
    if !provisioner.problem.is_empty() {
        text += &format!("It cannot be used here: {}\n\n", provisioner.problem);
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
    if !provisioner.actions.is_empty() {
        let names: Vec<&str> = provisioner.actions.iter().map(Action::title).collect();
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
        "Enter opens its page, which asks it how it is.\n\nThe provisioners of {place}. From the command line: `berth provisioner {}`",
        provisioner.id
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
            cursor: 0,
            question: None,
            arguments: None,
            said: vec![],
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
        lines.extend(self.answer_lines());
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
            (format!("{} · {}", info.label, info.id), Tone::Plain),
            (said, tone),
        ];
        // The settings row is one of them, in the same columns.
        let settings = (!info.own.is_empty()).then_some("Settings".len());
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
        lines.extend(self.answer_lines());
        lines
    }

    /// What the last action that answered at once said, set apart from
    /// how the subject is, as its details may be the same kind of rows.
    fn answer_lines(&self) -> Vec<(String, Tone)> {
        if self.said.is_empty() {
            return vec![];
        }
        let mut lines = vec![
            (String::new(), Tone::Plain),
            ("Answer".to_owned(), Tone::Muted),
        ];
        lines.extend(
            self.said
                .iter()
                .map(|line| (format!("  {line}"), Tone::Plain)),
        );
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
    /// provisioner's page what it holds, by kind.
    fn items(&self) -> Vec<Item<Entry>> {
        let mut groups = page::groups(&self.actions, Entry::Action);
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

    fn launch(&mut self, (index, values): page::Launch) -> Asked {
        match self.actions.get(index) {
            Some(action) if !action.available => Asked::Nothing(Some(format!(
                "{} cannot run now: {}",
                action.title(),
                action.why
            ))),
            Some(action) if self.resource.is_some() => Asked::RunResource {
                resource: self.id.clone(),
                action: Box::new(action.clone()),
                values,
            },
            Some(action) => Asked::Run {
                provisioner: self.id.clone(),
                action: Box::new(action.clone()),
                values,
            },
            None => Asked::Nothing(None),
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

impl Tab {
    /// The selected provisioner of the list.
    pub fn selected<'a>(&mut self, list: &'a [ProvisionerInfo]) -> Option<&'a ProvisionerInfo> {
        self.selected = self.selected.min(list.len().saturating_sub(1));
        list.get(self.selected)
    }

    /// Leave a dialog, on changing tabs.
    pub fn leave(&mut self) {
        if let Some(page) = &mut self.page {
            page.question = None;
            page.arguments = None;
        }
    }

    /// Put pasted text into the argument being asked for, if one is.
    pub fn paste(&mut self, text: &str) {
        if let Some(dialog) = self.page.as_mut().and_then(|page| page.arguments.as_mut()) {
            crate::event::paste(&mut dialog.input, text);
        }
    }

    /// Whether keys go to a dialog, so none of them changes tabs.
    pub fn typing(&self) -> bool {
        self.page
            .as_ref()
            .is_some_and(|page| page.question.is_some() || page.arguments.is_some())
    }

    /// Take in what a provisioner said for its page.
    pub fn opened(
        &mut self,
        id: &str,
        state: Result<ProvisionerState, String>,
        actions: Result<Vec<Action>, String>,
    ) {
        let Some(page) = self.page.as_mut().filter(|page| page.id == id) else {
            return;
        };
        page.state = Some(match (state, &actions) {
            (Ok(state), Err(error)) => Err(format!(
                "{} answered, but not with its actions: {error}",
                state.provisioner.id
            )),
            (state, _) => state,
        });
        page.actions = actions.unwrap_or_default();
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
        page.actions = actions.unwrap_or_default();
    }

    /// Take in what an action that answers at once said, of a provisioner
    /// or a resource by its id.
    pub fn answered(&mut self, id: &str, lines: Vec<String>) {
        if let Some(page) = self.page.as_mut().filter(|page| page.id == id) {
            page.said = lines;
        }
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
            let selected = self.selected.min(list.len().saturating_sub(1));
            return Shown {
                rows: rows(list),
                selected,
                help: help(list.get(selected), place),
                keys: "↑↓ select · Enter open · r refresh · Esc hosts",
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
                Item::Pick(text, _) => Line::styled(text.clone(), Style::new().fg(theme::TEXT)),
            })
            .collect();
        let help = match page.selected() {
            Some(Entry::Action(index)) => page::help(
                &page.actions[index],
                &match &page.resource {
                    Some(_) => format!(
                        "berth resource {} run {}",
                        page.id, page.actions[index].name
                    ),
                    None => format!(
                        "berth provisioner {} run {}",
                        page.id, page.actions[index].name
                    ),
                },
            ),
            Some(Entry::Resource(index)) => {
                let held = &page.held()[index];
                match (held.resource.is_empty(), held.kind.as_str()) {
                    (false, "machine") if !held.host.is_empty() => format!(
                        "The machine of {}, {} as the provisioner calls it. Enter opens its page, with its state, its actions and its log. It is looked after as that host too, on the host's page. From the command line: `berth resource {}`",
                        held.host, held.id, held.resource
                    ),
                    (false, _) => format!(
                        "The {} {}, {} as the provisioner calls it. Enter opens its page, with its state, its actions and its log. From the command line: `berth resource {}`",
                        held.kind, held.name, held.id, held.resource
                    ),
                    (true, _) => format!(
                        "{} is listed by the provisioner, but berth has no record of it: it was made otherwise, or berth lost track of it. berth does not remove it; the provisioner's own tools do.",
                        held.id
                    ),
                }
            }
            Some(Entry::Logs) => format!(
                "Its log as its provisioner gives it, the last lines and then what follows, with secrets masked. From the command line: `berth resource {} logs --follow`",
                page.id
            ),
            None if page.resource.is_some() => format!(
                "Asking about the resource. From the command line: `berth resource {}`",
                page.id
            ),
            None => format!(
                "Asking the provisioner how it is. From the command line: `berth provisioner {}`",
                page.id
            ),
        };
        Shown {
            rows,
            selected: self.selected_row(),
            help,
            keys: "↑↓ select · Enter run · r ask again · Esc back",
        }
    }

    /// A key on the tab. `platform` copies what is typed in a dialog.
    pub fn key(
        &mut self,
        key: Key,
        list: &[ProvisionerInfo],
        platform: &mut dyn Platform,
    ) -> Asked {
        let Some(page) = &mut self.page else {
            let count = list.len();
            match key.code {
                Code::Up | Code::Char('k') if count > 0 => {
                    self.selected = (self.selected + count - 1) % count
                }
                Code::Down | Code::Char('j') if count > 0 => {
                    self.selected = (self.selected + 1) % count
                }
                Code::Enter => {
                    if let Some(provisioner) = self.selected(list) {
                        let id = provisioner.id.clone();
                        self.page = Some(Page::new(&id));
                        return Asked::Open(id);
                    }
                }
                Code::Char('r') => return Asked::List,
                _ => return Asked::Other,
            }
            return Asked::Nothing(None);
        };
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
                    page.said.clear();
                    return page.begin(index, false);
                }
                Some(Entry::Resource(index)) => {
                    let held = page.held()[index].clone();
                    if held.resource.is_empty() {
                        return Asked::Nothing(Some(format!("berth has no record of {}", held.id)));
                    }
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
                        ..Page::new(&held.resource)
                    });
                    return Asked::OpenResource(held.resource);
                }
                Some(Entry::Logs) => return Asked::Logs(page.id.clone()),
                None => {}
            },
            Code::Char('r') => {
                let id = page.id.clone();
                page.state = None;
                page.said.clear();
                if let Some(part) = &mut page.resource {
                    part.seen = None;
                    return Asked::OpenResource(id);
                }
                return Asked::Open(id);
            }
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
                .contains("berth provisioner docker/container run images")
        );
        let Asked::Run { action, values, .. } = tab.key(key(Code::Enter), &list, &mut Nowhere)
        else {
            panic!("not run");
        };
        assert_eq!((action.name.as_str(), values.len()), ("images", 0));
        tab.answered("docker/container", vec!["2 images".into()]);
        // What it said is set apart from how the provisioner is.
        let shown_text = text(&tab.shown(&list, "here"));
        let at = shown_text.iter().position(|row| row == "Answer").unwrap();
        assert_eq!(shown_text[at + 1], "  2 images");
        // The next asks first, then offers what its argument takes.
        tab.key(key(Code::Down), &list, &mut Nowhere);
        assert_eq!(
            tab.key(key(Code::Enter), &list, &mut Nowhere),
            Asked::Nothing(None)
        );
        assert_eq!(tab.question(), Some("Prune?"));
        assert!(tab.typing());
        assert_eq!(
            tab.key(key(Code::Char('n')), &list, &mut Nowhere),
            Asked::Nothing(Some("Not run".into()))
        );
        tab.key(key(Code::Enter), &list, &mut Nowhere);
        tab.key(key(Code::Char('y')), &list, &mut Nowhere);
        let (_, dialog) = tab.arguments().unwrap();
        assert_eq!(dialog.choices, ["no", "yes"]);
        tab.key(key(Code::Down), &list, &mut Nowhere);
        tab.key(key(Code::Down), &list, &mut Nowhere);
        let Asked::Run { action, values, .. } = tab.key(key(Code::Enter), &list, &mut Nowhere)
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
        assert!(shown.help.contains("berth resource r-wiki run remove"));
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
}
