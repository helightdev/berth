//! The form of an action that asks for much: every argument on a row of its
//! own, laid out and edited as a host's set-up is. A value of one line is
//! typed in the box under the rows, one of a few values goes on to the next,
//! and a value of several lines opens the text editor.

use crate::event::{self, Code, Event, Key};
use crate::fields::Kind;
use crate::page::{Launch, argument_label};
use crate::screen::{self, Platform, draw_editor, draw_footer, draw_header, inside};
use crate::setup::value_line;
use crate::text::{Outcome, TextEditor};
use crate::{markdown, theme};
use berth_api::{Action, Argument};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Margin, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Clear, List, ListState, Paragraph, Wrap},
};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use tui_input::Input;

/// Whether an action's arguments are asked for in a form: a value of several
/// lines needs the editor, and more than two values are too many to ask for
/// one after another.
pub fn wants_form(action: &Action) -> bool {
    action.arguments.len() > 2
        || action
            .arguments
            .iter()
            .any(|argument| argument.kind == Argument::TEXT)
}

/// A row of the form the cursor stops on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Row {
    /// The provisioner that makes it, where several can.
    Maker,
    Argument(usize),
    Run,
}

/// What a key on the form leads to.
#[derive(Debug, PartialEq)]
pub enum Did {
    /// It stays open; maybe something to say.
    Stay(Option<String>),
    /// It was left without running.
    Leave,
    /// Another provisioner was chosen, by its name: its action is to be
    /// asked for, and given to [`Form::fill`].
    Switch(String),
    /// Run the action with these values.
    Run(Launch),
}

pub struct Form {
    /// Where the action is run and what it is, for the top line.
    place: String,
    what: String,
    /// The name of the action, which a provisioner chosen later has too.
    wanted: String,
    /// The provisioners that can run it, as (name, label), when the form
    /// chooses among them.
    makers: Vec<(String, String)>,
    maker: usize,
    /// The action with its index among its subject's; none while it is
    /// asked for.
    action: Option<(usize, Action)>,
    /// Why there is no action, once that is known.
    problem: String,
    values: BTreeMap<String, String>,
    /// The arguments whose values were set here, which stay when another
    /// provisioner is chosen.
    set: BTreeSet<String>,
    /// The values an argument takes, where it is one of a few.
    offered: BTreeMap<String, Vec<String>>,
    cursor: usize,
    /// The line being typed, for the argument under the cursor.
    editing: Option<Input>,
    text: Option<TextEditor>,
    /// The suggestion the cursor is on while the suggestions have the keys.
    suggested: Option<usize>,
    /// Where the rows were drawn, with the place of each among those the
    /// cursor stops on, and where the suggestions were.
    hits: RefCell<Vec<(Rect, usize)>>,
    suggestion_hits: RefCell<Vec<(Rect, usize)>>,
}

impl Form {
    fn empty(place: &str, what: &str, wanted: &str) -> Form {
        Form {
            place: place.to_owned(),
            what: what.to_owned(),
            wanted: wanted.to_owned(),
            makers: vec![],
            maker: 0,
            action: None,
            problem: String::new(),
            values: BTreeMap::new(),
            set: BTreeSet::new(),
            offered: BTreeMap::new(),
            cursor: 0,
            editing: None,
            text: None,
            suggested: None,
            hits: RefCell::new(vec![]),
            suggestion_hits: RefCell::new(vec![]),
        }
    }

    /// The form of the action at `index` of the actions of `place`, a
    /// provisioner or a resource. `narrowed` has the values an argument
    /// takes there, where those are fewer than it declares.
    pub fn of(
        place: &str,
        index: usize,
        action: &Action,
        narrowed: &BTreeMap<String, Vec<String>>,
    ) -> Form {
        let mut form = Form::empty(place, &action.title().to_lowercase(), &action.name);
        form.fill(index, action, narrowed);
        form
    }

    /// The form that makes something with one of `makers`, provisioners as
    /// (name, label), by their action `wanted`. It starts with the first,
    /// whose action is asked for.
    pub fn making(what: &str, wanted: &str, makers: Vec<(String, String)>) -> Form {
        let place = makers.first().map_or("", |(name, _)| name.as_str());
        let mut form = Form::empty(place, what, wanted);
        form.makers = makers;
        form
    }

