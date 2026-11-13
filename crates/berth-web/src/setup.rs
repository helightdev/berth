//! The set-up tabs of one of the server's hosts: how it is installed, what
//! is installed, its settings, and running it. The profile is edited here
//! and kept by the server.

use crate::admin::{Admin, Data};
use crate::browser::Browser;
use berth_tui::fields::{Action, Field, Kind, Step};
use berth_tui::machine::{self, Machine, Next};
use berth_tui::screen::{draw_footer, draw_header, draw_question, inside};
use berth_tui::setup::{self, Account, Entry, Feature, Item, Page, Problem, Profile, Row};
use berth_tui::{Click, Code, Event, Key, event, markdown, theme};
use ratzilla::ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph, Wrap},
};
use serde_json::{Value, json};
use tui_input::Input;

/// The ways the server installs a host itself.
const METHODS: [Action; 3] = [Action::Machine, Action::CloudSeed, Action::Ssh];

/// Draw the box something is typed in, under the rows it belongs to.
pub fn draw_editor(frame: &mut Frame, area: Rect, title: &str, input: &Input, masked: bool) {
    let width = area.width.saturating_sub(4) as usize;
    let typed = match masked {
        true => "•".repeat(input.value().chars().count()),
        false => input.value().to_owned(),
    };
    // The end of what is typed stays in view.
    let count = typed.chars().count();
    let scroll = count.saturating_sub(width);
    let shown: String = typed.chars().skip(scroll).collect();
    let cursor = match masked {
        true => count,
        false => input.visual_cursor(),
    }
    .saturating_sub(scroll);
    frame.render_widget(
        Paragraph::new(shown).block(theme::panel(title.trim(), true)),
        area,
    );
    frame.set_cursor_position((area.x + 2 + cursor as u16, area.y + 1));
}

/// What a key on the set-up pages leads to.
pub enum Flow {
    Stay,
    /// Back to the host's overview, or to the hosts for one never saved.
    Leave,
}

pub struct Setup {
    pub name: String,
    pub config: Value,
    /// The profile as the server keeps it; nothing for a host it does not have yet.
    saved: Value,
    /// Its profile has been asked for and is not here yet.
    pub loading: bool,
    pub failed: Option<String>,
    step: Step,
    action: Action,
    features: Vec<Feature>,
    registries: Vec<String>,
    secrets: Vec<String>,
    /// Where a release can be published.
    stores: Vec<String>,
    /// The server's own SSH public key.
    own_key: String,
    list: ListState,
    display: ListState,
    editing: Option<Input>,
    /// The editor holds a plain password to hash rather than a field value.
    password_entry: bool,
    quit_armed: bool,
    hits: Vec<(Rect, usize)>,
    tabs: Vec<Rect>,
    /// The size a panel for output gets.
    pub panel: (u16, u16),
    /// The machine the server creates for the host, on the set-up tabs.
    pub machine: Machine,
}

impl Setup {
    /// The set-up of a host the server has: its profile is on its way.
    pub fn open(name: &str, admin: &Admin) -> Setup {
        admin.profile(name);
        let mut setup = Setup {
            loading: true,
            ..Setup::with(name, Value::Null, Value::Null)
        };
        setup.ask_machine(admin);
        setup
    }

    /// The set-up of a new host, from the server's starting profile.
    pub fn new(name: &str, mut template: Value, admin: &Admin) -> Setup {
        // A laptop profile would add a display manager and power policy to a server.
        if template["profile"] == "laptop" {
            template["profile"] = json!("cloud");
        }
        let mut setup = Setup::with(name, template, Value::Null);
        setup.ask_machine(admin);
        setup
    }

    /// Ask for the provisioners and the host's machine, which the Install
    /// tab it opens on shows.
    fn ask_machine(&mut self, admin: &Admin) {
        self.machine.asking();
        admin.machine(&self.name);
    }

    fn with(name: &str, config: Value, saved: Value) -> Setup {
        Setup {
            name: name.to_owned(),
            config,
            saved,
            loading: false,
            failed: None,
            step: Step::Install,
            action: Action::Machine,
            features: vec![],
            registries: vec![],
            secrets: vec![],
            stores: vec![],
            own_key: String::new(),
            list: ListState::default().with_selected(Some(0)),
            display: ListState::default(),
            editing: None,
            password_entry: false,
            quit_armed: false,
            hits: vec![],
            tabs: vec![],
            panel: (24, 80),
            machine: Machine::default(),
        }
    }

