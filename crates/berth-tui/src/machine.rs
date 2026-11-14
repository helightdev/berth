//! The machine berth creates for a host with a provisioner of a plugin, as
//! the host's set-up tabs show it: what there is to create it with and what
//! the chosen one requires on the Install tab, the machine's size and the
//! rest of its arguments on the Settings tab, and creating it, or the machine
//! once there is one, on the Run tab. The machine is the host's resource of
//! the machine kind, and starting, stopping and deleting it are its actions.
//! berth and its page in a browser ask for these and run the jobs each their
//! own way.

use crate::fields::{Kind, Step};
use crate::setup::{Entry, Problem, Row, value_line};
use crate::theme;
use berth_api::{NewResource, ProvisionerInfo, Resource};
use ratatui::{
    style::Style,
    text::{Line, Span},
};
use std::collections::BTreeMap;

/// A row of the machine's part of the set-up tabs that does something.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pick {
    /// The provisioner to create the machine with; Enter switches to the next.
    Provisioner,
    /// An argument of the chosen provisioner, by index into its arguments.
    Argument(usize),
    Create,
    Start,
    Stop,
    Delete,
}

/// What choosing a row, or typing into one, leads to.
#[derive(Debug, PartialEq)]
pub enum Next {
    Nothing,
    Say(String),
    /// Open the editor with this text in it.
    Edit(String),
    /// Ask the question in [`Machine::question`] first.
    Confirm,
    /// Create the machine; the host is the caller's to fill in.
    Create(NewResource),
    /// `start` or `stop`.
    Power(&'static str),
    Destroy,
}

/// What is known of a host's machine and of the provisioners, and the form
/// of the one chosen to create the machine with.
#[derive(Default)]
pub struct Machine {
    /// They were asked for: the rows show what came back, or that it is awaited.
    pub asked: bool,
    pub provisioners: Option<Result<Vec<ProvisionerInfo>, String>>,
    /// The machine berth created for the host, the resource of the machine
    /// kind whose host it is; `None` inside when it has none.
    pub instance: Option<Result<Option<Resource>, String>>,
    /// The chosen provisioner, by its name.
    pub chosen: Option<String>,
    /// What its arguments are given, by name.
    pub values: BTreeMap<String, String>,
    /// The question asked before creating, while it is open.
    pub question: Option<String>,
}

/// Whether an error of asking for the machine only says there is none: the
/// host has none, or the server has no such host yet.
pub fn absent(error: &str) -> bool {
    error.contains("has no machine that berth created")
        || error.starts_with("No host ")
        || error.starts_with("No resource ")
}

/// How a provisioner sets the new machine up, in a few words.
pub fn setup_note(provisioner: &ProvisionerInfo) -> &'static str {
    match provisioner.setup.as_str() {
        "seed" => "sets itself up from the cloud seed",
        _ => "berth applies the profile over SSH",
    }
}

fn label(argument: &berth_api::Argument) -> &str {
    match argument.label.as_str() {
        "" => &argument.name,
        label => label,
    }
}

/// What the editor is called while it holds what is typed for `pick`.
pub fn editor_title(pick: Pick, host: &str) -> String {
    match pick {
        Pick::Delete => format!(" Type {host} to delete its machine "),
        _ => " Edit ".into(),
    }
}

/// A count of CPUs: a positive number, such as 4 or 1.5.
pub fn valid_count(value: &str) -> bool {
    value
        .parse::<f64>()
        .is_ok_and(|count| count > 0.0 && count.is_finite())
        && value.chars().all(|ch| ch.is_ascii_digit() || ch == '.')
}

/// A size with its unit, such as 8GiB, 512MiB or 4g.
pub fn valid_size(value: &str) -> bool {
    let split = value
        .find(|ch: char| !ch.is_ascii_digit() && ch != '.')
        .unwrap_or(value.len());
    let (number, unit) = value.split_at(split);
    let units = [
        "b", "k", "kb", "kib", "m", "mb", "mib", "g", "gb", "gib", "t", "tb", "tib",
    ];
    valid_count(number) && units.contains(&unit.to_ascii_lowercase().as_str())
}

/// Why a resource's value would not do, by what its name says it is.
fn resource_problem(argument: &berth_api::Argument, value: &str) -> Option<String> {
    let name = argument.name.to_ascii_lowercase();
    let sized = ["memory", "mem", "ram", "disk", "size", "storage", "swap"];
    if name.contains("cpu") {
        (!valid_count(value))
            .then(|| format!("{} must be a positive number, such as 4", label(argument)))
    } else if sized.iter().any(|word| name.contains(word)) {
        (!valid_size(value)).then(|| {
            format!(
                "{} must be a size with its unit, such as 8GiB, 512MiB or 4g",
                label(argument)
            )
        })
    } else {
        None
    }
}

