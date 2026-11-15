//! The actions of a host and of the installation: what can be done to each,
//! and running one. A host's actions are the actions its controller lists and
//! berth's own from [`HOST`]; the installation's are [`INSTALLATION`]. An
//! action that changes something runs as a job; one the host runs is a
//! session there, which a job follows unless it needs a terminal.

use super::{
    Remote, Serve,
    api::{Answer, App, Fail, Said, caller, internal, job_info, remote, report_path, turned_down},
    auth::Access,
    setup::{own_place, saved_profile},
};
use axum::{
    Json,
    body::Bytes,
    extract::{Extension, Path, State},
    http::StatusCode,
};
use berth_api::profile::{self, Auth};
use berth_api::{
    Act, Acted, Action, Argument, ErrorBody, HostAction, Resource, Rollout, RolloutReport, Runs,
    Session, b64, client, code, ops::Role, upgrade_server,
};
use berth_core::{
    apply,
    host::Host,
    host_type::{self, Capability, HostType, Powered},
    keys::{self, Once, contact, ring},
    profiles::Place,
    registry::Store,
    rollout,
    runner::{Controller, Request, Reserved, Subject, Work},
};
use serde_json::{Value, json};
use std::{future::Future, io::Write, pin::Pin, sync::Arc, time::Duration};

type Run = Pin<Box<dyn Future<Output = Answer<Acted>> + Send>>;

/// What berth's own actions are told of a subject, to say whether they can
/// run on it now.
pub(super) struct Seen<'a> {
    /// The host's name; empty for the installation.
    pub name: &'a str,
    /// The host's profile; null for the installation.
    pub profile: &'a Value,
    /// What the host's controller lists; `None` when it did not answer.
    pub host_actions: Option<&'a [HostAction]>,
    /// The names of the secrets the server keeps.
    pub secrets: &'a [String],
    /// The type the host's profile means it to be.
    pub host_type: HostType,
    /// What its controller says it is, when it answered.
    pub observed: Option<HostType>,
    /// Why its controller did not answer, when it was asked and did not.
    pub unanswered: Option<&'a str>,
    /// That its controller is too old for this berth, and how to update it,
    /// when that is why it did not answer.
    pub older: Option<&'a str>,
    /// The caller is on the machine the server runs on: in its process, or
    /// on its socket.
    pub local: bool,
    /// How the host is started and stopped.
    pub power: &'a Powered,
    /// The machine a provisioner made for it, or one planned.
    pub machine: Option<&'a Resource>,
}

/// berth's own actions that run through the host's controller, so the host
/// is asked before one of them is described.
const THROUGH_CONTROLLER: &[&str] = &["secrets", UPGRADE_SERVER];

/// berth's own action that upgrades the berth server in a container on a
/// node. It is offered only where the node's controller lists it, which its
/// role does when it found such a container.
const UPGRADE_SERVER: &str = upgrade_server::ACTION;

/// One of berth's own actions.
pub(super) struct Builtin {
    pub name: &'static str,
    /// Its description for a subject, with `available` and `why` set for it.
    pub describe: fn(&Seen) -> Action,
    /// Run it on the subject by name, once the caller's role and the
    /// action's availability have been checked.
    pub run: fn(Arc<Serve>, String, Act) -> Run,
}

/// berth's own actions on a host.
pub(super) static HOST: &[Builtin] = &[
    Builtin {
        name: "apply",
        describe: apply_action,
        run: apply,
    },
    Builtin {
        name: "install-controller",
        describe: install_controller_action,
        run: install_controller,
    },
    Builtin {
        name: "seed",
        describe: seed_action,
        run: seed,
    },
    Builtin {
        name: "secrets",
        describe: secrets_action,
        run: secrets,
    },
    Builtin {
        name: "trust-key",
        describe: trust_key_action,
        run: trust_key,
    },
    Builtin {
        name: "host-key",
        describe: host_key_action,
        run: host_key,
    },
    Builtin {
        name: "rename",
        describe: rename_action,
        run: rename,
    },
    Builtin {
        name: UPGRADE_SERVER,
        describe: upgrade_server_action,
        run: upgrade_server,
    },
    Builtin {
        name: "installer",
        describe: installer_action,
        run: installer,
    },
    Builtin {
        name: "start",
        describe: start_action,
        run: start,
    },
    Builtin {
        name: "stop",
        describe: stop_action,
        run: stop,
    },
    Builtin {
        name: "duplicate",
        describe: duplicate_action,
        run: duplicate,
    },
    Builtin {
        name: "remove",
        describe: remove_action,
        run: remove,
    },
];

/// The actions of the installation itself.
pub(super) static INSTALLATION: &[Builtin] = &[
    Builtin {
        name: "rollout",
        describe: rollout_action,
        run: rollout,
    },
    Builtin {
        name: "rotate-key",
        describe: rotate_key_action,
        run: rotate_key,
    },
    Builtin {
        name: "adopt",
        describe: adopt_action,
        run: adopt,
    },
];

/// berth's own actions that a simulated server does not simulate: each logs
/// in to a machine or works with this one's drives.
const NOT_SIMULATED: &[&str] = &[
    "seed",
    "trust-key",
    "host-key",
    "installer",
    "rollout",
    "rotate-key",
    "adopt",
];

/// The action as a simulated server offers it.
fn simulated(action: Action) -> Action {
    let off =
        berth_core::simulate::active().is_some() && NOT_SIMULATED.contains(&action.name.as_str());
    let why = format!("berth serve --simulate does not simulate {}", action.name);
    unavailable(action, off.then_some(why))
}

fn unavailable(mut action: Action, why: Option<String>) -> Action {
    if let Some(why) = why {
        action.available = false;
        action.why = why;
    }
    action
}

/// What a request to run an action carries; nothing is an empty one. Only
/// the action that takes bytes on its input is given them.
pub(super) fn parse(body: &Bytes) -> Answer<Act> {
    let act = parse_with_input(body)?;
    match act.input_b64 {
        Some(_) => Err(Fail::new(
            code::BAD_REQUEST,
            "This action takes no input_b64: only a node's upgrade-server with source=here does",
        )),
        None => Ok(act),
    }
}

fn parse_with_input(body: &Bytes) -> Answer<Act> {
    match body.is_empty() {
        true => Ok(Act::default()),
        false => serde_json::from_slice(body)
            .map_err(|error| Fail::new(code::BAD_REQUEST, error.to_string())),
    }
}

/// A job's answer is 202, with the job to follow; anything else is 200.
pub(super) fn answer(acted: Acted) -> (StatusCode, Json<Acted>) {
    let status = match acted.job {
        Some(_) => StatusCode::ACCEPTED,
        None => StatusCode::OK,
    };
    (status, Json(acted))
}

/// Why a host's controller did not list its actions.
enum Unlisted {
    /// It did not answer.
    Unanswered(String),
    /// It is older than this berth: what to say, with how to update it.
    Older(String),
}

impl Unlisted {
    fn said(self) -> String {
        match self {
            Unlisted::Unanswered(said) | Unlisted::Older(said) => said,
        }
    }
}

/// The actions a host's controller lists, asked for with a time limit. A
/// controller that does not know the call is asked its version, so that one
/// of an older berth is said to be old rather than broken.
async fn controller_actions(
    remote: &Remote,
    host_type: HostType,
) -> Result<Vec<HostAction>, Unlisted> {
    let link = remote.link.as_ref().ok_or_else(|| {
        Unlisted::Unanswered(remote.problem.clone().unwrap_or_else(|| "no link".into()))
    })?;
    let error =
        match tokio::time::timeout(Duration::from_secs(15), link.get::<Value>("/v1/actions")).await
        {
            Ok(Ok(listed)) => return Ok(berth_tui::host::list(listed, "actions")),
            Ok(Err(error)) => error.to_string(),
            Err(_) => return Err(Unlisted::Unanswered("no answer in time".into())),
        };
    Err(match remote.version().await {
        Ok(version) => berth_api::compat::older_controller(
            &remote.name,
            &version.version,
            version.api,
            host_type,
        )
        .map_or(Unlisted::Unanswered(error), Unlisted::Older),
        Err(_) => Unlisted::Unanswered(error),
    })
}

/// What a host's controller says the host is, when it answers.
async fn observed(remote: &Remote) -> Option<HostType> {
    remote
        .version()
        .await
        .ok()
        .as_ref()
        .and_then(|version| host_type::seen(Ok(version)))
}

/// A host's actions: its controller's, then berth's own. One that takes the
/// workstation's secrets on its input is berth's to run, with the server's.
fn host_list(seen: &Seen) -> Vec<Action> {
    let listed = seen.host_actions.unwrap_or(&[]);
    let own: Vec<Action> = HOST
        .iter()
        .filter(|builtin| {
            builtin.name != UPGRADE_SERVER || listed.iter().any(|kept| kept.name == UPGRADE_SERVER)
        })
        .map(|builtin| simulated((builtin.describe)(seen)))
        .collect();
    let mut actions: Vec<Action> = Action::of_host(listed)
        .into_iter()
        .filter(|action| !own.iter().any(|known| known.name == action.name))
        .filter(|action| {
            let kept = listed.iter().find(|kept| kept.name == action.name);
            kept.is_none_or(|kept| kept.input.is_none())
        })
        .collect();
    actions.extend(own);
    actions
}

/// The names of the secrets the server keeps, for what an action needs.
fn secret_names(state: &Serve) -> Vec<String> {
    Store::own(state.store.clone())
        .all_secret_names()
        .unwrap_or_default()
}

/// A host's saved profile; null for one the server was given without one.
fn profile_of(state: &Serve, name: &str) -> Value {
    saved_profile(state, name).map_or(Value::Null, |(_, profile)| profile)
}

/// The machine a provisioner made for the saved host `name`, or one planned
/// for it, as berth's record has it.
pub(super) fn machine_of(state: &Serve, name: &str) -> Option<Resource> {
    let place = Place::own(name).ok()?;
    let kept = berth_core::resource::of_place(&state.paths, &place).ok()??;
    let plugins = berth_core::provision::list_plugins(&state.paths);
    Some(berth_core::resource::info(
        &state.paths,
        &kept,
        &plugins,
        false,
    ))
}

/// Check that the caller may run the action and that it can run now, with
/// the arguments it needs.
pub(super) fn admit(state: &Serve, access: &Access, action: &Action, act: &Act) -> Answer<()> {
    state
        .auth
        .permits(access, &action.name, action.role)
        .map_err(|(kind, message)| Fail::new(kind, message))?;
    if !action.available {
        return Err(Fail::new(code::NOT_AVAILABLE, action.why.clone()));
    }
    for argument in &action.arguments {
        let given = act
            .arguments
            .get(&argument.name)
            .map_or("", |value| value.trim());
        if argument.required && given.is_empty() {
            return Err(Fail::new(
                code::BAD_REQUEST,
                format!("{} needs {}=VALUE", action.name, argument.name),
            ));
        }
    }
    Ok(())
}

fn no_action(subject: &str, action: &str, actions: &[Action]) -> Fail {
    let names: Vec<&str> = actions.iter().map(|known| known.name.as_str()).collect();
    Fail::new(
        code::NOT_FOUND,
        format!(
            "{subject} has no action {action}. Its actions: {}",
            names.join(", ")
        ),
    )
}

