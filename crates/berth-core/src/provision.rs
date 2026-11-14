//! Creating the machine a saved host's profile describes, with a provisioner
//! of a plugin. berth runs the provisioner where it runs itself, keeps the
//! machine as a resource of the host (see [`crate::resource`]), and from then
//! on the machine is an ordinary host. One machine per profile.
//!
//! A provisioner is run as `COMMAND VERB` in its package directory and in a
//! process group of its own. It reads one JSON object on standard input and
//! answers one on standard output; the lines of its standard error are
//! progress. Secrets travel only on its standard input, and the request is
//! never written anywhere.

use crate::resource::{self, Kept};
use crate::{
    Result, apply,
    host::Host,
    link::{self, Link},
    paths::Paths,
    plugins::{self, Plugin, Provisioner, Setup},
    profiles::Place,
    registry::Store,
    relay, seed,
    server::KEPT,
    store,
};
use berth_api::{NewResource, ProvisionerInfo};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    io::{BufRead, Read, Write},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

use crate::plugins::PROTOCOL;
/// Where a seeded machine says how far it has set itself up.
const PROVISION_STATE: &str = "/var/lib/devbox/provision-state";

/// How long things may take, and how the new machine is waited for. Tests
/// make the times short and stand in for the machine.
pub struct Options {
    /// For `status` and `list`.
    pub quick: Duration,
    pub create: Duration,
    /// For `start`, `stop` and `destroy`.
    pub change: Duration,
    /// How long a seeded machine has until its berthd answers.
    pub boot: Duration,
    /// How long a machine berth applies to has until SSH answers.
    pub ssh: Duration,
    /// How long to wait between asking whether it answers.
    pub every: Duration,
    /// How long berth stays quiet while it waits before it says it still does.
    pub alive: Duration,
    /// Whether SSH lets berth in.
    pub reaches: fn(&Paths, &Host) -> bool,
    /// Whether a seeded machine's berthd answers.
    pub answers: fn(&Paths, &Host) -> bool,
    /// What a seeded machine's first boot has written since a cursor of its
    /// log, read over SSH, as [`relay::FirstBoot::take`] reads it.
    pub first_boot: fn(&Paths, &Host, &str) -> Result<String>,
    /// What is done once it answers: a seeded one's provision state is
    /// told, and the profile is applied to the other.
    pub finish: fn(&Paths, &Host, Setup, &mut dyn Write) -> Result<()>,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            quick: Duration::from_secs(30),
            create: Duration::from_secs(15 * 60),
            change: Duration::from_secs(5 * 60),
            boot: Duration::from_secs(30 * 60),
            ssh: Duration::from_secs(5 * 60),
            every: Duration::from_secs(10),
            alive: Duration::from_secs(60),
            reaches: ssh_answers,
            answers,
            first_boot,
            finish,
        }
    }
}

impl Options {
    pub(crate) fn limit(&self, verb: &str) -> Duration {
        match verb {
            "create" => self.create,
            "status" | "list" => self.quick,
            _ => self.change,
        }
    }
}

pub use crate::runner::Lock;

/// Held while something is done to a host's machine: the lock of the host as
/// a subject of jobs.
pub fn lock(paths: &Paths, host: &str) -> Result<Lock> {
    let path = Place::resolve(paths, host)?.lock_file(paths);
    crate::runner::lock_at(&path, "work on the machine", |holder| {
        format!(
            "Something else is being done to {host}'s machine{holder}. Try again when it has ended"
        )
    })
}

/// Whether something is being done to a host's machine right now.
pub fn busy(paths: &Paths, host: &str) -> bool {
    Place::resolve(paths, host).is_ok_and(|place| place.lock_file(paths).exists())
        && lock(paths, host).is_err()
}

// ── Finding provisioners ────────────────────────────────────────────────────

/// The plugins whose provisioners berth runs here.
pub fn list_plugins(paths: &Paths) -> Vec<Plugin> {
    found(paths)
}

pub(crate) fn found(paths: &Paths) -> Vec<Plugin> {
    let own = plugins::own_dir(&paths.config_dir);
    match paths.source() {
        Ok(source) => plugins::found(&source, &own),
        Err(_) => plugins::load(&[own]),
    }
}

pub(crate) fn find<'a>(plugins: &'a [Plugin], name: &str) -> Result<(&'a Plugin, &'a Provisioner)> {
    let (plugin, id) = name.split_once('/').unwrap_or((name, ""));
    plugins
        .iter()
        .filter(|known| known.id == plugin)
        .find_map(|known| {
            let provisioner = known.provisioners.iter().find(|known| known.id == id)?;
            Some((known, provisioner))
        })
        .ok_or_else(|| format!("No provisioner {name}. See: berth provisioners").into())
}

/// The provisioner a name stands for: a registered entry's type, or the
/// `PLUGIN/ID` itself when no entry has that name.
pub(crate) fn resolve<'a>(
    paths: &Paths,
    plugins: &'a [Plugin],
    name: &str,
) -> Result<(&'a Plugin, &'a Provisioner)> {
    let kind =
        crate::provisioner::entry(paths, name)?.map_or_else(|| name.to_owned(), |entry| entry.kind);
    find(plugins, &kind)
}

/// The value of a secret, when it has a usable one here.
fn secret(store: &Store, name: &str) -> Option<String> {
    store
        .secret(name)
        .ok()
        .flatten()
        .filter(|value| !value.is_empty() && value != KEPT)
}

/// The secrets of the provisioner registered as `name`, under the names its
/// plugin declares, and the required ones without a value. Each entry keeps
/// its own, as `provisioner/NAME/SECRET`.
pub(crate) fn secrets(
    store: &Store,
    name: &str,
    provisioner: &Provisioner,
) -> (Map<String, Value>, Vec<String>) {
    let (mut given, mut missing) = (Map::new(), vec![]);
    for wanted in &provisioner.secrets {
        match secret(store, &format!("provisioner/{name}/{}", wanted.name))
            .or_else(|| secret(store, &wanted.name))
        {
            Some(value) => {
                given.insert(wanted.name.clone(), json!(value));
            }
            None if wanted.required => missing.push(wanted.name.clone()),
            None => {}
        }
    }
    (given, missing)
}

pub(crate) fn needs_secrets(name: &str, missing: &[String]) -> Result<()> {
    match missing {
        [] => Ok(()),
        missing => Err(format!(
            "{name} needs the secret{} {}: berth secret set NAME",
            if missing.len() == 1 { "" } else { "s" },
            missing.join(", ")
        )
        .into()),
    }
}

/// Why berth does not run a provisioner: one of another protocol.
pub(crate) fn runnable(provisioner: &Provisioner) -> Result<()> {
    match &provisioner.refused {
        Some(refused) => Err(refused.clone().into()),
        None => Ok(()),
    }
}

/// The value of each of `declared`: given, else kept in `settings`, else
/// its default. Names it does not declare, required ones without a value,
/// and values outside a few it takes are refused.
pub(crate) fn values_of(
    name: &str,
    declared: &[berth_api::Argument],
    given: &BTreeMap<String, String>,
    settings: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    if let Some(unknown) = given
        .keys()
        .find(|asked| !declared.iter().any(|known| &known.name == *asked))
    {
        let known: Vec<&str> = declared.iter().map(|known| known.name.as_str()).collect();
        return Err(format!(
            "{name} has no argument {unknown}. It takes: {}",
            match known.is_empty() {
                true => "none".to_owned(),
                false => known.join(", "),
            }
        )
        .into());
    }
    let mut values = BTreeMap::new();
    for argument in declared {
        let value = given
            .get(&argument.name)
            .or_else(|| settings.get(&argument.name))
            .cloned()
            .unwrap_or_else(|| argument.default.clone());
        if argument.required && value.is_empty() {
            return Err(format!("{name} needs {}=VALUE", argument.name).into());
        }
        if !value.is_empty() && !argument.values.is_empty() && !argument.values.contains(&value) {
            return Err(format!(
                "{name} takes {}={} only",
                argument.name,
                argument.values.join(" or ")
            )
            .into());
        }
        values.insert(argument.name.clone(), value);
    }
    Ok(values)
}

/// What every verb is sent; one about a resource adds what it is (see
/// [`resource::request`]).
pub(crate) fn request(
    verb: &str,
    provisioner: &Provisioner,
    arguments: &BTreeMap<String, String>,
    secrets: &Map<String, Value>,
) -> Value {
    json!({
        "version": PROTOCOL,
        "verb": verb,
        "provisioner": provisioner.id,
        "arguments": arguments,
        "secrets": secrets,
    })
}

/// The provisioners there are, with the required secrets that are not set,
/// the settings kept for each, and why one cannot be used here.
pub fn list(paths: &Paths, store: &Store) -> Vec<ProvisionerInfo> {
    let plugins = found(paths);
    crate::provisioner::entries(paths)
        .unwrap_or_default()
        .into_iter()
        .map(|(name, entry)| match find(&plugins, &entry.kind) {
            Ok((plugin, provisioner)) => {
                crate::provisioner::describe(paths, store, &name, &plugin.id, provisioner)
            }
            // Its plugin was removed: it stays listed, to add the plugin again or remove it.
            Err(_) => ProvisionerInfo {
                id: name,
                label: entry.kind.clone(),
                problem: format!(
                    "No plugin brings {} any more: add its plugin again",
                    entry.kind
                ),
                kind: entry.kind,
                settings: entry.settings,
                ..ProvisionerInfo::default()
            },
        })
        .collect()
}

/// The types of provisioner the plugins bring, each as `PLUGIN/ID`, which a
/// provisioner is registered as.
pub fn types(paths: &Paths) -> Vec<ProvisionerInfo> {
    let mut types: Vec<ProvisionerInfo> = found(paths)
        .iter()
        .flat_map(|plugin| {
            plugin
                .provisioners
                .iter()
                .map(|provisioner| provisioner.info(&plugin.id, vec![]))
        })
        .collect();
    types.sort_by(|one, other| one.id.cmp(&other.id));
    types
}

// ── Running a provisioner ───────────────────────────────────────────────────

