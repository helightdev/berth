//! Plugins: npm-format packages whose `package.json` has a `berth` section.
//! Each contributes features to the groups of the Features page, and tasks to
//! the overview of a host. The role does the installing from the same manifest.
//! A plugin can also bring provisioners, programs that create the machine a
//! profile describes; berth runs those itself (see [`crate::provision`]).

use crate::{Result, paths::Paths, server::Server};
use berth_api::{
    AppliedPlugin, PluginFeature, PluginInfo, PluginList as Listing, PluginOrigin,
    PluginReport as Report, PluginService, PluginTask, PluginUpdate, PluginUpdates,
    ProvisionerInfo,
};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

pub struct PluginAction {
    pub name: String,
    pub label: String,
    pub help: &'static str,
    /// Shell command the host runs. berth does not run it from here: the
    /// role writes it into the host's task list.
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
    /// The agent may run it with `box run`.
    pub agent: bool,
    /// What it asks for before it runs.
    pub arguments: Vec<berth_api::Argument>,
    /// A sentence to say yes to before it runs.
    pub confirm: Option<String>,
}

/// One thing a plugin can install, switched on its own.
pub struct Feature {
    pub id: String,
    /// The group of the Features page it is listed in.
    pub group: String,
    pub label: String,
    pub help: &'static str,
    pub actions: Vec<PluginAction>,
    /// Programs it keeps running on the host to answer calls; names without
    /// the plugin's id.
    pub services: Vec<berth_api::ServiceSpec>,
    /// Defaults and limits of its channel topics; topics without the plugin's id.
    pub channels: Vec<berth_api::Topic>,
}

/// How a machine a provisioner created becomes the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setup {
    /// It boots with berth's cloud seed and sets itself up.
    Seed,
    /// berth applies the profile over SSH once it answers.
    Apply,
}

impl Setup {
    pub fn name(self) -> &'static str {
        match self {
            Setup::Seed => "seed",
            Setup::Apply => "apply",
        }
    }
}

/// A berth secret a provisioner is given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionerSecret {
    pub name: String,
    /// Without a value it cannot run; one that is not required is passed when set.
    pub required: bool,
}

/// A program of a plugin that creates machines.
#[derive(Debug, Clone)]
pub struct Provisioner {
    pub id: String,
    pub label: String,
    pub description: String,
    /// The executable, resolved and inside the package.
    pub command: PathBuf,
    pub setup: Setup,
    /// The plugin puts the release archive into the machine, so the seed
    /// needs no `release_url`.
    pub archive: bool,
    /// Of `start`, `stop` and `list`, those it answers.
    pub verbs: Vec<String>,
    pub secrets: Vec<ProvisionerSecret>,
    /// Values are strings; `choices` is not used.
    pub arguments: Vec<berth_api::Argument>,
}

/// The verbs a provisioner may answer besides `create`, `status` and `destroy`.
pub const OPTIONAL_VERBS: &[&str] = &["start", "stop", "list"];

impl Provisioner {
    /// What is said of it, as `PLUGIN/ID`, with the required secrets that
    /// have no value.
    pub fn info(&self, plugin: &str, missing_secrets: Vec<String>) -> ProvisionerInfo {
        ProvisionerInfo {
            id: format!("{plugin}/{}", self.id),
            label: self.label.clone(),
            description: self.description.clone(),
            setup: self.setup.name().into(),
            verbs: self.verbs.clone(),
            secrets: self
                .secrets
                .iter()
                .map(|secret| secret.name.clone())
                .collect(),
            missing_secrets,
            arguments: self.arguments.clone(),
        }
    }
}

pub struct Plugin {
    pub id: String,
    pub label: String,
    /// The npm name and version, for listing.
    pub package: String,
    pub version: String,
    pub description: String,
    pub dir: PathBuf,
    /// Where it was found, before links are followed: what npm installed
    /// from a folder is a link to that folder.
    pub found: PathBuf,
    pub features: Vec<Feature>,
    pub provisioners: Vec<Provisioner>,
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
        for package in packages(dir)
            .into_iter()
            .chain(packages(&dir.join("node_modules")))
        {
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
    let or = |first: String, second: &str| {
        if first.is_empty() {
            second.to_owned()
        } else {
            first
        }
    };
    // Help texts live as long as the program; there are only a handful.
    let keep = |text: String| -> &'static str { Box::leak(text.into_boxed_str()) };
    let package = text(&data, "name");
    // `@scope/berth-plugin-unity` is the plugin `unity` unless it says otherwise.
    let short = package.rsplit('/').next().unwrap_or("");
    let id = or(
        text(berth, "id"),
        short.strip_prefix("berth-plugin-").unwrap_or(short),
    );
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
                    agent: action["agent"].as_bool().unwrap_or(false),
                    arguments: list(&action["arguments"])
                        .into_iter()
                        .filter_map(|argument| {
                            serde_json::from_value::<berth_api::Argument>(argument).ok()
                        })
                        .filter(|argument| valid_name(&argument.name))
                        // Only a provisioner's arguments size a machine or are a
                        // list of values; a task's offers them with `choices`.
                        .map(|argument| berth_api::Argument {
                            kind: String::new(),
                            values: vec![],
                            ..argument
                        })
                        .collect(),
                    confirm: action["confirm"]
                        .as_str()
                        .filter(|text| !text.is_empty())
                        .map(str::to_owned),
                })
                .collect(),
            services: list(&feature["services"])
                .into_iter()
                .filter_map(|service| {
                    serde_json::from_value::<berth_api::ServiceSpec>(service).ok()
                })
                .filter(|service| valid_name(&service.name))
                .collect(),
            channels: list(&feature["channels"])
                .into_iter()
                .filter_map(|topic| serde_json::from_value::<berth_api::Topic>(topic).ok())
                .collect(),
        })
        .collect();
    let real = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_owned());
    let provisioners = list(&berth["provisioners"])
        .iter()
        .filter_map(|provisioner| read_provisioner(provisioner, &real))
        .collect();
    Some(Plugin {
        id,
        label,
        package,
        version: text(&data, "version"),
        description: text(&data, "description"),
        dir: real,
        found: dir.to_owned(),
        features,
        provisioners,
    })
}

