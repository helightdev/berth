//! The Provisioners tab of the terminal's home screen: the shared tab of
//! [`berth_tui::provisioners`], with the provisioners of this machine or of
//! the berth server berth uses. What it asks for runs in a thread of its
//! own, so the screen goes on drawing while a provisioner is asked.

use crate::provisioner_command::{Change, Ran, Source, answer_lines};
use berth_api::{Act, Action, ProvisionerInfo, ProvisionerState, Resource};
use berth_core::paths::Paths;
use berth_tui::host::View as Panel;
use berth_tui::provisioners::{Asked, Shown, Tab};
use berth_tui::screen::Platform;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{Receiver, Sender, channel},
};

/// What came back from a thread.
enum Arrived {
    List(Result<Vec<ProvisionerInfo>, String>),
    Page(
        String,
        Box<Result<ProvisionerState, String>>,
        Result<Vec<Action>, String>,
    ),
    Resource(
        String,
        Box<Result<Resource, String>>,
        Result<Vec<Action>, String>,
    ),
    /// The types of provisioner, to register one.
    Types(Result<Vec<ProvisionerInfo>, String>),
    /// A provisioner was registered under this name, or why not.
    Saved(Result<String, String>),
    Output(Vec<u8>),
    /// A job ended: the subject's page to ask again, a resource's or a
    /// provisioner's, and how it ended.
    Ended(Subject, Option<i32>),
}

/// What a job was of, whose page is asked again when it ends.
enum Subject {
    Provisioner(String),
    Resource(String),
}

/// Where a job's output goes: on to the screen, a piece at a time, until
/// the panel is closed.
struct Lines(Sender<Arrived>, Arc<AtomicBool>);

impl std::io::Write for Lines {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if self.1.load(Ordering::Relaxed) {
            return Err(std::io::ErrorKind::BrokenPipe.into());
        }
        let _ = self.0.send(Arrived::Output(data.to_vec()));
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// What a key on the tab leads to.
pub enum Left {
    Stay,
    Tab(usize),
    Quit,
}

pub struct View {
    pub tab: Tab,
    listing: Option<Result<Vec<ProvisionerInfo>, String>>,
    asked: bool,
    sender: Sender<Arrived>,
    arrived: Receiver<Arrived>,
    /// A job that runs or ran, in a panel over the tab.
    pub job: Option<Panel>,
    /// Set when the panel is closed, so a log that is followed stops.
    closed: Arc<AtomicBool>,
    /// The kind to make once the list has come, as [`View::create`] asks.
    creating: Option<String>,
}

impl Default for View {
    fn default() -> View {
        let (sender, arrived) = channel();
        View {
            tab: Tab::default(),
            listing: None,
            asked: false,
            sender,
            arrived,
            job: None,
            closed: Arc::new(AtomicBool::new(false)),
            creating: None,
        }
    }
}

impl View {
    pub fn list(&self) -> &[ProvisionerInfo] {
        match &self.listing {
            Some(Ok(list)) => list,
            _ => &[],
        }
    }

    /// Ask for the list, unless it was asked for.
    pub fn load(&mut self, paths: &Paths) {
        if self.listing.is_some() || self.asked {
            return;
        }
        self.asked = true;
        let (paths, sender) = (paths.clone(), self.sender.clone());
        std::thread::spawn(move || {
            let listed = Source::of(&paths).list().map_err(|error| error.to_string());
            let _ = sender.send(Arrived::List(listed));
        });
    }

    /// Open the form that makes a resource of `kind` that is no host, such
    /// as an instance, with the provisioner that makes it; once the list has
    /// come, when it has not.
    pub fn create(&mut self, paths: &Paths, kind: &str, status: &mut String) {
        let list = match &self.listing {
            Some(Ok(list)) => list.clone(),
            _ => {
                self.creating = Some(kind.to_owned());
                self.listing = None;
                self.load(paths);
                return;
            }
        };
        match self.tab.create(&list, kind) {
            Asked::Open(id) => self.open(paths, id),
            Asked::Nothing(Some(said)) => *status = said,
            _ => {}
        }
    }

