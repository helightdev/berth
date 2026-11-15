//! Questions asked inline, below the command: a line of text, a hidden
//! password, a choice, a list to tick, a confirmation. Each is drawn with
//! ratatui in a few lines of the terminal and leaves one line behind with
//! its answer. Nobody is asked without a terminal or with `--no-input`.

use super::fail::{Fail, Result};
use super::out;
use berth_tui::flow::{Answer, Asked, Asker, Choice, Question};
use berth_tui::theme::{self, Role};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Terminal, TerminalOptions, Viewport,
    backend::CrosstermBackend,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};
use tui_input::Input;

/// The most rows of choices shown at once.
const ROWS: usize = 8;

/// What a question has been answered so far.
struct State {
    input: Input,
    cursor: usize,
    ticked: Vec<bool>,
    /// Why the last answer was not taken.
    said: Option<String>,
}

fn accent() -> Style {
    out::look(Role::Accent)
}

fn dim() -> Style {
    out::look(Role::Muted)
}

/// The first sentence of a help text, which is what fits under a question.
fn first_sentence(help: &str) -> &str {
    match help.find(". ") {
        Some(at) => &help[..=at],
        None => help,
    }
}

fn options_of(question: &Question) -> &[Choice] {
    match &question.asked {
        Asked::Choice { options, .. } | Asked::Many { options, .. } => options,
        _ => &[],
    }
}

/// The lines of a question while it is asked.
fn lines(question: &Question, state: &State) -> Vec<Line<'static>> {
    let mark = Span::styled("? ", accent().add_modifier(Modifier::BOLD));
    let label = Span::styled(
        question.label.clone(),
        Style::new().add_modifier(Modifier::BOLD),
    );
    let mut lines = vec![];
    match &question.asked {
        Asked::Text { secret, .. } => {
            let shown = match secret {
                true => "•".repeat(state.input.value().chars().count()),
                false => state.input.value().to_owned(),
            };
            lines.push(Line::from(vec![
                mark,
                label,
                Span::raw(" › "),
                Span::raw(shown),
            ]));
            lines.push(Line::styled(
                first_sentence(&question.help).to_owned(),
                dim(),
            ));
        }
        Asked::Confirm { default } => {
            let hint = if *default {
                " (Y/n) › "
            } else {
                " (y/N) › "
            };
            lines.push(Line::from(vec![mark, label, Span::styled(hint, dim())]));
        }
        Asked::Choice { options, .. } | Asked::Many { options, .. } => {
            let many = matches!(question.asked, Asked::Many { .. });
            let keys = match many {
                true => "  ↑↓ move · space tick · enter done",
                false => "  ↑↓ move · enter choose",
            };
            lines.push(Line::from(vec![mark, label, Span::styled(keys, dim())]));
            let first = state.cursor.saturating_sub(ROWS - 1);
            for (index, option) in options.iter().enumerate().skip(first).take(ROWS) {
                let here = index == state.cursor;
                let pointer = if here { theme::CURSOR } else { "  " };
                let tick = match (many, state.ticked.get(index)) {
                    (true, Some(true)) => "◉ ",
                    (true, _) => "○ ",
                    (false, _) => "",
                };
                let style = if here { accent() } else { Style::new() };
                lines.push(Line::from(vec![
                    Span::styled(format!("  {pointer}{tick}"), style),
                    Span::styled(option.label.clone(), style),
                ]));
            }
            let help = options
                .get(state.cursor)
                .map(|option| option.help.as_str())
                .filter(|help| !help.is_empty())
                .unwrap_or(first_sentence(&question.help));
            lines.push(Line::styled(first_sentence(help).to_owned(), dim()));
        }
    }
    if let Some(said) = &state.said {
        lines.push(Line::styled(said.clone(), out::look(Role::Bad)));
    }
    lines
}

/// What a question was answered, as the line it leaves behind shows it.
fn shown(question: &Question, answer: &Answer) -> String {
    let options = options_of(question);
    match answer {
        Answer::Text(_) if matches!(question.asked, Asked::Text { secret: true, .. }) => {
            "••••••".into()
        }
        Answer::Text(text) if text.is_empty() => "none".into(),
        Answer::Text(text) => text.clone(),
        Answer::Confirm(yes) => if *yes { "yes" } else { "no" }.into(),
        Answer::Choice(index) => options
            .get(*index)
            .map(|option| option.label.clone())
            .unwrap_or_default(),
        Answer::Many(ticked) => {
            let chosen: Vec<&str> = options
                .iter()
                .zip(ticked)
                .filter(|(_, on)| **on)
                .map(|(option, _)| option.key.as_str())
                .collect();
            match chosen.is_empty() {
                true => "none".into(),
                false => chosen.join(", "),
            }
        }
    }
}

