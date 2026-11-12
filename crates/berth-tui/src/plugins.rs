//! The Plugins tab of the home screen: the plugins there are with their
//! features, installing, updating and removing one, and bringing the hosts
//! that are behind up to date afterwards. berth shows those of this
//! machine or of its berth server, the page in a browser those of the server
//! it came from. What the tab asks for, each of them does its own way.

use crate::event::{self, Code, Key};
use crate::theme;
use berth_api::{HostBehind, PluginInfo, PluginOrigin, PluginReport, PluginUpdate, Rollout};
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
/// row that adds one. A plugin with a newer version, as the last check
/// found, says so.
pub fn rows(
    plugins: &[PluginInfo],
    updates: &[PluginUpdate],
) -> Vec<(Option<usize>, Line<'static>)> {
    let muted = Style::new().fg(theme::MUTED);
    let mut rows = vec![];
    for (index, plugin) in plugins.iter().enumerate() {
        let origin = match plugin.origin {
            PluginOrigin::Shipped => "comes with berth",
            _ => "yours",
        };
        let mut spans = vec![
            Span::styled(plugin.label.clone(), Style::new().fg(theme::ACCENT).bold()),
            Span::styled(
                format!("  {} {}  · {origin}", plugin.package, plugin.version),
                muted,
            ),
        ];
        if let Some(latest) = updates
            .iter()
            .find(|update| update.id == plugin.id)
            .and_then(|update| update.latest.as_ref())
        {
            spans.push(Span::styled(
                format!("  · update available: {} → {latest}", plugin.version),
                Style::new().fg(theme::WARN),
            ));
        }
        rows.push((Some(index), Line::from(spans)));
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
    /// Ask the registries for newer versions.
    Check,
    /// Update this plugin, by its id, or all.
    Update(Option<String>),
    /// Bring the hosts that are behind up to date.
    Rollout(Rollout),
    /// Not a key of the tab: switching tabs and leaving are the screen's.
    Other,
}

/// What waits for its key a second time.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Armed {
    Remove,
    /// A roll-out, with a reboot or not.
    Rollout(bool),
}

/// The tab's state: the selected row, what the last check and update found,
/// and what waits for its key a second time. The editor for what to install
/// is the screen's.
#[derive(Default)]
pub struct Tab {
    pub selected: usize,
    /// What the last check said of each plugin.
    pub updates: Vec<PluginUpdate>,
    /// The hosts the last update left behind.
    pub behind: Vec<HostBehind>,
    armed: Option<Armed>,
}

impl Tab {
    /// The selected row, kept within the rows there are.
    pub fn selected(&mut self, plugins: &[PluginInfo]) -> usize {
        let count = plugins
            .iter()
            .map(|plugin| plugin.features.len() + 1)
            .sum::<usize>()
            + 1;
        self.selected = self.selected.min(count - 1);
        self.selected
    }

    /// The plugin of the selected row, if it is one's.
    pub fn plugin<'a>(&mut self, plugins: &'a [PluginInfo]) -> Option<&'a PluginInfo> {
        let selected = self.selected(plugins);
        rows(plugins, &[])[selected].0.map(|index| &plugins[index])
    }

    /// Forget what waits, on leaving the tab.
    pub fn leave(&mut self) {
        self.armed = None;
    }

    /// Take what a check said.
    pub fn checked(&mut self, updates: Vec<PluginUpdate>) -> Vec<String> {
        let mut lines = vec![];
        for update in &updates {
            match (&update.latest, update.note.as_str()) {
                (Some(latest), _) => {
                    lines.push(format!("{} {} → {latest}", update.id, update.version))
                }
                (None, "") => {}
                (None, note) => lines.push(format!("{}: {note}", update.id)),
            }
        }
        let newer = updates
            .iter()
            .filter(|update| update.latest.is_some())
            .count();
        lines.insert(
            0,
            match newer {
                0 => "✓ No plugin has a newer version".to_owned(),
                1 => "✓ 1 plugin has a newer version: U updates the selected one, A all".to_owned(),
                count => format!(
                    "✓ {count} plugins have a newer version: U updates the selected one, A all"
                ),
            },
        );
        self.updates = updates;
        lines
    }

    /// Take what an update said: npm's lines, then the hosts that are behind
    /// and how to bring them up to date.
    pub fn updated(&mut self, report: PluginReport) -> Vec<String> {
        let mut lines = report.lines;
        self.updates.clear();
        self.behind = report.behind;
        if self.behind.is_empty() {
            lines.push("No host is behind.".into());
        } else {
            for host in &self.behind {
                lines.push(format!(
                    "{} needs applying: {}",
                    host.name,
                    host.behind.text()
                ));
            }
            lines.push(ROLL_OUT.into());
        }
        lines
    }
}

/// How the tab offers a roll-out.
const ROLL_OUT: &str =
    "r applies them one at a time, R also reboots those that need it. Either asks first.";