    fn open(&self, paths: &Paths, id: String) {
        let (paths, sender) = (paths.clone(), self.sender.clone());
        std::thread::spawn(move || {
            let source = Source::of(&paths);
            let state = source
                .state(&id, &Default::default())
                .map_err(|error| error.to_string());
            let actions = source.actions(&id).map_err(|error| error.to_string());
            let _ = sender.send(Arrived::Page(id, Box::new(state), actions));
        });
    }

    fn types(&self, paths: &Paths) {
        let (paths, sender) = (paths.clone(), self.sender.clone());
        std::thread::spawn(move || {
            let types = Source::of(&paths)
                .types()
                .map_err(|error| error.to_string());
            let _ = sender.send(Arrived::Types(types));
        });
    }

    fn save(&self, paths: &Paths, name: String, kind: String) {
        let (paths, sender) = (paths.clone(), self.sender.clone());
        std::thread::spawn(move || {
            let saved = berth_api::SavedProvisioner {
                kind,
                settings: None,
            };
            let said = Source::of(&paths)
                .save(&name, &saved)
                .map(|()| name)
                .map_err(|error| error.to_string());
            let _ = sender.send(Arrived::Saved(said));
        });
    }

    fn open_resource(&self, paths: &Paths, id: String) {
        let (paths, sender) = (paths.clone(), self.sender.clone());
        std::thread::spawn(move || {
            let source = Source::of(&paths);
            let seen = source.resource(&id).map_err(|error| error.to_string());
            let actions = source
                .resource_actions(&id)
                .map_err(|error| error.to_string());
            let _ = sender.send(Arrived::Resource(id, Box::new(seen), actions));
        });
    }

    /// A panel over the tab for what runs, and the flag that says it closed.
    fn panel(&mut self, title: &str) -> Arc<AtomicBool> {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((100, 30));
        self.job = Some(Panel::plain(
            title,
            rows.saturating_sub(6).max(8),
            cols.saturating_sub(8).max(20),
        ));
        self.closed = Arc::new(AtomicBool::new(false));
        self.closed.clone()
    }

    /// Show a resource's log in a panel, following it until the panel closes.
    fn logs(&mut self, paths: &Paths, id: String) {
        let closed = self.panel(&format!("Log · {id}"));
        let (paths, sender) = (paths.clone(), self.sender.clone());
        std::thread::spawn(move || {
            let mut out = Lines(sender.clone(), closed);
            let code = match Source::of(&paths).logs(&id, 200, true, &mut out) {
                Ok(()) => Some(0),
                Err(error) => {
                    let _ = sender.send(Arrived::Output(format!("✗ {error}\n").into_bytes()));
                    None
                }
            };
            let _ = sender.send(Arrived::Ended(Subject::Resource(id), code));
        });
    }

    /// Run an action of a resource, as `run` does one of a provisioner.
    fn run_resource(
        &mut self,
        paths: &Paths,
        id: String,
        action: Action,
        values: Change,
        status: &mut String,
    ) {
        let (paths, sender) = (paths.clone(), self.sender.clone());
        *status = format!("Asking {id}…");
        let closed = self.panel(&format!("{} · {id}", action.title()));
        std::thread::spawn(move || {
            let mut out = Lines(sender.clone(), closed);
            let code = match Source::of(&paths).resource_run(
                &id,
                &action.name,
                values,
                &mut out,
                &mut std::io::sink(),
            ) {
                Ok(Ran::Ended(code)) => Some(code),
                Ok(Ran::Answered(answer)) => {
                    let lines = answer_lines(&answer).join("\n");
                    let _ = sender.send(Arrived::Output(format!("{lines}\n").into_bytes()));
                    Some(0)
                }
                Err(error) => {
                    let _ = sender.send(Arrived::Output(format!("✗ {error}\n").into_bytes()));
                    None
                }
            };
            let _ = sender.send(Arrived::Ended(Subject::Resource(id), code));
        });
    }

