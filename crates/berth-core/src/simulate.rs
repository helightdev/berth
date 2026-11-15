//! A simulated world, for `berth serve --simulate`: the real server on a
//! scratch store, with only what reaches other machines stood in for.
//!
//! A host is a berthd of its own in single-user mode, in a folder of the
//! scratch data, reached over a link like any other. Its commands run
//! through a small shell that knows a few commands and nothing else, and it
//! shows only the processes it started. Applying a host writes progress and
//! the records a real apply leaves. The plugin `sim` holds a provisioner that
//! makes machines and services without making anything, and the tailnet
//! hands out keys that join nothing.

use crate::{
    Result, host::Host, host_type::HostType, link::LinkSpec, paths::Paths, plugins::Setup,
    profiles::Place,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Mutex, OnceLock},
    time::Duration,
};

/// The seconds a step takes, as `--delay` sets it for the server and its
/// provisioner.
pub const DELAY: &str = "BERTH_SIMULATE_DELAY";
/// The simulated provisioner's type, and the name it is registered under.
pub const PROVISIONER: &str = "sim/cloud";
pub const REGISTERED: &str = "sim";
/// What a scratch directory of a simulated server holds, so that no other
/// directory is ever taken for one.
pub const MARKER: &str = ".berth-simulated";

static ACTIVE: OnceLock<Simulation> = OnceLock::new();

/// The simulation this process runs, when it runs one.
pub fn active() -> Option<&'static Simulation> {
    ACTIVE.get()
}

/// How long one step takes: two seconds unless [`DELAY`] says otherwise.
pub fn delay() -> Duration {
    std::env::var(DELAY)
        .ok()
        .and_then(|seconds| seconds.trim().parse::<f64>().ok())
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
        .map_or(Duration::from_secs(2), Duration::from_secs_f64)
}

/// Wait `steps` of the delay.
pub fn pause(steps: f64) {
    std::thread::sleep(delay().mul_f64(steps));
}

/// Refuse a directory for a simulated server's state that is, holds or lies
/// in berth's own configuration or data, or that holds anything but an
/// earlier simulation.
pub fn check_dir(dir: &Path, own: Option<&Paths>) -> Result<()> {
    let absolute = |path: &Path| {
        let path = std::path::absolute(path).unwrap_or_else(|_| path.to_owned());
        fs::canonicalize(&path).unwrap_or(path)
    };
    let dir = absolute(dir);
    for real in own
        .into_iter()
        .flat_map(|own| [&own.config_dir, &own.data_dir])
    {
        let real = absolute(real);
        if dir.starts_with(&real) || real.starts_with(&dir) {
            return Err(format!(
                "{} is berth's own configuration or data, or holds it ({}): a simulated server never uses it. Give another directory",
                dir.display(),
                real.display()
            )
            .into());
        }
    }
    let used = fs::read_dir(&dir).is_ok_and(|mut entries| entries.next().is_some());
    if used && !dir.join(MARKER).exists() {
        return Err(format!(
            "{} holds files of something else: a simulated server starts in an empty directory, or in one of an earlier simulation",
            dir.display()
        )
        .into());
    }
    Ok(())
}

// ── The world a test starts from ────────────────────────────────────────────

/// A known world to start from: a JSON file.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Seed {
    /// The token clients over HTTP show, at least 32 characters. One is made
    /// without it.
    #[serde(default)]
    pub token: Option<String>,
    /// Saved hosts by name.
    #[serde(default)]
    pub hosts: BTreeMap<String, SeedHost>,
    /// Registered provisioners by name, as `provisioners.save` takes them.
    /// Without any, `sim` is registered as `sim/cloud`.
    #[serde(default)]
    pub provisioners: BTreeMap<String, berth_api::SavedProvisioner>,
    /// Secrets by name.
    #[serde(default)]
    pub secrets: BTreeMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeedHost {
    /// Its profile. A host without an address gets one of 192.0.2.0/24.
    #[serde(default)]
    pub profile: Option<Value>,
    /// Whether it starts applied, at this berth's version.
    #[serde(default)]
    pub applied: bool,
}

impl Seed {
    pub fn read(file: &Path) -> Result<Seed> {
        let text = fs::read_to_string(file)
            .map_err(|error| format!("The seed {}: {error}", file.display()))?;
        Seed::parse(&text).map_err(|error| format!("The seed {}: {error}", file.display()).into())
    }

    pub fn parse(text: &str) -> Result<Seed> {
        let seed: Seed = serde_json::from_str(text)?;
        if let Some(token) = &seed.token
            && (token.trim().len() < 32 || token.trim().contains(char::is_whitespace))
        {
            return Err("token has at least 32 characters and no spaces".into());
        }
        for (name, host) in &seed.hosts {
            Place::own(name).map_err(|error| format!("hosts.{name}: {error}"))?;
            if host
                .profile
                .as_ref()
                .is_some_and(|profile| !profile.is_object())
            {
                return Err(format!("hosts.{name}.profile is an object").into());
            }
        }
        Ok(seed)
    }

