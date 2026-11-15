//! What the set-up tabs of the screens do, as commands: a new host through
//! the same flow the screens walk, and the keys, secrets, registries,
//! plugins and settings of the home screen.

use super::args::{self, Global, Key, Plugin, Registry, Secret};
use super::ask;
use super::client::{Client, Subject};
use super::fail::{Fail, Result};
use super::follow;
use super::host;
use super::out::{self, Style, Table, styled};
use berth_api::Act;
use berth_tui::fields::{Action, Step};
use berth_tui::flow::{self, Catalogue, Flow, Given, Hash, Maker, Stop};
use berth_tui::setup::Feature;
use serde_json::{Value, json};

/// The keys of `--set`, for the help of `berth host create` and `berth host set`.
pub fn keys_help() -> String {
    let mut text = String::from("How it is installed, with --method:\n");
    for action in Action::SERVED {
        text.push_str(&format!("  {:<12} {}\n", action.key(), action.label()));
    }
    text.push_str(
        "\nThe fields of a profile, with --set KEY=VALUE. A switch takes on or off. feature.PLUGIN/FEATURE=on|off, secret.NAME=VARIABLE|PATH and registry.NAME=on|off give a host those:\n",
    );
    let wide = flow::keys()
        .iter()
        .map(|(key, _, _)| key.len())
        .max()
        .unwrap_or(0);
    for (key, label, help) in flow::keys() {
        let first = help
            .split(". ")
            .next()
            .unwrap_or(help)
            .trim_end_matches('.');
        let _ = label;
        text.push_str(&format!("  {key:<wide$}  {first}\n"));
    }
    text
}

/// What the server gives the set-up of a new host.
pub fn catalogue(client: &Client) -> Result<Catalogue> {
    let found = client.call("setup.catalogue", json!({}), None)?;
    let features = found["features"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|feature| {
            let text = |key: &str| feature[key].as_str().unwrap_or("").to_owned();
            Feature {
                key: text("key"),
                label: text("label"),
                group: text("group"),
                help: text("help"),
            }
        })
        .collect();
    let secrets = client
        .call("secrets.list", json!({}), None)
        .map(|found| names(&found["names"]))
        .unwrap_or_default();
    let own_key = client
        .call("keys.list", json!({}), None)
        .ok()
        .and_then(|found| {
            found["keys"]
                .as_array()?
                .iter()
                .find(|key| key["name"] == "berth")
                .map(|key| key["public"].as_str().unwrap_or("").to_owned())
        })
        .unwrap_or_default();
    Ok(Catalogue {
        template: found["template"].clone(),
        features,
        secrets,
        own_key,
        provisioners: vec![],
    })
}

fn names(list: &Value) -> Vec<String> {
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(|name| name.as_str().map(str::to_owned))
        .collect()
}

/// The server makes the hashes of the accounts' passwords, and keeps the
/// passwords it makes up.
struct Server<'a>(&'a Client, bool);

impl Maker for Server<'_> {
    fn hash(&mut self, host: &str, hash: Hash) -> std::result::Result<String, String> {
        let asked = match hash {
            Hash::Generate { user } => {
                self.1 = true;
                json!({"what": "password", "host": host, "user": user})
            }
            Hash::Of(password) => json!({"what": "password", "password": password}),
        };
        let made = self
            .0
            .call("setup.generate", json!({}), Some(asked))
            .map_err(|fail| fail.message)?;
        Ok(made["value"].as_str().unwrap_or("").to_owned())
    }
}

/// The profile `--from` names: a host's, a file, or standard input.
fn starting_profile(client: &Client, from: &str) -> Result<Value> {
    if from == "-" || std::path::Path::new(from).is_file() {
        let bytes = host::read_file(from)?;
        return serde_json::from_slice(&bytes)
            .map_err(|error| Fail::usage(format!("{from} holds no JSON profile: {error}")));
    }
    let mut profile = host::profile_of(client, from)
        .map_err(|fail| fail.context(&format!("--from {from} is no file and no host")))?;
    // What belongs to the other host itself stays with it.
    if let Some(profile) = profile.as_object_mut() {
        for key in [
            "hostname",
            "operator_password_hash",
            "agent_password_hash",
            "tailscale_auth_key",
        ] {
            profile.remove(key);
        }
    }
    if let Some(access) = profile.get_mut("access").and_then(Value::as_object_mut) {
        access.remove("address");
    }
    Ok(profile)
}

