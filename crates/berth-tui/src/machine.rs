//! The machine berth creates for a host with a provisioner of a plugin, as
//! the host's Install tab shows it: what there is to create it with, what the
//! chosen one asks for, and the machine once there is one. berth and its page
//! in a browser ask for these and run the jobs each their own way.

use crate::fields::Kind;
use crate::setup::{Entry, Row, value_line};
use crate::theme;
use berth_api::{Instance, Provision, ProvisionerInfo};
use ratatui::{
    style::Style,
    text::{Line, Span},
};
use std::collections::BTreeMap;

/// A row of the machine's part of the Install tab that does something.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pick {
    /// A provisioner to create the machine with, by index into those there are.
    Provisioner(usize),
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
    Create(Provision),
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
    /// The machine berth created for the host; `None` inside when it has none.
    pub instance: Option<Result<Option<Instance>, String>>,
    /// The chosen provisioner, by `PLUGIN/ID`.
    pub chosen: Option<String>,
    /// What its arguments are given, by name.
    pub values: BTreeMap<String, String>,
    /// The question asked before creating, while it is open.
    pub question: Option<String>,
}

/// Whether an error of `instances.get` only says there is no machine: the
/// host has none, or the server has no such host yet.
pub fn absent(error: &str) -> bool {
    error.contains("has no machine that berth created") || error.starts_with("No host ")
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

impl Machine {
    /// Forget what was known, before asking again. The form stays.
    pub fn asking(&mut self) {
        self.asked = true;
        self.provisioners = None;
        self.instance = None;
    }

    pub fn listed(&mut self, provisioners: Result<Vec<ProvisionerInfo>, String>) {
        self.provisioners = Some(provisioners);
        // One that is gone takes its form with it.
        if self.provisioner().is_none() {
            self.chosen = None;
            self.values.clear();
        }
    }

    pub fn found(&mut self, instance: Result<Option<Instance>, String>) {
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
    pub fn machine(&self) -> Option<&Instance> {
        match &self.instance {
            Some(Ok(Some(instance))) => Some(instance),
            _ => None,
        }
    }

    /// Choose a provisioner: its form starts from the defaults.
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
            .map(|argument| (argument.name.clone(), argument.default.clone()))
            .collect();
        self.chosen = Some(provisioner.id.clone());
        self.values = values;
    }

    /// Why creating with the chosen provisioner would be refused.
    pub fn problems(&self) -> Vec<String> {
        let Some(provisioner) = self.provisioner() else {
            return vec![];
        };
        let mut problems = vec![];
        if !provisioner.missing_secrets.is_empty() {
            problems.push(format!(
                "Needs the secret{} {}: berth secret set NAME, or the Secrets tab of the home screen",
                if provisioner.missing_secrets.len() == 1 {
                    ""
                } else {
                    "s"
                },
                provisioner.missing_secrets.join(", ")
            ));
        }
        for argument in &provisioner.arguments {
            let value = self.values.get(&argument.name).map_or("", String::as_str);
            if argument.required && value.trim().is_empty() {
                problems.push(format!("{} is required", label(argument)));
            }
        }
        problems
    }

    /// What creating the machine asks for, once nothing is in the way.
    pub fn request(&self) -> Result<Provision, String> {
        let provisioner = self.provisioner().ok_or("Choose a provisioner first")?;
        if let Some(problem) = self.problems().into_iter().next() {
            return Err(problem);
        }
        let arguments = provisioner
            .arguments
            .iter()
            .map(|argument| {
                let value = self.values.get(&argument.name).cloned().unwrap_or_default();
                (argument.name.clone(), value.trim().to_owned())
            })
            .collect();
        Ok(Provision {
            provisioner: provisioner.id.clone(),
            arguments,
        })
    }

    /// The actions the machine offers, beside what it is.
    pub fn actions(instance: &Instance) -> Vec<Pick> {
        let mut picks = vec![];
        for (verb, pick) in [("start", Pick::Start), ("stop", Pick::Stop)] {
            if instance.verbs.iter().any(|known| known == verb) {
                picks.push(pick);
            }
        }
        picks.push(Pick::Delete);
        picks
    }

    /// Whether berth created a machine for the host, which the Install tab
    /// then shows whatever way of installing is chosen.
    pub fn exists(&self) -> bool {
        matches!(self.instance, Some(Ok(Some(_))))
    }

    /// The rows of the machine's part of the Install tab; none before
    /// anything was asked.
    pub fn rows(&self) -> Vec<Row> {
        if !self.asked {
            return vec![];
        }
        let muted = Style::new().fg(theme::MUTED);
        let bad = |text: String| Row::info(Line::styled(text, Style::new().fg(theme::BAD)));
        let mut rows = vec![];
        let (Some(provisioners), Some(instance)) = (&self.provisioners, &self.instance) else {
            rows.push(Row::heading("Machine"));
            rows.push(Row::info(Line::styled(
                "  Asking for the machine and the provisioners…",
                muted,
            )));
            return rows;
        };
        match instance {
            Err(error) => {
                rows.push(Row::heading("Machine"));
                rows.push(bad(format!("  ✗ {error}")));
            }
            Ok(Some(instance)) => {
                rows.push(Row::heading("Machine"));
                let line =
                    |label: &str, value: &str| Row::info(value_line(label, value, Kind::Text));
                rows.push(line("  Created with", &instance.provisioner));
                rows.push(line("  Machine", &instance.id));
                let colour = match instance.state.as_str() {
                    "running" => theme::GOOD,
                    "gone" => theme::BAD,
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
            }
            Ok(None) => {
                rows.push(Row::heading("Provisioner"));
                match provisioners {
                    Err(error) => rows.push(bad(format!("  ✗ {error}"))),
                    Ok(all) if all.is_empty() => rows.push(Row::info(Line::styled(
                        "  No provisioner is installed. Plugins bring them.",
                        muted,
                    ))),
                    Ok(all) => self.form_rows(all, &mut rows),
                }
            }
        }
        rows
    }

    fn form_rows(&self, all: &[ProvisionerInfo], rows: &mut Vec<Row>) {
        let muted = Style::new().fg(theme::MUTED);
        for (index, provisioner) in all.iter().enumerate() {
            let chosen = self.chosen.as_ref() == Some(&provisioner.id);
            rows.push(Row::entry(
                Line::from(vec![
                    Span::raw(format!(
                        "  {} {}",
                        if chosen { "●" } else { "○" },
                        provisioner.label
                    )),
                    Span::styled(format!("  · {}", setup_note(provisioner)), muted),
                ]),
                Entry::Machine(Pick::Provisioner(index)),
            ));
            if chosen {
                self.chosen_rows(provisioner, rows);
            }
        }
    }

    /// The form of the chosen provisioner, under it.
    fn chosen_rows(&self, provisioner: &ProvisionerInfo, rows: &mut Vec<Row>) {
        let muted = Style::new().fg(theme::MUTED);
        let bad = |problem: String| {
            Row::info(Line::styled(
                format!("    ✗ {problem}"),
                Style::new().fg(theme::BAD),
            ))
        };
        let mut problems = self.problems().into_iter();
        // The secrets first: without them nothing below can be done.
        if !provisioner.missing_secrets.is_empty() {
            rows.extend(problems.next().map(bad));
        }
        let count = provisioner.arguments.len();
        for (index, argument) in provisioner.arguments.iter().enumerate() {
            let branch = if index + 1 < count {
                "    ├─ "
            } else {
                "    └─ "
            };
            let name = match argument.required {
                true => format!("{} *", label(argument)),
                false => label(argument).to_owned(),
            };
            let value = self.values.get(&argument.name).map_or("", String::as_str);
            let mut line = value_line(&name, value, Kind::Text);
            line.spans.insert(0, Span::styled(branch, muted));
            rows.push(Row::entry(line, Entry::Machine(Pick::Argument(index))));
        }
        rows.extend(problems.map(bad));
        // Without its secrets the provisioner is refused, so it is not offered.
        if provisioner.missing_secrets.is_empty() {
            rows.push(Row::entry(
                format!("    [Create machine · {}]", provisioner.label),
                Entry::Machine(Pick::Create),
            ));
        }
    }

    /// What the information box says of a row.
    pub fn help(&self, pick: Pick) -> String {
        match pick {
            Pick::Provisioner(index) => {
                let Some(provisioner) = self.all().get(index) else {
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
                text + "\n\nEnter chooses it and shows what it asks for."
            }
            Pick::Argument(index) => {
                let Some(argument) = self
                    .provisioner()
                    .and_then(|provisioner| provisioner.arguments.get(index))
                else {
                    return String::new();
                };
                let mut text = format!("{} (`{}`).", label(argument), argument.name);
                if argument.required {
                    text += " Required.";
                }
                if !argument.default.is_empty() {
                    text += &format!(" Default: {}.", argument.default);
                }
                text + " Enter changes it."
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
            Pick::Provisioner(index) => {
                self.choose(index);
                Next::Nothing
            }
            Pick::Argument(index) => {
                let value = self
                    .provisioner()
                    .and_then(|provisioner| provisioner.arguments.get(index))
                    .and_then(|argument| self.values.get(&argument.name))
                    .cloned()
                    .unwrap_or_default();
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

    #[test]
    fn nothing_shows_before_it_was_asked_and_little_while_it_is() {
        let mut machine = Machine::default();
        assert!(machine.rows().is_empty());
        machine.asking();
        assert!(text(&machine.rows())[1].contains("Asking"));
        let none = without_machine(vec![]);
        let shown = text(&none.rows());
        assert_eq!(shown.len(), 2);
        assert!(shown[1].contains("No provisioner is installed"));
        assert!(picks(&none.rows()).is_empty());
    }

    #[test]
    fn a_chosen_provisioner_is_a_form_from_its_arguments() {
        let mut machine = without_machine(vec![provisioner(&[])]);
        assert_eq!(picks(&machine.rows()), [Pick::Provisioner(0)]);
        assert!(text(&machine.rows())[1].contains("sets itself up from the cloud seed"));
        assert_eq!(machine.enter(Pick::Provisioner(0), "studio"), Next::Nothing);
        let rows = machine.rows();
        assert_eq!(
            picks(&rows),
            [
                Pick::Provisioner(0),
                Pick::Argument(0),
                Pick::Argument(1),
                Pick::Create
            ]
        );
        let shown = text(&rows);
        assert!(shown[1].starts_with("  ● Lab machine"), "{shown:?}");
        // The required one is marked and empty; the other has its default.
        assert_eq!(shown[2], "    ├─ Lab server *  Not set");
        assert_eq!(shown[3], "    └─ vCPUs  4");
        assert!(shown.contains(&"    ✗ Lab server is required".to_owned()));
        assert_eq!(
            machine.enter(Pick::Argument(1), "studio"),
            Next::Edit("4".into())
        );
    }

    #[test]
    fn creating_asks_once_with_what_was_filled_in() {
        let mut machine = without_machine(vec![provisioner(&[])]);
        machine.choose(0);
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
    fn missing_secrets_are_named_and_creating_is_not_offered() {
        let mut machine = without_machine(vec![provisioner(&["lab_token"])]);
        machine.choose(0);
        let rows = machine.rows();
        assert!(!picks(&rows).contains(&Pick::Create));
        let shown = text(&rows).join("\n");
        assert!(
            shown.contains("Needs the secret lab_token: berth secret set NAME, or the Secrets tab")
        );
        assert!(machine.request().is_err());
        assert!(
            machine
                .help(Pick::Provisioner(0))
                .contains("Not set yet: lab_token")
        );
    }

    #[test]
    fn a_machine_offers_what_its_provisioner_can_do() {
        let mut instance = Instance {
            provisioner: "lab/vm".into(),
            id: "studio".into(),
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
        let rows = machine.rows();
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
    }
}