    /// The profile of each host, with an address where it has none.
    pub fn profiles(&self) -> Vec<(String, Value, bool)> {
        self.hosts
            .iter()
            .enumerate()
            .map(|(index, (name, host))| {
                let mut profile = host.profile.clone().unwrap_or_else(|| json!({}));
                if profile["hostname"].as_str().is_none_or(str::is_empty) {
                    profile["hostname"] = json!(name);
                }
                // A workstation's profile has a builder, which may be empty.
                let workstation = profile["type"]
                    .as_str()
                    .is_none_or(|kind| kind == "workstation");
                if workstation && !profile["builder"].is_object() {
                    profile["builder"] = json!({});
                }
                if !profile["access"].is_object() {
                    profile["access"] = json!({});
                }
                let access = &mut profile["access"];
                if access["address"].as_str().is_none_or(str::is_empty) {
                    access["address"] = json!(format!("192.0.2.{}", 20 + index % 200));
                }
                if access["user"].as_str().is_none_or(str::is_empty) {
                    access["user"] = json!("ops");
                }
                (name.clone(), profile, host.applied)
            })
            .collect()
    }

    /// Put the world into the store, with the simulated plugin there already.
    pub fn plant(&self, paths: &Paths, simulation: &Simulation) -> Result<()> {
        let store = crate::store::shared(paths)?;
        if let Some(token) = &self.token {
            fs::create_dir_all(&paths.data_dir)?;
            write_private(
                &paths.data_dir.join("serve.token"),
                &format!("{}\n", token.trim()),
            )?;
        }
        let registries = crate::registry::Store::own(store.clone());
        for (name, value) in &self.secrets {
            registries.set_secret(name, Some(value))?;
        }
        let provisioners = match self.provisioners.is_empty() {
            true => BTreeMap::from([(
                REGISTERED.to_owned(),
                berth_api::SavedProvisioner {
                    kind: PROVISIONER.into(),
                    settings: None,
                },
            )]),
            false => self.provisioners.clone(),
        };
        for (name, saved) in &provisioners {
            crate::provisioner::register(paths, name, saved)
                .map_err(|error| format!("provisioners.{name}: {error}"))?;
        }
        for (name, profile, applied) in self.profiles() {
            let place = Place::own(&name)?;
            place.save(paths, &profile)?;
            if applied {
                let host = Host::load_at(paths, place)?;
                simulation
                    .record(paths, &host, level(&host))
                    .map_err(|error| format!("hosts.{name}: {error}"))?;
            }
        }
        Ok(())
    }
}

fn write_private(file: &Path, text: &str) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut opened = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(file)?;
    opened.write_all(text.as_bytes())?;
    Ok(())
}

/// What an apply of `host` records it as, as the role's `level`.
fn level(host: &Host) -> Option<&'static str> {
    match crate::host_type::of(&host.config) {
        HostType::Workstation => None,
        HostType::Node => Some("node"),
        HostType::Managed | HostType::Ssh => Some("controller"),
    }
}

// ── Hosts ───────────────────────────────────────────────────────────────────

/// In a host's folder while its machine is stopped or gone: its berthd
/// answers nothing then.
const STOPPED: &str = "stopped";

pub struct Simulation {
    /// Where it keeps what it simulates: the hosts are in `hosts/` of it.
    root: PathBuf,
    /// The berthd the hosts run, and their links reach them with.
    berthd: PathBuf,
    running: Mutex<BTreeMap<String, Child>>,
}

impl Simulation {
    /// Where the simulation of the installation at `paths` keeps its hosts
    /// and what its provisioner made.
    pub fn dir_of(paths: &Paths) -> PathBuf {
        paths.data_dir.join("simulated")
    }