    /// Open it again for the same action, now at `index` of its subject's.
    pub fn reopen(&mut self, index: usize) {
        if let Some((at, _)) = &mut self.action {
            *at = index;
        }
    }

    /// The name of the action the form is of.
    pub fn wanted(&self) -> &str {
        &self.wanted
    }

    /// Whether it chooses the provisioner that runs the action.
    pub fn makes(&self) -> bool {
        !self.makers.is_empty()
    }

    /// Whether the action is still asked for.
    pub fn waiting(&self) -> bool {
        self.action.is_none() && self.problem.is_empty()
    }

    /// Take the action, with every argument at its default unless it was
    /// set here already and takes that value.
    pub fn fill(
        &mut self,
        index: usize,
        action: &Action,
        narrowed: &BTreeMap<String, Vec<String>>,
    ) {
        self.offered = action
            .arguments
            .iter()
            .filter_map(|argument| {
                let values = narrowed.get(&argument.name).unwrap_or(&argument.values);
                (!values.is_empty()).then(|| (argument.name.clone(), values.clone()))
            })
            .collect();
        let before = std::mem::take(&mut self.values);
        self.set
            .retain(|name| action.arguments.iter().any(|known| known.name == *name));
        for argument in &action.arguments {
            let kept = before.get(&argument.name).filter(|value| {
                self.set.contains(&argument.name)
                    && self
                        .offered
                        .get(&argument.name)
                        .is_none_or(|values| values.contains(value))
            });
            let value = match kept {
                Some(value) => value.clone(),
                None => {
                    self.set.remove(&argument.name);
                    argument.default.clone()
                }
            };
            self.values.insert(argument.name.clone(), value);
        }
        self.problem.clear();
        self.action = Some((index, action.clone()));
    }

    /// There is no action to fill it with, and why.
    pub fn fail(&mut self, why: String) {
        self.action = None;
        self.problem = why;
    }

    /// What the top line says is open, such as `docker › new instance`.
    pub fn context(&self) -> String {
        format!("{} › {}", self.place, self.what)
    }

    fn rows(&self) -> Vec<Row> {
        let mut rows = vec![];
        if self.makes() {
            rows.push(Row::Maker);
        }
        if let Some((_, action)) = &self.action {
            rows.extend((0..action.arguments.len()).map(Row::Argument));
            rows.push(Row::Run);
        }
        rows
    }

    fn selected(&self) -> Option<Row> {
        let rows = self.rows();
        rows.get(self.cursor.min(rows.len().saturating_sub(1)))
            .copied()
    }

    fn argument(&self, index: usize) -> Option<&Argument> {
        self.action.as_ref()?.1.arguments.get(index)
    }

    fn value(&self, argument: &Argument) -> &str {
        self.values.get(&argument.name).map_or("", String::as_str)
    }

    /// What the row under the cursor can be set to, where that is one of a
    /// few: each as it is shown, with the place of the one it is set to.
    fn suggestions(&self) -> (Vec<String>, Option<usize>) {
        match self.selected() {
            Some(Row::Maker) => (
                self.makers
                    .iter()
                    .map(|(name, label)| match name == label {
                        true => name.clone(),
                        false => format!("{label} · {name}"),
                    })
                    .collect(),
                Some(self.maker),
            ),
            Some(Row::Argument(index)) => {
                let Some(argument) = self.argument(index) else {
                    return (vec![], None);
                };
                let values = self
                    .offered
                    .get(&argument.name)
                    .cloned()
                    .unwrap_or_default();
                let at = values
                    .iter()
                    .position(|known| known == self.value(argument));
                (values, at)
            }
            _ => (vec![], None),
        }
    }

