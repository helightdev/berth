//! The page of a subject: lines that say how it is, what can be done to it
//! in groups, and the dialogs before an action runs, its question and then
//! its arguments one after another. A host's page is one. A page of another
//! subject is its state and its actions given to the same functions, which
//! the screens draw the rows of.

use berth_api::{Action, Argument};
use std::collections::BTreeMap;
use tui_input::Input;

/// How a line of a page is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Plain,
    Muted,
    Warn,
    Bad,
}

/// A row of a page as the screens draw it. `E` is what a screen does when
/// the row is chosen.
#[derive(Clone, Debug, PartialEq)]
pub enum Item<E> {
    Heading(String),
    Note(String, Tone),
    Blank,
    Pick(String, E),
}

/// A group of rows under a heading.
#[derive(Clone, Debug, PartialEq)]
pub struct Group<E> {
    pub title: String,
    pub rows: Vec<(String, E)>,
}

/// The group an action is listed in.
pub fn group_of(action: &Action) -> &str {
    match action.group.is_empty() {
        true => "System",
        false => &action.group,
    }
}

/// What a row says of an action that cannot run now; its help says why.
pub const UNAVAILABLE: &str = " · not now";

/// The line of an action's row.
pub fn row_line(action: &Action) -> String {
    match action.available {
        true => format!("  ▸ {}", action.title()),
        false => format!("  ▸ {}{UNAVAILABLE}", action.title()),
    }
}

/// The actions in their groups, in the order they come; groups are matched
/// whatever their case. `entry` makes a row's entry from the action's index.
/// One that cannot run now is listed too, marked, so its reason can be read.
pub fn groups<E>(actions: &[Action], entry: impl Fn(usize) -> E) -> Vec<Group<E>> {
    let mut groups: Vec<Group<E>> = vec![];
    for (index, action) in actions.iter().enumerate() {
        let title = group_of(action);
        let row = (row_line(action), entry(index));
        match groups
            .iter_mut()
            .find(|known| known.title.eq_ignore_ascii_case(title))
        {
            Some(group) => group.rows.push(row),
            None => groups.push(Group {
                title: title.to_owned(),
                rows: vec![row],
            }),
        }
    }
    groups
}

