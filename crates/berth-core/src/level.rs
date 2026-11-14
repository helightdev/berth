//! What a host can do at each of its levels, and the one check that every
//! client asks before it offers or runs something: the UIs, the commands
//! and the server's API. A host's level is what its profile says it is
//! meant to be; what it is seen to be comes from asking it.

use crate::{Result, host::Host};
use berth_api::{Action, Version, ops::Role};

pub use berth_api::HostLevel as Level;

/// Something berth can do with a host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// A terminal and commands over SSH, the inventory, whether it answers.
    Ssh,
    /// Install or update berth's controller alone.
    InstallController,
    /// Sessions, the channel, files, units, logs and processes.
    Sessions,
    /// The controller's tasks: status, update and reboot.
    Tasks,
    /// Secrets and registry logins sent to the host.
    Secrets,
    /// Plugin features, their tasks and services.
    Plugins,
    /// Applying the profile, a roll-out over it, a seed or an installer image.
    Apply,
    /// The desktop: screenshots and access links.
    Desktop,
}

impl Capability {
    pub const ALL: [Capability; 8] = [
        Capability::Ssh,
        Capability::InstallController,
        Capability::Sessions,
        Capability::Tasks,
        Capability::Secrets,
        Capability::Plugins,
        Capability::Apply,
        Capability::Desktop,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Capability::Ssh => "ssh",
            Capability::InstallController => "install-controller",
            Capability::Sessions => "sessions",
            Capability::Tasks => "tasks",
            Capability::Secrets => "secrets",
            Capability::Plugins => "plugins",
            Capability::Apply => "apply",
            Capability::Desktop => "desktop",
        }
    }

    /// What it is, as the subject of a sentence.
    pub fn label(self) -> &'static str {
        match self {
            Capability::Ssh => "a terminal over SSH",
            Capability::InstallController => "installing the controller alone",
            Capability::Sessions => "working with sessions, the channel and files",
            Capability::Tasks => "running the controller's tasks",
            Capability::Secrets => "sending secrets and registry logins",
            Capability::Plugins => "using plugin features",
            Capability::Apply => "applying a profile, a roll-out, a seed or an installer",
            Capability::Desktop => "using the desktop",
        }
    }

    /// Whether a host of `level` has it. The controller alone is installed
    /// on a host below a workstation; a workstation's comes with applying.
    pub fn allowed(self, level: Level) -> bool {
        match self {
            Capability::Ssh => true,
            Capability::InstallController => level != Level::Workstation,
            Capability::Sessions | Capability::Tasks | Capability::Secrets => {
                level >= Level::Controller
            }
            Capability::Plugins | Capability::Apply | Capability::Desktop => {
                level == Level::Workstation
            }
        }
    }
}

/// The capabilities and the levels that have each, in the order of
/// [`Level::ALL`]: for a page that shows the table.
pub fn table() -> Vec<(Capability, [bool; 3])> {
    Capability::ALL
        .into_iter()
        .map(|capability| {
            (
                capability,
                Level::ALL.map(|level| capability.allowed(level)),
            )
        })
        .collect()
}

/// Why something cannot be done with a host now, and the action that
/// raises its level so it can.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub why: String,
    /// `install-controller`, `apply`, or `setup` for the set-up flow that
    /// makes the host a workstation.
    pub raise: &'static str,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.why)
    }
}

impl std::error::Error for Refusal {}

/// Whether the host `name` of `level` may do this, or why not.
pub fn check(name: &str, level: Level, capability: Capability) -> std::result::Result<(), Refusal> {
    if capability.allowed(level) {
        return Ok(());
    }
    let what = capability.label();
    Err(match (capability, level) {
        (Capability::InstallController, _) => Refusal {
            why: format!(
                "{name} is a workstation: applying its configuration installs and updates its controller. Apply it with: berth apply {name}"
            ),
            raise: "apply",
        },
        (_, Level::Reachable) if capability.allowed(Level::Controller) => Refusal {
            why: format!(
                "{name} is reachable only: berth's controller is not installed there, and {what} needs it. Install it with: berth host install {name}"
            ),
            raise: "install-controller",
        },
        _ => Refusal {
            why: format!(
                "{name} is a {level} host, not a workstation: {what} needs its whole profile. Set its level to workstation in its set-up, then apply it"
            ),
            raise: "setup",
        },
    })
}