/// A provisioner of a manifest, or None when it is not usable: a bad id or
/// set-up, or a command that is not a file inside the package.
fn read_provisioner(entry: &Value, dir: &Path) -> Option<Provisioner> {
    let text = |key: &str| entry[key].as_str().unwrap_or("").to_owned();
    let id = text("id");
    let setup = match entry["setup"].as_str() {
        Some("seed") => Setup::Seed,
        Some("apply") => Setup::Apply,
        _ => return None,
    };
    let relative = PathBuf::from(text("command"));
    // berth runs it as itself, so it must be the package's own file.
    let plain = relative.components().all(|part| {
        matches!(
            part,
            std::path::Component::Normal(_) | std::path::Component::CurDir
        )
    });
    if !valid_name(&id) || relative.as_os_str().is_empty() || !plain {
        return None;
    }
    let command = fs::canonicalize(dir.join(&relative)).ok()?;
    if !command.starts_with(dir) || !command.is_file() {
        return None;
    }
    let list = |key: &str| entry[key].as_array().cloned().unwrap_or_default();
    let mut secrets: Vec<ProvisionerSecret> = vec![];
    for secret in list("secrets") {
        let (name, required) = match &secret {
            Value::String(name) => (name.clone(), true),
            other => (
                other["name"].as_str().unwrap_or("").to_owned(),
                other["required"].as_bool().unwrap_or(true),
            ),
        };
        if valid_name(&name) && !secrets.iter().any(|known| known.name == name) {
            secrets.push(ProvisionerSecret { name, required });
        }
    }
    Some(Provisioner {
        label: match text("label") {
            label if label.is_empty() => id.clone(),
            label => label,
        },
        id,
        description: text("description"),
        command,
        setup,
        archive: setup == Setup::Seed && entry["archive"].as_bool().unwrap_or(false),
        verbs: OPTIONAL_VERBS
            .iter()
            .filter(|verb| list("verbs").iter().any(|known| known == **verb))
            .map(|verb| (*verb).to_owned())
            .collect(),
        secrets,
        arguments: list("arguments")
            .into_iter()
            .filter_map(|argument| serde_json::from_value::<berth_api::Argument>(argument).ok())
            .filter(|argument| valid_name(&argument.name))
            .map(|argument| berth_api::Argument {
                choices: String::new(),
                // A kind berth does not know makes an ordinary argument.
                kind: match argument.is_resource() {
                    true => argument.kind.clone(),
                    false => String::new(),
                },
                ..argument
            })
            .collect(),
    })
}

// ── Managing them ───────────────────────────────────────────────────────────

/// Where the user's own plugins are kept, under a configuration directory.
pub fn own_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("plugins")
}

/// The plugins of a provisioning source and those in a plugin directory.
pub fn found(source: &Path, own: &Path) -> Vec<Plugin> {
    load(&[source.join("plugins"), own.to_owned()])
}

impl Plugin {
    /// Where it comes from, judged by where it was found.
    pub fn origin(&self, own: &Path) -> PluginOrigin {
        if self.found.starts_with(own.join("node_modules")) {
            PluginOrigin::Npm
        } else if self.found.starts_with(own) {
            PluginOrigin::Folder
        } else {
            PluginOrigin::Shipped
        }
    }

    /// What is said of it to a screen or over the API.
    pub fn info(&self, own: &Path) -> PluginInfo {
        PluginInfo {
            id: self.id.clone(),
            label: self.label.clone(),
            package: self.package.clone(),
            version: self.version.clone(),
            description: self.description.clone(),
            origin: self.origin(own),
            dir: self.dir.display().to_string(),
            features: self
                .features
                .iter()
                .map(|feature| PluginFeature {
                    id: feature.id.clone(),
                    label: feature.label.clone(),
                    group: feature.group.clone(),
                    help: feature.help.to_owned(),
                    tasks: feature
                        .actions
                        .iter()
                        .map(|action| PluginTask {
                            name: action.name.clone(),
                            label: action.label.clone(),
                            user: action.user.clone(),
                            agent: action.agent,
                            extends: action.extends.clone(),
                            arguments: action
                                .arguments
                                .iter()
                                .map(|argument| argument.name.clone())
                                .collect(),
                        })
                        .collect(),
                    services: feature
                        .services
                        .iter()
                        .map(|service| PluginService {
                            name: service.name.clone(),
                            user: service.user.clone(),
                            methods: service
                                .methods
                                .iter()
                                .map(|method| method.name.clone())
                                .collect(),
                        })
                        .collect(),
                    channels: feature
                        .channels
                        .iter()
                        .map(|channel| channel.topic.clone())
                        .collect(),
                })
                .collect(),
            provisioners: self
                .provisioners
                .iter()
                .map(|provisioner| provisioner.info(&self.id, vec![]))
                .collect(),
        }
    }
}

