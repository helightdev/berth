//! The Provisioners tab of the home screen: the provisioners there are, each
//! with whether it can be used here, and the page of one, built with
//! [`crate::page`]: how it is now, what it says of itself, the machines berth
//! made with it, and its actions. berth shows those of this machine or of its
//! berth server, the page in a browser those of the server it came from. What
//! the tab asks for, each of them does its own way, and tells the tab what
//! came back.

use crate::event::{Code, Event, Key};
use crate::page::{self, Accepted, Arguments, Begin, Group, Item, Tone};
use crate::screen::{self, Platform};
use crate::theme;
use berth_api::{Action, ProvisionerInfo, ProvisionerState};
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
    /// A key the tab does not take, such as one that changes tabs.
    Other,
}

/// A row of a provisioner's page that can be chosen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Entry {
    Action(usize),
    Machine(usize),
}

/// The page of one provisioner, as it was last heard of.
pub struct Page {
    pub id: String,
    /// None while it is asked.
    pub state: Option<Result<ProvisionerState, String>>,
    pub actions: Vec<Action>,
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
    let kinds: Vec<&str> = provisioner
        .kinds
        .iter()
        .map(|kind| match kind.id.as_str() {
            "machine" => "machines for hosts",
            _ => kind.label.as_str(),
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
        text += &format!("{}.\n\n", set.join(", "));
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
            state: None,
            actions: vec![],
            cursor: 0,
            question: None,
            arguments: None,
            said: vec![],
        }
    }

    /// The lines that say how it is.
    fn state_lines(&self) -> Vec<(String, Tone)> {
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
        let width = state
            .details
            .iter()
            .map(|row| row.label.chars().count())
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
            lines.push((format!("  Settings  {}", set.join(" · ")), Tone::Muted));
        }
        lines.extend(self.said.iter().map(|line| (line.clone(), Tone::Plain)));
        lines
    }

    fn machines(&self) -> &[berth_api::MadeMachine] {
        match &self.state {
            Some(Ok(state)) => &state.machines,
            _ => &[],
        }
    }

    /// The rows: how it is, its actions in their groups, the machines it made.
    fn items(&self) -> Vec<Item<Entry>> {
        let mut groups = page::groups(&self.actions, Entry::Action);
        let machines: Vec<(String, Entry)> = self
            .machines()
            .iter()
            .enumerate()
            .map(|(index, machine)| {
                let mut notes = vec![];
                if !machine.state.is_empty() {
                    notes.push(machine.state.clone());
                }
                if !machine.recorded {
                    notes.push("no record of it".into());
                }
                let id = match machine.id.as_str() {
                    "" => "being made",
                    id => id,
                };
                (
                    format!("  {:<18} {id}  {}", machine.host, notes.join(" · ")),
                    Entry::Machine(index),
                )
            })
            .collect();
        if !machines.is_empty() {
            groups.push(Group {
                title: "Machines berth made with it".into(),
                rows: machines,
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

    /// Take in what an action that answers at once said.
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
                keys: "↑↓ select · Enter open · r refresh · Tab hosts · q quit",
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
                Item::Pick(text, Entry::Machine(_)) => Line::styled(text.clone(), muted),
                Item::Pick(text, _) => Line::styled(text.clone(), Style::new().fg(theme::TEXT)),
            })
            .collect();
        let help = match page.selected() {
            Some(Entry::Action(index)) => page::help(
                &page.actions[index],
                &format!(
                    "berth provisioner {} run {}",
                    page.id, page.actions[index].name
                ),
            ),
            Some(Entry::Machine(index)) => {
                let machine = &page.machines()[index];
                match machine.recorded {
                    true => format!(
                        "The machine of {}, {} as the provisioner calls it. It is looked after as that host: its machine is on the host's page, and `berth instance {}` from the command line.",
                        machine.host, machine.id, machine.host
                    ),
                    false => format!(
                        "{} carries berth's labels for the host {}, but berth has no record of it. berth does not delete it; the provisioner's own tools do.",
                        machine.id, machine.host
                    ),
                }
            }
            None => format!(
                "Asking the provisioner how it is. From the command line: `berth provisioner {}`",
                page.id
            ),
        };
        Shown {
            rows,
            selected: self.selected_row(),
            help,
            keys: "↑↓ select · Enter run · r ask again · Esc back · q quit",
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
                Some(Entry::Machine(_)) => {
                    return Asked::Nothing(Some("Open the host from the Hosts tab".into()));
                }
                None => {}
            },
            Code::Char('r') => {
                let id = page.id.clone();
                page.state = None;
                page.said.clear();
                return Asked::Open(id);
            }
            Code::Esc | Code::Backspace | Code::Left => {
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
    use super::*;
    use berth_api::{Argument, Detail, MadeMachine};

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
            machines: vec![MadeMachine {
                host: "studio".into(),
                id: "berth-studio".into(),
                state: "running".into(),
                recorded: true,
            }],
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
        assert!(text(&tab.shown(&list, "here")).contains(&"2 images".to_owned()));
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
}