/// The rows of a page: its state lines, then each group under its heading.
pub fn items<E>(state: &[(String, Tone)], groups: Vec<Group<E>>) -> Vec<Item<E>> {
    let mut items: Vec<Item<E>> = state
        .iter()
        .map(|(text, tone)| Item::Note(text.clone(), *tone))
        .collect();
    for group in groups {
        if !items.is_empty() {
            items.push(Item::Blank);
        }
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

pub fn argument_label(argument: &Argument) -> &str {
    match argument.label.is_empty() {
        true => &argument.name,
        false => &argument.label,
    }
}

/// What the information box says of an action: its own help, what it asks
/// for, what plugins add to it, why it cannot run now, and the command that
/// runs it.
pub fn help(action: &Action, command: &str) -> String {
    let mut help = action.help.clone();
    if !action.arguments.is_empty() {
        let names: Vec<&str> = action.arguments.iter().map(argument_label).collect();
        help += &format!("\n\nAsks for: {}.", names.join(", "));
    }
    if !action.steps.is_empty() {
        help += "\n\n### From plugins\n\n";
        for step in &action.steps {
            help += &format!("- **{}**: {}\n", step.label, step.help.replace('\n', " "));
        }
    }
    if !action.available {
        help += &format!("\n\nIt cannot run now: {}", action.why);
    }
    if !command.is_empty() {
        help += &format!("\n\nFrom the command line: `{command}`");
    }
    help
}

// ── Before an action runs ───────────────────────────────────────────────────

/// An action and the values of its arguments, ready to run.
pub type Launch = (usize, BTreeMap<String, String>);

/// The dialog that asks for an action's arguments, one after another.
pub struct Arguments {
    /// The action, by its index.
    pub action: usize,
    /// The argument being asked for.
    pub index: usize,
    pub values: BTreeMap<String, String>,
    pub input: Input,
    pub choices: Vec<String>,
    /// The choice last put into the input, for stepping through them.
    pub chosen: Option<usize>,
    /// What is typed is a secret: it is neither shown nor copied.
    pub secret: bool,
    /// What is typed has several lines, shown as [`NEWLINE`] in the input.
    pub text: bool,
}

/// How a line break of a value of several lines shows in an input of one.
pub const NEWLINE: char = '⏎';

impl Arguments {
    fn at(
        action: usize,
        index: usize,
        values: BTreeMap<String, String>,
        argument: &Argument,
    ) -> Self {
        Arguments {
            action,
            index,
            values,
            input: Input::new(argument.default.clone()),
            choices: vec![],
            chosen: None,
            secret: argument.secret,
            text: argument.kind == Argument::TEXT,
        }
    }

    /// The action and argument whose choices to ask for, when the argument
    /// being asked for has a command that offers them.
    pub fn wants_choices(&self, actions: &[Action]) -> Option<(String, String)> {
        let action = actions.get(self.action)?;
        let argument = action.arguments.get(self.index)?;
        (!argument.choices.is_empty()).then(|| (action.name.clone(), argument.name.clone()))
    }

    /// Take the choices offered for an argument, if it is still the one asked for.
    pub fn offered(
        &mut self,
        actions: &[Action],
        action: &str,
        argument: &str,
        choices: Vec<String>,
    ) {
        if let Some(known) = actions.get(self.action)
            && known.name == action
            && known
                .arguments
                .get(self.index)
                .is_some_and(|known| known.name == argument)
        {
            self.choices = choices;
        }
    }
}

/// What choosing an action leads to.
pub enum Begin {
    /// Its question, to be answered yes first.
    Ask(String),
    /// Its arguments, asked for one after another.
    Arguments(Arguments),
    Run(Launch),
    /// There is no such action.
    Nothing,
}

/// Begin an action: its question first, unless it was answered, then its
/// arguments.
pub fn begin(actions: &[Action], index: usize, confirmed: bool) -> Begin {
    let Some(action) = actions.get(index) else {
        return Begin::Nothing;
    };
    if let (Some(question), false) = (&action.confirm, confirmed) {
        return Begin::Ask(question.clone());
    }
    match action.arguments.first() {
        None => Begin::Run((index, BTreeMap::new())),
        Some(first) => Begin::Arguments(Arguments::at(index, 0, BTreeMap::new(), first)),
    }
}

/// What taking the value typed for an argument leads to.
pub enum Accepted {
    /// The next argument is asked for.
    Next(Arguments),
    /// All are there.
    Run(Launch),
    /// A needed one was left empty; the dialog stays, with what to say.
    Missing(Arguments, String),
}

/// Take the value typed for the argument being asked for.
pub fn accept(actions: &[Action], mut dialog: Arguments) -> Accepted {
    let Some(action) = actions.get(dialog.action) else {
        return Accepted::Run((dialog.action, dialog.values));
    };
    let Some(argument) = action.arguments.get(dialog.index) else {
        return Accepted::Run((dialog.action, dialog.values));
    };
    let value = match dialog.text {
        true => dialog.input.value().replace(NEWLINE, "\n"),
        false => dialog.input.value().to_owned(),
    };
    if value.trim().is_empty() && argument.required {
        let said = format!("{} is needed", argument_label(argument));
        return Accepted::Missing(dialog, said);
    }
    dialog.values.insert(argument.name.clone(), value);
    match action.arguments.get(dialog.index + 1) {
        Some(next) => Accepted::Next(Arguments::at(
            dialog.action,
            dialog.index + 1,
            dialog.values,
            next,
        )),
        None => Accepted::Run((dialog.action, dialog.values)),
    }
}

// ── Removing a host from a list ─────────────────────────────────────────────

/// What removing a host from the home screen asks: whether to forget it or
/// also destroy the machine its provisioner made, and for destroying, once
/// more. A host no provisioner made is only forgotten.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Removal {
    pub name: String,
    /// The provisioner that made its machine, and its id there.
    pub machine: Option<(String, String)>,
    /// Destroying was chosen, and is asked once more.
    pub destroying: bool,
}

/// What a key in the removal's dialog leads to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Removing {
    /// The dialog stays, maybe at its next question.
    Stay,
    Cancel,
    /// Run the host's action remove, destroying its machine or not.
    Run {
        destroy: bool,
    },
}

