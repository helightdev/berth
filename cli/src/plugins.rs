//! Plugins: npm-format packages whose `package.json` has a `berth` section.
//! Each contributes features to the groups of the Features page, and tasks to
//! the overview of a host. The role does the installing from the same manifest.

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

pub struct PluginAction {
    pub name: String,
    pub label: String,
    pub help: &'static str,
    /// Shell command to run on the host.
    pub run: String,
    /// `agent`, `operator` or `root`.
    pub user: String,
    /// Runs in a terminal, because it asks something or shows a link.
    pub terminal: bool,
    /// The group of the overview it is listed in; the plugin's own when empty.
    pub group: String,
    /// Tasks of berth it is part of, such as `status`. It runs after them and
    /// has no row of its own.
    pub extends: Vec<String>,
}

/// One thing a plugin can install, switched on its own.
pub struct Feature {
    pub id: String,
    /// The group of the Features page it is listed in.
    pub group: String,
    pub label: String,
    pub help: &'static str,
    pub actions: Vec<PluginAction>,
}

pub struct Plugin {
    pub id: String,
    pub label: String,
    /// The npm name and version, for listing.
    pub package: String,
    pub version: String,
    pub description: String,
    pub dir: PathBuf,
    pub features: Vec<Feature>,
}

/// A name that is safe as a directory name and inside a remote command.
pub fn valid_name(name: &str) -> bool {
    name.starts_with(|ch: char| ch.is_ascii_lowercase())
        && name
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' || ch == '_')
}

/// The plugins found in the given directories. Packages sit directly in a
/// directory, in its `node_modules` (where `npm install --prefix` puts them) or
/// one level further down under an `@scope`. A later directory replaces a plugin
/// of the same id from an earlier one. Packages without a usable `berth`
/// section are skipped.
pub fn load(dirs: &[PathBuf]) -> Vec<Plugin> {
    let mut plugins: Vec<Plugin> = vec![];
    for dir in dirs {
        for package in packages(dir).into_iter().chain(packages(&dir.join("node_modules"))) {
            if let Some(plugin) = read(&package) {
                plugins.retain(|known| known.id != plugin.id);
                plugins.push(plugin);
            }
        }
    }
    plugins.sort_by(|a, b| a.id.cmp(&b.id));
    plugins
}

fn packages(dir: &Path) -> Vec<PathBuf> {
    let children = |dir: &Path| -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = fs::read_dir(dir)
            .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
            .unwrap_or_default();
        paths.sort();
        paths
    };
    let mut found = vec![];
    for path in children(dir) {
        let scoped = path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with('@'));
        if scoped {
            found.extend(children(&path));
        } else {
            found.push(path);
        }
    }
    found
}

fn read(dir: &Path) -> Option<Plugin> {
    let data: Value = serde_json::from_slice(&fs::read(dir.join("package.json")).ok()?).ok()?;
    let berth = data.get("berth")?;
    let text = |value: &Value, key: &str| value[key].as_str().unwrap_or("").to_owned();
    let or = |first: String, second: &str| if first.is_empty() { second.to_owned() } else { first };
    // Help texts live as long as the program; there are only a handful.
    let keep = |text: String| -> &'static str { Box::leak(text.into_boxed_str()) };
    let package = text(&data, "name");
    // `@scope/berth-plugin-unity` is the plugin `unity` unless it says otherwise.
    let short = package.rsplit('/').next().unwrap_or("");
    let id = or(text(berth, "id"), short.strip_prefix("berth-plugin-").unwrap_or(short));
    // `sign-in:NAME` are berth's own sign-ins.
    if !valid_name(&id) || id == "sign-in" {
        return None;
    }
    let label = or(text(&data, "displayName"), &id);
    let list = |value: &Value| value.as_array().cloned().unwrap_or_default();
    let features = list(&berth["features"])
        .iter()
        .filter(|feature| valid_name(&text(feature, "id")))
        .map(|feature| Feature {
            id: text(feature, "id"),
            group: or(text(feature, "group"), &label),
            label: or(text(feature, "label"), &text(feature, "id")),
            help: keep(text(feature, "description")),
            actions: list(&feature["actions"])
                .iter()
                .filter(|action| {
                    valid_name(&text(action, "name")) && !text(action, "run").is_empty()
                })
                .map(|action| PluginAction {
                    name: text(action, "name"),
                    label: or(text(action, "label"), &text(action, "name")),
                    help: keep(text(action, "help")),
                    run: text(action, "run"),
                    user: match action["user"].as_str() {
                        Some(user @ ("operator" | "root")) => user.to_owned(),
                        _ => "agent".to_owned(),
                    },
                    terminal: action["terminal"].as_bool().unwrap_or(false),
                    group: text(action, "group"),
                    extends: match &action["extends"] {
                        Value::String(task) => vec![task.clone()],
                        tasks => list(tasks)
                            .iter()
                            .filter_map(|task| task.as_str().map(str::to_owned))
                            .collect(),
                    },
                })
                .collect(),
        })
        .collect();
    Some(Plugin {
        id,
        label,
        package,
        version: text(&data, "version"),
        description: text(&data, "description"),
        dir: fs::canonicalize(dir).unwrap_or_else(|_| dir.to_owned()),
        features,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packages_are_found_named_and_replaced() {
        let base = tempfile::tempdir().unwrap();
        let write = |dir: &str, body: &str| {
            let path = base.path().join(dir);
            fs::create_dir_all(&path).unwrap();
            fs::write(path.join("package.json"), body).unwrap();
        };
        write("a/one", r#"{"name":"berth-plugin-one","displayName":"First","berth":{}}"#);
        write("a/not-a-plugin", r#"{"name":"left-pad"}"#);
        write("a/broken", "{");
        write("a/bad", r#"{"name":"Bad Name","berth":{}}"#);
        write(
            "b/node_modules/@me/berth-plugin-one",
            r#"{"name":"@me/berth-plugin-one","version":"2.0.0","displayName":"Second",
                "berth":{"features":[
                  {"id":"x","group":"Development Tools","actions":[
                    {"name":"go","run":"true","user":"root"},
                    {"name":"no command"},
                    {"name":"plain","run":"id"}]},
                  {"id":"y"},
                  {"id":"Not Valid"}]}}"#,
        );
        write("b/node_modules/other", r#"{"name":"other","berth":{"id":"renamed"}}"#);
        let plugins = load(&[base.path().join("a"), base.path().join("b"), "missing".into()]);
        let ids: Vec<_> = plugins.iter().map(|plugin| plugin.id.as_str()).collect();
        assert_eq!(ids, ["one", "renamed"]);
        let one = &plugins[0];
        assert_eq!((one.label.as_str(), one.version.as_str()), ("Second", "2.0.0"));
        assert!(one.dir.ends_with("node_modules/@me/berth-plugin-one"));
        let features: Vec<_> = one
            .features
            .iter()
            .map(|feature| (feature.id.as_str(), feature.group.as_str()))
            .collect();
        // A feature without a group gets one named after its plugin.
        assert_eq!(features, [("x", "Development Tools"), ("y", "Second")]);
        let actions: Vec<_> = one.features[0]
            .actions
            .iter()
            .map(|action| (action.name.as_str(), action.user.as_str()))
            .collect();
        assert_eq!(actions, [("go", "root"), ("plain", "agent")]);
    }
}
