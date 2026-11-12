//! The Plugins tab of the home screen: the plugins there are with their
//! features, installing one and removing one. berth shows those of this
//! machine or of its berth server, the page in a browser those of the server
//! it came from. What the tab asks for, each of them does its own way.

use crate::event::{self, Code, Key};
use crate::theme;
use berth_api::{PluginInfo, PluginOrigin};
use ratatui::{
    style::Style,
    text::{Line, Span},
};
use tui_input::Input;

/// Where the plugins are, for what the tab says about adding one.
pub enum Place<'a> {
    /// This machine's.
    Here,
    /// The server's at this address, for berth in a terminal.
    Server(&'a str),
    /// The server's, for the page in a browser.
    Page,
}

impl Place<'_> {
    /// Where a removed plugin is gone from.
    fn name(&self) -> &'static str {
        match self {
            Place::Here => "this machine",
            _ => "the server",
        }
    }

    /// The title of the editor for what to install.
    pub fn editor_title(&self) -> &'static str {
        match self {
            Place::Page => " Package or address of a .tgz ",
            _ => " Package, .tgz or folder ",
        }
    }

    /// What the row that adds a plugin means. `dir` is where the own ones are.
    pub fn adding(&self, dir: &str) -> String {
        match self {
            Place::Here => format!(
                "Install a plugin with npm: a package name from your registry, a .tgz from npm pack, or a folder. Registries and logins come from your npm configuration. Nothing of a plugin runs on this machine.\n\nA folder copied to {dir} works as well."
            ),
            Place::Server(url) => format!(
                "The berth server at {url} installs it with npm: a package name from its registry, or the https:// address of a .tgz. A .tgz or a folder on this machine is packed and sent to the server.\n\n{}",
                server_note(dir)
            ),
            Place::Page => format!(
                "The server installs it with npm: a package name from its registry, or the https:// address of a .tgz. u sends a .tgz from this device instead.\n\n{}",
                server_note(dir)
            ),
        }
    }
}

fn server_note(dir: &str) -> String {
    format!(
        "Registries and logins come from the server's npm configuration. npm runs none of the plugin's scripts; its tasks run when a host is applied.\n\nA folder copied to {dir} on the server works as well."
    )
}

/// The rows of the tab: each line with the plugin it belongs to, and last the
/// row that adds one.
pub fn rows(plugins: &[PluginInfo]) -> Vec<(Option<usize>, Line<'static>)> {
    let muted = Style::new().fg(theme::MUTED);
    let mut rows = vec![];
    for (index, plugin) in plugins.iter().enumerate() {
        let origin = match plugin.origin {
            PluginOrigin::Shipped => "comes with berth",
            _ => "yours",
        };
        rows.push((
            Some(index),
            Line::from(vec![
                Span::styled(plugin.label.clone(), Style::new().fg(theme::ACCENT).bold()),
                Span::styled(
                    format!("  {} {}  · {origin}", plugin.package, plugin.version),
                    muted,
                ),
            ]),
        ));
        for (number, feature) in plugin.features.iter().enumerate() {
            let branch = if number + 1 < plugin.features.len() {
                "  ├─ "
            } else {
                "  └─ "
            };
            rows.push((
                Some(index),
                Line::from(vec![
                    Span::styled(branch, muted),
                    Span::styled(feature.label.clone(), Style::new().fg(theme::TEXT)),
                    Span::styled(format!("  in {}", feature.group), muted),
                ]),
            ));
        }
    }
    rows.push((
        None,
        Line::styled("+ Add plugin", Style::new().fg(theme::ACCENT)),
    ));
    rows
}

/// What the selected row means: the plugin with its features, or what the
/// last change said.
pub fn help(plugin: Option<&PluginInfo>, report: &[String], adding: &str) -> String {
    if !report.is_empty() {
        return report.join("\n\n");
    }
    let Some(plugin) = plugin else {
        return adding.to_owned();
    };
    let mut text = format!("{}\n\n", plugin.description);
    for feature in &plugin.features {
        text += &format!("{}: {}\n\n", feature.label, feature.help);
    }
    text + &format!(
        "Its features are switched on per host, on the Features page.\n\nFrom {}",
        plugin.dir
    )
}

/// What a key on the tab asks for.
#[derive(Debug, PartialEq)]
pub enum Asked {
    /// Nothing more; maybe something to say.
    Nothing(Option<String>),
    /// Install what was typed.
    Install(String),
    /// Remove this plugin, by its id.
    Remove(String),
    /// Not a key of the tab: switching tabs and leaving are the screen's.
    Other,
}

/// The tab's state: the selected row, and whether a removal waits for a
/// second x. The editor for what to install is the screen's.
#[derive(Default)]
pub struct Tab {
    pub selected: usize,
    armed: bool,
}

