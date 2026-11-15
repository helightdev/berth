//! What a host of each type can do, and the one check that every client
//! asks before it offers or runs something: the UIs, the commands and the
//! server's API. A host's type is what its profile says it is meant to be;
//! what it is seen to be comes from asking it.

use crate::{Result, host::Host};
use berth_api::{Action, Resource, Version, ops::Role};

pub use berth_api::HostType;

/// Something berth can do with a host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// A terminal and commands over SSH, the inventory, whether it answers.
    Ssh,
    /// Install or update berth's controller alone.
    InstallController,
    /// Sessions, the channel, files, units, logs and processes.
    Sessions,
    /// The controller's actions: status, update and reboot.
    Actions,
    /// Secrets and registry logins sent to the host.
    Secrets,
    /// Plugin features, their actions and services.
    Plugins,
    /// Applying the profile over SSH, and a roll-out over it.
    Apply,
    /// A cloud seed or an installer image, which install a whole workstation.
    Seed,
    /// The desktop: screenshots and access links.
    Desktop,
}

impl Capability {
    pub const ALL: [Capability; 9] = [
        Capability::Ssh,
        Capability::InstallController,
        Capability::Sessions,
        Capability::Actions,
        Capability::Secrets,
        Capability::Plugins,
        Capability::Apply,
        Capability::Seed,
        Capability::Desktop,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Capability::Ssh => "ssh",
            Capability::InstallController => "install-controller",
            Capability::Sessions => "sessions",
            Capability::Actions => "actions",
            Capability::Secrets => "secrets",
            Capability::Plugins => "plugins",
            Capability::Apply => "apply",
            Capability::Seed => "seed",
            Capability::Desktop => "desktop",
        }
    }

    /// What it is, as the subject of a sentence.
    pub fn label(self) -> &'static str {
        match self {
            Capability::Ssh => "a terminal over SSH",
            Capability::InstallController => "installing the controller alone",
            Capability::Sessions => "working with sessions, the channel and files",
            Capability::Actions => "running the controller's actions",
            Capability::Secrets => "sending secrets and registry logins",
            Capability::Plugins => "using plugin features",
            Capability::Apply => "applying a profile or a roll-out",
            Capability::Seed => "a cloud seed or an installer image",
            Capability::Desktop => "using the desktop",
        }
    }

    /// Whether a host of `host_type` has it. The controller alone is
    /// installed on a host that is not applied; the controller of a node or
    /// a workstation comes with applying.
    pub fn allowed(self, host_type: HostType) -> bool {
        use HostType::*;
        match self {
            Capability::Ssh => true,
            Capability::InstallController => matches!(host_type, Ssh | Managed),
            Capability::Sessions | Capability::Actions | Capability::Secrets => {
                matches!(host_type, Managed | Node | Workstation)
            }
            Capability::Plugins | Capability::Apply => matches!(host_type, Node | Workstation),
            Capability::Seed | Capability::Desktop => host_type == Workstation,
        }
    }
}

/// The capabilities and the types that have each, in the order of
/// [`HostType::ALL`]: for a page that shows the table.
pub fn table() -> Vec<(Capability, [bool; HostType::ALL.len()])> {
    Capability::ALL
        .into_iter()
        .map(|capability| {
            (
                capability,
                HostType::ALL.map(|host_type| capability.allowed(host_type)),
            )
        })
        .collect()
}

/// Why something cannot be done with a host now, and the action that
/// changes its type so it can.
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

