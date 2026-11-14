//! The actions of a host and of the installation: what can be done to each,
//! and running one. A host's actions are the tasks its controller lists and
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
    Act, Acted, Action, Argument, ErrorBody, Rollout, RolloutReport, Runs, Session, Task, b64,
    client, code, ops::Role,
};
use berth_core::{
    apply,
    host::Host,
    keys::{self, Once, contact, ring},
    level::{self, Capability, Level},
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
    pub tasks: Option<&'a [Task]>,
    /// The names of the secrets the server keeps.
    pub secrets: &'a [String],
    /// The level the host's profile means it to have.
    pub level: Level,
    /// What its controller says it is, when it answered.
    pub observed: Option<Level>,
    /// Why its controller did not answer, when it was asked and did not.
    pub unanswered: Option<&'a str>,
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

/// The tasks a host's controller lists, asked for with a time limit.
async fn host_tasks(remote: &Remote) -> Result<Vec<Task>, String> {
    let link = remote
        .link
        .as_ref()
        .ok_or_else(|| remote.problem.clone().unwrap_or_else(|| "no link".into()))?;
    match tokio::time::timeout(Duration::from_secs(15), link.get::<Value>("/v1/tasks")).await {
        Ok(Ok(tasks)) => Ok(berth_tui::host::list(tasks, "tasks")),
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Err("no answer in time".into()),
    }
}

/// What a host's controller says the host is, when it answers.
async fn observed(remote: &Remote) -> Option<Level> {
    remote
        .version()
        .await
        .ok()
        .as_ref()
        .and_then(|version| level::seen(Ok(version)))
}

/// A host's actions: its tasks, then berth's own. A task that takes the
/// workstation's secrets on its input is berth's to run, with the server's.
fn host_list(seen: &Seen) -> Vec<Action> {
    let own: Vec<Action> = HOST
        .iter()
        .map(|builtin| (builtin.describe)(seen))
        .collect();
    let tasks = seen.tasks.unwrap_or(&[]);
    let mut actions: Vec<Action> = Action::of_tasks(tasks)
        .into_iter()
        .filter(|action| !own.iter().any(|known| known.name == action.name))
        .filter(|action| {
            let task = tasks.iter().find(|task| task.name == action.name);
            task.is_none_or(|task| task.input.is_none())
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

#[utoipa::path(get, path = "/v1/hosts/{name}/actions", summary = "What can be done to a host: the tasks its controller lists, and berth's own actions.", tag = "hosts",
    params(("name" = String, Path, description = "A saved host")),
    responses((status = 200, description = "The actions, each saying whether it can run now", body = Vec<Action>),
              (status = 404, description = "No such host", body = ErrorBody)))]
pub async fn host_actions(
    State(state): App,
    Path(name): Path<String>,
) -> Answer<Json<Vec<Action>>> {
    state.refresh(false);
    let remote = remote(&state, &name)?;
    let profile = profile_of(&state, &name);
    // A host that is only reachable has no controller to ask.
    let asked = match level::of(&profile) {
        Level::Reachable => None,
        _ => Some(host_tasks(&remote).await),
    };
    let (tasks, unanswered) = match &asked {
        Some(Ok(tasks)) => (Some(tasks.as_slice()), None),
        Some(Err(error)) => (None, Some(error.as_str())),
        None => (None, None),
    };
    let secrets = secret_names(&state);
    let observed = observed(&remote).await;
    Ok(Json(host_list(&Seen {
        name: &name,
        profile: &profile,
        tasks,
        secrets: &secrets,
        level: level::of(&profile),
        observed,
        unanswered,
    })))
}

#[utoipa::path(post, path = "/v1/hosts/{name}/actions/{action}", summary = "Run one of a host's actions. One of berth's that changes something is a job; a task of the host is a session there, which a job follows unless it needs a terminal.", tag = "hosts",
    params(("name" = String, Path, description = "A saved host"),
           ("action" = String, Path, description = "The action, as hosts.actions names it")),
    request_body(content = Option<Act>, description = "Its arguments, and a token that makes asking again safe"),
    responses((status = 202, description = "Started as a job; a task of the host also says its session", body = Acted),
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
    let reachable_only = level::of(&profile) == Level::Reachable;
    let mut unanswered = None;
    let tasks = match builtin {
        Some(_) if reachable_only || !THROUGH_CONTROLLER.contains(&action.as_str()) => None,
        // One that runs through the controller is described as the host answers.
        Some(_) => match host_tasks(&remote).await {
            Ok(tasks) => Some(tasks),
            Err(error) => {
                unanswered = Some(error);
                None
            }
        },
        None if reachable_only => Some(vec![]),
        None => Some(
            host_tasks(&remote)
                .await
                .map_err(|error| Fail::new(code::UNREACHABLE, error))?,
        ),
    };
    let secrets = secret_names(&state);
    let observed = observed(&remote).await;
    let actions = host_list(&Seen {
        name: &name,
        profile: &profile,
        tasks: tasks.as_deref(),
        secrets: &secrets,
        level: level::of(&profile),
        observed,
        unanswered: unanswered.as_deref(),
    });
    let found = actions
        .iter()
        .find(|known| known.name == action)
        .ok_or_else(|| no_action(&name, &action, &actions))?;
    admit(&state, &caller(access), found, &act)?;
    let acted = match builtin {
        Some(builtin) => (builtin.run)(state, name, act).await?,
        None => run_task(&state, &remote, found, act, None).await?,
    };
    Ok(answer(acted))
}

#[utoipa::path(get, path = "/v1/actions", summary = "What can be done to the installation as a whole, such as a roll-out over its hosts.", tag = "hosts",
    responses((status = 200, description = "The actions", body = Vec<Action>)))]