impl Tab {
    /// The selected row, kept within the rows there are.
    pub fn selected(&mut self, plugins: &[PluginInfo]) -> usize {
        let count = rows(plugins).len();
        self.selected = self.selected.min(count - 1);
        self.selected
    }

    /// The plugin of the selected row, if it is one's.
    pub fn plugin<'a>(&mut self, plugins: &'a [PluginInfo]) -> Option<&'a PluginInfo> {
        let selected = self.selected(plugins);
        rows(plugins)[selected].0.map(|index| &plugins[index])
    }

    /// Forget a removal that waits, on leaving the tab.
    pub fn leave(&mut self) {
        self.armed = false;
    }

    /// Take a key, with `editor` open while what to install is typed. Moving
    /// on clears `report`, what the last change said.
    pub fn key(
        &mut self,
        key: Key,
        editor: &mut Option<Input>,
        plugins: &[PluginInfo],
        report: &mut Vec<String>,
        place: &Place,
    ) -> Asked {
        let count = rows(plugins).len();
        let selected = self.plugin(plugins).cloned();
        if let Some(input) = editor {
            match key.code {
                Code::Esc => *editor = None,
                Code::Enter => {
                    let spec = input.value().trim().to_owned();
                    *editor = None;
                    if !spec.is_empty() {
                        return Asked::Install(spec);
                    }
                }
                _ => event::edit(input, &key),
            }
            return Asked::Nothing(None);
        }
        let armed = std::mem::take(&mut self.armed);
        match key.code {
            Code::Up => {
                self.selected = self.selected.saturating_sub(1);
                report.clear();
            }
            Code::Down => {
                self.selected = (self.selected + 1).min(count - 1);
                report.clear();
            }
            Code::Enter if selected.is_none() => *editor = Some(Input::default()),
            Code::Char('x') | Code::Delete => {
                return Asked::Nothing(match selected {
                    Some(plugin) if plugin.origin == PluginOrigin::Shipped => {
                        Some(format!("{} comes with berth and stays", plugin.label))
                    }
                    Some(plugin) if armed => return Asked::Remove(plugin.id),
                    Some(plugin) => {
                        self.armed = true;
                        Some(format!(
                            "x again removes {} from {}",
                            plugin.label,
                            place.name()
                        ))
                    }
                    None => None,
                });
            }
            _ => return Asked::Other,
        }
        Asked::Nothing(None)
    }
}

/// What a change said, for the status line: its first line.
pub fn status(report: &[String]) -> String {
    report.first().cloned().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use berth_api::PluginFeature;

    fn plugin(id: &str, origin: PluginOrigin) -> PluginInfo {
        PluginInfo {
            id: id.into(),
            label: id.to_uppercase(),
            package: format!("berth-plugin-{id}"),
            version: "1.0.0".into(),
            origin,
            features: vec![PluginFeature {
                id: "cli".into(),
                label: "CLI".into(),
                group: "Development Tools".into(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn a_plugin_is_added_and_removed_from_the_tab() {
        let plugins = [
            plugin("aa", PluginOrigin::Npm),
            plugin("zz", PluginOrigin::Shipped),
        ];
        let text: Vec<String> = rows(&plugins)
            .iter()
            .map(|(_, line)| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();
        assert_eq!(text[0], "AA  berth-plugin-aa 1.0.0  · yours");
        assert_eq!(text[1], "  └─ CLI  in Development Tools");
        assert_eq!(text[4], "+ Add plugin");
        let mut tab = Tab::default();
        let mut editor = None;
        let mut report = vec!["✓ npm install x".to_owned()];
        let place = Place::Server("https://berth.example");
        let mut press = |tab: &mut Tab, code| {
            tab.key(Key::plain(code), &mut editor, &plugins, &mut report, &place)
        };
        // The one that comes with berth stays; another goes after x twice.
        assert_eq!(
            press(&mut tab, Code::Char('x')),
            Asked::Nothing(Some("x again removes AA from the server".into()))
        );
        assert_eq!(press(&mut tab, Code::Char('x')), Asked::Remove("aa".into()));
        press(&mut tab, Code::Down);
        press(&mut tab, Code::Down);
        assert_eq!(
            press(&mut tab, Code::Char('x')),
            Asked::Nothing(Some("ZZ comes with berth and stays".into()))
        );
        assert_eq!(press(&mut tab, Code::Char('1')), Asked::Other);
        for _ in 0..3 {
            press(&mut tab, Code::Down);
        }
        press(&mut tab, Code::Enter);
        for ch in "@acme/foo".chars() {
            press(&mut tab, Code::Char(ch));
        }
        assert_eq!(
            press(&mut tab, Code::Enter),
            Asked::Install("@acme/foo".into())
        );
        assert!(report.is_empty() && editor.is_none());
        assert!(help(None, &[], &place.adding("/srv/plugins")).contains("https://berth.example"));
    }
}