impl Removal {
    /// The removal of a host as the server lists it: a machine with an id
    /// there is one its provisioner made.
    pub fn of(host: &serde_json::Value) -> Removal {
        let machine = &host["machine"];
        let made = machine["external_id"].as_str().unwrap_or("");
        Removal {
            name: host["name"].as_str().unwrap_or("").to_owned(),
            machine: (!made.is_empty()).then(|| {
                (
                    machine["provisioner"].as_str().unwrap_or("").to_owned(),
                    made.to_owned(),
                )
            }),
            destroying: false,
        }
    }

    /// The dialog's lines and its keys.
    pub fn question(&self) -> (Vec<String>, &'static str) {
        let name = &self.name;
        match (&self.machine, self.destroying) {
            (None, _) => (
                vec![format!(
                    "Forget {name}, with its profile and passwords? The machine is left as it is."
                )],
                " y forget · n no ",
            ),
            (Some((provisioner, id)), false) => (
                vec![
                    format!("Remove {name}? {provisioner} made its machine, {id}."),
                    String::new(),
                    "f forgets the host and leaves the machine as it is, under its provisioner's resources.".into(),
                    format!("d destroys the machine with {provisioner} as well, with its disk."),
                ],
                " f forget · d destroy · Esc cancel ",
            ),
            (Some((provisioner, id)), true) => (
                vec![format!(
                    "Destroy {id} with {provisioner}, and its disk, and forget {name}? It cannot be undone."
                )],
                " y destroy · n no ",
            ),
        }
    }

