use crate::{Result, discovery::expand_path, theme};
use ratatui::{
    layout::Rect,
    style::Style,
    widgets::{Clear, List, ListState},
};
use std::{fs, path::PathBuf};

pub struct Picker {
    pub directory: PathBuf,
    pub entries: Vec<PathBuf>,
    pub state: ListState,
    pub area: Rect,
}

impl Picker {
    pub fn new(value: &str) -> Result<Self> {
        let path = expand_path(value);
        let directory = if path.is_dir() {
            path
        } else {
            path.parent()
                .filter(|p| p.is_dir())
                .unwrap_or(std::path::Path::new("."))
                .to_owned()
        };
        let mut picker = Self {
            directory,
            entries: vec![],
            state: ListState::default(),
            area: Rect::default(),
        };
        picker.reload()?;
        Ok(picker)
    }
    fn reload(&mut self) -> Result<()> {
        self.directory = self.directory.canonicalize()?;
        let mut entries = fs::read_dir(&self.directory)?
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .collect::<Vec<_>>();
        entries.sort_by_key(|path| (!path.is_dir(), path.file_name().map(|n| n.to_owned())));
        self.entries = self
            .directory
            .parent()
            .map(|p| vec![p.to_owned()])
            .unwrap_or_default();
        self.entries.extend(entries);
        self.state = ListState::default().with_selected(Some(0));
        Ok(())
    }
    pub fn choose(&mut self) -> Result<Option<String>> {
        let Some(path) = self
            .entries
            .get(self.state.selected().unwrap_or(0))
            .cloned()
        else {
            return Ok(None);
        };
        if path.is_dir() {
            self.directory = path;
            self.reload()?;
            Ok(None)
        } else {
            Ok(Some(path.display().to_string()))
        }
    }
    pub fn move_by(&mut self, up: bool) {
        let index = self.state.selected().unwrap_or(0);
        self.state.select(Some(if up {
            index.saturating_sub(1)
        } else {
            (index + 1).min(self.entries.len().saturating_sub(1))
        }));
    }
    pub fn draw(&mut self, frame: &mut ratatui::Frame) {
        self.area = frame.area().inner(ratatui::layout::Margin {
            horizontal: 4,
            vertical: 3,
        });
        frame.render_widget(Clear, self.area);
        let entries = self
            .entries
            .iter()
            .map(|path| {
                let name = if path == self.directory.parent().unwrap_or(&self.directory) {
                    "..".into()
                } else {
                    path.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned()
                };
                format!("{} {name}", if path.is_dir() { "▸" } else { " " })
            })
            .collect::<Vec<_>>();
        frame.render_stateful_widget(
            List::new(entries)
                .block(
                    theme::popup(&self.directory.display().to_string(), theme::ACCENT)
                        .title_bottom(theme::hints(
                            "Enter open/select · Ctrl+Enter use directory · Esc cancel",
                        )),
                )
                .highlight_style(Style::new().bg(theme::RAISED)),
            self.area,
            &mut self.state,
        );
    }
}