    /// Run this process's simulation of the installation at `paths`, with
    /// the hosts' berthd copied from `berthd`. Only one can run.
    pub fn begin(paths: &Paths, berthd: &Path) -> Result<&'static Simulation> {
        let root = &Simulation::dir_of(paths);
        if !berthd.is_file() {
            return Err(
                format!("No berthd to run the simulated hosts: {}", berthd.display()).into(),
            );
        }
        // The source keeps it as a file to copy, which need not be executable.
        let program = root.join("bin/berthd");
        fs::create_dir_all(root.join("bin"))?;
        fs::copy(berthd, &program)?;
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&program, fs::Permissions::from_mode(0o755))?;
        }
        let simulation = Simulation {
            root: root.to_owned(),
            berthd: program,
            running: Mutex::default(),
        };
        ACTIVE
            .set(simulation)
            .map_err(|_| "A simulation runs in this process already")?;
        Ok(ACTIVE.get().expect("just set"))
    }

    fn dir(&self, name: &str) -> Result<PathBuf> {
        Place::own(name)?;
        Ok(self.root.join("hosts").join(name))
    }

    /// The link to the host's berthd, which is started when it does not run.
    /// Until the host is first applied, the link finds no berthd, as on a
    /// machine nothing of berth was installed on.
    pub fn link(&self, name: &str) -> Result<LinkSpec> {
        let dir = self.dir(name)?;
        self.ensure(name, &dir)?;
        let script = "[ -e \"$1/etc/applied.json\" ] || { echo 'sh: 1: berthd: not found' >&2; exit 127; }; exec \"$2\" connect --dir \"$1\"";
        Ok(LinkSpec {
            program: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                script.into(),
                "sh".into(),
                dir.to_string_lossy().into_owned(),
                self.berthd.to_string_lossy().into_owned(),
            ],
            name: name.to_owned(),
            ..LinkSpec::default()
        })
    }

    fn ensure(&self, name: &str, dir: &Path) -> Result<()> {
        let mut running = self.running.lock().unwrap();
        if let Some(child) = running.get_mut(name)
            && child.try_wait().ok().flatten().is_none()
        {
            return Ok(());
        }
        let (etc, home, bin) = (dir.join("etc"), dir.join("home"), dir.join("bin"));
        for made in [&etc, &home, &bin] {
            fs::create_dir_all(made)?;
        }
        let shell = bin.join("shell");
        fs::write(&shell, SHELL)?;
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&shell, fs::Permissions::from_mode(0o755))?;
        }
        let config = json!({
            "agent": "agent",
            "roots": [home],
            "agent_env": shell,
            "simulated": true,
        });
        fs::write(etc.join("berthd.json"), config.to_string())?;
        if !etc.join("actions.json").exists() {
            fs::write(etc.join("actions.json"), "[]")?;
        }
        let readme = home.join("README.md");
        if !readme.exists() {
            fs::write(
                &readme,
                format!("# {name}\n\nA simulated host of berth serve --simulate.\n"),
            )?;
        }
        let _ = fs::remove_file(dir.join(berth_api::CONTROL_SOCKET));
        let log = fs::File::create(dir.join("berthd.log"))?;
        let child = Command::new(&self.berthd)
            .arg("--single")
            .arg(dir)
            .current_dir(&home)
            .env("HOME", &home)
            .env("BERTH_SIM_HOST", name)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()
            .map_err(|error| format!("{}: {error}", self.berthd.display()))?;
        running.insert(name.to_owned(), child);
        for _ in 0..100 {
            if dir.join(berth_api::CONTROL_SOCKET).exists() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Err(format!(
            "The simulated berthd of {name} did not start: see {}",
            dir.join("berthd.log").display()
        )
        .into())
    }

    /// Tell the host's berthd to read its configuration again.
    fn reload(&self, name: &str) {
        if let Some(child) = self.running.lock().unwrap().get(name) {
            // SAFETY: a plain system call on a child of ours.
            unsafe { libc::kill(child.id() as i32, libc::SIGHUP) };
        }
    }

    /// End every host's berthd.
    pub fn stop(&self) {
        for (_, mut child) in std::mem::take(&mut *self.running.lock().unwrap()) {
            // SAFETY: a plain system call on a child of ours.
            unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
            let _ = child.wait();
        }
    }

    /// Apply the host's profile, with progress on `out`: steps that take
    /// the delay, then what the role records. Whether it went well.
    pub fn apply(&self, paths: &Paths, host: &Host, out: &mut dyn Write) -> bool {
        self.run(paths, host, level(host), true, out)
    }

    /// Install the controller alone.
    pub fn install_controller(&self, paths: &Paths, host: &Host, out: &mut dyn Write) -> bool {
        self.run(paths, host, Some("controller"), false, out)
    }

    fn run(
        &self,
        paths: &Paths,
        host: &Host,
        level: Option<&str>,
        whole: bool,
        out: &mut dyn Write,
    ) -> bool {
        let name = &host.name;
        if whole
            && let Err(error) = crate::host_type::guard(host, crate::host_type::Capability::Apply)
        {
            let _ = writeln!(out, "✗ {error}");
            return false;
        }
        let mut tasks = vec!["Install the base packages", "Install berthd and box"];
        if whole && level.is_none() {
            tasks.splice(1..1, ["Install the coding agents", "Configure the desktop"]);
        }
        let _ = writeln!(out, "▸ {name}: applying, simulated: nothing is installed");
        let _ = writeln!(out, "PLAY [{name}]");
        for task in &tasks {
            pause(0.5);
            let _ = writeln!(out, "TASK [{task}] changed: [{name}]");
        }
        pause(0.5);
        let _ = writeln!(out, "TASK [Record what was applied] changed: [{name}]");
        if let Err(error) = self.record(paths, host, level) {
            let _ = writeln!(out, "✗ {name}: {error}");
            return false;
        }
        let _ = writeln!(
            out,
            "PLAY RECAP {name} : ok={count} changed={count} failed=0",
            count = tasks.len() + 1
        );
        if let Err(error) = crate::apply::settle_with(paths, host, out) {
            let _ = writeln!(
                out,
                "! {name} is applied, but its profile was not updated: {error}"
            );
        }
        let _ = writeln!(out, "✓ {name} is applied");
        true
    }

    /// Write what the role leaves on a host it applied: the record of the
    /// berth and plugins, the profile and the actions.
    fn record(&self, paths: &Paths, host: &Host, level: Option<&str>) -> Result<()> {
        let name = &host.name;
        let etc = self.dir(name)?.join("etc");
        self.link(name)?;
        let source = paths.source()?;
        let plugins =
            crate::plugins::load(&[source.join("plugins"), paths.config_dir.join("plugins")]);
        let features: Vec<String> = crate::node::features(&host.config)
            .iter()
            .filter_map(|feature| feature.split('/').next().map(str::to_owned))
            .collect();
        let records: Vec<_> = crate::plugins::records(&plugins)
            .into_iter()
            .filter(|record| features.contains(&record.id))
            .collect();
        let mut applied = json!({"berth": berth_api::VERSION, "plugins": records});
        if let Some(level) = level {
            applied["level"] = json!(level);
        }
        fs::write(etc.join("applied.json"), format!("{applied:#}\n"))?;
        let mut kept = crate::kept_profile::for_host(paths, host);
        kept["written"] = json!(berth_api::now());
        write_private(&etc.join("profile.json"), &format!("{kept:#}\n"))?;
        fs::write(etc.join("actions.json"), actions(level).to_string())?;
        self.reload(name);
        Ok(())
    }
}