/// Whether the host `name` of `host_type` may do this, or why not.
pub fn check(
    name: &str,
    host_type: HostType,
    capability: Capability,
) -> std::result::Result<(), Refusal> {
    if capability.allowed(host_type) {
        return Ok(());
    }
    let what = capability.label();
    Err(match (capability, host_type) {
        (Capability::InstallController, _) => Refusal {
            why: format!(
                "{name} is a {host_type}: applying its configuration installs and updates its controller. Apply it with: berth apply {name}"
            ),
            raise: "apply",
        },
        (_, HostType::Ssh) if capability.allowed(HostType::Managed) => Refusal {
            why: format!(
                "{name} is a host of the type ssh: berth's controller is not installed there, and {what} needs it. Install it with: berth run {name} install-controller"
            ),
            raise: "install-controller",
        },
        (Capability::Desktop, HostType::Node) => Refusal {
            why: format!(
                "{name} is a node, a server without a desktop: {what} needs a workstation. Set its type to workstation in its set-up, then apply it"
            ),
            raise: "setup",
        },
        (_, _) if capability.allowed(HostType::Node) => Refusal {
            why: format!(
                "{name} is a host of the type {host_type}, neither a node nor a workstation: {what} needs berth to apply its profile. Make it a node (berth host set {name} type=node) or a workstation in its set-up, then apply it"
            ),
            raise: "setup",
        },
        _ => Refusal {
            why: format!(
                "{name} is a host of the type {host_type}, not a workstation: {what} needs its whole profile. Set its type to workstation in its set-up, then apply it"
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

/// The type a profile means its host to be.
pub fn of(config: &serde_json::Value) -> HostType {
    berth_api::profile::host_type(config)
}

/// An action as a host of `host_type` may run it: unavailable, with the
/// reason, when the type rules it out.
pub fn fit(mut action: Action, name: &str, host_type: HostType, capability: Capability) -> Action {
    if let Err(refusal) = check(name, host_type, capability) {
        action.available = false;
        action.why = refusal.why;
    }
    action
}

/// What a host was seen to be by asking its controller: what its record
/// says when it answers, ssh when berth logged in and found no
/// controller, nothing when it could not be asked.
pub fn seen(answer: std::result::Result<&Version, bool>) -> Option<HostType> {
    match answer {
        // A controller without a record was installed outside berth.
        Ok(version) => Some(version.host_type.unwrap_or(HostType::Managed)),
        Err(true) => Some(HostType::Ssh),
        Err(false) => None,
    }
}

pub use berth_api::type_mismatch as mismatch;

/// Why installing the controller alone must not run on a host seen to be a
/// workstation, whatever its profile says: it would take the workstation's
/// controller apart.
pub fn refuse_install(
    name: &str,
    intended: HostType,
    observed: Option<HostType>,
) -> Option<String> {
    match (intended, observed) {
        (HostType::Workstation | HostType::Node, _) | (_, None) => None,
        (_, Some(HostType::Workstation | HostType::Node)) => Some(format!(
            "{} Installing the controller alone would take its controller apart, so it is not offered",
            mismatch(name, intended, observed).unwrap_or_default()
        )),
        _ => None,
    }
}

/// What the host's controller says the host is, asked through the link
/// once and briefly: `None` when no controller answers.
pub fn observe(
    paths: &crate::paths::Paths,
    host: &Host,
    password: Option<&str>,
) -> Option<HostType> {
    let link = crate::link::Link::new(host.link(paths, password).ok()?);
    let runtime = crate::link::runtime().ok()?;
    let asked = runtime.block_on(async {
        tokio::time::timeout(std::time::Duration::from_secs(15), link.version()).await
    });
    runtime.shutdown_background();
    match asked {
        Ok(Ok(version)) => seen(Ok(&version)),
        _ => None,
    }
}

/// berth's own action that puts the controller on a host, as a host of
/// `host_type` sees it. `observed` is what its controller says it is, when
/// it answered.
pub fn install_controller_action(
    name: &str,
    host_type: HostType,
    observed: Option<HostType>,
) -> Action {
    let (label, help) = match host_type {
        HostType::Managed => (
            "Update the controller",
            "Install berthd and box of this berth on the host again, and restart the controller: for example after berth was updated. Nothing else of the profile is applied.",
        ),
        _ => (
            "Install the controller",
            "Put berthd and box on the host and start the controller, so that sessions, actions and files work there. Nothing else of a workstation is installed: no desktop, agents or plugins. The SSH user is the account sessions run as. The host's type becomes managed.",
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
    let mut action = fit(action, name, host_type, Capability::InstallController);
    if let Some(why) = refuse_install(name, host_type, observed) {
        action.available = false;
        action.why = why;
    }
    action
}

/// berth's own action that gives a host another name, of any type: the
/// machine stays as it is.
pub fn rename_action() -> Action {
    Action {
        name: "rename".into(),
        label: "Rename".into(),
        help: "Give the host another name in berth. Its profile, passwords, host keys, machine record, messages and jobs go with it. The machine itself stays as it is and keeps running, and so does its hostname unless hostname says otherwise.".into(),
        group: "Host".into(),
        role: Role::Admin,
        arguments: vec![
            berth_api::Argument {
                name: "name".into(),
                label: "New name".into(),
                help: "The host's name in berth from now on: letters, digits, `-`, `_` and `.`."
                    .into(),
                required: true,
                ..Default::default()
            },
            berth_api::Argument {
                name: "hostname".into(),
                label: "Hostname too".into(),
                help: "Make the new name the profile's hostname as well. The machine takes it at its next apply, and so does its device on the tailnet.".into(),
                default: "no".into(),
                values: vec!["no".into(), "yes".into()],
                ..Default::default()
            },
        ],
        ..Action::default()
    }
}

/// How a host is started and stopped, with why it cannot be when it cannot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Powered {
    pub power: berth_api::Power,
    /// Why berth cannot start it; empty when it can.
    pub why_start: String,
    /// Why berth cannot stop it; empty when it can.
    pub why_stop: String,
}

impl Powered {
    /// Why `verb` (`start` or `stop`) cannot be done, or `None` when it can.
    pub fn why(&self, verb: &str) -> Option<&str> {
        let why = match verb {
            "start" => &self.why_start,
            _ => &self.why_stop,
        };
        (!why.is_empty()).then_some(why.as_str())
    }
}

/// How the host `name` is started and stopped: through the provisioner
/// that made `machine`, when its kind has the verb, or else as its profile
/// declares in `power`. A host that declares nothing can be neither. The
/// state is the provisioner's last word; the caller knows better when the
/// host's controller answers.
pub fn power(name: &str, profile: &serde_json::Value, machine: Option<&Resource>) -> Powered {
    use berth_api::{PowerState, profile::power_way};
    let host_type = of(profile);
    let mut powered = Powered::default();
    if let Some(machine) = machine {
        powered.power.state = PowerState::of(&machine.state);
    }
    for verb in ["start", "stop"] {
        let declared = power_way(profile, verb);
        let (by, why) = match machine {
            Some(machine) if machine.external_id.is_empty() => (
                "",
                format!("{name}'s machine is not made yet. Make it with: berth host create {name}"),
            ),
            Some(machine) if machine.verbs.iter().any(|known| known == verb) => {
                ("provisioner", String::new())
            }
            _ if declared == "controller" => match check(name, host_type, Capability::Actions) {
                Ok(()) => ("controller", String::new()),
                Err(refusal) => ("", refusal.why),
            },
            Some(machine) => (
                "",
                format!(
                    "{} cannot {verb} the machines it makes, and the profile of {name} declares no other way in power.{verb}",
                    machine.provisioner
                ),
            ),
            None => (
                "",
                match verb {
                    "stop" => format!(
                        "{name} declares no way to stop it: no provisioner made it, and its profile has no power.stop. To have its controller shut it down: berth host set {name} power_stop=controller"
                    ),
                    _ => format!(
                        "{name} declares no way to start it: berth starts a host only through the provisioner that made it"
                    ),
                },
            ),
        };
        match verb {
            "start" => {
                (powered.power.start, powered.power.start_by) = (!by.is_empty(), by.into());
                powered.why_start = why;
            }
            _ => {
                (powered.power.stop, powered.power.stop_by) = (!by.is_empty(), by.into());
                powered.why_stop = why;
            }
        }
    }
    powered
}

/// berth's own actions on a host of this machine, which need no server:
/// those that change the host's type, and renaming it.
pub fn local_actions(name: &str, host_type: HostType, observed: Option<HostType>) -> Vec<Action> {
    vec![
        install_controller_action(name, host_type, observed),
        rename_action(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_type_can_do_what_the_ones_before_it_can() {
        use Capability::*;
        let rows: Vec<(&str, [bool; 4])> = table()
            .into_iter()
            .map(|(capability, types)| (capability.name(), types))
            .collect();
        // ssh, managed, node, workstation.
        assert_eq!(
            rows,
            [
                (Ssh.name(), [true, true, true, true]),
                (InstallController.name(), [true, true, false, false]),
                (Sessions.name(), [false, true, true, true]),
                (Actions.name(), [false, true, true, true]),
                (Secrets.name(), [false, true, true, true]),
                (Plugins.name(), [false, false, true, true]),
                (Apply.name(), [false, false, true, true]),
                (Seed.name(), [false, false, false, true]),
                (Desktop.name(), [false, false, false, true]),
            ]
        );
    }

    #[test]
    fn a_refusal_names_the_action_that_changes_the_type() {
        let refused = check("studio", HostType::Ssh, Capability::Sessions).unwrap_err();
        assert_eq!(refused.raise, "install-controller");
        assert!(refused.why.contains("berth run studio install-controller"));
        let refused = check("studio", HostType::Ssh, Capability::Apply).unwrap_err();
        assert_eq!(refused.raise, "setup");
        assert!(
            refused
                .why
                .contains("of the type ssh, neither a node nor a workstation")
        );
        let refused = check("studio", HostType::Managed, Capability::Desktop).unwrap_err();
        assert_eq!(refused.raise, "setup");
        let refused = check(
            "studio",
            HostType::Workstation,
            Capability::InstallController,
        )
        .unwrap_err();
        assert_eq!(refused.raise, "apply");
        assert!(check("studio", HostType::Workstation, Capability::Apply).is_ok());
    }

    #[test]
    fn a_node_is_applied_and_has_no_desktop() {
        assert!(check("big", HostType::Node, Capability::Apply).is_ok());
        assert!(check("big", HostType::Node, Capability::Plugins).is_ok());
        assert!(check("big", HostType::Node, Capability::Sessions).is_ok());
        let refused = check("big", HostType::Node, Capability::Desktop).unwrap_err();
        assert!(
            refused
                .why
                .starts_with("big is a node, a server without a desktop")
        );
        let refused = check("big", HostType::Node, Capability::InstallController).unwrap_err();
        assert_eq!(refused.raise, "apply");
        assert!(refused.why.starts_with("big is a node: applying"));
        let refused = check("big", HostType::Managed, Capability::Apply).unwrap_err();
        assert!(refused.why.contains("neither a node nor a workstation"));
        assert!(refused.why.contains("berth host set big type=node"));
        // The controller alone is never put over a node's.
        let action = install_controller_action("big", HostType::Managed, Some(HostType::Node));
        assert!(!action.available);
        assert!(
            action
                .why
                .starts_with("big is a node; its profile here says managed")
        );
        assert!(!install_controller_action("big", HostType::Node, None).available);
        let refused = check("big", HostType::Node, Capability::Seed).unwrap_err();
        assert!(refused.why.contains("not a workstation"));
    }

    #[test]
    fn the_apply_guard_reads_the_profile() {
        let host = |config| Host {
            name: "dock".into(),
            place: crate::profiles::Place::File("/dock.json".into()),
            config,
        };
        let ssh = host(serde_json::json!({"type": "ssh", "access": {"address": "192.0.2.4"}}));
        let refused = guard(&ssh, Capability::Apply).unwrap_err();
        assert!(
            refused
                .to_string()
                .contains("neither a node nor a workstation")
        );
        assert!(guard(&host(serde_json::json!({"builder": {}})), Capability::Apply).is_ok());
    }

    #[test]
    fn what_a_host_is_seen_to_be_comes_from_its_controller() {
        let version = |host_type| Version {
            version: "1".into(),
            api: 2,
            mode: "system".into(),
            applied: None,
            host_type,
        };
        assert_eq!(seen(Ok(&version(None))), Some(HostType::Managed));
        assert_eq!(
            seen(Ok(&version(Some(HostType::Workstation)))),
            Some(HostType::Workstation)
        );
        assert_eq!(seen(Err(true)), Some(HostType::Ssh));
        assert_eq!(seen(Err(false)), None);
    }

    #[test]
    fn installing_the_controller_is_offered_to_a_host_that_is_not_a_workstation() {
        let action = install_controller_action("dock", HostType::Ssh, None);
        assert!(action.available && action.label == "Install the controller");
        assert_eq!(
            install_controller_action("dock", HostType::Managed, None).label,
            "Update the controller"
        );
        let action = install_controller_action("dock", HostType::Workstation, None);
        assert!(!action.available && action.why.contains("berth apply dock"));
        // A controller seen to be one is updated as its profile says.
        let action = install_controller_action("dock", HostType::Ssh, Some(HostType::Managed));
        assert!(action.available);
    }

    #[test]
    fn a_host_seen_to_be_a_workstation_never_gets_the_controller_alone() {
        for intended in [HostType::Ssh, HostType::Managed] {
            let action = install_controller_action("dock", intended, Some(HostType::Workstation));
            assert!(!action.available);
            assert!(
                action.why.starts_with(&format!(
                    "dock is a workstation; its profile here says {intended}"
                )),
                "{}",
                action.why
            );
            assert!(action.why.contains("berth host adopt dock"));
        }
        assert_eq!(
            mismatch("dock", HostType::Ssh, Some(HostType::Managed)).unwrap(),
            "dock has berth's controller; its profile here says ssh. To adopt it: berth host set dock type=managed"
        );
        assert!(mismatch("dock", HostType::Workstation, Some(HostType::Managed)).is_none());
        assert!(mismatch("dock", HostType::Managed, None).is_none());
    }

    fn machine(verbs: &[&str], state: &str, made: bool) -> Resource {
        Resource {
            provisioner: "incus".into(),
            kind: "machine".into(),
            state: state.into(),
            external_id: if made {
                "berth-studio".into()
            } else {
                String::new()
            },
            verbs: verbs.iter().map(|verb| (*verb).to_owned()).collect(),
            ..Resource::default()
        }
    }

    #[test]
    fn a_host_a_provisioner_made_is_started_and_stopped_through_it() {
        use berth_api::PowerState;
        let profile = serde_json::json!({"builder": {}});
        let made = machine(&["start", "stop"], "stopped", true);
        let powered = power("studio", &profile, Some(&made));
        assert!(powered.power.start && powered.power.stop);
        assert_eq!(
            (
                powered.power.start_by.as_str(),
                powered.power.stop_by.as_str()
            ),
            ("provisioner", "provisioner")
        );
        assert_eq!(powered.power.state, PowerState::Stopped);
        assert_eq!((powered.why("start"), powered.why("stop")), (None, None));
        // A kind without the verbs says so, unless the profile declares a way to stop.
        let plain = machine(&[], "running", true);
        let powered = power("studio", &profile, Some(&plain));
        assert!(!powered.power.start && !powered.power.stop);
        assert!(
            powered
                .why("stop")
                .unwrap()
                .starts_with("incus cannot stop the machines it makes")
        );
        let declared = serde_json::json!({"builder": {}, "power": {"stop": {"by": "controller"}}});
        let powered = power("studio", &declared, Some(&plain));
        assert_eq!(
            (powered.power.stop, powered.power.stop_by.as_str()),
            (true, "controller")
        );
        assert!(!powered.power.start);
        // One planned and not made yet can do neither.
        let planned = machine(&["start", "stop"], "planned", false);
        let powered = power("lab", &profile, Some(&planned));
        assert!(!powered.power.start && !powered.power.stop);
        assert!(
            powered
                .why("start")
                .unwrap()
                .contains("berth host create lab")
        );
        assert_eq!(powered.power.state, PowerState::Unknown);
    }

    #[test]
    fn any_other_host_declares_how_it_is_stopped() {
        let bare = serde_json::json!({"type": "managed", "access": {"address": "192.0.2.4"}});
        let powered = power("dock", &bare, None);
        assert_eq!(powered.power, berth_api::Power::default());
        assert!(
            powered
                .why("stop")
                .unwrap()
                .contains("berth host set dock power_stop=controller")
        );
        assert!(
            powered
                .why("start")
                .unwrap()
                .starts_with("dock declares no way to start it")
        );
        let declared = serde_json::json!({"type": "managed", "access": {"address": "192.0.2.4"},
                                          "power": {"stop": {"by": "controller"}}});
        let powered = power("dock", &declared, None);
        assert!(powered.power.stop && !powered.power.start);
        assert_eq!(powered.power.stop_by, "controller");
        // Without a controller there is nothing to shut it down.
        let ssh = serde_json::json!({"type": "ssh", "access": {"address": "192.0.2.4"},
                                     "power": {"stop": {"by": "controller"}}});
        let powered = power("dock", &ssh, None);
        assert!(!powered.power.stop);
        assert!(
            powered
                .why("stop")
                .unwrap()
                .contains("berth run dock install-controller")
        );
    }
}