/// What a key does to a question: its answer when it ends it.
fn key(question: &Question, state: &mut State, key: KeyEvent) -> Result<Option<Answer>> {
    let count = options_of(question).len();
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d'))
    {
        return Err(Fail::exit(130));
    }
    if key.code == KeyCode::Esc {
        return Err(Fail::exit(130));
    }
    state.said = None;
    match &question.asked {
        Asked::Text { .. } => match key.code {
            KeyCode::Enter => return Ok(Some(Answer::Text(state.input.value().to_owned()))),
            _ => {
                if let Some(key) = berth_page::keys::key(&key) {
                    berth_tui::event::edit(&mut state.input, &key);
                }
            }
        },
        Asked::Confirm { default } => match key.code {
            KeyCode::Enter => return Ok(Some(Answer::Confirm(*default))),
            KeyCode::Char('y' | 'Y') => return Ok(Some(Answer::Confirm(true))),
            KeyCode::Char('n' | 'N') => return Ok(Some(Answer::Confirm(false))),
            _ => {}
        },
        Asked::Choice { .. } | Asked::Many { .. } => match key.code {
            KeyCode::Up | KeyCode::Char('k') => state.cursor = state.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                state.cursor = (state.cursor + 1).min(count.saturating_sub(1))
            }
            KeyCode::Char(' ') if matches!(question.asked, Asked::Many { .. }) => {
                if let Some(on) = state.ticked.get_mut(state.cursor) {
                    *on = !*on;
                }
            }
            KeyCode::Enter => {
                return Ok(Some(match question.asked {
                    Asked::Many { .. } => Answer::Many(state.ticked.clone()),
                    _ => Answer::Choice(state.cursor),
                }));
            }
            _ => {}
        },
    }
    Ok(None)
}

fn start(question: &Question) -> State {
    let (input, cursor, ticked) = match &question.asked {
        Asked::Text { default, .. } => (Input::new(default.clone()), 0, vec![]),
        Asked::Choice { default, .. } => (Input::default(), *default, vec![]),
        Asked::Many { chosen, .. } => (Input::default(), 0, chosen.clone()),
        Asked::Confirm { .. } => (Input::default(), 0, vec![]),
    };
    State {
        input,
        cursor,
        ticked,
        said: None,
    }
}

/// Ask one question on the terminal, drawn on standard error.
fn on_terminal(question: &Question) -> Result<Answer> {
    let mut state = start(question);
    let height = (lines(question, &state).len() + 1) as u16;
    crossterm::terminal::enable_raw_mode()?;
    let backend = CrosstermBackend::new(std::io::stderr());
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    )?;
    let answered = (|| -> Result<Answer> {
        loop {
            terminal.draw(|frame| {
                let text = lines(question, &state);
                frame.render_widget(
                    Paragraph::new(text).wrap(Wrap { trim: false }),
                    frame.area(),
                );
                if let Asked::Text { .. } = question.asked {
                    let at = question.label.chars().count() + 5 + state.input.visual_cursor();
                    frame.set_cursor_position((at as u16, frame.area().y));
                }
            })?;
            if let Event::Key(pressed) = event::read()?
                && pressed.kind != KeyEventKind::Release
                && let Some(answer) = key(question, &mut state, pressed)?
            {
                return Ok(answer);
            }
        }
    })();
    let _ = terminal.clear();
    out::to_line_of(&mut terminal);
    let _ = crossterm::terminal::disable_raw_mode();
    drop(terminal);
    let answer = answered?;
    eprintln!(
        "{} {} › {}",
        out::styled_err("?", out::Style::Accent),
        question.label,
        out::styled_err(&shown(question, &answer), out::Style::Accent)
    );
    Ok(answer)
}

/// Asks on the terminal when there is one and `--no-input` was not given.
pub struct Person;

impl Asker for Person {
    fn ask(&mut self, question: &Question) -> std::result::Result<Option<Answer>, String> {
        if !out::get().ask {
            return Ok(None);
        }
        match on_terminal(question) {
            Ok(answer) => Ok(Some(answer)),
            Err(fail) if fail.status == 130 => Err("Stopped".into()),
            Err(fail) => Err(fail.message),
        }
    }
}

/// The asker of this run.
pub fn asker() -> Person {
    Person
}

/// Ask for a line of text, or say which flag gives it.
pub fn text(label: &str, help: &str, flag: &str) -> Result<String> {
    let question = Question {
        step: berth_tui::fields::Step::Install,
        label: label.into(),
        help: help.into(),
        flag: flag.into(),
        asked: Asked::Text {
            default: String::new(),
            secret: false,
        },
        required: true,
    };
    match ask(&question)? {
        Answer::Text(text) => Ok(text),
        _ => Err(Fail::usage(format!("{label} is missing: pass {flag}"))),
    }
}