/// The actions a simulated host's controller lists once applied, which its
/// shell answers.
fn actions(level: Option<&str>) -> Value {
    let action = |name: &str, label: &str, group: &str, help: &str| json!({"name": name, "label": label, "group": group, "help": help, "run": name});
    let mut listed = vec![
        action("status", "Status", "Host", "How the simulated host is."),
        action(
            "update",
            "Update",
            "Sync",
            "Pretend to bring the packages up to date.",
        ),
    ];
    if level.is_none() {
        let mut smoke = action(
            "smoke",
            "Self-Test",
            "Host",
            "Simulated checks that all pass.",
        );
        smoke["agent"] = json!(true);
        listed.insert(1, smoke);
    }
    let mut reboot = action("reboot", "Reboot", "Power", "Pretend to restart the host.");
    reboot["confirm"] = json!("Restart the host?");
    listed.push(reboot);
    let mut secrets = action(
        "secrets",
        "Secrets",
        "Sync",
        "Take the host's secrets on standard input.",
    );
    secrets["input"] = json!("secrets");
    listed.push(secrets);
    let mut fail = action(
        "fail",
        "Fail",
        "Host",
        "Fail with exit code 3, to test failures.",
    );
    fail["arguments"] = json!([{"name": "message", "default": "simulated failure"}]);
    listed.push(fail);
    Value::Array(listed)
}

/// The shell of a simulated host: every command of a session or an action
/// runs through it, and it runs only the commands it knows itself.
pub const SHELL: &str = r#"#!/bin/sh
# The shell of a host of berth serve --simulate. It runs the few commands
# below itself and no program of the machine it is on.
host=${BERTH_SIM_HOST:-host}
cd "$HOME" 2>/dev/null || exit 1
# The session's own input, for the commands that read it.
exec 3<&0
known="echo hostname whoami id uname pwd date ls cat sleep true false exit help status smoke update reboot fail secrets"

