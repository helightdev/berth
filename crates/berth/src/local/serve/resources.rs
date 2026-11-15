//! The routes of resources: what provisioners made for berth, a host's
//! machine among them. berth's records answer the list; one resource is
//! asked of its provisioner. Making, changing and removing one are jobs of
//! the resource as a subject, holding its lock, and a token given again
//! answers with the job the first call started.

use super::{
    Serve,
    actions::{admit, answer, parse, refused},
    api::{Answer, App, Fail, Ws, caller, internal, job_info},
    auth::Access,
};
use axum::{
    Json,
    body::Bytes,
    extract::{Extension, Path, Query, State, ws::Message as WsMessage},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use berth_api::{
    Act, Acted, Action, Created, ErrorBody, NewResource, Resource, ResourceChange, code,
};
use berth_core::{
    host::Host,
    paths::Paths,
    provisioner::Answered,
    registry::Store,
    resource::{self, Asked, Given},
    runner::{Request, Reserved, Work},
};
use serde::Deserialize;
use serde_json::Value;
use std::{io::Write, sync::Arc, time::Duration};
use utoipa::IntoParams;

fn bad(error: impl std::fmt::Display) -> Fail {
    Fail::new(code::BAD_REQUEST, error.to_string())
}

/// A refusal of the core as an answer: a missing resource is 404, one that
/// is busy 409, anything else a request that does not fit.
fn refusal(error: impl std::fmt::Display) -> Fail {
    let message = error.to_string();
    if message.starts_with("No resource ") || message.starts_with("No provisioner ") {
        Fail::new(code::NOT_FOUND, message)
    } else if message.starts_with("Something else is being done to")
        || message.contains(" already, ")
    {
        Fail::new(code::CONFLICT, message)
    } else {
        bad(message)
    }
}

fn store(state: &Serve) -> Store {
    Store::own(state.store.clone())
}

/// Run work of the core off the runtime: it waits for the provisioner.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> berth_core::Result<T> + Send + 'static,
) -> Answer<Result<T, String>> {
    tokio::task::spawn_blocking(move || work().map_err(|error| error.to_string()))
        .await
        .map_err(internal)
}

/// A resource by its id, as its record has it.
fn found(state: &Serve, id: &str) -> Answer<resource::Kept> {
    resource::find(&state.paths, id).map_err(refusal)
}

fn described(state: &Serve, kept: &resource::Kept) -> Resource {
    let plugins = berth_core::provision::list_plugins(&state.paths);
    let busy = kept.record.external_id.is_empty() && kept.busy(&state.paths);
    resource::info(&state.paths, kept, &plugins, busy)
}

/// How a host is reached, to tell whether making its machine changed it.
fn reached(paths: &Paths, host: &str) -> Value {
    berth_core::profiles::Place::own(host)
        .and_then(|place| Host::load_at(paths, place))
        .map(|host| host.config["access"].clone())
        .unwrap_or_default()
}

#[derive(Deserialize, IntoParams, Default)]
#[into_params(parameter_in = Query)]
pub struct Filter {
    /// Only those of this provisioner, by its name.
    provisioner: Option<String>,
    /// Only those of this kind, such as `machine` or `deployment`.
    kind: Option<String>,
    /// Only the machine of this saved host.
    host: Option<String>,
}

#[utoipa::path(get, path = "/v1/resources", summary = "What provisioners made for berth, as berth's records have it. Nothing is asked of the provisioners.", tag = "resources",
    params(Filter),
    responses((status = 200, description = "The resources, each as it was last seen", body = Vec<Resource>)))]
pub async fn list(State(state): App, Query(filter): Query<Filter>) -> Answer<Json<Vec<Resource>>> {
    let listed = resource::listed(
        &state.paths,
        filter.provisioner.as_deref(),
        filter.kind.as_deref(),
        filter.host.as_deref(),
    )
    .map_err(internal)?;
    Ok(Json(listed))
}

#[utoipa::path(get, path = "/v1/resources/{id}", summary = "One resource, as its provisioner says it is now. One it no longer finds is `missing`, and its record stays.", tag = "resources",
    params(("id" = String, Path, description = "berth's id of the resource")),
    responses((status = 200, description = "The resource; a state of `unknown` comes with the problem", body = Resource),
              (status = 404, description = "No such resource", body = ErrorBody)))]
pub async fn get(State(state): App, Path(id): Path<String>) -> Answer<Json<Resource>> {
    found(&state, &id)?;
    let (paths, registries, options) = (state.paths.clone(), store(&state), (state.provision)());
    let seen = blocking(move || resource::status(&paths, &registries, &id, &options))
        .await?
        .map_err(refusal)?;
    Ok(Json(seen))
}