#[utoipa::path(get, path = "/v1/hosts/{name}/actions", summary = "What can be done to a host: the actions its controller lists, and berth's own.", tag = "hosts",
    params(("name" = String, Path, description = "A saved host")),
    responses((status = 200, description = "The actions, each saying whether it can run now", body = Vec<Action>),
              (status = 404, description = "No such host", body = ErrorBody)))]
pub async fn host_actions(
    State(state): App,
    access: Option<Extension<Access>>,
    Path(name): Path<String>,
) -> Answer<Json<Vec<Action>>> {
    state.refresh(false);
    let remote = remote(&state, &name)?;
    let profile = profile_of(&state, &name);
    // A host of the type ssh has no controller to ask.
    let asked = match host_type::of(&profile) {
        HostType::Ssh => None,
        host_type => Some(controller_actions(&remote, host_type).await),
    };
    let (listed, unanswered, older) = match &asked {
        Some(Ok(listed)) => (Some(listed.as_slice()), None, None),
        Some(Err(Unlisted::Unanswered(error))) => (None, Some(error.as_str()), None),
        Some(Err(Unlisted::Older(said))) => (None, None, Some(said.as_str())),
        None => (None, None, None),
    };
    let secrets = secret_names(&state);
    let observed = observed(&remote).await;
    let machine = machine_of(&state, &name);
    let power = host_type::power(&name, &profile, machine.as_ref());
    Ok(Json(host_list(&Seen {
        name: &name,
        profile: &profile,
        host_actions: listed,
        secrets: &secrets,
        host_type: host_type::of(&profile),
        observed,
        unanswered,
        older,
        local: access.is_none(),
        power: &power,
        machine: machine.as_ref(),
    })))
}

#[utoipa::path(post, path = "/v1/hosts/{name}/actions/{action}", summary = "Run one of a host's actions. One of berth's that changes something is a job; an action of the host's controller is a session there, which a job follows unless it needs a terminal.", tag = "hosts",
    params(("name" = String, Path, description = "A saved host"),
           ("action" = String, Path, description = "The action, as hosts.actions names it")),
    request_body(content = Option<Act>, description = "Its arguments, and a token that makes asking again safe"),
    responses((status = 202, description = "Started as a job; an action of its controller also says its session", body = Acted),
              (status = 200, description = "Answered at once: the result, or the terminal session to attach to", body = Acted),
              (status = 400, description = "An argument is missing, or the body is not one", body = ErrorBody),
              (status = 403, description = "The caller's role is below the action's", body = ErrorBody),
              (status = 404, description = "No such host or action", body = ErrorBody),
              (status = 409, description = "Something else is being done to the host", body = ErrorBody),
              (status = 501, description = "It cannot run on this host now; the message says why", body = ErrorBody),
              (status = 502, description = "The host could not be reached", body = ErrorBody)))]
pub async fn host_act(
    State(state): App,
    access: Option<Extension<Access>>,
    Path((name, action)): Path<(String, String)>,
    body: Bytes,
) -> Answer<(StatusCode, Json<Acted>)> {
    state.refresh(false);
    let act = match action == UPGRADE_SERVER {
        true => parse_with_input(&body)?,
        false => parse(&body)?,
    };
    let remote = remote(&state, &name)?;
    let profile = profile_of(&state, &name);
    // berth's own are described without asking the host, which they do themselves.
    let builtin = HOST.iter().find(|builtin| builtin.name == action);
    let ssh_only = host_type::of(&profile) == HostType::Ssh;
    let machine = machine_of(&state, &name);
    let power = host_type::power(&name, &profile, machine.as_ref());
    // Stopping a host goes through its controller when that is how it is stopped.
    let through = THROUGH_CONTROLLER.contains(&action.as_str())
        || (action == "stop" && power.power.stop_by == CONTROLLER);
    let (mut unanswered, mut older) = (None, None);
    let listed = match builtin {
        Some(_) if ssh_only || !through => None,
        // One that runs through the controller is described as the host answers.
        Some(_) => match controller_actions(&remote, host_type::of(&profile)).await {
            Ok(listed) => Some(listed),
            Err(Unlisted::Unanswered(error)) => {
                unanswered = Some(error);
                None
            }
            Err(Unlisted::Older(said)) => {
                older = Some(said);
                None
            }
        },
        None if ssh_only => Some(vec![]),
        None => Some(
            controller_actions(&remote, host_type::of(&profile))
                .await
                .map_err(|unlisted| match unlisted {
                    Unlisted::Older(said) => Fail::new(code::PRECONDITION, said),
                    unlisted => Fail::new(code::UNREACHABLE, unlisted.said()),
                })?,
        ),
    };
    let secrets = secret_names(&state);
    // Only installing the controller asks what the host is, which takes a login there.
    let observed = match action.as_str() {
        "install-controller" => observed(&remote).await,
        _ => None,
    };
    let actions = host_list(&Seen {
        name: &name,
        profile: &profile,
        host_actions: listed.as_deref(),
        secrets: &secrets,
        host_type: host_type::of(&profile),
        observed,
        unanswered: unanswered.as_deref(),
        older: older.as_deref(),
        local: access.is_none(),
        power: &power,
        machine: machine.as_ref(),
    });
    let found = actions
        .iter()
        .find(|known| known.name == action)
        .ok_or_else(|| match &older {
            // The controller's own actions are not known when it is too old to list them.
            Some(said) => Fail::new(code::PRECONDITION, said.clone()),
            None => no_action(&name, &action, &actions),
        })?;
    let local = access.is_none();
    admit(&state, &caller(access), found, &act)?;
    if local && builtin.is_some() {
        take_known_host_key(&state, &name);
    }
    let acted = match builtin {
        Some(builtin) => (builtin.run)(state, name, act).await?,
        None => run_action(&state, &remote, found, act, None).await?,
    };
    Ok(answer(acted))
}

#[utoipa::path(get, path = "/v1/installation/actions", summary = "What can be done to the installation as a whole, such as a roll-out over its hosts.", tag = "hosts",
    responses((status = 200, description = "The actions", body = Vec<Action>)))]
pub async fn list() -> Json<Vec<Action>> {
    Json(installation_list())
}

fn installation_list() -> Vec<Action> {
    let seen = Seen {
        name: "",
        profile: &Value::Null,
        host_actions: None,
        secrets: &[],
        host_type: HostType::Workstation,
        observed: None,
        unanswered: None,
        older: None,
        local: false,
        power: &Powered::default(),
        machine: None,
    };
    INSTALLATION
        .iter()
        .map(|builtin| simulated((builtin.describe)(&seen)))
        .collect()
}

#[utoipa::path(post, path = "/v1/installation/actions/{action}", summary = "Run one of the installation's actions, as a job.", tag = "hosts",
    params(("action" = String, Path, description = "The action, as installation.actions names it")),
    request_body(content = Option<Act>, description = "Its arguments, and a token that makes asking again safe"),
    responses((status = 202, description = "Started as a job", body = Acted),
              (status = 400, description = "An argument is wrong, or the body is not one", body = ErrorBody),
              (status = 403, description = "The caller's role is below the action's", body = ErrorBody),
              (status = 404, description = "No such action, or a host it names", body = ErrorBody),
              (status = 409, description = "Another one is running", body = ErrorBody)))]
pub async fn act(
    State(state): App,
    access: Option<Extension<Access>>,
    Path(action): Path<String>,
    body: Bytes,
) -> Answer<(StatusCode, Json<Acted>)> {
    let act = parse(&body)?;
    let actions = installation_list();
    let found = actions
        .iter()
        .find(|known| known.name == action)
        .ok_or_else(|| no_action("The installation", &action, &actions))?;
    admit(&state, &caller(access), found, &act)?;
    let builtin = INSTALLATION
        .iter()
        .find(|builtin| builtin.name == action)
        .ok_or_else(|| no_action("The installation", &action, &actions))?;
    Ok(answer((builtin.run)(state, String::new(), act).await?))
}

/// Why a job did not start: its subject's lock is taken, or something broke.
pub(super) fn refused(error: Box<dyn std::error::Error>) -> Fail {
    let message = error.to_string();
    match message.starts_with("Something else is being done to") {
        true => Fail::new(code::CONFLICT, message),
        false => internal(message),
    }
}

// ── Actions of a host ─────────────────────────────────────────────────────────

/// The host's controller through the server's link to it, for a job that
/// waits on it from a thread of its own.
struct Through {
    remote: Arc<Remote>,
    runtime: tokio::runtime::Handle,
}

impl Controller for Through {
    fn get(&mut self, path: &str) -> Result<Value, String> {
        let link = self.remote.link.as_ref().ok_or("no link to the host")?;
        self.runtime
            .block_on(link.get::<Value>(path))
            .map_err(|error| error.to_string())
    }

