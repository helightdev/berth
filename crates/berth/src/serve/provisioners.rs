//! The routes that create the machines of saved hosts with the provisioners
//! of plugins, and look after them. The provisioners run here, with the
//! server's secrets. All of them need the role `admin`.

use super::{
    Serve,
    api::{Answer, App, Fail, JobInfo, internal, start_job},
    setup::profile_path,
};
use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
};
use berth_api::{Done, ErrorBody, Instance, ListedInstance, Provision, ProvisionerInfo, code};
use berth_core::{
    host::Host,
    paths::Paths,
    provision::{self, Record},
    registry::Store,
};
use serde_json::Value;
use std::{collections::BTreeMap, io::Write};

fn bad(error: impl std::fmt::Display) -> Fail {
    Fail::new(code::BAD_REQUEST, error.to_string())
}

fn missing(what: impl std::fmt::Display) -> Fail {
    Fail::new(code::NOT_FOUND, what.to_string())
}

fn conflict(what: impl std::fmt::Display) -> Fail {
    Fail::new(code::CONFLICT, what.to_string())
}

/// The server's registries and secrets.
fn store(state: &Serve) -> Store {
    Store::at(state.paths.config_dir.clone())
}

fn paths(state: &Serve) -> Paths {
    Paths {
        config_dir: state.paths.config_dir.clone(),
        data_dir: state.paths.data_dir.clone(),
    }
}

/// Run a provisioner's work off the runtime: it waits for the provisioner.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> berth_core::Result<T> + Send + 'static,
) -> Answer<Result<T, String>> {
    tokio::task::spawn_blocking(move || work().map_err(|error| error.to_string()))
        .await
        .map_err(internal)
}

/// A saved host, by a name that is never a path.
fn saved(state: &Serve, name: &str) -> Answer<()> {
    match profile_path(state, name)?.is_file() {
        true => Ok(()),
        false => Err(missing(format!("No host {name}"))),
    }
}

/// A provisioner by `PLUGIN/ID`.
fn known(state: &Serve, store: &Store, name: &str) -> Answer<ProvisionerInfo> {
    provision::list(&state.paths, store)
        .into_iter()
        .find(|known| known.id == name)
        .ok_or_else(|| missing(format!("No provisioner {name}. See: berth provisioners")))
}

/// The record of the machine berth created for a host.
fn record(state: &Serve, name: &str) -> Answer<Record> {
    Record::load(&state.paths, name)
        .map_err(internal)?
        .ok_or_else(|| missing(format!("{name} has no machine that berth created")))
}

/// Refused while something else is done to the host's machine.
fn idle(state: &Serve, name: &str) -> Answer<()> {
    match provision::busy(&state.paths, name) {
        true => Err(conflict(format!(
            "Something else is being done to {name}'s machine. Try again when it has ended"
        ))),
        false => Ok(()),
    }
}

/// How a host is reached, to tell whether creating its machine changed it.
fn reached(paths: &Paths, name: &str) -> Value {
    Host::load(paths, name)
        .map(|host| host.config["builder"].clone())
        .unwrap_or_default()
}

#[utoipa::path(get, path = "/v1/provisioners", summary = "What the plugins can create machines with, and what each asks for.", tag = "set-up",
    responses((status = 200, description = "The provisioners, with the required secrets that have no value", body = Vec<ProvisionerInfo>)))]
pub async fn list(State(state): App) -> Answer<Json<Vec<ProvisionerInfo>>> {
    Ok(Json(provision::list(&state.paths, &store(&state))))
}

#[utoipa::path(get, path = "/v1/provisioners/{plugin}/{id}/instances", summary = "The machines a provisioner has that berth created, as it lists them.", tag = "set-up",
    params(("plugin" = String, Path, description = "The plugin"),
           ("id" = String, Path, description = "The provisioner within the plugin"),
           ("arguments" = Option<BTreeMap<String, String>>, Query, style = Form, explode, description = "The provisioner's arguments as `NAME=VALUE`, such as the server to ask; the others take their defaults")),
    responses((status = 200, description = "The machines", body = Vec<ListedInstance>),
              (status = 400, description = "It cannot list, an argument or a secret is missing, or it failed; the message says which", body = ErrorBody),
              (status = 404, description = "No such provisioner", body = ErrorBody)))]
pub async fn instances(
    State(state): App,
    Path((plugin, id)): Path<(String, String)>,
    Query(given): Query<BTreeMap<String, String>>,
) -> Answer<Json<Vec<ListedInstance>>> {
    let name = format!("{plugin}/{id}");
    let registries = store(&state);
    known(&state, &registries, &name)?;
    let (paths, options) = (paths(&state), (state.provision)());
    let listed =
        blocking(move || provision::instances(&paths, &registries, &name, &given, &options))
            .await?
            .map_err(bad)?;
    Ok(Json(listed))
}

#[utoipa::path(post, path = "/v1/hosts/{name}/instance", summary = "Create the machine for a saved host with a provisioner and set it up, as a job.", tag = "set-up",
    params(("name" = String, Path, description = "A saved host")),
    request_body(content = Provision, description = "The provisioner and its arguments"),
    responses((status = 200, description = "The job that creates the machine and sets it up", body = JobInfo),
              (status = 400, description = "An argument or a secret is missing or unknown, or the profile does not fit; the message says which", body = ErrorBody),
              (status = 404, description = "No such host or provisioner", body = ErrorBody),
              (status = 409, description = "The host has a machine already, or something else is being done to it", body = ErrorBody)))]