/// `berth host create`: the flow of the screens' set-up tabs, asked inline
/// or taken from the flags, then saved, then run as its way says.
pub fn create(client: &Client, create: &args::HostCreate) -> Result<()> {
    let name = &create.name;
    if client
        .call("hosts.get", json!({"name": name}), None)
        .is_ok()
    {
        return match &create.provisioner {
            Some(_) => make_saved(client, create),
            None => Err(Fail::failed(format!(
                "There is a host {name} already. Change it with berth host set {name} KEY=VALUE, or berth ui {name}"
            ))),
        };
    }
    let mut catalogue = catalogue(client)?;
    if let Some(from) = &create.from {
        catalogue.template = starting_profile(client, from)?;
    }
    let method = match &create.method {
        Some(key) => Some(Action::by_key(key).ok_or_else(|| {
            let known: Vec<&str> = Action::SERVED.iter().map(|action| action.key()).collect();
            Fail::usage(format!("--method is {}, not {key}", known.join(", ")))
        })?),
        None => None,
    };
    if method.is_none_or(|method| method == Action::Machine) {
        catalogue.provisioners = provisioners(client)?;
    }
    let mut given = Given {
        method,
        generate_passwords: create.generate_passwords,
        provisioner: create.provisioner.clone(),
        ..Given::default()
    };
    given.set(&create.set).map_err(Fail::usage)?;
    given
        .features
        .extend(create.feature.iter().map(|key| (key.clone(), true)));
    given
        .features
        .extend(create.no_feature.iter().map(|key| (key.clone(), false)));
    for word in &create.arguments {
        let (key, value) = word.split_once('=').ok_or_else(|| {
            Fail::usage(format!(
                "The provisioner's arguments are NAME=VALUE, not {word}"
            ))
        })?;
        given.arguments.push((key.to_owned(), value.to_owned()));
    }
    if !given.arguments.is_empty() && given.provisioner.is_none() && method != Some(Action::Machine)
    {
        return Err(Fail::usage(
            "NAME=VALUE arguments go to a provisioner: add --provisioner NAME",
        ));
    }
    let mut flow = Flow::new(name, catalogue);
    let mut maker = Server(client, false);
    flow.run(&given, &mut ask::asker(), &mut maker)
        .map_err(stopped)?;
    host::save(client, name, &flow.config)?;
    out::done(format!("Saved {name}"));
    if maker.1 {
        out::hint(format!("Its passwords: berth host credentials {name}"));
    }
    run_way(client, &flow, create)
}

/// The provisioners that can make a host, as the Install tab lists them.
fn provisioners(client: &Client) -> Result<Vec<berth_api::ProvisionerInfo>> {
    let listed = client.call("provisioners.list", json!({}), None)?;
    Ok(serde_json::from_value(listed)?)
}

/// Make the machine of a host that is saved and has none, with its profile
/// as it is, as the Run tab of its set-up does.
fn make_saved(client: &Client, create: &args::HostCreate) -> Result<()> {
    let name = &create.name;
    if let Some(machine) = host::machine_of(client, name)? {
        return Err(Fail::failed(format!(
            "{name} has a machine already, {} of {}. berth host show {name} says how it is",
            machine["external_id"].as_str().unwrap_or(""),
            machine["provisioner"].as_str().unwrap_or("")
        )));
    }
    let catalogue = Catalogue {
        provisioners: provisioners(client)?,
        ..Catalogue::default()
    };
    let mut flow = Flow::new(name, catalogue);
    flow.config = host::profile_of(client, name)?;
    let mut given = Given {
        provisioner: create.provisioner.clone(),
        ..Given::default()
    };
    for word in &create.arguments {
        let (key, value) = word.split_once('=').ok_or_else(|| {
            Fail::usage(format!(
                "The provisioner's arguments are NAME=VALUE, not {word}"
            ))
        })?;
        given.arguments.push((key.to_owned(), value.to_owned()));
    }
    flow.machine_questions(&given, &mut ask::asker())
        .map_err(stopped)?;
    run_way(client, &flow, create)
}

