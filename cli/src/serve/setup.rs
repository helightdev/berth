//! The routes that set the server up: the hosts' profiles, what is sent to
//! and installed on a host, and the registries, secrets and settings the
//! server keeps. All of them need the role `admin`.

use super::{
    Serve,
    api::{Answer, App, Fail, internal, remote},
};
use axum::{
    Json,
    extract::{Path, State},
};
use berth_api::{Done, ErrorBody, Session, b64, code};
use berth_core::{
    apply,
    host::Host,
    paths::SETTINGS,
    plugins,
    registry::{self, Store},
    server::safe,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use utoipa::ToSchema;

fn bad(error: impl std::fmt::Display) -> Fail {
    Fail::new(code::BAD_REQUEST, error.to_string())
}

fn missing(what: impl std::fmt::Display) -> Fail {
    Fail::new(code::NOT_FOUND, what.to_string())
}

/// The server's registries and secrets.
fn store(state: &Serve) -> Store {
    Store::at(state.paths.config_dir.clone())
}

// ── Profiles ────────────────────────────────────────────────────────────────

/// Where the profile of a host of this name is. A name is a file name here,
/// and never a path.
fn profile_path(state: &Serve, name: &str) -> Answer<std::path::PathBuf> {
    if !safe(name) || name.ends_with(".json") || name.ends_with(".credentials") {
        return Err(bad(format!("Not a host name: {name}")));
    }
    state.paths.profile(name).map_err(bad)
}

#[utoipa::path(get, path = "/v1/hosts/{name}/profile", summary = "A host's profile, as it is saved.", tag = "set-up",
    params(("name" = String, Path, description = "A saved host")),
    responses((status = 200, description = "The profile, with the passwords and keys in it", body = Value),
              (status = 404, description = "No such host", body = ErrorBody)))]
pub async fn profile(State(state): App, Path(name): Path<String>) -> Answer<Json<Value>> {
    let path = profile_path(&state, &name)?;
    let data = std::fs::read(&path).map_err(|_| missing(format!("No host {name}")))?;
    Ok(Json(serde_json::from_slice(&data).map_err(internal)?))
}

#[utoipa::path(put, path = "/v1/hosts/{name}/profile", summary = "Save a host's profile: a new host, or a changed one.", tag = "set-up",
    params(("name" = String, Path, description = "The name to save it under: letters, digits, `-`, `_` and `.`")),
    request_body(content = Value, description = "The whole profile"),
    responses((status = 200, description = "Saved", body = Done),
              (status = 400, description = "Not a profile, or not a name", body = ErrorBody)))]
pub async fn save_profile(
    State(state): App,
    Path(name): Path<String>,
    Json(mut profile): Json<Value>,
) -> Answer<Json<Done>> {
    let path = profile_path(&state, &name)?;
    if !profile.is_object() || !profile["builder"].is_object() {
        return Err(bad("A profile is an object with a builder in it"));
    }
    // How the caller reached this server is not part of the host.
    if let Some(builder) = profile["builder"].as_object_mut() {
        builder.remove("served");
        builder.remove("served_only");
    }
    // The link is a command this server runs: only its own user names one.
    let kept = std::fs::read(&path)
        .ok()
        .and_then(|data| serde_json::from_slice::<Value>(&data).ok());
    let link = kept.as_ref().map(|kept| &kept["builder"]["link"]);
    if !profile["builder"]["link"].is_null() && link != Some(&profile["builder"]["link"]) {
        return Err(bad(
            "builder.link is a command the server runs, and is set on the server itself",
        ));
    }
    state.paths.prepare().map_err(internal)?;
    let parent = path.parent().ok_or_else(|| internal("no directory"))?;
    // Temporary files are created private, which the profile stays.
    let mut file = tempfile::NamedTempFile::new_in(parent).map_err(internal)?;
    serde_json::to_writer_pretty(&mut file, &profile).map_err(internal)?;
    std::io::Write::write_all(&mut file, b"\n").map_err(internal)?;
    file.persist(&path).map_err(internal)?;
    // Where it is and how it is reached may be different now.
    state.forget(&name);
    state.refresh(false);
    Ok(Json(Done { ok: true }))
}

#[utoipa::path(delete, path = "/v1/hosts/{name}", summary = "Remove a host's profile. The host itself is left as it is.", tag = "set-up",
    params(("name" = String, Path, description = "A saved host")),
    responses((status = 200, description = "Removed", body = Done),
              (status = 404, description = "No such host", body = ErrorBody)))]
