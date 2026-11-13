//! The Provisioners tab of the terminal's home screen: the shared tab of
//! [`berth_tui::provisioners`], with the provisioners of this machine or of
//! the berth server berth uses. What it asks for runs in a thread of its
//! own, so the screen goes on drawing while a provisioner is asked.

use crate::provisioner_command::{Ran, Source, answer_lines};
use berth_api::{Act, Action, ProvisionerInfo, ProvisionerState};
use berth_core::paths::Paths;
use berth_tui::host::View as Panel;
use berth_tui::provisioners::{Asked, Shown, Tab};
use berth_tui::screen::Platform;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use std::sync::mpsc::{Receiver, Sender, channel};

/// What came back from a thread.
enum Arrived {
    List(Result<Vec<ProvisionerInfo>, String>),
    Page(
        String,
        Box<Result<ProvisionerState, String>>,
        Result<Vec<Action>, String>,
    ),
    Answered(String, Result<Vec<String>, String>),
    Output(Vec<u8>),
    Ended(String, Option<i32>),
}

/// Where a job's output goes: on to the screen, a piece at a time.
struct Lines(Sender<Arrived>);

impl std::io::Write for Lines {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
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

    /// Take in what has come back, and say what there is to say.
    pub fn pump(&mut self, paths: &Paths, status: &mut String) {
        while let Ok(arrived) = self.arrived.try_recv() {
            match arrived {
                Arrived::List(listed) => {
                    if let Err(error) = &listed {
                        *status = format!("✗ {error}");
                    }
                    self.listing = Some(listed);
                    self.asked = false;
                }
                Arrived::Page(id, state, actions) => self.tab.opened(&id, *state, actions),
                Arrived::Answered(id, Ok(lines)) => {
                    *status = lines.first().cloned().unwrap_or_default();
                    self.tab.answered(&id, lines);
                }
                Arrived::Answered(_, Err(error)) => *status = format!("✗ {error}"),
                Arrived::Output(data) => {
                    if let Some(job) = &mut self.job {
                        job.feed(&data);
                    }
                }
                Arrived::Ended(id, code) => {
                    if let Some(job) = &mut self.job {
                        job.ended = Some(code);
                    }
                    // What the action changed shows on the page.
                    self.open(paths, id);
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

    /// Draw what is over the tab: a job's panel, a question, the arguments.
    pub fn draw_over(&self, frame: &mut Frame) {
        if let Some(job) = &self.job {
            let keys = match job.outcome() {
                None => " It runs · its output comes as it is written ",
                Some(true) => " ✓ Done · Enter close ",
                Some(false) => " ✗ It did not finish · Enter close ",
            };
            berth_tui::screen::draw_panel(frame, &job.title, job.screen(), job.outcome(), keys, "");
            return;
        }
        if let Some(question) = self.tab.question() {
            berth_tui::screen::draw_question(frame, question);
        }
        if let Some((actions, asked)) = self.tab.arguments() {
            berth_tui::screen::draw_action_arguments(frame, actions, asked);
        }
    }

    /// Run an action, in a thread: one that only reads answers on the page,
    /// one that changes something is a job whose output fills a panel.
    fn run(&mut self, paths: &Paths, id: String, action: Action, values: Act, status: &mut String) {
        let (paths, sender) = (paths.clone(), self.sender.clone());
        if !action.changes {
            *status = format!("Asking {id}…");
            std::thread::spawn(move || {
                let ran = Source::of(&paths).run(
                    &id,
                    &action.name,
                    values,
                    &mut std::io::sink(),
                    &mut std::io::sink(),
                );
                let said = match ran {
                    Ok(Ran::Answered(answer)) => Ok(answer_lines(&answer)),
                    Ok(Ran::Ended(code)) => Ok(vec![format!("It ended with {code}")]),
                    Err(error) => Err(error.to_string()),
                };
                let _ = sender.send(Arrived::Answered(id, said));
            });
            return;
        }
        let (cols, rows) = crossterm::terminal::size().unwrap_or((100, 30));
        self.job = Some(Panel::plain(
            &format!("{} · {id}", action.title()),
            rows.saturating_sub(6).max(8),
            cols.saturating_sub(8).max(20),
        ));
        std::thread::spawn(move || {
            let mut out = Lines(sender.clone());
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
            let _ = sender.send(Arrived::Ended(id, code));
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
            if !job.running()
                && matches!(key.code, KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q'))
            {
                self.job = None;
            }
            return Left::Stay;
        }
        let Some(shared) = crate::keys::key(&key) else {
            return Left::Stay;
        };
        let list = self.list().to_vec();
        match self.tab.key(shared, &list, platform) {
            Asked::Nothing(said) => {
                if let Some(said) = said {
                    *status = said;
                }
            }
            Asked::List => {
                self.listing = None;
                self.load(paths);
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
            Asked::Other => match key.code {
                KeyCode::Char(digit @ '1'..='6') => {
                    return Left::Tab(digit as usize - '1' as usize);
                }
                KeyCode::Tab | KeyCode::Esc => return Left::Tab(0),
                KeyCode::BackTab => return Left::Tab(4),
                KeyCode::Char('q') => return Left::Quit,
                _ => {}
            },
        }
        Left::Stay
    }
}