/// Why the flow stopped, as the command line says it: with the flags that
/// give what is missing.
fn stopped(stop: Stop) -> Fail {
    match stop {
        Stop::Missing(question) => Fail::usage(format!(
            "{} is missing: pass {}",
            question.label, question.flag
        )),
        Stop::Problems(problems) => {
            let said: Vec<String> = problems
                .iter()
                .map(|problem| match problem.field {
                    Some(field) if field.is_password_hash() => {
                        format!(
                            "{} (--generate-passwords, or {})",
                            problem.text,
                            flow::flag_of(field)
                        )
                    }
                    Some(field) => format!("{} ({})", problem.text, flow::flag_of(field)),
                    None => problem.text.clone(),
                })
                .collect();
            Fail::usage(format!("Not saved: {}", said.join("; ")))
        }
        Stop::Refused(why) if why == "Stopped" => Fail::exit(130),
        Stop::Refused(why) => Fail::usage(why),
    }
}

/// The Run tab: create the host with its provisioner, or install it the
/// way chosen, when that was asked for.
fn run_way(client: &Client, flow: &Flow, create: &args::HostCreate) -> Result<()> {
    let name = &flow.name;
    let host_type = berth_api::profile::host_type(&flow.config);
    if flow.action == Action::Machine {
        let request = flow.request().map_err(Fail::usage)?;
        let made = client.call(
            "resources.create",
            json!({}),
            Some(serde_json::to_value(&request)?),
        )?;
        let made: berth_api::Created = serde_json::from_value(made)?;
        let acted = berth_api::Acted {
            job: Some(made.job),
            ..berth_api::Acted::default()
        };
        let title = format!("Creating {name} with {}", request.provisioner);
        follow::acted(client, acted, &title, create.detach.detach, false)?;
        return Ok(());
    }
    let (question, then) = match (flow.action, host_type) {
        (Action::Iso, _) => (
            "Build the USB installer now?",
            format!("berth host iso {name} --write DEVICE"),
        ),
        (Action::CloudSeed, _) => {
            return {
                out::hint(format!(
                    "Its cloud seed: berth host seed {name} > user-data"
                ));
                Ok(())
            };
        }
        (Action::Reach, _) | (Action::Ssh, berth_api::HostType::Ssh) => {
            out::hint(format!("Log in to it once: berth key trust {name}"));
            return Ok(());
        }
        (_, berth_api::HostType::Managed) => (
            "Install the controller on it now?",
            format!("berth host apply {name}"),
        ),
        _ => ("Apply it now?", format!("berth host apply {name}")),
    };
    let run = create.run
        || (out::get().ask
            && matches!(
                ask::ask(&berth_tui::flow::Question {
                    step: Step::Run,
                    label: question.into(),
                    help: String::new(),
                    flag: "--run".into(),
                    asked: berth_tui::flow::Asked::Confirm { default: false },
                    required: false,
                })?,
                berth_tui::flow::Answer::Confirm(true)
            ));
    if !run {
        out::hint(format!("Install it with: {then}"));
        return Ok(());
    }
    match flow.action {
        Action::Iso => {
            let acted = client.act(&Subject::Host(name.clone()), "installer", &Act::default())?;
            follow::acted(
                client,
                acted,
                &format!("Building the installer of {name}"),
                create.detach.detach,
                false,
            )?;
            Ok(())
        }
        _ => host::apply(
            client,
            &args::HostApply {
                name: name.clone(),
                sudo: create.sudo.clone(),
                detach: create.detach.clone(),
            },
        ),
    }
}