pub async fn remove_host(State(state): App, Path(name): Path<String>) -> Answer<Json<Done>> {
    let path = profile_path(&state, &name)?;
    std::fs::remove_file(&path).map_err(|_| missing(format!("No host {name}")))?;
    let _ = std::fs::remove_file(path.with_extension("credentials.json"));
    state.forget(&name);
    Ok(Json(Done { ok: true }))
}

// ── What a host is given ────────────────────────────────────────────────────

/// What a new machine is started with to become the host.
#[derive(Serialize, ToSchema)]
pub struct Seed {
    /// cloud-init user data, for the provider's field of that name.
    pub user_data: String,
}

#[utoipa::path(post, path = "/v1/hosts/{name}/seed", summary = "Render the cloud-init seed that installs the host on a new machine.", tag = "set-up",
    params(("name" = String, Path, description = "A saved host")),
    responses((status = 200, description = "The seed", body = Seed),
              (status = 400, description = "The profile cannot be installed this way; the message says why", body = ErrorBody),
              (status = 404, description = "No such host", body = ErrorBody)))]
pub async fn seed(State(state): App, Path(name): Path<String>) -> Answer<Json<Seed>> {
    let path = profile_path(&state, &name)?;
    let host = Host::load(&state.paths, &name).map_err(|_| missing(format!("No host {name}")))?;
    let source = state.paths.source().map_err(internal)?;
    let dirs = apply::plugin_dirs(&plugins::load(&[
        source.join("plugins"),
        state.paths.config_dir.join("plugins"),
    ]));
    // With a store named for it, the release goes there first and the seed
    // points at what was just published.
    let release = host.config["release_store"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    if !release.is_empty() {
        let (registries, build, plugins) = (store(&state), state.paths.build_dir(), dirs.clone());
        let destination = release.clone();
        let url = tokio::task::spawn_blocking(move || {
            registry::publish_from(&registries, &build, &plugins, &destination)
                .map_err(|error| error.to_string())
        })
        .await
        .map_err(internal)?
        .map_err(|error| bad(format!("Publishing to {release} failed: {error}")))?;
        let mut profile = host.config.clone();
        profile["release_url"] = json!(url);
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&profile).map_err(internal)? + "\n",
        )
        .map_err(internal)?;
    }
    let mut command = tokio::process::Command::new("python3");
    command
        .current_dir(&source)
        .args(["bin/devbox.py", "render", "--kind", "cloud", "--config"])
        .arg(&path)
        .env("DEVBOX_BUILD_DIR", state.paths.build_dir())
        .env("BERTH_PLUGINS", dirs.to_string())
        .stdin(std::process::Stdio::null());
    // A release that is not public: the seed carries the login of the registry it is in.
    let private = host.config["release_private"].as_bool().unwrap_or(false);
    let login = match host.config["release_login"].as_str() {
        Some(name) => Some(name),
        None if private && !release.is_empty() => release.split('/').next(),
        None => None,
    };
    if let Some(registry) = login {
        let login = store(&state)
            .curl_login(registry)
            .map_err(bad)?
            .ok_or_else(|| {
                bad(format!(
                    "Registry {registry} has no stored login for downloading the release"
                ))
            })?;
        command.env("BERTH_RELEASE_LOGIN", login);
    }
    let output = command.output().await.map_err(internal)?;
    if !output.status.success() {
        return Err(bad(String::from_utf8_lossy(&output.stderr).trim()));
    }
    let said = String::from_utf8_lossy(&output.stdout);
    let out = said.lines().last().unwrap_or("").trim();
    let user_data =
        std::fs::read_to_string(std::path::Path::new(out).join("user-data")).map_err(internal)?;
    Ok(Json(Seed { user_data }))
}

#[utoipa::path(post, path = "/v1/hosts/{name}/secrets", summary = "Send the host the secrets and registry logins its profile names.", tag = "set-up",
    params(("name" = String, Path, description = "A saved host")),
    responses((status = 200, description = "The session on the host that installs them", body = Session),
              (status = 400, description = "A secret has no value, or a registry is not known", body = ErrorBody),
              (status = 404, description = "No such host", body = ErrorBody)))]