impl Machine {
    /// Forget what was known, before asking again. The form stays.
    pub fn asking(&mut self) {
        self.asked = true;
        self.provisioners = None;
        self.instance = None;
    }

    /// Take in the provisioners there are. The first is chosen until
    /// another is, as a choice of a field has a value from the start.
    pub fn listed(&mut self, provisioners: Result<Vec<ProvisionerInfo>, String>) {
        self.provisioners = Some(provisioners);
        // One that is gone takes its form with it.
        if self.provisioner().is_none() {
            self.chosen = None;
            self.values.clear();
            self.choose(0);
        }
    }

    pub fn found(&mut self, instance: Result<Option<Resource>, String>) {
        // A machine there is was made by its provisioner, which is the chosen one then.
        if let Ok(Some(made)) = &instance {
            let id = made.provisioner.clone();
            self.choose_id(&id);
        }
        self.instance = Some(instance);
    }

    fn all(&self) -> &[ProvisionerInfo] {
        match &self.provisioners {
            Some(Ok(all)) => all,
            _ => &[],
        }
    }

    /// The provisioner chosen to create the machine with.
    pub fn provisioner(&self) -> Option<&ProvisionerInfo> {
        let chosen = self.chosen.as_ref()?;
        self.all().iter().find(|known| &known.id == chosen)
    }

    /// The machine the host has, once that is known.
    pub fn machine(&self) -> Option<&Resource> {
        match &self.instance {
            Some(Ok(Some(instance))) => Some(instance),
            _ => None,
        }
    }

    /// Choose a provisioner: its form starts from the provisioner's kept
    /// settings, such as its server, and the defaults of the rest.
    pub fn choose(&mut self, index: usize) {
        let Some(provisioner) = self.all().get(index) else {
            return;
        };
        if self.chosen.as_ref() == Some(&provisioner.id) {
            return;
        }
        let values = provisioner
            .arguments
            .iter()
            .map(|argument| {
                let value = match provisioner.settings.get(&argument.name) {
                    Some(kept) if !kept.is_empty() => kept.clone(),
                    _ => argument.default.clone(),
                };
                (argument.name.clone(), value)
            })
            .collect();
        self.chosen = Some(provisioner.id.clone());
        self.values = values;
    }

    /// The provisioners to choose from, as (label, name), the way a
    /// choice field offers its values.
    pub fn choices(&self) -> Vec<(String, String)> {
        self.all()
            .iter()
            .map(|known| {
                // Two provisioners of one type differ by their names alone.
                let label = match known.kind.is_empty() || known.kind == known.id {
                    true => known.label.clone(),
                    false => format!("{} · {}", known.id, known.label),
                };
                (label, known.id.clone())
            })
            .collect()
    }

    /// An argument of the chosen provisioner, by its place among them.
    pub fn argument(&self, index: usize) -> Option<&berth_api::Argument> {
        self.provisioner()?.arguments.get(index)
    }

