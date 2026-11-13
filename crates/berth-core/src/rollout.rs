//! Bringing hosts up to date one after another: apply each configuration,
//! maybe run its update and reboot it, and check that it is healthy before
//! going on. The first host that fails stops the roll-out, and the hosts
//! after it are left as they are.

use crate::{Result, apply, host::Host, link::Link, paths::Paths, plugins};
use berth_api::{
    self as api, Applied, Behind, Rollout, RolloutHost, RolloutReport, RolloutState, Version,
};
use serde_json::{Value, json};
use std::io::{BufRead, Write};

/// How long a rebooted host has to answer again.
const REBOOT_SECONDS: u64 = 600;
/// How often a rebooting host is asked whether it is back.
const REBOOT_EVERY: u64 = 5;

/// One host, as the roll-out uses it. A test has its own.
pub trait Target {
    fn name(&self) -> &str;
    /// What its berthd says; `None` when it has no berthd yet.
    fn version(&mut self) -> std::result::Result<Option<Version>, String>;
    /// Apply its configuration, its output going to `out`.
    fn apply(&mut self, out: &mut dyn Write) -> std::result::Result<(), String>;
    /// Run one of its tasks to the end, with its output to `out`: the exit code.
    fn task(&mut self, name: &str, out: &mut dyn Write) -> std::result::Result<i32, String>;
    /// Seconds it has been up, and whether it says it needs a reboot.
    fn uptime(&mut self) -> std::result::Result<(u64, bool), String>;
    /// How many sessions run there.
    fn sessions(&mut self) -> std::result::Result<usize, String>;
    fn reboot(&mut self, force: bool) -> std::result::Result<(), String>;
    /// Wait this long before asking again.
    fn pause(&mut self, seconds: u64);
}

/// What a host that says what it was applied from is: current, or behind.
/// A host that does not say is taken as behind, which applying settles.
pub fn seen(version: &Option<Version>, behind: &dyn Fn(&Applied) -> Option<Behind>) -> Seen {
    match version
        .as_ref()
        .and_then(|version| version.applied.as_ref())
    {
        None => Seen::Unknown,
        Some(applied) => match behind(applied) {
            None => Seen::Current,
            Some(behind) => Seen::Behind(behind),
        },
    }
}

#[derive(Debug, PartialEq)]
pub enum Seen {
    /// It does not say what it was applied from.
    Unknown,
    Current,
    Behind(Behind),
}