pub async fn send_secrets(State(state): App, Path(name): Path<String>) -> Answer<Json<Session>> {
    state.refresh(false);
    let remote = remote(&state, &name)?;
    let host = Host::load(&state.paths, &name).map_err(bad)?;
    let payload = store(&state).host_payload(&host.config).map_err(bad)?;
    let Some(link) = &remote.link else {
        let problem = remote.problem.clone().unwrap_or_else(|| "no link".into());
        return Err(Fail::new(code::UNREACHABLE, problem));
    };
    let body = json!({"arguments": {}, "input_b64": b64(format!("{payload}\n").as_bytes())});
    Ok(Json(link.post("/v1/tasks/secrets", &body).await?))
}

// ── Secrets ─────────────────────────────────────────────────────────────────

/// The names of the secrets the server keeps. Their values never leave it
/// except to a host.
#[derive(Serialize, ToSchema)]
pub struct SecretNames {
    /// A registry's password or token is `registry/NAME`.
    pub names: Vec<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct SecretValue {
    pub value: String,
}

fn secret_name(name: &str) -> Answer<&str> {
    let plain = name.strip_prefix("registry/").unwrap_or(name);
    match plugins::valid_name(plain) {
        true => Ok(name),
        false => Err(bad(format!(
            "Not a secret's name: {name} (lower-case letters, digits, - and _)"
        ))),
    }
}

#[utoipa::path(get, path = "/v1/secrets", summary = "The names of the secrets.", tag = "set-up",
    responses((status = 200, description = "The names", body = SecretNames)))]
pub async fn secrets(State(state): App) -> Answer<Json<SecretNames>> {
    let names = store(&state).secrets().map_err(internal)?;
    Ok(Json(SecretNames {
        names: names.keys().cloned().collect(),
    }))
}

#[utoipa::path(put, path = "/v1/secrets/{name}", summary = "Set a secret's value.", tag = "set-up",
    params(("name" = String, Path, description = "The secret; `registry/NAME` is a registry's login")),
    request_body(content = SecretValue, description = "The value"),
    responses((status = 200, description = "Kept", body = Done),
              (status = 400, description = "Not a name", body = ErrorBody)))]
pub async fn set_secret(
    State(state): App,
    Path(name): Path<String>,
    Json(secret): Json<SecretValue>,
) -> Answer<Json<Done>> {
    store(&state)
        .set_secret(secret_name(&name)?, Some(&secret.value))
        .map_err(internal)?;
    Ok(Json(Done { ok: true }))
}

#[utoipa::path(delete, path = "/v1/secrets/{name}", summary = "Remove a secret.", tag = "set-up",
    params(("name" = String, Path, description = "The secret")),
    responses((status = 200, description = "Removed", body = Done),
              (status = 404, description = "No such secret", body = ErrorBody)))]
pub async fn remove_secret(State(state): App, Path(name): Path<String>) -> Answer<Json<Done>> {
    let store = store(&state);
    if store.secret(&name).map_err(internal)?.is_none() {
        return Err(missing(format!("No secret {name}")));
    }
    store.set_secret(&name, None).map_err(internal)?;
    Ok(Json(Done { ok: true }))
}

// ── Registries ──────────────────────────────────────────────────────────────

#[derive(Serialize, ToSchema)]
pub struct Registries {
    /// By name: `type` (nexus, npm, raw or docker), `url`, `username` and, for
    /// a Nexus server, its `repositories`.
    pub registries: Map<String, Value>,
}

#[derive(Serialize, ToSchema)]
pub struct Checked {
    /// One line per thing checked, starting with ✓, ! or ✗.
    pub lines: Vec<String>,
}

#[utoipa::path(get, path = "/v1/registries", summary = "The registries.", tag = "set-up",
    responses((status = 200, description = "The registries by name", body = Registries)))]
pub async fn registries(State(state): App) -> Answer<Json<Registries>> {
    let registries = store(&state).registries().map_err(internal)?;
    Ok(Json(Registries { registries }))
}

#[utoipa::path(put, path = "/v1/registries/{name}", summary = "Add a registry, or change one.", tag = "set-up",
    params(("name" = String, Path, description = "The registry's name here")),
    request_body(content = Value, description = "`type`, `url`, and optionally `username` and `repositories`"),
    responses((status = 200, description = "Saved", body = Done),
              (status = 400, description = "Not a registry", body = ErrorBody)))]
