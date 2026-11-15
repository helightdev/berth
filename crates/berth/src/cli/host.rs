//! `berth host …`: the hosts of the server, as the screens' home list and
//! a host's overview and set-up show them.

use super::args::{self, Global, Host};
use super::ask;
use super::client::{Client, Subject};
use super::fail::{Fail, Result};
use super::follow;
use super::out::{self, Style, Table, styled};
use berth_api::{Act, HostType, profile};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub fn command(global: &Global, command: Host) -> Result<()> {
    let open = || Client::open(global);
    match command {
        Host::List => list(&open()?),
        Host::Show { name } => show(&open()?, &name),
        Host::Create(create) => super::setup::create(&open()?, &create),
        Host::Add(add) => self::add(&open()?, &add),
        Host::Set(set) => self::set(&open()?, &set.name, &set.values),
        Host::Edit { name } => edit(&open()?, &name),
        Host::Export { name } => {
            let profile = profile_of(&open()?, &name)?;
            out::json(&profile);
            Ok(())
        }
        Host::Import { name, file } => import(&open()?, &name, &file),
        Host::Apply(apply) => self::apply(&open()?, &apply),
        Host::Seed { name, file } => seed(&open()?, &name, file.as_deref()),
        Host::Iso(iso) => installer(&open()?, &iso),
        Host::Rename {
            name,
            new,
            hostname,
        } => rename(&open()?, &name, &new, hostname),
        Host::Remove { name, destroy } => remove(&open()?, &name, destroy),
        Host::Adopt(adopt) => self::adopt(&open()?, &adopt),
        Host::Credentials { name, show } => credentials(&open()?, &name, show),
        Host::Verify { name, accept } => verify(&open()?, &name, accept),
        Host::Start(power) => self::power(&open()?, &power, "start"),
        Host::Stop(power) => self::power(&open()?, &power, "stop"),
        #[cfg(target_os = "linux")]
        Host::Inventory { names } => crate::local::inventory(global, &names),
    }
}

/// The state of a host in a word or two, as the home list shows it.
fn state_of(host: &Value) -> String {
    if host["reachable"] == true {
        return match &host["behind"] {
            Value::Null => styled("answers", Style::Good),
            _ => styled("behind", Style::Warn),
        };
    }
    match host["address"].as_str() {
        Some("") | None => styled("no address", Style::Dim),
        _ => styled("unreachable", Style::Bad),
    }
}

pub fn list(client: &Client) -> Result<()> {
    let hosts = client.call("hosts.list", json!({}), None)?;
    out::data(&hosts, || {
        let mut table = Table::new(&["name", "type", "address", "state", "berthd"]);
        for host in hosts.as_array().into_iter().flatten() {
            let text = |key: &str| host[key].as_str().unwrap_or("").to_owned();
            table.row(vec![
                text("name"),
                text("type"),
                text("address"),
                state_of(host),
                text("version"),
            ]);
        }
        table.print(
            "No hosts yet. Add one with berth host create NAME, or berth host add NAME ADDRESS",
        );
    });
    Ok(())
}