/// The listing of `berth plugins`: each plugin with what its features add.
pub fn describe(plugins: &[PluginInfo]) -> String {
    let mut text = String::new();
    for plugin in plugins {
        text += &format!(
            "{}  {} {}  {}\n",
            plugin.id, plugin.package, plugin.version, plugin.dir
        );
        for feature in &plugin.features {
            text += &format!(
                "  {}/{}  {} ({})\n",
                plugin.id, feature.id, feature.label, feature.group
            );
            for task in &feature.tasks {
                let mut notes = vec![format!("as {}", task.user)];
                if task.agent {
                    notes.push("the agent may run it".into());
                }
                if !task.extends.is_empty() {
                    notes.push(format!("part of {}", task.extends.join(", ")));
                }
                for argument in &task.arguments {
                    notes.push(format!("asks {argument}"));
                }
                text += &format!(
                    "    task     {}:{}  {} ({})\n",
                    plugin.id,
                    task.name,
                    task.label,
                    notes.join("; ")
                );
            }
            for service in &feature.services {
                text += &format!(
                    "    service  {}/{}  as {}: {}\n",
                    plugin.id,
                    service.name,
                    service.user,
                    service.methods.join(", ")
                );
            }
            for channel in &feature.channels {
                text += &format!("    channel  {}/{channel}\n", plugin.id);
            }
        }
        for provisioner in &plugin.provisioners {
            text += &format!(
                "  provisioner  {}  {} ({})\n",
                provisioner.id,
                provisioner.label,
                provisioner_notes(provisioner).join("; ")
            );
        }
    }
    text
}

/// What a provisioner does and asks for, in a few words each.
pub fn provisioner_notes(provisioner: &ProvisionerInfo) -> Vec<String> {
    let mut notes = vec![match provisioner.setup.as_str() {
        "seed" => "sets itself up from the cloud seed".to_owned(),
        _ => "berth applies the profile over SSH".to_owned(),
    }];
    if !provisioner.verbs.is_empty() {
        notes.push(format!("also {}", provisioner.verbs.join(", ")));
    }
    for argument in &provisioner.arguments {
        let mut note = match (argument.required, argument.default.as_str()) {
            (true, _) => format!("asks {}", argument.name),
            (false, "") => format!("may ask {}", argument.name),
            (false, default) => format!("asks {} (default {default})", argument.name),
        };
        if !argument.values.is_empty() {
            note += &format!(": {}", argument.values.join(" or "));
        }
        notes.push(note);
    }
    for secret in &provisioner.secrets {
        let missing = provisioner.missing_secrets.contains(secret);
        notes.push(match missing {
            true => format!("secret {secret}, not set"),
            false => format!("secret {secret}"),
        });
    }
    notes
}

/// SHA-256 over a plugin's files, their paths and contents, so a plugin
/// that changed without a new version is told apart. What npm installed
/// beside it (`node_modules`) and a `.git` folder are not part of it.
pub fn digest(dir: &Path) -> String {
    use sha2::{Digest, Sha256};
    fn walk(base: &Path, dir: &Path, files: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            if name == "node_modules" || name == ".git" {
                continue;
            }
            match fs::metadata(&path) {
                Ok(meta) if meta.is_dir() => walk(base, &path, files),
                Ok(_) => files.push(path.strip_prefix(base).unwrap_or(&path).to_owned()),
                Err(_) => {}
            }
        }
    }
    let mut files = vec![];
    walk(dir, dir, &mut files);
    files.sort();
    let mut hash = Sha256::new();
    for file in files {
        let data = fs::read(dir.join(&file)).unwrap_or_default();
        hash.update(file.to_string_lossy().as_bytes());
        hash.update([0]);
        hash.update((data.len() as u64).to_be_bytes());
        hash.update(&data);
    }
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// What is recorded of each plugin on a host when it is applied.
pub fn records(plugins: &[Plugin]) -> Vec<AppliedPlugin> {
    plugins
        .iter()
        .map(|plugin| AppliedPlugin {
            id: plugin.id.clone(),
            package: plugin.package.clone(),
            version: plugin.version.clone(),
            digest: digest(&plugin.dir),
        })
        .collect()
}

/// How a host that says what it was applied from differs from what these
/// plugins and this berth would apply now.
pub fn behind(applied: &berth_api::Applied, plugins: &[Plugin]) -> Option<berth_api::Behind> {
    berth_api::Behind::compare(applied, berth_api::VERSION, &records(plugins))
}

/// Run npm on a plugin directory, and say what it said: a line with ✓ or ✗,
/// then a few of npm's. Nothing of a plugin runs: its scripts are off.
pub fn npm(own: &Path, verb: &str, what: &str) -> Result<Vec<String>> {
    fs::create_dir_all(own)?;
    let output = std::process::Command::new("npm")
        .arg(verb)
        .args([
            "--ignore-scripts",
            "--no-audit",
            "--no-fund",
            "--no-progress",
            "--prefix",
        ])
        .arg(own)
        .arg(what)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| format!("✗ npm could not be started: {error}"))?;
    let mut lines = vec![match output.status.success() {
        true => format!("✓ npm {verb} {what}"),
        false => format!("✗ npm {verb} {what} failed"),
    }];
    let said = [output.stdout, output.stderr].concat();
    lines.extend(
        String::from_utf8_lossy(&said)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .take(12)
            .map(str::to_owned),
    );
    forget_uploads(own);
    match output.status.success() {
        true => Ok(lines),
        false => Err(lines.join("\n").into()),
    }
}

/// Install a plugin with npm: a package name, an address or a path.
pub fn install(own: &Path, spec: &str) -> Result<Vec<String>> {
    npm(own, "install", spec)
}