pub async fn save_registry(
    State(state): App,
    Path(name): Path<String>,
    Json(registry): Json<Value>,
) -> Answer<Json<Done>> {
    let kind = registry["type"].as_str().unwrap_or("");
    let url = registry["url"].as_str().unwrap_or("");
    if !plugins::valid_name(&name) || !registry::TYPES.contains(&kind) || url.is_empty() {
        return Err(bad(format!(
            "A registry has a name of lower-case letters, digits, - and _, a type ({}) and a url",
            registry::TYPES.join(", ")
        )));
    }
    let store = store(&state);
    let mut registries = store.registries().map_err(internal)?;
    registries.insert(name, registry);
    store.save_registries(registries).map_err(internal)?;
    Ok(Json(Done { ok: true }))
}

#[utoipa::path(delete, path = "/v1/registries/{name}", summary = "Remove a registry with its stored login.", tag = "set-up",
    params(("name" = String, Path, description = "A registry")),
    responses((status = 200, description = "Removed", body = Done),
              (status = 404, description = "No such registry", body = ErrorBody)))]
pub async fn remove_registry(State(state): App, Path(name): Path<String>) -> Answer<Json<Done>> {
    let store = store(&state);
    let mut registries = store.registries().map_err(internal)?;
    if registries.remove(&name).is_none() {
        return Err(missing(format!("No registry {name}")));
    }
    store
        .set_secret(&format!("registry/{name}"), None)
        .map_err(internal)?;
    store.save_registries(registries).map_err(internal)?;
    Ok(Json(Done { ok: true }))
}

#[utoipa::path(post, path = "/v1/registries/{name}/check", summary = "Whether a registry answers and takes its login.", tag = "set-up",
    params(("name" = String, Path, description = "A registry")),
    responses((status = 200, description = "What was found", body = Checked),
              (status = 404, description = "No such registry", body = ErrorBody)))]
pub async fn check_registry(State(state): App, Path(name): Path<String>) -> Answer<Json<Checked>> {
    let store = store(&state);
    if !store.registries().map_err(internal)?.contains_key(&name) {
        return Err(missing(format!("No registry {name}")));
    }
    // It asks the registry with curl, and waits for it.
    let lines = tokio::task::spawn_blocking(move || {
        registry::check(&store, &name).map_err(|error| error.to_string())
    })
    .await
    .map_err(internal)?
    .map_err(internal)?;
    Ok(Json(Checked { lines }))
}

// ── Settings ────────────────────────────────────────────────────────────────

/// The settings a caller may change. The others say who gets in and where the
/// server's files are, which is set on the server itself.
const OPEN_SETTINGS: &[&str] = &["default_host", "retention_days"];

#[derive(Serialize, ToSchema)]
pub struct Setting {
    pub key: String,
    /// Empty when it is not set, and for a token.
    pub value: String,
    pub meaning: String,
    /// Whether `settings.set` takes it.
    pub open: bool,
}

#[derive(Deserialize, ToSchema)]
pub struct SettingValue {
    /// Empty to go back to the default.
    pub value: String,
}

#[utoipa::path(get, path = "/v1/settings", summary = "The server's settings.", tag = "set-up",
    responses((status = 200, description = "The settings", body = Vec<Setting>)))]
pub async fn settings(State(state): App) -> Answer<Json<Vec<Setting>>> {
    let saved = state.paths.settings().map_err(internal)?;
    let settings = SETTINGS
        .iter()
        .map(|(key, meaning)| Setting {
            key: (*key).into(),
            value: match saved.get(*key) {
                Some(_) if key.ends_with("token") => String::new(),
                Some(Value::String(text)) => text.clone(),
                Some(other) => other.to_string(),
                None => String::new(),
            },
            meaning: (*meaning).into(),
            open: OPEN_SETTINGS.contains(key),
        })
        .collect();
    Ok(Json(settings))
}

#[utoipa::path(put, path = "/v1/settings/{key}", summary = "Change one of the settings that are open to callers.", tag = "set-up",
    params(("key" = String, Path, description = "The setting")),
    request_body(content = SettingValue, description = "The value"),
    responses((status = 200, description = "Saved", body = Done),
              (status = 403, description = "This one is set on the server itself", body = ErrorBody)))]
pub async fn set_setting(
    State(state): App,
    Path(key): Path<String>,
    Json(setting): Json<SettingValue>,
) -> Answer<Json<Done>> {
    if !OPEN_SETTINGS.contains(&key.as_str()) {
        return Err(Fail::new(
            code::FORBIDDEN,
            format!("{key} is set on the server itself, with berth config"),
        ));
    }
    state.paths.set(&key, &setting.value).map_err(bad)?;
    Ok(Json(Done { ok: true }))
}