    pub fn key(&mut self, key: char) -> Removing {
        match (self.machine.is_some(), self.destroying, key) {
            (false, _, 'y' | 'Y') => Removing::Run { destroy: false },
            (true, false, 'f' | 'F') => Removing::Run { destroy: false },
            (true, false, 'd' | 'D') => {
                self.destroying = true;
                Removing::Stay
            }
            (true, true, 'y' | 'Y') => Removing::Run { destroy: true },
            (_, _, 'n' | 'N') => Removing::Cancel,
            _ => Removing::Stay,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use berth_api::ActionStep;

    /// A subject that is no host: its actions and its state are all the page has.
    fn pool() -> Vec<Action> {
        vec![
            Action {
                name: "volumes".into(),
                label: "Volumes".into(),
                group: "Storage".into(),
                changes: false,
                ..Action::default()
            },
            Action {
                name: "prune".into(),
                label: "Remove unused images".into(),
                group: "storage".into(),
                confirm: Some("Remove every image no container uses?".into()),
                ..Action::default()
            },
            Action {
                name: "resize".into(),
                label: "Resize a pool".into(),
                help: "Grows the pool.".into(),
                group: "Pools".into(),
                arguments: vec![
                    Argument {
                        name: "pool".into(),
                        required: true,
                        choices: "pools".into(),
                        ..Argument::default()
                    },
                    Argument {
                        name: "size".into(),
                        label: "New size".into(),
                        default: "200GiB".into(),
                        ..Argument::default()
                    },
                ],
                steps: vec![ActionStep {
                    label: "Check".into(),
                    help: "Says how full it is.".into(),
                }],
                available: false,
                why: "The pool is busy".into(),
                ..Action::default()
            },
        ]
    }

    #[test]
    fn a_page_is_its_state_and_its_actions_in_groups() {
        let actions = pool();
        let groups = groups(&actions, |index| index);
        assert_eq!(groups.len(), 2);
        assert_eq!(
            groups[0].rows,
            vec![
                ("  ▸ Volumes".to_owned(), 0),
                ("  ▸ Remove unused images".to_owned(), 1)
            ]
        );
        let items = items(&[("reachable · 3 pools".into(), Tone::Muted)], groups);
        assert_eq!(
            items[0],
            Item::Note("reachable · 3 pools".into(), Tone::Muted)
        );
        assert_eq!(items[1], Item::Blank);
        assert_eq!(items[2], Item::Heading("Storage".into()));
        assert_eq!(
            items.last(),
            Some(&Item::Pick("  ▸ Resize a pool · not now".into(), 2))
        );
        let said = help(&actions[2], "berth provisioner run incus/vm resize");
        assert!(said.starts_with("Grows the pool.") && said.contains("Asks for: pool, New size."));
        assert!(said.contains("- **Check**: Says how full it is."));
        assert!(said.contains("It cannot run now: The pool is busy"));
        assert!(said.ends_with("`berth provisioner run incus/vm resize`"));
    }

    #[test]
    fn an_action_asks_its_question_then_each_argument() {
        let actions = pool();
        assert!(matches!(begin(&actions, 0, false), Begin::Run((0, values)) if values.is_empty()));
        assert!(matches!(begin(&actions, 9, false), Begin::Nothing));
        assert!(
            matches!(begin(&actions, 1, false), Begin::Ask(question) if question.contains("Remove every"))
        );
        assert!(matches!(begin(&actions, 1, true), Begin::Run(_)));
        let Begin::Arguments(mut dialog) = begin(&actions, 2, false) else {
            panic!("no arguments");
        };
        assert_eq!(
            dialog.wants_choices(&actions),
            Some(("resize".into(), "pool".into()))
        );
        dialog.offered(&actions, "resize", "size", vec!["no".into()]);
        assert!(dialog.choices.is_empty());
        dialog.offered(
            &actions,
            "resize",
            "pool",
            vec!["default".into(), "fast".into()],
        );
        assert_eq!(dialog.choices.len(), 2);
        // A needed one cannot be left empty.
        let Accepted::Missing(mut dialog, said) = accept(&actions, dialog) else {
            panic!("taken empty");
        };
        assert_eq!(said, "pool is needed");
        dialog.input = Input::new("fast".into());
        let Accepted::Next(dialog) = accept(&actions, dialog) else {
            panic!("no next");
        };
        assert_eq!((dialog.index, dialog.input.value()), (1, "200GiB"));
        assert!(dialog.wants_choices(&actions).is_none());
        let Accepted::Run((index, values)) = accept(&actions, dialog) else {
            panic!("not run");
        };
        assert_eq!(index, 2);
        assert_eq!(
            (values["pool"].as_str(), values["size"].as_str()),
            ("fast", "200GiB")
        );
    }

    #[test]
    fn removing_a_host_a_provisioner_made_asks_to_forget_or_destroy() {
        let made = serde_json::json!({"name": "studio", "machine": {"provisioner": "incus", "external_id": "berth-studio", "state": "running"}});
        let mut removal = Removal::of(&made);
        let (lines, keys) = removal.question();
        assert_eq!(
            lines[0],
            "Remove studio? incus made its machine, berth-studio."
        );
        assert!(keys.contains("f forget") && keys.contains("d destroy"));
        assert_eq!(removal.key('y'), Removing::Stay);
        assert_eq!(removal.key('f'), Removing::Run { destroy: false });
        assert_eq!(removal.key('d'), Removing::Stay);
        assert!(
            removal.question().0[0].starts_with("Destroy berth-studio with incus, and its disk")
        );
        assert_eq!(removal.key('d'), Removing::Stay);
        assert_eq!(removal.key('y'), Removing::Run { destroy: true });
        // One only planned, or none, is forgotten.
        let planned = serde_json::json!({"name": "lab", "machine": {"provisioner": "incus", "external_id": "", "state": "planned"}});
        let mut removal = Removal::of(&planned);
        assert!(removal.machine.is_none());
        assert_eq!(removal.key('d'), Removing::Stay);
        assert_eq!(removal.key('y'), Removing::Run { destroy: false });
        assert_eq!(
            Removal::of(&serde_json::json!({"name": "dock"})).key('n'),
            Removing::Cancel
        );
    }
}