inside() {
  case "$1" in /*|..|../*|*/..|*/../*) return 1 ;; esac
}

run() {
  case "$1" in
    echo) shift; printf '%s\n' "$*" ;;
    hostname) printf '%s\n' "$host" ;;
    whoami) echo agent ;;
    id) echo "uid=1000(agent) gid=1000(agent) groups=1000(agent)" ;;
    uname)
      case "$2" in
        -a) echo "Linux $host 6.8.0-simulated #1 SMP x86_64 GNU/Linux" ;;
        *) echo Linux ;;
      esac ;;
    pwd) echo /home/agent ;;
    date) date -u '+%Y-%m-%dT%H:%M:%SZ' ;;
    ls)
      shift
      [ $# -eq 0 ] && set -- .
      for path in "$@"; do
        inside "$path" || { echo "ls: $path: outside the home" >&2; return 2; }
        ls -1 -- "$path" || return 2
      done ;;
    cat)
      shift
      for path in "$@"; do
        inside "$path" || { echo "cat: $path: outside the home" >&2; return 1; }
        cat -- "$path" || return 1
      done ;;
    sleep)
      case "${2:-1}" in
        *[!0-9.]*|'') echo "sleep: ${2}: not a number" >&2; return 1 ;;
        *) sleep "${2:-1}" ;;
      esac ;;
    true) return 0 ;;
    false) return 1 ;;
    exit) exit "${2:-0}" ;;
    help) echo "A simulated host runs: $known" ;;
    status)
      echo "$host: simulated"
      echo "Provisioning: done"
      echo "berthd: running"
      echo "Disk: 12% of 80G used" ;;
    smoke)
      for check in graphics browser agents; do echo "✓ $check (simulated)"; done ;;
    update)
      echo "Reading package lists... done (simulated)"
      echo "0 upgraded, 0 newly installed. No reboot is needed." ;;
    reboot) echo "$host restarts (simulated): nothing stops" ;;
    fail) echo "${BERTH_ARG_MESSAGE:-simulated failure}" >&2; return 3 ;;
    secrets)
      count=$(wc -c <&3 | tr -d ' ')
      echo "Took $count bytes of secrets (simulated): nothing is installed" ;;
    *) echo "$1: not a command of this simulated host; help lists them" >&2; return 127 ;;
  esac
}

line() {
  set -f
  # shellcheck disable=SC2086
  set -- $1
  set +f
  [ $# -eq 0 ] && return 0
  run "$@"
}

script() {
  printf '%s\n' "$1" | tr ';' '\n' | {
    code=0
    while IFS= read -r each; do
      line "$each"
      code=$?
    done
    exit $code
  }
}

interactive() {
  while :; do
    printf 'agent@%s:~$ ' "$host"
    IFS= read -r typed || { echo; exit 0; }
    line "$typed"
  done
}

case "$1" in
  sh|bash|zsh|*/sh|*/bash|*/zsh)
    case "$2" in
      -c) script "$3" ;;
      *) interactive ;;
    esac ;;
  '') interactive ;;
  *) run "$@" ;;
esac
"#;

// ── The simulated provisioner ───────────────────────────────────────────────

/// The plugin that holds the simulated provisioner, written into the
/// server's own plugins. Its command runs `berth` at `program` again.
pub fn write_plugin(paths: &Paths, program: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    // Opened first, so the store takes the plugin as one added now.
    crate::store::shared(paths)?;
    let dir = crate::plugins::own_dir(&paths.config_dir).join(REGISTERED);
    fs::create_dir_all(dir.join("bin"))?;
    let quoted = program.to_string_lossy().replace('\'', r"'\''");
    let script = dir.join("bin/provision");
    fs::write(
        &script,
        format!("#!/bin/sh\nexec '{quoted}' simulated-provisioner \"$@\"\n"),
    )?;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755))?;
    let mut manifest = manifest();
    manifest["version"] = json!(berth_api::VERSION);
    fs::write(dir.join("package.json"), format!("{manifest:#}\n"))?;
    crate::plugin_state::remember(paths)
}

fn manifest() -> Value {
    json!({
        "name": "berth-plugin-sim",
        "displayName": "Simulated",
        "description": "Machines and services of berth serve --simulate, which make nothing.",
        "files": ["bin"],
        "berth": {"provisioners": [{
            "id": "cloud",
            "label": "Simulated cloud",
            "description": "Makes simulated machines for hosts and simulated services, with addresses of 192.0.2.0/24.",
            "command": "bin/provision",
            "protocol": 2,
            "verbs": ["list"],
            "arguments": [{"name": "region", "label": "Region", "default": "sim-1", "values": ["sim-1", "sim-2"]}],
            "kinds": [
                {"id": "machine", "label": "Simulated machine", "setup": "apply",
                 "verbs": ["start", "stop", "logs"],
                 "arguments": [
                     {"name": "cpus", "label": "CPUs", "default": "2", "kind": "resource"},
                     {"name": "memory", "label": "Memory", "default": "4GiB", "kind": "resource"}]},
                {"id": "service", "label": "Simulated service",
                 "verbs": ["start", "stop", "change", "logs", "exec"],
                 "spec": {"format": "text", "label": "Spec"},
                 "arguments": [{"name": "image", "label": "Image", "default": "web:1"}],
                 "actions": [{"name": "restart", "label": "Restart", "confirm": "Restart the service?"}]}
            ],
            "actions": [
                {"name": "images", "label": "Images", "changes": false},
                {"name": "inspect", "label": "Inspect", "changes": false,
                 "arguments": [{"name": "image", "required": true}]}
            ],
            "collections": [{
                "id": "images", "label": "Image", "plural": "Images",
                "key": ["image"],
                "columns": [{"name": "size", "label": "Size"}],
                "item": [{"action": "inspect"}],
                "show": "images"
            }]
        }]}
    })
}