    /// Take in what has come back, and say what there is to say.
    pub fn pump(&mut self, paths: &Paths, status: &mut String) {
        while let Ok(arrived) = self.arrived.try_recv() {
            match arrived {
                Arrived::List(listed) => {
                    if let Err(error) = &listed {
                        *status = format!("✗ {error}");
                    }
                    let ready = listed.is_ok();
                    self.listing = Some(listed);
                    self.asked = false;
                    if let Some(kind) = self.creating.take()
                        && ready
                    {
                        self.create(paths, &kind, status);
                    }
                }
                // One that was removed takes its page back to the list.
                Arrived::Page(_, state, _)
                    if state
                        .as_ref()
                        .as_ref()
                        .is_err_and(|error| error.starts_with("No provisioner ")) =>
                {
                    self.tab.page = None;
                    self.listing = None;
                    self.load(paths);
                }
                Arrived::Page(id, state, actions) => self.tab.opened(&id, *state, actions),
                Arrived::Types(types) => {
                    if let Some(said) = self.tab.adding(types) {
                        *status = said;
                    }
                }
                Arrived::Saved(Ok(name)) => {
                    *status = format!("Registered {name}. Its page has its settings and secrets");
                    self.listing = None;
                    self.load(paths);
                }
                Arrived::Saved(Err(error)) => *status = format!("✗ {error}"),
                // One that was removed takes its page back to its provisioner's.
                Arrived::Resource(id, seen, _)
                    if seen
                        .as_ref()
                        .as_ref()
                        .is_err_and(|error| error.starts_with("No resource ")) =>
                {
                    if let Some(provisioner) = self.tab.back_from(&id) {
                        self.open(paths, provisioner);
                    }
                }
                Arrived::Resource(id, seen, actions) => {
                    self.tab.opened_resource(&id, *seen, actions)
                }
                Arrived::Output(data) => {
                    if let Some(job) = &mut self.job {
                        job.feed(&data);
                    }
                }
                Arrived::Ended(subject, code) => {
                    if let Some(job) = &mut self.job {
                        job.ended = Some(code);
                    }
                    // What the action changed shows on the page.
                    match subject {
                        Subject::Provisioner(id) => self.open(paths, id),
                        Subject::Resource(id) => self.open_resource(paths, id),
                    }
                }
            }
        }
    }

    pub fn shown(&self, paths: &Paths) -> Shown {
        let mut shown = self.tab.shown(self.list(), &Source::of(paths).place());
        if let Some(Err(error)) = &self.listing {
            shown.help = format!("✗ {error}\n\n{}", shown.help);
        }
        shown
    }

    /// Draw what is over the tab: a job's panel, a form, a question, the
    /// arguments. `status` is the last message, which a form shows itself.
    pub fn draw_over(&self, frame: &mut Frame, status: &str) {
        if let Some(job) = &self.job {
            let keys = match job.outcome() {
                None => " It runs · its output comes as it is written ",
                Some(true) => " ✓ Done · Enter close ",
                Some(false) => " ✗ It did not finish · Enter close ",
            };
            berth_tui::screen::draw_panel(frame, &job.title, job.screen(), job.outcome(), keys, "");
            return;
        }
        if let Some(form) = self.tab.form() {
            form.draw(frame, status);
        }
        if let Some(question) = self.tab.question() {
            berth_tui::screen::draw_question(frame, question);
        }
        if let Some((actions, asked)) = self.tab.arguments() {
            berth_tui::screen::draw_action_arguments(frame, actions, asked);
        }
    }