pub fn show(client: &Client, name: &str) -> Result<()> {
    let host = client.call("hosts.get", json!({"name": name}), None)?;
    let machine = machine_of(client, name).ok().flatten();
    // The profile needs the role admin, and the actions a host that answers: each is left out without.
    let given = profile_of(client, name).ok();
    let actions = client.actions(&Subject::Host(name.to_owned())).ok();
    out::data(&host, || {
        let text = |key: &str| host[key].as_str().unwrap_or("").to_owned();
        let behind = match &host["behind"] {
            Value::Null => String::new(),
            behind => behind["text"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| behind.to_string()),
        };
        let mut pairs = vec![
            ("Name", text("name")),
            ("Type", text("type")),
            ("Address", text("address")),
            ("State", state_of(&host)),
            ("Problem", text("error")),
            ("berthd", text("version")),
            ("Applied", text("applied")),
            ("Behind", behind),
        ];
        if host["seen"].is_string() && host["seen"] != host["type"] {
            pairs.push(("Seen as", text("seen")));
        }
        if let Some(machine) = &machine {
            let made = format!(
                "{} of {}, {}",
                machine["external_id"].as_str().unwrap_or(""),
                machine["provisioner"].as_str().unwrap_or(""),
                machine["state"].as_str().unwrap_or("")
            );
            pairs.push(("Made by", made));
        }
        if let Some(profile) = &given {
            let secrets: Vec<String> = profile["secrets"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|secret| {
                    let named = secret["name"].as_str().unwrap_or("");
                    match (secret["env"].as_str(), secret["file"].as_str()) {
                        (Some(env), _) => format!("{named} as {env}"),
                        (None, Some(file)) => match secret["mode"].as_str() {
                            Some(mode) => format!("{named} in {file} ({mode})"),
                            None => format!("{named} in {file}"),
                        },
                        _ => named.to_owned(),
                    }
                })
                .collect();
            pairs.push(("Secrets", secrets.join(", ")));
            let registries: Vec<&str> = profile["registries"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            pairs.push(("Registries", registries.join(", ")));
        }
        out::fields(&pairs);
        if let Some(actions) = &actions {
            println!();
            super::things::print_actions(actions);
        }
        out::hint(format!(
            "Run an action with berth run {name} ACTION. Its profile: berth host export {name}"
        ));
    });
    Ok(())
}

/// The profile the server keeps of a host.
pub fn profile_of(client: &Client, name: &str) -> Result<Value> {
    client.call("hosts.profile", json!({"name": name}), None)
}

/// Save a profile as a host's.
pub fn save(client: &Client, name: &str, profile: &Value) -> Result<()> {
    client.call("hosts.save", json!({"name": name}), Some(profile.clone()))?;
    Ok(())
}

/// The machine a provisioner made for a host, as its resource.
pub fn machine_of(client: &Client, name: &str) -> Result<Option<Value>> {
    let found = client.call(
        "resources.list",
        json!({"kind": "machine", "host": name}),
        None,
    )?;
    Ok(found
        .as_array()
        .and_then(|all| all.iter().find(|made| made["host"] == name).cloned()))
}

fn exists(client: &Client, name: &str) -> Result<bool> {
    match client.call("hosts.get", json!({"name": name}), None) {
        Ok(_) => Ok(true),
        Err(fail) if fail.code == "not_found" => Ok(false),
        Err(fail) => Err(fail),
    }
}

fn add(client: &Client, add: &args::HostAdd) -> Result<()> {
    let host_type = HostType::parse(&add.host_type).ok_or_else(|| {
        Fail::usage(format!(
            "--type is ssh, managed, node or workstation, not {}",
            add.host_type
        ))
    })?;
    let mut access = profile::Access {
        address: add.address.clone(),
        user: add.user.clone().unwrap_or_default(),
        port: add.port.clone().unwrap_or_default(),
        ..profile::Access::default()
    };
    if let Some(auth) = &add.auth {
        access.auth = profile::Auth::parse(auth).ok_or_else(|| {
            Fail::usage(format!(
                "--auth is berth, key:NAME, password or ssh, not {auth}"
            ))
        })?;
    }
    let mut config = profile::minimal(host_type, &access);
    if host_type == HostType::Workstation {
        config["builder"] = json!({});
    }
    if !add.feature.is_empty() {
        let features = super::setup::catalogue(client)?.features;
        for key in &add.feature {
            let setting = berth_tui::flow::Setting::Feature(key.clone(), true);
            berth_tui::flow::apply(&mut config, &features, &setting)
                .map_err(|why| Fail::usage(format!("--feature {key}: {why}")))?;
        }
    }
    let problems = profile::problems(&config);
    if !problems.is_empty() {
        return Err(Fail::usage(problems.join("; ")));
    }
    if exists(client, &add.name)? {
        return Err(Fail::failed(format!(
            "There is a host {} already. Change it with berth host set {} KEY=VALUE",
            add.name, add.name
        )));
    }
    save(client, &add.name, &config)?;
    out::done(format!(
        "Added {} as a host of the type {host_type}",
        add.name
    ));
    if add.no_contact {
        return Ok(());
    }
    // First contact: the key is tried, and a host that refuses it is
    // logged in to once with a password to install it.
    let password = match add.password_stdin {
        true => Some(ask::secret_from(
            "Password",
            None,
            true,
            "--password-stdin",
        )?),
        false => None,
    };
    trust(client, &add.name, None, password).map_err(|fail| {
        fail.context(&format!(
            "{} is saved, but logging in to it did not work yet. Try again with berth key trust {}",
            add.name, add.name
        ))
    })?;
    let host = client.call("hosts.get", json!({"name": add.name}), None)?;
    let seen = host["seen"].as_str().and_then(HostType::parse);
    if let Some(said) = berth_api::type_mismatch(&add.name, host_type, seen) {
        out::warn(said);
    }
    if host_type == HostType::Node {
        out::hint(format!(
            "Its features install with: berth host apply {}",
            add.name
        ));
    }
    Ok(())
}

/// Have a host take a key of the server's: berth's own is tried, and a
/// password or another key logs in once when the host refuses it. The
/// password is asked for when it is needed and not given.
pub fn trust(
    client: &Client,
    name: &str,
    key: Option<&str>,
    password: Option<String>,
) -> Result<()> {
    let mut arguments = BTreeMap::new();
    if let Some(key) = key {
        arguments.insert("key".to_owned(), key.to_owned());
    }
    if let Some(password) = &password {
        arguments.insert("password".to_owned(), password.clone());
    }
    let act = Act {
        arguments: arguments.clone(),
        ..Act::default()
    };
    let title = format!("Logging in to {name}");
    let tried = client.act(&Subject::Host(name.to_owned()), "trust-key", &act)?;
    match follow::acted(client, tried, &title, false, false) {
        Ok(_) => Ok(()),
        // Refused without a password: ask for one, once.
        Err(_) if password.is_none() && key.is_none() && out::get().ask => {
            let password = ask::secret(
                &format!("Password of the SSH user of {name}, used once"),
                "--password-stdin",
            )?;
            arguments.insert("password".into(), password);
            let act = Act {
                arguments,
                ..Act::default()
            };
            let tried = client.act(&Subject::Host(name.to_owned()), "trust-key", &act)?;
            follow::acted(client, tried, &title, false, false).map(|_| ())
        }
        Err(fail) => Err(fail),
    }
}

/// Write `KEY=VALUE` settings into a host's profile and save it.
pub fn set(client: &Client, name: &str, values: &[String]) -> Result<()> {
    let mut profile = profile_of(client, name)?;
    let features = super::setup::catalogue(client)?.features;
    for word in values {
        let setting = berth_tui::flow::setting(word).map_err(Fail::usage)?;
        berth_tui::flow::apply(&mut profile, &features, &setting)
            .map_err(|why| Fail::usage(format!("{word}: {why}")))?;
    }
    save(client, name, &profile)?;
    out::done(format!("Saved {name}"));
    if matches!(
        profile::host_type(&profile),
        HostType::Workstation | HostType::Node
    ) {
        out::hint(format!(
            "It takes effect when it is applied: berth host apply {name}"
        ));
    }
    Ok(())
}

fn edit(client: &Client, name: &str) -> Result<()> {
    let profile = profile_of(client, name)?;
    let dir = tempfile::Builder::new().prefix("berth-edit").tempdir()?;
    let file = dir.path().join(format!("{name}.json"));
    std::fs::write(&file, serde_json::to_string_pretty(&profile)? + "\n")?;
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into());
    let mut words = editor.split_whitespace();
    let program = words.next().unwrap_or("vi");
    let status = std::process::Command::new(program)
        .args(words)
        .arg(&file)
        .status()
        .map_err(|error| Fail::failed(format!("{editor}: {error}")))?;
    if !status.success() {
        return Err(Fail::failed(format!(
            "{editor} ended with {status}; nothing saved"
        )));
    }
    let edited: Value = serde_json::from_slice(&std::fs::read(&file)?).map_err(|error| {
        Fail::usage(format!(
            "The profile is no JSON now: {error}. Nothing saved"
        ))
    })?;
    if edited == profile {
        out::hint("Nothing changed");
        return Ok(());
    }
    save(client, name, &edited)?;
    out::done(format!("Saved {name}"));
    Ok(())
}