impl Tab {
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
        let count = rows(plugins, &[]).len();
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
        let armed = self.armed.take();
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
            Code::Char('c') => return Asked::Check,
            Code::Char('A') => return Asked::Update(None),
            Code::Char('U') => {
                return match selected {
                    Some(plugin) if plugin.origin == PluginOrigin::Shipped => Asked::Nothing(Some(
                        format!("{} comes with berth and is updated with it", plugin.label),
                    )),
                    Some(plugin) => Asked::Update(Some(plugin.id)),
                    None => Asked::Update(None),
                };
            }
            Code::Char(key @ ('r' | 'R')) => {
                let reboot = key == 'R';
                if self.behind.is_empty() {
                    return Asked::Nothing(Some(
                        "No host is behind after an update here; a host's overview applies it by itself"
                            .into(),
                    ));
                }
                let options = Rollout {
                    hosts: self.behind.iter().map(|host| host.name.clone()).collect(),
                    reboot,
                    ..Default::default()
                };
                if armed == Some(Armed::Rollout(reboot)) {
                    return Asked::Rollout(options);
                }
                self.armed = Some(Armed::Rollout(reboot));
                *report = plan(&self.behind, &options);
                report.push(format!("{key} again starts it, any other key does not."));
                return Asked::Nothing(Some(format!(
                    "{key} again rolls out to {}",
                    options.hosts.join(", ")
                )));
            }
            Code::Char('x') | Code::Delete => {
                return Asked::Nothing(match selected {
                    Some(plugin) if plugin.origin == PluginOrigin::Shipped => {
                        Some(format!("{} comes with berth and stays", plugin.label))
                    }
                    Some(plugin) if armed == Some(Armed::Remove) => {
                        return Asked::Remove(plugin.id);
                    }
                    Some(plugin) => {
                        self.armed = Some(Armed::Remove);
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

/// What a roll-out will do, before it starts: a line per host, and the steps.
fn plan(behind: &[HostBehind], options: &Rollout) -> Vec<String> {
    let mut lines = vec![format!(
        "Roll out to {} host{}:",
        behind.len(),
        if behind.len() == 1 { "" } else { "s" }
    )];
    lines.extend(
        behind
            .iter()
            .map(|host| format!("{}: {}", host.name, host.behind.text())),
    );
    lines.push(format!(
        "One host at a time: apply{}, then check it. The first that fails stops the rest.",
        if options.reboot {
            ", reboot if it needs it (not while sessions run)"
        } else {
            ""
        }
    ));
    lines
}

pub use berth_api::rollout_line;

/// A line per host of a roll-out's report.
pub fn summary(report: &berth_api::RolloutReport) -> Vec<String> {
    use berth_api::RolloutState::*;
    let mut lines = vec![];
    for host in &report.hosts {
        let state = match host.state {
            Done if host.rebooted => "up to date, rebooted",
            Done => "up to date",
            Current => "was current",
            Skipped => "not started",
            Failed => "failed",
        };
        let mut line = format!("{}: {state}", host.name);
        if !host.error.is_empty() {
            line += &format!(": {}", host.error);
        }
        for warning in &host.warnings {
            line += &format!("; {warning}");
        }
        lines.push(line);
    }
    if lines.is_empty() {
        lines.push("No hosts".into());
    }
    lines
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
        let text: Vec<String> = rows(&plugins, &[])
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

    #[test]
    fn updates_are_checked_made_and_rolled_out_from_the_tab() {
        let plugins = [
            plugin("aa", PluginOrigin::Npm),
            plugin("zz", PluginOrigin::Shipped),
        ];
        let mut tab = Tab::default();
        let mut editor = None;
        let mut report = vec![];
        let place = Place::Here;
        let mut press = |tab: &mut Tab, report: &mut Vec<String>, code| {
            tab.key(Key::plain(code), &mut editor, &plugins, report, &place)
        };
        assert_eq!(press(&mut tab, &mut report, Code::Char('c')), Asked::Check);
        let said = tab.checked(vec![PluginUpdate {
            id: "aa".into(),
            package: "berth-plugin-aa".into(),
            version: "1.0.0".into(),
            latest: Some("2.0.0".into()),
            note: String::new(),
        }]);
        assert!(said[0].contains("1 plugin has a newer version"));
        let first = rows(&plugins, &tab.updates)[0]
            .1
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(
            first.ends_with("· update available: 1.0.0 → 2.0.0"),
            "{first}"
        );
        assert_eq!(
            press(&mut tab, &mut report, Code::Char('U')),
            Asked::Update(Some("aa".into()))
        );
        assert_eq!(
            press(&mut tab, &mut report, Code::Char('A')),
            Asked::Update(None)
        );
        // Nothing to roll out before an update said what is behind.
        assert!(matches!(
            press(&mut tab, &mut report, Code::Char('r')),
            Asked::Nothing(Some(_))
        ));
        let lines = tab.updated(PluginReport {
            lines: vec!["✓ aa 1.0.0 → 2.0.0".into()],
            behind: vec![HostBehind {
                name: "studio".into(),
                behind: berth_api::Behind {
                    changes: vec![berth_api::Change {
                        id: "aa".into(),
                        from: "1.0.0".into(),
                        to: "2.0.0".into(),
                    }],
                },
            }],
        });
        assert_eq!(lines[1], "studio needs applying: aa 1.0.0 → 2.0.0");
        assert!(tab.updates.is_empty());
        // The plan first, then the same key again starts it.
        press(&mut tab, &mut report, Code::Char('R'));
        assert_eq!(report[1], "studio: aa 1.0.0 → 2.0.0");
        assert!(report[2].contains("reboot if it needs it"));
        assert_eq!(
            press(&mut tab, &mut report, Code::Char('R')),
            Asked::Rollout(Rollout {
                hosts: vec!["studio".into()],
                reboot: true,
                ..Default::default()
            })
        );
        // Another key in between disarms it.
        press(&mut tab, &mut report, Code::Char('r'));
        press(&mut tab, &mut report, Code::Down);
        assert!(matches!(
            press(&mut tab, &mut report, Code::Char('r')),
            Asked::Nothing(_)
        ));
        assert!(rollout_line("▸ studio: applying") && !rollout_line("TASK [x]"));
    }
}
