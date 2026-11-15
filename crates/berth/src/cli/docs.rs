//! The reference page of the commands, written from the same definition as
//! the help. A test writes it with `BERTH_UPDATE_DOCS=1`, and fails when
//! the committed page is stale.

use super::args::EXIT_STATUSES;
use clap::{Arg, ArgAction, Command};

/// Where the page is, from the repository's root.
pub const PAGE: &str = "docs/content/docs/(guide)/reference/cli.mdx";

/// Text made safe for MDX: no tags or expressions, and no pipes that end a
/// table's cell.
fn safe(text: &str) -> String {
    text.replace('|', "\\|")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('{', "\\{")
        .replace('}', "\\}")
}

fn about(command: &Command) -> String {
    command
        .get_long_about()
        .or(command.get_about())
        .map(|text| text.to_string())
        .unwrap_or_default()
}

/// How an argument is written: `--flag VALUE`, `-o, --output FORMAT`, `NAME`.
fn shown(arg: &Arg) -> String {
    let value = arg
        .get_value_names()
        .map(|names| {
            names
                .iter()
                .map(|name| name.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_else(|| arg.get_id().to_string().to_uppercase());
    let takes = matches!(arg.get_action(), ArgAction::Set | ArgAction::Append);
    if arg.is_positional() {
        return match arg
            .get_num_args()
            .is_some_and(|range| range.max_values() > 1)
            || matches!(arg.get_action(), ArgAction::Append)
        {
            true => format!("{value}…"),
            false => value,
        };
    }
    let mut names = vec![];
    if let Some(short) = arg.get_short() {
        names.push(format!("-{short}"));
    }
    if let Some(long) = arg.get_long() {
        names.push(format!("--{long}"));
    }
    let mut text = names.join(", ");
    if takes {
        text.push(' ');
        text.push_str(&value);
    }
    text
}

/// What a name on the command line is, for one without help of its own.
fn named(id: &str) -> &'static str {
    match id {
        "name" => "Its name.",
        "host" => "A host's name.",
        "id" => "Its id.",
        "new" => "Its new name.",
        "action" => "The action's name.",
        "key" => "The setting or key.",
        "value" => "The value.",
        "namespace" => "The namespace.",
        "spec" => "A plugin's npm name, or the address of its .tgz.",
        "url" => "The address.",
        "service" => "PLUGIN/NAME of the service.",
        "method" => "The service's method.",
        "prompt" => "The prompt.",
        _ => "",
    }
}

/// A sentence ends with a full stop, which clap leaves out of a short help.
fn sentence(text: &str) -> String {
    let text = text.trim();
    match text.ends_with(['.', '?', '!', ':']) || text.is_empty() {
        true => text.to_owned(),
        false => format!("{text}."),
    }
}

fn help(arg: &Arg) -> String {
    let mut text = arg
        .get_long_help()
        .or(arg.get_help())
        .map(|text| sentence(&text.to_string()))
        .unwrap_or_else(|| named(arg.get_id().as_str()).to_owned());
    let values: Vec<String> = arg
        .get_possible_values()
        .iter()
        .filter(|value| !value.is_hide_set())
        .map(|value| format!("`{}`", value.get_name()))
        .collect();
    if !values.is_empty() && !arg.is_positional() && matches!(arg.get_action(), ArgAction::Set) {
        text = format!("{} One of {}.", text.trim_end(), values.join(", "));
    }
    if !values.is_empty() && arg.is_positional() {
        text = format!("{} One of {}.", text.trim_end(), values.join(", "));
    }
    let defaults: Vec<String> = arg
        .get_default_values()
        .iter()
        .map(|value| value.to_string_lossy().into_owned())
        .collect();
    if !defaults.is_empty()
        && !defaults.iter().all(|value| value.is_empty())
        && !matches!(arg.get_action(), ArgAction::SetTrue)
    {
        text = format!("{} By default `{}`.", text.trim_end(), defaults.join(" "));
    }
    text.trim().to_owned()
}

/// The usage line of a command, without the global flags.
fn usage(path: &str, command: &Command) -> String {
    let mut words = vec![path.to_owned()];
    if command.has_subcommands() {
        words.push("ACTION".into());
    }
    for arg in command.get_arguments() {
        if arg.is_global_set() || arg.is_hide_set() {
            continue;
        }
        let text = shown(arg);
        let text = match (arg.is_positional(), arg.is_last_set()) {
            (true, true) => format!("-- {text}"),
            _ => text,
        };
        words.push(match arg.is_required_set() {
            true => text,
            false => format!("[{text}]"),
        });
    }
    words.join(" ")
}

fn table(rows: &[(String, String)], heading: (&str, &str)) -> String {
    let mut text = format!("| {} | {} |\n| --- | --- |\n", heading.0, heading.1);
    for (what, does) in rows {
        text.push_str(&format!("| `{}` | {} |\n", what, safe(does)));
    }
    text
}

fn section(out: &mut String, path: &str, command: &Command, depth: usize) {
    if command.is_hide_set() {
        return;
    }
    let hashes = "#".repeat(depth.min(4));
    out.push_str(&format!("{hashes} {path}\n\n"));
    let said = about(command);
    if !said.is_empty() {
        out.push_str(&format!("{}\n\n", safe(&sentence(&said))));
    }
    let before = path.rsplit_once(' ').map_or("", |(before, _)| before);
    for alias in command.get_visible_aliases() {
        out.push_str(&format!("`{before} {alias}` is the same.\n\n"));
    }
    if !command.has_subcommands() {
        out.push_str(&format!("```text\n{}\n```\n\n", usage(path, command)));
        let rows: Vec<(String, String)> = command
            .get_arguments()
            .filter(|arg| !arg.is_global_set() && !arg.is_hide_set())
            .filter(|arg| !["help", "version"].contains(&arg.get_id().as_str()))
            .map(|arg| (shown(arg), help(arg)))
            .collect();
        if !rows.is_empty() {
            out.push_str(&table(&rows, ("Argument", "What it is")));
            out.push('\n');
        }
        if path == "berth host create" {
            out.push_str("The ways to install, for `--method`:\n\n");
            let methods: Vec<(String, String)> = berth_tui::fields::Action::SERVED
                .iter()
                .map(|action| (action.key().to_owned(), action.label().to_owned()))
                .collect();
            out.push_str(&table(&methods, ("Method", "What it is")));
            out.push_str("\nThe fields of a profile, for `--set KEY=VALUE` here and `berth host set`. A switch takes `on` or `off`. `feature.PLUGIN/FEATURE=on`, `secret.NAME=VARIABLE` and `registry.NAME=on` give a host those as well.\n\n");
            let keys: Vec<(String, String)> = berth_tui::flow::keys()
                .into_iter()
                .map(|(key, label, _)| (key, label.to_owned()))
                .collect();
            out.push_str(&table(&keys, ("Key", "Field")));
            out.push('\n');
        }
        return;
    }
    for sub in command.get_subcommands() {
        if sub.get_name() == "help" {
            continue;
        }
        section(out, &format!("{path} {}", sub.get_name()), sub, depth + 1);
    }
}

/// The whole page.
pub fn page() -> String {
    let command = super::command();
    let mut out = String::from(
        "---\ntitle: Command reference\ndescription: Every command of berth with its arguments, made from the same definition as berth --help.\nicon: SquareTerminal\n---\n\n",
    );
    out.push_str("This page is generated from the commands themselves. [The command line](/docs/use/cli) explains them by task.\n\n");
    out.push_str("## Flags of every command\n\n");
    let global: Vec<(String, String)> = command
        .get_arguments()
        .filter(|arg| arg.is_global_set())
        .map(|arg| (shown(arg), help(arg)))
        .collect();
    out.push_str(&table(&global, ("Flag", "What it does")));
    out.push_str("\nFlags go before or after the names, as `--tail 50` or `--tail=50`. `--` ends the flags; what follows it is passed on as it is.\n\n");
    out.push_str("## Exit statuses\n\n| Status | Means |\n| --- | --- |\n");
    for (status, means) in EXIT_STATUSES {
        out.push_str(&format!("| {status} | {} |\n", safe(means)));
    }
    out.push_str(
        "\nA command run on a host, such as `berth exec`, ends with that command's own status.\n\n",
    );
    out.push_str("## Environment\n\n| Variable | What it does |\n| --- | --- |\n");
    for (name, does) in ENVIRONMENT {
        out.push_str(&format!("| `{name}` | {} |\n", safe(does)));
    }
    out.push('\n');
    for sub in command.get_subcommands() {
        if sub.get_name() == "help" {
            continue;
        }
        section(&mut out, &format!("berth {}", sub.get_name()), sub, 2);
    }
    out.trim_end().to_owned() + "\n"
}

/// The variables berth reads, besides its flags and settings.
pub const ENVIRONMENT: &[(&str, &str)] = &[
    (
        "BERTH_SERVER",
        "The server, when no --server is given. It wins over the one berth login kept.",
    ),
    (
        "BERTH_TOKEN",
        "Its token, when no --token-file or --token-stdin is given. For containers and CI.",
    ),
    ("NO_COLOR", "No colours, as --color never."),
    (
        "BERTH_ and a setting's name",
        "A setting of the server, in capitals, such as BERTH_RETENTION_DAYS, for a server in a container.",
    ),
    (
        "XDG_CONFIG_HOME, XDG_DATA_HOME, XDG_RUNTIME_DIR",
        "Where berth keeps its settings, its data and its socket.",
    ),
    ("VISUAL, EDITOR", "The editor of berth host edit."),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reference_page_is_the_commands_as_they_are() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let file = root.join(PAGE);
        let page = page();
        if std::env::var_os("BERTH_UPDATE_DOCS").is_some() {
            std::fs::write(&file, &page).unwrap();
            return;
        }
        let committed = std::fs::read_to_string(&file).unwrap_or_default();
        assert!(
            committed == page,
            "{PAGE} is stale: write it again with BERTH_UPDATE_DOCS=1 cargo test -p berth"
        );
    }

    #[test]
    fn every_command_is_on_the_page() {
        let page = page();
        for line in [
            "## berth host",
            "### berth host create",
            "`--set KEY=VALUE`",
            "| 4 |",
        ] {
            assert!(page.contains(line), "{line}");
        }
        assert!(!page.contains("berth askpass"));
    }
}