fn say(out: &mut dyn Write, line: &str) {
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// The texts of the secrets a request carries, which are never shown.
pub(crate) fn hidden(secrets: &Map<String, Value>) -> Vec<String> {
    crate::runner::hidden(secrets.values().filter_map(Value::as_str))
}

pub(crate) fn hide(line: String, hidden: &[String]) -> String {
    String::from_utf8_lossy(&crate::runner::mask(line.as_bytes(), hidden)).into_owned()
}

fn words(time: Duration) -> String {
    match time.as_secs() {
        seconds if seconds >= 120 => format!("{} minutes", seconds / 60),
        seconds => format!("{seconds} seconds"),
    }
}

fn kill_group(group: i32) {
    // SAFETY: a signal to a process group, by number.
    unsafe {
        libc::kill(-group, libc::SIGKILL);
    }
}

/// Run one verb of a provisioner: its progress, with secrets hidden, goes
/// to `out`, and its answer is returned. One that runs out of time is
/// killed with everything it started.
pub(crate) fn call(
    plugin: &Path,
    provisioner: &Provisioner,
    verb: &str,
    request: &Value,
    limit: Duration,
    out: &mut dyn Write,
) -> Result<Value> {
    let hidden = hidden(request["secrets"].as_object().unwrap_or(&Map::new()));
    let mut child = Command::new(&provisioner.command)
        .arg(verb)
        .current_dir(plugin)
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("{}: {error}", provisioner.command.display()))?;
    let group = child.id() as i32;
    let mut input = serde_json::to_vec(request)?;
    input.push(b'\n');
    let mut stdin = child.stdin.take().ok_or("no standard input")?;
    // One that does not read its input must not hold this up.
    std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let mut stdout = child.stdout.take().ok_or("no standard output")?;
    let (answered, answer) = mpsc::channel();
    std::thread::spawn(move || {
        let mut data = vec![];
        let _ = stdout.read_to_end(&mut data);
        let _ = answered.send(data);
    });
    let stderr = child.stderr.take().ok_or("no standard error")?;
    let (sender, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + limit;
    let mut last: VecDeque<String> = VecDeque::new();
    let mut show = |line: String, last: &mut VecDeque<String>| {
        let line = hide(line, &hidden);
        say(out, &line);
        if !line.trim().is_empty() {
            last.push_back(line);
            if last.len() > 5 {
                last.pop_front();
            }
        }
    };
    let status = loop {
        match lines.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                show(line, &mut last);
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                std::thread::sleep(Duration::from_millis(50))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            kill_group(group);
            let _ = child.wait();
            return Err(format!(
                "{} {verb} did not end within {} and was stopped",
                provisioner.id,
                words(limit)
            )
            .into());
        }
    };
    // Nothing it started outlives it, which also closes its output.
    kill_group(group);
    while let Ok(line) = lines.recv_timeout(Duration::from_secs(1)) {
        show(line, &mut last);
    }
    if !status.success() {
        let reason = match last.is_empty() {
            true => format!("it ended with {}", status.code().unwrap_or(-1)),
            false => Vec::from(last).join(" / "),
        };
        return Err(format!("{} {verb} failed: {reason}", provisioner.id).into());
    }
    let data = answer
        .recv_timeout(Duration::from_secs(5))
        .unwrap_or_default();
    let text = String::from_utf8_lossy(&data);
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    match serde_json::from_str::<Value>(text.trim()) {
        Ok(answer) if answer.is_object() => Ok(answer),
        _ => Err(format!(
            "{} {verb} did not answer with one JSON object",
            provisioner.id
        )
        .into()),
    }
}

// ── Provisioning ────────────────────────────────────────────────────────────

/// A provisioning that has been checked and holds the host's lock, ready to
/// run. A server checks before it starts a job, and runs it in the job.
pub struct Prepared {
    host: String,
    /// The provisioner's name.
    name: String,
    dir: PathBuf,
    provisioner: Provisioner,
    secrets: Map<String, Value>,
    /// What is never shown of what the machine prints: the provisioner's
    /// secrets and those the seed carries.
    hidden: Vec<String>,
    kept: Kept,
    lock: Option<Lock>,
}

/// Check that `host` can have a machine made by what `asked` names, and take
/// its lock. Refused: a host that has a machine, an unfinished one of another
/// provisioner, an unknown provisioner, unknown or missing arguments and
/// missing secrets, and a host something else is being done to.
pub fn prepare(paths: &Paths, store: &Store, host: &str, asked: &NewResource) -> Result<Prepared> {
    let profile = Host::load(paths, host).map_err(|_| format!("No host {host}"))?;
    let lock = lock(paths, host)?;
    let kept = resource::of_place(paths, &profile.place)?;
    if let Some(kept) = kept
        .as_ref()
        .filter(|kept| !kept.record.external_id.is_empty())
    {
        return Err(format!(
            "{host} has a machine already, {} of {}. See it with: berth resource {}; delete it with: berth resource {} remove",
            kept.record.external_id, kept.record.provisioner, kept.record.id, kept.record.id
        )
        .into());
    }
    if let Some(kept) = kept
        .as_ref()
        .filter(|kept| kept.record.provisioner != asked.provisioner)
    {
        return Err(format!(
            "A machine for {host} was asked of {}, which did not say it made one. Forget it with: berth resource {} remove",
            kept.record.provisioner, kept.record.id
        )
        .into());
    }
    if asked.spec.is_some() {
        return Err("A machine takes no specification".into());
    }
    if !asked.secrets.is_empty() {
        return Err("A machine is given no secrets of its own; its profile names them".into());
    }
    let plugins = found(paths);
    let (plugin, provisioner) = resolve(paths, &plugins, &asked.provisioner)?;
    runnable(provisioner)?;
    if provisioner.machine().is_none() {
        return Err(format!("{} makes no machines for hosts", asked.provisioner).into());
    }
    let settings = crate::provisioner::settings(paths, &asked.provisioner)?;
    let arguments = values_of(
        &asked.provisioner,
        &provisioner.arguments,
        &asked.arguments,
        &settings,
    )?;
    let (secrets, missing) = secrets(store, &asked.provisioner, provisioner);
    needs_secrets(&asked.provisioner, &missing)?;
    if provisioner.setup == Setup::Apply && profile.access().user.is_empty() {
        return Err(format!(
            "{} creates the machine with an account berth reaches over SSH: the profile of {host} needs access.user",
            asked.provisioner
        )
        .into());
    }
    let (scope, side) = profile.place.side();
    // Asked again after a failure, the provisioner finds the machine by its token.
    let kept = match kept {
        Some(mut kept) => {
            kept.record.arguments = arguments;
            kept.record.asked = asked.token.clone().unwrap_or_default();
            kept
        }
        None => Kept {
            scope,
            record: resource::Record {
                id: resource::new_id()?,
                provisioner: asked.provisioner.clone(),
                kind: crate::plugins::MACHINE.into(),
                name: profile.place.title(),
                host: side,
                token: new_token()?,
                asked: asked.token.clone().unwrap_or_default(),
                arguments,
                created: berth_api::now(),
                ..resource::Record::default()
            },
        },
    };
    // Saved before there is a job, so that its answer names the resource.
    kept.save(paths)?;
    Ok(Prepared {
        host: host.to_owned(),
        name: asked.provisioner.clone(),
        dir: plugin.dir.clone(),
        provisioner: provisioner.clone(),
        hidden: hidden(&secrets),
        secrets,
        kept,
        lock: Some(lock),
    })
}

/// Make the host's profile a `cloud` one before the machine is made from it:
/// a created machine has no laptop's physical login or power settings, and
/// the seed is rendered from, or the role applied with, what is saved.
fn cloud_profile(paths: &Paths, host: &str) -> Result<()> {
    let place = Place::resolve(paths, host)?;
    let mut profile = place
        .load(paths)?
        .ok_or_else(|| format!("No host {host}"))?;
    if profile["profile"] != "cloud" && profile.is_object() {
        profile["profile"] = json!("cloud");
        place.save(paths, &profile)?;
    }
    Ok(())
}

