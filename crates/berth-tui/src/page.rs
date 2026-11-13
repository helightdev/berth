//! The page of a subject: lines that say how it is, what can be done to it
//! in groups, and the dialogs before an action runs, its question and then
//! its arguments one after another. A host's page is one. A page of another
//! subject is its state and its actions given to the same functions; the
//! terminal and the browser draw the rows each their own way.

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
}

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
    let value = dialog.input.value().to_owned();
    if value.is_empty() && argument.required {
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
        let said = help(&actions[2], "berth provisioner incus/vm resize");
        assert!(said.starts_with("Grows the pool.") && said.contains("Asks for: pool, New size."));
        assert!(said.contains("- **Check**: Says how full it is."));
        assert!(said.contains("It cannot run now: The pool is busy"));
        assert!(said.ends_with("`berth provisioner incus/vm resize`"));
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
}