    /// Run an action, in a thread. Its output, or its answer, fills a panel.
    fn run(&mut self, paths: &Paths, id: String, action: Action, values: Act, status: &mut String) {
        let (paths, sender) = (paths.clone(), self.sender.clone());
        *status = format!("Asking {id}…");
        let closed = self.panel(&format!("{} · {id}", action.title()));
        std::thread::spawn(move || {
            let mut out = Lines(sender.clone(), closed);
            let code = match Source::of(&paths).run(
                &id,
                &action.name,
                values,
                &mut out,
                &mut std::io::sink(),
            ) {
                Ok(Ran::Ended(code)) => Some(code),
                Ok(Ran::Answered(answer)) => {
                    let lines = answer_lines(&answer).join("\n");
                    let _ = sender.send(Arrived::Output(format!("{lines}\n").into_bytes()));
                    Some(0)
                }
                Err(error) => {
                    let _ = sender.send(Arrived::Output(format!("✗ {error}\n").into_bytes()));
                    None
                }
            };
            let _ = sender.send(Arrived::Ended(Subject::Provisioner(id), code));
        });
    }

    /// Whether keys go to a dialog or a panel, so none of them changes tabs.
    pub fn typing(&self) -> bool {
        self.job.is_some() || self.tab.typing()
    }

    /// Handle a key on the tab.
    pub fn key(
        &mut self,
        paths: &Paths,
        key: KeyEvent,
        status: &mut String,
        platform: &mut dyn Platform,
    ) -> Left {
        if let Some(job) = &self.job {
            // A log goes on until it is closed; a job's panel closes once it ended.
            let log = job.title.starts_with("Log · ");
            if (log || !job.running())
                && matches!(key.code, KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q'))
            {
                self.closed.store(true, Ordering::Relaxed);
                self.job = None;
            }
            return Left::Stay;
        }
        let Some(shared) = crate::keys::key(&key) else {
            return Left::Stay;
        };
        let list = self.list().to_vec();
        let asked = self.tab.key(shared, &list, platform);
        if asked == Asked::Other {
            match key.code {
                KeyCode::Char(digit @ '1'..='6') => {
                    return Left::Tab(digit as usize - '1' as usize);
                }
                // Next to the hosts: Tab goes on to the settings, Shift+Tab back.
                KeyCode::Tab => return Left::Tab(2),
                KeyCode::BackTab | KeyCode::Esc => return Left::Tab(0),
                KeyCode::Char('q') => return Left::Quit,
                _ => {}
            }
        }
        self.asked(paths, asked, status)
    }

    /// Put pasted text into what is being typed.
    pub fn paste(&mut self, text: &str) {
        self.tab.paste(text);
    }

    /// A click while a form is open, which the form takes.
    pub fn click(
        &mut self,
        paths: &Paths,
        (column, row): (u16, u16),
        status: &mut String,
        platform: &mut dyn Platform,
    ) -> Left {
        let list = self.list().to_vec();
        let asked = self.tab.click(column, row, &list, platform);
        self.asked(paths, asked, status)
    }

    /// Do what a key or a click on the tab asked for.
    fn asked(&mut self, paths: &Paths, asked: Asked, status: &mut String) -> Left {
        match asked {
            Asked::Nothing(said) => {
                if let Some(said) = said {
                    *status = said;
                }
            }
            Asked::List => {
                self.listing = None;
                self.load(paths);
            }
            Asked::Add => self.types(paths),
            Asked::Save { name, kind } => {
                *status = format!("Registering {name}…");
                self.save(paths, name, kind);
            }
            Asked::Open(id) => self.open(paths, id),
            Asked::Run {
                provisioner,
                action,
                values,
            } => {
                let act = Act {
                    arguments: values,
                    ..Act::default()
                };
                self.run(paths, provisioner, *action, act, status);
            }
            Asked::OpenResource(id) => self.open_resource(paths, id),
            Asked::RunResource {
                resource,
                action,
                values,
            } => {
                let change = Change {
                    arguments: values,
                    ..Change::default()
                };
                self.run_resource(paths, resource, *action, change, status);
            }
            Asked::Logs(id) => self.logs(paths, id),
            Asked::Hosts => return Left::Tab(0),
            Asked::Other => {}
        }
        Left::Stay
    }
}