#[utoipa::path(post, path = "/v1/resources", summary = "Create a resource with a provisioner, as a job: a host's machine, which is then set up as the host, or a kind its plugin declares, such as a deployment from a specification.", tag = "resources",
    request_body(content = NewResource, description = "The provisioner, the kind, its name or host, arguments, the specification and the secrets it is given, and a token that makes asking again safe"),
    responses((status = 202, description = "Its record, saved before anything is made, and the job that makes it", body = Created),
              (status = 400, description = "An argument, a secret or the specification is missing or does not fit; the message says which", body = ErrorBody),
              (status = 404, description = "No such host or provisioner", body = ErrorBody),
              (status = 409, description = "One of that name exists already, or something else is being done to it", body = ErrorBody)))]
pub async fn create(State(state): App, body: Bytes) -> Answer<(StatusCode, Json<Created>)> {
    let asked: NewResource = serde_json::from_slice(&body).map_err(bad)?;
    if let Some(host) = asked.host.as_deref().filter(|host| !host.is_empty()) {
        super::setup::saved_profile(&state, host)?;
    }
    let (paths, registries) = (state.paths.clone(), store(&state));
    let token = asked.token.clone();
    let host = asked.host.clone().unwrap_or_default();
    let prepared = blocking({
        let (paths, registries) = (paths.clone(), registries.clone());
        move || resource::prepare(&paths, &registries, &asked)
    })
    .await?
    .map_err(refusal)?;
    let mut creating = match prepared {
        Asked::Again(kept) => {
            let job = state
                .store
                .job_by_token(
                    &kept.subject().key(),
                    "create",
                    token.as_deref().unwrap_or(""),
                )?
                .ok_or_else(|| {
                    Fail::new(
                        code::CONFLICT,
                        format!(
                            "{} was asked for with this token, and its job is gone",
                            kept.record.id
                        ),
                    )
                })?;
            return Ok((
                StatusCode::ACCEPTED,
                Json(Created {
                    resource: described(&state, &kept),
                    job: job_info(&state.paths, job),
                }),
            ));
        }
        Asked::New(creating) => creating,
    };
    let kept = creating.kept().clone();
    let mut request = Request::new(kept.subject(), "create");
    request.host = kept.record.host.clone();
    request.token = token;
    request.held = creating.take_lock();
    request.secrets = creating.secrets();
    let before = reached(&paths, &host);
    let (watcher, runtime) = (state.clone(), tokio::runtime::Handle::current());
    let work = Work::Function(Box::new(move |output| {
        let paths = &watcher.paths;
        let done = creating.run(paths, &registries, &(watcher.provision)(), output);
        if let Err(error) = &done {
            let _ = writeln!(output, "✗ {error}");
        }
        // An address the provisioner gave is how the host is reached now.
        if !host.is_empty() && reached(paths, &host) != before {
            let _inside = runtime.enter();
            watcher.forget(&host);
            watcher.refresh(false);
        }
        done.is_ok()
    }));
    let job = state.runner.start(request, work).map_err(refused)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(Created {
            resource: described(&state, &kept),
            job: job_info(&state.paths, job),
        }),
    ))
}

#[utoipa::path(get, path = "/v1/resources/{id}/actions", summary = "What can be done to a resource: start, stop and change as its kind can, its plugin's actions of the kind, and removing it.", tag = "resources",
    params(("id" = String, Path, description = "berth's id of the resource")),
    responses((status = 200, description = "The actions, each saying whether it can run now", body = Vec<Action>),
              (status = 404, description = "No such resource", body = ErrorBody)))]
pub async fn actions(State(state): App, Path(id): Path<String>) -> Answer<Json<Vec<Action>>> {
    let kept = found(&state, &id)?;
    Ok(Json(resource::actions(&state.paths, &store(&state), &kept)))
}