pub async fn list() -> Json<Vec<Action>> {
    Json(installation_list())
}

fn installation_list() -> Vec<Action> {
    let seen = Seen {
        name: "",
        profile: &Value::Null,
        tasks: None,
        secrets: &[],
        level: Level::Workstation,
        observed: None,
        unanswered: None,
    };
    INSTALLATION
        .iter()
        .map(|builtin| (builtin.describe)(&seen))
        .collect()
}

#[utoipa::path(post, path = "/v1/actions/{action}", summary = "Run one of the installation's actions, as a job.", tag = "hosts",
    params(("action" = String, Path, description = "The action, as actions.list names it")),
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

// ── Tasks of a host ─────────────────────────────────────────────────────────

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

/// Run a task of the host as a session there, with `input` on its standard
/// input. A job follows one that needs no terminal, copying its output.
async fn run_task(
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
    let path = format!("/v1/tasks/{}", client::encode(&action.name));
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
    // A task is the host's own session, and runs beside the others there.
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
    let action = level::fit(action, seen.name, seen.level, Capability::Apply);
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
    let action = level::install_controller_action(seen.name, seen.level, seen.observed);
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
    level::fit(action, seen.name, seen.level, Capability::Apply)
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

fn secrets_action(seen: &Seen) -> Action {
    let task = seen
        .tasks
        .and_then(|tasks| tasks.iter().find(|task| task.name == "secrets"));
    let mut action = set_up(
        "secrets",
        "Send secrets",
        "Send the host the secrets and registry logins its profile names, from the server's own.",
    );
    action.runs = Runs::Host;
    if let Some(task) = task {
        action.label = task.label.clone();
        action.help = task.help.clone();
        action.group = task.group.clone();
    }
    if let Err(refusal) = level::check(seen.name, seen.level, Capability::Secrets) {
        return unavailable(action, Some(refusal.why));
    }
    let why = match (seen.tasks, task, seen.unanswered) {
        (_, _, Some(error)) => Some(format!(
            "The controller on {} does not answer, and the secrets go through it: {error}",
            seen.name
        )),
        (Some(_), None, _) => Some(format!(
            "The controller on {} has no secrets task yet: apply its configuration",
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
        run_task(&state, &remote, &action, act, Some(payload.to_string())).await
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
            flag("all", "Also current hosts", "Apply the hosts that are current too."),
            flag("update", "Update", "Run each host's update task after applying."),
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
        let names = rollout::chosen(known, &options.hosts)
            .map_err(|error| Fail::new(code::NOT_FOUND, error.to_string()))?;
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
                    Ok(host) if level::of(&host.config) != Level::Workstation => {
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
                    Fail::new(code::NOT_FOUND, format!("No key {key}. See: berth key"))
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