pub(crate) fn new_token() -> Result<String> {
    let mut bytes = [0u8; 16];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Create the machine for a saved host and set it up, with the progress to
/// `out`.
pub fn provision(
    paths: &Paths,
    store: &Store,
    host: &str,
    asked: &NewResource,
    options: &Options,
    out: &mut dyn Write,
) -> Result<()> {
    prepare(paths, store, host, asked)?.run(paths, store, options, out)
}

impl Prepared {
    /// The record of the machine, saved already.
    pub fn kept(&self) -> &Kept {
        &self.kept
    }

    /// The host's lock, for a job to hold while this runs in it.
    pub fn take_lock(&mut self) -> Option<Lock> {
        self.lock.take()
    }

    /// The secrets the provisioner is given, which its job never shows.
    pub fn secrets(&self) -> Vec<String> {
        self.secrets
            .values()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    }

    /// Create the machine and wait for it to be the host. A failure after
    /// the machine was created leaves it and its record, and says so.
    pub fn run(
        mut self,
        paths: &Paths,
        store: &Store,
        options: &Options,
        out: &mut dyn Write,
    ) -> Result<()> {
        let (host, name) = (self.host.clone(), self.name.clone());
        say(out, &format!("▸ {host}: creating its machine with {name}"));
        cloud_profile(paths, &host)?;
        let profile = Host::load(paths, &host)?;
        let mut request = resource::request(
            "create",
            &self.provisioner,
            &self.kept.record,
            &self.kept.record.arguments,
            &self.secrets,
        );
        request["token"] = json!(self.kept.record.token);
        match self.provisioner.setup {
            Setup::Seed => {
                say(out, &format!("▸ {host}: rendering the cloud seed"));
                let seed = seed::cloud(paths, store, &host, self.provisioner.archive)?;
                self.hidden.extend(seed_secrets(&seed.user_data));
                // The longest first, so a part does not leave the rest of a whole.
                self.hidden
                    .sort_by_key(|text| std::cmp::Reverse(text.len()));
                let hostname = match profile.config["hostname"].as_str() {
                    Some(hostname) if !hostname.is_empty() => hostname.to_owned(),
                    _ => host.clone(),
                };
                request["seed"] = json!({
                    "user_data": seed.user_data,
                    "meta_data": format!("instance-id: {}\nlocal-hostname: {hostname}\n", self.kept.record.token),
                });
                if let Some(archive) = seed.archive {
                    request["seed"]["archive"] =
                        json!({"path": archive.path, "sha256": archive.sha256});
                }
            }
            Setup::Apply => {
                let mut keys: Vec<String> = crate::keys::operator_keys(paths, &profile.config)?;
                for key in berth_api::profile::account_keys(&profile.config, "operator") {
                    if !keys.contains(&key) {
                        keys.push(key);
                    }
                }
                request["access"] = json!({
                    "user": profile.access().user,
                    "public_keys": keys,
                });
            }
        }
        // Written before the machine is asked for, so a crash leaves the token to ask again with.
        self.kept.save(paths)?;
        let answer = call(
            &self.dir,
            &self.provisioner,
            "create",
            &request,
            options.limit("create"),
            out,
        )?;
        let id = answer["id"].as_str().unwrap_or("").to_owned();
        if id.is_empty() {
            return Err(format!(
                "{name} create said no id. Asking again finds the same machine: berth provision {host} {name}"
            )
            .into());
        }
        self.kept.record.external_id = id.clone();
        self.kept.record.state = "running".into();
        self.kept.record.checked = berth_api::now();
        if let Some(address) = answer["address"].as_str() {
            self.kept.record.address = address.to_owned();
        }
        self.kept.save(paths)?;
        say(out, &format!("✓ {host}: {name} created {id}"));
        let resource = self.kept.record.id.clone();
        let exists = |error: Box<dyn std::error::Error>| -> Box<dyn std::error::Error> {
            format!(
                "{error}. The machine {id} exists and stays: berth resource {resource} remove deletes it"
            )
            .into()
        };
        take_address(paths, &profile.place, &answer, out).map_err(exists)?;
        self.wait(paths, options, out).map_err(exists)
    }

    /// Wait until the machine answers, asking the provisioner about it in
    /// between, and then finish setting it up.
    fn wait(&self, paths: &Paths, options: &Options, out: &mut dyn Write) -> Result<()> {
        let host = Host::load(paths, &self.host)?;
        let name = &self.host;
        let setup = self.provisioner.setup;
        let reachable = host.access().reachable();
        if !reachable && setup == Setup::Seed {
            say(
                out,
                &format!(
                    "! {name} has no address berth knows: it sets itself up on its own. Set access.address to reach it"
                ),
            );
            return Ok(());
        }
        if !reachable {
            return Err(format!("{name} has no address to reach it at: set access.address").into());
        }
        let limit = match setup {
            Setup::Seed => options.boot,
            Setup::Apply => options.ssh,
        };
        say(
            out,
            &format!(
                "▸ {name}: waiting for {} to answer (up to {})",
                match setup {
                    Setup::Seed => "berthd",
                    Setup::Apply => "SSH",
                },
                words(limit)
            ),
        );
        // The machine was made just now, and has a host key nobody has seen:
        // the one recorded for the host was the old machine's.
        let over_ssh = !host.address().is_empty();
        if over_ssh {
            let store = store::shared(paths)?;
            crate::keys::known::forget_host_key(paths, &store, &host.place)?;
        }
        let started = Instant::now();
        let deadline = started + limit;
        let status = resource::request(
            "status",
            &self.provisioner,
            &self.kept.record,
            &self.kept.record.arguments,
            &self.secrets,
        );
        let mut log = resource::request(
            "logs",
            &self.provisioner,
            &self.kept.record,
            &self.kept.record.arguments,
            &self.secrets,
        );
        (log["tail"], log["follow"]) = (json!(CONSOLE_LINES), json!(false));
        let logs = self
            .provisioner
            .kinds
            .iter()
            .find(|kind| kind.id == plugins::MACHINE)
            .is_some_and(|kind| kind.verbs.iter().any(|verb| verb == "logs"));
        let mut out = Noted::new(out);
        let mut seen = Seen::default();
        // A host reached only through its link command has no SSH to wait for.
        let mut in_reach = !over_ssh;
        loop {
            if !in_reach && (options.reaches)(paths, &host) {
                in_reach = true;
                say(&mut out, &format!("✓ {name}: SSH answers"));
            }
            if in_reach && setup == Setup::Apply {
                break;
            }
            if in_reach && over_ssh {
                // A look that fails is tried again the next time round.
                if let Ok(printed) = (options.first_boot)(paths, &host, &seen.boot.cursor) {
                    let said = seen.boot.take(&printed, &self.hidden);
                    relayed(&mut out, said);
                }
                if seen.boot.failed() {
                    say(
                        &mut out,
                        &format!("! {name}: its first boot failed. The end of its log:"),
                    );
                    relayed(&mut out, seen.boot.last_lines());
                    return Err(format!(
                        "{name}'s first boot failed, as the lines above show. It tries again by itself every 10 minutes"
                    )
                    .into());
                }
            }
            if in_reach && (options.answers)(paths, &host) {
                say(&mut out, &format!("✓ {name}: berthd answers"));
                break;
            }
            if Instant::now() >= deadline {
                return Err(format!("{name} did not answer within {}", words(limit)).into());
            }
            match call(
                &self.dir,
                &self.provisioner,
                "status",
                &status,
                options.quick,
                &mut std::io::sink(),
            ) {
                Ok(answer) => {
                    if let Some(state @ ("gone" | "stopped")) = answer["state"].as_str() {
                        return Err(format!("{name}'s machine is {state}").into());
                    }
                    seen.machine(name, &answer, &mut out);
                }
                Err(error) => say(&mut out, &format!("! {error}")),
            }
            // Once SSH answers, the first boot tells more than the console.
            if logs && !in_reach {
                let mut printed = vec![];
                match crate::provisioner::stream(
                    &self.dir,
                    &self.provisioner,
                    &log,
                    &self.hidden,
                    options.quick,
                    &mut printed,
                ) {
                    Ok(()) => {
                        let lines = String::from_utf8_lossy(&printed)
                            .lines()
                            .map(|line| relay::scrub(relay::clean(line), &self.hidden))
                            .filter(|line| !line.is_empty())
                            .collect();
                        relayed(&mut out, relay::capped(seen.console.fresh(lines)));
                    }
                    // Said once: the console is extra, and the wait goes on without it.
                    Err(error) if !seen.log_failed => {
                        seen.log_failed = true;
                        say(&mut out, &format!("! {error}"));
                    }
                    Err(_) => {}
                }
            }
            if out.last.elapsed() >= options.alive {
                say(
                    &mut out,
                    &format!(
                        "… {name}: still waiting, {} so far",
                        words(started.elapsed())
                    ),
                );
            }
            std::thread::sleep(options.every);
        }
        (options.finish)(paths, &host, setup, &mut out)?;
        say(&mut out, &format!("✓ {name} is set up"));
        Ok(())
    }
}

/// How many of the console's latest lines the wait asks for each time.
const CONSOLE_LINES: u32 = 100;

/// What has been seen of a machine while it is waited for.
#[derive(Default)]
struct Seen {
    /// Its state and address, as its provisioner said them last.
    state: (String, String),
    console: relay::Tail,
    boot: relay::FirstBoot,
    log_failed: bool,
}

impl Seen {
    /// Say the machine's state and address when they are not what they were.
    fn machine(&mut self, name: &str, answer: &Value, out: &mut dyn Write) {
        let text = |key: &str| relay::clean(answer[key].as_str().unwrap_or(""));
        let now = (text("state"), text("address"));
        if now.0.is_empty() || now == self.state {
            return;
        }
        let at = match now.1.as_str() {
            "" => String::new(),
            address => format!(" at {address}"),
        };
        say(out, &format!("▸ {name}: its machine is {}{at}", now.0));
        self.state = now;
    }
}

/// Lines of the machine's, set apart from berth's own.
fn relayed(out: &mut dyn Write, lines: Vec<String>) {
    for line in lines {
        say(out, &format!("{}{line}", relay::PREFIX));
    }
}

/// Output that notes when it was last written to.
struct Noted<'a> {
    out: &'a mut dyn Write,
    last: Instant,
}

impl<'a> Noted<'a> {
    fn new(out: &'a mut dyn Write) -> Noted<'a> {
        Noted {
            out,
            last: Instant::now(),
        }
    }
}

impl Write for Noted<'_> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.last = Instant::now();
        self.out.write(data)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}

/// What the seed carries that the machine must never be seen to print: the
/// password hashes, the enrollment key and the login for the release.
fn seed_secrets(user_data: &str) -> Vec<String> {
    let body = user_data.split_once('\n').map_or("", |(_, rest)| rest);
    let Ok(seed) = serde_json::from_str::<Value>(body) else {
        return vec![];
    };
    let mut found: Vec<String> = vec![];
    for user in seed["users"].as_array().into_iter().flatten() {
        found.extend(user["passwd"].as_str().map(str::to_owned));
    }
    let secret = |key: &str| {
        let key = key.to_ascii_lowercase();
        !key.contains("public")
            && [
                "password", "passwd", "secret", "token", "auth_key", "private",
            ]
            .iter()
            .any(|word| key.contains(word))
    };
    for file in seed["write_files"].as_array().into_iter().flatten() {
        let path = file["path"].as_str().unwrap_or("");
        let content = file["content"].as_str().unwrap_or("");
        if path.ends_with("/release-login") {
            // A line of curl configuration: the value is what is between the quotes.
            found.push(content.trim().to_owned());
            if let (Some(start), Some(end)) = (content.find('"'), content.rfind('"'))
                && start < end
            {
                found.push(content[start + 1..end].to_owned());
            }
        }
        if path.ends_with("/deployment.json")
            && let Ok(deployment) = serde_json::from_str::<Value>(content)
        {
            for (key, value) in deployment["ansible_vars"].as_object().into_iter().flatten() {
                if let (true, Some(value)) = (secret(key), value.as_str()) {
                    found.push(value.to_owned());
                }
            }
        }
    }
    // A short value would hide ordinary words, and no real secret is that short.
    found.retain(|text| text.len() >= 8);
    found
}