/// The images the simulated cloud has, with their sizes.
const IMAGES: &[(&str, &str)] = &[
    ("ubuntu-26.04", "2.1 GiB"),
    ("web:1", "180 MiB"),
    ("db:16", "420 MiB"),
];

/// What the simulated provisioner made, kept in the scratch data.
#[derive(Debug, Default, serde::Serialize, Deserialize)]
struct Made {
    #[serde(default)]
    next: u32,
    #[serde(default)]
    resources: BTreeMap<String, Value>,
}

/// Run one verb of the simulated provisioner: the request on `input`, the
/// answer returned, progress on `progress`. The answer of `logs` is the log
/// as text.
pub fn provision(verb: &str, input: &str, progress: &mut dyn Write) -> Result<Value> {
    let request: Value = serde_json::from_str(input)?;
    let paths = Paths::locate()?;
    let dir = Simulation::dir_of(&paths);
    fs::create_dir_all(&dir)?;
    let lock = fs::File::create(dir.join("cloud.lock"))?;
    use std::os::fd::AsRawFd;
    // SAFETY: the descriptor is open; the lock goes when the file is closed.
    unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) };
    let file = dir.join("cloud.json");
    let mut made: Made = fs::read(&file)
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default();
    let answer = answer(verb, &request, &mut made, progress, delay())?;
    fs::write(&file, serde_json::to_vec_pretty(&made)?)?;
    // A host whose machine does not run does not answer.
    if request["kind"] == "machine"
        && let Some(host) = request["host"]
            .as_str()
            .filter(|host| Place::own(host).is_ok())
    {
        let stopped = dir.join("hosts").join(host).join(STOPPED);
        match verb {
            "stop" | "destroy" => {
                fs::create_dir_all(dir.join("hosts").join(host))?;
                fs::write(stopped, "")?;
            }
            "start" | "create" => {
                let _ = fs::remove_file(stopped);
            }
            _ => {}
        }
    }
    Ok(answer)
}

fn say(progress: &mut dyn Write, line: &str) {
    let _ = writeln!(progress, "{line}");
    let _ = progress.flush();
}

fn endpoints(kind: &str, address: &str) -> Value {
    match kind {
        "machine" => json!([{"name": "ssh", "address": address, "port": 22, "hint": "ssh"}]),
        _ => json!([{"name": "http", "address": address, "port": 80, "hint": "http"}]),
    }
}

