//! The routes of the provisioners of plugins: each as a subject with its
//! page and its actions. What they make is a resource (see
//! [`super::resources`]). The provisioners run here, with the server's
//! secrets. Looking needs no more than `view`; an action needs its own role.

use super::{
    Serve,
    actions::{admit, answer, parse, refused},
    api::{Answer, App, Fail, caller, internal, job_info},
    auth::Access,
};
use axum::{
    Json,
    body::Bytes,
    extract::{Extension, Path, Query, State},
    http::StatusCode,
};
use berth_api::{
    Act, Acted, Action, ErrorBody, ProvisionerInfo, ProvisionerState, SavedProvisioner, code,
};
use berth_core::{
    provision, provisioner,
    registry::Store,
    runner::{Request, Subject, Work},
};
use std::{collections::BTreeMap, io::Write};

fn bad(error: impl std::fmt::Display) -> Fail {
    Fail::new(code::BAD_REQUEST, error.to_string())
}

fn missing(what: impl std::fmt::Display) -> Fail {
    Fail::new(code::NOT_FOUND, what.to_string())
}

/// The server's own registries and secrets.
fn store(state: &Serve) -> Store {
    Store::own(state.store.clone())
}

/// Run a provisioner's work off the runtime: it waits for the provisioner.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> berth_core::Result<T> + Send + 'static,
) -> Answer<Result<T, String>> {
    tokio::task::spawn_blocking(move || work().map_err(|error| error.to_string()))
        .await
        .map_err(internal)
}

/// A registered provisioner by its name.
fn known(state: &Serve, store: &Store, name: &str) -> Answer<ProvisionerInfo> {
    provision::list(&state.paths, store)
        .into_iter()
        .find(|known| known.id == name)
        .ok_or_else(|| missing(format!("No provisioner {name}. See: berth provisioners")))
}

#[utoipa::path(get, path = "/v1/provisioners", summary = "What the plugins can create machines and other resources with, what each asks for and can do, and whether it can be used here.", tag = "set-up",
    responses((status = 200, description = "The provisioners, each with its settings, the required secrets that have no value, and as `problem` why it cannot be used", body = Vec<ProvisionerInfo>)))]
pub async fn list(State(state): App) -> Answer<Json<Vec<ProvisionerInfo>>> {
    Ok(Json(provision::list(&state.paths, &store(&state))))
}

#[utoipa::path(get, path = "/v1/provisioner-types", summary = "The types of provisioner the plugins bring, which a provisioner is registered as.", tag = "set-up",
    responses((status = 200, description = "Each type as `PLUGIN/ID` in `id` and `type`, with what it asks for and can make", body = Vec<ProvisionerInfo>)))]
pub async fn types(State(state): App) -> Answer<Json<Vec<ProvisionerInfo>>> {
    Ok(Json(provision::types(&state.paths)))
}

#[utoipa::path(put, path = "/v1/provisioners/{name}", summary = "Register a provisioner under a name of its own, of a type a plugin brings, or change the settings of one that is registered. A registered one keeps its type.", tag = "set-up",
    params(("name" = String, Path, description = "Its name: lowercase letters, digits, - and _, starting with a letter")),
    request_body = SavedProvisioner,
    responses((status = 200, description = "Registered, as it is listed", body = ProvisionerInfo),
              (status = 400, description = "The name, the type or a setting does not fit; the message says which", body = ErrorBody)))]
pub async fn save(
    State(state): App,
    Path(name): Path<String>,
    body: Bytes,
) -> Answer<Json<ProvisionerInfo>> {
    let saved: SavedProvisioner = serde_json::from_slice(&body).map_err(bad)?;
    provisioner::register(&state.paths, &name, &saved).map_err(bad)?;
    let registries = store(&state);
    Ok(Json(known(&state, &registries, &name)?))
}

#[utoipa::path(delete, path = "/v1/provisioners/{name}", summary = "Forget a registered provisioner, with its settings and the secrets it keeps of its own. It is kept while it holds resources.", tag = "set-up",
    params(("name" = String, Path, description = "Its name")),
    responses((status = 204, description = "Forgotten"),
              (status = 404, description = "No such provisioner", body = ErrorBody),
              (status = 409, description = "It still holds resources; the message names them", body = ErrorBody)))]