/// The folder of uploaded packages in a plugin directory. npm installs from
/// there and finds them again when it installs something else.
fn uploads(own: &Path) -> PathBuf {
    own.join("uploads")
}

/// Install a package that was sent as a .tgz, as `npm pack` makes one.
pub fn install_upload(own: &Path, package: &[u8]) -> Result<Vec<String>> {
    if !package.starts_with(&[0x1f, 0x8b]) {
        return Err("A plugin is sent as a .tgz, as npm pack makes it".into());
    }
    let dir = uploads(own);
    fs::create_dir_all(&dir)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_nanos())
        .unwrap_or_default();
    let path = dir.join(format!("{stamp}.tgz"));
    fs::write(&path, package)?;
    let mut lines = install(own, &path.to_string_lossy())?;
    // Where it was kept says nothing to whoever sent it.
    lines[0] = "✓ npm install of the .tgz that was sent".into();
    Ok(lines)
}

/// Remove the uploaded packages that npm no longer has a dependency on.
fn forget_uploads(own: &Path) {
    let wanted = fs::read(own.join("package.json"))
        .ok()
        .and_then(|data| serde_json::from_slice::<Value>(&data).ok())
        .map(|manifest| manifest["dependencies"].to_string())
        .unwrap_or_default();
    let Ok(entries) = fs::read_dir(uploads(own)) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !wanted.contains(&format!("uploads/{name}")) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Remove one of the user's plugins: with npm when npm installed it, and
/// otherwise its folder. One that comes with berth stays.
pub fn remove(own: &Path, plugin: &Plugin) -> Result<Vec<String>> {
    match plugin.origin(own) {
        PluginOrigin::Shipped => Err(format!("{} comes with berth and stays", plugin.label).into()),
        PluginOrigin::Npm => npm(own, "uninstall", &plugin.package),
        PluginOrigin::Folder => {
            fs::remove_dir_all(&plugin.found)?;
            Ok(vec![format!("✓ Removed {}", plugin.label)])
        }
    }
}

/// What the plugin directory's `package.json` says each package was
/// installed from: a version range or tag for a registry, `file:…` for a
/// .tgz or a folder, or an address.
fn installed_from(own: &Path) -> serde_json::Map<String, Value> {
    fs::read(own.join("package.json"))
        .ok()
        .and_then(|data| serde_json::from_slice::<Value>(&data).ok())
        .and_then(|manifest| manifest["dependencies"].as_object().cloned())
        .unwrap_or_default()
}

/// Whether a dependency's spec is one a registry answers for.
fn from_registry(spec: &str) -> bool {
    !spec.is_empty() && !spec.contains(':') && !spec.contains('/')
}

/// What can be said of updates without asking a registry: shipped plugins
/// are not listed, and those without a registry say so. The rest are the
/// packages to ask npm about.
fn updatable(own: &Path, plugins: &[Plugin]) -> (Vec<PluginUpdate>, Vec<String>) {
    let from = installed_from(own);
    let mut updates = vec![];
    let mut ask = vec![];
    for plugin in plugins {
        let note = match plugin.origin(own) {
            PluginOrigin::Shipped => continue,
            PluginOrigin::Folder => {
                "A folder copied into the plugin directory: copy the new one over it".to_owned()
            }
            PluginOrigin::Npm => match from.get(&plugin.package).and_then(Value::as_str) {
                Some(spec) if from_registry(spec) => {
                    ask.push(plugin.package.clone());
                    String::new()
                }
                _ => "Installed from a .tgz, a folder or an address, which has no newer version to ask for: add it again to replace it".to_owned(),
            },
        };
        updates.push(PluginUpdate {
            id: plugin.id.clone(),
            package: plugin.package.clone(),
            version: plugin.version.clone(),
            latest: None,
            note,
        });
    }
    (updates, ask)
}

/// Take in what `npm outdated --json` said: by package, the latest version.
/// npm says `{"error": …}` when it could not ask.
fn take_outdated(updates: &mut [PluginUpdate], asked: &[String], said: &str) {
    let said: Value = serde_json::from_str(said.trim()).unwrap_or(json!({}));
    let error = said["error"]["summary"]
        .as_str()
        .or(said["error"]["code"].as_str())
        .map(str::to_owned);
    for update in updates
        .iter_mut()
        .filter(|update| asked.contains(&update.package))
    {
        if let Some(error) = &error {
            update.note = format!("npm could not ask the registry: {error}");
            continue;
        }
        // An entry per place a package is installed, or one.
        let entry = match &said[&update.package] {
            Value::Array(entries) => entries.first().cloned().unwrap_or(Value::Null),
            entry => entry.clone(),
        };
        if let Some(latest) = entry["latest"].as_str()
            && latest != update.version
        {
            update.latest = Some(latest.to_owned());
        }
    }
}

/// Ask npm, with its configuration as installing has it, for the latest
/// versions of these packages.
fn ask_npm(own: &Path, updates: &mut [PluginUpdate], ask: &[String]) -> Result<()> {
    if ask.is_empty() {
        return Ok(());
    }
    let output = std::process::Command::new("npm")
        .args(["outdated", "--json", "--prefix"])
        .arg(own)
        .args(ask)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| format!("npm could not be started: {error}"))?;
    // npm ends with 1 when something is outdated; what it printed counts.
    take_outdated(updates, ask, &String::from_utf8_lossy(&output.stdout));
    Ok(())
}

/// Ask the registries the user's plugins came from whether they have newer
/// versions.
pub fn check(own: &Path, plugins: &[Plugin]) -> Result<PluginUpdates> {
    let (mut updates, ask) = updatable(own, plugins);
    ask_npm(own, &mut updates, &ask)?;
    Ok(PluginUpdates { plugins: updates })
}