/// A line of the roll-out itself, among the output of what it runs. They
/// start with ▸, ✓, ! or ✗, which tells them apart.
fn say(out: &mut dyn Write, line: &str) {
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// Roll out to the targets in the order given. `behind` compares what a
/// host was applied from with what would be applied now.
pub fn run(
    targets: &mut [Box<dyn Target + '_>],
    options: &Rollout,
    behind: &dyn Fn(&Applied) -> Option<Behind>,
    out: &mut dyn Write,
) -> RolloutReport {
    let mut hosts = vec![];
    let mut stopped = false;
    for target in targets.iter_mut() {
        let mut host = RolloutHost {
            name: target.name().to_owned(),
            ..Default::default()
        };
        if stopped {
            hosts.push(host);
            continue;
        }
        let failed = one(target.as_mut(), options, behind, &mut host, out);
        if let Err(error) = failed {
            say(out, &format!("✗ {}: {error}", host.name));
            host.state = RolloutState::Failed;
            host.error = error;
            stopped = true;
        } else if !host.warnings.is_empty() {
            say(
                out,
                &format!(
                    "! {}: {}. The roll-out stops here.",
                    host.name,
                    host.warnings.join("; ")
                ),
            );
            stopped = true;
        }
        hosts.push(host);
    }
    let skipped: Vec<&str> = hosts
        .iter()
        .filter(|host| host.state == RolloutState::Skipped)
        .map(|host| host.name.as_str())
        .collect();
    if !skipped.is_empty() {
        say(out, &format!("! Not started: {}", skipped.join(", ")));
    }
    let ok = !stopped;
    RolloutReport { hosts, ok }
}

fn one(
    target: &mut dyn Target,
    options: &Rollout,
    behind: &dyn Fn(&Applied) -> Option<Behind>,
    host: &mut RolloutHost,
    out: &mut dyn Write,
) -> std::result::Result<(), String> {
    let name = host.name.clone();
    let version = target
        .version()
        .map_err(|error| format!("not reachable: {error}"))?;
    match seen(&version, behind) {
        Seen::Current if !options.all => {
            host.state = RolloutState::Current;
            say(out, &format!("✓ {name} is current"));
            return Ok(());
        }
        Seen::Behind(behind) => {
            say(out, &format!("▸ {name}: applying ({})", behind.text()));
            host.changes = behind.changes;
        }
        _ => say(out, &format!("▸ {name}: applying")),
    }
    target.apply(out)?;
    if options.update {
        say(out, &format!("▸ {name}: update"));
        match target.task("update", out) {
            Ok(0) => {}
            Ok(code) => host.warnings.push(format!("update ended with {code}")),
            Err(error) => host.warnings.push(format!("update did not run: {error}")),
        }
    }
    if options.reboot {
        let (up, needed) = target.uptime()?;
        if needed {
            let running = target.sessions()?;
            if running > 0 && !options.force {
                return Err(format!(
                    "needs a reboot, but {running} session{} running there; force reboots anyway and ends them",
                    if running == 1 { " is" } else { "s are" }
                ));
            }
            say(out, &format!("▸ {name}: rebooting"));
            target.reboot(options.force)?;
            come_back(target, up)?;
            host.rebooted = true;
        }
    }
    say(out, &format!("▸ {name}: checking"));
    let version = target
        .version()
        .map_err(|error| format!("berthd does not answer after applying: {error}"))?
        .ok_or("berthd is not there after applying")?;
    if version.version != api::VERSION {
        return Err(format!(
            "berthd answers as {}, not {} as applied",
            version.version,
            api::VERSION
        ));
    }
    match seen(&Some(version), behind) {
        Seen::Current => {}
        Seen::Unknown => return Err("it does not say what it was applied from".into()),
        Seen::Behind(behind) => {
            return Err(format!("still behind after applying: {}", behind.text()));
        }
    }
    match target.task("status", out)? {
        0 => {}
        code => return Err(format!("its status task ended with {code}")),
    }
    host.state = RolloutState::Done;
    say(out, &format!("✓ {name} is up to date"));
    Ok(())
}

/// Wait until the host has been up for less time than before the reboot.
fn come_back(target: &mut dyn Target, before: u64) -> std::result::Result<(), String> {
    let mut waited = 0;
    while waited < REBOOT_SECONDS {
        target.pause(REBOOT_EVERY);
        waited += REBOOT_EVERY;
        if let Ok((up, _)) = target.uptime()
            && up < before
        {
            return Ok(());
        }
    }
    Err(format!(
        "did not come back within {} minutes of the reboot",
        REBOOT_SECONDS / 60
    ))
}

/// The hosts a roll-out takes, by name: those asked for in that order, or all
/// saved ones sorted by name.
pub fn chosen(all: Vec<String>, asked: &[String]) -> Result<Vec<String>> {
    if asked.is_empty() {
        let mut all = all;
        all.sort();
        return Ok(all);
    }
    for name in asked {
        if !all.contains(name) {
            return Err(format!("No host {name}").into());
        }
    }
    Ok(asked.to_vec())
}

/// How this berth compares a host with what it would apply now.
pub fn comparer(paths: &Paths) -> impl Fn(&Applied) -> Option<Behind> + use<> {
    let found = match paths.source() {
        Ok(source) => plugins::found(&source, &plugins::own_dir(&paths.config_dir)),
        Err(_) => plugins::load(&[plugins::own_dir(&paths.config_dir)]),
    };
    let now = plugins::records(&found);
    move |applied: &Applied| Behind::compare(applied, api::VERSION, &now)
}

// ── A real host ─────────────────────────────────────────────────────────────

/// A saved host, applied from here over SSH and asked through its berthd.
pub struct Live<'a> {
    paths: &'a Paths,
    host: Host,
    password: Option<String>,
    link: Link,
    runtime: tokio::runtime::Runtime,
}

impl<'a> Live<'a> {
    pub fn new(paths: &'a Paths, host: Host, password: Option<String>) -> Result<Live<'a>> {
        let link = Link::new(host.link(paths, password.as_deref())?);
        Ok(Live {
            paths,
            host,
            password,
            link,
            runtime: crate::link::runtime()?,
        })
    }

    fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> std::result::Result<T, String> {
        self.runtime
            .block_on(self.link.get(path))
            .map_err(|error| error.to_string())
    }

    fn post<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &Value,
    ) -> std::result::Result<T, String> {
        self.runtime
            .block_on(self.link.post(path, body))
            .map_err(|error| error.to_string())
    }
}

impl Target for Live<'_> {
    fn name(&self) -> &str {
        &self.host.name
    }

    fn version(&mut self) -> std::result::Result<Option<Version>, String> {
        match self
            .runtime
            .block_on(self.link.get::<Version>("/v1/version"))
        {
            Ok(version) => Ok(Some(version)),
            Err(error) if Link::missing(&error) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    fn apply(&mut self, out: &mut dyn Write) -> std::result::Result<(), String> {
        if self.host.asks_password() && self.password.is_none() {
            return Err(
                "its SSH user's sudo asks for a password: set BERTH_SUDO_PASSWORD, or apply it by itself"
                    .into(),
            );
        }
        let mut prepared = apply::prepare(self.paths, &self.host, self.password.as_deref())
            .map_err(|error| error.to_string())?;
        let (reader, writer) = std::io::pipe().map_err(|error| error.to_string())?;
        let copy = writer.try_clone().map_err(|error| error.to_string())?;
        let mut child = prepared
            .command
            .stdin(std::process::Stdio::null())
            .stdout(writer)
            .stderr(copy)
            .spawn()
            .map_err(|error| format!("ansible-playbook: {error}"))?;
        // The command holds the pipe's other ends; they close when it is dropped.
        drop(prepared.command);
        for line in std::io::BufReader::new(reader).lines() {
            let Ok(line) = line else { break };
            let _ = writeln!(out, "{line}");
        }
        let status = child.wait().map_err(|error| error.to_string())?;
        drop(prepared.files);
        match status.success() {
            true => Ok(()),
            false => Err(format!(
                "applying failed (ansible-playbook ended with {})",
                status.code().unwrap_or(-1)
            )),
        }
    }

    fn task(&mut self, name: &str, out: &mut dyn Write) -> std::result::Result<i32, String> {
        let path = format!("/v1/tasks/{}?wait=10m", api::client::encode(name));
        let started: api::Started = self.post(&path, &json!({}))?;
        let id = started.session.id.clone();
        let mut session = started.session;
        // A task that takes longer than the wait is asked after again.
        while session.running() {
            self.pause(REBOOT_EVERY);
            session = self.get(&format!("/v1/sessions/{id}"))?;
        }
        let output: api::Output = self.get(&format!("/v1/sessions/{id}/output"))?;
        if let Some(bytes) = api::unb64(&output.data_b64) {
            let _ = out.write_all(&bytes);
            let _ = out.flush();
        }
        Ok(session.exit_code.unwrap_or(-1))
    }

    fn uptime(&mut self) -> std::result::Result<(u64, bool), String> {
        let stats: api::Stats = self.get("/v1/stats")?;
        Ok((stats.uptime, stats.reboot_required))
    }

    fn sessions(&mut self) -> std::result::Result<usize, String> {
        // A list, or one under `sessions`.
        let mut listed: Value = self.get("/v1/sessions")?;
        if let Some(inner) = listed.get_mut("sessions") {
            listed = inner.take();
        }
        let sessions: Vec<api::Session> =
            serde_json::from_value(listed).map_err(|error| error.to_string())?;
        Ok(sessions.iter().filter(|session| session.running()).count())
    }

    fn reboot(&mut self, force: bool) -> std::result::Result<(), String> {
        let path = format!("/v1/power/reboot?force={force}");
        self.post::<Value>(&path, &json!({})).map(|_| ())
    }

    fn pause(&mut self, seconds: u64) {
        std::thread::sleep(std::time::Duration::from_secs(seconds));
    }
}

/// Roll out to this machine's saved hosts, applying from here. Passwords for
/// hosts that ask come from `password`.
pub fn local(
    paths: &Paths,
    options: &Rollout,
    password: &dyn Fn(&Host) -> Option<String>,
    out: &mut dyn Write,
) -> Result<RolloutReport> {
    let all: Vec<String> = Host::all(paths).into_iter().map(|host| host.name).collect();
    let names = chosen(all, &options.hosts)?;
    let mut targets: Vec<Box<dyn Target + '_>> = vec![];
    for name in names {
        let host = Host::load(paths, &name)?;
        let password = password(&host);
        targets.push(Box::new(Live::new(paths, host, password)?));
    }
    let compare = comparer(paths);
    Ok(run(&mut targets, options, &compare, out))
}

/// The saved hosts of this machine that are behind, asked all at once. A
/// host that cannot be asked, or does not say, is left out.
pub fn survey(paths: &Paths) -> Vec<api::HostBehind> {
    let compare = comparer(paths);
    let Ok(runtime) = crate::link::runtime() else {
        return vec![];
    };
    let hosts = Host::all(paths);
    let asked = runtime.block_on(futures_util::future::join_all(hosts.iter().map(
        |host| async move {
            let password = std::env::var("BERTH_SUDO_PASSWORD").ok();
            let link = Link::new(host.link(paths, password.as_deref()).ok()?);
            let version = tokio::time::timeout(
                std::time::Duration::from_secs(20),
                link.get::<Version>("/v1/version"),
            )
            .await
            .ok()?
            .ok()?;
            Some((host.name.clone(), version))
        },
    )));
    asked
        .into_iter()
        .flatten()
        .filter_map(|(name, version)| match seen(&Some(version), &compare) {
            Seen::Behind(behind) => Some(api::HostBehind { name, behind }),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use api::{AppliedPlugin, Change};

    /// A host that does what it is told it does, and writes down what it was asked.
    #[derive(Default)]
    struct Fake {
        name: String,
        applied: Option<Applied>,
        /// What applying records.
        records: Applied,
        apply_fails: bool,
        update_code: i32,
        status_code: i32,
        reboot_required: bool,
        sessions: usize,
        /// Uptime answers after a reboot, until it is back.
        comes_back: bool,
        rebooting: bool,
        uptime: u64,
        log: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
    }

    impl Target for Fake {
        fn name(&self) -> &str {
            &self.name
        }
        fn version(&mut self) -> std::result::Result<Option<Version>, String> {
            Ok(Some(Version {
                version: api::VERSION.into(),
                api: api::API,
                mode: "system".into(),
                applied: self.applied.clone(),
            }))
        }
        fn apply(&mut self, _out: &mut dyn Write) -> std::result::Result<(), String> {
            self.log.borrow_mut().push(format!("{} apply", self.name));
            match self.apply_fails {
                true => Err("applying failed".into()),
                false => {
                    self.applied = Some(self.records.clone());
                    Ok(())
                }
            }
        }
        fn task(&mut self, name: &str, _out: &mut dyn Write) -> std::result::Result<i32, String> {
            self.log.borrow_mut().push(format!("{} {name}", self.name));
            Ok(match name {
                "update" => self.update_code,
                _ => self.status_code,
            })
        }
        fn uptime(&mut self) -> std::result::Result<(u64, bool), String> {
            if self.rebooting {
                if !self.comes_back {
                    return Err("unreachable".into());
                }
                self.rebooting = false;
                self.uptime = 5;
                self.reboot_required = false;
            }
            Ok((self.uptime, self.reboot_required))
        }
        fn sessions(&mut self) -> std::result::Result<usize, String> {
            Ok(self.sessions)
        }
        fn reboot(&mut self, force: bool) -> std::result::Result<(), String> {
            self.log.borrow_mut().push(format!(
                "{} reboot{}",
                self.name,
                if force { " forced" } else { "" }
            ));
            self.rebooting = true;
            Ok(())
        }
        fn pause(&mut self, _seconds: u64) {}
    }

    fn record(version: &str) -> Applied {
        Applied {
            berth: api::VERSION.into(),
            plugins: vec![AppliedPlugin {
                id: "x".into(),
                package: "berth-plugin-x".into(),
                version: version.into(),
                digest: version.into(),
            }],
        }
    }

    fn fleet(
        log: &std::rc::Rc<std::cell::RefCell<Vec<String>>>,
        hosts: &[(&str, &str)],
    ) -> Vec<Box<dyn Target>> {
        hosts
            .iter()
            .map(|(name, version)| {
                Box::new(Fake {
                    name: (*name).into(),
                    applied: Some(record(version)),
                    records: record("2"),
                    uptime: 1000,
                    log: log.clone(),
                    ..Default::default()
                }) as Box<dyn Target>
            })
            .collect()
    }

    fn now(applied: &Applied) -> Option<Behind> {
        Behind::compare(applied, api::VERSION, &record("2").plugins)
    }

    fn states(report: &RolloutReport) -> Vec<(&str, RolloutState)> {
        report
            .hosts
            .iter()
            .map(|host| (host.name.as_str(), host.state))
            .collect()
    }

    #[test]
    fn hosts_go_in_turn_and_current_ones_are_left() {
        let log = Default::default();
        let mut targets = fleet(&log, &[("a", "1"), ("b", "2"), ("c", "1")]);
        let mut out = vec![];
        let report = run(&mut targets, &Rollout::default(), &now, &mut out);
        assert!(report.ok);
        assert_eq!(
            states(&report),
            [
                ("a", RolloutState::Done),
                ("b", RolloutState::Current),
                ("c", RolloutState::Done)
            ]
        );
        assert_eq!(
            report.hosts[0].changes,
            [Change {
                id: "x".into(),
                from: "1".into(),
                to: "2".into()
            }]
        );
        assert_eq!(
            *log.borrow(),
            ["a apply", "a status", "c apply", "c status"]
        );
        let said = String::from_utf8(out).unwrap();
        assert!(said.contains("▸ a: applying (x 1 → 2)") && said.contains("✓ b is current"));
        assert!(said.lines().all(api::rollout_line));
    }

    #[test]
    fn the_first_failure_stops_the_rest() {
        let log = Default::default();
        let mut targets = fleet(&log, &[("a", "1"), ("b", "1"), ("c", "1")]);
        // b fails applying; c is not touched.
        let failing = Fake {
            name: "b".into(),
            applied: Some(record("1")),
            records: record("2"),
            apply_fails: true,
            log: log.clone(),
            ..Default::default()
        };
        targets[1] = Box::new(failing);
        let report = run(&mut targets, &Rollout::default(), &now, &mut vec![]);
        assert!(!report.ok);
        assert_eq!(
            states(&report),
            [
                ("a", RolloutState::Done),
                ("b", RolloutState::Failed),
                ("c", RolloutState::Skipped)
            ]
        );
        assert_eq!(report.hosts[1].error, "applying failed");
        assert_eq!(*log.borrow(), ["a apply", "a status", "b apply"]);
    }

    #[test]
    fn a_bad_update_finishes_the_host_and_stops_the_next() {
        let log = Default::default();
        let mut targets = fleet(&log, &[("a", "1"), ("b", "1")]);
        let options = Rollout {
            update: true,
            ..Default::default()
        };
        // a's update ends badly.
        let a = Fake {
            name: "a".into(),
            applied: Some(record("1")),
            records: record("2"),
            update_code: 100,
            log: log.clone(),
            ..Default::default()
        };
        targets[0] = Box::new(a);
        let report = run(&mut targets, &options, &now, &mut vec![]);
        assert!(!report.ok);
        assert_eq!(report.hosts[0].state, RolloutState::Done);
        assert_eq!(report.hosts[0].warnings, ["update ended with 100"]);
        assert_eq!(report.hosts[1].state, RolloutState::Skipped);
        assert_eq!(*log.borrow(), ["a apply", "a update", "a status"]);
    }

    #[test]
    fn a_reboot_is_asked_for_needed_and_kept_from_sessions() {
        let make = |log: &std::rc::Rc<std::cell::RefCell<Vec<String>>>,
                    sessions,
                    comes_back,
                    status_code| Fake {
            name: "a".into(),
            applied: Some(record("1")),
            records: record("2"),
            reboot_required: true,
            sessions,
            comes_back,
            status_code,
            uptime: 1000,
            log: log.clone(),
            ..Default::default()
        };
        let reboot = Rollout {
            reboot: true,
            ..Default::default()
        };
        // Without asking for it, there is no reboot.
        let log = Default::default();
        let mut targets: Vec<Box<dyn Target>> = vec![Box::new(make(&log, 0, true, 0))];
        let report = run(&mut targets, &Rollout::default(), &now, &mut vec![]);
        assert!(report.ok && !report.hosts[0].rebooted);
        // Asked for and needed: it reboots, comes back and is checked.
        let log = Default::default();
        let mut targets: Vec<Box<dyn Target>> = vec![Box::new(make(&log, 0, true, 0))];
        let report = run(&mut targets, &reboot, &now, &mut vec![]);
        assert!(report.ok && report.hosts[0].rebooted);
        assert_eq!(*log.borrow(), ["a apply", "a reboot", "a status"]);
        // Sessions keep it from rebooting, unless forced.
        let log = Default::default();
        let mut targets: Vec<Box<dyn Target>> = vec![Box::new(make(&log, 2, true, 0))];
        let report = run(&mut targets, &reboot, &now, &mut vec![]);
        assert_eq!(report.hosts[0].state, RolloutState::Failed);
        assert!(report.hosts[0].error.contains("2 sessions are running"));
        let forced = Rollout {
            force: true,
            ..reboot.clone()
        };
        let log = Default::default();
        let mut targets: Vec<Box<dyn Target>> = vec![Box::new(make(&log, 2, true, 0))];
        assert!(run(&mut targets, &forced, &now, &mut vec![]).ok);
        assert_eq!(log.borrow()[1], "a reboot forced");
        // One that does not come back, or is not well afterwards, fails.
        let log = Default::default();
        let mut targets: Vec<Box<dyn Target>> = vec![Box::new(make(&log, 0, false, 0))];
        let report = run(&mut targets, &reboot, &now, &mut vec![]);
        assert!(report.hosts[0].error.contains("did not come back"));
        let log = Default::default();
        let mut targets: Vec<Box<dyn Target>> = vec![Box::new(make(&log, 0, true, 3))];
        let report = run(&mut targets, &reboot, &now, &mut vec![]);
        assert_eq!(report.hosts[0].error, "its status task ended with 3");
    }

    #[test]
    fn hosts_are_taken_by_name_or_as_asked() {
        let all = vec!["b".to_owned(), "a".to_owned()];
        assert_eq!(chosen(all.clone(), &[]).unwrap(), ["a", "b"]);
        assert_eq!(chosen(all.clone(), &["b".into()]).unwrap(), ["b"]);
        assert!(chosen(all, &["c".into()]).is_err());
    }
}