pub async fn create(
    State(state): App,
    Path(name): Path<String>,
    body: Bytes,
) -> Answer<Json<JobInfo>> {
    saved(&state, &name)?;
    let asked: Provision = serde_json::from_slice(&body).map_err(bad)?;
    let registries = store(&state);
    known(&state, &registries, &asked.provisioner)?;
    if let Some(kept) = Record::load(&state.paths, &name)
        .map_err(internal)?
        .filter(|kept| !kept.id.is_empty())
    {
        return Err(conflict(format!(
            "{name} has a machine already, {} of {}. Delete it with: berth instance {name} destroy",
            kept.id, kept.provisioner
        )));
    }
    idle(&state, &name)?;
    // Checked before there is a job, so a refusal is an answer of its own.
    let prepared = provision::prepare(&state.paths, &registries, &name, &asked).map_err(bad)?;
    let before = reached(&state.paths, &name);
    let (watcher, runtime) = (state.clone(), tokio::runtime::Handle::current());
    let host = name.clone();
    let job = start_job(&state, &name, "provision", move |output| {
        let paths = &watcher.paths;
        let done = prepared.run(paths, &registries, &(watcher.provision)(), output);
        if let Err(error) = &done {
            let _ = writeln!(output, "✗ {error}");
        }
        // An address the provisioner gave is how the host is reached now.
        if reached(paths, &host) != before {
            let _inside = runtime.enter();
            watcher.forget(&host);
            watcher.refresh(false);
        }
        done.is_ok()
    })?;
    Ok(Json(job))
}

#[utoipa::path(get, path = "/v1/hosts/{name}/instance", summary = "The machine berth created for a host, and what its provisioner says of it now.", tag = "set-up",
    params(("name" = String, Path, description = "A saved host")),
    responses((status = 200, description = "The machine; a state of `unknown` comes with the problem", body = Instance),
              (status = 404, description = "No such host, or berth created no machine for it", body = ErrorBody)))]
pub async fn get(State(state): App, Path(name): Path<String>) -> Answer<Json<Instance>> {
    saved(&state, &name)?;
    let (paths, registries, options) = (paths(&state), store(&state), (state.provision)());
    let host = name.clone();
    let instance = blocking(move || provision::status(&paths, &registries, &host, &options))
        .await?
        .map_err(internal)?;
    instance
        .map(Json)
        .ok_or_else(|| missing(format!("{name} has no machine that berth created")))
}

#[utoipa::path(post, path = "/v1/hosts/{name}/instance/{verb}", summary = "Start or stop the machine berth created for a host, when its provisioner can.", tag = "set-up",
    params(("name" = String, Path, description = "A saved host"),
           ("verb" = String, Path, description = "`start` or `stop`")),
    responses((status = 200, description = "Done", body = Done),
              (status = 400, description = "Not start or stop, its provisioner cannot, or it failed; the message says which", body = ErrorBody),
              (status = 404, description = "No such host, or berth created no machine for it", body = ErrorBody),
              (status = 409, description = "Something else is being done to it", body = ErrorBody)))]
pub async fn power(
    State(state): App,
    Path((name, verb)): Path<(String, String)>,
) -> Answer<Json<Done>> {
    saved(&state, &name)?;
    if !matches!(verb.as_str(), "start" | "stop") {
        return Err(bad(format!("A machine is started or stopped, not {verb}")));
    }
    record(&state, &name)?;
    idle(&state, &name)?;
    let (paths, registries, options) = (paths(&state), store(&state), (state.provision)());
    blocking(move || {
        provision::power(
            &paths,
            &registries,
            &name,
            &verb,
            &options,
            &mut std::io::sink(),
        )
    })
    .await?
    .map_err(bad)?;
    Ok(Json(Done { ok: true }))
}

#[utoipa::path(delete, path = "/v1/hosts/{name}/instance", summary = "Delete the machine berth created for a host, as a job. The profile stays.", tag = "set-up",
    params(("name" = String, Path, description = "A saved host")),
    responses((status = 200, description = "The job that deletes the machine", body = JobInfo),
              (status = 400, description = "A secret its provisioner needs has no value", body = ErrorBody),
              (status = 404, description = "No such host or provisioner, or berth created no machine for it", body = ErrorBody),
              (status = 409, description = "Something else is being done to it", body = ErrorBody)))]
pub async fn destroy(State(state): App, Path(name): Path<String>) -> Answer<Json<JobInfo>> {
    saved(&state, &name)?;
    let kept = record(&state, &name)?;
    idle(&state, &name)?;
    let registries = store(&state);
    // A record without an id is only forgotten, which needs no provisioner.
    if !kept.id.is_empty() {
        let found = known(&state, &registries, &kept.provisioner).map_err(|_| {
            missing(format!(
                "No provisioner {} any more, which made this machine: add its plugin again",
                kept.provisioner
            ))
        })?;
        if !found.missing_secrets.is_empty() {
            return Err(bad(format!(
                "{} needs the secrets {}: berth secret set NAME",
                kept.provisioner,
                found.missing_secrets.join(", ")
            )));
        }
    }
    let watcher = state.clone();
    let host = name.clone();
    let job = start_job(&state, &name, "unprovision", move |output| {
        let done = provision::destroy(
            &watcher.paths,
            &registries,
            &host,
            &(watcher.provision)(),
            output,
        );
        if let Err(error) = &done {
            let _ = writeln!(output, "✗ {error}");
        }
        done.is_ok()
    })?;
    Ok(Json(job))
}