    /// The server has this host's profile.
    pub fn kept(&self) -> bool {
        !self.saved.is_null()
    }

    pub fn unsaved(&self) -> bool {
        !self.loading && self.failed.is_none() && self.config != self.saved
    }

    pub fn arrived(&mut self, profile: Result<Value, String>) {
        self.loading = false;
        match profile {
            Ok(profile) => {
                self.config = profile.clone();
                self.saved = profile;
                // A host with an address is one to install over SSH.
                if !self.view().text(Field::SshHost).is_empty() {
                    self.action = Action::Ssh;
                }
            }
            Err(error) => self.failed = Some(error),
        }
    }

    pub fn saved(&mut self, profile: Value) {
        self.saved = profile;
    }

    /// Take what the server made up for a field.
    pub fn generated(&mut self, field: Field, value: &str) {
        setup::set_pointer(&mut self.config, field.spec().pointer, json!(value));
    }

    /// Keep in step with what the server has to give.
    pub fn sync(&mut self, data: &Data) {
        self.features = data.features.clone();
        self.registries = data.registry_names();
        let names = data.secret_names();
        if !self.loading {
            setup::default_enrollment(&mut self.config, &names);
        }
        self.secrets = setup::host_secrets(names);
        self.stores = setup::release_stores(&data.registries);
        self.own_key = data.own_key.clone();
    }