fn answer(
    verb: &str,
    request: &Value,
    made: &mut Made,
    progress: &mut dyn Write,
    delay: Duration,
) -> Result<Value> {
    let pause = |steps: f64| std::thread::sleep(delay.mul_f64(steps));
    let id = request["id"].as_str().unwrap_or("").to_owned();
    let kind = request["kind"].as_str().unwrap_or("machine").to_owned();
    let found = |made: &Made| -> Result<Value> {
        made.resources
            .get(&id)
            .cloned()
            .ok_or_else(|| format!("No simulated resource {id}").into())
    };
    let set = |made: &mut Made, state: &str| -> Result<Value> {
        let mut resource = found(made)?;
        resource["state"] = json!(state);
        made.resources.insert(id.clone(), resource.clone());
        Ok(resource)
    };
    Ok(match verb {
        "status" if id.is_empty() => json!({
            "state": "ready",
            "message": "Simulated: nothing is made",
            "details": [
                {"label": "Region", "value": request["arguments"]["region"].as_str().unwrap_or("sim-1")},
                {"label": "Resources", "value": made.resources.len().to_string()}
            ],
            "values": {"region": ["sim-1", "sim-2"]},
            "collections": {"images": IMAGES.iter().map(|(name, size)| {
                json!({"name": name, "columns": {"size": size}})
            }).collect::<Vec<_>>()}
        }),
        "status" => match made.resources.get(&id) {
            Some(resource) => json!({
                "state": resource["state"],
                "address": resource["address"],
                "endpoints": endpoints(&kind, resource["address"].as_str().unwrap_or("")),
            }),
            None => json!({"state": "gone"}),
        },
        "create" => {
            // Asked again with its token, it is the one made before.
            let token = request["token"].as_str().unwrap_or("");
            if let Some((id, resource)) = made
                .resources
                .iter()
                .find(|(_, resource)| resource["token"] == token)
            {
                return Ok(json!({"id": id, "address": resource["address"], "port": 22}));
            }
            say(progress, &format!("Allocating a simulated {kind}"));
            pause(1.0);
            made.next += 1;
            let id = format!("sim-{}", made.next);
            let address = format!("192.0.2.{}", 100 + made.next % 150);
            say(progress, &format!("Starting {id} at {address}"));
            pause(1.0);
            made.resources.insert(
                id.clone(),
                json!({
                    "kind": kind,
                    "name": request["name"],
                    "host": request["host"],
                    "token": token,
                    "state": "running",
                    "address": address,
                    "arguments": request["arguments"],
                }),
            );
            say(progress, &format!("{id} is running"));
            match kind.as_str() {
                "machine" => json!({"id": id, "address": address, "port": 22}),
                _ => json!({"id": id, "endpoints": endpoints(&kind, &address)}),
            }
        }
        "start" | "stop" => {
            let state = if verb == "start" {
                "running"
            } else {
                "stopped"
            };
            say(
                progress,
                &format!(
                    "{}ing {id}",
                    if verb == "start" { "Start" } else { "Stopp" }
                ),
            );
            pause(1.0);
            let resource = set(made, state)?;
            json!({"state": state, "endpoints": endpoints(&kind, resource["address"].as_str().unwrap_or(""))})
        }
        "change" => {
            say(progress, &format!("Changing {id}"));
            pause(1.0);
            let mut resource = found(made)?;
            resource["arguments"] = request["arguments"].clone();
            made.resources.insert(id.clone(), resource.clone());
            json!({"state": resource["state"]})
        }
        "destroy" => {
            say(progress, &format!("Removing {id}"));
            pause(1.0);
            made.resources.remove(&id);
            json!({})
        }
        // The log is the answer itself, as text.
        "logs" => {
            let resource = found(made).unwrap_or_default();
            let state = resource["state"].as_str().unwrap_or("gone");
            json!(format!("{id}: simulated boot\n{id}: {state}\n"))
        }
        "exec" => {
            if found(made)?["state"] != "running" {
                return Err(format!("{id} is not running").into());
            }
            let words: Vec<&str> = request["command"]
                .as_array()
                .map(|words| words.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            match words.as_slice() {
                ["echo", rest @ ..] => say(progress, &rest.join(" ")),
                ["hostname"] => say(progress, &id),
                ["false"] => return Ok(json!({"exit_code": 1})),
                ["true"] => {}
                other => {
                    say(
                        progress,
                        &format!("{}: not a command of a simulated service", other.join(" ")),
                    );
                    return Ok(json!({"exit_code": 127}));
                }
            }
            json!({"exit_code": 0})
        }
        "list" => json!({"resources": made.resources.iter().map(|(id, resource)| json!({
            "id": id,
            "kind": resource["kind"],
            "name": resource["name"],
            "host": resource["host"],
            "state": resource["state"],
        })).collect::<Vec<_>>()}),
        "act" => match request["action"]["name"].as_str().unwrap_or("") {
            "images" => json!({
                "message": format!("{} images", IMAGES.len()),
                "details": IMAGES.iter().map(|(name, size)| json!({"label": name, "value": size})).collect::<Vec<_>>(),
            }),
            "inspect" => {
                let wanted = request["action"]["arguments"]["image"]
                    .as_str()
                    .unwrap_or("");
                match IMAGES.iter().find(|(name, _)| *name == wanted) {
                    Some((name, size)) => json!({"message": format!("{name}: {size}")}),
                    None => return Err(format!("No image {wanted}").into()),
                }
            }
            "restart" => {
                say(progress, &format!("Restarting {id}"));
                pause(1.0);
                set(made, "running")?;
                json!({"message": format!("{id} restarted")})
            }
            other => return Err(format!("No action {other}").into()),
        },
        other => return Err(format!("The simulated provisioner has no verb {other}").into()),
    })
}

// ── What a provision waits for ──────────────────────────────────────────────

/// How a new machine of the simulated provisioner is waited for and set up:
/// it answers at once and is applied by the simulation.
pub fn options() -> crate::provision::Options {
    crate::provision::Options {
        every: (delay() / 4).max(Duration::from_millis(20)),
        boot: Duration::from_secs(60),
        ssh: Duration::from_secs(60),
        reaches: |_, _| true,
        answers: |_, _| true,
        first_boot: |_, _, _| Ok(String::new()),
        finish: |paths, host, setup, out| {
            let simulation = active().ok_or("No simulation runs")?;
            let done = match setup {
                Setup::Apply => simulation.apply(paths, host, out),
                Setup::Seed => simulation.record(paths, host, level(host)).is_ok(),
            };
            match done {
                true => Ok(()),
                false => Err(format!("Applying {} failed", host.name).into()),
            }
        },
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seed_names_hosts_provisioners_and_secrets() {
        let seed = Seed::parse(
            r#"{"token": "0123456789abcdef0123456789abcdef",
                "hosts": {"studio": {"applied": true},
                          "dock": {"profile": {"type": "node", "access": {"address": "192.0.2.7", "user": "admin"}}}},
                "provisioners": {"cloud": {"type": "sim/cloud", "settings": {"region": "sim-2"}}},
                "secrets": {"api_token": "made-up"}}"#,
        )
        .unwrap();
        let profiles = seed.profiles();
        let (name, dock, applied) = &profiles[0];
        assert_eq!((name.as_str(), *applied), ("dock", false));
        assert_eq!(
            dock["access"],
            json!({"address": "192.0.2.7", "user": "admin"})
        );
        assert_eq!(dock["hostname"], "dock");
        let (name, studio, applied) = &profiles[1];
        assert_eq!((name.as_str(), *applied), ("studio", true));
        assert_eq!(studio["access"]["address"], "192.0.2.21");
        assert_eq!(seed.provisioners["cloud"].kind, "sim/cloud");
        assert_eq!(seed.secrets["api_token"], "made-up");
    }

    #[test]
    fn a_seed_that_is_wrong_says_where() {
        let wrong = |text: &str| Seed::parse(text).unwrap_err().to_string();
        assert!(wrong(r#"{"token": "short"}"#).contains("32 characters"));
        assert!(wrong(r#"{"hosts": {"a/b": {}}}"#).contains("hosts.a/b"));
        assert!(wrong(r#"{"hosts": {"studio": {"profile": []}}}"#).contains("is an object"));
        assert!(wrong(r#"{"host": {}}"#).contains("unknown field"));
        assert!(Seed::parse("{}").unwrap().hosts.is_empty());
    }

    #[test]
    fn berths_own_directories_and_others_in_use_are_refused() {
        let scratch = tempfile::tempdir().unwrap();
        let own = Paths {
            config_dir: scratch.path().join("home/.config/berth"),
            data_dir: scratch.path().join("home/.local/share/berth"),
        };
        fs::create_dir_all(&own.config_dir).unwrap();
        let refused = |dir: &Path| check_dir(dir, Some(&own)).unwrap_err().to_string();
        assert!(refused(&own.config_dir).contains("berth's own"));
        assert!(refused(&own.data_dir.join("sim")).contains("berth's own"));
        assert!(refused(&scratch.path().join("home")).contains("berth's own"));

        let other = scratch.path().join("other");
        fs::create_dir_all(&other).unwrap();
        check_dir(&other, Some(&own)).unwrap();
        check_dir(&scratch.path().join("new"), Some(&own)).unwrap();
        fs::write(other.join("notes.txt"), "mine").unwrap();
        assert!(refused(&other).contains("something else"));
        fs::write(other.join(MARKER), "").unwrap();
        check_dir(&other, Some(&own)).unwrap();
    }

    #[test]
    fn the_provisioner_moves_what_it_made_through_its_states() {
        let mut made = Made::default();
        let mut progress = vec![];
        let mut ask = |verb: &str, request: Value| {
            answer(verb, &request, &mut made, &mut progress, Duration::ZERO)
        };
        let created = ask(
            "create",
            json!({"kind": "service", "name": "web", "token": "t1"}),
        )
        .unwrap();
        let id = created["id"].as_str().unwrap().to_owned();
        assert_eq!(created["endpoints"][0]["port"], 80);
        assert!(
            created["endpoints"][0]["address"]
                .as_str()
                .unwrap()
                .starts_with("192.0.2.")
        );
        let again = ask("create", json!({"kind": "service", "token": "t1"})).unwrap();
        assert_eq!(again["id"], id.as_str());
        let stopped = ask("stop", json!({"kind": "service", "id": id})).unwrap();
        assert_eq!(stopped["state"], "stopped");
        assert_eq!(
            ask("status", json!({"kind": "service", "id": id})).unwrap()["state"],
            "stopped"
        );
        assert!(ask("exec", json!({"id": id, "command": ["echo", "hi"]})).is_err());
        ask("start", json!({"kind": "service", "id": id})).unwrap();
        let ran = ask("exec", json!({"id": id, "command": ["echo", "hi"]})).unwrap();
        assert_eq!(ran["exit_code"], 0);
        let listed = ask("list", json!({})).unwrap();
        assert_eq!(listed["resources"][0]["state"], "running");
        let status = ask("status", json!({})).unwrap();
        assert_eq!(
            status["collections"]["images"].as_array().unwrap().len(),
            IMAGES.len()
        );
        ask("destroy", json!({"id": id})).unwrap();
        assert_eq!(ask("status", json!({"id": id})).unwrap()["state"], "gone");
        let said = String::from_utf8(progress).unwrap();
        assert!(said.contains("hi") && said.contains("Removing"), "{said}");
    }
}