/// The same for a host by its profile, as an error to stop with.
pub fn guard(host: &Host, capability: Capability) -> Result<()> {
    check(&host.name, of(&host.config), capability)?;
    Ok(())
}

/// The level a profile means its host to have.
pub fn of(config: &serde_json::Value) -> Level {
    berth_api::profile::level(config)
}

/// An action as a host of `level` may run it: unavailable, with the reason,
/// when the level rules it out.
pub fn fit(mut action: Action, name: &str, level: Level, capability: Capability) -> Action {
    if let Err(refusal) = check(name, level, capability) {
        action.available = false;
        action.why = refusal.why;
    }
    action
}

/// What a host was seen to be by asking its controller: what its record
/// says when it answers, reachable when berth logged in and found no
/// controller, nothing when it could not be asked.
pub fn seen(answer: std::result::Result<&Version, bool>) -> Option<Level> {
    match answer {
        // A controller without a record was installed outside berth.
        Ok(version) => Some(version.level.unwrap_or(Level::Controller)),
        Err(true) => Some(Level::Reachable),
        Err(false) => None,
    }
}

pub use berth_api::level_mismatch as mismatch;

/// Why installing the controller alone must not run on a host seen to be a
/// workstation, whatever its profile says: it would take the workstation's
/// controller apart.
pub fn refuse_install(name: &str, intended: Level, observed: Option<Level>) -> Option<String> {
    match (intended, observed) {
        (Level::Workstation, _) | (_, None) => None,
        (_, Some(Level::Workstation)) => Some(format!(
            "{} Installing the controller alone would take its controller apart, so it is not offered",
            mismatch(name, intended, observed).unwrap_or_default()
        )),
        _ => None,
    }
}

/// What the host's controller says the host is, asked through the link
/// once and briefly: `None` when no controller answers.
pub fn observe(paths: &crate::paths::Paths, host: &Host, password: Option<&str>) -> Option<Level> {
    let link = crate::link::Link::new(host.link(paths, password).ok()?);
    let runtime = crate::link::runtime().ok()?;
    let asked = runtime.block_on(async {
        tokio::time::timeout(
            std::time::Duration::from_secs(15),
            link.get::<Version>("/v1/version"),
        )
        .await
    });
    runtime.shutdown_background();
    match asked {
        Ok(Ok(version)) => seen(Ok(&version)),
        _ => None,
    }
}

/// berth's own action that puts the controller on a host, as a host of
/// `level` sees it. `observed` is what its controller says it is, when it
/// answered.
pub fn install_controller_action(name: &str, level: Level, observed: Option<Level>) -> Action {
    let (label, help) = match level {
        Level::Controller => (
            "Update the controller",
            "Install berthd and box of this berth on the host again, and restart the controller: for example after berth was updated. Nothing else of the profile is applied.",
        ),
        _ => (
            "Install the controller",
            "Put berthd and box on the host and start the controller, so that sessions, tasks and files work there. Nothing else of a workstation is installed: no desktop, agents or plugins. The SSH user is the account sessions run as. The host's level becomes controller.",
        ),
    };
    let action = Action {
        name: Capability::InstallController.name().into(),
        label: label.into(),
        help: help.into(),
        group: "Set-Up".into(),
        role: Role::Admin,
        confirm: Some(format!("Install berth's controller on {name} now?")),
        ..Action::default()
    };
    let mut action = fit(action, name, level, Capability::InstallController);
    if let Some(why) = refuse_install(name, level, observed) {
        action.available = false;
        action.why = why;
    }
    action
}