    fn view(&self) -> Profile<'_> {
        Profile {
            config: &self.config,
            step: self.step,
            action: self.action,
            features: &self.features,
            registries: &self.registries,
            secrets: &self.secrets,
            own_key: &self.own_key,
            named: setup::valid_hostname(&self.name),
        }
    }

    /// Every problem, each on the tab of what it is about.
    fn problems(&self) -> Vec<Problem> {
        let view = self.view();
        // The keys that make these up are other ones here.
        let mut problems: Vec<Problem> = view
            .problems_with(&self.machine)
            .into_iter()
            .map(|problem| Problem {
                text: problem
                    .text
                    .replace("(F4 generates)", "(g generates)")
                    .replace("(F4 or F5)", "(g or p)"),
                ..problem
            })
            .collect();
        if self.action == Action::Ssh && view.text(Field::SshHost).is_empty() {
            problems.push(Problem::new(
                Step::Install,
                "The host's address is required: its name, IP address or tailnet name",
            ));
        }
        problems
    }

    fn rows(&self) -> Vec<Row> {
        let view = self.view();
        let problems = self.problems();
        view.page_rows(Page {
            methods: &METHODS,
            machine: &self.machine,
            problems: &problems,
            notes: vec![],
        })
    }

    fn entries(&self) -> Vec<Entry> {
        self.rows()
            .into_iter()
            .filter_map(|row| row.entry)
            .collect()
    }

    fn selected(&self) -> Option<Entry> {
        self.entries()
            .get(self.list.selected().unwrap_or(0))
            .copied()
    }

    fn selected_field(&self) -> Option<Field> {
        match self.selected() {
            Some(Entry::Field(field)) => Some(field),
            _ => None,
        }
    }

    fn help(&self) -> String {
        match self.selected() {
            // F5 is the browser's own, so the letters do what berth's F4 and F5 do.
            // The key itself, to give to a host that was not set up from here.
            Some(Entry::Field(Field::BerthKey)) => berth_tui::berth_key::field_help(&self.own_key),
            Some(Entry::Field(field)) => field.spec().help.replace("F4", "g").replace("F5", "p"),
            Some(Entry::Key(Account::Operator, Some(_))) => Field::OperatorKey.spec().help.into(),
            Some(Entry::Key(Account::Agent, Some(_))) => Field::AgentKey.spec().help.into(),
            Some(Entry::Key(_, None)) => setup::ADD_KEY_HELP.into(),
            Some(Entry::Action(Action::CloudSeed)) => "For a new machine at a cloud provider: the server renders cloud-init user data that installs the host on first boot. With a place to publish to chosen, it publishes the release there first.".into(),
            Some(Entry::Action(Action::Ssh)) => "Install on an existing Ubuntu host over SSH, from the server: it needs a key of the server's that the host takes. The output is shown here as it runs.".into(),
            Some(Entry::Action(action)) => action.help().to_owned(),
            Some(Entry::Group(_)) => setup::GROUP_HELP.into(),
            Some(Entry::Feature(index)) => self.features[index].help.clone(),
            Some(Entry::Registry(_)) => setup::REGISTRY_HELP.into(),
            Some(Entry::Secret(_)) => setup::SECRET_HELP.into(),
            Some(Entry::Run) => match self.action {
                Action::CloudSeed => "Saves the profile on the server and renders the seed. Paste it into the provider's user data field when creating the machine.".into(),
                _ => "Saves the profile on the server and applies it to the host from there.".into(),
            },
            Some(Entry::Machine(pick)) => self.machine.help(pick),
            Some(Entry::Save) => "Saves the profile on the server, as Ctrl+S does, without running anything.".into(),
            None => String::new(),
        }
    }

    fn set_step(&mut self, step: Step, admin: &Admin) {
        self.step = step;
        self.list = ListState::default().with_selected(Some(0));
        if step == Step::Install {
            self.ask_machine(admin);
        }
    }

    /// Do what a row of the machine led to.
    fn follow(&mut self, next: Next, admin: &Admin) -> Option<String> {
        let (name, size) = (self.name.clone(), self.panel);
        match next {
            Next::Nothing | Next::Confirm => None,
            Next::Say(text) => Some(text),
            Next::Edit(value) => {
                self.editing = Some(Input::new(value));
                None
            }
            Next::Create(asked) => {
                // The server's provisioner works from the profile as it keeps it.
                admin.save(&self.name, &self.config, move |admin| {
                    admin.provision(&name, &asked, size)
                });
                Some("Saving…".into())
            }
            Next::Power(verb) => {
                admin.power(&name, verb);
                None
            }
            Next::Destroy => {
                admin.unprovision(&name, size);
                None
            }
        }
    }

    fn move_selection(&mut self, up: bool) {
        let index = self.list.selected().unwrap_or(0);
        let last = self.entries().len().saturating_sub(1);
        self.list.select(Some(match up {
            true => index.saturating_sub(1),
            false => (index + 1).min(last),
        }));
    }

    fn set(&mut self, field: Field, value: Value) {
        setup::set_pointer(&mut self.config, field.spec().pointer, value);
    }

    /// Toggle or cycle the field, or open the editor for free text.
    fn activate(&mut self, field: Field) -> Option<String> {
        if field == Field::ReleaseStore {
            let next = setup::next_of(&self.stores, self.view().text(field));
            self.set(field, json!(next));
            return self.stores.is_empty().then(|| {
                "No file server or raw repository yet. Add one on the Registries tab of the home screen.".into()
            });
        }
        match field.spec().kind {
            Kind::Toggle => {
                let on = !self.view().enabled(field);
                self.set(field, json!(on));
            }
            Kind::Choice(choices) => {
                let current = self.view().text(field);
                let next = choices
                    .iter()
                    .position(|choice| *choice == current)
                    .map_or(0, |index| (index + 1) % choices.len());
                self.set(field, json!(choices[next]));
            }
            _ => self.editing = Some(Input::new(self.view().text(field).to_owned())),
        }
        None
    }

    fn accept_edit(&mut self, admin: &Admin) -> Option<String> {
        let input = self.editing.take()?;
        let hashing = std::mem::take(&mut self.password_entry);
        match self.selected() {
            Some(Entry::Field(field)) if hashing => {
                if input.value().is_empty() {
                    return Some("Nothing entered, kept as it was".into());
                }
                admin.generate(
                    field,
                    json!({"what": "password", "password": input.value()}),
                );
                None
            }
            Some(Entry::Field(field)) => {
                setup::commit(&mut self.config, field, input.value()).err()
            }
            Some(Entry::Secret(index)) => {
                setup::set_secret(&mut self.config, &self.secrets[index], input.value())
                    .err()
                    .map(str::to_owned)
            }
            Some(Entry::Key(account, index)) if !input.value().trim().is_empty() => {
                setup::set_key(&mut self.config, account, index, input.value()).err()
            }
            Some(Entry::Machine(pick)) => {
                let next = self.machine.typed(pick, input.value(), &self.name);
                self.follow(next, admin)
            }
            _ => None,
        }
    }

    fn generate(&mut self, field: Field, admin: &Admin) -> Option<String> {
        let view = self.view();
        let asked = match field {
            Field::Hostname => json!({"what": "hostname"}),
            Field::TailscaleKey => {
                json!({"what": "tailscale_key", "host": view.text(Field::Hostname)})
            }
            _ if field.is_password_hash() => {
                let user = match field {
                    Field::OperatorHash => Field::OperatorUser,
                    _ => Field::AgentUser,
                };
                json!({"what": "password", "host": self.name, "user": view.text(user)})
            }
            _ => return None,
        };
        admin.generate(field, asked);
        Some("Asking the server…".into())
    }

    fn save(&mut self, admin: &Admin) {
        admin.save(&self.name, &self.config, |admin| {
            admin.say("Saved on the server")
        });
    }

    fn run(&mut self, admin: &Admin) -> Option<String> {
        if !self.problems().is_empty() {
            return Some("Fix the listed problems first".into());
        }
        let (name, action, size) = (self.name.clone(), self.action, self.panel);
        admin.save(&self.name, &self.config, move |admin| match action {
            Action::CloudSeed => admin.seed(&name),
            _ => admin.apply(&name, None, size),
        });
        Some("Saving…".into())
    }

    fn choose(&mut self, entry: Entry, admin: &Admin) -> Option<String> {
        match entry {
            Entry::Action(action) => {
                self.action = action;
                // A laptop profile would add a display manager and power policy to a server.
                if action.seeds() && self.view().text(Field::Profile) == "laptop" {
                    self.config["profile"] = json!("cloud");
                }
            }
            Entry::Field(field) => return self.activate(field),
            Entry::Registry(index) => {
                setup::toggle_registry(&mut self.config, &self.registries[index])
            }
            Entry::Secret(index) => self.editing = Some(Input::new(self.view().secret_how(index))),
            Entry::Key(..) => self.editing = Some(Input::default()),
            Entry::Group(_) => return Some("Space switches the whole group".into()),
            Entry::Feature(index) => {
                let on = !self.view().item_enabled(Item::Feature(index));
                setup::set_feature(&mut self.config, &self.features, index, on);
            }
            Entry::Run => return self.run(admin),
            Entry::Save => self.save(admin),
            Entry::Machine(pick) => {
                // The machine is made from the profile, which has to be whole first.
                if pick == machine::Pick::Create && !self.problems().is_empty() {
                    return Some("Fix the listed problems first".into());
                }
                let next = self.machine.enter(pick, &self.name);
                return self.follow(next, admin);
            }
        }
        None
    }

    fn toggle_group(&mut self, index: usize) -> String {
        let on = !self.view().group_all_on(index);
        let group = self.view().feature_groups().swap_remove(index);
        for item in group.items {
            match item {
                Item::Field(field) => self.set(field, json!(on)),
                Item::Feature(feature) => {
                    setup::set_feature(&mut self.config, &self.features, feature, on)
                }
            }
        }
        format!("{}: all {}", group.title, if on { "on" } else { "off" })
    }

    fn key(&mut self, key: Key, admin: &Admin, status: &mut String) -> Flow {
        let armed = std::mem::take(&mut self.quit_armed);
        let mut say = |said: Option<String>| {
            if let Some(said) = said {
                *status = said;
            }
        };
        if self.loading || self.failed.is_some() {
            return match key.code {
                Code::Esc | Code::Char('q') => Flow::Leave,
                _ => Flow::Stay,
            };
        }
        if self.machine.question.is_some() {
            let yes = match key.code {
                Code::Char('y' | 'Y') => true,
                Code::Char('n' | 'N') | Code::Esc => false,
                _ => return Flow::Stay,
            };
            let next = self.machine.answer(yes);
            say(self.follow(next, admin));
            return Flow::Stay;
        }
        match key.code {
            Code::Char('s') if key.ctrl => {
                say(self.accept_edit(admin));
                self.save(admin);
                return Flow::Stay;
            }
            Code::Tab | Code::BackTab => {
                say(self.accept_edit(admin));
                let step = self.step.offset(if key.code == Code::Tab { 1 } else { -1 });
                self.set_step(step, admin);
                return Flow::Stay;
            }
            _ => {}
        }
        if let Some(input) = &mut self.editing {
            match key.code {
                Code::Esc => {
                    self.editing = None;
                    self.password_entry = false;
                }
                Code::Enter => say(self.accept_edit(admin)),
                Code::Up | Code::Down => {
                    say(self.accept_edit(admin));
                    self.move_selection(key.code == Code::Up);
                }
                Code::Char('u') if key.ctrl => input.reset(),
                _ => event::edit(input, &key),
            }
            return Flow::Stay;
        }
        match key.code {
            Code::Up => self.move_selection(true),
            Code::Down => self.move_selection(false),
            // F5 is the browser's own, so the letters do what berth's F4 and F5 do.
            Code::Char('g') | Code::F(4)
                if self.selected_field().is_some_and(Field::can_generate) =>
            {
                let field = self.selected_field().unwrap();
                say(self.generate(field, admin));
            }
            _ if self.selected_field() == Some(Field::BerthKey)
                && berth_tui::berth_key::is_copy(&key) =>
            {
                say(Some(berth_tui::berth_key::copy(
                    &mut Browser,
                    &self.own_key,
                )));
            }
            Code::Char('p') if self.selected_field().is_some_and(Field::is_password_hash) => {
                self.password_entry = true;
                self.editing = Some(Input::default());
            }
            Code::Char(digit @ '1'..='4') => {
                self.set_step(Step::SETUP[digit as usize - '1' as usize], admin)
            }
            Code::Char(' ') => match self.selected() {
                Some(Entry::Group(group)) => say(Some(self.toggle_group(group))),
                Some(entry @ (Entry::Field(_) | Entry::Feature(_) | Entry::Registry(_))) => {
                    say(self.choose(entry, admin))
                }
                _ => {}
            },
            Code::Char('x') | Code::Delete => match self.selected() {
                Some(Entry::Secret(index)) => {
                    let _ = setup::set_secret(&mut self.config, &self.secrets[index], "");
                }
                Some(Entry::Key(account, Some(index))) => {
                    let _ = setup::set_key(&mut self.config, account, Some(index), "");
                    say(Some("SSH key removed".into()));
                }
                Some(Entry::Field(field)) if field.can_clear() => {
                    let _ = setup::commit(&mut self.config, field, "");
                    say(Some(format!("{} unset", field.spec().label)));
                }
                _ => {}
            },
            Code::Char('q') | Code::Esc => {
                if armed || !self.unsaved() {
                    return Flow::Leave;
                }
                self.quit_armed = true;
                say(Some(
                    "Unsaved changes · Ctrl+S saves, Esc again discards".into(),
                ));
            }
            Code::Enter => {
                if let Some(entry) = self.selected() {
                    say(self.choose(entry, admin));
                }
            }
            _ => {}
        }
        Flow::Stay
    }

    pub fn event(&mut self, event: Event, admin: &Admin, status: &mut String) -> Flow {
        match event {
            Event::Key(key) => {
                status.clear();
                return self.key(key, admin, status);
            }
            Event::Paste(text) => {
                if let Some(input) = &mut self.editing {
                    event::paste(input, text.trim());
                }
            }
            // The question before creating is answered with its keys.
            Event::Mouse(_) if self.machine.question.is_some() => {}
            Event::Mouse(mouse) => match mouse.click {
                Click::ScrollUp => self.move_selection(true),
                Click::ScrollDown => self.move_selection(false),
                Click::Left => {
                    let tab = self
                        .tabs
                        .iter()
                        .position(|rect| inside(*rect, mouse.column, mouse.row));
                    if let Some(tab) = tab {
                        return self.key(
                            Key::plain(Code::Char((b'1' + tab as u8) as char)),
                            admin,
                            status,
                        );
                    }
                    let hit = self
                        .hits
                        .iter()
                        .find(|(rect, _)| inside(*rect, mouse.column, mouse.row))
                        .map(|(_, index)| *index);
                    if let Some(index) = hit {
                        // A click selects; a click on what is selected chooses it.
                        if self.list.selected() == Some(index) {
                            return self.key(Key::plain(Code::Enter), admin, status);
                        }
                        if let Some(said) = self.accept_edit(admin) {
                            *status = said;
                        }
                        self.list.select(Some(index));
                    }
                }
                Click::Right => {}
            },
        }
        Flow::Stay
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect, status: &str) {
        let [header, _, panel, _, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(4),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(area);
        let labels: Vec<&str> = Step::SETUP.iter().map(|step| step.label()).collect();
        let current = Step::SETUP
            .iter()
            .position(|step| *step == self.step)
            .unwrap_or(0);
        let context = match self.unsaved() {
            true => format!("{} · not saved", self.name),
            false => self.name.clone(),
        };
        self.tabs = draw_header(frame, header, &labels, current, &context);
        self.hits.clear();
        if self.loading || self.failed.is_some() {
            let (text, colour) = match &self.failed {
                Some(error) => (format!("✗ {error}"), theme::BAD),
                None => (
                    "Asking the server for the host's profile…".into(),
                    theme::MUTED,
                ),
            };
            frame.render_widget(
                Paragraph::new(text)
                    .style(Style::new().fg(colour))
                    .wrap(Wrap { trim: false }),
                panel,
            );
            draw_footer(frame, footer, "Esc back", status, theme::ACCENT);
            return;
        }
        let [left, _, right] = Layout::horizontal([
            Constraint::Percentage(62),
            Constraint::Length(2),
            Constraint::Min(0),
        ])
        .areas(panel);
        frame.render_widget(
            Paragraph::new(markdown::render(&self.help()))
                .block(theme::panel("Information", false))
                .wrap(Wrap { trim: false }),
            right,
        );
        let [body, edit] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(if self.editing.is_some() { 3 } else { 0 }),
        ])
        .areas(left);
        // The cursor only stops on rows that do something.
        let rows = self.rows();
        let positions: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter_map(|(position, row)| row.entry.map(|_| position))
            .collect();
        let last = positions.len().saturating_sub(1);
        if self.list.selected().unwrap_or(0) > last {
            self.list.select(Some(last));
        }
        self.display.select(
            self.list
                .selected()
                .and_then(|index| positions.get(index).copied()),
        );
        frame.render_stateful_widget(
            List::new(rows.iter().map(|row| ListItem::new(row.line.clone())))
                .highlight_symbol(theme::cursor())
                .highlight_style(Style::new().bg(theme::RAISED)),
            body,
            &mut self.display,
        );
        for (index, &position) in positions.iter().enumerate() {
            if position < self.display.offset() {
                continue;
            }
            let y = body.y + (position - self.display.offset()) as u16;
            if y >= body.bottom() {
                break;
            }
            self.hits.push((Rect::new(body.x, y, body.width, 1), index));
        }
        let field = self.selected_field();
        if let Some(input) = &self.editing {
            let (title, masked) = match (self.password_entry, self.selected()) {
                (true, _) => (" Password, hashed on the server ".to_owned(), true),
                (_, Some(Entry::Secret(_))) => (" Variable name or file path ".to_owned(), false),
                (_, Some(Entry::Key(..))) => (" OpenSSH public key ".to_owned(), false),
                (_, Some(Entry::Machine(pick))) => (machine::editor_title(pick, &self.name), false),
                _ => (
                    " Edit ".to_owned(),
                    field.is_some_and(|field| field.spec().kind == Kind::Secret),
                ),
            };
            draw_editor(frame, edit, &title, input, masked);
        }
        let keys = match (&self.editing, field) {
            (Some(_), _) => "Enter accept · Esc cancel · Ctrl+S save".to_owned(),
            (None, Some(field)) if field.is_password_hash() => {
                "↑↓ select · g generate · p type a password · x clear · 1-4 tab · Ctrl+S save · Esc back".to_owned()
            }
            (None, Some(Field::BerthKey)) => format!(
                "↑↓ select · Space switch · {} · 1-4 tab · Ctrl+S save · Esc back",
                berth_tui::berth_key::HINT
            ),
            (None, Some(field)) if field.can_generate() => {
                "↑↓ select · Enter change · g generate · x clear · 1-4 tab · Ctrl+S save · Esc back".to_owned()
            }
            _ => "↑↓ select · Enter change · Space switch · x clear · 1-4 tab · Ctrl+S save · Esc back".to_owned(),
        };
        draw_footer(frame, footer, &keys, status, theme::ACCENT);
        if let Some(question) = &self.machine.question {
            draw_question(frame, question);
        }
    }
}

/// A line of the review that names what is being typed over.
pub fn masked_line(label: &'static str, typed: &str, active: bool) -> Line<'static> {
    Line::from(vec![
        Span::styled(label, Style::new().fg(theme::MUTED)),
        Span::styled(
            "› ",
            Style::new().fg(if active { theme::ACCENT } else { theme::LINE }),
        ),
        Span::raw("•".repeat(typed.chars().count())),
    ])
}