// ── keys ────────────────────────────────────────────────────────────────────

pub fn key(client: &Client, command: Key) -> Result<()> {
    match command {
        Key::List => {
            let found = client.call("keys.list", json!({}), None)?;
            out::data(&found, || {
                let mut table = Table::new(&["name", "fingerprint", "hosts"]);
                for key in found["keys"].as_array().into_iter().flatten() {
                    let mut name = key["name"].as_str().unwrap_or("").to_owned();
                    if key["pending"].is_object() {
                        name.push_str(&styled(" (rotating)", Style::Warn));
                    }
                    table.row(vec![
                        name,
                        key["fingerprint"].as_str().unwrap_or("").to_owned(),
                        names(&key["hosts"]).join(", "),
                    ]);
                }
                table.print("No keys");
            });
            Ok(())
        }
        Key::Show { name } => {
            let found = client.call("keys.list", json!({}), None)?;
            let key = found["keys"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|key| key["name"] == name.as_str())
                .cloned()
                .ok_or_else(|| Fail::failed(format!("No key {name}. berth key list names them")))?;
            out::data(&key, || {
                println!("{}", key["public"].as_str().unwrap_or(""))
            });
            Ok(())
        }
        Key::Create { name } => {
            let made = client.call("keys.add", json!({"name": name}), None)?;
            out::data(&made, || {
                out::done(format!("Made the key {name}"));
                println!("{}", made["public"].as_str().unwrap_or(""));
            });
            Ok(())
        }
        Key::Import { name, file } => {
            let private = host::read_file(&file)?;
            let data = json!({"base64": berth_api::b64(&private)});
            let made = client.call("keys.add", json!({"name": name}), Some(data))?;
            out::data(&made, || {
                out::done(format!(
                    "Added the key {name}: {}",
                    made["fingerprint"].as_str().unwrap_or("")
                ))
            });
            Ok(())
        }
        Key::Remove { name } => {
            ask::confirm(&format!(
                "This removes the key {name} from berth's key ring."
            ))?;
            client.call("keys.remove", json!({"name": name}), None)?;
            out::done(format!("Removed the key {name}"));
            Ok(())
        }
        Key::Rotate { name, detach } => {
            ask::confirm(&format!(
                "This replaces the key {name} on every host that takes it."
            ))?;
            let acted: berth_api::Acted =
                serde_json::from_value(client.call("keys.rotate", json!({"name": name}), None)?)?;
            follow::acted(
                client,
                acted,
                &format!("Rotating the key {name}"),
                detach.detach,
                false,
            )?;
            Ok(())
        }
        Key::Trust {
            host,
            key,
            password_stdin,
        } => {
            let password = match password_stdin {
                true => Some(ask::secret_from(
                    "Password",
                    None,
                    true,
                    "--password-stdin",
                )?),
                false => None,
            };
            host::trust(client, &host, key.as_deref(), password)
        }
    }
}

// ── secrets ─────────────────────────────────────────────────────────────────

pub fn secret(client: &Client, command: Secret) -> Result<()> {
    match command {
        Secret::List => {
            let found = client.call("secrets.list", json!({}), None)?;
            out::data(&found, || {
                let all = names(&found["names"]);
                if all.is_empty() {
                    out::hint("No secrets. Set one with berth secret set NAME");
                }
                all.iter().for_each(|name| println!("{name}"));
            });
            Ok(())
        }
        Secret::Set { name, value_file } => {
            let value = ask::secret_from(
                &format!("Value of {name}"),
                value_file.as_deref(),
                false,
                "--value-file FILE",
            )?;
            client.call(
                "secrets.set",
                json!({"name": name}),
                Some(json!({"value": value})),
            )?;
            out::done(format!("Set the secret {name}"));
            Ok(())
        }
        Secret::Remove { name } => {
            ask::confirm(&format!("This removes the secret {name}."))?;
            client.call("secrets.remove", json!({"name": name}), None)?;
            out::done(format!("Removed the secret {name}"));
            Ok(())
        }
    }
}