/// berth's own actions on a host of this machine, which need no server:
/// those that change the host's level.
pub fn local_actions(name: &str, level: Level, observed: Option<Level>) -> Vec<Action> {
    vec![install_controller_action(name, level, observed)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_level_can_do_what_the_ones_below_it_can() {
        use Capability::*;
        let rows: Vec<(&str, [bool; 3])> = table()
            .into_iter()
            .map(|(capability, levels)| (capability.name(), levels))
            .collect();
        assert_eq!(
            rows,
            [
                (Ssh.name(), [true, true, true]),
                (InstallController.name(), [true, true, false]),
                (Sessions.name(), [false, true, true]),
                (Tasks.name(), [false, true, true]),
                (Secrets.name(), [false, true, true]),
                (Plugins.name(), [false, false, true]),
                (Apply.name(), [false, false, true]),
                (Desktop.name(), [false, false, true]),
            ]
        );
    }

    #[test]
    fn a_refusal_names_the_action_that_raises_the_level() {
        let refused = check("studio", Level::Reachable, Capability::Sessions).unwrap_err();
        assert_eq!(refused.raise, "install-controller");
        assert!(refused.why.contains("berth host install studio"));
        let refused = check("studio", Level::Reachable, Capability::Apply).unwrap_err();
        assert_eq!(refused.raise, "setup");
        assert!(refused.why.contains("a reachable host, not a workstation"));
        let refused = check("studio", Level::Controller, Capability::Desktop).unwrap_err();
        assert_eq!(refused.raise, "setup");
        let refused =
            check("studio", Level::Workstation, Capability::InstallController).unwrap_err();
        assert_eq!(refused.raise, "apply");
        assert!(check("studio", Level::Workstation, Capability::Apply).is_ok());
    }

    #[test]
    fn the_apply_guard_reads_the_profile() {
        let host = |config| Host {
            name: "dock".into(),
            place: crate::profiles::Place::File("/dock.json".into()),
            config,
        };
        let reachable =
            host(serde_json::json!({"level": "reachable", "access": {"address": "192.0.2.4"}}));
        let refused = guard(&reachable, Capability::Apply).unwrap_err();
        assert!(refused.to_string().contains("not a workstation"));
        assert!(guard(&host(serde_json::json!({"builder": {}})), Capability::Apply).is_ok());
    }

    #[test]
    fn what_a_host_is_seen_to_be_comes_from_its_controller() {
        let version = |level| Version {
            version: "1".into(),
            api: 1,
            mode: "system".into(),
            applied: None,
            level,
        };
        assert_eq!(seen(Ok(&version(None))), Some(Level::Controller));
        assert_eq!(
            seen(Ok(&version(Some(Level::Workstation)))),
            Some(Level::Workstation)
        );
        assert_eq!(seen(Err(true)), Some(Level::Reachable));
        assert_eq!(seen(Err(false)), None);
    }

    #[test]
    fn installing_the_controller_is_offered_below_a_workstation() {
        let action = install_controller_action("dock", Level::Reachable, None);
        assert!(action.available && action.label == "Install the controller");
        assert_eq!(
            install_controller_action("dock", Level::Controller, None).label,
            "Update the controller"
        );
        let action = install_controller_action("dock", Level::Workstation, None);
        assert!(!action.available && action.why.contains("berth apply dock"));
        // A controller seen to be one is updated as its profile says.
        let action = install_controller_action("dock", Level::Reachable, Some(Level::Controller));
        assert!(action.available);
    }

    #[test]
    fn a_host_seen_to_be_a_workstation_never_gets_the_controller_alone() {
        for intended in [Level::Reachable, Level::Controller] {
            let action = install_controller_action("dock", intended, Some(Level::Workstation));
            assert!(!action.available);
            assert!(
                action.why.starts_with(&format!(
                    "dock is a workstation; its profile here says {intended}"
                )),
                "{}",
                action.why
            );
            assert!(action.why.contains("berth adopt dock"));
        }
        assert_eq!(
            mismatch("dock", Level::Reachable, Some(Level::Controller)).unwrap(),
            "dock has berth's controller; its profile here says reachable. To adopt it: berth host level dock controller"
        );
        assert!(mismatch("dock", Level::Workstation, Some(Level::Controller)).is_none());
        assert!(mismatch("dock", Level::Controller, None).is_none());
    }
}