/// Run an action of a resource that has been admitted: one that only reads
/// answers at once, the others are jobs of the resource.
async fn run_action(
    state: Arc<Serve>,
    id: String,
    action: String,
    given: Given,
    token: Option<String>,
) -> Answer<Acted> {
    let kept = found(&state, &id)?;
    // The same token again is the job it started, before anything is checked again.
    if let Some(token) = token.as_deref()
        && let Some(job) = state
            .store
            .job_by_token(&kept.subject().key(), &action, token)?
    {
        return Ok(Acted {
            job: Some(job_info(&state.paths, job)),
            ..Acted::default()
        });
    }
    let (paths, registries, options) = (state.paths.clone(), store(&state), (state.provision)());
    let mut acting = blocking({
        let (paths, id, action) = (paths.clone(), id.clone(), action.clone());
        move || resource::prepare_action(&paths, &registries, &id, &action, &given)
    })
    .await?
    .map_err(refusal)?;
    if !acting.changes() {
        let answered = blocking(move || acting.run(&paths, &options, &mut std::io::sink()))
            .await?
            .map_err(|error| Fail::new(code::UNREACHABLE, error))?;
        return Ok(Acted {
            result: Some(answered.value()),
            ..Acted::default()
        });
    }
    let mut request = Request::new(kept.subject(), &action);
    request.host = kept.record.host.clone();
    request.token = token;
    request.held = acting.take_lock();
    request.secrets = acting.secrets();
    let ticket = match state.runner.reserve(request).map_err(refused)? {
        Reserved::Found(job) => {
            return Ok(Acted {
                job: Some(job_info(&state.paths, job)),
                ..Acted::default()
            });
        }
        Reserved::New(ticket) => ticket,
    };
    let work = Work::Function(Box::new(move |out| {
        match acting.run(&paths, &options, out) {
            Ok(answered) => {
                say(out, &answered);
                true
            }
            Err(error) => {
                let _ = writeln!(out, "✗ {error}");
                false
            }
        }
    }));
    Ok(Acted {
        job: Some(job_info(&state.paths, ticket.run(work))),
        ..Acted::default()
    })
}

fn say(out: &mut dyn Write, answered: &Answered) {
    for line in answered.lines() {
        let _ = writeln!(out, "{line}");
    }
}

/// Check the caller against the action's own role and its availability.
fn admitted(state: &Serve, access: &Access, id: &str, action: &str, act: &Act) -> Answer<()> {
    let kept = found(state, id)?;
    let listed = resource::actions(&state.paths, &store(state), &kept);
    let Some(known) = listed.iter().find(|known| known.name == action) else {
        let names: Vec<&str> = listed.iter().map(|known| known.name.as_str()).collect();
        return Err(Fail::new(
            code::NOT_FOUND,
            format!(
                "{id} has no action {action}. Its actions: {}",
                names.join(", ")
            ),
        ));
    };
    admit(state, access, known, act)
}

#[utoipa::path(post, path = "/v1/resources/{id}/actions/{action}", summary = "Run one of a resource's actions. One that changes something is a job of the resource, which holds its lock; one that only reads answers at once.", tag = "resources",
    params(("id" = String, Path, description = "berth's id of the resource"),
           ("action" = String, Path, description = "The action, as resources.actions names it")),
    request_body(content = Option<Act>, description = "Its arguments, and a token that makes asking again safe"),
    responses((status = 202, description = "Started as a job", body = Acted),
              (status = 200, description = "Answered at once: its `message` and `details` as the result", body = Acted),
              (status = 400, description = "An argument is missing or unknown, or the body is not one", body = ErrorBody),
              (status = 403, description = "The caller's role is below the action's", body = ErrorBody),
              (status = 404, description = "No such resource or action", body = ErrorBody),
              (status = 409, description = "Something else is being done to it", body = ErrorBody),
              (status = 501, description = "It cannot run now; the message says why", body = ErrorBody),
              (status = 502, description = "The provisioner failed", body = ErrorBody)))]
pub async fn act(
    State(state): App,
    access: Option<Extension<Access>>,
    Path((id, action)): Path<(String, String)>,
    body: Bytes,
) -> Answer<(StatusCode, Json<Acted>)> {
    let act = parse(&body)?;
    admitted(&state, &caller(access), &id, &action, &act)?;
    let given = Given {
        arguments: act.arguments,
        ..Given::default()
    };
    Ok(answer(
        run_action(state, id, action, given, act.token).await?,
    ))
}

#[utoipa::path(put, path = "/v1/resources/{id}", summary = "Change a resource, as a job: other values of its kind's arguments, a new specification, other secrets. What is not given stays.", tag = "resources",
    params(("id" = String, Path, description = "berth's id of the resource")),
    request_body(content = ResourceChange, description = "What changes, and a token that makes asking again safe"),
    responses((status = 202, description = "Started as a job", body = Acted),
              (status = 400, description = "An argument, a secret or the specification does not fit", body = ErrorBody),
              (status = 404, description = "No such resource", body = ErrorBody),
              (status = 409, description = "Something else is being done to it", body = ErrorBody),
              (status = 501, description = "Its kind cannot be changed, or it is not made yet", body = ErrorBody)))]