/// Update one plugin, by its id or npm name, or all of them to the latest
/// version their registry has. A plugin without a registry says why not.
pub fn update(own: &Path, plugins: &[Plugin], name: Option<&str>) -> Result<Vec<String>> {
    let (mut updates, mut ask) = updatable(own, plugins);
    if let Some(name) = name {
        let Some(plugin) = plugins
            .iter()
            .find(|plugin| plugin.id == name || plugin.package == name)
        else {
            return Err(format!("No plugin {name}. See: berth plugins").into());
        };
        match updates.iter().find(|update| update.id == plugin.id) {
            None => {
                return Err(format!(
                    "{} comes with berth and is updated with it: berth update",
                    plugin.label
                )
                .into());
            }
            Some(update) if !update.note.is_empty() => {
                return Err(format!("✗ {}: {}", update.id, update.note).into());
            }
            Some(_) => {
                ask = vec![plugin.package.clone()];
                updates.retain(|update| update.id == plugin.id);
            }
        }
    }
    ask_npm(own, &mut updates, &ask)?;
    let mut lines = vec![];
    for update in updates
        .iter()
        .filter(|update| ask.contains(&update.package))
    {
        match &update.latest {
            Some(latest) => {
                npm(own, "install", &format!("{}@{latest}", update.package))?;
                lines.push(format!("✓ {} {} → {latest}", update.id, update.version));
            }
            None if update.note.is_empty() && name.is_some() => {
                lines.push(format!("✓ {} {} is the latest", update.id, update.version));
            }
            None => {}
        }
    }
    if lines.is_empty() {
        lines.push("✓ No plugin has a newer version".into());
    }
    for update in updates.iter().filter(|update| !update.note.is_empty()) {
        lines.push(format!("  {}: {}", update.id, update.note));
    }
    Ok(lines)
}

/// Whether a spec names a package of a registry, `[@SCOPE/]NAME[@VERSION]`,
/// or a .tgz at an https address. A path, a git repository or anything npm
/// would take as an option is something else.
pub fn registry_spec(spec: &str) -> bool {
    if let Some(rest) = spec.strip_prefix("https://") {
        return !rest.is_empty()
            && !rest.contains(|ch: char| ch.is_whitespace() || ch.is_control());
    }
    let part = |text: &str| {
        text.starts_with(|ch: char| ch.is_ascii_lowercase() || ch.is_ascii_digit())
            && text.chars().all(|ch| {
                ch.is_ascii_lowercase()
                    || ch.is_ascii_digit()
                    || matches!(ch, '-' | '.' | '_' | '~')
            })
    };
    let rest = match spec.strip_prefix('@') {
        Some(scoped) => match scoped.split_once('/') {
            Some((scope, rest)) if part(scope) => rest,
            _ => return false,
        },
        None => spec,
    };
    let (name, version) = match rest.split_once('@') {
        Some((name, version)) => (name, Some(version)),
        None => (rest, None),
    };
    part(name)
        && version.is_none_or(|version| {
            !version.is_empty()
                && version.chars().all(|ch| {
                    ch.is_ascii_alphanumeric()
                        || matches!(
                            ch,
                            '.' | '-' | '+' | '^' | '~' | '<' | '>' | '=' | '*' | '_'
                        )
                })
        })
}

/// Whether a spec is a file or folder on this machine rather than a package
/// of a registry, the way npm tells them apart.
pub fn local_spec(spec: &str) -> bool {
    if spec.contains("://") {
        return false;
    }
    spec.starts_with(['~', '.', '/'])
        || spec.starts_with("file:")
        || [".tgz", ".tar.gz", ".tar"]
            .iter()
            .any(|end| spec.ends_with(end))
}

/// Where plugins are managed: this machine's plugin directory, or that of
/// the berth server berth is set to use.
pub struct Manager {
    source: Option<PathBuf>,
    own: PathBuf,
    server: Option<Server>,
}

impl Manager {
    pub fn new(paths: &Paths) -> Manager {
        Manager {
            source: paths.source().ok(),
            own: own_dir(&paths.config_dir),
            server: Server::configured(paths),
        }
    }

    pub fn server(&self) -> Option<&Server> {
        self.server.as_ref()
    }

    fn found(&self) -> Vec<Plugin> {
        match &self.source {
            Some(source) => found(source, &self.own),
            None => load(std::slice::from_ref(&self.own)),
        }
    }

    /// The plugins there are, and where the user's own are kept.
    pub fn list(&self) -> Result<Listing> {
        if let Some(server) = &self.server {
            return Ok(serde_json::from_value(server.call(
                "plugins.list",
                Value::Null,
                None,
            )?)?);
        }
        Ok(Listing {
            plugins: self
                .found()
                .iter()
                .map(|plugin| plugin.info(&self.own))
                .collect(),
            dir: self.own.display().to_string(),
        })
    }

    /// Install a plugin. A server installs a package of a registry itself,
    /// and is sent a .tgz or a folder of this machine as a .tgz.
    pub fn add(&self, spec: &str) -> Result<Vec<String>> {
        let local = local_spec(spec);
        let path = match local {
            true => fs::canonicalize(crate::discovery::expand_path(
                spec.strip_prefix("file:").unwrap_or(spec),
            ))
            .map_err(|error| format!("{spec}: {error}"))?,
            false => PathBuf::new(),
        };
        let Some(server) = &self.server else {
            return install(
                &self.own,
                match local {
                    true => path.to_str().unwrap_or(spec),
                    false => spec,
                },
            );
        };
        if !local {
            let report = server.call("plugins.add", Value::Null, Some(json!({"spec": spec})))?;
            return Ok(serde_json::from_value::<Report>(report)?.lines);
        }
        let package = match path.is_dir() {
            true => pack(&path)?,
            false => fs::read(&path)?,
        };
        if package.len() > berth_api::ops::PORTAL_BYTES {
            return Err(format!("{spec} is larger than the 8 MiB a server is sent").into());
        }
        let data = json!({"base64": berth_api::b64(&package)});
        let report = server.call("plugins.upload", Value::Null, Some(data))?;
        Ok(serde_json::from_value::<Report>(report)?.lines)
    }

