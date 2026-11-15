//! The actions of a host and of the installation: what can be done to each,
//! and running one. A host's actions are the actions its controller lists and
//! berth's own from [`HOST`]; the installation's are [`INSTALLATION`]. An
//! action that changes something runs as a job; one the host runs is a
//! session there, which a job follows unless it needs a terminal.

use super::{
    Remote, Serve,
    api::{Answer, App, Fail, caller, internal, job_info, remote, report_path},
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
    Act, Acted, Action, Argument, ErrorBody, HostAction, Rollout, RolloutReport, Runs, Session,
    b64, client, code, ops::Role,
};
use berth_core::{
    apply,
    host::Host,
    host_type::{self, Capability, HostType},
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
    /// The caller is on the machine the server runs on: in its process, or
    /// on its socket.
    pub local: bool,
}

/// berth's own actions that run through the host's controller, so the host
/// is asked before one of them is described.
const THROUGH_CONTROLLER: &[&str] = &["secrets"];

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
        name: "installer",
        describe: installer_action,
        run: installer,
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

fn unavailable(mut action: Action, why: Option<String>) -> Action {
    if let Some(why) = why {
        action.available = false;
        action.why = why;
    }
    action
}

/// What a request to run an action carries; nothing is an empty one.
pub(super) fn parse(body: &Bytes) -> Answer<Act> {
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

/// The actions a host's controller lists, asked for with a time limit.
async fn controller_actions(remote: &Remote) -> Result<Vec<HostAction>, String> {
    let link = remote
        .link
        .as_ref()
        .ok_or_else(|| remote.problem.clone().unwrap_or_else(|| "no link".into()))?;
    match tokio::time::timeout(Duration::from_secs(15), link.get::<Value>("/v1/actions")).await {
        Ok(Ok(listed)) => Ok(berth_tui::host::list(listed, "actions")),
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Err("no answer in time".into()),
    }
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
    let own: Vec<Action> = HOST
        .iter()
        .map(|builtin| (builtin.describe)(seen))
        .collect();
    let listed = seen.host_actions.unwrap_or(&[]);
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
        _ => Some(controller_actions(&remote).await),
    };
    let (listed, unanswered) = match &asked {
        Some(Ok(listed)) => (Some(listed.as_slice()), None),
        Some(Err(error)) => (None, Some(error.as_str())),
        None => (None, None),
    };
    let secrets = secret_names(&state);
    let observed = observed(&remote).await;
    Ok(Json(host_list(&Seen {
        name: &name,
        profile: &profile,
        host_actions: listed,
        secrets: &secrets,
        host_type: host_type::of(&profile),
        observed,
        unanswered,
        local: access.is_none(),
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
    let act = parse(&body)?;
    let remote = remote(&state, &name)?;
    let profile = profile_of(&state, &name);
    // berth's own are described without asking the host, which they do themselves.
    let builtin = HOST.iter().find(|builtin| builtin.name == action);
    let ssh_only = host_type::of(&profile) == HostType::Ssh;
    let mut unanswered = None;
    let listed = match builtin {
        Some(_) if ssh_only || !THROUGH_CONTROLLER.contains(&action.as_str()) => None,
        // One that runs through the controller is described as the host answers.
        Some(_) => match controller_actions(&remote).await {
            Ok(listed) => Some(listed),
            Err(error) => {
                unanswered = Some(error);
                None
            }
        },
        None if ssh_only => Some(vec![]),
        None => Some(
            controller_actions(&remote)
                .await
                .map_err(|error| Fail::new(code::UNREACHABLE, error))?,
        ),
    };
    let secrets = secret_names(&state);
    let observed = observed(&remote).await;
    let actions = host_list(&Seen {
        name: &name,
        profile: &profile,
        host_actions: listed.as_deref(),
        secrets: &secrets,
        host_type: host_type::of(&profile),
        observed,
        unanswered: unanswered.as_deref(),
        local: access.is_none(),
    });
    let found = actions
        .iter()
        .find(|known| known.name == action)
        .ok_or_else(|| no_action(&name, &action, &actions))?;
    admit(&state, &caller(access), found, &act)?;
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
        local: false,
    };
    INSTALLATION
        .iter()
        .map(|builtin| (builtin.describe)(&seen))
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
}

/// Run an action of the host as a session there, with `input` on its standard
/// input. A job follows one that needs no terminal, copying its output.
async fn run_action(
    state: &Arc<Serve>,
    remote: &Arc<Remote>,
    action: &Action,
    act: Act,
    input: Option<String>,
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
        body["input_b64"] = json!(b64(format!("{input}\n").as_bytes()));
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
    request.token = act.token;
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
        let bad =
            |error: Box<dyn std::error::Error>| Fail::new(code::BAD_REQUEST, error.to_string());
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
        let work = Work::Function(Box::new(move |out| {
            apply::install_controller(&paths, &host, password.as_deref(), out)
        }));
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
        let bad =
            |error: Box<dyn std::error::Error>| Fail::new(code::BAD_REQUEST, error.to_string());
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
        let prepared = apply::prepare(&state.paths, &host, password.as_deref()).map_err(bad)?;
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
            if berth_core::runner::run_program(program, out) != Some(0) {
                return false;
            }
            if let Err(error) = apply::settle(&paths, &host) {
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
    host_type::fit(action, seen.name, seen.host_type, Capability::Apply)
}

fn seed(state: Arc<Serve>, name: String, _: Act) -> Run {
    Box::pin(async move {
        let (paths, registries) = (state.paths.clone(), Store::own(state.store.clone()));
        // It may publish the release and runs the seed builder, and waits for both.
        let seed = tokio::task::spawn_blocking(move || {
            berth_core::seed::cloud(&paths, &registries, &name, false)
                .map_err(|error| error.to_string())
        })
        .await
        .map_err(internal)?
        .map_err(|error| Fail::new(code::BAD_REQUEST, error))?;
        Ok(Acted {
            result: Some(json!({"user_data": seed.user_data})),
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
        let bad =
            |error: Box<dyn std::error::Error>| Fail::new(code::BAD_REQUEST, error.to_string());
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
                code::BAD_REQUEST,
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
            let (iso, _profile) = (built.iso.clone(), built.profile);
            let program = berth_core::runner::Program {
                command: built.command,
                input: None,
                files: vec![],
            };
            if berth_core::runner::run_program(program, out) != Some(0) {
                return false;
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
        let bad =
            |error: Box<dyn std::error::Error>| Fail::new(code::BAD_REQUEST, error.to_string());
        let host = Host::load_at(&state.paths, own_place(&name)?).map_err(bad)?;
        let payload = Store::own(state.store.clone())
            .host_payload(&host.config)
            .map_err(bad)?;
        let action = Action {
            name: "secrets".into(),
            ..Action::default()
        };
        run_action(&state, &remote, &action, act, Some(payload.to_string())).await
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
                // Only a workstation is applied; the others are said and left.
                let host = match host {
                    Ok(host) if host_type::of(&host.config) != HostType::Workstation => {
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
                        let _ = writeln!(
                            out,
                            "Its profile's hostname is {new}; the machine takes it at its next apply"
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
            if profile::joins_automatically(&adopted.profile)
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
                "Next: berth host apply {name}. It gives this berth's own key a block of its own in the operator's authorized_keys."
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