/// What a file or standard input holds.
pub fn read_file(file: &str) -> Result<Vec<u8>> {
    match file {
        "-" => {
            let mut bytes = vec![];
            std::io::Read::read_to_end(&mut std::io::stdin(), &mut bytes)?;
            Ok(bytes)
        }
        file => std::fs::read(file).map_err(|error| Fail::usage(format!("{file}: {error}"))),
    }
}

fn import(client: &Client, name: &str, file: &str) -> Result<()> {
    let profile: Value = serde_json::from_slice(&read_file(file)?)
        .map_err(|error| Fail::usage(format!("{file} holds no JSON profile: {error}")))?;
    save(client, name, &profile)?;
    out::done(format!("Saved {name}"));
    Ok(())
}

/// The sudo password `--sudo-password-stdin` gave, read once for the run.
pub fn sudo_from_stdin() -> Result<String> {
    static READ: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    if let Some(read) = READ.get() {
        return Ok(read.clone());
    }
    let read = ask::secret_from("Sudo password", None, true, "--sudo-password-stdin")?;
    Ok(READ.get_or_init(|| read).clone())
}

/// The sudo password an action of a host needs, when its SSH user's sudo
/// asks for one and the server keeps none: from standard input, or asked
/// on the terminal once in a run.
pub fn sudo_password(client: &Client, name: &str) -> Result<Option<String>> {
    static ASKED: std::sync::Mutex<BTreeMap<String, String>> =
        std::sync::Mutex::new(BTreeMap::new());
    if out::get().sudo_stdin {
        return sudo_from_stdin().map(Some);
    }
    if let Some(asked) = ASKED.lock().unwrap().get(name) {
        return Ok(Some(asked.clone()));
    }
    let profile = profile_of(client, name)?;
    if !profile::asks_become_pass(&profile) || profile["access"]["link"].is_array() {
        return Ok(None);
    }
    let kept = client.call("secrets.list", json!({}), None)?;
    let secret = format!("sudo/{name}");
    if kept["names"]
        .as_array()
        .is_some_and(|names| names.iter().any(|known| known == secret.as_str()))
    {
        return Ok(None);
    }
    match out::get().ask {
        true => {
            let asked = ask::secret(
                &format!("Sudo password of the SSH user of {name}"),
                "--sudo-password-stdin",
            )?;
            ASKED.lock().unwrap().insert(name.to_owned(), asked.clone());
            Ok(Some(asked))
        }
        // Without one sudo says so itself, when it does want it.
        false => Ok(None),
    }
}