    /// Remove a plugin by its id or its npm name.
    pub fn remove(&self, name: &str) -> Result<Vec<String>> {
        let listing = self.list()?;
        let plugin = listing
            .plugins
            .iter()
            .find(|plugin| plugin.id == name || plugin.package == name);
        match (&self.server, plugin) {
            (_, None) => Err(format!("No plugin {name}. See: berth plugins").into()),
            (Some(server), Some(plugin)) => {
                let report = server.call("plugins.remove", json!({"id": plugin.id}), None)?;
                Ok(serde_json::from_value::<Report>(report)?.lines)
            }
            (None, Some(plugin)) => {
                let found = self.found();
                let plugin = found
                    .iter()
                    .find(|known| known.id == plugin.id)
                    .ok_or("The plugin is gone")?;
                remove(&self.own, plugin)
            }
        }
    }

    /// Which of the user's plugins have a newer version.
    pub fn check(&self) -> Result<PluginUpdates> {
        if let Some(server) = &self.server {
            return Ok(serde_json::from_value(server.call(
                "plugins.check",
                Value::Null,
                None,
            )?)?);
        }
        check(&self.own, &self.found())
    }

    /// Update one plugin, by its id or npm name, or all. A server says which
    /// of its hosts are behind afterwards.
    pub fn update(&self, name: Option<&str>) -> Result<Report> {
        if let Some(server) = &self.server {
            let data = json!({"id": name});
            let report = server.call("plugins.update", Value::Null, Some(data))?;
            return Ok(serde_json::from_value(report)?);
        }
        Ok(Report {
            lines: update(&self.own, &self.found(), name)?,
            behind: vec![],
        })
    }
}

