//! Creating the machine a saved host's profile describes, with a provisioner
//! of a plugin. berth runs the provisioner where it runs itself, keeps the
//! machine's id beside the profile in `NAME.instance.json`, and from then on
//! the machine is an ordinary host. One machine per profile.
//!
//! A provisioner is run as `COMMAND VERB` in its package directory and in a
//! process group of its own. It reads one JSON object on standard input and
//! answers one on standard output; the lines of its standard error are
//! progress. Secrets travel only on its standard input, and the request is
//! never written anywhere.

use crate::{
    Result, apply,
    host::Host,
    link::{self, Link},
    paths::Paths,
    plugins::{self, Plugin, Provisioner, Setup},
    registry::Store,
    relay, seed,
    server::KEPT,
};
use berth_api::{Instance, ListedInstance, Provision, ProvisionerInfo};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    io::{BufRead, Read, Write},
    os::unix::{fs::OpenOptionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

/// The version of the protocol in each request.
const PROTOCOL: u64 = 1;
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
    pub reaches: fn(&Host) -> bool,
    /// Whether a seeded machine's berthd answers.
    pub answers: fn(&Paths, &Host) -> bool,
    /// What a seeded machine's first boot has written since a cursor of its
    /// log, read over SSH, as [`relay::FirstBoot::take`] reads it.
    pub first_boot: fn(&Host, &str) -> Result<String>,
    /// What is done once it answers: a seeded one's provision state is
    /// told, and the profile is applied to the other.
    pub finish: fn(&Paths, &Host, Setup, &mut dyn Write) -> Result<()>,
    /// Forget the SSH host key known for the machine's address: a machine
    /// just made has a new one, and the old one was another machine's.
    pub forget: fn(&Host),
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
            forget,
        }
    }
}

impl Options {
    fn limit(&self, verb: &str) -> Duration {
        match verb {
            "create" => self.create,
            "status" | "list" => self.quick,
            _ => self.change,
        }
    }
}

// ── The record beside the profile ───────────────────────────────────────────

/// What berth keeps of the machine it created for a host, in
/// `NAME.instance.json` beside the profile.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// `PLUGIN/ID`.
    pub provisioner: String,
    /// Empty until `create` has said it.
    #[serde(default)]
    pub id: String,
    /// What `create` is asked with, so asking again finds the same machine.
    pub token: String,
    #[serde(default)]
    pub arguments: BTreeMap<String, String>,
    #[serde(default)]
    pub created: String,
}

impl Record {
    pub fn path(paths: &Paths, host: &str) -> Result<PathBuf> {
        Ok(paths.profile(host)?.with_extension("instance.json"))
    }

    pub fn load(paths: &Paths, host: &str) -> Result<Option<Record>> {
        let path = Record::path(paths, host)?;
        match fs::read(&path) {
            Ok(data) => Ok(Some(
                serde_json::from_slice(&data)
                    .map_err(|error| format!("{}: {error}", path.display()))?,
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("{}: {error}", path.display()).into()),
        }
    }

    pub fn save(&self, paths: &Paths, host: &str) -> Result<()> {
        let path = Record::path(paths, host)?;
        let parent = path.parent().ok_or("A record is in a directory")?;
        // Temporary files are created private, which the record stays.
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer_pretty(&mut file, self)?;
        file.write_all(b"\n")?;
        file.persist(&path)?;
        Ok(())
    }

    pub fn remove(paths: &Paths, host: &str) -> Result<()> {
        match fs::remove_file(Record::path(paths, host)?) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
            _ => Ok(()),
        }
    }
}

/// Held while something is done to a host's machine. A second caller is
/// refused rather than kept waiting.
pub struct Lock(fs::File);