pub async fn remove(State(state): App, Path(name): Path<String>) -> Answer<StatusCode> {
    let registries = store(&state);
    known(&state, &registries, &name)?;
    provisioner::remove_entry(&state.paths, &registries, &name)
        .map_err(|error| Fail::new(code::CONFLICT, error.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(get, path = "/v1/provisioners/{name}", summary = "A provisioner's page: what it says of itself now, such as whether its engine answers, and what it holds: berth's resources, with what its own list says.", tag = "set-up",
    params(("name" = String, Path, description = "The provisioner's name"),
           ("arguments" = Option<BTreeMap<String, String>>, Query, style = Form, explode, description = "Values of the provisioner's own arguments as `NAME=VALUE` for this once, such as another server to ask; the others are its settings")),
    responses((status = 200, description = "What it said. A state of `unreachable` or `unusable` comes with why as its message", body = ProvisionerState),
              (status = 404, description = "No such provisioner", body = ErrorBody)))]
pub async fn provisioner(
    State(state): App,
    Path(name): Path<String>,
    Query(given): Query<BTreeMap<String, String>>,
) -> Answer<Json<ProvisionerState>> {
    let registries = store(&state);
    known(&state, &registries, &name)?;
    let (paths, options) = (state.paths.clone(), (state.provision)());
    let found = blocking(move || provisioner::state(&paths, &registries, &name, &given, &options))
        .await?
        .map_err(internal)?;
    Ok(Json(found))
}

#[utoipa::path(get, path = "/v1/provisioners/{name}/actions", summary = "What can be done to a provisioner: its plugin's actions, and berth's own settings of it.", tag = "set-up",
    params(("name" = String, Path, description = "The provisioner's name")),
    responses((status = 200, description = "The actions, each saying whether it can run here now", body = Vec<Action>),
              (status = 404, description = "No such provisioner", body = ErrorBody)))]
pub async fn provisioner_actions(
    State(state): App,
    Path(name): Path<String>,
) -> Answer<Json<Vec<Action>>> {
    let registries = store(&state);
    known(&state, &registries, &name)?;
    Ok(Json(
        provisioner::actions(&state.paths, &registries, &name).map_err(internal)?,
    ))
}

#[utoipa::path(post, path = "/v1/provisioners/{name}/actions/{action}", summary = "Run one of a provisioner's actions. One that changes something is a job, which holds the provisioner's lock; one that only reads answers at once.", tag = "set-up",
    params(("name" = String, Path, description = "The provisioner's name"),
           ("action" = String, Path, description = "The action, as provisioners.actions names it")),
    request_body(content = Option<Act>, description = "Its arguments, which may also give the provisioner's own for this once, and a token that makes asking again safe"),
    responses((status = 202, description = "Started as a job", body = Acted),
              (status = 200, description = "Answered at once: its `message` and `details` as the result", body = Acted),
              (status = 400, description = "An argument is missing or unknown, or the body is not one", body = ErrorBody),
              (status = 403, description = "The caller's role is below the action's", body = ErrorBody),
              (status = 404, description = "No such provisioner or action", body = ErrorBody),
              (status = 409, description = "Something else is being done to the provisioner", body = ErrorBody),
              (status = 501, description = "It cannot run here now; the message says why", body = ErrorBody),
              (status = 502, description = "The provisioner failed, as when its engine does not answer", body = ErrorBody)))]
pub async fn provisioner_act(
    State(state): App,
    access: Option<Extension<Access>>,
    Path((name, action)): Path<(String, String)>,
    body: Bytes,
) -> Answer<(StatusCode, Json<Acted>)> {
    let act = parse(&body)?;
    let registries = store(&state);
    known(&state, &registries, &name)?;
    let actions = provisioner::actions(&state.paths, &registries, &name).map_err(internal)?;
    let Some(found) = actions.iter().find(|known| known.name == action) else {
        let names: Vec<&str> = actions.iter().map(|known| known.name.as_str()).collect();
        return Err(missing(format!(
            "{name} has no action {action}. Its actions: {}",
            names.join(", ")
        )));
    };
    admit(&state, &caller(access), found, &act)?;
    let acting = provisioner::prepare(&state.paths, &registries, &name, &action, &act.arguments)
        .map_err(bad)?;
    let (paths, options) = (state.paths.clone(), (state.provision)());
    if !acting.changes() {
        let answered = blocking(move || acting.run(&paths, &options, &mut std::io::sink()))
            .await?
            .map_err(|error| Fail::new(code::UNREACHABLE, error))?;
        return Ok(answer(Acted {
            result: Some(answered.value()),
            ..Acted::default()
        }));
    }
    let mut request = Request::new(Subject::Provisioner(name), &action);
    request.token = act.token;
    request.secrets = acting.secrets();
    let work = Work::Function(Box::new(move |out| {
        match acting.run(&paths, &options, out) {
            Ok(answered) => {
                for line in answered.lines() {
                    let _ = writeln!(out, "{line}");
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
    Ok(answer(Acted {
        job: Some(job_info(&state.paths, job)),
        ..Acted::default()
    }))
}