/// A folder as the .tgz `npm pack` makes of it, without running its scripts.
fn pack(folder: &Path) -> Result<Vec<u8>> {
    let into = tempfile::tempdir()?;
    let output = std::process::Command::new("npm")
        .args(["pack", "--ignore-scripts", "--silent", "--pack-destination"])
        .arg(into.path())
        .arg(folder)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| format!("npm could not be started: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "npm pack {}: {}",
            folder.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    let packed = fs::read_dir(into.path())?
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|end| end == "tgz"))
        .ok_or("npm pack made no .tgz")?;
    Ok(fs::read(packed)?)
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
        write(
            "a/one",
            r#"{"name":"berth-plugin-one","displayName":"First","berth":{}}"#,
        );
        write("a/not-a-plugin", r#"{"name":"left-pad"}"#);
        write("a/broken", "{");
        write("a/bad", r#"{"name":"Bad Name","berth":{}}"#);
        write(
            "b/node_modules/@me/berth-plugin-one",
            r#"{"name":"@me/berth-plugin-one","version":"2.0.0","displayName":"Second",
                "berth":{"features":[
                  {"id":"x","group":"Development Tools","actions":[
                    {"name":"go","run":"true","user":"root","agent":true,"confirm":"Sure?",
                     "arguments":[{"name":"folder","required":true},{"name":"Bad Name"}]},
                    {"name":"no command"},
                    {"name":"plain","run":"id"}]},
                  {"id":"y","services":[{"name":"manager","user":"root","methods":[{"name":"start"}]}],
                   "channels":[{"topic":"progress","keep":"none","max":1000}]},
                  {"id":"Not Valid"}]}}"#,
        );
        write(
            "b/node_modules/other",
            r#"{"name":"other","berth":{"id":"renamed"}}"#,
        );
        let plugins = load(&[
            base.path().join("a"),
            base.path().join("b"),
            "missing".into(),
        ]);
        let ids: Vec<_> = plugins.iter().map(|plugin| plugin.id.as_str()).collect();
        assert_eq!(ids, ["one", "renamed"]);
        let one = &plugins[0];
        assert_eq!(
            (one.label.as_str(), one.version.as_str()),
            ("Second", "2.0.0")
        );
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
        let go = &one.features[0].actions[0];
        assert!(go.agent && go.confirm.as_deref() == Some("Sure?"));
        assert_eq!(go.arguments.len(), 1);
        assert!(go.arguments[0].required && !one.features[0].actions[1].agent);
        let extra = &one.features[1];
        assert_eq!(extra.services[0].methods[0].name, "start");
        assert_eq!(extra.channels[0].keep, Some(berth_api::Keep::None));
    }

    #[test]
    fn provisioners_are_read_and_their_commands_kept_inside_the_package() {
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("plugins/cloud");
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::write(dir.join("bin/provision"), "#!/bin/sh\n").unwrap();
        fs::write(base.path().join("plugins/outside"), "#!/bin/sh\n").unwrap();
        std::os::unix::fs::symlink(base.path().join("plugins/outside"), dir.join("bin/out"))
            .unwrap();
        fs::write(
            dir.join("package.json"),
            r#"{"name": "berth-plugin-cloud", "berth": {"provisioners": [
                {"id": "vm", "label": "A VM", "description": "One machine.", "command": "bin/provision",
                 "setup": "seed", "archive": true, "verbs": ["stop", "list", "reboot"],
                 "secrets": ["key", {"name": "cert", "required": false}, {"name": "Bad Name"}, "key"],
                 "arguments": [{"name": "server", "label": "Server", "required": true},
                               {"name": "cpus", "default": "4", "choices": "seq 8", "kind": "resource"},
                               {"name": "image", "kind": "colour", "help": "What it starts from.",
                                "values": ["a", "b"]}, {"name": "Bad"}]},
                {"id": "ssh", "command": "./bin/provision", "setup": "apply", "archive": true},
                {"id": "up", "command": "../outside", "setup": "apply"},
                {"id": "absolute", "command": "/bin/sh", "setup": "apply"},
                {"id": "linked", "command": "bin/out", "setup": "apply"},
                {"id": "missing", "command": "bin/none", "setup": "apply"},
                {"id": "folder", "command": "bin", "setup": "apply"},
                {"id": "odd", "command": "bin/provision", "setup": "magic"},
                {"id": "Bad Id", "command": "bin/provision", "setup": "seed"}]}}"#,
        )
        .unwrap();
        // Provisioners and no features is a plugin too.
        let plugins = load(&[base.path().join("plugins")]);
        assert_eq!(plugins.len(), 1);
        let ids: Vec<&str> = plugins[0]
            .provisioners
            .iter()
            .map(|provisioner| provisioner.id.as_str())
            .collect();
        assert_eq!(ids, ["vm", "ssh"]);
        let vm = &plugins[0].provisioners[0];
        assert_eq!(vm.command, plugins[0].dir.join("bin/provision"));
        assert_eq!((vm.setup, vm.archive), (Setup::Seed, true));
        assert_eq!(vm.verbs, ["stop", "list"]);
        assert_eq!(
            vm.secrets,
            [
                ProvisionerSecret {
                    name: "key".into(),
                    required: true
                },
                ProvisionerSecret {
                    name: "cert".into(),
                    required: false
                },
            ]
        );
        let arguments: Vec<_> = vm
            .arguments
            .iter()
            .map(|argument| {
                (
                    argument.name.as_str(),
                    argument.required,
                    argument.choices.as_str(),
                    argument.kind.as_str(),
                )
            })
            .collect();
        assert_eq!(
            arguments,
            [
                ("server", true, "", ""),
                ("cpus", false, "", "resource"),
                ("image", false, "", "")
            ]
        );
        // Only a seeded machine is given the archive.
        let ssh = &plugins[0].provisioners[1];
        assert_eq!(
            (ssh.setup, ssh.archive, ssh.label.as_str()),
            (Setup::Apply, false, "ssh")
        );
        let info = plugins[0].info(base.path());
        assert_eq!(info.provisioners[0].id, "cloud/vm");
        // A provisioner's argument keeps its help and its values.
        assert_eq!(vm.arguments[2].help, "What it starts from.");
        assert_eq!(vm.arguments[2].values, ["a", "b"]);
        // The marker reaches the API and comes back from it.
        let sent = serde_json::to_value(&info.provisioners[0]).unwrap();
        assert_eq!(sent["arguments"][1]["kind"], "resource");
        assert!(sent["arguments"][0].get("kind").is_none());
        let back: berth_api::ProvisionerInfo = serde_json::from_value(sent).unwrap();
        assert!(back.arguments[1].is_resource() && !back.arguments[0].is_resource());
        assert_eq!(info.provisioners[0].secrets, ["key", "cert"]);
        let listing = describe(&[info]);
        assert!(
            listing.contains("  provisioner  cloud/vm  A VM (sets itself up from the cloud seed; also stop, list; asks server; asks cpus (default 4); may ask image: a or b; secret key; secret cert)"),
            "{listing}"
        );
    }

    #[test]
    fn specs_for_a_server_are_packages_or_addresses() {
        for spec in [
            "berth-plugin-foo",
            "@acme/berth-plugin-foo",
            "@acme/berth-plugin-foo@1.2.0",
            "foo@^2",
            "foo@latest",
            "https://npm.example/foo-1.0.0.tgz",
        ] {
            assert!(registry_spec(spec), "{spec}");
        }
        for spec in [
            "",
            "-g",
            "--prefix=/",
            "./x",
            "/etc",
            "~/x",
            "file:x",
            "git+ssh://x/y",
            "github:a/b",
            "http://x/y.tgz",
            "a b",
            "Foo",
            "@acme",
            "@/x",
            "foo@",
            "foo@1 <2",
        ] {
            assert!(!registry_spec(spec), "{spec}");
        }
        assert!(local_spec("./x") && local_spec("foo-1.0.0.tgz") && local_spec("~/src/x"));
        assert!(!local_spec("@acme/foo") && !local_spec("https://x/y.tgz"));
    }

    #[test]
    fn where_a_plugin_comes_from_says_whether_it_goes() {
        let base = tempfile::tempdir().unwrap();
        let own = base.path().join("own");
        let write = |dir: &Path| {
            fs::create_dir_all(dir).unwrap();
            fs::write(
                dir.join("package.json"),
                r#"{"name":"berth-plugin-x","berth":{"features":[{"id":"a","actions":[{"name":"go","run":"true"}]}]}}"#,
            )
            .unwrap();
        };
        let outside = base.path().join("elsewhere/x");
        write(&base.path().join("source/plugins/x"));
        write(&outside);
        // npm installs a folder as a link to it.
        fs::create_dir_all(own.join("node_modules")).unwrap();
        std::os::unix::fs::symlink(&outside, own.join("node_modules/berth-plugin-x")).unwrap();
        let shipped = load(&[base.path().join("source/plugins")]);
        assert_eq!(shipped[0].origin(&own), PluginOrigin::Shipped);
        let linked = found(&base.path().join("source"), &own);
        assert_eq!(linked.len(), 1);
        assert_eq!(linked[0].origin(&own), PluginOrigin::Npm);
        let info = linked[0].info(&own);
        assert_eq!(info.features[0].tasks[0].name, "go");
        assert!(describe(&[info]).contains("    task     x:go"));
        assert!(remove(&own, &shipped[0]).is_err());
        fs::remove_file(own.join("node_modules/berth-plugin-x")).unwrap();
        write(&own.join("x"));
        let copied = load(std::slice::from_ref(&own));
        assert_eq!(copied[0].origin(&own), PluginOrigin::Folder);
        remove(&own, &copied[0]).unwrap();
        assert!(!own.join("x").exists() && outside.exists());
    }

    #[test]
    fn uploads_go_when_npm_no_longer_needs_them() {
        let own = tempfile::tempdir().unwrap();
        let own = own.path();
        fs::create_dir_all(uploads(own)).unwrap();
        fs::write(uploads(own).join("1.tgz"), "").unwrap();
        fs::write(uploads(own).join("2.tgz"), "").unwrap();
        fs::write(
            own.join("package.json"),
            r#"{"dependencies":{"berth-plugin-x":"file:uploads/2.tgz"}}"#,
        )
        .unwrap();
        forget_uploads(own);
        assert!(!uploads(own).join("1.tgz").exists() && uploads(own).join("2.tgz").exists());
        assert!(install_upload(own, b"not a tarball").is_err());
    }

    #[test]
    fn updates_are_asked_for_packages_of_a_registry_only() {
        let base = tempfile::tempdir().unwrap();
        let own = base.path().join("own");
        let write = |dir: &Path, name: &str| {
            fs::create_dir_all(dir).unwrap();
            fs::write(
                dir.join("package.json"),
                format!(r#"{{"name":"{name}","version":"1.0.0","berth":{{}}}}"#),
            )
            .unwrap();
        };
        write(
            &base.path().join("source/plugins/shipped"),
            "berth-plugin-shipped",
        );
        write(&own.join("node_modules/berth-plugin-a"), "berth-plugin-a");
        write(
            &own.join("node_modules/@acme/berth-plugin-b"),
            "@acme/berth-plugin-b",
        );
        write(&own.join("node_modules/berth-plugin-c"), "berth-plugin-c");
        write(&own.join("d"), "berth-plugin-d");
        fs::write(
            own.join("package.json"),
            r#"{"dependencies":{"berth-plugin-a":"^1.0.0","@acme/berth-plugin-b":"latest",
                "berth-plugin-c":"file:uploads/1.tgz"}}"#,
        )
        .unwrap();
        let plugins = found(&base.path().join("source"), &own);
        let (mut updates, ask) = updatable(&own, &plugins);
        assert_eq!(ask, ["berth-plugin-a", "@acme/berth-plugin-b"]);
        let ids: Vec<_> = updates.iter().map(|update| update.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c", "d"]);
        assert!(updates[2].note.contains("add it again") && updates[3].note.contains("folder"));
        take_outdated(
            &mut updates,
            &ask,
            r#"{"berth-plugin-a":{"current":"1.0.0","wanted":"1.0.0","latest":"2.0.0"},
                "@acme/berth-plugin-b":[{"current":"1.0.0","latest":"1.0.0"}]}"#,
        );
        assert_eq!(updates[0].latest.as_deref(), Some("2.0.0"));
        assert_eq!(updates[1].latest, None);
        take_outdated(
            &mut updates,
            &ask,
            r#"{"error":{"code":"ENOTFOUND","summary":"no"}}"#,
        );
        assert!(updates[1].note.contains("could not ask"));
        // A shipped plugin is updated with berth.
        let error = update(&own, &plugins, Some("shipped"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("berth update"), "{error}");
        let error = update(&own, &plugins, Some("c")).unwrap_err().to_string();
        assert!(error.contains("add it again"), "{error}");
    }

    #[test]
    fn a_changed_file_changes_the_digest_and_shows_the_host_behind() {
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("plugins/x");
        fs::create_dir_all(dir.join("tasks")).unwrap();
        fs::write(
            dir.join("package.json"),
            r#"{"name":"berth-plugin-x","version":"1.0.0","berth":{}}"#,
        )
        .unwrap();
        fs::write(dir.join("tasks/main.yml"), "- debug: {msg: one}\n").unwrap();
        fs::create_dir_all(dir.join("node_modules/dep")).unwrap();
        let plugins = load(&[base.path().join("plugins")]);
        let before = berth_api::Applied {
            berth: berth_api::VERSION.into(),
            plugins: records(&plugins),
        };
        assert_eq!(behind(&before, &plugins), None);
        // What npm put beside it does not count; a task does.
        fs::write(dir.join("node_modules/dep/index.js"), "").unwrap();
        assert_eq!(behind(&before, &plugins), None);
        fs::write(dir.join("tasks/main.yml"), "- debug: {msg: two}\n").unwrap();
        let changed = behind(&before, &plugins).unwrap();
        assert_eq!(changed.text(), "x 1.0.0 (changed)");
        let older = berth_api::Applied {
            berth: "0.0.1".into(),
            ..before.clone()
        };
        assert_eq!(
            behind(&older, &[]).unwrap().text(),
            format!("berth 0.0.1 → {}, x 1.0.0 (gone)", berth_api::VERSION)
        );
    }
}