impl Drop for Lock {
    /// Unlocked rather than only closed: a process started meanwhile may hold
    /// a copy of the descriptor until it runs its program, and with it the lock.
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // SAFETY: the descriptor is the open file's own.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

pub fn lock(paths: &Paths, host: &str) -> Result<Lock> {
    let path = Record::path(paths, host)?.with_extension("lock");
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(&path)?;
    use std::os::fd::AsRawFd;
    // SAFETY: the descriptor is the open file's own.
    let taken = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    match taken {
        true => Ok(Lock(file)),
        false => Err(format!(
            "Something else is being done to {host}'s machine. Try again when it has ended"
        )
        .into()),
    }
}

/// Whether something is being done to a host's machine right now.
pub fn busy(paths: &Paths, host: &str) -> bool {
    Record::path(paths, host).is_ok_and(|path| path.with_extension("lock").exists())
        && lock(paths, host).is_err()
}

// ── Finding provisioners ────────────────────────────────────────────────────

fn found(paths: &Paths) -> Vec<Plugin> {
    let own = plugins::own_dir(&paths.config_dir);
    match paths.source() {
        Ok(source) => plugins::found(&source, &own),
        Err(_) => plugins::load(&[own]),
    }
}

fn find<'a>(plugins: &'a [Plugin], name: &str) -> Result<(&'a Plugin, &'a Provisioner)> {
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

/// The value of a secret, when it has a usable one here.
fn secret(store: &Store, name: &str) -> Option<String> {
    store
        .secret(name)
        .ok()
        .flatten()
        .filter(|value| !value.is_empty() && value != KEPT)
}

/// The secrets a provisioner is given, and the required ones without a value.
fn secrets(store: &Store, provisioner: &Provisioner) -> (Map<String, Value>, Vec<String>) {
    let (mut given, mut missing) = (Map::new(), vec![]);
    for wanted in &provisioner.secrets {
        match secret(store, &wanted.name) {
            Some(value) => {
                given.insert(wanted.name.clone(), json!(value));
            }
            None if wanted.required => missing.push(wanted.name.clone()),
            None => {}
        }
    }
    (given, missing)
}

fn needs_secrets(name: &str, missing: &[String]) -> Result<()> {
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

/// Every argument the provisioner declares, given or with its default.
/// Names it does not declare and required ones without a value are refused.
fn arguments(
    name: &str,
    provisioner: &Provisioner,
    given: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    if let Some(unknown) = given.keys().find(|asked| {
        !provisioner
            .arguments
            .iter()
            .any(|known| &known.name == *asked)
    }) {
        let known: Vec<&str> = provisioner
            .arguments
            .iter()
            .map(|known| known.name.as_str())
            .collect();
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
    for argument in &provisioner.arguments {
        let value = given
            .get(&argument.name)
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

/// What every verb is sent. `list` is about no host.
fn request(
    verb: &str,
    provisioner: &Provisioner,
    host: Option<(&str, &str)>,
    arguments: &BTreeMap<String, String>,
    secrets: &Map<String, Value>,
) -> Value {
    let mut request = json!({
        "version": PROTOCOL,
        "verb": verb,
        "provisioner": provisioner.id,
        "arguments": arguments,
        "secrets": secrets,
    });
    if let Some((host, token)) = host {
        request["host"] = json!(host);
        request["labels"] = json!({"berth-host": host, "berth-instance": token});
    }
    request
}

/// The provisioners there are, with the required secrets that are not set.
pub fn list(paths: &Paths, store: &Store) -> Vec<ProvisionerInfo> {
    found(paths)
        .iter()
        .flat_map(|plugin| {
            plugin
                .provisioners
                .iter()
                .map(|provisioner| provisioner.info(&plugin.id, secrets(store, provisioner).1))
        })
        .collect()
}

// ── Running a provisioner ───────────────────────────────────────────────────

fn say(out: &mut dyn Write, line: &str) {
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// The texts that must not be shown: each secret, and each longer line of
/// one, as a key or a certificate is split over lines.
fn hidden(secrets: &Map<String, Value>) -> Vec<String> {
    let mut texts = vec![];
    for value in secrets.values().filter_map(Value::as_str) {
        texts.push(value.to_owned());
        texts.extend(
            value
                .lines()
                .map(str::trim)
                .filter(|line| line.len() >= 8)
                .map(str::to_owned),
        );
    }
    texts.retain(|text| !text.is_empty());
    // The longest first, so a part does not leave the rest of a whole.
    texts.sort_by_key(|text| std::cmp::Reverse(text.len()));
    texts
}

fn hide(line: String, hidden: &[String]) -> String {
    hidden
        .iter()
        .fold(line, |line, text| line.replace(text.as_str(), "[secret]"))
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
fn call(
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
    /// `PLUGIN/ID`.
    name: String,
    dir: PathBuf,
    provisioner: Provisioner,
    secrets: Map<String, Value>,
    /// What is never shown of what the machine prints: the provisioner's
    /// secrets and those the seed carries.
    hidden: Vec<String>,
    record: Record,
    _lock: Lock,
}

/// Check that `host` can have a machine made by what `asked` names, and take
/// its lock. Refused: a host that has a machine, an unfinished one of another
/// provisioner, an unknown provisioner, unknown or missing arguments and
/// missing secrets, and a host something else is being done to.
pub fn prepare(paths: &Paths, store: &Store, host: &str, asked: &Provision) -> Result<Prepared> {
    let profile = Host::load(paths, host).map_err(|_| format!("No host {host}"))?;
    let lock = lock(paths, host)?;
    let kept = Record::load(paths, host)?;
    if let Some(kept) = kept.as_ref().filter(|kept| !kept.id.is_empty()) {
        return Err(format!(
            "{host} has a machine already, {} of {}. See it with: berth instance {host}; delete it with: berth instance {host} destroy",
            kept.id, kept.provisioner
        )
        .into());
    }
    if let Some(kept) = kept
        .as_ref()
        .filter(|kept| kept.provisioner != asked.provisioner)
    {
        return Err(format!(
            "A machine for {host} was asked of {}, which did not say it made one. Forget it with: berth instance {host} destroy",
            kept.provisioner
        )
        .into());
    }
    let plugins = found(paths);
    let (plugin, provisioner) = find(&plugins, &asked.provisioner)?;
    let arguments = arguments(&asked.provisioner, provisioner, &asked.arguments)?;
    let (secrets, missing) = secrets(store, provisioner);
    needs_secrets(&asked.provisioner, &missing)?;
    if provisioner.setup == Setup::Apply
        && profile.config["builder"]["ssh_user"]
            .as_str()
            .unwrap_or("")
            .is_empty()
    {
        return Err(format!(
            "{} creates the machine with an account berth reaches over SSH: the profile of {host} needs builder.ssh_user",
            asked.provisioner
        )
        .into());
    }
    // Asked again after a failure, the provisioner finds the machine by its token.
    let token = match kept {
        Some(kept) => kept.token,
        None => new_token()?,
    };
    Ok(Prepared {
        host: host.to_owned(),
        name: asked.provisioner.clone(),
        dir: plugin.dir.clone(),
        provisioner: provisioner.clone(),
        hidden: hidden(&secrets),
        secrets,
        record: Record {
            provisioner: asked.provisioner.clone(),
            id: String::new(),
            token,
            arguments,
            created: berth_api::now(),
        },
        _lock: lock,
    })
}

/// Make the host's profile a `cloud` one before the machine is made from it:
/// a created machine has no laptop's physical login or power settings, and
/// the seed is rendered from, or the role applied with, what is saved.
fn cloud_profile(paths: &Paths, host: &str) -> Result<()> {
    let path = paths.profile(host)?;
    let mut profile: Value = serde_json::from_slice(&fs::read(&path)?)?;
    if profile["profile"] != "cloud" && profile.is_object() {
        profile["profile"] = json!("cloud");
        seed::save_profile(&path, &profile)?;
    }
    Ok(())
}

fn new_token() -> Result<String> {
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
    asked: &Provision,
    options: &Options,
    out: &mut dyn Write,
) -> Result<()> {
    prepare(paths, store, host, asked)?.run(paths, store, options, out)
}

impl Prepared {
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
        let mut request = request(
            "create",
            &self.provisioner,
            Some((&host, &self.record.token)),
            &self.record.arguments,
            &self.secrets,
        );
        request["token"] = json!(self.record.token);
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
                    "meta_data": format!("instance-id: {}\nlocal-hostname: {hostname}\n", self.record.token),
                });
                if let Some(archive) = seed.archive {
                    request["seed"]["archive"] =
                        json!({"path": archive.path, "sha256": archive.sha256});
                }
            }
            Setup::Apply => {
                let mut keys: Vec<String> = paths.own_public_key().into_iter().collect();
                for key in berth_api::profile::account_keys(&profile.config, "operator") {
                    if !keys.contains(&key) {
                        keys.push(key);
                    }
                }
                request["access"] = json!({
                    "user": profile.config["builder"]["ssh_user"],
                    "public_keys": keys,
                });
            }
        }
        // Written before the machine is asked for, so a crash leaves the token to ask again with.
        self.record.save(paths, &host)?;
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
        self.record.id = id.clone();
        self.record.save(paths, &host)?;
        say(out, &format!("✓ {host}: {name} created {id}"));
        let exists = |error: Box<dyn std::error::Error>| -> Box<dyn std::error::Error> {
            format!(
                "{error}. The machine {id} exists and stays: berth instance {host} destroy deletes it"
            )
            .into()
        };
        take_address(&profile.path, &answer, out).map_err(exists)?;
        self.wait(paths, options, out).map_err(exists)
    }

    /// Wait until the machine answers, asking the provisioner about it in
    /// between, and then finish setting it up.
    fn wait(&self, paths: &Paths, options: &Options, out: &mut dyn Write) -> Result<()> {
        let host = Host::load(paths, &self.host)?;
        let name = &self.host;
        let setup = self.provisioner.setup;
        let reachable = !host.address().is_empty() || host.config["builder"]["link"].is_array();
        if !reachable && setup == Setup::Seed {
            say(
                out,
                &format!(
                    "! {name} has no address berth knows: it sets itself up on its own. Set builder.ssh_host to reach it"
                ),
            );
            return Ok(());
        }
        if !reachable {
            return Err(
                format!("{name} has no address to reach it at: set builder.ssh_host").into(),
            );
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
        let over_ssh = !host.address().is_empty();
        if over_ssh {
            (options.forget)(&host);
        }
        let started = Instant::now();
        let deadline = started + limit;
        let mut status = request(
            "status",
            &self.provisioner,
            Some((name, &self.record.token)),
            &self.record.arguments,
            &self.secrets,
        );
        status["id"] = json!(self.record.id);
        let mut log = status.clone();
        log["verb"] = json!("log");
        let logs = self.provisioner.verbs.iter().any(|verb| verb == "log");
        let mut out = Noted::new(out);
        let mut seen = Seen::default();
        // A host reached only through its link command has no SSH to wait for.
        let mut in_reach = !over_ssh;
        loop {
            if !in_reach && (options.reaches)(&host) {
                in_reach = true;
                say(&mut out, &format!("✓ {name}: SSH answers"));
            }
            if in_reach && setup == Setup::Apply {
                break;
            }
            if in_reach && over_ssh {
                // A look that fails is tried again the next time round.
                if let Ok(printed) = (options.first_boot)(&host, &seen.boot.cursor) {
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
                match call(
                    &self.dir,
                    &self.provisioner,
                    "log",
                    &log,
                    options.quick,
                    &mut std::io::sink(),
                ) {
                    Ok(answer) => {
                        let lines = answer["lines"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
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
fn take_address(path: &Path, answer: &Value, out: &mut dyn Write) -> Result<()> {
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
    let mut profile: Value = serde_json::from_slice(&fs::read(path)?)?;
    if !profile["builder"].is_object() {
        profile["builder"] = json!({});
    }
    let mut changed = false;
    for (key, value) in [("ssh_host", address), ("ssh_port", port.as_str())] {
        let kept = profile["builder"][key].as_str().unwrap_or("");
        if kept.is_empty() && !value.is_empty() {
            profile["builder"][key] = json!(value);
            changed = true;
        }
    }
    // A profile that names no SSH user would be reached as whoever runs berth;
    // the account that holds berth's key on the new machine is the operator.
    let operator = profile["operator_username"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    if changed
        && profile["builder"]["ssh_user"]
            .as_str()
            .unwrap_or("")
            .is_empty()
        && usable(&operator)
        && !operator.is_empty()
    {
        profile["builder"]["ssh_user"] = json!(operator);
    }
    if changed {
        seed::save_profile(path, &profile)?;
        say(out, &format!("✓ The profile reaches it at {address}"));
    }
    Ok(())
}

// ── Waiting for the machine ─────────────────────────────────────────────────

/// The sudo password of a host whose SSH user's sudo asks for one, from the
/// environment: nobody is there to ask. Only a machine that is applied rather
/// than seeded can have such a user: the seed gives the operator sudo without
/// a password before SSH answers.
fn password(host: &Host) -> Option<String> {
    std::env::var("BERTH_SUDO_PASSWORD")
        .ok()
        .filter(|_| host.asks_password())
}

fn forget(host: &Host) {
    let port = host.config["builder"]["ssh_port"].as_str().unwrap_or("");
    // ssh files a key under [ADDRESS]:PORT when the port is not its usual one.
    let known = match port {
        "" | "22" => host.address().to_owned(),
        port => format!("[{}]:{port}", host.address()),
    };
    let _ = Command::new("ssh-keygen")
        .args(["-R", &known])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Whether SSH lets berth in. A machine just made has a key nobody has seen;
/// its first one is taken.
fn ssh_answers(host: &Host) -> bool {
    let mut ssh = host.ssh();
    ssh.args([
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "StrictHostKeyChecking=accept-new",
    ])
    .arg(host.target())
    .arg("true")
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    ssh.status().is_ok_and(|status| status.success())
}

/// Whether berthd answers on a seeded host's link. The wait asks only once SSH
/// answers, which has made the machine's key known for the link to use.
fn answers(_paths: &Paths, host: &Host) -> bool {
    let (Ok(spec), Ok(runtime)) = (host.link(None), link::runtime()) else {
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
fn first_boot(host: &Host, cursor: &str) -> Result<String> {
    let mut ssh = host.ssh();
    ssh.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"])
        .arg(host.target())
        .arg(first_boot_look(cursor));
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
            let link = Link::new(host.link(None)?);
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
            let password = password(host);
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
                true => apply::settle(host),
                false => Err(format!(
                    "applying failed (ansible-playbook ended with {})",
                    status.code().unwrap_or(-1)
                )
                .into()),
            }
        }
    }
}

// ── The machine afterwards ──────────────────────────────────────────────────

/// The record of a host and the provisioner that made its machine.
struct Made {
    record: Record,
    plugins: Vec<Plugin>,
}

impl Made {
    fn load(paths: &Paths, host: &str) -> Result<Made> {
        let record = Record::load(paths, host)?
            .ok_or_else(|| format!("{host} has no machine that berth created"))?;
        Ok(Made {
            record,
            plugins: found(paths),
        })
    }

    fn provisioner(&self) -> Result<(&Plugin, &Provisioner)> {
        find(&self.plugins, &self.record.provisioner).map_err(|_| {
            format!(
                "No provisioner {} any more, which made this machine: add its plugin again",
                self.record.provisioner
            )
            .into()
        })
    }

    /// A request about the machine. Arguments it did not have when the
    /// machine was made take their defaults.
    fn request(
        &self,
        verb: &str,
        host: &str,
        store: &Store,
    ) -> Result<(PathBuf, Provisioner, Value)> {
        let (plugin, provisioner) = self.provisioner()?;
        let (secrets, missing) = secrets(store, provisioner);
        needs_secrets(&self.record.provisioner, &missing)?;
        let mut arguments = self.record.arguments.clone();
        for argument in &provisioner.arguments {
            arguments
                .entry(argument.name.clone())
                .or_insert_with(|| argument.default.clone());
        }
        let mut request = request(
            verb,
            provisioner,
            Some((host, &self.record.token)),
            &arguments,
            &secrets,
        );
        request["id"] = json!(self.record.id);
        Ok((plugin.dir.clone(), provisioner.clone(), request))
    }
}

/// The machine berth created for a host, and what its provisioner says of
/// it now. None when berth created none.
pub fn status(
    paths: &Paths,
    store: &Store,
    host: &str,
    options: &Options,
) -> Result<Option<Instance>> {
    if Record::load(paths, host)?.is_none() {
        return Ok(None);
    }
    let made = Made::load(paths, host)?;
    let mut instance = Instance {
        provisioner: made.record.provisioner.clone(),
        id: made.record.id.clone(),
        state: "unknown".into(),
        address: Host::load(paths, host)
            .map(|host| host.address().to_owned())
            .unwrap_or_default(),
        created: made.record.created.clone(),
        arguments: made.record.arguments.clone(),
        verbs: vec![],
        problem: String::new(),
    };
    if let Ok((_, provisioner)) = made.provisioner() {
        instance.verbs = provisioner
            .verbs
            .iter()
            .filter(|verb| !matches!(verb.as_str(), "list" | "log"))
            .cloned()
            .collect();
    }
    if made.record.id.is_empty() {
        match busy(paths, host) {
            true => instance.state = "creating".into(),
            false => {
                instance.problem = format!(
                    "Creating it did not end. berth provision {host} {} asks again; berth instance {host} destroy forgets it",
                    made.record.provisioner
                )
            }
        }
        return Ok(Some(instance));
    }
    let answer = made
        .request("status", host, store)
        .and_then(|(dir, provisioner, request)| {
            call(
                &dir,
                &provisioner,
                "status",
                &request,
                options.quick,
                &mut std::io::sink(),
            )
        });
    match answer {
        Ok(answer) => match answer["state"].as_str() {
            Some(state @ ("running" | "stopped" | "gone")) => {
                instance.state = state.into();
                if let Some(address) = answer["address"]
                    .as_str()
                    .filter(|address| !address.is_empty())
                {
                    instance.address = address.into();
                }
            }
            _ => instance.problem = format!("{} status said no state", made.record.provisioner),
        },
        Err(error) => instance.problem = error.to_string(),
    }
    Ok(Some(instance))
}

/// Delete the machine berth created for a host, by the id it keeps, and
/// forget it. The profile stays. A record without an id is only forgotten.
pub fn destroy(
    paths: &Paths,
    store: &Store,
    host: &str,
    options: &Options,
    out: &mut dyn Write,
) -> Result<()> {
    let _lock = lock(paths, host)?;
    let made = Made::load(paths, host)?;
    if made.record.id.is_empty() {
        Record::remove(paths, host)?;
        say(
            out,
            &format!(
                "✓ Forgot the unfinished machine of {host}. One {} made anyway is not deleted",
                made.record.provisioner
            ),
        );
        return Ok(());
    }
    let (dir, provisioner, request) = made.request("destroy", host, store)?;
    say(
        out,
        &format!(
            "▸ {host}: deleting {} with {}",
            made.record.id, made.record.provisioner
        ),
    );
    if let Err(error) = call(
        &dir,
        &provisioner,
        "destroy",
        &request,
        options.limit("destroy"),
        out,
    ) {
        // One that is gone already is what was asked for.
        let mut asked = request.clone();
        asked["verb"] = json!("status");
        let gone = call(
            &dir,
            &provisioner,
            "status",
            &asked,
            options.quick,
            &mut std::io::sink(),
        )
        .is_ok_and(|answer| answer["state"] == "gone");
        if !gone {
            return Err(error);
        }
    }
    Record::remove(paths, host)?;
    say(
        out,
        &format!(
            "✓ Deleted {host}'s machine. The profile stays, and so does its node on the tailnet: remove that there"
        ),
    );
    Ok(())
}

/// Start or stop the machine, when its provisioner can.
pub fn power(
    paths: &Paths,
    store: &Store,
    host: &str,
    verb: &str,
    options: &Options,
    out: &mut dyn Write,
) -> Result<()> {
    if !matches!(verb, "start" | "stop") {
        return Err(format!("A machine is started or stopped, not {verb}").into());
    }
    let _lock = lock(paths, host)?;
    let made = Made::load(paths, host)?;
    if made.record.id.is_empty() {
        return Err(format!("{host}'s machine was not made yet").into());
    }
    let (dir, provisioner, request) = made.request(verb, host, store)?;
    if !provisioner.verbs.iter().any(|known| known == verb) {
        return Err(format!("{} cannot {verb} a machine", made.record.provisioner).into());
    }
    call(&dir, &provisioner, verb, &request, options.limit(verb), out)?;
    Ok(())
}

/// The machines a provisioner has that berth created, as it lists them.
pub fn instances(
    paths: &Paths,
    store: &Store,
    name: &str,
    given: &BTreeMap<String, String>,
    options: &Options,
) -> Result<Vec<ListedInstance>> {
    let plugins = found(paths);
    let (plugin, provisioner) = find(&plugins, name)?;
    if !provisioner.verbs.iter().any(|verb| verb == "list") {
        return Err(format!("{name} cannot list its machines").into());
    }
    let arguments = arguments(name, provisioner, given)?;
    let (secrets, missing) = secrets(store, provisioner);
    needs_secrets(name, &missing)?;
    let request = request("list", provisioner, None, &arguments, &secrets);
    let answer = call(
        &plugin.dir,
        provisioner,
        "list",
        &request,
        options.quick,
        &mut std::io::sink(),
    )?;
    let listed = answer["instances"].as_array().cloned().unwrap_or_default();
    let text = |entry: &Value, key: &str| entry[key].as_str().unwrap_or("").to_owned();
    Ok(listed
        .iter()
        .filter(|entry| !text(entry, "id").is_empty())
        .map(|entry| ListedInstance {
            id: text(entry, "id"),
            host: text(entry, "host"),
            state: text(entry, "state"),
        })
        .collect())
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
                    {"id": "vm", "label": "Fake VM", "command": "bin/provision", "setup": "apply",
                     "verbs": ["start", "list", "dance"],
                     "secrets": ["api_token", {"name": "extra", "required": false}],
                     "arguments": [{"name": "server", "required": true}, {"name": "cpus", "default": "4"}]},
                    {"id": "seeded", "command": "bin/provision", "setup": "seed", "archive": true,
                     "verbs": ["log"]}]}}"#,
            )
            .unwrap();
            let world = World {
                _home: home,
                paths,
                plugin: fs::canonicalize(plugin).unwrap(),
            };
            world.profile(
                "studio",
                json!({"hostname": "studio", "builder": {"ssh_user": "ops"},
                       "ansible_vars": {"devbox_install_tailscale": false}}),
            );
            world
        }

        fn profile(&self, name: &str, profile: Value) {
            fs::write(self.paths.profile(name).unwrap(), profile.to_string()).unwrap();
        }

        fn store(&self) -> Store {
            Store::new(&self.paths)
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
            let asked = Provision {
                provisioner: name.into(),
                arguments: arguments
                    .iter()
                    .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                    .collect(),
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

        fn record(&self) -> Option<Record> {
            Record::load(&self.paths, "studio").unwrap()
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
            reaches: |_| true,
            answers: |_, _| true,
            first_boot: |_, _| Ok(String::new()),
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
            forget: |_| {},
        }
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
        assert!(kept.id.is_empty() && kept.token.len() == 32);
        let mode = fs::metadata(Record::path(&world.paths, "studio").unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        // Another provisioner cannot take over while the first may have made one.
        let said = world
            .provision("fake/seeded", &[])
            .0
            .unwrap_err()
            .to_string();
        assert!(said.contains("berth instance studio destroy"), "{said}");

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
        assert_eq!(create["version"], 1);
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
        assert_eq!(world.record().unwrap().id, "vm-1");
        let host = Host::load(&world.paths, "studio").unwrap();
        assert_eq!(host.address(), "192.0.2.20");
        assert_eq!(host.config["builder"]["ssh_port"], "2222");

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
        assert!(refused("fake/vm", &[("server", "x")]).contains("api_token"));
        let listed = list(&world.paths, &world.store());
        assert_eq!(listed[0].id, "fake/vm");
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
        assert!(refused("fake/vm", &[("server", "x")]).contains("builder.ssh_user"));
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
        let asked = Provision {
            provisioner: "fake/vm".into(),
            arguments: [("server".to_owned(), "x".to_owned())].into(),
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
        let asked = Provision {
            provisioner: "fake/vm".into(),
            arguments: [("server".to_owned(), "x".to_owned())].into(),
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
            json!({"builder": {"ssh_user": "ops", "ssh_host": "192.0.2.30"}}),
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
    fn destroy_uses_the_kept_id_takes_gone_for_done_and_keeps_the_profile() {
        let world = World::new();
        with_secret(&world);
        world.file(
            "answer.create",
            r#"{"id": "vm-1", "address": "192.0.2.20"}"#,
        );
        world.provision("fake/vm", &[("server", "x")]).0.unwrap();
        let mut out = vec![];
        // It fails, but says the machine is gone: what was asked for.
        world.file("code.destroy", "1");
        world.file("answer.status", r#"{"state": "gone"}"#);
        destroy(&world.paths, &world.store(), "studio", &quick(), &mut out).unwrap();
        let said = String::from_utf8(out).unwrap();
        assert!(said.contains("tailnet"), "{said}");
        assert!(world.record().is_none());
        assert!(world.paths.profile("studio").unwrap().is_file());
        let (verb, request) = world
            .requests()
            .into_iter()
            .find(|(verb, _)| verb == "destroy")
            .unwrap();
        assert_eq!(
            (verb.as_str(), request["id"].as_str()),
            ("destroy", Some("vm-1"))
        );
        assert!(
            destroy(
                &world.paths,
                &world.store(),
                "studio",
                &quick(),
                &mut vec![]
            )
            .is_err()
        );

        // One that fails and is not gone stays.
        world.forget("requests.log");
        world.file("answer.create", r#"{"id": "vm-2"}"#);
        world.profile(
            "studio",
            json!({"builder": {"ssh_user": "ops", "ssh_host": "192.0.2.20"}}),
        );
        world.provision("fake/vm", &[("server", "x")]).0.unwrap();
        world.file("answer.status", r#"{"state": "running"}"#);
        world.file("say.destroy", "permission denied\n");
        let said = destroy(
            &world.paths,
            &world.store(),
            "studio",
            &quick(),
            &mut vec![],
        )
        .unwrap_err()
        .to_string();
        assert!(said.contains("permission denied"), "{said}");
        assert_eq!(world.record().unwrap().id, "vm-2");

        // A record without an id is only forgotten.
        Record {
            provisioner: "fake/vm".into(),
            token: "t".into(),
            ..Default::default()
        }
        .save(&world.paths, "studio")
        .unwrap();
        world.forget("requests.log");
        destroy(
            &world.paths,
            &world.store(),
            "studio",
            &quick(),
            &mut vec![],
        )
        .unwrap();
        assert!(world.record().is_none() && world.requests().is_empty());
    }

    #[test]
    fn power_and_status_follow_what_the_provisioner_answers() {
        let world = World::new();
        with_secret(&world);
        assert!(
            status(&world.paths, &world.store(), "studio", &quick())
                .unwrap()
                .is_none()
        );
        world.file(
            "answer.create",
            r#"{"id": "vm-1", "address": "192.0.2.20"}"#,
        );
        world.provision("fake/vm", &[("server", "x")]).0.unwrap();
        power(
            &world.paths,
            &world.store(),
            "studio",
            "start",
            &quick(),
            &mut vec![],
        )
        .unwrap();
        let said = power(
            &world.paths,
            &world.store(),
            "studio",
            "stop",
            &quick(),
            &mut vec![],
        )
        .unwrap_err()
        .to_string();
        assert!(said.contains("cannot stop"), "{said}");
        assert!(
            power(
                &world.paths,
                &world.store(),
                "studio",
                "dance",
                &quick(),
                &mut vec![]
            )
            .is_err()
        );
        let verbs: Vec<String> = world.requests().into_iter().map(|(verb, _)| verb).collect();
        assert_eq!(verbs, ["create", "start"]);

        world.file(
            "answer.status",
            r#"{"state": "stopped", "address": "192.0.2.21"}"#,
        );
        let instance = status(&world.paths, &world.store(), "studio", &quick())
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                instance.state.as_str(),
                instance.id.as_str(),
                instance.address.as_str()
            ),
            ("stopped", "vm-1", "192.0.2.21")
        );
        // The manifest's unknown verb is not one, and list and log change no machine.
        assert_eq!(instance.verbs, ["start"]);
        world.file("code.status", "2");
        let instance = status(&world.paths, &world.store(), "studio", &quick())
            .unwrap()
            .unwrap();
        assert_eq!(instance.state, "unknown");
        assert!(!instance.problem.is_empty());

        world.file(
            "answer.list",
            r#"{"instances": [{"id": "vm-1", "host": "studio", "state": "running"}, {"host": "no id"}]}"#,
        );
        let listed = instances(
            &world.paths,
            &world.store(),
            "fake/vm",
            &[("server".to_owned(), "x".to_owned())].into(),
            &quick(),
        )
        .unwrap();
        assert_eq!(
            listed,
            [ListedInstance {
                id: "vm-1".into(),
                host: "studio".into(),
                state: "running".into()
            }]
        );
        let (_, request) = world.requests().pop().unwrap();
        assert!(request.get("host").is_none() && request.get("labels").is_none());
        assert!(
            instances(
                &world.paths,
                &world.store(),
                "fake/seeded",
                &BTreeMap::new(),
                &quick()
            )
            .is_err()
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
            reaches: |_| false,
            answers: |_, _| false,
            ..quick()
        };
        let asked = Provision {
            provisioner: "fake/seeded".into(),
            arguments: BTreeMap::new(),
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
            said.contains("gone") && said.contains("berth instance studio destroy"),
            "{said}"
        );
        assert_eq!(world.record().unwrap().id, "seeded-1");
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
        let asked = Provision {
            provisioner: "fake/seeded".into(),
            arguments: BTreeMap::new(),
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
            "answer.log",
            r#"{"lines": ["[  OK  ] Started Journal Service.", "\u001b[32mcloud-init\u001b[0m: key tskey-auth-k1 taken", "  "]}"#,
        );
        let options = Options {
            boot: Duration::from_secs(30),
            reaches: |_| asked("ssh") > 3,
            answers: |_, _| asked("berthd") > 2,
            first_boot: |_, cursor| {
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
        // The console is not asked for once SSH answers.
        let logs = world
            .requests()
            .iter()
            .filter(|(verb, _)| verb == "log")
            .count();
        assert_eq!(logs, 3);
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
            first_boot: |_, _| {
                Ok("state failed\n==> Applying the role\nTASK [devbox : Install Docker] *\nfatal: [localhost]: FAILED! => {\"msg\": \"No package docker-ce\"}\n".into())
            },
            ..quick()
        };
        let started = Instant::now();
        let (done, out) = seeded(&world, &options);
        let said = done.unwrap_err().to_string();
        assert!(started.elapsed() < Duration::from_secs(20));
        assert!(said.contains("studio's first boot failed"), "{said}");
        assert!(said.contains("berth instance studio destroy"), "{said}");
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
            reaches: |_| false,
            ..quick()
        };
        let asked = Provision {
            provisioner: "fake/vm".into(),
            arguments: [("server".to_owned(), "x".to_owned())].into(),
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
        // A provisioner without the log verb is not asked for one.
        assert!(world.requests().iter().all(|(verb, _)| verb != "log"));
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
        let path = home.path().join("studio.json");
        let taken = |profile: Value| {
            fs::write(&path, profile.to_string()).unwrap();
            let answer = json!({"address": "192.0.2.40"});
            take_address(&path, &answer, &mut Vec::new()).unwrap();
            let profile: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            profile["builder"].clone()
        };
        assert_eq!(
            taken(json!({"operator_username": "ops"})),
            json!({"ssh_host": "192.0.2.40", "ssh_user": "ops"})
        );
        // A user the profile names is kept.
        assert_eq!(
            taken(json!({"operator_username": "ops", "builder": {"ssh_user": "admin"}})),
            json!({"ssh_host": "192.0.2.40", "ssh_user": "admin"})
        );
    }

    #[test]
    fn an_argument_of_a_few_values_takes_only_those() {
        let provisioner = Provisioner {
            id: "box".into(),
            label: "Box".into(),
            description: String::new(),
            command: PathBuf::from("/bin/true"),
            setup: Setup::Apply,
            archive: false,
            verbs: vec![],
            secrets: vec![],
            arguments: vec![berth_api::Argument {
                name: "keep_volume".into(),
                default: "no".into(),
                values: vec!["no".into(), "yes".into()],
                ..Default::default()
            }],
        };
        let given = |value: &str| BTreeMap::from([("keep_volume".to_owned(), value.to_owned())]);
        assert_eq!(
            arguments("lab/box", &provisioner, &BTreeMap::new()).unwrap()["keep_volume"],
            "no"
        );
        assert_eq!(
            arguments("lab/box", &provisioner, &given("yes")).unwrap()["keep_volume"],
            "yes"
        );
        let said = arguments("lab/box", &provisioner, &given("maybe"))
            .unwrap_err()
            .to_string();
        assert_eq!(said, "lab/box takes keep_volume=no or yes only");
    }
}