    fn stop(&mut self, id: &str) -> Result<(), String> {
        let link = self.remote.link.as_ref().ok_or("no link to the host")?;
        // TERM, so the session ends as it would and its transcript stays.
        let path = format!("/v1/sessions/{}/signal", client::encode(id));
        self.runtime
            .block_on(link.post::<Value>(&path, &json!({"signal": "TERM"})))
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

/// Run an action of the host as a session there, with `input` on its standard
/// input. A job follows one that needs no terminal, copying its output.
async fn run_action(
    state: &Arc<Serve>,
    remote: &Arc<Remote>,
    action: &Action,
    act: Act,
    input: Option<Vec<u8>>,
) -> Answer<Acted> {
    let Some(link) = &remote.link else {
        let problem = remote.problem.clone().unwrap_or_else(|| "no link".into());
        return Err(Fail::new(code::UNREACHABLE, problem));
    };
    let mut body = json!({"arguments": act.arguments});
    if action.terminal {
        (body["rows"], body["cols"]) =
            (json!(act.rows.unwrap_or(24)), json!(act.cols.unwrap_or(80)));
    }
    if let Some(input) = input {
        body["input_b64"] = json!(b64(&input));
    }
    let path = format!("/v1/actions/{}", client::encode(&action.name));
    // One to attach to is the session alone, with no job to find again.
    if action.terminal {
        let session: Session = link.post(&path, &body).await?;
        return Ok(Acted {
            session: Some(session),
            ..Acted::default()
        });
    }
    let mut request = Request::new(Subject::Host(remote.name.clone()), &action.name);
    request.arguments = Argument::shown(&action.arguments, &act.arguments);
    request.token = act.token;
    request.role = action.role;
    // An action is the host's own session, and runs beside the others there.
    request.changes = false;
    // The job is written first, so that of the callers with one token only
    // the one that wrote it starts a session; the others find its job.
    let ticket = match state.runner.reserve(request).map_err(refused)? {
        Reserved::Found(job) => {
            return Ok(Acted {
                job: Some(job_info(&state.paths, job)),
                ..Acted::default()
            });
        }
        Reserved::New(ticket) => ticket,
    };
    let session: Session = match link.post(&path, &body).await {
        Ok(session) => session,
        Err(error) => {
            ticket.fail(&format!("The host did not start {}: {error}", action.name));
            return Err(error.into());
        }
    };
    let controller = Box::new(Through {
        remote: remote.clone(),
        runtime: tokio::runtime::Handle::current(),
    });
    let work = Work::Session {
        controller,
        id: session.id.clone(),
    };
    let job = ticket.run(work);
    Ok(Acted {
        job: Some(job_info(&state.paths, job)),
        session: Some(session),
        result: None,
    })
}

// ── berth's own actions on a host ───────────────────────────────────────────

fn set_up(name: &str, label: &str, help: &str) -> Action {
    Action {
        name: name.into(),
        label: label.into(),
        help: help.into(),
        group: "Set-Up".into(),
        role: Role::Admin,
        ..Action::default()
    }
}

fn apply_action(seen: &Seen) -> Action {
    let action = Action {
        confirm: Some(format!(
            "Apply the configuration of {} to it now?",
            seen.name
        )),
        ..set_up(
            "apply",
            "Apply this configuration",
            "Run the installer role against the host again, from the server, with the settings of its profile: for example after changing what is installed. It also installs or updates berth's own tools on the host. It runs as a job of the server.",
        )
    };
    let action = host_type::fit(action, seen.name, seen.host_type, Capability::Apply);
    logs_in_alone(seen, action)
}

/// An action whose job logs in to the host, unavailable when it would need
/// a password typed: a job cannot type one, so it is the server's secret.
fn logs_in_alone(seen: &Seen, action: Action) -> Action {
    if !action.available {
        return action;
    }
    let wants = profile::Access::of(seen.profile).auth == Auth::Password;
    let kept = Place::own(seen.name)
        .is_ok_and(|place| seen.secrets.contains(&keys::password_secret(&place)));
    unavailable(
        action,
        (wants && !kept).then(|| {
            format!(
                "{} logs in with an SSH password, and the server has none stored: berth secret set ssh/{}, or install a key with: berth key trust {}",
                seen.name, seen.name, seen.name
            )
        }),
    )
}

fn install_controller_action(seen: &Seen) -> Action {
    let action = host_type::install_controller_action(seen.name, seen.host_type, seen.observed);
    let action = match seen.profile.is_null() {
        true => unavailable(
            action,
            Some(format!("{} has no profile on the server", seen.name)),
        ),
        false => action,
    };
    logs_in_alone(seen, action)
}

fn install_controller(state: Arc<Serve>, name: String, act: Act) -> Run {
    Box::pin(async move {
        let remote = remote(&state, &name)?;
        let bad = |error: Box<dyn std::error::Error>| turned_down(error);
        let host =
            own_place(&name).and_then(|place| Host::load_at(&state.paths, place).map_err(bad))?;
        let password = act
            .sudo_password
            .or_else(|| remote.password.clone())
            .or_else(|| host.kept_sudo_password(&state.paths));
        if host.asks_password() && password.is_none() {
            return Err(Fail::new(
                code::NOT_AVAILABLE,
                format!("{name} asks for a sudo password: send it as sudo_password"),
            ));
        }
        let mut request = Request::new(Subject::Host(name), "install-controller");
        request.token = act.token;
        request.secrets = password.iter().cloned().collect();
        let paths = state.paths.clone();
        let work = match berth_core::simulate::active() {
            Some(simulation) => Work::Function(Box::new(move |out| {
                let done = simulation.install_controller(&paths, &host, out);
                // What it was seen to be before is not so any more.
                *remote.seen.lock().unwrap() = None;
                done
            })),
            None => Work::Function(Box::new(move |out| {
                apply::install_controller(&paths, &host, password.as_deref(), out)
            })),
        };
        let job = state.runner.start(request, work).map_err(refused)?;
        Ok(Acted {
            job: Some(job_info(&state.paths, job)),
            ..Acted::default()
        })
    })
}

fn apply(state: Arc<Serve>, name: String, act: Act) -> Run {
    Box::pin(async move {
        let remote = remote(&state, &name)?;
        let bad = |error: Box<dyn std::error::Error>| turned_down(error);
        let host =
            own_place(&name).and_then(|place| Host::load_at(&state.paths, place).map_err(bad))?;
        let password = act
            .sudo_password
            .or_else(|| remote.password.clone())
            .or_else(|| host.kept_sudo_password(&state.paths));
        if host.asks_password() && password.is_none() {
            return Err(Fail::new(
                code::NOT_AVAILABLE,
                format!("{name} asks for a sudo password: send it as sudo_password"),
            ));
        }
        if let Some(simulation) = berth_core::simulate::active() {
            let mut request = Request::new(Subject::Host(name), "apply");
            request.token = act.token;
            let paths = state.paths.clone();
            let work = Work::Function(Box::new(move |out| {
                let done = simulation.apply(&paths, &host, out);
                *remote.seen.lock().unwrap() = None;
                done
            }));
            let job = state.runner.start(request, work).map_err(refused)?;
            return Ok(Acted {
                job: Some(job_info(&state.paths, job)),
                ..Acted::default()
            });
        }
        let prepared = apply::prepare(&state.paths, &host, password.as_deref()).map_err(bad)?;
        let notes = prepared.notes;
        let mut request = Request::new(Subject::Host(name.clone()), "apply");
        request.token = act.token;
        request.secrets = password.into_iter().collect();
        let program = berth_core::runner::Program {
            command: prepared.command,
            input: None,
            files: prepared.files,
        };
        let paths = state.paths.clone();
        let work = Work::Function(Box::new(move |out| {
            for note in &notes {
                let _ = writeln!(out, "{note}");
            }
            if berth_core::runner::run_program(program, out) != Some(0) {
                return false;
            }
            if let Err(error) = apply::settle_with(&paths, &host, out) {
                let _ = writeln!(
                    out,
                    "! {name} is applied, but its profile was not updated: {error}"
                );
            }
            true
        }));
        let job = state.runner.start(request, work).map_err(refused)?;
        Ok(Acted {
            job: Some(job_info(&state.paths, job)),
            ..Acted::default()
        })
    })
}

fn seed_action(seen: &Seen) -> Action {
    let action = Action {
        changes: false,
        ..set_up(
            "seed",
            "Render the cloud seed",
            "Render the cloud-init user data that installs the host on a new machine, for a provider's field of that name. It answers with the seed.",
        )
    };
    host_type::fit(action, seen.name, seen.host_type, Capability::Seed)
}

fn seed(state: Arc<Serve>, name: String, _: Act) -> Run {
    Box::pin(async move {
        let (paths, registries) = (state.paths.clone(), Store::own(state.store.clone()));
        // It may publish the release and runs the seed builder, and waits for both.
        let seed = tokio::task::spawn_blocking(move || {
            berth_core::seed::cloud(&paths, &registries, &name, false).map_err(Said::from)
        })
        .await
        .map_err(internal)?
        .map_err(turned_down)?;
        // A seed's key joins the tailnet for a day, which whoever starts the machine needs to know.
        Ok(Acted {
            result: Some(json!({"user_data": seed.user_data, "tailnet": seed.tailnet})),
            ..Acted::default()
        })
    })
}

/// Why an action that works with this machine's own drives and files is
/// not offered to a caller elsewhere.
fn only_here(what: &str) -> String {
    format!(
        "{what} is done on the machine berth serve runs on: there, with berth ui or berth host iso"
    )
}

fn installer_action(seen: &Seen) -> Action {
    // The drives are looked for only for a caller who can write them.
    let drives = match seen.local {
        true => berth_core::discovery::usb_drives(),
        false => vec![],
    };
    let kept = seen.profile["builder"]["usb_device"].as_str().unwrap_or("");
    let default = match (kept, drives.as_slice()) {
        ("", [(_, only)]) => only.clone(),
        (kept, _) => kept.to_owned(),
    };
    let found = match drives.is_empty() {
        true => "No USB drive is connected now.".to_owned(),
        false => format!(
            "Connected now: {}.",
            drives
                .iter()
                .map(|(label, _)| label.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        ),
    };
    let action = Action {
        arguments: vec![
            Argument {
                name: "device".into(),
                label: "USB drive to write it to".into(),
                default,
                help: format!(
                    "The whole USB drive the finished ISO is written to, such as /dev/sdb. It is erased. Empty builds the ISO only. {found}"
                ),
                ..Argument::default()
            },
            Argument {
                name: "password".into(),
                label: "Your sudo password, to write the drive".into(),
                help: "Writing the drive runs as root through sudo. Empty when sudo needs none or no drive is written.".into(),
                secret: true,
                ..Argument::default()
            },
        ],
        ..set_up(
            "installer",
            "Build the USB installer",
            "For a blank machine: build an installer image from Ubuntu Server's ISO that installs and sets up the host without questions, and write it to a USB drive. Without an ISO in the profile the current one is downloaded and checked first. Booting the drive erases the machine's chosen disk.",
        )
    };
    let action = host_type::fit(action, seen.name, seen.host_type, Capability::Apply);
    match (seen.local, seen.profile.is_null()) {
        (false, _) => unavailable(action, Some(only_here("Building the USB installer"))),
        (_, true) => unavailable(
            action,
            Some(format!("{} has no profile on the server", seen.name)),
        ),
        _ => action,
    }
}

fn installer(state: Arc<Serve>, name: String, act: Act) -> Run {
    Box::pin(async move {
        let bad = |error: Box<dyn std::error::Error>| turned_down(error);
        let text = |key: &str| act.arguments.get(key).map_or("", |value| value.trim());
        let device = text("device").to_owned();
        let password = act.arguments.get("password").cloned().unwrap_or_default();
        // Only a whole USB drive that is connected now is written, as the script checks again.
        if !device.is_empty()
            && !berth_core::discovery::usb_drives()
                .iter()
                .any(|(_, path)| *path == device)
        {
            return Err(Fail::new(
                code::BAD_REQUEST,
                format!("{device} is no USB drive connected to this machine"),
            ));
        }
        let profile = profile_of(&state, &name);
        let ubuntu = berth_core::seed::ubuntu_iso(&profile);
        if let Some(iso) = &ubuntu
            && !iso.is_file()
        {
            return Err(Fail::new(
                code::PRECONDITION,
                format!("The profile's ISO {} is not there", iso.display()),
            ));
        }
        // The host must be one an installer can be built for before a job starts.
        let paths = state.paths.clone();
        let fetch = berth_core::seed::fetch_iso(&paths).map_err(bad)?;
        let mut request = Request::new(Subject::Host(name.clone()), "installer");
        request.token = act.token;
        request.secrets = Some(password.clone())
            .filter(|password| !password.is_empty())
            .into_iter()
            .collect();
        let work = Work::Function(Box::new(move |out| {
            let ubuntu = match ubuntu {
                Some(iso) => iso,
                None => {
                    let _ = writeln!(out, "Downloading Ubuntu's installer ISO…");
                    match fetched(fetch, out) {
                        Some(iso) => iso,
                        None => return false,
                    }
                }
            };
            let built = match berth_core::seed::installer(&paths, &name, &ubuntu) {
                Ok(built) => built,
                Err(error) => {
                    let _ = writeln!(out, "✗ {error}");
                    return false;
                }
            };
            // The builder reads both files while it runs.
            let (iso, _profile, key) = (built.iso.clone(), built.profile, built.key);
            let program = berth_core::runner::Program {
                command: built.command,
                input: None,
                files: vec![],
            };
            if berth_core::runner::run_program(program, out) != Some(0) {
                return false;
            }
            if let Some(key) = &key {
                let _ = writeln!(out, "{}", key.note());
            }
            if device.is_empty() {
                let _ = writeln!(out, "Built {}", iso.display());
                return true;
            }
            let write =
                match berth_core::seed::write_usb(&paths, &iso, &device, !password.is_empty()) {
                    Ok(write) => write,
                    Err(error) => {
                        let _ = writeln!(out, "✗ {error}");
                        return false;
                    }
                };
            let program = berth_core::runner::Program {
                command: write,
                input: (!password.is_empty()).then(|| format!("{password}\n").into_bytes()),
                files: vec![],
            };
            let written = berth_core::runner::run_program(program, out) == Some(0);
            if written {
                let _ = writeln!(
                    out,
                    "Next: boot the machine from the USB drive. Booting it erases the chosen disk."
                );
            }
            written
        }));
        let job = state.runner.start(request, work).map_err(refused)?;
        Ok(Acted {
            job: Some(job_info(&state.paths, job)),
            ..Acted::default()
        })
    })
}

/// Download Ubuntu's ISO with the builder: where it is, which it says last.
fn fetched(
    command: std::process::Command,
    out: &mut berth_core::runner::Output,
) -> Option<std::path::PathBuf> {
    let mut command = command;
    let output = match command.stderr(std::process::Stdio::piped()).output() {
        Ok(output) => output,
        Err(error) => {
            let _ = writeln!(out, "✗ python3: {error}");
            return None;
        }
    };
    let _ = out.write_all(&output.stderr);
    let said = String::from_utf8_lossy(&output.stdout);
    let iso = said
        .lines()
        .last()
        .map(|line| std::path::PathBuf::from(line.trim()));
    match (output.status.success(), iso) {
        (true, Some(iso)) if iso.is_file() => Some(iso),
        _ => {
            let _ = writeln!(out, "✗ Downloading Ubuntu's installer ISO failed");
            None
        }
    }
}

fn secrets_action(seen: &Seen) -> Action {
    let kept = seen
        .host_actions
        .and_then(|listed| listed.iter().find(|kept| kept.name == "secrets"));
    let mut action = set_up(
        "secrets",
        "Send secrets",
        "Send the host the secrets and registry logins its profile names, from the server's own.",
    );
    action.runs = Runs::Host;
    if let Some(kept) = kept {
        action.label = kept.label.clone();
        action.help = kept.help.clone();
        action.group = kept.group.clone();
    }
    if let Err(refusal) = host_type::check(seen.name, seen.host_type, Capability::Secrets) {
        return unavailable(action, Some(refusal.why));
    }
    let why = match (seen.host_actions, kept, seen.unanswered) {
        _ if seen.older.is_some() => seen.older.map(str::to_owned),
        (_, _, Some(error)) => Some(format!(
            "The controller on {} does not answer, and the secrets go through it: {error}",
            seen.name
        )),
        (Some(_), None, _) => Some(format!(
            "The controller on {} has no secrets action yet: apply its configuration",
            seen.name
        )),
        _ => None,
    };
    unavailable(action, why)
}

fn secrets(state: Arc<Serve>, name: String, act: Act) -> Run {
    Box::pin(async move {
        let remote = remote(&state, &name)?;
        let bad = |error: Box<dyn std::error::Error>| turned_down(error);
        let host = Host::load_at(&state.paths, own_place(&name)?).map_err(bad)?;
        let payload = Store::own(state.store.clone())
            .host_payload(&host.config)
            .map_err(bad)?;
        let action = Action {
            name: "secrets".into(),
            ..Action::default()
        };
        let input = format!("{payload}\n").into_bytes();
        run_action(&state, &remote, &action, act, Some(input)).await
    })
}

/// The arguments of `upgrade-server`.
fn upgrade_arguments() -> Vec<Argument> {
    vec![
        Argument {
            name: "version".into(),
            label: "Version".into(),
            help: "The version of berth to run, as its image is tagged: this berth's own unless another is given. With source=here it is that of the berth sent.".into(),
            default: berth_api::VERSION.into(),
            ..Argument::default()
        },
        Argument {
            name: upgrade_server::SOURCE.into(),
            label: "Source".into(),
            help: "release pulls the published image ghcr.io/helightdev/berth:VERSION, and takes a server built from a berth back to it. here builds the image berth:VERSION on the node from the berth that runs the action, which berth run sends from Linux.".into(),
            default: upgrade_server::RELEASE.into(),
            values: vec![upgrade_server::RELEASE.into(), upgrade_server::HERE.into()],
            ..Argument::default()
        },
    ]
}

fn upgrade_server_action(seen: &Seen) -> Action {
    let kept = seen
        .host_actions
        .and_then(|listed| listed.iter().find(|kept| kept.name == UPGRADE_SERVER));
    let mut action = set_up(
        UPGRADE_SERVER,
        "Upgrade the berth server",
        "Run the berth server in the container on this node from the image of this berth's version, with the same data and the same options as the running container. With source=here the node builds the image from the berth that runs the action instead, which sends itself: berth run NODE upgrade-server source=here, from berth for Linux. It is refused when the container has an option it cannot carry over, and the old container is started again when the new one does not come up. A server that upgrades itself restarts during the job.",
    );
    action.group = "Server".into();
    action.runs = Runs::Host;
    action.confirm = Some(format!(
        "Upgrade the berth server on {} to {}? It restarts, and its jobs end",
        seen.name,
        berth_api::VERSION
    ));
    action.arguments = upgrade_arguments();
    if let Some(kept) = kept {
        action.help = match kept.help.is_empty() {
            true => action.help,
            false => kept.help.clone(),
        };
    }
    let why = match (seen.host_type, seen.unanswered) {
        (HostType::Node, _) if seen.older.is_some() => seen.older.map(str::to_owned),
        (HostType::Node, Some(error)) => Some(format!(
            "The controller on {} does not answer, and the upgrade runs through it: {error}",
            seen.name
        )),
        (HostType::Node, None) => None,
        (other, _) => Some(format!(
            "{} is a host of the type {other}: only a node's controller upgrades a berth server it runs. A server of berth 0.5, and one on no node, is updated by its recipe: {}",
            seen.name,
            berth_api::compat::UPGRADE_DOCS
        )),
    };
    unavailable(action, why)
}

/// What `upgrade-server` passes to the controller: its arguments, and the
/// build context with source=here. A release is asked for with the version
/// alone, as a controller of an older berth knows no source.
fn upgrade_request(act: &mut Act) -> Answer<Option<Vec<u8>>> {
    let given = |name: &str| {
        act.arguments
            .get(name)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    };
    let source = given(upgrade_server::SOURCE).unwrap_or_else(|| upgrade_server::RELEASE.into());
    let version = given("version");
    let bad = |message: String| Fail::new(code::BAD_REQUEST, message);
    let input = match (source.as_str(), act.input_b64.take()) {
        (upgrade_server::RELEASE, None) => None,
        (upgrade_server::RELEASE, Some(_)) => {
            return Err(bad(
                "A release is pulled: source=release takes no build context".into(),
            ));
        }
        (upgrade_server::HERE, Some(context)) => {
            let context = berth_api::unb64(&context)
                .filter(|context| !context.is_empty())
                .ok_or_else(|| bad("input_b64 is not a build context in base64".into()))?;
            if version.is_none() {
                return Err(bad(
                    "source=here needs the version of the berth it sends".into()
                ));
            }
            Some(context)
        }
        (upgrade_server::HERE, None) => {
            return Err(bad(
                "source=here needs the build context of the berth that runs it, which berth run NODE upgrade-server source=here sends from berth for Linux".into(),
            ));
        }
        (other, _) => return Err(bad(format!("source is release or here, not {other}"))),
    };
    let version = version.unwrap_or_else(|| berth_api::VERSION.to_owned());
    act.arguments = [("version".to_owned(), version)].into();
    if input.is_some() {
        act.arguments
            .insert(upgrade_server::SOURCE.into(), upgrade_server::HERE.into());
    }
    Ok(input)
}

fn upgrade_server(state: Arc<Serve>, name: String, mut act: Act) -> Run {
    Box::pin(async move {
        let remote = remote(&state, &name)?;
        let input = upgrade_request(&mut act)?;
        // A controller of an older role cannot build: its action has no source.
        if input.is_some() {
            let profile = profile_of(&state, &name);
            let listed = controller_actions(&remote, host_type::of(&profile)).await;
            let builds = listed.iter().flatten().any(|kept| {
                kept.name == UPGRADE_SERVER
                    && kept
                        .arguments
                        .iter()
                        .any(|argument| argument.name == upgrade_server::SOURCE)
            });
            if !builds {
                return Err(Fail::new(
                    code::PRECONDITION,
                    format!(
                        "The controller on {name} cannot build the server's image yet: apply its configuration first, with berth apply {name}"
                    ),
                ));
            }
        }
        let action = Action {
            name: UPGRADE_SERVER.into(),
            arguments: upgrade_arguments(),
            ..Action::default()
        };
        run_action(&state, &remote, &action, act, input).await
    })
}

// ── Its power, its copy and removing it ─────────────────────────────────────

/// What stops a host that declares in its profile that its controller does.
const CONTROLLER: &str = "controller";

fn power_action(seen: &Seen, verb: &str) -> Action {
    let (label, help, by) = match verb {
        "start" => (
            "Start",
            "Start the host's machine: through the provisioner that made it. A host no provisioner made cannot be started by berth yet.",
            seen.power.power.start_by.as_str(),
        ),
        _ => (
            "Stop",
            "Stop the host's machine: through the provisioner that made it, which keeps its disk, or as the host's profile declares in power.stop, such as by its controller shutting it down. Its sessions end.",
            seen.power.power.stop_by.as_str(),
        ),
    };
    let mut action = Action {
        name: verb.into(),
        label: label.into(),
        help: help.into(),
        group: "Power".into(),
        ..Action::default()
    };
    if verb == "stop" {
        action.confirm = Some(format!("Stop {}? Its sessions end.", seen.name));
    }
    if by == CONTROLLER {
        action.arguments = vec![flag(
            "force",
            "Although sessions run",
            "Shut it down although sessions run, which ends them. Without it the controller refuses while any runs.",
        )];
    }
    let why = match seen.power.why(verb) {
        Some(why) => Some(why.to_owned()),
        None if by == CONTROLLER && seen.older.is_some() => seen.older.map(str::to_owned),
        None if by == CONTROLLER => seen.unanswered.map(|error| {
            format!(
                "The controller on {} does not answer, and it shuts the host down: {error}",
                seen.name
            )
        }),
        None => None,
    };
    unavailable(action, why)
}

fn start_action(seen: &Seen) -> Action {
    power_action(seen, "start")
}

fn stop_action(seen: &Seen) -> Action {
    power_action(seen, "stop")
}

fn start(state: Arc<Serve>, name: String, act: Act) -> Run {
    power(state, name, act, "start")
}

fn stop(state: Arc<Serve>, name: String, act: Act) -> Run {
    power(state, name, act, "stop")
}

/// Start or stop a host, as a job: one of its machine's through the
/// provisioner, or one of the host's that asks its controller to power off.
fn power(state: Arc<Serve>, name: String, act: Act, verb: &'static str) -> Run {
    Box::pin(async move {
        let profile = profile_of(&state, &name);
        let machine = machine_of(&state, &name);
        let powered = host_type::power(&name, &profile, machine.as_ref());
        if let Some(why) = powered.why(verb) {
            return Err(Fail::new(code::NOT_AVAILABLE, why.to_owned()));
        }
        let by = match verb {
            "start" => &powered.power.start_by,
            _ => &powered.power.stop_by,
        };
        if let (Some(machine), "provisioner") = (&machine, by.as_str()) {
            let given = berth_core::resource::Given::default();
            return super::resources::run_action(
                state,
                machine.id.clone(),
                verb.into(),
                given,
                act.token,
            )
            .await;
        }
        let force = yes(&act, "force")?;
        let remote = remote(&state, &name)?;
        let mut request = Request::new(Subject::Host(name.clone()), verb);
        request.token = act.token;
        request.role = Role::Operate;
        request.arguments = Argument::shown(&stop_action_arguments(), &act.arguments);
        let runtime = tokio::runtime::Handle::current();
        let work = Work::Function(Box::new(move |out| {
            let Some(link) = &remote.link else {
                let _ = writeln!(
                    out,
                    "✗ {name} cannot be reached: {}",
                    remote.problem.clone().unwrap_or_default()
                );
                return false;
            };
            let _ = writeln!(out, "▸ Asking the controller on {name} to shut it down");
            let path = format!("/v1/power/poweroff?force={force}");
            match runtime.block_on(link.post::<Value>(&path, &json!({}))) {
                Ok(_) => {
                    let _ = writeln!(
                        out,
                        "✓ {name} is shutting down. It is off in a moment, and stops answering"
                    );
                    true
                }
                Err(error) => {
                    let _ = writeln!(out, "✗ {error}");
                    false
                }
            }
        }));
        let job = state.runner.start(request, work).map_err(refused)?;
        Ok(Acted {
            job: Some(job_info(&state.paths, job)),
            ..Acted::default()
        })
    })
}

/// The arguments stopping a host through its controller takes.
fn stop_action_arguments() -> Vec<Argument> {
    vec![flag("force", "Although sessions run", "")]
}

/// The machine of a host that a provisioner made, not one only planned.
fn made(machine: Option<&Resource>) -> Option<&Resource> {
    machine.filter(|machine| !machine.external_id.is_empty())
}

fn remove_action(seen: &Seen) -> Action {
    let name = seen.name;
    let (help, confirm) = match made(seen.machine) {
        Some(machine) => (
            format!(
                "Forget {name}: its profile, the passwords berth generated and keeps for it, and its host keys. With destroy, {} also destroys its machine {} with its disk, and berth removes its device from the tailnet. Without destroy the machine and its device are left as they are, and berth keeps it as a resource of {} with no host, which berth resource remove deletes later.",
                machine.provisioner, machine.external_id, machine.provisioner
            ),
            format!(
                "Remove {name}? With destroy=yes {} destroys its machine {} and the disk it has; without, the machine is left as it is.",
                machine.provisioner, machine.external_id
            ),
        ),
        None => (
            format!(
                "Forget {name}: its profile, the passwords berth generated and keeps for it, and its host keys. The machine is left as it is: no provisioner made it, so there is nothing for berth to destroy."
            ),
            format!("Forget {name}? The machine is left as it is."),
        ),
    };
    let destroy = match made(seen.machine) {
        Some(machine) => format!(
            "Destroy the machine {} made, {}, with its disk, and remove its device from the tailnet. It cannot be undone.",
            machine.provisioner, machine.external_id
        ),
        None => "Destroy the machine a provisioner made for the host. This host has none, so yes is refused.".into(),
    };
    let action = Action {
        name: "remove".into(),
        label: "Remove".into(),
        help,
        group: "Host".into(),
        role: Role::Admin,
        confirm: Some(confirm),
        arguments: vec![flag("destroy", "Destroy its machine", &destroy)],
        ..Action::default()
    };
    let why = seen
        .profile
        .is_null()
        .then(|| format!("{name} has no profile on the server"));
    unavailable(action, why)
}

/// Why `destroy` cannot be done to the host `name`, whose machine is `machine`.
fn destroy_refused(name: &str, machine: Option<&Resource>) -> Option<String> {
    match (machine, made(machine)) {
        (_, Some(_)) => None,
        (Some(_), None) => Some(format!(
            "{name}'s machine is not made yet, so there is nothing to destroy. Remove it without destroy to forget it with its plan"
        )),
        (None, _) => Some(format!(
            "No provisioner made {name}, so there is nothing for berth to destroy. Remove it without destroy to forget it; the machine is left as it is"
        )),
    }
}

fn remove(state: Arc<Serve>, name: String, act: Act) -> Run {
    Box::pin(async move {
        let destroy = yes(&act, "destroy")?;
        let place = own_place(&name)?;
        let (_, profile) = saved_profile(&state, &name)?;
        let machine = machine_of(&state, &name);
        if destroy && let Some(why) = destroy_refused(&name, machine.as_ref()) {
            return Err(Fail::new(code::NOT_AVAILABLE, why));
        }
        let mut request = Request::new(Subject::Host(name.clone()), "remove");
        request.token = act.token;
        request.arguments = Argument::shown(&[flag("destroy", "", "")], &act.arguments);
        // Destroying takes the machine's lock, which is the host's.
        let acting = match (destroy, &machine) {
            (true, Some(machine)) => {
                let (paths, registries, id) = (
                    state.paths.clone(),
                    Store::own(state.store.clone()),
                    machine.id.clone(),
                );
                let mut acting = tokio::task::spawn_blocking(move || {
                    berth_core::resource::prepare_action(
                        &paths,
                        &registries,
                        &id,
                        berth_core::resource::REMOVE,
                        &berth_core::resource::Given::default(),
                    )
                    .map_err(Said::from)
                })
                .await
                .map_err(internal)?
                .map_err(super::resources::refusal)?;
                request.held = acting.take_lock();
                request.secrets = acting.secrets();
                Some(acting)
            }
            _ => None,
        };
        let (serve, runtime) = (state.clone(), tokio::runtime::Handle::current());
        let work = Work::Function(Box::new(move |out| {
            let paths = &serve.paths;
            if let Some(acting) = acting {
                let (provisioner, id) = (
                    acting.kept().record.provisioner.clone(),
                    acting.kept().record.external_id.clone(),
                );
                match acting.run(paths, &(serve.provision)(), out) {
                    // What it says of the profile staying is not so here.
                    Ok(_) => {
                        let _ = writeln!(out, "✓ {provisioner} destroyed {id}");
                        // The machine is gone, so its device on the tailnet goes too; a
                        // device berth cannot remove is named, and the host is forgotten all the same.
                        if let Some(line) =
                            berth_core::tailnet::remove_device(paths, &place, &profile)
                        {
                            let _ = writeln!(out, "{line}");
                        }
                    }
                    Err(error) => {
                        let _ = writeln!(out, "✗ {error}");
                        let _ = writeln!(out, "{name} is kept, as its machine is");
                        return false;
                    }
                }
            } else if let Ok(Some(mut kept)) = berth_core::resource::of_place(paths, &place)
                && !kept.record.external_id.is_empty()
            {
                // The record stays, without the host, so the machine can still be deleted.
                if let Err(error) = kept.detach(paths) {
                    let _ = writeln!(out, "✗ {error}");
                    return false;
                }
                let _ = writeln!(
                    out,
                    "{name}'s machine {} of {} is left as it is. berth keeps it as {}: berth resource remove {} deletes it",
                    kept.record.external_id,
                    kept.record.provisioner,
                    kept.record.id,
                    kept.record.id
                );
            }
            let _inside = runtime.enter();
            match berth_core::profiles::remove(paths, &place) {
                Ok(()) => {
                    serve.forget(&name);
                    serve.notice();
                    let _ = writeln!(out, "✓ Removed {name}");
                    true
                }
                Err(error) => {
                    let _ = writeln!(out, "✗ {error}");
                    false
                }
            }
        }));
        let job = state.runner.start(request, work).map_err(refused)?;
        Ok(Acted {
            job: Some(job_info(&state.paths, job)),
            ..Acted::default()
        })
    })
}

fn duplicate_action(seen: &Seen) -> Action {
    let name = seen.name;
    let made_by = seen
        .machine
        .map(|machine| machine.provisioner.as_str())
        .unwrap_or("");
    let help = match made_by {
        "" => format!(
            "Save a copy of {name}'s profile as a new host, with its own hostname and without what belongs to the one machine: its address, host keys, the passwords berth generated, how it is known on the tailnet and what was applied to it. The disk is not cloned: the new host is installed from the profile, as any other."
        ),
        provisioner => format!(
            "Save a copy of {name}'s profile as a new host, with its own hostname and without what belongs to the one machine: its address, host keys, the passwords berth generated, how it is known on the tailnet and what was applied to it. Its machine is to be made by {provisioner} with the same settings: with create, at once, or later on its Run tab or with berth host create NAME. The disk is not cloned: the new machine is installed from the profile."
        ),
    };
    let mut arguments = vec![Argument {
        name: "name".into(),
        label: "New host's name".into(),
        help: "The copy's name in berth, and its hostname: letters, digits, `-`, `_` and `.`."
            .into(),
        required: true,
        ..Argument::default()
    }];
    if !made_by.is_empty() {
        arguments.push(flag(
            "create",
            "Create its machine now",
            &format!("Have {made_by} make the copy's machine at once, in the same job."),
        ));
    }
    let action = Action {
        name: "duplicate".into(),
        label: "Duplicate".into(),
        help,
        group: "Host".into(),
        role: Role::Admin,
        arguments,
        ..Action::default()
    };
    let why = seen
        .profile
        .is_null()
        .then(|| format!("{name} has no profile on the server"));
    unavailable(action, why)
}

fn duplicate(state: Arc<Serve>, name: String, act: Act) -> Run {
    Box::pin(async move {
        let new = given(&act, "name").unwrap_or_default().to_owned();
        let create = yes(&act, "create")?;
        let to = own_place(&new)?;
        let from = own_place(&name)?;
        saved_profile(&state, &name)?;
        if to.load(&state.paths).map_err(internal)?.is_some() {
            return Err(Fail::new(
                code::CONFLICT,
                format!("There is a host {new} already"),
            ));
        }
        let machine = machine_of(&state, &name);
        if create && machine.is_none() {
            return Err(Fail::new(
                code::NOT_AVAILABLE,
                format!(
                    "No provisioner made {name}, so its copy has none to be made with: create is for a host a provisioner made"
                ),
            ));
        }
        let mut request = Request::new(Subject::Host(name.clone()), "duplicate");
        request.token = act.token;
        request.host = format!("{name}, {new}");
        request.arguments = Argument::shown(
            &[
                Argument {
                    name: "name".into(),
                    ..Argument::default()
                },
                flag("create", "", ""),
            ],
            &act.arguments,
        );
        // The copy takes the lock of its own name itself.
        request.changes = false;
        let (serve, runtime) = (state.clone(), tokio::runtime::Handle::current());
        let work = Work::Function(Box::new(move |out| {
            let paths = &serve.paths;
            let planned = match berth_core::profiles::duplicate(paths, &from, &new) {
                Ok((_, planned)) => planned,
                Err(error) => {
                    let _ = writeln!(out, "✗ {error}");
                    return false;
                }
            };
            {
                let _inside = runtime.enter();
                serve.refresh(false);
                serve.notice();
            }
            let _ = writeln!(out, "✓ Saved {new}, a copy of {name}'s profile");
            let Some(planned) = planned else {
                let _ = writeln!(
                    out,
                    "Next: give it an address and apply it (berth host set {new} address=ADDRESS, then berth host apply {new}), or install it another way: berth ui {new}"
                );
                return true;
            };
            let provisioner = planned.record.provisioner.clone();
            if !create {
                let _ = writeln!(
                    out,
                    "Its machine is to be made by {provisioner} with {name}'s settings. Make it with: berth host create {new}"
                );
                return true;
            }
            let asked = berth_api::NewResource {
                provisioner: provisioner.clone(),
                kind: berth_core::plugins::MACHINE.into(),
                host: Some(new.clone()),
                arguments: planned.record.arguments.clone(),
                ..berth_api::NewResource::default()
            };
            let registries = Store::own(serve.store.clone());
            let made =
                berth_core::resource::prepare(paths, &registries, &asked).and_then(|asked| {
                    match asked {
                        berth_core::resource::Asked::New(creating) => {
                            out.hide(creating.secrets().iter().map(String::as_str));
                            creating.run(paths, &registries, &(serve.provision)(), out)
                        }
                        berth_core::resource::Asked::Again(_) => Ok(()),
                    }
                });
            let _inside = runtime.enter();
            // An address the provisioner gave is how the copy is reached now.
            serve.forget(&new);
            serve.refresh(false);
            match made {
                Ok(()) => true,
                Err(error) => {
                    let _ = writeln!(out, "✗ {error}");
                    let _ = writeln!(
                        out,
                        "{new} is saved. Asking again makes the same machine: berth host create {new}"
                    );
                    false
                }
            }
        }));
        let job = state.runner.start(request, work).map_err(refused)?;
        Ok(Acted {
            job: Some(job_info(&state.paths, job)),
            ..Acted::default()
        })
    })
}

// ── The installation's ──────────────────────────────────────────────────────

fn flag(name: &str, label: &str, help: &str) -> Argument {
    Argument {
        name: name.into(),
        label: label.into(),
        help: help.into(),
        default: "no".into(),
        values: vec!["no".into(), "yes".into()],
        ..Argument::default()
    }
}

fn rollout_action(_: &Seen) -> Action {
    rollout_declared()
}

fn rollout_declared() -> Action {
    Action {
        name: "rollout".into(),
        label: "Roll out".into(),
        help: "Bring the hosts up to date one after another: apply each that is behind, maybe run its update and reboot it, and check it. The first that fails stops the rest. Its job's report says how each host went.".into(),
        group: "Hosts".into(),
        role: Role::Admin,
        arguments: vec![
            Argument {
                name: "hosts".into(),
                label: "Hosts".into(),
                help: "Host names, separated by commas, in the order to take them. All saved hosts when empty.".into(),
                ..Argument::default()
            },
            Argument {
                name: "except".into(),
                label: "Leave out".into(),
                help: "Host names, separated by commas, that the roll-out leaves alone.".into(),
                ..Argument::default()
            },
            flag("all", "Also current hosts", "Apply the hosts that are current too."),
            flag("update", "Update", "Run each host's update action after applying."),
            flag("reboot", "Reboot", "Reboot a host that says it needs it, and wait for it to come back."),
            flag("force", "Force", "Reboot although sessions run, which ends them."),
        ],
        ..Action::default()
    }
}

fn rollout(state: Arc<Serve>, _: String, act: Act) -> Run {
    Box::pin(async move {
        state.refresh(false);
        let options = Rollout::from_arguments(&act.arguments)
            .map_err(|error| Fail::new(code::BAD_REQUEST, error))?;
        let known: Vec<String> = state.hosts.read().unwrap().keys().cloned().collect();
        if let Some(unknown) = options.except.iter().find(|name| !known.contains(name)) {
            return Err(Fail::new(
                code::NOT_FOUND,
                format!("No host {unknown} to leave out"),
            ));
        }
        let mut names = rollout::chosen(known, &options.hosts)
            .map_err(|error| Fail::new(code::NOT_FOUND, error.to_string()))?;
        names.retain(|name| !options.except.contains(name));
        let passwords: Vec<Option<String>> = names
            .iter()
            .map(|name| state.host(name).and_then(|remote| remote.password.clone()))
            .collect();
        let mut request = Request::new(Subject::Installation, "rollout");
        request.arguments = Argument::shown(&rollout_declared().arguments, &act.arguments);
        request.host = names.join(", ");
        request.token = act.token;
        request.secrets = passwords.iter().flatten().cloned().collect();
        let paths = state.paths.clone();
        let work = Work::Function(Box::new(move |out| {
            let mut targets: Vec<Box<dyn rollout::Target + '_>> = vec![];
            let mut problem = None;
            let mut skipped = vec![];
            for (name, password) in names.iter().zip(passwords) {
                let host = berth_core::profiles::Place::own(name)
                    .and_then(|place| Host::load_at(&paths, place));
                // Only a node or a workstation is applied; the others are said and left.
                let host = match host {
                    Ok(host) if !Capability::Apply.allowed(host_type::of(&host.config)) => {
                        skipped.push(host.name);
                        continue;
                    }
                    host => host,
                };
                match host.and_then(|host| rollout::Live::new(&paths, host, password)) {
                    Ok(target) => targets.push(Box::new(target)),
                    Err(error) => problem = Some(format!("✗ {name}: {error}")),
                }
            }
            if let Some(note) = rollout::skipped_note(&skipped) {
                let _ = writeln!(out, "{note}");
            }
            let report = match problem {
                Some(problem) => {
                    let _ = writeln!(out, "{problem}");
                    RolloutReport::default()
                }
                None => {
                    let compare = rollout::comparer(&paths);
                    rollout::run(&mut targets, &options, &compare, out)
                }
            };
            drop(targets);
            if let Some(log) = out.log() {
                let _ = std::fs::write(
                    report_path(log),
                    serde_json::to_vec(&report).unwrap_or_default(),
                );
            }
            report.ok
        }));
        // The roll-out waits on hosts with a runtime of its own, in the job's thread.
        let job = state.runner.start(request, work).map_err(refused)?;
        Ok(Acted {
            job: Some(job_info(&state.paths, job)),
            ..Acted::default()
        })
    })
}

// ── Keys and first contact ──────────────────────────────────────────────────

/// The yes or no of a flag argument.
fn yes(act: &Act, name: &str) -> Answer<bool> {
    match act.arguments.get(name).map(|value| value.trim()) {
        None | Some("" | "no" | "false") => Ok(false),
        Some("yes" | "true") => Ok(true),
        Some(other) => Err(Fail::new(
            code::BAD_REQUEST,
            format!("{name} is yes or no, not {other}"),
        )),
    }
}

/// A text argument, or `None` when it is empty.
fn given<'a>(act: &'a Act, name: &str) -> Option<&'a str> {
    act.arguments
        .get(name)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
}

/// For a caller on the server's machine, take the host's key from the
/// known-hosts file of the user the server runs as, when berth has none of
/// it yet: that user said yes to it already, so a first login asks nothing.
/// A caller elsewhere sends none of its files; the key the host shows at its
/// first login is recorded and printed, and `berth host verify` shows it.
fn take_known_host_key(state: &Serve, name: &str) {
    let Some(known_hosts) = state.known_hosts.lock().unwrap().clone() else {
        return;
    };
    let Ok(place) = own_place(name) else {
        return;
    };
    let Ok(host) = Host::load_at(&state.paths, place.clone()) else {
        return;
    };
    let taken = berth_core::keys::known::take_from_user(
        &state.paths,
        &state.store,
        &place,
        &host.config,
        &known_hosts,
    );
    if let Err(error) = taken {
        eprintln!(
            "berth: {name}'s host key was not taken from {}: {error}",
            known_hosts.display()
        );
    }
}

/// What logs in to a host first: it needs only a profile, not a controller.
fn first_contact(seen: &Seen, mut action: Action) -> Action {
    action.group = "Access".into();
    let why = seen
        .profile
        .is_null()
        .then(|| format!("{} has no profile on the server", seen.name));
    unavailable(action, why)
}

fn trust_key_action(seen: &Seen) -> Action {
    let action = Action {
        arguments: vec![
            Argument {
                name: "password".into(),
                label: "Password".into(),
                help: "The SSH user's password, to log in once. Not kept unless keep_password says so.".into(),
                secret: true,
                ..Argument::default()
            },
            Argument {
                name: "key".into(),
                label: "Key".into(),
                help: "A key of the server's ring to log in with once, in place of a password.".into(),
                ..Argument::default()
            },
            flag(
                "keep_password",
                "Keep the password",
                "Keep the password as the secret ssh/HOST and go on logging in with it.",
            ),
        ],
        ..set_up(
            "trust-key",
            "Install berth's key",
            "First contact: make the host take its key of the server's ring. The key is tried first; when the host refuses it, the password or key given logs in once to add it to the SSH user's authorized_keys. It runs as a job of the server.",
        )
    };
    first_contact(seen, action)
}

fn trust_key(state: Arc<Serve>, name: String, act: Act) -> Run {
    Box::pin(async move {
        let bad = |error: String| Fail::new(code::BAD_REQUEST, error);
        let host = Host::load_at(&state.paths, own_place(&name)?)
            .map_err(|error| bad(error.to_string()))?;
        let keep = yes(&act, "keep_password")?;
        let password = given(&act, "password").map(str::to_owned);
        let once = match (&password, given(&act, "key")) {
            (Some(_), Some(_)) => return Err(bad("Give a password or a key, not both".into())),
            (Some(password), None) => Some(Once::Password(password.clone())),
            (None, Some(key)) => {
                let store = berth_core::store::shared(&state.paths).map_err(internal)?;
                let halves = ring::read(&store, key, false).map_err(internal)?;
                let halves = halves.ok_or_else(|| {
                    Fail::new(
                        code::NOT_FOUND,
                        format!("No key {key}. See: berth key list"),
                    )
                })?;
                Some(Once::Key(halves))
            }
            (None, None) => None,
        };
        let mut request = Request::new(Subject::Host(name.clone()), "trust-key");
        request.token = act.token;
        // The password goes to ssh only, and never into what the job shows.
        request.secrets = password.into_iter().collect();
        let paths = state.paths.clone();
        let work = Work::Function(Box::new(move |out| {
            match contact::trust_key(&paths, &host, once, keep) {
                Ok(trusted) => {
                    let _ = match trusted.already {
                        true => writeln!(out, "✓ {name} takes the key {} already", trusted.key),
                        false => writeln!(
                            out,
                            "✓ {name} takes the key {} now: {}",
                            trusted.key, trusted.fingerprint
                        ),
                    };
                    for key in &trusted.host_keys {
                        let _ = writeln!(out, "Recorded its host key: {}", ring::fingerprint(key));
                    }
                    true
                }
                Err(error) => {
                    let _ = writeln!(out, "✗ {error}");
                    false
                }
            }
        }));
        let job = state.runner.start(request, work).map_err(refused)?;
        Ok(Acted {
            job: Some(job_info(&state.paths, job)),
            ..Acted::default()
        })
    })
}

fn host_key_action(seen: &Seen) -> Action {
    let action = Action {
        arguments: vec![flag(
            "accept",
            "Accept a changed key",
            "Forget the recorded host key and record the one the host has now.",
        )],
        ..set_up(
            "host-key",
            "Host key",
            "The host keys the server has recorded for the host, by their fingerprints. With accept, a changed key is taken in place of the recorded one.",
        )
    };
    first_contact(seen, action)
}

fn host_key(state: Arc<Serve>, name: String, act: Act) -> Run {
    Box::pin(async move {
        let accept = yes(&act, "accept")?;
        let host = Host::load_at(&state.paths, own_place(&name)?)
            .map_err(|error| Fail::new(code::BAD_REQUEST, error.to_string()))?;
        let paths = state.paths.clone();
        // It may log in to the host, and waits for that.
        let keys = tokio::task::spawn_blocking(move || {
            contact::host_key(&paths, &host, accept).map_err(|error| error.to_string())
        })
        .await
        .map_err(internal)?
        .map_err(|error| Fail::new(code::BAD_REQUEST, error))?;
        let keys: Vec<Value> = keys
            .iter()
            .map(|key| {
                let kind = key.split_whitespace().next().unwrap_or("");
                json!({"type": kind, "fingerprint": ring::fingerprint(key)})
            })
            .collect();
        Ok(Acted {
            result: Some(json!({"host_keys": keys, "accepted": accept})),
            ..Acted::default()
        })
    })
}

// ── Its name ────────────────────────────────────────────────────────────────

fn rename_action(seen: &Seen) -> Action {
    let action = host_type::rename_action();
    let why = seen
        .profile
        .is_null()
        .then(|| format!("{} has no profile on the server", seen.name));
    unavailable(action, why)
}

fn rename(state: Arc<Serve>, name: String, act: Act) -> Run {
    Box::pin(async move {
        let bad = |error: String| Fail::new(code::BAD_REQUEST, error);
        let new = given(&act, "name").unwrap_or_default().to_owned();
        let hostname = yes(&act, "hostname")?;
        let to = own_place(&new)?;
        if hostname && let Some(problem) = berth_tui::setup::hostname_problem(&new) {
            return Err(bad(problem));
        }
        let from = own_place(&name)?;
        let joins = profile::joins_tailnet(&profile_of(&state, &name));
        if hostname {
            berth_core::profiles::hostname_allowed(&name, &profile_of(&state, &name))
                .map_err(|error| bad(error.to_string()))?;
        }
        if to.load(&state.paths).map_err(internal)?.is_some() {
            return Err(Fail::new(
                code::CONFLICT,
                format!("There is a host {new} already"),
            ));
        }
        let mut request = Request::new(Subject::Host(name.clone()), "rename");
        request.token = act.token;
        // The rename takes the locks of both names itself.
        request.changes = false;
        let (serve, runtime) = (state.clone(), tokio::runtime::Handle::current());
        let work = Work::Function(Box::new(move |out| {
            let _inside = runtime.enter();
            // Its channel is not taken in under the old name meanwhile.
            serve.forget(&name);
            let done = berth_core::profiles::rename(&serve.paths, &from, &new, hostname);
            if done.is_ok() {
                serve.renamed(&name, &new);
            }
            // Its channel is followed again at once: under the new name, or
            // under the old one when nothing changed.
            serve.refresh(false);
            match done {
                Ok(_) => {
                    let _ = writeln!(out, "✓ {name} is called {new} now");
                    if hostname {
                        let also = match joins {
                            true => ", and its device on the tailnet is renamed then",
                            false => "",
                        };
                        let _ = writeln!(
                            out,
                            "Its profile's hostname is {new}; the machine takes it at its next apply{also}"
                        );
                    }
                    true
                }
                Err(error) => {
                    let _ = writeln!(out, "✗ {error}");
                    false
                }
            }
        }));
        let job = state.runner.start(request, work).map_err(refused)?;
        Ok(Acted {
            job: Some(job_info(&state.paths, job)),
            ..Acted::default()
        })
    })
}

fn rotate_key_action(_: &Seen) -> Action {
    Action {
        name: "rotate-key".into(),
        label: "Rotate a key".into(),
        help: "Replace a key of the server's ring on every host that takes it, as a job. A host that does not answer stays pending; running it again goes on from there.".into(),
        group: "Keys".into(),
        role: Role::Admin,
        arguments: vec![Argument {
            name: "name".into(),
            label: "Key".into(),
            help: "The key to replace.".into(),
            default: berth_api::profile::BERTH_KEY.into(),
            ..Argument::default()
        }],
        ..Action::default()
    }
}

fn rotate_key(state: Arc<Serve>, _: String, act: Act) -> Run {
    Box::pin(async move {
        let name = given(&act, "name")
            .unwrap_or(berth_api::profile::BERTH_KEY)
            .to_owned();
        super::keys::start_rotation(&state, name, act.token)
    })
}

// ── Adopting a host ─────────────────────────────────────────────────────────

fn adopt_action(_: &Seen) -> Action {
    let text = |name: &str, label: &str, help: &str| Argument {
        name: name.into(),
        label: label.into(),
        help: help.into(),
        ..Argument::default()
    };
    Action {
        name: "adopt".into(),
        label: "Adopt a host".into(),
        help: "Take in a host another berth set up, from the profile it keeps, read over SSH with sudo so that it works while berthd is down. Without an address it is the saved host of that name, reached as its profile says. It runs as a job.".into(),
        group: "Hosts".into(),
        role: Role::Admin,
        arguments: vec![
            Argument {
                required: true,
                ..text("name", "Name", "The host's name here.")
            },
            text("address", "Address", "[USER@]ADDRESS to read the profile at; empty for the saved host of that name."),
            text("port", "Port", "The SSH port, with an address."),
            text("key", "Key file", "A private key file where the server runs, offered as well for reading the profile only."),
            flag("force", "Replace", "Replace a host of that name, or a saved workstation's profile."),
        ],
        ..Action::default()
    }
}

/// What adopting a host is to log in to, and where its profile goes.
fn adopt_reach(
    paths: &berth_core::paths::Paths,
    name: &str,
    act: &Act,
) -> Result<(Place, berth_core::kept_profile::Reach), String> {
    use berth_core::kept_profile as kept;
    let force = act
        .arguments
        .get("force")
        .is_some_and(|value| value == "yes" || value == "true");
    let address = given(act, "address").unwrap_or("");
    let port = given(act, "port").unwrap_or("");
    let key = given(act, "key");
    let place = Place::own(name).map_err(|error| error.to_string())?;
    let saved = place.load(paths).map_err(|error| error.to_string())?;
    let reach = match (&saved, address) {
        (None, "") => {
            return Err(format!(
                "There is no host {name} to adopt by its own address. Say where it is: berth host adopt {name} [USER@]ADDRESS"
            ));
        }
        // A host saved below a workstation says less than the host keeps;
        // adopting it by its own address is what replacing it means.
        (Some(profile), "") => {
            if host_type::of(profile) == HostType::Workstation && !force {
                return Err(format!(
                    "{name} is saved as a workstation already. Replace its profile with the one it keeps with --force"
                ));
            }
            let access = profile::Access::of(profile);
            if access.link.is_some() || access.address.is_empty() {
                return Err(format!(
                    "{name} has no SSH address to read its profile at: berth host adopt {name} [USER@]ADDRESS"
                ));
            }
            let given = kept::Reach::parse(&access.address, &access.port, key)
                .map_err(|error| error.to_string())?;
            kept::Reach {
                key: given.key,
                ..kept::Reach::of(&access)
            }
        }
        (Some(_), _) if !force => {
            return Err(format!(
                "There is a host {name} already. Choose another name, or replace it with --force"
            ));
        }
        (_, address) => {
            kept::Reach::parse(address, port, key).map_err(|error| error.to_string())?
        }
    };
    Ok((place, reach))
}

fn adopt(state: Arc<Serve>, _: String, act: Act) -> Run {
    Box::pin(async move {
        use berth_core::kept_profile as kept;
        let name = given(&act, "name").unwrap_or_default().to_owned();
        let (place, reach) = adopt_reach(&state.paths, &name, &act)
            .map_err(|error| Fail::new(code::BAD_REQUEST, error))?;
        let mut request = Request::new(Subject::Host(name.clone()), "adopt");
        request.token = act.token.clone();
        request.secrets = act.sudo_password.iter().cloned().collect();
        let (serve, sudo) = (state.clone(), act.sudo_password.clone());
        let work = Work::Function(Box::new(move |out| {
            let paths = &serve.paths;
            let target = reach.target();
            let password = || -> berth_core::Result<String> {
                sudo.clone().ok_or_else(|| {
                    format!(
                        "sudo asks for a password on {target}: give it with --sudo-password-stdin"
                    )
                    .into()
                })
            };
            let adopted = kept::read(paths, &place, &reach, &password).and_then(|read| {
                let access = profile::Access {
                    address: reach.address.clone(),
                    user: reach.user.clone(),
                    port: reach.port.clone(),
                    auth: read.auth.clone(),
                    // The operator's sudo takes none; another account's may.
                    sudo: read.sudo_asked.then_some(true),
                    ..profile::Access::default()
                };
                let adopted = kept::adopted(paths, &place.title(), &read.kept, &access)?;
                kept::save(paths, &place, &adopted)?;
                Ok((read, adopted))
            });
            let (read, adopted) = match adopted {
                Ok(done) => done,
                Err(error) => {
                    let _ = writeln!(out, "✗ {error}");
                    return false;
                }
            };
            serve.forget(&name);
            serve.refresh(false);
            let written = match (adopted.berth.as_str(), adopted.written.as_str()) {
                ("", _) => String::new(),
                (berth, "") => format!(" by berth {berth}"),
                (berth, when) => format!(" by berth {berth} at {when}"),
            };
            let was = match adopted.host.as_str() {
                old if old.is_empty() || old == name => String::new(),
                old => format!(", where it was {old}"),
            };
            let _ = writeln!(
                out,
                "✓ Adopted {name} from {target}: its profile was written{written}{was}"
            );
            let store = Store::own(serve.store.clone());
            let known = store.secret_names();
            let mut left = vec![];
            for secret in adopted.secrets() {
                if !known.contains(&secret) {
                    left.push(format!(
                        "the value of secret {secret}: berth secret set {secret}"
                    ));
                }
            }
            let registries = store.registries().unwrap_or_default();
            for registry in adopted.registries() {
                if !registries.contains_key(&registry) {
                    left.push(format!(
                        "registry {registry}, with its login: berth registry add {registry} …"
                    ));
                }
            }
            if profile::joins_tailnet(&adopted.profile)
                && !known.iter().any(|known| known == profile::TAILNET_API_KEY)
            {
                left.push(format!(
                    "secret {}, which a host that has left the tailnet needs to join again",
                    profile::TAILNET_API_KEY
                ));
            }
            if !left.is_empty() {
                let _ = writeln!(
                    out,
                    "What did not come with it, and this berth has not got:"
                );
                for line in &left {
                    let _ = writeln!(out, "  {line}");
                }
            }
            let way = match read.auth {
                Auth::Berth => "with this berth's key".to_owned(),
                _ => "with the SSH configuration and agent of the server's account, until an apply has given the operator this berth's key".to_owned(),
            };
            let _ = writeln!(out, "berth logs in to it {way}.");
            let _ = writeln!(
                out,
                "Next: berth apply {name}. It gives this berth's own key a block of its own in the operator's authorized_keys."
            );
            let _ = writeln!(
                out,
                "The berth that set the host up still has access: its key keeps its BERTH block until you delete that block by hand."
            );
            true
        }));
        let job = state.runner.start(request, work).map_err(refused)?;
        Ok(Acted {
            job: Some(job_info(&state.paths, job)),
            ..Acted::default()
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listed(names: &[&str]) -> Vec<HostAction> {
        names
            .iter()
            .map(|name| HostAction {
                name: (*name).into(),
                ..HostAction::default()
            })
            .collect()
    }

    static UNPOWERED: std::sync::LazyLock<Powered> = std::sync::LazyLock::new(Powered::default);

    fn seen<'a>(
        host_type: HostType,
        listed: Option<&'a [HostAction]>,
        profile: &'a Value,
    ) -> Seen<'a> {
        Seen {
            name: "big",
            profile,
            host_actions: listed,
            secrets: &[],
            host_type,
            observed: None,
            unanswered: None,
            older: None,
            local: false,
            power: &UNPOWERED,
            machine: None,
        }
    }

    #[test]
    fn upgrading_the_server_is_offered_where_the_node_has_its_container() {
        let profile = json!({"type": "node", "access": {"address": "192.0.2.20", "user": "root"}});
        let find = |actions: &[Action]| {
            actions
                .iter()
                .find(|action| action.name == UPGRADE_SERVER)
                .cloned()
        };
        // A node whose role found no berth server's container lists none.
        let without = listed(&["status", "update"]);
        assert!(find(&host_list(&seen(HostType::Node, Some(&without), &profile))).is_none());
        let with = listed(&["status", UPGRADE_SERVER]);
        let actions = host_list(&seen(HostType::Node, Some(&with), &profile));
        let upgrade = find(&actions).unwrap();
        assert!(upgrade.available, "{}", upgrade.why);
        assert_eq!(upgrade.arguments[0].default, berth_api::VERSION);
        // Its source is the published image, or this berth sent with the request.
        let source = &upgrade.arguments[1];
        assert_eq!(source.name, upgrade_server::SOURCE);
        assert_eq!(source.default, upgrade_server::RELEASE);
        assert_eq!(
            source.values,
            [upgrade_server::RELEASE, upgrade_server::HERE]
        );
        assert_eq!(upgrade.role, Role::Admin);
        assert_eq!(upgrade.runs, Runs::Host);
        // Listed once: berth's own takes the place of the controller's.
        assert_eq!(
            actions
                .iter()
                .filter(|action| action.name == UPGRADE_SERVER)
                .count(),
            1
        );
        // A controller that does not answer cannot run it.
        let mut quiet = seen(HostType::Node, Some(&with), &profile);
        quiet.unanswered = Some("no answer in time");
        assert!(!upgrade_server_action(&quiet).available);
        // Only a node's controller does it.
        let managed = upgrade_server_action(&seen(HostType::Managed, Some(&with), &profile));
        assert!(!managed.available && managed.why.contains("only a node's controller"));
        assert!(managed.why.contains(berth_api::compat::UPGRADE_DOCS));
    }

    #[test]
    fn upgrading_from_here_takes_the_build_context_and_a_release_takes_none() {
        let act = |arguments: &[(&str, &str)], input: Option<&[u8]>| Act {
            arguments: arguments
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            input_b64: input.map(b64),
            ..Act::default()
        };
        // A release: the version alone, as a controller of an older role takes it.
        let mut release = act(&[("version", " 0.6.1 ")], None);
        assert_eq!(upgrade_request(&mut release).ok().unwrap(), None);
        assert_eq!(
            release.arguments,
            [("version".to_owned(), "0.6.1".to_owned())].into()
        );
        let mut default = act(&[("source", "release")], None);
        assert!(upgrade_request(&mut default).is_ok());
        assert_eq!(default.arguments["version"], berth_api::VERSION);
        assert!(!default.arguments.contains_key("source"));
        // Here: the context goes to the controller's input, with the version sent.
        let mut here = act(
            &[("version", "0.6.0"), ("source", "here")],
            Some(b"context"),
        );
        assert_eq!(
            upgrade_request(&mut here).ok().flatten().as_deref(),
            Some(&b"context"[..])
        );
        assert_eq!(here.arguments["source"], "here");
        assert_eq!(here.input_b64, None);
        let refused = |mut act: Act| upgrade_request(&mut act).unwrap_err().message().to_owned();
        assert!(refused(act(&[("source", "here")], None)).contains("needs the build context"));
        assert!(
            refused(act(&[("source", "here")], Some(b"context"))).contains("needs the version")
        );
        assert!(
            refused(act(&[("version", "0.6.0")], Some(b"context")))
                .contains("takes no build context")
        );
        assert!(refused(act(&[("source", "there")], None)).contains("release or here"));
        // No other action takes input.
        let body = Bytes::from(serde_json::to_vec(&act(&[], Some(b"x"))).unwrap());
        assert!(parse(&body).is_err());
        assert!(parse_with_input(&body).is_ok());
    }

    #[test]
    fn a_controller_that_is_too_old_is_said_to_be_old() {
        let profile =
            json!({"type": "managed", "access": {"address": "192.0.2.12", "user": "root"}});
        let said =
            berth_api::compat::older_controller("big", "0.5.2", 1, HostType::Managed).unwrap();
        let mut old = seen(HostType::Managed, None, &profile);
        old.older = Some(&said);
        let actions = host_list(&old);
        let secrets = actions
            .iter()
            .find(|action| action.name == "secrets")
            .unwrap();
        assert!(!secrets.available);
        assert_eq!(secrets.why, said);
        assert!(
            secrets.why.ends_with("berth run big install-controller"),
            "{}",
            secrets.why
        );
        let install = actions
            .iter()
            .find(|action| action.name == "install-controller")
            .unwrap();
        assert!(install.available, "{}", install.why);
    }

    #[test]
    fn a_node_is_applied_and_makes_no_seed() {
        let profile = json!({"type": "node", "access": {"address": "192.0.2.20", "user": "root"}});
        let actions = host_list(&seen(HostType::Node, Some(&[]), &profile));
        let find = |name: &str| actions.iter().find(|action| action.name == name).unwrap();
        assert!(find("apply").available, "{}", find("apply").why);
        assert!(!find("seed").available);
        assert!(find("seed").why.contains("not a workstation"));
        assert!(!find("install-controller").available);
    }

    fn made_machine(made: bool) -> Resource {
        Resource {
            id: "r-big".into(),
            provisioner: "incus".into(),
            kind: "machine".into(),
            external_id: if made {
                "berth-big".into()
            } else {
                String::new()
            },
            verbs: vec!["start".into(), "stop".into()],
            state: if made {
                "stopped".into()
            } else {
                "planned".into()
            },
            ..Resource::default()
        }
    }

    #[test]
    fn starting_and_stopping_follow_how_the_host_is_powered() {
        let profile = json!({"builder": {}});
        let machine = made_machine(true);
        let powered = host_type::power("big", &profile, Some(&machine));
        let mut by_provisioner = seen(HostType::Workstation, Some(&[]), &profile);
        by_provisioner.power = &powered;
        by_provisioner.machine = Some(&machine);
        let (start, stop) = (start_action(&by_provisioner), stop_action(&by_provisioner));
        assert!(
            start.available && stop.available,
            "{} {}",
            start.why,
            stop.why
        );
        assert!(start.confirm.is_none());
        assert_eq!(stop.confirm.as_deref(), Some("Stop big? Its sessions end."));
        assert!(stop.arguments.is_empty());
        assert_eq!((start.role, stop.role), (Role::Operate, Role::Operate));
        // A host that declares nothing says why.
        let unpowered = host_type::power("big", &profile, None);
        let mut bare = seen(HostType::Managed, Some(&[]), &profile);
        bare.power = &unpowered;
        let (start, stop) = (start_action(&bare), stop_action(&bare));
        assert!(!start.available && start.why.contains("declares no way to start it"));
        assert!(!stop.available && stop.why.contains("power_stop=controller"));
        // Its controller shuts it down only while it answers.
        let declared = json!({"type": "managed", "access": {"address": "192.0.2.4"}, "power": {"stop": {"by": "controller"}}});
        let powered = host_type::power("big", &declared, None);
        let mut by_controller = seen(HostType::Managed, Some(&[]), &declared);
        by_controller.power = &powered;
        let stop = stop_action(&by_controller);
        assert!(stop.available, "{}", stop.why);
        assert_eq!(stop.arguments[0].name, "force");
        by_controller.unanswered = Some("no answer in time");
        let stop = stop_action(&by_controller);
        assert!(!stop.available);
        assert!(
            stop.why
                .ends_with("it shuts the host down: no answer in time"),
            "{}",
            stop.why
        );
    }

    #[test]
    fn removing_names_what_is_destroyed_and_refuses_destroying_nothing() {
        let profile = json!({"builder": {}});
        let machine = made_machine(true);
        let mut made = seen(HostType::Workstation, None, &profile);
        made.machine = Some(&machine);
        let remove = remove_action(&made);
        assert!(remove.available);
        assert_eq!(remove.role, Role::Admin);
        assert_eq!(
            remove.confirm.as_deref(),
            Some(
                "Remove big? With destroy=yes incus destroys its machine berth-big and the disk it has; without, the machine is left as it is."
            )
        );
        assert_eq!(
            (
                remove.arguments[0].name.as_str(),
                remove.arguments[0].default.as_str()
            ),
            ("destroy", "no")
        );
        assert!(destroy_refused("big", Some(&machine)).is_none());
        let bare = remove_action(&seen(HostType::Workstation, None, &profile));
        assert_eq!(
            bare.confirm.as_deref(),
            Some("Forget big? The machine is left as it is.")
        );
        let refused = destroy_refused("big", None).unwrap();
        assert!(
            refused
                .starts_with("No provisioner made big, so there is nothing for berth to destroy")
        );
        let planned = made_machine(false);
        assert!(
            destroy_refused("big", Some(&planned))
                .unwrap()
                .contains("not made yet")
        );
        // A host the server has no profile of has nothing to remove here.
        let unsaved = remove_action(&seen(HostType::Workstation, None, &Value::Null));
        assert!(!unsaved.available);
    }

    #[test]
    fn a_duplicate_of_a_host_a_provisioner_made_may_be_created_at_once() {
        let profile = json!({"builder": {}});
        let machine = made_machine(true);
        let mut made = seen(HostType::Workstation, None, &profile);
        made.machine = Some(&machine);
        let duplicate = duplicate_action(&made);
        let names: Vec<&str> = duplicate
            .arguments
            .iter()
            .map(|argument| argument.name.as_str())
            .collect();
        assert_eq!(names, ["name", "create"]);
        assert!(duplicate.arguments[0].required);
        assert!(
            duplicate
                .help
                .contains("to be made by incus with the same settings")
        );
        assert!(duplicate.help.contains("The disk is not cloned"));
        let bare = duplicate_action(&seen(HostType::Workstation, None, &profile));
        assert_eq!(bare.arguments.len(), 1);
        assert!(bare.help.contains("The disk is not cloned"));
        // All four are listed with berth's own, whatever the controller says.
        let names: Vec<String> = host_list(&made)
            .into_iter()
            .map(|action| action.name)
            .collect();
        for name in ["start", "stop", "duplicate", "remove"] {
            assert!(names.iter().any(|known| known == name), "{name}: {names:?}");
        }
    }
}