pub fn apply(client: &Client, apply: &args::HostApply) -> Result<()> {
    let name = &apply.name;
    let profile = profile_of(client, name)?;
    let action = match profile::host_type(&profile) {
        HostType::Ssh => {
            return Err(Fail::refused(format!(
                "{name} is a host of the type ssh, neither a node nor a workstation, and berth installs nothing on it. Make it managed for the controller alone (berth host set {name} type=managed), or a node or a workstation, and apply it"
            )));
        }
        HostType::Managed => "install-controller",
        _ => "apply",
    };
    let act = Act {
        sudo_password: sudo_password(client, name)?,
        rows: Some(24),
        cols: Some(100),
        ..Act::default()
    };
    let acted = client.act(&Subject::Host(name.clone()), action, &act)?;
    let title = match action {
        "apply" => format!("Applying {name}"),
        _ => format!("Installing the controller on {name}"),
    };
    follow::acted(client, acted, &title, apply.detach.detach, false)?;
    Ok(())
}

fn seed(client: &Client, name: &str, file: Option<&std::path::Path>) -> Result<()> {
    let acted = client.act(&Subject::Host(name.to_owned()), "seed", &Act::default())?;
    let seed = acted.result.unwrap_or_default();
    let data = seed["user_data"].as_str().unwrap_or("");
    if out::get().json {
        out::json(&seed);
        return Ok(());
    }
    // Until when its key joins the tailnet, for whoever starts the machine.
    if let Some(note) = seed["tailnet"].as_str().filter(|note| !note.is_empty()) {
        out::note(note);
    }
    match file {
        Some(file) => {
            write_private(file, data.as_bytes())?;
            out::done(format!("Wrote the seed of {name} to {}", file.display()));
            out::hint("It holds secrets of the host; keep it to yourself");
        }
        None => print!("{data}"),
    }
    Ok(())
}