/// Ask for a secret without showing it, or say which flag gives it.
pub fn secret(label: &str, flag: &str) -> Result<String> {
    let question = Question {
        step: berth_tui::fields::Step::Install,
        label: label.into(),
        help: String::new(),
        flag: flag.into(),
        asked: Asked::Text {
            default: String::new(),
            secret: true,
        },
        required: true,
    };
    match ask(&question)? {
        Answer::Text(text) => Ok(text),
        _ => Err(Fail::usage(format!("{label} is missing: pass {flag}"))),
    }
}

/// Ask a question, or fail with its flag when nobody can be asked.
pub fn ask(question: &Question) -> Result<Answer> {
    if !out::get().ask {
        return Err(Fail::usage(format!(
            "{} is missing: pass {}",
            question.label, question.flag
        )));
    }
    on_terminal(question).map_err(|fail| match fail.status {
        130 => Fail::exit(130),
        _ => fail,
    })
}

/// Confirm something that cannot be undone: `--yes` does, and without a
/// terminal it is refused.
pub fn confirm(question: &str) -> Result<()> {
    let out = out::get();
    if out.yes {
        return Ok(());
    }
    if !out.ask {
        return Err(Fail::refused(format!(
            "{question} Confirm with --yes, as nobody can be asked here"
        )));
    }
    let asked = Question {
        step: berth_tui::fields::Step::Run,
        label: question.into(),
        help: String::new(),
        flag: "--yes".into(),
        asked: Asked::Confirm { default: false },
        required: true,
    };
    match on_terminal(&asked)? {
        Answer::Confirm(true) => Ok(()),
        _ => Err(Fail::refused("Not confirmed, so nothing was done")),
    }
}

/// A secret from a file, from standard input, or asked for: never from the
/// command line.
pub fn secret_from(
    label: &str,
    file: Option<&std::path::Path>,
    stdin: bool,
    flag: &str,
) -> Result<String> {
    if let Some(file) = file {
        let text = std::fs::read_to_string(file)
            .map_err(|error| Fail::usage(format!("{}: {error}", file.display())))?;
        return Ok(text.trim_end_matches(['\n', '\r']).to_owned());
    }
    let piped = !std::io::IsTerminal::is_terminal(&std::io::stdin());
    if stdin || piped {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)?;
        return Ok(text.trim_end_matches(['\n', '\r']).to_owned());
    }
    secret(label, flag)
}

#[cfg(test)]
mod tests {
    use super::*;
    use berth_tui::fields::Step;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn choice() -> Question {
        Question {
            step: Step::Install,
            label: "How it is installed".into(),
            help: "The ways.".into(),
            flag: "--method METHOD".into(),
            asked: Asked::Choice {
                options: vec![
                    Choice {
                        key: "provisioner".into(),
                        label: "From a provisioner".into(),
                        help: String::new(),
                    },
                    Choice {
                        key: "iso".into(),
                        label: "USB installer".into(),
                        help: String::new(),
                    },
                ],
                default: 0,
            },
            required: true,
        }
    }

    #[test]
    fn keys_move_through_a_choice_and_enter_takes_it() {
        let question = choice();
        let mut state = start(&question);
        assert_eq!(
            key(&question, &mut state, press(KeyCode::Down)).unwrap(),
            None
        );
        let shown = lines(&question, &state);
        assert!(
            shown[2].to_string().contains("› USB installer"),
            "{shown:?}"
        );
        let answer = key(&question, &mut state, press(KeyCode::Enter)).unwrap();
        assert_eq!(answer, Some(Answer::Choice(1)));
        assert_eq!(shown_answer(&question, 1), "USB installer");
        assert!(key(&question, &mut state, press(KeyCode::Esc)).is_err());
    }

    fn shown_answer(question: &Question, index: usize) -> String {
        shown(question, &Answer::Choice(index))
    }

    #[test]
    fn typed_text_is_the_answer_and_a_secret_is_not_shown() {
        let question = Question {
            asked: Asked::Text {
                default: String::new(),
                secret: true,
            },
            ..choice()
        };
        let mut state = start(&question);
        for ch in "s3cret".chars() {
            key(&question, &mut state, press(KeyCode::Char(ch))).unwrap();
        }
        assert!(lines(&question, &state)[0].to_string().ends_with("••••••"));
        let answer = key(&question, &mut state, press(KeyCode::Enter)).unwrap();
        assert_eq!(answer, Some(Answer::Text("s3cret".into())));
        assert_eq!(shown(&question, &Answer::Text("s3cret".into())), "••••••");
    }
}