// ── registries ──────────────────────────────────────────────────────────────

pub fn registry(global: &Global, command: Registry) -> Result<()> {
    #[cfg(target_os = "linux")]
    match command {
        Registry::Publish {
            destination,
            host,
            login,
        } => return crate::local::publish(global, &destination, host.as_deref(), login.as_deref()),
        Registry::PushImage { destination, tag } => {
            return crate::local::push_image(global, &destination, &tag);
        }
        _ => {}
    }
    let client = Client::open(global)?;
    let registries = || -> Result<serde_json::Map<String, Value>> {
        let found = client.call("registries.list", json!({}), None)?;
        Ok(found["registries"].as_object().cloned().unwrap_or_default())
    };
    match command {
        Registry::List => {
            let found = client.call("registries.list", json!({}), None)?;
            out::data(&found, || {
                let mut table = Table::new(&["name", "type", "url", "user", "repositories"]);
                for (name, registry) in found["registries"].as_object().into_iter().flatten() {
                    let repositories: Vec<String> = registry["repositories"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|repo| {
                            format!(
                                "{} {}",
                                repo["format"].as_str().unwrap_or(""),
                                repo["name"].as_str().unwrap_or("")
                            )
                        })
                        .collect();
                    table.row(vec![
                        name.clone(),
                        registry["type"].as_str().unwrap_or("").to_owned(),
                        registry["url"].as_str().unwrap_or("").to_owned(),
                        registry["username"].as_str().unwrap_or("").to_owned(),
                        repositories.join(", "),
                    ]);
                }
                table.print("No registries. Add one with berth registry add NAME TYPE URL");
            });
            Ok(())
        }
        Registry::Add {
            name,
            kind,
            url,
            user,
        } => {
            let mut registry = json!({"type": kind, "url": url.trim_end_matches('/')});
            if let Some(user) = user {
                registry["username"] = json!(user);
            }
            // Adding again changes the address or user, and keeps the repositories.
            if let Some(known) = registries()?.get(&name) {
                registry["repositories"] = known["repositories"].clone();
            }
            client.call("registries.save", json!({"name": name}), Some(registry))?;
            out::done(format!("Saved the registry {name}"));
            out::hint(format!("Keep its login with: berth registry login {name}"));
            Ok(())
        }
        Registry::AddRepo {
            name,
            format,
            repository,
            address,
        } => {
            let mut registry = registries()?
                .get(&name)
                .filter(|registry| registry["type"] == "nexus")
                .cloned()
                .ok_or_else(|| Fail::usage(format!("{name} is no Nexus registry")))?;
            let mut entry = json!({"format": format, "name": repository});
            if let Some(address) = address {
                entry["address"] = json!(address);
            }
            let mut repositories = registry["repositories"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            repositories.retain(|known| known["name"] != repository.as_str());
            repositories.push(entry);
            registry["repositories"] = json!(repositories);
            client.call("registries.save", json!({"name": name}), Some(registry))?;
            out::done(format!("Added {repository} to {name}"));
            Ok(())
        }
        Registry::Login {
            name,
            password_stdin,
        } => {
            if !registries()?.contains_key(&name) {
                return Err(Fail::failed(format!(
                    "No registry {name}. berth registry list names them"
                )));
            }
            let value = ask::secret_from(
                &format!("Password or token of {name}"),
                None,
                password_stdin,
                "--password-stdin",
            )?;
            client.call(
                "secrets.set",
                json!({"name": format!("registry/{name}")}),
                Some(json!({"value": value})),
            )?;
            out::done(format!("Kept the login of {name}"));
            Ok(())
        }
        Registry::Check { name } => {
            let names: Vec<String> = match name {
                Some(name) => vec![name],
                None => registries()?.keys().cloned().collect(),
            };
            let mut all = serde_json::Map::new();
            let mut failed = false;
            for name in names {
                let checked = client.call("registries.check", json!({"name": name}), None)?;
                failed |= checked["lines"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|line| line.as_str().is_some_and(|line| line.starts_with('✗')));
                all.insert(name, checked);
            }
            out::data(&Value::Object(all.clone()), || {
                for (name, checked) in &all {
                    println!("{}", styled(name, Style::Bold));
                    for line in names_of(&checked["lines"]) {
                        println!("  {line}");
                    }
                }
            });
            match failed {
                true => Err(Fail::exit(1)),
                false => Ok(()),
            }
        }
        Registry::Remove { name } => {
            ask::confirm(&format!("This removes the registry {name} with its login."))?;
            client.call("registries.remove", json!({"name": name}), None)?;
            out::done(format!("Removed the registry {name}"));
            Ok(())
        }
        #[cfg(target_os = "linux")]
        Registry::Publish { .. } | Registry::PushImage { .. } => unreachable!("handled above"),
    }
}

fn names_of(list: &Value) -> Vec<String> {
    names(list)
}

// ── plugins ─────────────────────────────────────────────────────────────────

fn report(found: &Value) {
    out::data(found, || {
        for line in names(&found["lines"]) {
            println!("{line}");
        }
        for host in found["behind"].as_array().into_iter().flatten() {
            out::warn(format!(
                "{} needs applying: {}",
                host["name"].as_str().unwrap_or(""),
                host["behind"]["text"].as_str().unwrap_or("")
            ));
        }
        if found["behind"]
            .as_array()
            .is_some_and(|behind| !behind.is_empty())
        {
            out::hint("berth rollout brings them up to date");
        }
    });
}

pub fn plugin(client: &Client, command: Plugin) -> Result<()> {
    match command {
        Plugin::List => {
            let found = client.call("plugins.list", json!({}), None)?;
            out::data(&found, || {
                let mut table = Table::new(&["id", "version", "from", "features"]);
                for plugin in found["plugins"].as_array().into_iter().flatten() {
                    let features: Vec<String> = plugin["features"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|feature| feature["id"].as_str().map(str::to_owned))
                        .collect();
                    let origin = match &plugin["origin"] {
                        Value::String(origin) => origin.clone(),
                        other => other
                            .as_object()
                            .and_then(|origin| origin.keys().next().cloned())
                            .unwrap_or_default(),
                    };
                    table.row(vec![
                        plugin["id"].as_str().unwrap_or("").to_owned(),
                        plugin["version"].as_str().unwrap_or("").to_owned(),
                        origin,
                        features.join(", "),
                    ]);
                }
                table.print("No plugins");
            });
            Ok(())
        }
        Plugin::Show { id } => {
            let found = client.call("plugins.list", json!({}), None)?;
            let plugins: Vec<berth_api::PluginInfo> =
                serde_json::from_value(found["plugins"].clone())?;
            let Some(plugin) = plugins.into_iter().find(|plugin| plugin.id == id) else {
                return Err(Fail::failed(format!(
                    "There is no plugin {id}. The plugins are listed by berth plugin list"
                )));
            };
            out::data(&serde_json::to_value(&plugin)?, || {
                print!("{}", super::pages::plugins(std::slice::from_ref(&plugin)));
            });
            Ok(())
        }
        Plugin::Add { spec } => {
            let local = std::path::Path::new(spec.strip_prefix("file:").unwrap_or(&spec));
            if !local.exists() {
                report(&client.call("plugins.add", json!({}), Some(json!({"spec": spec})))?);
                return Ok(());
            }
            // A file or folder of this machine is sent as the .tgz npm packs.
            let package = match local.is_dir() {
                true => pack(local)?,
                false => std::fs::read(local)?,
            };
            if package.len() > berth_api::ops::PORTAL_BYTES {
                return Err(Fail::usage(format!(
                    "{spec} is larger than the 8 MiB a server is sent"
                )));
            }
            let data = json!({"base64": berth_api::b64(&package)});
            report(&client.call("plugins.upload", json!({}), Some(data))?);
            Ok(())
        }
        Plugin::Remove { id } => {
            ask::confirm(&format!("This removes the plugin {id}."))?;
            report(&client.call("plugins.remove", json!({"id": id}), None)?);
            Ok(())
        }
        Plugin::Update { id, check: true } => {
            let found = client.call("plugins.check", json!({}), None)?;
            out::data(&found, || {
                let mut table = Table::new(&["id", "version", "latest"]);
                for plugin in found["plugins"].as_array().into_iter().flatten() {
                    if id.as_ref().is_some_and(|id| plugin["id"] != id.as_str()) {
                        continue;
                    }
                    let latest = match (
                        plugin["latest"].as_str(),
                        plugin["note"].as_str().unwrap_or(""),
                    ) {
                        (Some(latest), _) => styled(latest, Style::Warn),
                        (None, "") => "the latest".to_owned(),
                        (None, note) => note.to_owned(),
                    };
                    table.row(vec![
                        plugin["id"].as_str().unwrap_or("").to_owned(),
                        plugin["version"].as_str().unwrap_or("").to_owned(),
                        latest,
                    ]);
                }
                table.print("None of the server's own plugins came from a registry");
            });
            Ok(())
        }
        Plugin::Update { id, check: false } => {
            let body = match id {
                Some(id) => json!({"id": id}),
                None => json!({}),
            };
            report(&client.call("plugins.update", json!({}), Some(body))?);
            Ok(())
        }
    }
}

/// A folder as the .tgz `npm pack` makes of it, without running its scripts.
fn pack(folder: &std::path::Path) -> Result<Vec<u8>> {
    let into = tempfile::tempdir()?;
    let output = std::process::Command::new("npm")
        .args(["pack", "--ignore-scripts", "--silent", "--pack-destination"])
        .arg(into.path())
        .arg("--")
        .arg(folder)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| Fail::failed(format!("npm could not be started: {error}")))?;
    if !output.status.success() {
        return Err(Fail::failed(format!(
            "npm pack {}: {}",
            folder.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let packed = std::fs::read_dir(into.path())?
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|end| end == "tgz"))
        .ok_or_else(|| Fail::failed("npm pack made no .tgz"))?;
    Ok(std::fs::read(packed)?)
}

// ── settings ────────────────────────────────────────────────────────────────

pub fn config(client: &Client, command: args::Config) -> Result<()> {
    let settings = || -> Result<Vec<Value>> {
        let found = client.call("settings.list", json!({}), None)?;
        Ok(found.as_array().cloned().unwrap_or_default())
    };
    let known = |key: &str| -> Result<Value> {
        settings()?
            .into_iter()
            .find(|setting| setting["key"] == key)
            .ok_or_else(|| {
                Fail::usage(format!("{key} is no setting. berth config list names them"))
            })
    };
    let set = |key: &str, value: &str| -> Result<()> {
        known(key)?;
        client.call(
            "settings.set",
            json!({"key": key}),
            Some(json!({"value": value})),
        )?;
        Ok(())
    };
    match command {
        args::Config::List => {
            let all = settings()?;
            out::data(&Value::Array(all.clone()), || {
                for setting in &all {
                    let value = setting["value"].as_str().unwrap_or("");
                    let shown = match value {
                        "" => styled("(not set)", Style::Dim),
                        value => value.to_owned(),
                    };
                    println!("{} = {shown}", setting["key"].as_str().unwrap_or(""));
                    println!(
                        "    {}",
                        styled(setting["meaning"].as_str().unwrap_or(""), Style::Dim)
                    );
                }
            });
            Ok(())
        }
        args::Config::Get { key } => {
            let setting = known(&key)?;
            out::data(&setting, || {
                println!("{}", setting["value"].as_str().unwrap_or(""))
            });
            Ok(())
        }
        args::Config::Set { key, value } => {
            set(&key, &value)?;
            out::done(format!("{key} = {value}"));
            Ok(())
        }
        args::Config::Unset { key } => {
            set(&key, "")?;
            out::done(format!("{key} is back to its default"));
            Ok(())
        }
    }
}