    /// Set the row under the cursor to the suggestion at `place`.
    fn suggest(&mut self, place: usize) -> Did {
        match self.selected() {
            Some(Row::Maker) if place < self.makers.len() && place != self.maker => {
                self.maker = place;
                let name = self.makers[place].0.clone();
                self.place = name.clone();
                self.action = None;
                self.problem.clear();
                Did::Switch(name)
            }
            Some(Row::Argument(index)) => {
                let value = self
                    .argument(index)
                    .and_then(|argument| self.offered.get(&argument.name))
                    .and_then(|values| values.get(place))
                    .cloned();
                if let Some(value) = value {
                    self.put(index, value);
                }
                Did::Stay(None)
            }
            _ => Did::Stay(None),
        }
    }

    /// The first needed argument that has no value.
    fn missing(&self) -> Option<usize> {
        let (_, action) = self.action.as_ref()?;
        action
            .arguments
            .iter()
            .position(|argument| argument.required && self.value(argument).trim().is_empty())
    }

    fn put(&mut self, index: usize, value: String) {
        if let Some(name) = self.argument(index).map(|argument| argument.name.clone()) {
            self.set.insert(name.clone());
            self.values.insert(name, value);
        }
    }

    fn open_editor(&mut self, index: usize) {
        if let Some(argument) = self.argument(index) {
            let editor = TextEditor::new(argument_label(argument), self.value(argument));
            self.text = Some(editor);
        }
    }

    /// The lines of the form, each with the row the cursor stops on there.
    fn lines(&self) -> Vec<(Line<'static>, Option<Row>)> {
        let muted = Style::new().fg(theme::MUTED);
        let title = match &self.action {
            Some((_, action)) => action.title().to_owned(),
            None => self.what.clone(),
        };
        let mut heading = title.chars();
        let heading: String = heading
            .next()
            .map(|first| first.to_uppercase().chain(heading).collect())
            .unwrap_or_default();
        let mut lines = vec![(
            Line::styled(heading, Style::new().fg(theme::ACCENT).bold()),
            None,
        )];
        if let Some((name, label)) = self.makers.get(self.maker) {
            let mut line = value_line("  Provisioner", label, Kind::Text);
            if name != label {
                line.spans.push(Span::styled(format!("  · {name}"), muted));
            }
            lines.push((line, Some(Row::Maker)));
        }
        let Some((_, action)) = &self.action else {
            let said = match self.problem.is_empty() {
                true => Line::styled("  Asking the provisioner what it needs…", muted.italic()),
                false => Line::styled(format!("  ✗ {}", self.problem), Style::new().fg(theme::BAD)),
            };
            lines.push((said, None));
            return lines;
        };
        for (index, argument) in action.arguments.iter().enumerate() {
            let name = match argument.required {
                true => format!("  {} *", argument_label(argument)),
                false => format!("  {}", argument_label(argument)),
            };
            let value = self.value(argument);
            let line = match (argument.secret, argument.kind == Argument::TEXT) {
                (true, _) => value_line(&name, value, Kind::Secret),
                // A text shows its first line, and how many it has.
                (false, true) if !value.is_empty() => {
                    let count = value.lines().count();
                    let first = value.lines().find(|line| !line.trim().is_empty());
                    let mut line = value_line(&name, first.unwrap_or("").trim(), Kind::Text);
                    let lines = match count {
                        1 => "1 line".to_owned(),
                        count => format!("{count} lines"),
                    };
                    line.spans.push(Span::styled(format!("  · {lines}"), muted));
                    line
                }
                _ => value_line(&name, value, Kind::Text),
            };
            lines.push((line, Some(Row::Argument(index))));
        }
        lines.push((Line::raw(""), None));
        lines.push((
            Line::styled(
                format!("[Run · {}]", action.title()),
                Style::new().fg(theme::TEXT),
            ),
            Some(Row::Run),
        ));
        lines
    }