/// Put an address the provisioner gave into the profile, where it has none.
fn take_address(paths: &Paths, place: &Place, answer: &Value, out: &mut dyn Write) -> Result<()> {
    let address = answer["address"].as_str().unwrap_or("").trim();
    let port = match &answer["port"] {
        Value::Number(port) => port.to_string(),
        Value::String(port) => port.trim().to_owned(),
        _ => String::new(),
    };
    if address.is_empty() && port.is_empty() {
        return Ok(());
    }
    // It becomes a word of an ssh command line.
    let usable = |text: &str| {
        !text.starts_with('-') && !text.contains(|ch: char| ch.is_whitespace() || ch.is_control())
    };
    if !usable(address) || !port.bytes().all(|byte| byte.is_ascii_digit()) {
        say(
            out,
            &format!("! The address {address}:{port} is not one berth uses"),
        );
        return Ok(());
    }
    let mut profile = place
        .load(paths)?
        .ok_or_else(|| format!("No host {}", place.argument()))?;
    let mut access = berth_api::profile::Access::of(&profile);
    let mut changed = false;
    for (field, value) in [
        (&mut access.address, address),
        (&mut access.port, port.as_str()),
    ] {
        if field.is_empty() && !value.is_empty() {
            *field = value.to_owned();
            changed = true;
        }
    }
    // A profile that names no SSH user would be reached as whoever runs berth;
    // the account that holds berth's key on the new machine is the operator.
    let operator = profile["operator_username"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    if changed && access.user.is_empty() && usable(&operator) && !operator.is_empty() {
        access.user = operator;
    }
    if changed {
        // A profile that says nothing of its login logs in with berth's key.
        let sudo = profile["access"]["sudo"].clone();
        access.write(&mut profile);
        if sudo.is_null()
            && let Some(object) = profile["access"].as_object_mut()
        {
            object.remove("sudo");
        }
    }
    if changed {
        place.save(paths, &profile)?;
        say(out, &format!("✓ The profile reaches it at {address}"));
    }
    Ok(())
}

// ── Waiting for the machine ─────────────────────────────────────────────────

/// The sudo password of a host whose SSH user's sudo asks for one, from the
/// environment: nobody is there to ask. Only a machine that is applied rather
/// than seeded can have such a user: the seed gives the operator sudo without
/// a password before SSH answers.
fn password(paths: &Paths, host: &Host) -> Option<String> {
    if !host.asks_password() {
        return None;
    }
    host.kept_sudo_password(paths)
        .or_else(|| std::env::var("BERTH_SUDO_PASSWORD").ok())
}

/// Whether SSH lets berth in. A machine just made has a key nobody has seen;
/// its first one is taken.
fn ssh_answers(paths: &Paths, host: &Host) -> bool {
    let Ok(login) = crate::keys::login_of(paths, host) else {
        return false;
    };
    let mut ssh = login.batch_ssh();
    ssh.arg(login.target())
        .arg("true")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let answered = ssh.status().is_ok_and(|status| status.success());
    if answered && let Ok(store) = store::shared(paths) {
        let _ = crate::keys::known::absorb(paths, &store, &host.place);
    }
    answered
}

/// Whether berthd answers on a seeded host's link. The wait asks only once SSH
/// answers, which has made the machine's key known for the link to use.
fn answers(paths: &Paths, host: &Host) -> bool {
    let (Ok(spec), Ok(runtime)) = (host.link(paths, None), link::runtime()) else {
        return false;
    };
    let link = Link::new(spec);
    // The timer is made inside the runtime, which is what it runs on.
    let asked = runtime.block_on(async {
        tokio::time::timeout(
            Duration::from_secs(20),
            link.get::<berth_api::Version>("/v1/version"),
        )
        .await
    });
    runtime.shutdown_background();
    matches!(asked, Ok(Ok(_)))
}

/// The service that runs a seeded machine's first boot.
const FIRST_BOOT_UNIT: &str = "devbox-provision.service";

/// The shell command that prints the provision state and the first boot's
/// log after `cursor`. The state is readable by anyone and the log by root:
/// the seed gives the operator sudo without a password, and an SSH user
/// without it is shown only the state.
fn first_boot_look(cursor: &str) -> String {
    let after = match relay::valid_cursor(cursor) {
        true => format!(" --after-cursor=\"{cursor}\""),
        false => String::new(),
    };
    // Single quotes are left out: the look is itself put in single quotes.
    let look = format!(
        "printf \"state %s\\n\" \"$(cat {PROVISION_STATE} 2>/dev/null || echo none)\"; \
         journalctl -q -u {FIRST_BOOT_UNIT} -o cat --no-pager --show-cursor{after} 2>/dev/null | tail -n 400"
    );
    format!(
        "q='{look}'; \
         if sudo -n true 2>/dev/null; then sudo -n sh -c \"$q\"; \
         else sh -c \"$q\"; fi"
    )
}

/// What the first boot of a seeded machine has written since `cursor`, read
/// over SSH.
fn first_boot(paths: &Paths, host: &Host, cursor: &str) -> Result<String> {
    let login = crate::keys::login_of(paths, host)?;
    let mut ssh = login.batch_ssh();
    ssh.arg(login.target()).arg(first_boot_look(cursor));
    output_within(ssh, String::new(), Duration::from_secs(40))
}

/// What `command` prints with `input` on its standard input, when it ends
/// well within `limit`. One that takes longer is killed.
fn output_within(mut command: Command, input: String, limit: Duration) -> Result<String> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdin = child.stdin.take().ok_or("no standard input")?;
    std::thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
    });
    let mut stdout = child.stdout.take().ok_or("no standard output")?;
    let (sent, printed) = mpsc::channel();
    std::thread::spawn(move || {
        let mut data = vec![];
        let _ = stdout.read_to_end(&mut data);
        let _ = sent.send(data);
    });
    let Ok(data) = printed.recv_timeout(limit) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("it did not answer in time".into());
    };
    match child.wait()?.success() {
        true => Ok(String::from_utf8_lossy(&data).into_owned()),
        false => Err("it failed".into()),
    }
}