    /// The values an argument that is a choice offers, as (label, value);
    /// none for one that takes any text.
    pub fn argument_choices(&self, index: usize) -> Vec<(String, String)> {
        self.argument(index)
            .map(|argument| {
                argument
                    .values
                    .iter()
                    .map(|value| (value.clone(), value.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// What an argument is given now.
    pub fn value(&self, index: usize) -> Option<&str> {
        let argument = self.argument(index)?;
        Some(self.values.get(&argument.name).map_or("", String::as_str))
    }

    /// Choose the provisioner with this name.
    pub fn choose_id(&mut self, id: &str) {
        if let Some(index) = self.all().iter().position(|known| known.id == id) {
            self.choose(index);
        }
    }

    /// Choose the provisioner after the chosen one, and the first after the last.
    pub fn choose_next(&mut self) {
        let count = self.all().len();
        if count == 0 {
            return;
        }
        let next = match self.chosen.as_ref() {
            Some(chosen) => self
                .all()
                .iter()
                .position(|known| &known.id == chosen)
                .map_or(0, |index| (index + 1) % count),
            None => 0,
        };
        self.choose(next);
    }

    /// The tab an argument of the chosen provisioner is on: a required one
    /// has to be chosen, the rest have defaults.
    pub fn step_of(argument: &berth_api::Argument) -> Step {
        match argument.required {
            true => Step::Install,
            false => Step::Settings,
        }
    }

    /// Whether the answers have come, so what they say can be judged.
    fn answered(&self) -> bool {
        self.provisioners.is_some() && self.instance.is_some()
    }

    /// Why creating with the chosen provisioner would be refused, each on
    /// the tab of what it is about. Nothing before anything was asked.
    pub fn problems(&self) -> Vec<Problem> {
        if !self.asked || !self.answered() || self.exists() {
            return vec![];
        }
        let mut problems = vec![];
        if let Some(Err(error)) = &self.instance {
            problems.push(Problem::new(Step::Run, error.clone()));
        }
        match &self.provisioners {
            Some(Err(error)) => problems.push(Problem::new(Step::Install, error.clone())),
            Some(Ok(all)) if all.is_empty() => problems.push(Problem::new(
                Step::Install,
                "No provisioner is registered. Add one on the Provisioners tab.",
            )),
            _ => {}
        }
        let Some(provisioner) = self.provisioner() else {
            if !self.all().is_empty() {
                problems.push(Problem::new(Step::Install, "Choose a provisioner"));
            }
            return problems;
        };
        if !provisioner.missing_secrets.is_empty() {
            problems.push(Problem::new(
                Step::Install,
                format!(
                    "Needs the secret{} {}: berth secret set NAME, or the Secrets tab of the home screen",
                    if provisioner.missing_secrets.len() == 1 {
                        ""
                    } else {
                        "s"
                    },
                    provisioner.missing_secrets.join(", ")
                ),
            ));
        }
        for argument in &provisioner.arguments {
            let value = self
                .values
                .get(&argument.name)
                .map_or("", |value| value.trim());
            let step = Machine::step_of(argument);
            if value.is_empty() {
                if argument.required {
                    problems.push(Problem::new(
                        step,
                        format!("{} is required", label(argument)),
                    ));
                }
                continue;
            }
            if argument.is_resource()
                && let Some(problem) = resource_problem(argument, value)
            {
                problems.push(Problem::new(step, problem));
            }
        }
        problems
    }

    /// What creating the machine asks for, once nothing is in the way.
    pub fn request(&self) -> Result<NewResource, String> {
        let provisioner = self.provisioner().ok_or("Choose a provisioner first")?;
        if let Some(problem) = self.problems().into_iter().next() {
            return Err(problem.text);
        }
        let arguments = provisioner
            .arguments
            .iter()
            .map(|argument| {
                let value = self.values.get(&argument.name).cloned().unwrap_or_default();
                (argument.name.clone(), value.trim().to_owned())
            })
            .collect();
        Ok(NewResource {
            provisioner: provisioner.id.clone(),
            kind: "machine".into(),
            arguments,
            ..NewResource::default()
        })
    }

    /// The actions the machine offers, beside what it is.
    pub fn actions(instance: &Resource) -> Vec<Pick> {
        let mut picks = vec![];
        for (verb, pick) in [("start", Pick::Start), ("stop", Pick::Stop)] {
            if instance.verbs.iter().any(|known| known == verb) {
                picks.push(pick);
            }
        }
        picks.push(Pick::Delete);
        picks
    }

    /// Whether berth created a machine for the host, which the Run tab
    /// then shows whatever way of installing is chosen.
    pub fn exists(&self) -> bool {
        matches!(self.instance, Some(Ok(Some(_))))
    }

    /// The rows of arguments of the chosen provisioner that are on `step`
    /// and pass `keep`, as a tree under what is above them.
    fn argument_rows(&self, step: Step, keep: impl Fn(&berth_api::Argument) -> bool) -> Vec<Row> {
        let Some(provisioner) = self.provisioner() else {
            return vec![];
        };
        let muted = Style::new().fg(theme::MUTED);
        let shown: Vec<(usize, &berth_api::Argument)> = provisioner
            .arguments
            .iter()
            .enumerate()
            .filter(|(_, argument)| Machine::step_of(argument) == step && keep(argument))
            .collect();
        let count = shown.len();
        shown
            .into_iter()
            .enumerate()
            .map(|(place, (index, argument))| {
                let branch = if place + 1 < count {
                    "  ├─ "
                } else {
                    "  └─ "
                };
                let name = match argument.required {
                    true => format!("{} *", label(argument)),
                    false => label(argument).to_owned(),
                };
                let value = self.values.get(&argument.name).map_or("", String::as_str);
                let mut line = value_line(&name, value, Kind::Text);
                line.spans.insert(0, Span::styled(branch, muted));
                Row::entry(line, Entry::Machine(Pick::Argument(index)))
            })
            .collect()
    }

    /// The provisioner's rows of the Install tab: the one chosen, and what
    /// it requires under it. Its problems are listed with the tab's.
    pub fn install_rows(&self) -> Vec<Row> {
        if !self.asked {
            return vec![];
        }
        let muted = Style::new().fg(theme::MUTED);
        let note = |text: &str| {
            Row::info(Line::from(vec![
                Span::styled("Provisioner  ", Style::new().fg(theme::TEXT)),
                Span::styled(text.to_owned(), muted.italic()),
            ]))
        };
        if !self.answered() {
            return vec![note("Asking for the machine and the provisioners…")];
        }
        if let Some(instance) = self.machine() {
            return vec![note(&format!(
                "{} created it · the Run tab has the machine",
                instance.provisioner
            ))];
        }
        let Some(provisioner) = self.provisioner() else {
            return vec![note("None to choose")];
        };
        let mut line = value_line("Provisioner", &provisioner.label, Kind::Text);
        line.spans.push(Span::styled(
            format!("  · {}", setup_note(provisioner)),
            muted,
        ));
        let mut rows = vec![Row::entry(line, Entry::Machine(Pick::Provisioner))];
        rows.extend(self.argument_rows(Step::Install, |_| true));
        rows
    }

    /// The provisioner's rows of the Settings tab: the machine's size, then
    /// its other arguments, each under a heading when it has any.
    pub fn settings_rows(&self) -> Vec<Row> {
        if self.exists() {
            return vec![];
        }
        let mut rows = vec![];
        let resources = self.argument_rows(Step::Settings, |argument| argument.is_resource());
        if !resources.is_empty() {
            rows.push(Row::heading("Machine"));
            rows.extend(resources);
        }
        let others = self.argument_rows(Step::Settings, |argument| !argument.is_resource());
        if !others.is_empty() {
            rows.push(Row::heading("Provisioner Options"));
            rows.extend(others);
        }
        rows
    }

    /// The machine's rows of the Run tab: the button that creates it, or the
    /// machine once there is one, with what can be done to it.
    pub fn run_rows(&self) -> Vec<Row> {
        let muted = Style::new().fg(theme::MUTED);
        if self.asked && !self.answered() {
            return vec![Row::info(Line::styled(
                "Asking for the machine and the provisioners…",
                muted,
            ))];
        }
        let Some(instance) = self.machine() else {
            let label = self
                .provisioner()
                .map_or(String::new(), |known| format!(" · {}", known.label));
            return vec![Row::entry(
                format!("[Create machine{label}]"),
                Entry::Machine(Pick::Create),
            )];
        };
        let mut rows = vec![Row::heading("Machine")];
        let line = |label: &str, value: &str| Row::info(value_line(label, value, Kind::Text));
        rows.push(line("  Created with", &instance.provisioner));
        rows.push(line("  Machine", &instance.external_id));
        rows.push(line("  Resource", &instance.id));
        let colour = match instance.state.as_str() {
            "running" => theme::GOOD,
            "missing" => theme::BAD,
            "stopped" => theme::MUTED,
            _ => theme::WARN,
        };
        rows.push(Row::info(Line::from(vec![
            Span::styled("  State  ", Style::new().fg(theme::TEXT)),
            Span::styled(instance.state.clone(), Style::new().fg(colour)),
        ])));
        if !instance.address.is_empty() {
            rows.push(line("  Address", &instance.address));
        }
        if !instance.problem.is_empty() {
            rows.push(Row::info(Line::styled(
                format!("  ! {}", instance.problem),
                Style::new().fg(theme::WARN),
            )));
        }
        for pick in Machine::actions(instance) {
            let text = match pick {
                Pick::Start => "  ▸ Start machine",
                Pick::Stop => "  ▸ Stop machine",
                _ => "  ▸ Delete machine",
            };
            rows.push(Row::entry(text, Entry::Machine(pick)));
        }
        rows
    }

    /// What the information box says of a row.
    pub fn help(&self, pick: Pick) -> String {
        match pick {
            Pick::Provisioner => {
                let Some(provisioner) = self.provisioner() else {
                    return String::new();
                };
                let mut text = format!("{}\n\n", provisioner.description);
                text += match provisioner.setup.as_str() {
                    "seed" => "The new machine boots with berth's cloud seed, made from this profile, and sets itself up.",
                    _ => "berth applies this profile over SSH once the new machine answers. The profile needs an SSH user for that.",
                };
                if !provisioner.secrets.is_empty() {
                    text += &format!(
                        "\n\nIt is given the secrets {}.",
                        provisioner.secrets.join(", ")
                    );
                }
                if !provisioner.missing_secrets.is_empty() {
                    text += &format!(
                        " Not set yet: {}. Set them with `berth secret set NAME`, or on the Secrets tab of the home screen.",
                        provisioner.missing_secrets.join(", ")
                    );
                }
                if self.all().len() > 1 {
                    text += "\n\nSpace or Enter switches to the next provisioner.";
                }
                text + "\n\nWhat it requires is below. The machine's size and its other arguments are on the Settings tab."
            }
            Pick::Argument(index) => {
                let Some(argument) = self
                    .provisioner()
                    .and_then(|provisioner| provisioner.arguments.get(index))
                else {
                    return String::new();
                };
                let mut text = format!("{} (`{}`).", label(argument), argument.name);
                if !argument.help.is_empty() {
                    text += &format!(" {}", argument.help);
                }
                if argument.required {
                    text += " Required.";
                }
                if argument.is_resource() {
                    text += " Part of the machine's size.";
                }
                if !argument.default.is_empty() {
                    text += &format!(" Default: {}.", argument.default);
                }
                match argument.values.is_empty() {
                    true => text + " Enter changes it.",
                    false => text + " Space or Enter switches to the next value.",
                }
            }
            Pick::Create => "Creates the machine and sets it up as this host, with the profile saved first. The output is shown as it runs. When setting it up fails, the machine stays: Delete machine removes it.".into(),
            Pick::Start => "Starts the machine.".into(),
            Pick::Stop => "Stops the machine.".into(),
            Pick::Delete => "Deletes the machine berth created for this host. The profile stays, and so does its node on the tailnet. Enter, then type the host's name to say so.".into(),
        }
    }

    /// Enter on a row.
    pub fn enter(&mut self, pick: Pick, host: &str) -> Next {
        match pick {
            Pick::Provisioner => {
                self.choose_next();
                Next::Nothing
            }
            Pick::Argument(index) => {
                let Some(argument) = self.argument(index) else {
                    return Next::Nothing;
                };
                let value = self.value(index).unwrap_or("").to_owned();
                // One of a few values goes on to the next, as a choice field does.
                if !argument.values.is_empty() {
                    let all = &argument.values;
                    let next = all
                        .iter()
                        .position(|known| *known == value)
                        .map_or(0, |at| (at + 1) % all.len());
                    let (name, next) = (argument.name.clone(), all[next].clone());
                    self.values.insert(name, next);
                    return Next::Nothing;
                }
                Next::Edit(value)
            }
            Pick::Create => match self.request() {
                Ok(_) => {
                    let label = self.provisioner().map_or("", |known| known.label.as_str());
                    self.question = Some(format!("Create a machine for {host} with {label}?"));
                    Next::Confirm
                }
                Err(problem) => Next::Say(problem),
            },
            Pick::Start => Next::Power("start"),
            Pick::Stop => Next::Power("stop"),
            Pick::Delete => Next::Edit(String::new()),
        }
    }

    /// Take what was typed into the editor for a row.
    pub fn typed(&mut self, pick: Pick, value: &str, host: &str) -> Next {
        match pick {
            Pick::Argument(index) => {
                let name = self
                    .provisioner()
                    .and_then(|provisioner| provisioner.arguments.get(index))
                    .map(|argument| argument.name.clone());
                if let Some(name) = name {
                    self.values.insert(name, value.trim().to_owned());
                }
                Next::Nothing
            }
            // A provisioner chosen from the suggestions, by its id.
            Pick::Provisioner => {
                self.choose_id(value.trim());
                Next::Nothing
            }
            Pick::Delete if value.trim() == host => Next::Destroy,
            Pick::Delete if value.trim().is_empty() => Next::Nothing,
            Pick::Delete => Next::Say(format!("Not deleted: type {host} to delete its machine")),
            _ => Next::Nothing,
        }
    }

    /// The answer to the question before creating.
    pub fn answer(&mut self, yes: bool) -> Next {
        if self.question.take().is_none() || !yes {
            return Next::Nothing;
        }
        match self.request() {
            Ok(asked) => Next::Create(asked),
            Err(problem) => Next::Say(problem),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use berth_api::Argument;

    fn provisioner(missing: &[&str]) -> ProvisionerInfo {
        ProvisionerInfo {
            id: "lab/vm".into(),
            label: "Lab machine".into(),
            description: "A machine in the lab.".into(),
            setup: "seed".into(),
            verbs: vec!["start".into(), "stop".into()],
            secrets: vec!["lab_token".into()],
            missing_secrets: missing.iter().map(|name| (*name).to_owned()).collect(),
            arguments: vec![
                Argument {
                    name: "server".into(),
                    label: "Lab server".into(),
                    required: true,
                    ..Default::default()
                },
                Argument {
                    name: "cpus".into(),
                    label: "vCPUs".into(),
                    default: "4".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    fn text(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|row| {
                row.line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    }

    fn picks(rows: &[Row]) -> Vec<Pick> {
        rows.iter()
            .filter_map(|row| match row.entry {
                Some(Entry::Machine(pick)) => Some(pick),
                _ => None,
            })
            .collect()
    }

    fn without_machine(provisioners: Vec<ProvisionerInfo>) -> Machine {
        let mut machine = Machine::default();
        machine.asking();
        machine.listed(Ok(provisioners));
        machine.found(Ok(None));
        machine
    }

    fn texts(problems: Vec<Problem>) -> Vec<(Step, String)> {
        problems
            .into_iter()
            .map(|problem| (problem.step, problem.text))
            .collect()
    }

    /// A provisioner that sizes its machine, as the Incus plugin's does.
    fn sized(id: &str, label: &str, resources: &[&str]) -> ProvisionerInfo {
        let mut arguments = vec![Argument {
            name: "server".into(),
            label: "Server".into(),
            required: true,
            ..Default::default()
        }];
        for name in resources {
            arguments.push(Argument {
                name: (*name).into(),
                label: (*name).into(),
                default: "1".into(),
                kind: Argument::RESOURCE.into(),
                ..Default::default()
            });
        }
        arguments.push(Argument {
            name: "image".into(),
            label: "Image".into(),
            default: "ubuntu".into(),
            ..Default::default()
        });
        ProvisionerInfo {
            id: id.into(),
            label: label.into(),
            setup: "seed".into(),
            arguments,
            ..Default::default()
        }
    }

    #[test]
    fn nothing_shows_before_it_was_asked_and_little_while_it_is() {
        let mut machine = Machine::default();
        assert!(machine.install_rows().is_empty() && machine.problems().is_empty());
        machine.asking();
        assert!(text(&machine.install_rows())[0].contains("Asking"));
        assert!(text(&machine.run_rows())[0].contains("Asking"));
        let none = without_machine(vec![]);
        assert_eq!(text(&none.install_rows()), ["Provisioner  None to choose"]);
        assert!(picks(&none.install_rows()).is_empty());
        assert_eq!(
            texts(none.problems()),
            [(
                Step::Install,
                "No provisioner is registered. Add one on the Provisioners tab.".into()
            )]
        );
    }

    #[test]
    fn a_kept_setting_of_the_provisioner_fills_what_it_requires() {
        let mut kept = provisioner(&[]);
        kept.settings =
            BTreeMap::from([("server".to_owned(), "https://192.0.2.10:8443".to_owned())]);
        let machine = without_machine(vec![kept]);
        assert_eq!(
            text(&machine.install_rows())[1],
            "  └─ Lab server *  https://192.0.2.10:8443"
        );
        assert!(machine.problems().is_empty());
        let request = machine.request().unwrap();
        assert_eq!(request.arguments["server"], "https://192.0.2.10:8443");
    }

    #[test]
    fn the_chosen_provisioner_is_one_row_with_what_it_requires_under_it() {
        let mut machine = without_machine(vec![provisioner(&[])]);
        // The first is chosen when they arrive, as a choice has a value.
        let rows = machine.install_rows();
        assert_eq!(picks(&rows), [Pick::Provisioner, Pick::Argument(0)]);
        let shown = text(&rows);
        assert_eq!(
            shown[0],
            "Provisioner  Lab machine  · sets itself up from the cloud seed"
        );
        // Only the required one is on Install; the other has its default on Settings.
        assert_eq!(shown[1], "  └─ Lab server *  Not set");
        assert_eq!(
            text(&machine.settings_rows()),
            ["Provisioner Options", "  └─ vCPUs  4"]
        );
        assert_eq!(
            texts(machine.problems()),
            [(Step::Install, "Lab server is required".into())]
        );
        assert_eq!(
            machine.enter(Pick::Argument(1), "studio"),
            Next::Edit("4".into())
        );
        assert_eq!(picks(&machine.run_rows()), [Pick::Create]);
        assert_eq!(
            text(&machine.run_rows()),
            ["[Create machine · Lab machine]"]
        );
    }

    #[test]
    fn resources_are_the_machine_and_switching_the_provisioner_changes_them() {
        let mut machine = without_machine(vec![
            sized(
                "incus/vm",
                "Incus virtual machine",
                &["cpus", "memory", "disk"],
            ),
            sized("docker/container", "Docker container", &["cpus", "memory"]),
        ]);
        assert_eq!(
            text(&machine.settings_rows()),
            [
                "Machine",
                "  ├─ cpus  1",
                "  ├─ memory  1",
                "  └─ disk  1",
                "Provisioner Options",
                "  └─ Image  ubuntu"
            ]
        );
        // Enter on the row switches to the next, and around again.
        assert_eq!(machine.enter(Pick::Provisioner, "studio"), Next::Nothing);
        assert_eq!(machine.chosen.as_deref(), Some("docker/container"));
        assert!(text(&machine.install_rows())[0].contains("Docker container"));
        let machine_rows = text(&machine.settings_rows());
        assert_eq!(
            machine_rows[..3],
            ["Machine", "  ├─ cpus  1", "  └─ memory  1"]
        );
        // The Suggestions panel lists them; the help says what the keys do.
        assert_eq!(
            machine.choices(),
            [
                ("Incus virtual machine".to_owned(), "incus/vm".to_owned()),
                ("Docker container".to_owned(), "docker/container".to_owned())
            ]
        );
        let help = machine.help(Pick::Provisioner);
        assert!(help.contains("Space or Enter switches to the next provisioner"));
        assert!(!help.contains("Incus virtual machine"));
        // One chosen from the suggestions comes by its id.
        machine.typed(Pick::Provisioner, "incus/vm", "studio");
        assert_eq!(machine.chosen.as_deref(), Some("incus/vm"));
        machine.choose_next();
        assert_eq!(machine.chosen.as_deref(), Some("docker/container"));
        machine.choose_next();
        assert_eq!(machine.chosen.as_deref(), Some("incus/vm"));
    }

    #[test]
    fn a_bad_resource_is_a_problem_of_the_settings_tab_and_blocks_creating() {
        let mut machine = without_machine(vec![sized(
            "incus/vm",
            "Incus virtual machine",
            &["cpus", "memory", "disk"],
        )]);
        machine.typed(Pick::Argument(0), "https://192.0.2.10:8443", "studio");
        for (index, value) in [(1, "4"), (2, "8GiB"), (3, "60g")] {
            machine.typed(Pick::Argument(index), value, "studio");
        }
        assert!(machine.problems().is_empty());
        machine.typed(Pick::Argument(1), "four", "studio");
        machine.typed(Pick::Argument(2), "8", "studio");
        assert_eq!(
            texts(machine.problems()),
            [
                (
                    Step::Settings,
                    "cpus must be a positive number, such as 4".into()
                ),
                (
                    Step::Settings,
                    "memory must be a size with its unit, such as 8GiB, 512MiB or 4g".into()
                ),
            ]
        );
        assert!(
            matches!(machine.enter(Pick::Create, "studio"), Next::Say(problem) if problem.contains("cpus"))
        );
        // An optional one left empty takes the provisioner's own default.
        machine.typed(Pick::Argument(1), "", "studio");
        machine.typed(Pick::Argument(2), "512MiB", "studio");
        assert!(machine.problems().is_empty());
        assert!(valid_count("1.5") && !valid_count("0") && !valid_count("-2"));
        assert!(valid_size("4g") && valid_size("512MiB") && !valid_size("GiB"));
    }

    #[test]
    fn creating_asks_once_with_what_was_filled_in() {
        let mut machine = without_machine(vec![provisioner(&[])]);
        assert_eq!(
            machine.enter(Pick::Create, "studio"),
            Next::Say("Lab server is required".into())
        );
        assert!(machine.question.is_none());
        machine.typed(Pick::Argument(0), " https://192.0.2.10:8443 ", "studio");
        machine.typed(Pick::Argument(1), "8", "studio");
        assert!(machine.problems().is_empty());
        assert_eq!(machine.enter(Pick::Create, "studio"), Next::Confirm);
        assert_eq!(
            machine.question.as_deref(),
            Some("Create a machine for studio with Lab machine?")
        );
        let Next::Create(asked) = machine.answer(true) else {
            panic!("not created");
        };
        assert_eq!(asked.provisioner, "lab/vm");
        assert_eq!(asked.arguments["server"], "https://192.0.2.10:8443");
        assert_eq!(asked.arguments["cpus"], "8");
        // The question is gone, and no is no.
        assert_eq!(machine.answer(true), Next::Nothing);
        machine.enter(Pick::Create, "studio");
        assert_eq!(machine.answer(false), Next::Nothing);
        assert!(machine.question.is_none());
    }

    #[test]
    fn missing_secrets_are_a_problem_of_the_install_tab() {
        let machine = without_machine(vec![provisioner(&["lab_token"])]);
        let problems = texts(machine.problems());
        assert_eq!(problems[0].0, Step::Install);
        assert!(
            problems[0]
                .1
                .contains("Needs the secret lab_token: berth secret set NAME, or the Secrets tab")
        );
        assert!(machine.request().is_err());
        assert!(
            machine
                .help(Pick::Provisioner)
                .contains("Not set yet: lab_token")
        );
    }

    #[test]
    fn a_machine_offers_what_its_provisioner_can_do() {
        let mut instance = Resource {
            provisioner: "lab/vm".into(),
            id: "r-studio".into(),
            external_id: "studio".into(),
            state: "running".into(),
            address: "192.0.2.20".into(),
            verbs: vec!["start".into(), "stop".into()],
            ..Default::default()
        };
        assert_eq!(
            Machine::actions(&instance),
            [Pick::Start, Pick::Stop, Pick::Delete]
        );
        instance.verbs.clear();
        instance.state = "unknown".into();
        instance.problem = "lab/vm status said no state".into();
        assert_eq!(Machine::actions(&instance), [Pick::Delete]);
        let mut machine = Machine::default();
        machine.asking();
        machine.listed(Ok(vec![provisioner(&[])]));
        machine.found(Ok(Some(instance)));
        // The machine is on the Run tab; there is nothing to choose or size.
        assert!(picks(&machine.install_rows()).is_empty());
        assert!(machine.settings_rows().is_empty() && machine.problems().is_empty());
        let rows = machine.run_rows();
        let shown = text(&rows);
        assert_eq!(shown[0], "Machine");
        assert!(shown.contains(&"  State  unknown".to_owned()));
        assert!(shown.contains(&"  Address  192.0.2.20".to_owned()));
        assert!(shown.contains(&"  ! lab/vm status said no state".to_owned()));
        assert_eq!(picks(&rows), [Pick::Delete]);
        // Deleting wants the host's name typed out.
        assert_eq!(machine.enter(Pick::Delete, "studio"), Next::Edit("".into()));
        assert!(matches!(
            machine.typed(Pick::Delete, "studi", "studio"),
            Next::Say(_)
        ));
        assert_eq!(
            machine.typed(Pick::Delete, "studio", "studio"),
            Next::Destroy
        );
        assert_eq!(machine.enter(Pick::Stop, "studio"), Next::Power("stop"));
        assert!(editor_title(Pick::Delete, "studio").contains("Type studio"));
    }

    #[test]
    fn only_a_missing_machine_is_no_machine() {
        assert!(absent("studio has no machine that berth created"));
        assert!(absent("No host studio"));
        assert!(!absent("The berth server did not answer"));
        // An error asking for it is in the way of creating it.
        let mut machine = Machine::default();
        machine.asking();
        machine.listed(Ok(vec![provisioner(&[])]));
        machine.found(Err("The berth server did not answer".into()));
        assert!(
            texts(machine.problems())
                .contains(&(Step::Run, "The berth server did not answer".into()))
        );
    }

    #[test]
    fn an_argument_of_a_few_values_is_a_choice_and_its_help_is_shown() {
        let mut lab = provisioner(&[]);
        lab.arguments.push(Argument {
            name: "keep_volume".into(),
            label: "Keep volume on destroy".into(),
            default: "no".into(),
            values: vec!["no".into(), "yes".into()],
            help: "Whether deleting the machine keeps its volume.".into(),
            ..Default::default()
        });
        let mut machine = without_machine(vec![lab]);
        assert_eq!(
            machine.argument_choices(2),
            [
                ("no".to_owned(), "no".to_owned()),
                ("yes".to_owned(), "yes".to_owned())
            ]
        );
        assert!(machine.argument_choices(1).is_empty());
        // Enter goes on to the next value and around, as on a choice field.
        assert_eq!(machine.enter(Pick::Argument(2), "studio"), Next::Nothing);
        assert_eq!(machine.value(2), Some("yes"));
        machine.enter(Pick::Argument(2), "studio");
        assert_eq!(machine.value(2), Some("no"));
        // The label is a name; what it means is in the help.
        let help = machine.help(Pick::Argument(2));
        assert!(help.starts_with("Keep volume on destroy (`keep_volume`). Whether deleting"));
        assert!(help.contains("Space or Enter switches to the next value"));
        assert!(
            machine
                .help(Pick::Argument(1))
                .ends_with("Enter changes it.")
        );
    }
}
