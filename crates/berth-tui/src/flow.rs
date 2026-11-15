//! The set-up of a new host as questions. It walks the tabs of the screens
//! in their order, Install, Features, Settings and Run, with the same rows:
//! what [`Profile::fields`] shows on each, written with [`setup::commit`]
//! and [`setup::set_feature`], and stopped by the same [`Problem`]s. What
//! the command line was given is taken first; whoever runs the flow is asked
//! for the rest, one [`Question`] at a time, or names the flag that gives it.

use crate::fields::{Action, Field, Kind, Step};
use crate::machine::Machine;
use crate::setup::{self, Feature, Problem, Profile};
use serde_json::{Value, json};

/// Something to choose from.
#[derive(Clone, Debug, PartialEq)]
pub struct Choice {
    /// What it is called on the command line.
    pub key: String,
    pub label: String,
    pub help: String,
}

impl Choice {
    fn new(key: &str, label: &str, help: &str) -> Choice {
        Choice {
            key: key.to_owned(),
            label: label.to_owned(),
            help: help.to_owned(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Asked {
    Text {
        default: String,
        secret: bool,
    },
    Choice {
        options: Vec<Choice>,
        default: usize,
    },
    Many {
        options: Vec<Choice>,
        chosen: Vec<bool>,
    },
    Confirm {
        default: bool,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Answer {
    Text(String),
    Choice(usize),
    Many(Vec<bool>),
    Confirm(bool),
}

/// One thing the flow wants to know.
#[derive(Clone, Debug, PartialEq)]
pub struct Question {
    /// The tab it belongs to, as the screens show it.
    pub step: Step,
    pub label: String,
    pub help: String,
    /// How to give it without being asked, such as `--set hostname=NAME`.
    pub flag: String,
    pub asked: Asked,
    /// Whether nothing can go on without it. A question that is not
    /// required keeps its default when nobody can be asked.
    pub required: bool,
}

/// Whoever answers: a person at a terminal, a list of answers in a test,
/// or nobody.
pub trait Asker {
    /// The answer to `question`, or `None` when nobody can be asked.
    fn ask(&mut self, question: &Question) -> Result<Option<Answer>, String>;
}

/// An asker for when nobody is there: every question keeps its default.
pub struct Nobody;

impl Asker for Nobody {
    fn ask(&mut self, _: &Question) -> Result<Option<Answer>, String> {
        Ok(None)
    }
}

/// What the server has for the set-up of a new host.
#[derive(Clone, Debug, Default)]
pub struct Catalogue {
    /// What a new host starts from.
    pub template: Value,
    pub features: Vec<Feature>,
    /// The names of the secrets the server keeps.
    pub secrets: Vec<String>,
    /// The server's own SSH public key.
    pub own_key: String,
    /// The provisioners, for a host one of them makes.
    pub provisioners: Vec<berth_api::ProvisionerInfo>,
}

/// What the command line gave, before anyone is asked.
#[derive(Clone, Debug, Default)]
pub struct Given {
    pub method: Option<Action>,
    /// Fields by their keys, in the order given.
    pub values: Vec<(Field, String)>,
    /// Features switched on or off, by `PLUGIN/FEATURE`.
    pub features: Vec<(String, bool)>,
    /// Secrets and registries the host is given, as `secret.NAME` and
    /// `registry.NAME` say.
    pub secrets: Vec<(String, String)>,
    pub registries: Vec<(String, bool)>,
    pub generate_passwords: bool,
    pub provisioner: Option<String>,
    /// The provisioner's arguments.
    pub arguments: Vec<(String, String)>,
}

/// What a word `KEY=VALUE` of the command line sets.
#[derive(Clone, Debug, PartialEq)]
pub enum Setting {
    Field(Field, String),
    Feature(String, bool),
    Secret(String, String),
    Registry(String, bool),
}

/// The keys `--set` and `berth host set` take, with what each is.
pub fn keys() -> Vec<(String, &'static str, &'static str)> {
    Field::ALL
        .iter()
        .map(|field| {
            let spec = field.spec();
            (field.key(), spec.label, spec.help)
        })
        .collect()
}

fn switch(value: &str) -> Option<bool> {
    match value {
        "on" | "true" | "yes" => Some(true),
        "off" | "false" | "no" => Some(false),
        _ => None,
    }
}

/// Read one `KEY=VALUE`.
pub fn setting(word: &str) -> Result<Setting, String> {
    let (key, value) = word
        .split_once('=')
        .ok_or_else(|| format!("{word} is no KEY=VALUE"))?;
    let on = || switch(value).ok_or_else(|| format!("{key} is on or off, not {value}"));
    if let Some(feature) = key.strip_prefix("feature.") {
        return Ok(Setting::Feature(feature.to_owned(), on()?));
    }
    if let Some(secret) = key.strip_prefix("secret.") {
        return Ok(Setting::Secret(secret.to_owned(), value.to_owned()));
    }
    if let Some(registry) = key.strip_prefix("registry.") {
        return Ok(Setting::Registry(registry.to_owned(), on()?));
    }
    let field = Field::by_key(key).ok_or_else(|| {
        format!("{key} is no field of a profile. berth host create --help lists them")
    })?;
    // A switch is given as on or off, as the screens show it.
    let value = match field.spec().kind {
        Kind::Toggle => on()?.to_string(),
        _ => value.to_owned(),
    };
    Ok(Setting::Field(field, value))
}

impl Given {
    /// Take in `KEY=VALUE` words.
    pub fn set(&mut self, words: &[String]) -> Result<(), String> {
        for word in words {
            match setting(word)? {
                Setting::Field(field, value) => self.values.push((field, value)),
                Setting::Feature(key, on) => self.features.push((key, on)),
                Setting::Secret(name, how) => self.secrets.push((name, how)),
                Setting::Registry(name, on) => self.registries.push((name, on)),
            }
        }
        Ok(())
    }

    fn has(&self, field: Field) -> bool {
        self.values.iter().any(|(given, _)| *given == field)
    }
}

/// Write `value` into a field of the profile, as the screens' row does.
pub fn write(config: &mut Value, field: Field, value: &str) -> Result<(), String> {
    match field.spec().kind {
        Kind::Toggle => {
            let on = switch(value).ok_or_else(|| format!("{} is on or off", field.key()))?;
            setup::set_pointer(config, field.spec().pointer, json!(on));
            Ok(())
        }
        _ => setup::commit(config, field, value),
    }
}

/// Apply one setting to a profile, with the features there are.
pub fn apply(config: &mut Value, features: &[Feature], setting: &Setting) -> Result<(), String> {
    match setting {
        Setting::Field(field, value) => write(config, *field, value),
        Setting::Feature(key, on) => {
            let index = features
                .iter()
                .position(|feature| &feature.key == key)
                .ok_or_else(|| format!("No plugin has the feature {key}"))?;
            let host_type = berth_api::profile::host_type(config);
            let fits = match host_type {
                berth_api::HostType::Node => features[index].fits(host_type),
                _ => features[index].fits(berth_api::HostType::Workstation),
            };
            if *on && !fits {
                return Err(format!(
                    "The feature {key} is not for a host of the type {host_type}"
                ));
            }
            setup::set_feature(config, features, index, *on);
            Ok(())
        }
        Setting::Secret(name, how) => {
            // A file may say its mode after it, as PATH:0640.
            let (how, mode) = match how.rsplit_once(':') {
                Some((path, mode))
                    if (3..=4).contains(&mode.len())
                        && mode.chars().all(|ch| ('0'..='7').contains(&ch)) =>
                {
                    (path, Some(mode))
                }
                _ => (how.as_str(), None),
            };
            setup::set_secret(config, name, how).map_err(|why| format!("secret.{name}: {why}"))?;
            if let Some(mode) = mode
                && let Some(entry) = config["secrets"]
                    .as_array_mut()
                    .and_then(|secrets| {
                        secrets
                            .iter_mut()
                            .find(|entry| entry["name"] == name.as_str())
                    })
                    .filter(|entry| entry["file"].is_string())
            {
                entry["mode"] = json!(mode);
            }
            Ok(())
        }
        Setting::Registry(name, on) => {
            let has = config["registries"]
                .as_array()
                .is_some_and(|names| names.iter().any(|known| known == name.as_str()));
            if has != *on {
                setup::toggle_registry(config, name);
            }
            Ok(())
        }
    }
}

/// The flag that gives a field.
pub fn flag_of(field: Field) -> String {
    let value = match field.spec().kind {
        Kind::Toggle => "on|off".to_owned(),
        Kind::Choice(choices) => choices.join("|"),
        _ => field.spec().label.to_uppercase().replace(' ', "_"),
    };
    format!("--set {}={value}", field.key())
}

/// What makes a password hash: a password made up, or one typed, and the
/// account it is for.
pub enum Hash<'a> {
    Generate { user: &'a str },
    Of(&'a str),
}

/// The server's part of the flow: what only it can make.
pub trait Maker {
    /// A password hash for an account of the host.
    fn hash(&mut self, host: &str, hash: Hash) -> Result<String, String>;
}

/// The set-up of one new host.
pub struct Flow {
    pub name: String,
    pub config: Value,
    pub action: Action,
    pub catalogue: Catalogue,
    pub machine: Machine,
}

/// Why the flow stopped.
#[derive(Debug, PartialEq)]
pub enum Stop {
    /// A question that has to be answered, and nobody could be asked: its flag.
    Missing(Box<Question>),
    /// What still stops it, each on its tab.
    Problems(Vec<Problem>),
    /// A value was refused, or the asker gave up.
    Refused(String),
}

/// The ways offered, as choices.
fn method_choices() -> Vec<Choice> {
    Action::SERVED
        .iter()
        .map(|action| Choice::new(action.key(), action.label(), action.help()))
        .collect()
}

impl Flow {
    /// A new host from the server's starting profile, as the screens begin one.
    pub fn new(name: &str, catalogue: Catalogue) -> Flow {
        let mut config = catalogue.template.clone();
        if !config.is_object() {
            config = json!({});
        }
        let mut machine = Machine::default();
        machine.asking();
        machine.listed(Ok(catalogue.provisioners.clone()));
        machine.found(Ok(None));
        let mut flow = Flow {
            name: name.to_owned(),
            config,
            action: Action::Machine,
            catalogue,
            machine,
        };
        setup::fit_target(&mut flow.config, flow.action);
        setup::default_enrollment(&mut flow.config, &flow.catalogue.secrets.clone());
        flow
    }

    /// The profile as the screens' tab `step` sees it.
    pub fn view(&self, step: Step) -> Profile<'_> {
        Profile {
            config: &self.config,
            step,
            action: self.action,
            features: &self.catalogue.features,
            registries: &[],
            secrets: &[],
            own_key: &self.catalogue.own_key,
            named: setup::valid_hostname(&self.name),
            tailnet_key: self
                .catalogue
                .secrets
                .iter()
                .any(|name| name == berth_api::profile::TAILNET_API_KEY),
        }
    }

    /// Choose the way it is installed, as a row of the Install tab does.
    pub fn choose(&mut self, action: Action) {
        self.action = action.choose(&mut self.config);
        setup::fit_target(&mut self.config, self.action);
    }

    /// Everything that stops it, as the Run tab lists it.
    pub fn problems(&self) -> Vec<Problem> {
        let mut problems = self.view(Step::Run).problems_with(&self.machine);
        if self.action == Action::Machine {
            problems.retain(|problem| {
                // An argument given later is no problem of the flow.
                problem.field.is_some() || problem.step != Step::Settings
            });
        }
        problems
    }

    fn question(&self, step: Step, field: Field, required: bool) -> Question {
        let spec = field.spec();
        let view = self.view(step);
        let asked = match spec.kind {
            Kind::Toggle => Asked::Confirm {
                default: view.enabled(field),
            },
            Kind::Choice(choices) => {
                let current = view.text(field);
                Asked::Choice {
                    options: choices
                        .iter()
                        .map(|choice| Choice::new(choice, choice, ""))
                        .collect(),
                    default: choices
                        .iter()
                        .position(|choice| *choice == current)
                        .unwrap_or(0),
                }
            }
            Kind::Secret => Asked::Text {
                default: String::new(),
                secret: true,
            },
            Kind::Text | Kind::Path => Asked::Text {
                default: view.text(field).to_owned(),
                secret: false,
            },
        };
        Question {
            step,
            label: spec.label.to_owned(),
            help: spec.help.to_owned(),
            flag: flag_of(field),
            asked,
            required,
        }
    }

    /// Ask a question: the answer, or `None` when its default stays.
    fn put(asker: &mut dyn Asker, question: Question) -> Result<Option<Answer>, Stop> {
        match asker.ask(&question).map_err(Stop::Refused)? {
            Some(answer) => Ok(Some(answer)),
            None if question.required => Err(Stop::Missing(Box::new(question))),
            None => Ok(None),
        }
    }

    /// Ask for a field and write the answer into the profile, asking again
    /// while it is refused.
    fn ask_field(
        &mut self,
        asker: &mut dyn Asker,
        step: Step,
        field: Field,
        required: bool,
    ) -> Result<(), Stop> {
        for _ in 0..3 {
            let question = self.question(step, field, required);
            let options = match &question.asked {
                Asked::Choice { options, .. } => options.clone(),
                _ => vec![],
            };
            let value = match Flow::put(asker, question)? {
                None => return Ok(()),
                Some(Answer::Text(text)) => text,
                Some(Answer::Confirm(on)) => if on { "on" } else { "off" }.to_owned(),
                Some(Answer::Choice(index)) => options
                    .get(index)
                    .map(|choice| choice.key.clone())
                    .unwrap_or_default(),
                Some(Answer::Many(_)) => return Err(Stop::Refused("Not a list".into())),
            };
            match write(&mut self.config, field, &value) {
                Ok(()) => return Ok(()),
                Err(why) if required => {
                    let _ = why;
                }
                Err(why) => return Err(Stop::Refused(why)),
            }
        }
        Err(Stop::Refused(format!(
            "{} was not taken",
            field.spec().label
        )))
    }

    /// Run the flow: take what is given, then ask the tabs' questions in
    /// their order, and what is still a problem at the end.
    pub fn run(
        &mut self,
        given: &Given,
        asker: &mut dyn Asker,
        maker: &mut dyn Maker,
    ) -> Result<(), Stop> {
        // Install: the way first, which decides what the tabs hold.
        let method = match (given.method, &given.provisioner) {
            (Some(method), _) => Some(method),
            (None, Some(_)) => Some(Action::Machine),
            // A node is only ever applied over SSH.
            (None, None) if given.values.contains(&(Field::Type, "node".into())) => {
                Some(Action::Ssh)
            }
            (None, None) if given.has(Field::SshHost) => Some(Action::Ssh),
            _ => None,
        };
        match method {
            Some(method) => self.choose(method),
            None => {
                let question = Question {
                    step: Step::Install,
                    label: "How it is installed".into(),
                    help: "The ways the server installs a host.".into(),
                    flag: "--method METHOD".into(),
                    asked: Asked::Choice {
                        options: method_choices(),
                        default: 0,
                    },
                    required: true,
                };
                let chosen = match Flow::put(asker, question)? {
                    Some(Answer::Choice(index)) => Action::SERVED.get(index).copied(),
                    _ => None,
                };
                self.choose(chosen.unwrap_or(Action::Machine));
            }
        }
        // What the command line says comes before anything is asked.
        for (field, value) in &given.values {
            write(&mut self.config, *field, value)
                .map_err(|why| Stop::Refused(format!("{}: {why}", field.key())))?;
        }
        for (key, on) in &given.features {
            apply(
                &mut self.config,
                &self.catalogue.features.clone(),
                &Setting::Feature(key.clone(), *on),
            )
            .map_err(Stop::Refused)?;
        }
        for (name, how) in &given.secrets {
            setup::set_secret(&mut self.config, name, how)
                .map_err(|why| Stop::Refused(format!("secret.{name}: {why}")))?;
        }
        for (name, on) in &given.registries {
            apply(&mut self.config, &[], &Setting::Registry(name.clone(), *on))
                .map_err(Stop::Refused)?;
        }
        if self.action == Action::Machine {
            self.machine_questions(given, asker)?;
        }
        for step in [Step::Install, Step::Settings] {
            for field in self.view(step).fields() {
                if field.asked() && !given.has(field) {
                    self.ask_field(asker, step, field, false)?;
                }
            }
            if step == Step::Install && given.features.is_empty() {
                self.feature_question(asker)?;
            }
        }
        self.passwords(given, asker, maker)?;
        // What is still in the way is asked for, field by field.
        for _ in 0..3 {
            let open: Vec<Problem> = self
                .problems()
                .into_iter()
                .filter(|problem| problem.field.is_some_and(|field| !given.has(field)))
                .collect();
            let Some(problem) = open.first() else {
                break;
            };
            let field = problem.field.expect("only problems of fields are left");
            match field {
                _ if field.is_password_hash() => self.password(field, true, asker, maker)?,
                // An account's keys are added one by one, with berth host set.
                Field::OperatorKey | Field::AgentKey => break,
                _ => self.ask_field(asker, problem.step, field, true)?,
            }
            if self.problems().first() == Some(problem) {
                break;
            }
        }
        let problems = self.problems();
        match problems.is_empty() {
            true => Ok(()),
            false => Err(Stop::Problems(problems)),
        }
    }

    /// The provisioner and the arguments it requires, as the Install tab
    /// asks for them when a provisioner makes the host.
    pub fn machine_questions(&mut self, given: &Given, asker: &mut dyn Asker) -> Result<(), Stop> {
        match &given.provisioner {
            Some(name) => {
                if !self
                    .catalogue
                    .provisioners
                    .iter()
                    .any(|known| &known.id == name)
                {
                    return Err(Stop::Refused(format!(
                        "No provisioner {name}. berth provisioner list names them"
                    )));
                }
                self.machine.choose_id(name);
            }
            None if self.catalogue.provisioners.len() > 1 => {
                let options = self
                    .machine
                    .choices()
                    .into_iter()
                    .map(|(label, id)| Choice::new(&id, &label, ""))
                    .collect();
                let question = Question {
                    step: Step::Install,
                    label: "Provisioner".into(),
                    help: crate::fields::Action::Machine.help().into(),
                    flag: "--provisioner NAME".into(),
                    asked: Asked::Choice {
                        options,
                        default: 0,
                    },
                    required: true,
                };
                if let Some(Answer::Choice(index)) = Flow::put(asker, question)? {
                    self.machine.choose(index);
                }
            }
            None => {}
        }
        for (name, value) in &given.arguments {
            let known = self
                .machine
                .provisioner()
                .is_some_and(|chosen| chosen.arguments.iter().any(|known| &known.name == name));
            if !known {
                return Err(Stop::Refused(format!(
                    "The provisioner takes no argument {name}"
                )));
            }
            self.machine.values.insert(name.clone(), value.clone());
        }
        let arguments = self
            .machine
            .provisioner()
            .map(|chosen| chosen.arguments.clone())
            .unwrap_or_default();
        for argument in arguments {
            let value = self
                .machine
                .values
                .get(&argument.name)
                .cloned()
                .unwrap_or_default();
            if !argument.required || !value.trim().is_empty() {
                continue;
            }
            let label = match argument.label.as_str() {
                "" => argument.name.clone(),
                label => label.to_owned(),
            };
            let asked = match argument.values.is_empty() {
                true => Asked::Text {
                    default: argument.default.clone(),
                    secret: argument.secret,
                },
                false => Asked::Choice {
                    options: argument
                        .values
                        .iter()
                        .map(|value| Choice::new(value, value, ""))
                        .collect(),
                    default: 0,
                },
            };
            let question = Question {
                step: Step::Install,
                label,
                help: argument.help.clone(),
                flag: format!("{}=VALUE", argument.name),
                asked,
                required: true,
            };
            let value = match Flow::put(asker, question)? {
                Some(Answer::Text(text)) => text,
                Some(Answer::Choice(index)) => {
                    argument.values.get(index).cloned().unwrap_or_default()
                }
                _ => continue,
            };
            self.machine.values.insert(argument.name.clone(), value);
        }
        Ok(())
    }

    /// The Features tab as one list to tick: the plugins' features and
    /// berth's own things to install.
    fn feature_question(&mut self, asker: &mut dyn Asker) -> Result<(), Stop> {
        let view = self.view(Step::Features);
        let fields = view.fields();
        let chosen_features = setup::chosen_features(&self.config);
        let offered = view.offered_features();
        let mut options = vec![];
        let mut chosen = vec![];
        for index in &offered {
            let feature = &self.catalogue.features[*index];
            options.push(Choice::new(&feature.key, &feature.label, &feature.help));
            chosen.push(chosen_features.contains(&feature.key));
        }
        for field in &fields {
            let spec = field.spec();
            options.push(Choice::new(&field.key(), spec.label, spec.help));
            chosen.push(view.enabled(*field));
        }
        if options.is_empty() {
            return Ok(());
        }
        let question = Question {
            step: Step::Features,
            label: "Features".into(),
            help: setup::GROUP_HELP.into(),
            flag: "--feature PLUGIN/FEATURE".into(),
            asked: Asked::Many {
                options,
                chosen: chosen.clone(),
            },
            required: false,
        };
        let Some(Answer::Many(ticked)) = Flow::put(asker, question)? else {
            return Ok(());
        };
        let features = self.catalogue.features.clone();
        for (index, on) in ticked.iter().enumerate() {
            if chosen.get(index) == Some(on) {
                continue;
            }
            if let Some(feature) = offered.get(index) {
                setup::set_feature(&mut self.config, &features, *feature, *on);
            } else if let Some(field) = fields.get(index - offered.len()) {
                setup::set_pointer(&mut self.config, field.spec().pointer, json!(on));
            }
        }
        Ok(())
    }

    /// The accounts' passwords: generated, typed, or left empty.
    fn passwords(
        &mut self,
        given: &Given,
        asker: &mut dyn Asker,
        maker: &mut dyn Maker,
    ) -> Result<(), Stop> {
        let fields: Vec<Field> = self
            .view(Step::Settings)
            .fields()
            .into_iter()
            .filter(|field| field.is_password_hash() && !given.has(*field))
            .filter(|field| self.view(Step::Settings).text(*field).is_empty())
            .collect();
        if fields.is_empty() {
            return Ok(());
        }
        if given.generate_passwords {
            for field in fields {
                self.generate(field, maker)?;
            }
            return Ok(());
        }
        let question = Question {
            step: Step::Settings,
            label: "Passwords".into(),
            help: Field::OperatorHash.spec().help.into(),
            flag: "--generate-passwords".into(),
            asked: Asked::Choice {
                options: vec![
                    Choice::new(
                        "generate",
                        "generate them",
                        "berth host credentials NAME shows them later.",
                    ),
                    Choice::new("enter", "enter them", "Typed here, and kept as hashes."),
                    Choice::new("none", "none", "An empty password locks password login."),
                ],
                default: 0,
            },
            required: false,
        };
        match Flow::put(asker, question)? {
            Some(Answer::Choice(0)) => {
                for field in fields {
                    self.generate(field, maker)?;
                }
            }
            Some(Answer::Choice(1)) => {
                for field in fields {
                    self.password(field, false, asker, maker)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn user_of(&self, field: Field) -> String {
        let user = match field {
            Field::OperatorHash => Field::OperatorUser,
            _ => Field::AgentUser,
        };
        self.view(Step::Settings).text(user).to_owned()
    }

    fn generate(&mut self, field: Field, maker: &mut dyn Maker) -> Result<(), Stop> {
        let user = self.user_of(field);
        let hash = maker
            .hash(&self.name, Hash::Generate { user: &user })
            .map_err(Stop::Refused)?;
        setup::set_pointer(&mut self.config, field.spec().pointer, json!(hash));
        Ok(())
    }

    /// A typed password for one account, hashed by the server.
    fn password(
        &mut self,
        field: Field,
        required: bool,
        asker: &mut dyn Asker,
        maker: &mut dyn Maker,
    ) -> Result<(), Stop> {
        let user = self.user_of(field);
        let question = Question {
            step: Step::Settings,
            label: format!("Password of {user}"),
            help: field.spec().help.into(),
            flag: "--generate-passwords".into(),
            asked: Asked::Text {
                default: String::new(),
                secret: true,
            },
            required,
        };
        if let Some(Answer::Text(password)) = Flow::put(asker, question)?
            && !password.is_empty()
        {
            let hash = maker
                .hash(&self.name, Hash::Of(&password))
                .map_err(Stop::Refused)?;
            setup::set_pointer(&mut self.config, field.spec().pointer, json!(hash));
        }
        Ok(())
    }

    /// What creating the host with its provisioner asks the server for.
    pub fn request(&self) -> Result<berth_api::NewResource, String> {
        let mut request = self.machine.request()?;
        request.host = Some(self.name.clone());
        Ok(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Answers from a list, in the order the questions come, and the
    /// questions as they were asked.
    struct Script {
        answers: Vec<Answer>,
        asked: Vec<String>,
    }

    impl Asker for Script {
        fn ask(&mut self, question: &Question) -> Result<Option<Answer>, String> {
            self.asked.push(question.label.clone());
            Ok(match self.answers.is_empty() {
                true => None,
                false => Some(self.answers.remove(0)),
            })
        }
    }

    /// Hashes that say what they were made of.
    struct Hashes;

    impl Maker for Hashes {
        fn hash(&mut self, host: &str, hash: Hash) -> Result<String, String> {
            Ok(match hash {
                Hash::Generate { user } => {
                    format!("$6$made${host}{user}{}", "x".repeat(40))
                }
                Hash::Of(password) => format!("$6$typed${password}{}", "x".repeat(40)),
            })
        }
    }

    fn catalogue() -> Catalogue {
        Catalogue {
            template: json!({
                "target": "laptop",
                "operator_username": "ops",
                "agent_username": "agent",
                "builder": {},
                "ansible_vars": {}
            }),
            features: vec![
                Feature {
                    key: "claude/cli".into(),
                    label: "Claude Code".into(),
                    group: "Agents".into(),
                    help: String::new(),
                    types: vec![],
                },
                Feature {
                    key: "codex/cli".into(),
                    label: "Codex".into(),
                    group: "Agents".into(),
                    help: String::new(),
                    types: vec![],
                },
            ],
            secrets: vec!["tailnet_api_key".into()],
            own_key: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOwn berth".into(),
            provisioners: vec![],
        }
    }

    /// The same host set up as the screens do it: a way chosen on the
    /// Install tab, features ticked, rows of the Settings tab written.
    fn on_the_screens() -> Value {
        let catalogue = catalogue();
        let mut config = catalogue.template.clone();
        let mut action = Action::Machine;
        setup::fit_target(&mut config, action);
        action = Action::Iso.choose(&mut config);
        setup::fit_target(&mut config, action);
        // A new host starts as one a provisioner makes, which is a cloud
        // host; the Install tab's Target is chosen again for the installer.
        setup::commit(&mut config, Field::Target, "laptop").unwrap();
        setup::set_feature(&mut config, &catalogue.features, 0, true);
        setup::commit(&mut config, Field::Hostname, "studio").unwrap();
        setup::commit(&mut config, Field::OperatorUser, "olga").unwrap();
        setup::set_pointer(
            &mut config,
            Field::OperatorHash.spec().pointer,
            json!(format!("$6$made$studioolga{}", "x".repeat(40))),
        );
        setup::set_pointer(
            &mut config,
            Field::AgentHash.spec().pointer,
            json!(format!("$6$made$studioagent{}", "x".repeat(40))),
        );
        setup::commit(&mut config, Field::DiskMode, "largest").unwrap();
        config
    }

    #[test]
    fn answers_flags_and_the_screens_make_the_same_profile() {
        // With flags, nothing is asked.
        let mut given = Given {
            method: Action::by_key("iso"),
            generate_passwords: true,
            ..Given::default()
        };
        given
            .set(&[
                "target=laptop".into(),
                "hostname=studio".into(),
                "operator_username=olga".into(),
                "feature.claude/cli=on".into(),
                "target_disk_mode=largest".into(),
            ])
            .unwrap();
        let mut flagged = Flow::new("studio", catalogue());
        let mut nobody = Script {
            answers: vec![],
            asked: vec![],
        };
        flagged.run(&given, &mut nobody, &mut Hashes).unwrap();

        // With a terminal, the same answers to the questions in their order.
        let mut asked = Flow::new("studio", catalogue());
        let mut script = Script {
            answers: vec![
                // How it is installed: the USB installer.
                Answer::Choice(1),
                // Target: a laptop.
                Answer::Choice(0),
                // Features: Claude Code, and berth's own as they are.
                Answer::Many({
                    let view = asked.view(Step::Features);
                    let mut ticked = vec![true, false];
                    ticked.extend(view.fields().iter().map(|field| view.enabled(*field)));
                    ticked
                }),
                // The operator and the agent.
                Answer::Text("olga".into()),
                Answer::Text("agent".into()),
                // Passwords: generated.
                Answer::Choice(0),
            ],
            asked: vec![],
        };
        let given = Given {
            values: vec![
                (Field::Hostname, "studio".into()),
                (Field::DiskMode, "largest".into()),
            ],
            ..Given::default()
        };
        asked.run(&given, &mut script, &mut Hashes).unwrap();
        assert_eq!(
            script.asked,
            [
                "How it is installed",
                "Target",
                "Features",
                "Operator username",
                "Agent username",
                "Passwords"
            ]
        );
        assert!(script.answers.is_empty());

        let screens = on_the_screens();
        assert_eq!(flagged.config, screens);
        assert_eq!(asked.config, screens);
        assert_eq!((flagged.action, asked.action), (Action::Iso, Action::Iso));
    }

    #[test]
    fn without_a_terminal_what_is_missing_names_its_flag() {
        let mut flow = Flow::new("studio", catalogue());
        let stopped = flow.run(&Given::default(), &mut Nobody, &mut Hashes);
        match stopped {
            Err(Stop::Missing(question)) => assert_eq!(question.flag, "--method METHOD"),
            other => panic!("{other:?}"),
        }
        let mut flow = Flow::new("studio", catalogue());
        let given = Given {
            method: Some(Action::Iso),
            ..Given::default()
        };
        // The operator's password is required for an installer.
        match flow.run(&given, &mut Nobody, &mut Hashes) {
            Err(Stop::Missing(question)) => {
                assert_eq!(question.flag, "--generate-passwords");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_node_is_applied_over_ssh_with_a_servers_features_and_no_accounts() {
        let mut catalogue = catalogue();
        catalogue.features.push(Feature {
            key: "incus/server".into(),
            label: "Incus".into(),
            group: "Servers".into(),
            help: String::new(),
            types: vec![berth_api::HostType::Node],
        });
        let mut given = Given::default();
        given
            .set(&[
                "type=node".into(),
                "address=192.0.2.20".into(),
                "user=root".into(),
            ])
            .unwrap();
        let mut flow = Flow::new("big", catalogue.clone());
        let mut script = Script {
            answers: vec![Answer::Many(vec![true])],
            asked: vec![],
        };
        flow.run(&given, &mut script, &mut Hashes).unwrap();
        // The way is SSH, and of the features only the server's is offered.
        assert_eq!(flow.action, Action::Ssh);
        assert!(
            script
                .asked
                .iter()
                .all(|asked| asked != "How it is installed")
        );
        assert!(
            script.asked.contains(&"Features".to_owned()),
            "{:?}",
            script.asked
        );
        assert_eq!(
            flow.config["ansible_vars"]["devbox_plugin_features"],
            json!(["incus/server"])
        );
        assert_eq!(flow.config["type"], "node");
        assert!(flow.config.get("operator_username").is_none());
        assert!(flow.config.get("agent_username").is_none());
        // A workstation's feature is refused for a node.
        let mut given = given.clone();
        given.features.push(("claude/cli".into(), true));
        let mut flow = Flow::new("big", catalogue);
        match flow.run(&given, &mut Nobody, &mut Hashes) {
            Err(Stop::Refused(why)) => {
                assert!(why.contains("not for a host of the type node"), "{why}")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_host_over_ssh_needs_its_address() {
        let mut flow = Flow::new("dock", catalogue());
        let given = Given {
            method: Action::by_key("ssh-only"),
            ..Given::default()
        };
        match flow.run(&given, &mut Nobody, &mut Hashes) {
            Err(Stop::Missing(question)) => assert_eq!(question.flag, "--set address=SSH_ADDRESS"),
            other => panic!("{other:?}"),
        }
        let mut given = given;
        given.set(&["address=192.0.2.11".into()]).unwrap();
        let mut flow = Flow::new("dock", catalogue());
        flow.run(&given, &mut Nobody, &mut Hashes).unwrap();
        assert_eq!(flow.config["access"]["address"], "192.0.2.11");
        assert_eq!(flow.config["type"], "ssh");
    }

    #[test]
    fn every_field_has_a_key_of_its_own() {
        let keys: Vec<String> = Field::ALL.iter().map(|field| field.key()).collect();
        let mut unique = keys.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), keys.len(), "{keys:?}");
        for key in [
            "hostname",
            "operator_username",
            "address",
            "user",
            "wifi_ssid",
            "iso",
            "tools_web",
            "type",
        ] {
            assert!(keys.iter().any(|known| known == key), "{key}");
        }
        assert!(
            matches!(setting("install_docker=off"), Ok(Setting::Field(Field::Docker, value)) if value == "false")
        );
        assert!(setting("nothing=1").is_err());
    }
}