/// Write a file only its owner can read.
fn write_private(file: &std::path::Path, data: &[u8]) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    std::io::Write::write_all(&mut options.open(file)?, data)?;
    Ok(())
}

fn installer(client: &Client, iso: &args::HostIso) -> Result<()> {
    let mut arguments = BTreeMap::new();
    if let Some(device) = &iso.write {
        ask::confirm(&format!(
            "This erases {device} and writes the installer of {} to it.",
            iso.name
        ))?;
        let password = match iso.password_stdin {
            true => ask::secret_from("Sudo password", None, true, "--password-stdin")?,
            false if out::get().ask => ask::secret(
                "Your sudo password here, to write the drive (empty when sudo needs none)",
                "--password-stdin",
            )?,
            false => String::new(),
        };
        arguments.insert("device".to_owned(), device.clone());
        arguments.insert("password".to_owned(), password);
    }
    let act = Act {
        arguments,
        ..Act::default()
    };
    let acted = client.act(&Subject::Host(iso.name.clone()), "installer", &act)?;
    let title = match &iso.write {
        Some(device) => format!(
            "Building the installer of {} and writing it to {device}",
            iso.name
        ),
        None => format!("Building the installer of {}", iso.name),
    };
    let Some(job) = acted.job.as_ref().filter(|_| !iso.detach.detach) else {
        follow::acted(client, acted, &title, iso.detach.detach, false)?;
        return Ok(());
    };
    let ended = follow::job(client, &job.id, &title, false)?;
    // Until when its key joins the tailnet, which the job's output says among its lines.
    for line in &ended.tail {
        if line.contains("joins the tailnet until") {
            out::note(line);
        }
    }
    if out::get().json {
        out::json(&ended.job);
    }
    match ended.code {
        0 => Ok(()),
        _ => Err(Fail::exit(1)),
    }
}

fn rename(client: &Client, name: &str, new: &str, hostname: bool) -> Result<()> {
    let arguments = BTreeMap::from([
        ("name".to_owned(), new.to_owned()),
        (
            "hostname".to_owned(),
            if hostname { "yes" } else { "no" }.to_owned(),
        ),
    ]);
    let act = Act {
        arguments,
        ..Act::default()
    };
    let acted = client.act(&Subject::Host(name.to_owned()), "rename", &act)?;
    follow::acted(
        client,
        acted,
        &format!("Renaming {name} to {new}"),
        false,
        false,
    )?;
    Ok(())
}

fn remove(client: &Client, name: &str, destroy: bool) -> Result<()> {
    let machine = machine_of(client, name)?;
    match (&machine, destroy) {
        (Some(machine), true) => {
            let id = machine["id"].as_str().unwrap_or("");
            ask::confirm(&format!(
                "This destroys {} of {}, the machine of {name}, and forgets {name}.",
                machine["external_id"].as_str().unwrap_or(id),
                machine["provisioner"].as_str().unwrap_or("its provisioner")
            ))?;
            let acted = client.call("resources.remove", json!({"id": id}), None)?;
            let acted: berth_api::Acted = serde_json::from_value(acted["acted"].clone())
                .or_else(|_| serde_json::from_value(acted.clone()))?;
            follow::acted(
                client,
                acted,
                &format!("Destroying the machine of {name}"),
                false,
                false,
            )?;
        }
        (Some(machine), false) => {
            return Err(Fail::refused(format!(
                "{name} has a machine {} made, {}. Destroy it with the host: berth host remove {name} --destroy",
                machine["provisioner"].as_str().unwrap_or("a provisioner"),
                machine["external_id"].as_str().unwrap_or("")
            )));
        }
        (None, true) => {
            return Err(Fail::usage(format!(
                "No provisioner made {name}, so there is nothing to destroy. berth host remove {name} forgets it"
            )));
        }
        (None, false) => ask::confirm(&format!(
            "This forgets {name}, with its profile and passwords. The machine is left as it is."
        ))?,
    }
    client.call("hosts.remove", json!({"name": name}), None)?;
    out::done(format!("Removed {name}"));
    Ok(())
}