pub async fn change(
    State(state): App,
    access: Option<Extension<Access>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Answer<(StatusCode, Json<Acted>)> {
    let asked: ResourceChange = serde_json::from_slice(&body).map_err(bad)?;
    let act = Act::default();
    admitted(&state, &caller(access), &id, resource::CHANGE, &act)?;
    let given = Given {
        arguments: asked.arguments,
        spec: asked.spec,
        secrets: asked.secrets,
    };
    Ok(answer(
        run_action(state, id, resource::CHANGE.into(), given, asked.token).await?,
    ))
}

#[derive(Deserialize, IntoParams, Default)]
#[into_params(parameter_in = Query)]
pub struct Token {
    /// Any text of the caller's; the same again answers with the first job.
    token: Option<String>,
}

#[utoipa::path(delete, path = "/v1/resources/{id}", summary = "Remove a resource with its provisioner, as a job, and forget it. A host's profile stays.", tag = "resources",
    params(("id" = String, Path, description = "berth's id of the resource"), Token),
    responses((status = 202, description = "Started as a job", body = Acted),
              (status = 404, description = "No such resource", body = ErrorBody),
              (status = 409, description = "Something else is being done to it", body = ErrorBody),
              (status = 501, description = "Its provisioner cannot be asked now; the message says why", body = ErrorBody)))]
pub async fn remove(
    State(state): App,
    access: Option<Extension<Access>>,
    Path(id): Path<String>,
    Query(query): Query<Token>,
) -> Answer<(StatusCode, Json<Acted>)> {
    admitted(
        &state,
        &caller(access),
        &id,
        resource::REMOVE,
        &Act::default(),
    )?;
    Ok(answer(
        run_action(
            state,
            id,
            resource::REMOVE.into(),
            Given::default(),
            query.token,
        )
        .await?,
    ))
}

#[derive(Deserialize, IntoParams, Default)]
#[into_params(parameter_in = Query)]
pub struct LogQuery {
    /// The last so many lines; all when 0 (default 100).
    tail: Option<u32>,
}

/// How long a followed log goes on before it is ended.
const FOLLOW_FOR: Duration = Duration::from_secs(60 * 60);
/// How long the provisioner has to give the lines of a plain read.
const READ_FOR: Duration = Duration::from_secs(60);

#[utoipa::path(get, path = "/v1/resources/{id}/logs", summary = "What a resource wrote, as its provisioner gives it, with secrets masked: the last lines as text, or as a WebSocket those and what follows.", tag = "resources",
    params(("id" = String, Path, description = "berth's id of the resource"), LogQuery),
    responses((status = 200, description = "The last lines", content_type = "text/plain", body = String),
              (status = 101, description = "As a WebSocket: the lines as they come"),
              (status = 404, description = "No such resource", body = ErrorBody),
              (status = 400, description = "Its kind gives no logs, or it is not made yet", body = ErrorBody)))]
pub async fn logs(
    State(state): App,
    Path(id): Path<String>,
    Query(query): Query<LogQuery>,
    ws: Ws,
) -> Answer<Response> {
    found(&state, &id)?;
    let tail = query.tail.unwrap_or(100);
    match ws {
        Ok(ws) => Ok(ws.on_upgrade(move |socket| follow(state, id, tail, socket))),
        Err(_) => {
            let (paths, registries) = (state.paths.clone(), store(&state));
            let text = blocking(move || {
                let mut out = vec![];
                resource::logs(&paths, &registries, &id, tail, false, READ_FOR, &mut out)?;
                Ok(out)
            })
            .await?
            .map_err(refusal)?;
            Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response())
        }
    }
}

/// Lines written to a channel, for a socket that sends them on.
struct Lines(tokio::sync::mpsc::UnboundedSender<Vec<u8>>);

impl Write for Lines {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0
            .send(data.to_vec())
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::BrokenPipe))?;
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A resource's log on a socket as it comes, then how it ended.
async fn follow(
    state: Arc<Serve>,
    id: String,
    tail: u32,
    mut socket: axum::extract::ws::WebSocket,
) {
    let (sender, mut lines) = tokio::sync::mpsc::unbounded_channel();
    let (paths, registries) = (state.paths.clone(), store(&state));
    let reading = tokio::task::spawn_blocking(move || {
        resource::logs(
            &paths,
            &registries,
            &id,
            tail,
            true,
            FOLLOW_FOR,
            &mut Lines(sender),
        )
        .map_err(|error| error.to_string())
    });
    loop {
        tokio::select! {
            line = lines.recv() => match line {
                Some(data) => if socket.send(WsMessage::Binary(data.into())).await.is_err() { return },
                None => break,
            },
            frame = socket.recv() => if !matches!(frame, Some(Ok(_))) { return },
        }
    }
    let ended = match reading.await {
        Ok(Ok(())) => serde_json::json!({"exited": {"exit_code": 0}}),
        Ok(Err(error)) => serde_json::json!({"exited": {"exit_code": 1}, "error": error}),
        Err(error) => serde_json::json!({"exited": {"exit_code": 1}, "error": error.to_string()}),
    };
    let _ = socket.send(WsMessage::Text(ended.to_string().into())).await;
    let _ = socket.send(WsMessage::Close(None)).await;
}
