//! The screen a setup key opens, after an install: berth is installed, this
//! browser is signed in, and the token for a terminal. The key works until
//! Done, so opening the link again before then shows the screen again.

use crate::browser::{self, Browser};
use berth_tui::screen::{Platform, note_hints, wrap};
use berth_tui::{Code, Event, theme};
use ratzilla::ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Clear, Paragraph},
};
use serde_json::Value;
use std::cell::RefCell;
use std::rc::Rc;

/// What collecting the token came to.
enum Claim {
    Asking,
    Claimed { version: String, token: String },
    Failed(String),
}

/// What the screen asks of the page.
pub enum Outcome {
    /// Nothing yet.
    Stay,
    /// Sign in with the token that was collected.
    SignIn(String),
    /// The key was not taken: go to the token dialog, saying why.
    Refused(String),
    /// Done: on to the hosts.
    Done,
}

pub struct Installed {
    server: String,
    key: String,
    claim: Rc<RefCell<Claim>>,
    signed_in: bool,
}

const KEYS: &str = " c copy the token · t copy the commands · Enter done ";

impl Installed {
    /// Collect the token of `server` with the setup key.
    pub fn claim(server: &str, key: String) -> Installed {
        let claim = Rc::new(RefCell::new(Claim::Asking));
        let answered = claim.clone();
        browser::request(
            "POST",
            &format!("{server}/v1/setup-key"),
            &key,
            move |answer| {
                *answered.borrow_mut() = match answer {
                    Ok((200, body)) => {
                        let said: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                        let text = |name: &str| said[name].as_str().unwrap_or("").to_owned();
                        match text("token").as_str() {
                            "" => Claim::Failed("The server sent no token.".into()),
                            token => Claim::Claimed {
                                version: text("version"),
                                token: token.to_owned(),
                            },
                        }
                    }
                    Ok((401, _)) => Claim::Failed(
                        "The setup key in the link was not taken: it is wrong, used up or expired. Make a new one where the server runs (berth setup-key), or give the token."
                            .into(),
                    ),
                    Ok((status, body)) => {
                        let said: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                        Claim::Failed(match said["error"]["message"].as_str() {
                            Some(message) => format!("The setup key did not work: {message}"),
                            None => format!("The setup key did not work: HTTP {status}"),
                        })
                    }
                    Err(error) => Claim::Failed(format!("The server did not answer: {error}")),
                };
            },
        );
        Installed {
            server: server.to_owned(),
            key,
            claim,
            signed_in: false,
        }
    }

    /// What the page does next, asked each frame.
    pub fn tick(&mut self) -> Outcome {
        match &*self.claim.borrow() {
            Claim::Claimed { token, .. } if !self.signed_in => {
                self.signed_in = true;
                Outcome::SignIn(token.clone())
            }
            Claim::Failed(why) => Outcome::Refused(why.clone()),
            _ => Outcome::Stay,
        }
    }

    fn token(&self) -> Option<String> {
        match &*self.claim.borrow() {
            Claim::Claimed { token, .. } => Some(token.clone()),
            _ => None,
        }
    }

    fn commands(&self, token: &str) -> [String; 2] {
        [
            format!("berth config server {}", self.server),
            format!("berth config server_token {token}"),
        ]
    }

    /// A key on the screen, and what the status line then says.
    pub fn event(&mut self, event: Event, said: &mut String) -> Outcome {
        let Event::Key(key) = event else {
            return Outcome::Stay;
        };
        let Some(token) = self.token() else {
            return Outcome::Stay;
        };
        let copy = |text: &str, what: &str| match Browser.copy(text) {
            true => format!("Copied {what}"),
            false => "The browser did not let the page copy".into(),
        };
        match key.code {
            Code::Char('c') => *said = copy(&token, "the token"),
            Code::Char('t') => *said = copy(&self.commands(&token).join("\n"), "the commands"),
            Code::Enter => return Outcome::Done,
            _ => {}
        }
        Outcome::Stay
    }

    /// End the key. `then` gets what to say if that did not work.
    pub fn done(&self, then: impl FnOnce(Option<String>) + 'static) {
        browser::request(
            "DELETE",
            &format!("{}/v1/setup-key", self.server),
            &self.key,
            move |answer| {
                then(match answer {
                    Ok((200, _)) => None,
                    _ => Some(
                        "The setup key was not ended. It stops by itself within a day, or with berth setup-key --remove where the server runs."
                            .into(),
                    ),
                })
            },
        );
    }

    pub fn draw(&self, frame: &mut Frame) {
        let screen = frame.area();
        let width = 96.min(screen.width);
        let inner = width.saturating_sub(4) as usize;
        let muted = Style::new().fg(theme::MUTED);
        let accent = Style::new().fg(theme::ACCENT);
        let mut lines: Vec<Line> = vec![];
        let (title, keys) = match &*self.claim.borrow() {
            Claim::Claimed { version, token } => {
                let version = match version.as_str() {
                    "" => String::new(),
                    version => format!(" {version}"),
                };
                for line in wrap(
                    &format!(
                        "berth{version} is installed and serves at {}. This browser is signed in.",
                        self.server
                    ),
                    inner,
                ) {
                    lines.push(Line::raw(line));
                }
                lines.push(Line::raw(""));
                lines.push(Line::styled("The token", muted));
                for line in wrap(token, inner) {
                    lines.push(Line::styled(format!("  {line}"), accent));
                }
                lines.push(Line::raw(""));
                lines.push(Line::styled(
                    "For berth in a terminal, on your own machine",
                    muted,
                ));
                for command in self.commands(token) {
                    for line in wrap(&command, inner) {
                        lines.push(Line::from(vec![Span::raw("  "), Span::raw(line)]));
                    }
                }
                lines.push(Line::raw(""));
                for line in wrap(
                    "The token gives full control of this server: keep it like a password. The setup key works until you choose Done.",
                    inner,
                ) {
                    lines.push(Line::styled(line, muted));
                }
                (" berth is installed ", KEYS)
            }
            _ => {
                lines.push(Line::raw("Collecting the token with the setup key…"));
                (" berth is installed ", "")
            }
        };
        let height = (lines.len() as u16 + 2).min(screen.height);
        let area = Rect::new(
            screen.x + (screen.width - width) / 2,
            screen.y + (screen.height - height) / 2,
            width,
            height,
        );
        note_hints(
            keys.trim(),
            area.x + 1,
            area.bottom().saturating_sub(1),
            true,
        );
        frame.render_widget(Clear, area);
        let block = theme::popup(title.trim(), theme::ACCENT);
        let block = match keys.is_empty() {
            true => block,
            false => block.title_bottom(theme::hints(keys.trim())),
        };
        frame.render_widget(Paragraph::new(lines).block(block), area);
    }
}