fn finish(paths: &Paths, host: &Host, setup: Setup, out: &mut dyn Write) -> Result<()> {
    match setup {
        Setup::Seed => {
            let link = Link::new(host.link(paths, None)?);
            let runtime = link::runtime()?;
            let body = json!({
                "kind": "process", "user": "root",
                "command": ["cat", PROVISION_STATE], "title": "provision state",
            });
            let started =
                runtime.block_on(link.post::<berth_api::Started>("/v1/sessions?wait=30s", &body));
            let started = started.map_err(|error| error.to_string())?;
            let state = started
                .stdout_b64
                .as_deref()
                .and_then(berth_api::unb64)
                .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_owned())
                .unwrap_or_default();
            let path = format!(
                "/v1/sessions/{}",
                berth_api::client::encode(&started.session.id)
            );
            let _ = runtime.block_on(link.call::<Value>("DELETE", &path, None));
            runtime.shutdown_background();
            say(
                out,
                &format!(
                    "✓ {}: provision state {}",
                    host.name,
                    if state.is_empty() { "unknown" } else { &state }
                ),
            );
            Ok(())
        }
        Setup::Apply => {
            let password = password(paths, host);
            if host.asks_password() && password.is_none() {
                return Err(format!(
                    "{}'s SSH user's sudo asks for a password: set BERTH_SUDO_PASSWORD, or apply it with: berth apply {}",
                    host.name, host.name
                )
                .into());
            }
            say(out, &format!("▸ {}: applying", host.name));
            let mut prepared = apply::prepare(paths, host, password.as_deref())?;
            let (reader, writer) = std::io::pipe()?;
            let copy = writer.try_clone()?;
            let mut child = prepared
                .command
                .stdin(Stdio::null())
                .stdout(writer)
                .stderr(copy)
                .spawn()
                .map_err(|error| format!("ansible-playbook: {error}"))?;
            // The command holds the pipe's other ends; they close when it is dropped.
            drop(prepared.command);
            for line in std::io::BufReader::new(reader).lines() {
                let Ok(line) = line else { break };
                say(out, &line);
            }
            let status = child.wait()?;
            drop(prepared.files);
            match status.success() {
                true => apply::settle(paths, host),
                false => Err(format!(
                    "applying failed (ansible-playbook ended with {})",
                    status.code().unwrap_or(-1)
                )
                .into()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A provisioner that writes down each request in `requests.log` of its
    /// folder and answers from files there: `answer.VERB` on standard
    /// output, `code.VERB` as its exit code, `say.VERB` on standard error.
    /// With `hang.VERB` it starts a child and waits; with `leak` it shows its
    /// whole request on standard error.
    const FAKE: &str = r#"#!/bin/sh
verb="$1"
request=$(cat)
printf '%s %s\n' "$verb" "$request" >> requests.log
[ -f "say.$verb" ] && cat "say.$verb" >&2
[ -f leak ] && printf '%s\n' "$request" >&2
if [ -f "hang.$verb" ]; then sleep 60 & echo $! > hung.pid; wait; fi
[ -f "answer.$verb" ] && cat "answer.$verb"
exit "$(cat "code.$verb" 2>/dev/null || echo 0)"
"#;

    /// A seed builder that writes a seed and an archive, and says where both are.
    const BUILDER: &str = r#"import json, os, sys
from pathlib import Path
args = sys.argv[1:]
config = json.loads(Path(args[args.index('--config') + 1]).read_text())
build = Path(os.environ['DEVBOX_BUILD_DIR'])
out = build / config['hostname'] / 'cloud'
out.mkdir(parents=True, exist_ok=True)
(build / 'devbox-release.tar.gz').write_bytes(b'release')
(out / 'user-data').write_text('#cloud-config\n# delivered=' + os.environ.get('BERTH_RELEASE_DELIVERED', '') + '\n'
                               + '# profile=' + config.get('profile', '') + '\n')
print(build / 'devbox-release.tar.gz')
print(out)
"#;

    struct World {
        _home: tempfile::TempDir,
        paths: Paths,
        /// The fake provisioner's folder.
        plugin: PathBuf,
    }

    impl World {
        fn new() -> World {
            let home = tempfile::tempdir().unwrap();
            let paths = Paths {
                config_dir: home.path().join("config"),
                data_dir: home.path().join("data"),
            };
            paths.prepare().unwrap();
            paths.give_own_key();
            let source = home.path().join("source");
            fs::create_dir_all(source.join("provision/bin")).unwrap();
            fs::write(source.join(crate::paths::layout::BUILDER), BUILDER).unwrap();
            paths.set("source", source.to_str().unwrap()).unwrap();
            let plugin = source.join("plugins/fake");
            fs::create_dir_all(plugin.join("bin")).unwrap();
            fs::write(plugin.join("bin/provision"), FAKE).unwrap();
            fs::set_permissions(
                plugin.join("bin/provision"),
                fs::Permissions::from_mode(0o755),
            )
            .unwrap();
            fs::write(
                plugin.join("package.json"),
                r#"{"name": "berth-plugin-fake", "berth": {"provisioners": [
                    {"id": "vm", "label": "Fake VM", "command": "bin/provision", "protocol": 2,
                     "verbs": ["list"],
                     "secrets": ["api_token", {"name": "extra", "required": false}],
                     "arguments": [{"name": "server", "required": true}],
                     "kinds": [{"id": "machine", "setup": "apply", "verbs": ["start", "dance", "logs"],
                                "arguments": [{"name": "cpus", "default": "4"}]}],
                     "actions": [
                       {"name": "images", "label": "Images", "changes": false,
                        "arguments": [{"name": "filter"}]},
                       {"name": "prune", "confirm": "Prune?",
                        "arguments": [{"name": "password", "secret": true}]}]},
                    {"id": "seeded", "command": "bin/provision", "protocol": 2,
                     "kinds": [{"id": "machine", "setup": "seed", "archive": true, "verbs": ["logs"]}]},
                    {"id": "old", "command": "bin/provision", "setup": "apply"}]}}"#,
            )
            .unwrap();
            let world = World {
                _home: home,
                paths,
                plugin: fs::canonicalize(plugin).unwrap(),
            };
            world.profile(
                "studio",
                json!({"hostname": "studio", "access": {"user": "ops"},
                       "ansible_vars": {"devbox_install_tailscale": false}}),
            );
            world
        }

        fn profile(&self, name: &str, profile: Value) {
            Place::own(name)
                .unwrap()
                .save(&self.paths, &profile)
                .unwrap();
        }

        fn store(&self) -> Store {
            Store::new(&self.paths).unwrap()
        }

        fn file(&self, name: &str, text: &str) {
            fs::write(self.plugin.join(name), text).unwrap();
        }

        fn forget(&self, name: &str) {
            let _ = fs::remove_file(self.plugin.join(name));
        }

        /// The requests the provisioner was sent, as (verb, request).
        fn requests(&self) -> Vec<(String, Value)> {
            fs::read_to_string(self.plugin.join("requests.log"))
                .unwrap_or_default()
                .lines()
                .map(|line| {
                    let (verb, request) = line.split_once(' ').unwrap();
                    (verb.to_owned(), serde_json::from_str(request).unwrap())
                })
                .collect()
        }

        fn provision(&self, name: &str, arguments: &[(&str, &str)]) -> (Result<()>, String) {
            let asked = NewResource {
                provisioner: name.into(),
                arguments: arguments
                    .iter()
                    .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                    .collect(),
                ..NewResource::default()
            };
            let mut out = vec![];
            let done = provision(
                &self.paths,
                &self.store(),
                "studio",
                &asked,
                &quick(),
                &mut out,
            );
            (done, String::from_utf8(out).unwrap())
        }

        fn record(&self) -> Option<resource::Record> {
            resource::of_host(&self.paths, "studio")
                .unwrap()
                .map(|kept| kept.record)
        }

        fn machine(&self) -> String {
            self.record().unwrap().id
        }

        /// Run an action of the studio's machine.
        fn act(&self, action: &str) -> Result<crate::provisioner::Answered> {
            resource::prepare_action(
                &self.paths,
                &self.store(),
                &self.machine(),
                action,
                &resource::Given::default(),
            )?
            .run(&self.paths, &quick(), &mut vec![])
        }
    }

    fn quick() -> Options {
        Options {
            quick: Duration::from_secs(5),
            create: Duration::from_secs(5),
            change: Duration::from_secs(5),
            boot: Duration::from_millis(500),
            ssh: Duration::from_millis(500),
            every: Duration::from_millis(10),
            alive: Duration::from_secs(60),
            reaches: |_, _| true,
            answers: |_, _| true,
            first_boot: |_, _, _| Ok(String::new()),
            finish: |_, host, _, out| {
                say(
                    out,
                    &format!(
                        "finished {} as {}",
                        host.address(),
                        host.config["profile"].as_str().unwrap_or("")
                    ),
                );
                Ok(())
            },
        }
    }

    /// Register the fake plugin's provisioner `kind` as `name`.
    fn register(world: &World, name: &str, kind: &str) {
        crate::provisioner::register(
            &world.paths,
            name,
            &berth_api::SavedProvisioner {
                kind: kind.into(),
                settings: None,
            },
        )
        .unwrap();
    }

    fn with_secret(world: &World) {
        world
            .store()
            .set_secret("api_token", Some("tok-s3cret-value"))
            .unwrap();
    }

    #[test]
    fn a_failed_create_is_asked_again_with_its_token_and_a_second_machine_refused() {
        let world = World::new();
        with_secret(&world);
        world.file("code.create", "3");
        world.file("say.create", "quota exceeded\n");
        let (done, _) = world.provision("fake/vm", &[("server", "https://192.0.2.10:8443")]);
        let said = done.unwrap_err().to_string();
        assert!(said.contains("quota exceeded"), "{said}");
        let kept = world.record().unwrap();
        assert!(kept.external_id.is_empty() && kept.token.len() == 32);
        assert_eq!(
            (kept.kind.as_str(), kept.host.as_str()),
            ("machine", "studio")
        );
        // The record is a resource in the store, and no file is beside anything.
        let store = store::shared(&world.paths).unwrap();
        assert_eq!(
            store.names(store::kind::RESOURCE, store::OWN).unwrap(),
            std::slice::from_ref(&kept.id)
        );
        assert!(!world.paths.config_dir.join("hosts").exists());
        // Another provisioner cannot take over while the first may have made one.
        let said = world
            .provision("fake/seeded", &[])
            .0
            .unwrap_err()
            .to_string();
        assert!(
            said.contains(&format!("berth resource {} remove", kept.id)),
            "{said}"
        );

        world.forget("code.create");
        world.file(
            "answer.create",
            r#"{"id": "vm-1", "address": "192.0.2.20", "port": 2222}"#,
        );
        let (done, out) = world.provision("fake/vm", &[("server", "https://192.0.2.10:8443")]);
        done.unwrap();
        // It is applied as a cloud host.
        assert!(out.contains("finished 192.0.2.20 as cloud"), "{out}");
        let creates: Vec<Value> = world
            .requests()
            .into_iter()
            .filter(|(verb, _)| verb == "create")
            .map(|(_, request)| request)
            .collect();
        assert_eq!(creates.len(), 2);
        assert_eq!(creates[0]["token"], creates[1]["token"]);
        assert_eq!(creates[1]["token"], json!(kept.token));
        let create = &creates[1];
        assert_eq!(create["version"], 2);
        assert_eq!(
            (create["kind"].as_str(), create["name"].as_str()),
            (Some("machine"), Some("studio"))
        );
        assert_eq!(create["host"], "studio");
        assert_eq!(create["provisioner"], "vm");
        assert_eq!(
            create["arguments"],
            json!({"server": "https://192.0.2.10:8443", "cpus": "4"})
        );
        assert_eq!(create["secrets"], json!({"api_token": "tok-s3cret-value"}));
        assert_eq!(
            create["labels"],
            json!({"berth-host": "studio", "berth-instance": kept.token})
        );
        assert_eq!(create["access"]["user"], "ops");
        assert!(create.get("seed").is_none());
        assert_eq!(world.record().unwrap().external_id, "vm-1");
        assert_eq!(world.record().unwrap().id, kept.id);
        let host = Host::load(&world.paths, "studio").unwrap();
        assert_eq!(host.address(), "192.0.2.20");
        assert_eq!(host.config["access"]["port"], "2222");

        let said = world
            .provision("fake/vm", &[("server", "x")])
            .0
            .unwrap_err()
            .to_string();
        assert!(said.contains("has a machine already"), "{said}");
    }

    #[test]
    fn what_is_asked_for_is_checked_before_anything_runs() {
        let world = World::new();
        let refused = |name: &str, arguments: &[(&str, &str)]| {
            world.provision(name, arguments).0.unwrap_err().to_string()
        };
        assert!(refused("fake/none", &[]).contains("No provisioner fake/none"));
        // One of the first protocol says what to change.
        let said = refused("fake/old", &[]);
        assert!(
            said.contains("version 1") && said.contains("\"protocol\": 2"),
            "{said}"
        );
        assert!(refused("fake/vm", &[("server", "x")]).contains("api_token"));
        // Only registered ones are listed, by their names.
        assert!(list(&world.paths, &world.store()).is_empty());
        register(&world, "vm", "fake/vm");
        let listed = list(&world.paths, &world.store());
        assert_eq!(
            (listed[0].id.as_str(), listed[0].kind.as_str()),
            ("vm", "fake/vm")
        );
        assert_eq!(listed[0].missing_secrets, ["api_token"]);
        with_secret(&world);
        assert!(
            list(&world.paths, &world.store())[0]
                .missing_secrets
                .is_empty()
        );
        assert!(refused("fake/vm", &[]).contains("needs server=VALUE"));
        assert!(
            refused("fake/vm", &[("server", "x"), ("colour", "red")])
                .contains("no argument colour")
        );
        // berth applies to it over SSH, as a user the profile names.
        world.profile("studio", json!({"builder": {}}));
        assert!(refused("fake/vm", &[("server", "x")]).contains("access.user"));
        world.profile("other", json!({}));
        let said = world.provision("fake/vm", &[]).0;
        assert!(said.is_err());
        assert!(world.requests().is_empty());
        assert!(world.record().is_none());
    }

    #[test]
    fn one_thing_at_a_time_is_done_to_a_host() {
        let world = World::new();
        with_secret(&world);
        let asked = NewResource {
            provisioner: "fake/vm".into(),
            arguments: [("server".to_owned(), "x".to_owned())].into(),
            ..NewResource::default()
        };
        let first = prepare(&world.paths, &world.store(), "studio", &asked).unwrap();
        let said = prepare(&world.paths, &world.store(), "studio", &asked)
            .err()
            .unwrap();
        assert!(said.to_string().contains("Something else"), "{said}");
        assert!(busy(&world.paths, "studio"));
        drop(first);
        assert!(!busy(&world.paths, "studio"));
        assert!(prepare(&world.paths, &world.store(), "studio", &asked).is_ok());
    }

    #[test]
    fn a_provisioner_that_takes_too_long_is_killed_with_what_it_started() {
        let world = World::new();
        with_secret(&world);
        world.file("hang.create", "");
        let asked = NewResource {
            provisioner: "fake/vm".into(),
            arguments: [("server".to_owned(), "x".to_owned())].into(),
            ..NewResource::default()
        };
        let options = Options {
            create: Duration::from_millis(500),
            ..quick()
        };
        let started = Instant::now();
        let said = provision(
            &world.paths,
            &world.store(),
            "studio",
            &asked,
            &options,
            &mut vec![],
        )
        .unwrap_err()
        .to_string();
        assert!(said.contains("did not end within"), "{said}");
        assert!(started.elapsed() < Duration::from_secs(10));
        let child: i32 = fs::read_to_string(world.plugin.join("hung.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut alive = true;
        for _ in 0..100 {
            // SAFETY: signal 0 only asks whether the process is there.
            alive = unsafe { libc::kill(child, 0) } == 0;
            if !alive {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!alive, "the provisioner's child {child} is still running");
    }

    #[test]
    fn secrets_go_on_standard_input_and_never_into_the_output() {
        let world = World::new();
        world
            .store()
            .set_secret("api_token", Some("tok-s3cret-value\nsecond-line-of-it"))
            .unwrap();
        world
            .store()
            .set_secret("extra", Some("optional-one"))
            .unwrap();
        world.file("leak", "");
        world.file("say.create", "using tok-s3cret-value\n");
        world.file("answer.create", r#"{"id": "vm-1"}"#);
        world.profile(
            "studio",
            json!({"access": {"user": "ops", "address": "192.0.2.30"}}),
        );
        let (done, out) = world.provision("fake/vm", &[("server", "x")]);
        done.unwrap();
        assert!(out.contains("[secret]"), "{out}");
        for secret in ["tok-s3cret-value", "second-line-of-it", "optional-one"] {
            assert!(!out.contains(secret), "{secret} in {out}");
        }
        let (_, create) = &world.requests()[0];
        assert_eq!(create["secrets"]["extra"], "optional-one");
        // The profile's own address is kept.
        assert_eq!(
            Host::load(&world.paths, "studio").unwrap().address(),
            "192.0.2.30"
        );
    }

    #[test]
    fn removing_uses_the_kept_id_takes_gone_for_done_and_keeps_the_profile() {
        let world = World::new();
        with_secret(&world);
        world.file(
            "answer.create",
            r#"{"id": "vm-1", "address": "192.0.2.20"}"#,
        );
        world.provision("fake/vm", &[("server", "x")]).0.unwrap();
        // It fails, but says the machine is gone: what was asked for.
        world.file("code.destroy", "1");
        world.file("answer.status", r#"{"state": "gone"}"#);
        let said = world.act(resource::REMOVE).unwrap().message;
        assert!(said.contains("tailnet"), "{said}");
        assert!(world.record().is_none());
        assert!(Host::load(&world.paths, "studio").is_ok());
        let (verb, request) = world
            .requests()
            .into_iter()
            .find(|(verb, _)| verb == "destroy")
            .unwrap();
        assert_eq!(
            (verb.as_str(), request["id"].as_str()),
            ("destroy", Some("vm-1"))
        );

        // One that fails and is not gone stays.
        world.forget("requests.log");
        world.file("answer.create", r#"{"id": "vm-2"}"#);
        world.profile(
            "studio",
            json!({"access": {"user": "ops", "address": "192.0.2.20"}}),
        );
        world.provision("fake/vm", &[("server", "x")]).0.unwrap();
        world.file("answer.status", r#"{"state": "running"}"#);
        world.file("say.destroy", "permission denied\n");
        let said = world.act(resource::REMOVE).err().unwrap().to_string();
        assert!(said.contains("permission denied"), "{said}");
        assert_eq!(world.record().unwrap().external_id, "vm-2");

        // A record without an id is only forgotten.
        let mut kept = resource::of_host(&world.paths, "studio").unwrap().unwrap();
        kept.record.external_id.clear();
        kept.save(&world.paths).unwrap();
        world.forget("requests.log");
        world.act(resource::REMOVE).unwrap();
        assert!(world.record().is_none() && world.requests().is_empty());
    }

    #[test]
    fn power_and_status_follow_what_the_provisioner_answers() {
        let world = World::new();
        with_secret(&world);
        assert!(world.record().is_none());
        world.file(
            "answer.create",
            r#"{"id": "vm-1", "address": "192.0.2.20"}"#,
        );
        world.provision("fake/vm", &[("server", "x")]).0.unwrap();
        world.act("start").unwrap();
        // The kind declares start and not stop, and the manifest's unknown verb is none.
        let said = world.act("stop").err().unwrap().to_string();
        assert!(said.contains("has no action stop"), "{said}");
        assert!(world.act("dance").is_err());
        let verbs: Vec<String> = world.requests().into_iter().map(|(verb, _)| verb).collect();
        assert_eq!(verbs, ["create", "start"]);

        world.file(
            "answer.status",
            r#"{"state": "stopped", "address": "192.0.2.21"}"#,
        );
        let id = world.machine();
        let seen = resource::status(&world.paths, &world.store(), &id, &quick()).unwrap();
        assert_eq!(
            (
                seen.state.as_str(),
                seen.external_id.as_str(),
                seen.address.as_str()
            ),
            ("stopped", "vm-1", "192.0.2.21")
        );
        assert_eq!(seen.verbs, ["start", "logs"]);
        // What it said is kept, and listing asks nothing.
        world.forget("requests.log");
        let listed = resource::listed(&world.paths, Some("fake/vm"), None, Some("studio")).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].state, "stopped");
        assert!(world.requests().is_empty());
        // One that is gone is missing, and its record stays.
        world.file("answer.status", r#"{"state": "gone"}"#);
        let seen = resource::status(&world.paths, &world.store(), &id, &quick()).unwrap();
        assert_eq!(seen.state, "missing");
        assert!(world.record().is_some());
        world.file("code.status", "2");
        let seen = resource::status(&world.paths, &world.store(), &id, &quick()).unwrap();
        assert_eq!(seen.state, "unknown");
        assert!(!seen.problem.is_empty());
        world.forget("code.status");

        world.file(
            "answer.list",
            r#"{"resources": [{"id": "vm-1", "kind": "machine", "name": "studio", "state": "running"},
                {"name": "no id"}, {"id": "wiki-1", "kind": "deployment", "name": "wiki"}]}"#,
        );
        let listed = resource::listed_of(
            &serde_json::from_str(&fs::read_to_string(world.plugin.join("answer.list")).unwrap())
                .unwrap(),
        );
        let names: Vec<(&str, &str, &str)> = listed
            .iter()
            .map(|entry| (entry.id.as_str(), entry.kind.as_str(), entry.host.as_str()))
            .collect();
        assert_eq!(
            names,
            [("vm-1", "machine", "studio"), ("wiki-1", "deployment", "")]
        );
    }

    #[test]
    fn a_seeded_machine_gets_the_seed_and_the_archive_and_is_given_up_when_gone() {
        let world = World::new();
        world.file(
            "answer.create",
            r#"{"id": "seeded-1", "address": "192.0.2.40"}"#,
        );
        world.file("answer.status", r#"{"state": "gone"}"#);
        let options = Options {
            reaches: |_, _| false,
            answers: |_, _| false,
            ..quick()
        };
        let asked = NewResource {
            provisioner: "fake/seeded".into(),
            ..NewResource::default()
        };
        let said = provision(
            &world.paths,
            &world.store(),
            "studio",
            &asked,
            &options,
            &mut vec![],
        )
        .unwrap_err()
        .to_string();
        assert!(
            said.contains("gone") && said.contains("berth resource r-") && said.contains(" remove"),
            "{said}"
        );
        assert_eq!(world.record().unwrap().external_id, "seeded-1");
        let (_, create) = world.requests().into_iter().next().unwrap();
        let token = world.record().unwrap().token;
        // A created machine is a cloud host, whatever the profile said before.
        assert_eq!(
            create["seed"]["user_data"],
            "#cloud-config\n# delivered=1\n# profile=cloud\n"
        );
        let host = Host::load(&world.paths, "studio").unwrap();
        assert_eq!(host.config["profile"], "cloud");
        assert_eq!(
            create["seed"]["meta_data"],
            format!("instance-id: {token}\nlocal-hostname: studio\n")
        );
        let archive = PathBuf::from(create["seed"]["archive"]["path"].as_str().unwrap());
        assert!(archive.is_absolute() && fs::read(&archive).unwrap() == b"release");
        assert_eq!(
            create["seed"]["archive"]["sha256"],
            "a4d451ec23463726f72c43d64c710968f6b602cd653b4de8adee1b556240a829"
        );
        assert!(create.get("access").is_none());
    }

    std::thread_local! {
        /// How often each stand-in for the machine was asked, on this test's thread.
        static ASKED: std::cell::RefCell<BTreeMap<&'static str, usize>> = Default::default();
        /// The cursors the first boot was looked at from.
        static CURSORS: std::cell::RefCell<Vec<String>> = Default::default();
    }

    /// How many times `what` was asked before, counting this one.
    fn asked(what: &'static str) -> usize {
        ASKED.with(|asked| {
            let mut asked = asked.borrow_mut();
            let count = asked.entry(what).or_default();
            *count += 1;
            *count
        })
    }

    fn seeded(world: &World, options: &Options) -> (Result<()>, String) {
        let asked = NewResource {
            provisioner: "fake/seeded".into(),
            ..NewResource::default()
        };
        let mut out = vec![];
        let done = provision(
            &world.paths,
            &world.store(),
            "studio",
            &asked,
            options,
            &mut out,
        );
        (done, String::from_utf8(out).unwrap())
    }

    #[test]
    fn the_wait_tells_each_stage_the_console_and_the_first_boot_once() {
        let world = World::new();
        world.file(
            "answer.create",
            r#"{"id": "seeded-1", "address": "192.0.2.40"}"#,
        );
        world.file(
            "answer.status",
            r#"{"state": "running", "address": "192.0.2.40"}"#,
        );
        world.file(
            "answer.logs",
            "[  OK  ] Started Journal Service.\n\u{1b}[32mcloud-init\u{1b}[0m: key tskey-auth-k1 taken\n  \n",
        );
        let options = Options {
            boot: Duration::from_secs(30),
            reaches: |_, _| asked("ssh") > 3,
            answers: |_, _| asked("berthd") > 2,
            first_boot: |_, _, cursor| {
                CURSORS.with(|cursors| cursors.borrow_mut().push(cursor.to_owned()));
                Ok(match asked("first boot") {
                    1 => "state provisioning\n==> Installing Ansible\n-- cursor: s=1;i=1\n",
                    2 => "state provisioning\n==> Applying the role\nTASK [devbox : Check the profile] *\nTASK [devbox : Install Docker] *\n-- cursor: s=1;i=2\n",
                    _ => "state configured-needs-enrollment\n==> The role is applied\n-- cursor: s=1;i=3\n",
                }
                .to_owned())
            },
            ..quick()
        };
        let (done, out) = seeded(&world, &options);
        done.unwrap();
        let lines: Vec<&str> = out.lines().collect();
        let start = lines
            .iter()
            .position(|line| line.contains("waiting for berthd"))
            .unwrap();
        assert_eq!(
            lines[start + 1..],
            [
                "▸ studio: its machine is running at 192.0.2.40",
                "  │ [  OK  ] Started Journal Service.",
                "  │ cloud-init: key [secret] taken",
                "✓ studio: SSH answers",
                "  │ first boot: provisioning",
                "  │ Installing Ansible",
                "  │ Applying the role",
                "  │ Ansible task 2: Install Docker",
                "  │ first boot: configured-needs-enrollment",
                "  │ The role is applied",
                "✓ studio: berthd answers",
                "finished 192.0.2.40 as cloud",
                "✓ studio is set up",
            ],
            "{out}"
        );
        // Each look goes on from where the last one ended.
        assert_eq!(
            CURSORS.with(|cursors| cursors.borrow().clone()),
            ["", "s=1;i=1", "s=1;i=2"]
        );
        // The console is not asked for once SSH answers, and only for its latest lines.
        let logs: Vec<Value> = world
            .requests()
            .into_iter()
            .filter(|(verb, _)| verb == "logs")
            .map(|(_, request)| request)
            .collect();
        assert_eq!(logs.len(), 3);
        assert_eq!(
            (&logs[0]["id"], &logs[0]["tail"], &logs[0]["follow"]),
            (&json!("seeded-1"), &json!(CONSOLE_LINES), &json!(false))
        );
    }

    #[test]
    fn a_first_boot_that_failed_ends_the_wait_with_the_end_of_its_log() {
        let world = World::new();
        world.file(
            "answer.create",
            r#"{"id": "seeded-1", "address": "192.0.2.40"}"#,
        );
        world.file("answer.status", r#"{"state": "running"}"#);
        let options = Options {
            boot: Duration::from_secs(120),
            answers: |_, _| false,
            first_boot: |_, _, _| {
                Ok("state failed\n==> Applying the role\nTASK [devbox : Install Docker] *\nfatal: [localhost]: FAILED! => {\"msg\": \"No package docker-ce\"}\n".into())
            },
            ..quick()
        };
        let started = Instant::now();
        let (done, out) = seeded(&world, &options);
        let said = done.unwrap_err().to_string();
        assert!(started.elapsed() < Duration::from_secs(20));
        assert!(said.contains("studio's first boot failed"), "{said}");
        assert!(
            said.contains("berth resource r-") && said.contains(" remove"),
            "{said}"
        );
        let end = out
            .split_once("! studio: its first boot failed. The end of its log:\n")
            .unwrap()
            .1;
        assert_eq!(
            end,
            "  │ ==> Applying the role\n  │ TASK [devbox : Install Docker] *\n  │ fatal: [localhost]: FAILED! => {\"msg\": \"No package docker-ce\"}\n"
        );
    }

    #[test]
    fn a_long_silence_is_broken_by_how_long_it_has_waited() {
        let world = World::new();
        with_secret(&world);
        world.file(
            "answer.create",
            r#"{"id": "vm-1", "address": "192.0.2.20"}"#,
        );
        world.file("answer.status", r#"{"state": "running"}"#);
        let options = Options {
            ssh: Duration::from_millis(400),
            alive: Duration::from_millis(100),
            every: Duration::from_millis(20),
            reaches: |_, _| false,
            ..quick()
        };
        let asked = NewResource {
            provisioner: "fake/vm".into(),
            arguments: [("server".to_owned(), "x".to_owned())].into(),
            ..NewResource::default()
        };
        let mut out = vec![];
        let said = provision(
            &world.paths,
            &world.store(),
            "studio",
            &asked,
            &options,
            &mut out,
        )
        .unwrap_err()
        .to_string();
        let out = String::from_utf8(out).unwrap();
        assert!(said.contains("did not answer within"), "{said}");
        let alive = out
            .lines()
            .filter(|line| line.starts_with("… studio: still waiting, "))
            .count();
        assert!((1..=6).contains(&alive), "{out}");
    }

    #[test]
    fn the_secrets_a_seed_carries_are_found_in_it() {
        let deployment = json!({"profile": "cloud", "ansible_vars": {
            "devbox_tailscale_auth_key": "tskey-auth-kAbc123-xyz",
            "devbox_operator_password_hash": "$6$salt$hashhashhash",
            "devbox_operator_ssh_public_key": "ssh-ed25519 AAAAexample",
            "devbox_agent_passwordless_sudo": true,
            "devbox_hostname": "studio",
        }});
        let seed = json!({
            "users": ["default", {"name": "ops", "passwd": "$6$salt$hashhashhash"}],
            "write_files": [
                {"path": "/opt/devbox-bootstrap/deployment.json", "content": deployment.to_string()},
                {"path": "/opt/devbox-bootstrap/release-login",
                 "content": "user = \"reader:registry-password\"\n"},
            ],
        });
        let mut found = seed_secrets(&format!("#cloud-config\n{seed:#}\n"));
        found.sort();
        found.dedup();
        assert_eq!(
            found,
            [
                "$6$salt$hashhashhash",
                "reader:registry-password",
                "tskey-auth-kAbc123-xyz",
                "user = \"reader:registry-password\"",
            ]
        );
        assert!(seed_secrets("#cloud-config\n# not JSON\n").is_empty());
    }

    #[test]
    fn the_look_at_the_first_boot_carries_no_password_and_a_checked_cursor() {
        let look = first_boot_look("s=ab;i=1");
        assert!(look.contains("--after-cursor=\"s=ab;i=1\""), "{look}");
        // There is no password to read, nor a sudo that would take one.
        assert!(
            !look.contains("read") && !look.contains("sudo -S"),
            "{look}"
        );
        // A cursor that is not one is left out rather than run.
        assert!(!first_boot_look("x\"; reboot; \"").contains("after-cursor"));
        // The look runs in a shell here as it would on the machine, with a
        // sudo that refuses and no journal.
        let bin = tempfile::tempdir().unwrap();
        for tool in ["sh", "cat", "tail"] {
            let found = Command::new("sh")
                .args(["-c", &format!("command -v {tool}")])
                .output()
                .unwrap();
            let path = String::from_utf8(found.stdout).unwrap();
            std::os::unix::fs::symlink(path.trim(), bin.path().join(tool)).unwrap();
        }
        fs::write(bin.path().join("sudo"), "#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(bin.path().join("sudo"), fs::Permissions::from_mode(0o755)).unwrap();
        let mut sh = Command::new(bin.path().join("sh"));
        sh.arg("-c")
            .arg(first_boot_look("s=ab;i=1"))
            .env("PATH", bin.path());
        let printed = output_within(sh, String::new(), Duration::from_secs(10)).unwrap();
        assert_eq!(printed, "state none\n");
    }

    #[test]
    fn the_address_comes_with_the_operator_as_the_ssh_user() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: home.path().join("config"),
            data_dir: home.path().join("data"),
        };
        for place in [
            Place::File(home.path().join("studio.json")),
            Place::own("studio").unwrap(),
        ] {
            let taken = |profile: Value| {
                place.save(&paths, &profile).unwrap();
                let answer = json!({"address": "192.0.2.40"});
                take_address(&paths, &place, &answer, &mut Vec::new()).unwrap();
                place.load(&paths).unwrap().unwrap()["access"].clone()
            };
            assert_eq!(
                taken(json!({"operator_username": "ops"})),
                json!({"address": "192.0.2.40", "user": "ops", "auth": "berth"})
            );
            // A user the profile names is kept.
            assert_eq!(
                taken(json!({"operator_username": "ops", "access": {"user": "admin"}})),
                json!({"address": "192.0.2.40", "user": "admin", "auth": "berth"})
            );
        }
    }

    #[test]
    fn two_lockers_of_one_host_and_hosts_apart() {
        let world = World::new();
        world.profile("attic", json!({}));
        let first = lock(&world.paths, "studio").unwrap();
        assert!(lock(&world.paths, "studio").is_err());
        assert!(busy(&world.paths, "studio"));
        // Another host, and a profile in a file, have locks of their own.
        let other = lock(&world.paths, "attic").unwrap();
        let file = world.plugin.join("studio.json");
        let by_path = lock(&world.paths, file.to_str().unwrap()).unwrap();
        assert!(busy(&world.paths, "attic") && busy(&world.paths, file.to_str().unwrap()));
        drop(first);
        assert!(!busy(&world.paths, "studio"));
        assert!(lock(&world.paths, "studio").is_ok());
        drop((other, by_path));
        assert!(!busy(&world.paths, "attic"));
        // A machine of a profile in a file is found by its path.
        Kept {
            scope: crate::profiles::FILE_SCOPE.into(),
            record: resource::Record {
                id: "r-file".into(),
                provisioner: "fake/vm".into(),
                kind: "machine".into(),
                name: "studio".into(),
                host: file.to_str().unwrap().into(),
                token: "t".into(),
                ..Default::default()
            },
        }
        .save(&world.paths)
        .unwrap();
        assert!(world.record().is_none());
        let found = resource::of_host(&world.paths, file.to_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(found.record.token, "t");
        assert_eq!(found.title(), "studio's machine");
    }

    #[test]
    fn an_argument_of_a_few_values_takes_only_those() {
        let declared = [berth_api::Argument {
            name: "keep_volume".into(),
            default: "no".into(),
            values: vec!["no".into(), "yes".into()],
            ..Default::default()
        }];
        let given = |value: &str| BTreeMap::from([("keep_volume".to_owned(), value.to_owned())]);
        let none = BTreeMap::new();
        let of = |given: &BTreeMap<String, String>, kept: &BTreeMap<String, String>| {
            values_of("lab/box", &declared, given, kept)
        };
        assert_eq!(of(&none, &none).unwrap()["keep_volume"], "no");
        assert_eq!(of(&given("yes"), &none).unwrap()["keep_volume"], "yes");
        // A kept setting comes before the default, and what is given before both.
        assert_eq!(of(&none, &given("yes")).unwrap()["keep_volume"], "yes");
        let kept = given("yes");
        assert_eq!(of(&given("no"), &kept).unwrap()["keep_volume"], "no");
        let said = of(&given("maybe"), &none).unwrap_err().to_string();
        assert_eq!(said, "lab/box takes keep_volume=no or yes only");
    }

    #[test]
    fn a_kept_setting_is_where_a_machine_is_made_unless_another_is_given() {
        let world = World::new();
        with_secret(&world);
        world.file(
            "answer.create",
            r#"{"id": "vm-1", "address": "192.0.2.20"}"#,
        );
        register(&world, "lab", "fake/vm");
        // A secret of its own wins over the one all share.
        world
            .store()
            .set_secret("provisioner/lab/api_token", Some("tok-lab"))
            .unwrap();
        crate::provisioner::save_settings(
            &world.paths,
            "lab",
            &BTreeMap::from([("server".to_owned(), "https://192.0.2.10:8443".to_owned())]),
        )
        .unwrap();
        world.provision("lab", &[]).0.unwrap();
        let (_, create) = world.requests().pop().unwrap();
        assert_eq!(create["arguments"]["server"], "https://192.0.2.10:8443");
        assert_eq!(create["secrets"]["api_token"], "tok-lab");
        assert_eq!(world.record().unwrap().provisioner, "lab");
        // The record keeps where it was made, whatever the setting says later.
        assert_eq!(
            world.record().unwrap().arguments["server"],
            "https://192.0.2.10:8443"
        );
        let listed = list(&world.paths, &world.store());
        assert_eq!(listed[0].settings["server"], "https://192.0.2.10:8443");
        assert!(listed[0].problem.is_empty(), "{}", listed[0].problem);
        // An empty value is forgotten.
        crate::provisioner::save_settings(
            &world.paths,
            "lab",
            &BTreeMap::from([("server".to_owned(), " ".to_owned())]),
        )
        .unwrap();
        let listed = list(&world.paths, &world.store());
        assert!(listed[0].settings.is_empty());
        assert!(
            listed[0]
                .problem
                .contains("berth provisioner lab run settings server=VALUE"),
            "{}",
            listed[0].problem
        );
    }

    #[test]
    fn a_provisioner_says_how_it_is_and_lists_the_machines_berth_made() {
        use crate::provisioner::state;
        let world = World::new();
        let server = BTreeMap::from([("server".to_owned(), "x".to_owned())]);
        // Without its secret it is not asked.
        let page = state(&world.paths, &world.store(), "fake/vm", &server, &quick()).unwrap();
        assert_eq!(page.state, "unusable");
        assert!(page.message.contains("api_token"), "{}", page.message);
        assert!(world.requests().is_empty());
        with_secret(&world);
        // Nor without where it is.
        let page = state(
            &world.paths,
            &world.store(),
            "fake/vm",
            &BTreeMap::new(),
            &quick(),
        )
        .unwrap();
        assert_eq!(page.state, "unusable");
        assert!(
            page.message.contains("needs server=VALUE"),
            "{}",
            page.message
        );
        let refused = state(
            &world.paths,
            &world.store(),
            "fake/old",
            &BTreeMap::new(),
            &quick(),
        )
        .unwrap();
        assert!(refused.message.contains("version 1"));

        world.file(
            "answer.create",
            r#"{"id": "vm-1", "address": "192.0.2.20"}"#,
        );
        world.provision("fake/vm", &[("server", "x")]).0.unwrap();
        world.file(
            "answer.status",
            r#"{"message": "Engine 27.1 answers", "details": [{"label": "Version", "value": "27.1"},
                {"label": "Containers", "value": 3}, {"value": "no label"}],
               "values": {"runtime": ["privileged"], "odd": "x"}}"#,
        );
        world.file(
            "answer.list",
            r#"{"resources": [{"id": "vm-1", "kind": "machine", "name": "studio", "state": "running"},
                {"id": "vm-9", "kind": "machine", "name": "attic", "state": "stopped"}]}"#,
        );
        let page = state(&world.paths, &world.store(), "fake/vm", &server, &quick()).unwrap();
        assert_eq!(
            (page.state.as_str(), page.message.as_str()),
            ("ready", "Engine 27.1 answers")
        );
        assert_eq!(
            page.details,
            [
                berth_api::Detail {
                    label: "Version".into(),
                    value: "27.1".into()
                },
                berth_api::Detail {
                    label: "Containers".into(),
                    value: "3".into()
                },
            ]
        );
        assert_eq!(page.values["runtime"], ["privileged"]);
        assert!(page.values["odd"].is_empty());
        let machines: Vec<_> = page
            .resources
            .iter()
            .map(|made| {
                (
                    made.host.as_str(),
                    made.id.as_str(),
                    made.state.as_str(),
                    !made.resource.is_empty(),
                )
            })
            .collect();
        assert_eq!(
            machines,
            [
                ("studio", "vm-1", "running", true),
                ("attic", "vm-9", "stopped", false)
            ]
        );
        // status about the provisioner itself names no machine.
        let (verb, status) = world
            .requests()
            .into_iter()
            .rfind(|(verb, _)| verb == "status")
            .unwrap();
        assert_eq!(verb, "status");
        assert!(status.get("id").is_none() && status.get("host").is_none());
        assert_eq!(status["version"], 2);
        assert_eq!(status["arguments"], json!({"server": "x"}));
        // One that does not answer is unreachable, with why.
        world.file("code.status", "1");
        world.file("say.status", "cannot reach 192.0.2.10\n");
        let page = state(&world.paths, &world.store(), "fake/vm", &server, &quick()).unwrap();
        assert_eq!(page.state, "unreachable");
        assert!(
            page.message.contains("cannot reach 192.0.2.10"),
            "{}",
            page.message
        );
        assert_eq!(page.resources.len(), 1);
    }

    #[test]
    fn an_action_is_checked_and_sent_with_its_arguments_apart_from_the_provisioners() {
        use crate::provisioner::{SETTINGS, actions, prepare};
        let world = World::new();
        let listed = actions(&world.paths, &world.store(), "fake/vm").unwrap();
        let names: Vec<&str> = listed.iter().map(|action| action.name.as_str()).collect();
        assert_eq!(names, [SETTINGS, "images", "prune"]);
        // Without its secret its own actions cannot run, and say why; its settings can.
        assert!(!listed[1].available && listed[1].why.contains("api_token"));
        assert!(listed[0].available);
        // Until it is set, an action asks for where the provisioner is.
        let asks: Vec<&str> = listed[1]
            .arguments
            .iter()
            .map(|argument| argument.name.as_str())
            .collect();
        assert_eq!(asks, ["server", "filter"]);
        assert!(
            actions(&world.paths, &world.store(), "fake/old")
                .unwrap()
                .is_empty()
        );
        with_secret(&world);
        let given = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect()
        };
        let refused = |action: &str, pairs: &[(&str, &str)]| {
            prepare(
                &world.paths,
                &world.store(),
                "fake/vm",
                action,
                &given(pairs),
            )
            .err()
            .unwrap()
            .to_string()
        };
        assert!(refused("dance", &[]).contains("Its actions: settings, images, prune"));
        let said = refused("images", &[("colour", "red")]);
        assert!(said.contains("It takes: filter, server"), "{said}");
        assert!(refused("images", &[]).contains("needs server=VALUE"));

        world.file(
            "answer.act",
            r#"{"message": "2 images", "details": [{"label": "ubuntu", "value": "80 MB"}]}"#,
        );
        world.file("say.act", "using hunter2-pass and tok-s3cret-value\n");
        let acting = prepare(
            &world.paths,
            &world.store(),
            "fake/vm",
            "prune",
            &given(&[("server", "x"), ("password", "hunter2-pass")]),
        )
        .unwrap();
        assert!(acting.changes());
        let secrets = acting.secrets();
        assert!(secrets.contains(&"hunter2-pass".to_owned()));
        let mut out = vec![];
        let answered = acting.run(&world.paths, &quick(), &mut out).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert_eq!(out, "using hunter2-pass and [secret]\n");
        assert_eq!(answered.message, "2 images");
        assert_eq!(answered.lines(), ["✓ 2 images", "  ubuntu  80 MB"]);
        let (verb, act) = world.requests().pop().unwrap();
        assert_eq!(verb, "act");
        assert_eq!(act["arguments"], json!({"server": "x"}));
        assert_eq!(
            act["action"],
            json!({"name": "prune", "arguments": {"password": "hunter2-pass"}})
        );
        assert!(act.get("id").is_none());
        assert_eq!(act["secrets"]["api_token"], "tok-s3cret-value");

        // Settings are berth's own, and kept for every call after.
        let acting = prepare(
            &world.paths,
            &world.store(),
            "fake/vm",
            SETTINGS,
            &given(&[("server", "https://192.0.2.11:8443")]),
        )
        .unwrap();
        let answered = acting.run(&world.paths, &quick(), &mut vec![]).unwrap();
        assert_eq!(
            answered.message,
            "fake/vm uses server=https://192.0.2.11:8443"
        );
        assert!(refused(SETTINGS, &[("cpus", "2")]).contains("no argument cpus"));
        let acting = prepare(
            &world.paths,
            &world.store(),
            "fake/vm",
            "images",
            &BTreeMap::new(),
        )
        .unwrap();
        assert!(!acting.changes());
        acting.run(&world.paths, &quick(), &mut vec![]).unwrap();
        let (_, act) = world.requests().pop().unwrap();
        assert_eq!(act["arguments"]["server"], "https://192.0.2.11:8443");
        assert_eq!(act["action"]["arguments"], json!({"filter": ""}));
        let listed = actions(&world.paths, &world.store(), "fake/vm").unwrap();
        assert_eq!(listed[0].arguments[0].default, "https://192.0.2.11:8443");
        assert_eq!(listed[1].arguments.len(), 1);
    }

    #[test]
    fn a_machines_log_is_streamed_with_secrets_masked() {
        let world = World::new();
        with_secret(&world);
        world.file(
            "answer.create",
            r#"{"id": "vm-1", "address": "192.0.2.20"}"#,
        );
        world.provision("fake/vm", &[("server", "x")]).0.unwrap();
        world.file("answer.logs", "booted\nkey tok-s3cret-value\n");
        let mut out = vec![];
        resource::logs(
            &world.paths,
            &world.store(),
            &world.machine(),
            20,
            false,
            Duration::from_secs(5),
            &mut out,
        )
        .unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(
            out.contains("booted\n") && out.contains("key [secret]"),
            "{out}"
        );
        let (verb, asked) = world.requests().pop().unwrap();
        assert_eq!(verb, "logs");
        assert_eq!(
            (asked["id"].as_str(), asked["tail"].as_u64()),
            (Some("vm-1"), Some(20))
        );
        assert_eq!(asked["follow"], false);
        // One that follows is stopped when the time is up.
        world.file("hang.logs", "");
        let started = Instant::now();
        resource::logs(
            &world.paths,
            &world.store(),
            &world.machine(),
            0,
            true,
            Duration::from_millis(300),
            &mut vec![],
        )
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