    /// What the information box says of the row under the cursor.
    fn help(&self) -> String {
        match self.selected() {
            Some(Row::Maker) => {
                let all: Vec<String> = self
                    .makers
                    .iter()
                    .map(|(name, label)| match name == label {
                        true => format!("- `{name}`"),
                        false => format!("- `{name}`: {label}"),
                    })
                    .collect();
                let how = match self.makers.len() {
                    1 => "It is the only one that does.",
                    _ => "Enter or Space goes on to the next, and → chooses from the suggestions.",
                };
                format!(
                    "The provisioner that makes it. {how}\n\n{}\n\nA provisioner is registered on the Provisioners tab.",
                    all.join("\n")
                )
            }
            Some(Row::Argument(index)) => {
                let Some(argument) = self.argument(index) else {
                    return String::new();
                };
                let mut text = argument.help.clone();
                if let Some(values) = self.offered.get(&argument.name) {
                    text += &format!(
                        "\n\nOne of {}. Enter or Space goes on to the next, and → chooses from the suggestions.",
                        values
                            .iter()
                            .map(|value| format!("`{value}`"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                } else if argument.kind == Argument::TEXT {
                    text += "\n\nEnter opens the editor, which takes the whole screen. berth keeps the text itself, so no file is needed.";
                }
                if argument.required {
                    text += "\n\nIt is needed.";
                }
                text.trim().to_owned()
            }
            Some(Row::Run) => {
                let Some((_, action)) = &self.action else {
                    return String::new();
                };
                let mut text = action.help.clone();
                if let Some(argument) = self.missing().and_then(|index| self.argument(index)) {
                    text += &format!("\n\n{} is needed first.", argument_label(argument));
                }
                text.trim().to_owned()
            }
            None if self.problem.is_empty() => "Asking the provisioner what it needs.".into(),
            None => self.problem.clone(),
        }
    }

    /// Draw the form over the whole screen, as a host's set-up is drawn, or
    /// the text editor while a text is edited. `status` is the last message.
    pub fn draw(&self, frame: &mut Frame, status: &str) {
        self.hits.borrow_mut().clear();
        if let Some(editor) = &self.text {
            return editor.draw(frame, &self.context(), status);
        }
        frame.render_widget(Clear, frame.area());
        frame.render_widget(theme::backdrop(), frame.area());
        let area = frame.area().inner(Margin {
            horizontal: 2,
            vertical: 1,
        });
        let [header, _, panel, _, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(4),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(area);
        draw_header(frame, header, &[], 0, &self.context());
        let [left, _, right] = Layout::horizontal([
            Constraint::Percentage(62),
            Constraint::Length(2),
            Constraint::Min(0),
        ])
        .areas(panel);
        let [body, edit] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(if self.editing.is_some() { 3 } else { 0 }),
        ])
        .areas(left);
        // Right column: what the row can be set to, above what it means.
        let (suggestions, current) = self.suggestions();
        let [choices, help] = Layout::vertical([
            Constraint::Length(match suggestions.len() {
                0 => 0,
                count => (count as u16 + 2).min(10),
            }),
            Constraint::Min(3),
        ])
        .areas(right);
        self.suggestion_hits.borrow_mut().clear();
        if !suggestions.is_empty() {
            let width = choices.width.saturating_sub(4) as usize;
            let mut state = ListState::default().with_selected(self.suggested.or(current));
            frame.render_stateful_widget(
                List::new(
                    suggestions
                        .iter()
                        .map(|label| screen::ellipsize(label, width)),
                )
                .block(theme::panel(
                    "Suggestions · → to choose",
                    self.suggested.is_some(),
                ))
                .highlight_style(Style::new().bg(theme::RAISED)),
                choices,
                &mut state,
            );
            for index in state.offset()..suggestions.len() {
                let y = choices.y + 1 + (index - state.offset()) as u16;
                if y + 1 >= choices.bottom() {
                    break;
                }
                let rect = Rect::new(choices.x + 1, y, choices.width.saturating_sub(2), 1);
                self.suggestion_hits.borrow_mut().push((rect, index));
            }
        }
        frame.render_widget(
            Paragraph::new(markdown::render(&self.help()))
                .block(theme::panel("Information", false))
                .wrap(Wrap { trim: false }),
            help,
        );
        let lines = self.lines();
        let selected = self.selected();
        let at = lines
            .iter()
            .position(|(_, row)| row.is_some() && *row == selected);
        let mut state = ListState::default().with_selected(at);
        frame.render_stateful_widget(
            List::new(lines.iter().map(|(line, _)| line.clone()))
                .highlight_symbol(theme::cursor())
                .highlight_style(Style::new().bg(theme::RAISED)),
            body,
            &mut state,
        );
        let mut place = 0;
        for (index, (_, row)) in lines.iter().enumerate() {
            if row.is_none() {
                continue;
            }
            let y = body.y as usize + index.wrapping_sub(state.offset());
            if index >= state.offset() && y < body.bottom() as usize {
                let rect = Rect::new(body.x, y as u16, body.width, 1);
                self.hits.borrow_mut().push((rect, place));
            }
            place += 1;
        }
        let secret = match selected {
            Some(Row::Argument(index)) => self.argument(index).is_some_and(|known| known.secret),
            _ => false,
        };
        if let (Some(input), Some(Row::Argument(index))) = (&self.editing, selected) {
            let title = self.argument(index).map_or("Edit", argument_label);
            draw_editor(frame, edit, title, input, secret);
        }
        let keys = match (&self.editing, secret, self.suggested) {
            (Some(_), true, _) => "Enter accept · Esc cancel".to_owned(),
            (Some(_), false, _) => format!("Enter accept · Esc cancel · {}", screen::COPY_FIELD),
            (None, _, Some(_)) => "↑↓ select · Enter choose · Esc back to the rows".to_owned(),
            (None, _, None) => "↑↓ select · Enter choose · x unset · Esc back".to_owned(),
        };
        draw_footer(frame, footer, &keys, status, theme::ACCENT);
    }

    /// A click at a place of the screen: it selects the row there, and on
    /// the row selected it is the Enter key, which is given back.
    pub fn click(&mut self, column: u16, row: u16) -> Option<Key> {
        if self.editing.is_some() || self.text.is_some() {
            return None;
        }
        // A suggestion that is clicked is chosen as Enter on it would.
        let suggestion = self
            .suggestion_hits
            .borrow()
            .iter()
            .find(|(rect, _)| inside(*rect, column, row))
            .map(|(_, place)| *place);
        if let Some(place) = suggestion {
            self.suggested = Some(place);
            return Some(Key::plain(Code::Enter));
        }
        self.suggested = None;
        let place = self
            .hits
            .borrow()
            .iter()
            .find(|(rect, _)| inside(*rect, column, row))
            .map(|(_, place)| *place)?;
        if place == self.cursor {
            return Some(Key::plain(Code::Enter));
        }
        self.cursor = place;
        None
    }

    /// Put pasted text where text is typed. On the row of a text it opens
    /// the editor with it, where Ctrl+S keeps it.
    pub fn paste(&mut self, text: &str) {
        if let Some(editor) = &mut self.text {
            return editor.paste(text);
        }
        if let Some(input) = &mut self.editing {
            return event::paste(input, text.trim());
        }
        if let Some(Row::Argument(index)) = self.selected()
            && self
                .argument(index)
                .is_some_and(|argument| argument.kind == Argument::TEXT)
        {
            self.open_editor(index);
            if let Some(editor) = &mut self.text {
                editor.paste(text);
            }
        }
    }

    /// A key on the form. `platform` copies what is typed.
    pub fn key(&mut self, key: Key, platform: &mut dyn Platform) -> Did {
        if let Some(mut editor) = self.text.take() {
            match editor.handle(&key) {
                Outcome::Editing => self.text = Some(editor),
                Outcome::Keep => {
                    if let Some(Row::Argument(index)) = self.selected() {
                        self.put(index, editor.text());
                    }
                }
                Outcome::Leave => return Did::Stay(Some("Left as it was".into())),
            }
            return Did::Stay(None);
        }
        let selected = self.selected();
        if let Some(mut input) = self.editing.take() {
            let secret = match selected {
                Some(Row::Argument(index)) => {
                    self.argument(index).is_some_and(|known| known.secret)
                }
                _ => false,
            };
            match key.code {
                Code::Esc => {}
                Code::Enter => {
                    if let Some(Row::Argument(index)) = selected {
                        self.put(index, input.value().trim().to_owned());
                    }
                }
                _ if screen::is_copy_field(&key) => {
                    let said = match secret {
                        true => "A secret is not copied".to_owned(),
                        false => screen::copy_field(platform, &input),
                    };
                    self.editing = Some(input);
                    return Did::Stay(Some(said));
                }
                _ => {
                    event::edit(&mut input, &key);
                    self.editing = Some(input);
                }
            }
            return Did::Stay(None);
        }
        if let Some(at) = self.suggested {
            let count = self.suggestions().0.len();
            match key.code {
                Code::Up | Code::Char('k') => self.suggested = Some(at.saturating_sub(1)),
                Code::Down | Code::Char('j') => {
                    self.suggested = Some((at + 1).min(count.saturating_sub(1)))
                }
                Code::Enter | Code::Char(' ') => {
                    self.suggested = None;
                    return self.suggest(at);
                }
                Code::Left | Code::Esc => self.suggested = None,
                _ => {}
            }
            return Did::Stay(None);
        }
        let count = self.rows().len();
        match key.code {
            Code::Esc => return Did::Leave,
            // The suggestions take the keys, starting from the one chosen.
            Code::Right => {
                let (suggestions, current) = self.suggestions();
                if !suggestions.is_empty() {
                    self.suggested = Some(current.unwrap_or(0));
                }
            }
            // Space goes on to the next of a few values, as on a set-up.
            Code::Char(' ') if !self.suggestions().0.is_empty() => return self.enter(selected),
            Code::Up | Code::Char('k') if count > 0 => {
                self.cursor = (self.cursor.min(count - 1) + count - 1) % count
            }
            Code::Down | Code::Char('j') if count > 0 => {
                self.cursor = (self.cursor.min(count - 1) + 1) % count
            }
            Code::Char('x') => {
                if let Some(Row::Argument(index)) = selected {
                    self.put(index, String::new());
                }
            }
            Code::Enter => return self.enter(selected),
            _ => {}
        }
        Did::Stay(None)
    }

    fn enter(&mut self, selected: Option<Row>) -> Did {
        match selected {
            Some(Row::Maker) if self.makers.len() > 1 => {
                self.suggest((self.maker + 1) % self.makers.len())
            }
            Some(Row::Maker) => Did::Stay(Some("No other provisioner makes these".into())),
            Some(Row::Argument(index)) => {
                let Some(argument) = self.argument(index) else {
                    return Did::Stay(None);
                };
                let value = self.value(argument).to_owned();
                // One of a few values goes on to the next, as a choice of the set-up does.
                if let Some(values) = self.offered.get(&argument.name) {
                    let next = values
                        .iter()
                        .position(|known| *known == value)
                        .map_or(0, |at| (at + 1) % values.len());
                    let next = values[next].clone();
                    self.put(index, next);
                } else if argument.kind == Argument::TEXT {
                    self.open_editor(index);
                } else if argument.secret {
                    // A secret is typed anew, never shown.
                    self.editing = Some(Input::default());
                } else {
                    self.editing = Some(Input::new(value));
                }
                Did::Stay(None)
            }
            Some(Row::Run) => {
                if let Some(index) = self.missing() {
                    let label = self.argument(index).map_or("", argument_label).to_owned();
                    self.cursor = self
                        .rows()
                        .iter()
                        .position(|row| *row == Row::Argument(index))
                        .unwrap_or(self.cursor);
                    return Did::Stay(Some(format!("{label} is needed")));
                }
                match &self.action {
                    Some((index, _)) => Did::Run((*index, self.values.clone())),
                    None => Did::Stay(None),
                }
            }
            None => Did::Stay(None),
        }
    }
}

/// An event for the form, from a key, a paste or a click.
pub fn event(form: &mut Form, event: &Event, platform: &mut dyn Platform) -> Did {
    match event {
        Event::Key(key) => form.key(*key, platform),
        Event::Paste(text) => {
            form.paste(text);
            Did::Stay(None)
        }
        Event::Mouse(mouse) if mouse.click == crate::event::Click::Left => {
            match form.click(mouse.column, mouse.row) {
                Some(key) => form.key(key, platform),
                None => Did::Stay(None),
            }
        }
        _ => Did::Stay(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Nowhere;

    impl Platform for Nowhere {
        fn copy(&mut self, _: &str) -> bool {
            false
        }
        fn paste(&mut self) -> Option<String> {
            None
        }
        fn open(&mut self, _: &str) -> Result<(), String> {
            Ok(())
        }
    }

    fn argument(name: &str) -> Argument {
        Argument {
            name: name.into(),
            ..Argument::default()
        }
    }

    fn project() -> Action {
        Action {
            name: "new-deployment".into(),
            label: "New compose project".into(),
            arguments: vec![
                Argument {
                    required: true,
                    ..argument("name")
                },
                Argument {
                    required: true,
                    kind: Argument::TEXT.into(),
                    label: "Compose file".into(),
                    ..argument("spec")
                },
                Argument {
                    default: "no".into(),
                    values: vec!["no".into(), "always".into()],
                    ..argument("restart")
                },
            ],
            ..Action::default()
        }
    }

    fn press(form: &mut Form, code: Code) -> Did {
        form.key(Key::plain(code), &mut Nowhere)
    }

    fn text(form: &Form) -> Vec<String> {
        form.lines()
            .iter()
            .map(|(line, _)| line.to_string())
            .collect()
    }

    #[test]
    fn a_form_is_for_a_text_or_for_more_than_two_arguments() {
        let two = Action {
            arguments: vec![argument("name"), argument("type")],
            ..Action::default()
        };
        assert!(!wants_form(&two));
        assert!(wants_form(&project()));
        let text = Action {
            arguments: vec![Argument {
                kind: Argument::TEXT.into(),
                ..argument("spec")
            }],
            ..Action::default()
        };
        assert!(wants_form(&text));
    }

    #[test]
    fn its_rows_are_typed_chosen_and_run() {
        let mut form = Form::of("docker", 3, &project(), &BTreeMap::new());
        assert_eq!(form.context(), "docker › new compose project");
        assert_eq!(
            text(&form),
            [
                "New compose project",
                "  name *  Not set",
                "  Compose file *  Not set",
                "  restart  no",
                "",
                "[Run · New compose project]"
            ]
        );
        // Run says what is needed first, and goes there.
        form.cursor = 3;
        assert_eq!(
            press(&mut form, Code::Enter),
            Did::Stay(Some("name is needed".into()))
        );
        assert_eq!(form.selected(), Some(Row::Argument(0)));
        // A line is typed in the box under the rows; Esc leaves the value.
        press(&mut form, Code::Enter);
        for ch in "wiki".chars() {
            press(&mut form, Code::Char(ch));
        }
        press(&mut form, Code::Esc);
        assert_eq!(form.values["name"], "");
        press(&mut form, Code::Enter);
        for ch in "wiki".chars() {
            press(&mut form, Code::Char(ch));
        }
        press(&mut form, Code::Enter);
        assert_eq!(form.values["name"], "wiki");
        // A text opens the editor, which Ctrl+S keeps and Esc leaves.
        press(&mut form, Code::Down);
        press(&mut form, Code::Enter);
        assert!(form.text.is_some());
        form.paste("services:\n\tweb:\n");
        assert_eq!(form.key(Key::ctrl('s'), &mut Nowhere), Did::Stay(None));
        assert_eq!(form.values["spec"], "services:\n  web:\n");
        assert!(text(&form)[2].ends_with("services:  · 2 lines"));
        press(&mut form, Code::Enter);
        form.paste("more");
        assert!(matches!(press(&mut form, Code::Esc), Did::Stay(Some(_))));
        assert_eq!(form.values["spec"], "services:\n  web:\n");
        // One of a few values goes on to the next; x unsets a value.
        press(&mut form, Code::Down);
        press(&mut form, Code::Enter);
        assert_eq!(form.values["restart"], "always");
        press(&mut form, Code::Char('x'));
        assert_eq!(form.values["restart"], "");
        press(&mut form, Code::Down);
        let Did::Run((index, values)) = press(&mut form, Code::Enter) else {
            panic!("not run");
        };
        assert_eq!((index, values["name"].as_str()), (3, "wiki"));
        assert_eq!(press(&mut form, Code::Esc), Did::Leave);
    }

    fn screen(form: &Form, status: &str) -> String {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(110, 16)).unwrap();
        terminal.draw(|frame| form.draw(frame, status)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..16)
            .map(|y| {
                (0..110)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn it_is_drawn_as_a_set_up_is_and_the_editor_takes_the_screen() {
        let makers = vec![("docker".to_owned(), "Docker container".to_owned())];
        let mut form = Form::making("new compose project", "new-deployment", makers);
        form.fill(0, &project(), &BTreeMap::new());
        let shown = screen(&form, "name is needed");
        assert!(shown.contains("berth") && shown.contains("docker › new compose project"));
        assert!(
            shown.contains("▌   Provisioner  Docker container  · docker"),
            "{shown}"
        );
        assert!(shown.contains("╭ Information "), "{shown}");
        assert!(shown.contains("↑↓ select") && shown.contains("name is needed"));
        // The box a line is typed in is under the rows, with the field's name.
        press(&mut form, Code::Down);
        press(&mut form, Code::Enter);
        let shown = screen(&form, "");
        assert!(shown.contains("╭ name "), "{shown}");
        assert!(shown.contains("Enter accept"));
        press(&mut form, Code::Esc);
        // The editor has the screen to itself, with the lines numbered.
        press(&mut form, Code::Down);
        press(&mut form, Code::Enter);
        form.paste("services:\n  web:\n    image: nginx");
        let shown = screen(&form, "");
        assert!(
            shown.contains("╭ Compose file · line 3 of 3, column 17 "),
            "{shown}"
        );
        assert!(shown.contains(" 2   web:") && shown.contains("Ctrl+S keep"));
        assert!(!shown.contains("Information") && !shown.contains("Provisioner"));
    }

    #[test]
    fn it_chooses_among_the_provisioners_that_make_the_thing() {
        let makers = vec![
            ("docker".to_owned(), "Docker container".to_owned()),
            ("incus".to_owned(), "Incus container".to_owned()),
        ];
        let mut form = Form::making("new instance", "new-instance", makers);
        assert!(form.makes() && form.waiting());
        assert_eq!(form.context(), "docker › new instance");
        assert!(text(&form)[2].contains("Asking the provisioner"));
        let mut made = Action {
            name: "new-instance".into(),
            arguments: vec![
                argument("name"),
                Argument {
                    default: "debian:12".into(),
                    ..argument("image")
                },
            ],
            ..Action::default()
        };
        form.fill(1, &made, &BTreeMap::new());
        assert_eq!(text(&form)[1], "  Provisioner  Docker container  · docker");
        press(&mut form, Code::Down);
        press(&mut form, Code::Enter);
        press(&mut form, Code::Char('a'));
        press(&mut form, Code::Enter);
        // The next provisioner's action has its own defaults; what was typed stays.
        press(&mut form, Code::Up);
        assert_eq!(press(&mut form, Code::Enter), Did::Switch("incus".into()));
        assert!(form.waiting());
        made.arguments[1].default = "ubuntu/24.04".into();
        form.fill(0, &made, &BTreeMap::new());
        assert_eq!(form.context(), "incus › new instance");
        assert_eq!(
            (form.values["name"].as_str(), form.values["image"].as_str()),
            ("a", "ubuntu/24.04")
        );
        // Space goes on as Enter does, and → chooses from the suggestions.
        assert_eq!(
            press(&mut form, Code::Char(' ')),
            Did::Switch("docker".into())
        );
        form.fill(1, &made, &BTreeMap::new());
        let shown = screen(&form, "");
        assert!(shown.contains("╭ Suggestions · → to choose "), "{shown}");
        assert!(shown.contains("Incus container · incus"), "{shown}");
        press(&mut form, Code::Right);
        assert_eq!(press(&mut form, Code::Down), Did::Stay(None));
        assert_eq!(press(&mut form, Code::Enter), Did::Switch("incus".into()));
        form.fill(0, &made, &BTreeMap::new());
        // One that cannot make it says why, and another can still be chosen.
        assert_eq!(press(&mut form, Code::Enter), Did::Switch("docker".into()));
        form.fail("docker does not answer".into());
        assert!(!form.waiting());
        assert_eq!(text(&form)[2], "  ✗ docker does not answer");
        assert_eq!(press(&mut form, Code::Enter), Did::Switch("incus".into()));
    }
}