fn adopt(client: &Client, adopt: &args::HostAdopt) -> Result<()> {
    let mut arguments = BTreeMap::from([("name".to_owned(), adopt.name.clone())]);
    if let Some(address) = &adopt.address {
        arguments.insert("address".into(), address.clone());
    }
    if let Some(port) = &adopt.port {
        arguments.insert("port".into(), port.clone());
    }
    if let Some(key) = &adopt.key {
        arguments.insert("key".into(), key.clone());
    }
    if adopt.force {
        arguments.insert("force".into(), "yes".into());
    }
    let sudo_password = match out::get().sudo_stdin {
        true => Some(sudo_from_stdin()?),
        false => None,
    };
    let act = Act {
        arguments,
        sudo_password,
        ..Act::default()
    };
    let acted = client.act(&Subject::Installation, "adopt", &act)?;
    follow::acted(
        client,
        acted,
        &format!("Adopting {}", adopt.name),
        adopt.detach.detach,
        false,
    )?;
    Ok(())
}

fn credentials(client: &Client, name: &str, show: bool) -> Result<()> {
    let found = client.call("hosts.credentials", json!({"name": name}), None)?;
    if !out::get().terminal.0 && !show && !out::get().json {
        return Err(Fail::refused(
            "The passwords go to a terminal only. Add --show to print them here",
        ));
    }
    out::data(&found, || {
        let mut table = Table::new(&["account", "password"]);
        for (account, password) in found["credentials"].as_object().into_iter().flatten() {
            table.row(vec![
                account.clone(),
                password.as_str().unwrap_or("").to_owned(),
            ]);
        }
        table.print(&format!("berth generated no passwords for {name}"));
    });
    Ok(())
}

fn verify(client: &Client, name: &str, accept: bool) -> Result<()> {
    if accept {
        ask::confirm(&format!(
            "This forgets the host key recorded for {name} and takes the one it shows now."
        ))?;
    }
    let act = Act {
        arguments: BTreeMap::from([(
            "accept".to_owned(),
            if accept { "yes" } else { "no" }.to_owned(),
        )]),
        ..Act::default()
    };
    let acted = client.act(&Subject::Host(name.to_owned()), "host-key", &act)?;
    let result = acted.result.unwrap_or_default();
    out::data(&result, || {
        let mut table = Table::new(&["type", "fingerprint"]);
        for key in result["host_keys"].as_array().into_iter().flatten() {
            table.row(vec![
                key["type"].as_str().unwrap_or("").to_owned(),
                key["fingerprint"].as_str().unwrap_or("").to_owned(),
            ]);
        }
        table.print(&format!("No host key of {name} is recorded yet"));
    });
    Ok(())
}

fn power(client: &Client, power: &args::HostPower, verb: &str) -> Result<()> {
    let name = &power.name;
    let machine = machine_of(client, name)?.ok_or_else(|| {
        Fail::usage(format!(
            "No provisioner made {name}, so berth cannot {verb} it. berth host create NAME --provisioner P makes one that can"
        ))
    })?;
    let id = machine["id"].as_str().unwrap_or("").to_owned();
    let acted = client.act(&Subject::Resource(id), verb, &Act::default())?;
    let title = match verb {
        "start" => format!("Starting {name}"),
        _ => format!("Stopping {name}"),
    };
    follow::acted(client, acted, &title, power.detach.detach, false)?;
    Ok(())
}
