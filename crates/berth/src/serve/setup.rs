//! The routes that set the server up: the hosts' profiles, what is sent to
//! and installed on a host, and the registries, secrets, settings and plugins
//! the server keeps. All of them need the role `admin`.

use super::{
    Serve,
    api::{Answer, App, Fail, internal, remote},
};
use axum::{
    Json,
    body::Bytes,
    extract::{Path, State},
};
use berth_api::{
    Done, ErrorBody, PluginList, PluginOrigin, PluginReport, PluginUpdateSpec, PluginUpdates,
    Session, b64, code,
};
use berth_core::{
    host::Host,
    paths::{SETTINGS, layout},
    plugins,
    profiles::{self, Place},
    provision,
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

/// The server's registries and secrets: its own, even when its settings
/// name another berth server for its commands.
fn store(state: &Serve) -> Store {
    Store::own(state.store.clone())
}

// ── Profiles ────────────────────────────────────────────────────────────────

/// Where the profile of a host of this name is: a host of the server's own,
/// whatever its settings say, by a name that is never a path.
pub(super) fn own_place(name: &str) -> Answer<Place> {
    if !safe(name) {
        return Err(bad(format!("Not a host name: {name}")));
    }
    Place::own(name).map_err(bad)
}

/// A saved host's profile, or a 404.
pub(super) fn saved_profile(state: &Serve, name: &str) -> Answer<(Place, Value)> {
    let place = own_place(name)?;
    match place.load(&state.paths).map_err(internal)? {
        Some(profile) => Ok((place, profile)),
        None => Err(missing(format!("No host {name}"))),
    }
}

#[utoipa::path(get, path = "/v1/hosts/{name}/profile", summary = "A host's profile, as it is saved.", tag = "set-up",
    params(("name" = String, Path, description = "A saved host")),
    responses((status = 200, description = "The profile, with the passwords and keys in it", body = Value),
              (status = 404, description = "No such host", body = ErrorBody)))]
pub async fn profile(State(state): App, Path(name): Path<String>) -> Answer<Json<Value>> {
    Ok(Json(saved_profile(&state, &name)?.1))
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
    let place = own_place(&name)?;
    if !profile.is_object() || !profile["builder"].is_object() {
        return Err(bad("A profile is an object with a builder in it"));
    }
    // How the caller reached this server is not part of the host.
    if let Some(builder) = profile["builder"].as_object_mut() {
        builder.remove("served");
        builder.remove("served_only");
    }
    // The link is a command this server runs: only its own user names one.
    let kept = place.load(&state.paths).map_err(internal)?;
    let link = kept.as_ref().map(|kept| &kept["builder"]["link"]);
    if !profile["builder"]["link"].is_null() && link != Some(&profile["builder"]["link"]) {
        return Err(bad(
            "builder.link is a command the server runs, and is set on the server itself",
        ));
    }
    place.save(&state.paths, &profile).map_err(internal)?;
    // Where it is and how it is reached may be different now.
    state.forget(&name);
    state.refresh(false);
    Ok(Json(Done { ok: true }))
}

#[utoipa::path(delete, path = "/v1/hosts/{name}", summary = "Remove a host's profile. The host itself is left as it is.", tag = "set-up",
    params(("name" = String, Path, description = "A saved host")),
    responses((status = 200, description = "Removed", body = Done),
              (status = 404, description = "No such host", body = ErrorBody),
              (status = 409, description = "It has a machine berth created, which is deleted first, or one is being created", body = ErrorBody)))]
pub async fn remove_host(State(state): App, Path(name): Path<String>) -> Answer<Json<Done>> {
    let (place, _) = saved_profile(&state, &name)?;
    // Without its record nobody could delete the machine any more.
    if let Some(record) = provision::Record::load_at(&state.paths, &place)
        .map_err(internal)?
        .filter(|record| !record.id.is_empty())
    {
        return Err(Fail::new(
            code::CONFLICT,
            format!(
                "{name} has a machine berth created, {} of {}. Delete it first: berth instance {name} destroy",
                record.id, record.provisioner
            ),
        ));
    }
    if provision::busy(&state.paths, &name) {
        return Err(Fail::new(
            code::CONFLICT,
            format!("A machine is being created for {name}. Remove it when that has ended"),
        ));
    }
    profiles::remove(&state.paths, &place).map_err(internal)?;
    state.forget(&name);
    Ok(Json(Done { ok: true }))
}

/// The passwords berth generated for a host's accounts.
#[derive(Serialize, ToSchema)]
pub struct Credentials {
    /// The password of each account, by the account's name.
    pub credentials: Map<String, Value>,
}

#[utoipa::path(get, path = "/v1/hosts/{name}/credentials", summary = "The passwords the server generated for a host's accounts.", tag = "set-up",
    params(("name" = String, Path, description = "A saved host")),
    responses((status = 200, description = "The passwords by account; none when none were generated", body = Credentials),
              (status = 404, description = "No such host", body = ErrorBody)))]
pub async fn credentials(State(state): App, Path(name): Path<String>) -> Answer<Json<Credentials>> {
    let (place, _) = saved_profile(&state, &name)?;
    let credentials = berth_core::credentials::load(&state.paths, &place).map_err(internal)?;
    Ok(Json(Credentials { credentials }))
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
    saved_profile(&state, &name)?;
    let paths = state.paths.clone();
    let registries = store(&state);
    // It may publish the release and runs the seed builder, and waits for both.
    let seed = tokio::task::spawn_blocking(move || {
        berth_core::seed::cloud(&paths, &registries, &name, false)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(internal)?
    .map_err(bad)?;
    Ok(Json(Seed {
        user_data: seed.user_data,
    }))
}

#[utoipa::path(post, path = "/v1/hosts/{name}/secrets", summary = "Send the host the secrets and registry logins its profile names.", tag = "set-up",
    params(("name" = String, Path, description = "A saved host")),
    responses((status = 200, description = "The session on the host that installs them", body = Session),
              (status = 400, description = "A secret has no value, or a registry is not known", body = ErrorBody),
              (status = 404, description = "No such host", body = ErrorBody)))]
pub async fn send_secrets(State(state): App, Path(name): Path<String>) -> Answer<Json<Session>> {
    state.refresh(false);
    let remote = remote(&state, &name)?;
    let host = Host::load_at(&state.paths, own_place(&name)?).map_err(bad)?;
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
    let names = store(&state).all_secret_names().map_err(internal)?;
    Ok(Json(SecretNames { names }))
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
    if !store.has_secret(&name).map_err(internal)? {
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
const OPEN_SETTINGS: &[&str] = &[
    "default_host",
    "retention_days",
    "tailnet_login_server",
    "tailnet_user",
    "tailnet_tags",
];

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
                Some(_) if berth_core::paths::is_secret(key) => String::new(),
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

// ── Plugins ─────────────────────────────────────────────────────────────────

/// Where the server keeps its own plugins.
fn plugin_dir(state: &Serve) -> std::path::PathBuf {
    plugins::own_dir(&state.paths.config_dir)
}

/// One npm at a time works on the plugin directory.
static NPM: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Run a change of the plugin directory, which waits for npm.
async fn change_plugins(
    work: impl FnOnce() -> berth_core::Result<Vec<String>> + Send + 'static,
) -> Answer<Json<PluginReport>> {
    let lines = tokio::task::spawn_blocking(move || {
        let _one = NPM.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        work().map_err(|error| error.to_string())
    })
    .await
    .map_err(internal)?
    .map_err(bad)?;
    Ok(Json(PluginReport {
        lines,
        behind: vec![],
    }))
}

/// What npm installs, with the server's npm configuration and none of the
/// package's scripts.
#[derive(Deserialize, ToSchema)]
pub struct PluginSpec {
    /// A package of the server's npm registry, `[@SCOPE/]NAME[@VERSION]`, or
    /// the https address of a .tgz.
    pub spec: String,
}

#[utoipa::path(get, path = "/v1/plugins", summary = "The plugins: those that come with berth and the server's own.", tag = "set-up",
    responses((status = 200, description = "The plugins with their features", body = PluginList)))]
pub async fn plugins(State(state): App) -> Answer<Json<PluginList>> {
    let source = state.paths.source().map_err(internal)?;
    let own = plugin_dir(&state);
    Ok(Json(PluginList {
        plugins: plugins::found(&source, &own)
            .iter()
            .map(|plugin| plugin.info(&own))
            .collect(),
        dir: own.display().to_string(),
    }))
}

#[utoipa::path(post, path = "/v1/plugins", summary = "Install a plugin with npm from a registry or an address.", tag = "set-up",
    request_body(content = PluginSpec, description = "What to install"),
    responses((status = 200, description = "Installed; what npm said", body = PluginReport),
              (status = 400, description = "Not a package or address, or npm failed; the message has its output", body = ErrorBody)))]
pub async fn add_plugin(
    State(state): App,
    Json(asked): Json<PluginSpec>,
) -> Answer<Json<PluginReport>> {
    let spec = asked.spec.trim().to_owned();
    if !plugins::registry_spec(&spec) {
        return Err(bad(format!(
            "{spec} is no package of a registry or https address of a .tgz. A file of yours is sent with plugins.upload"
        )));
    }
    let paths = state.paths.clone();
    change_plugins(move || plugins::install(&paths, &spec)).await
}

#[utoipa::path(post, path = "/v1/plugins/package", summary = "Install a plugin from a .tgz, as npm pack makes one, which the server keeps.", tag = "set-up",
    request_body(content = Vec<u8>, content_type = "application/gzip", description = "The .tgz"),
    responses((status = 200, description = "Installed; what npm said", body = PluginReport),
              (status = 400, description = "Not a .tgz, or npm failed; the message has its output", body = ErrorBody)))]
pub async fn upload_plugin(State(state): App, body: Bytes) -> Answer<Json<PluginReport>> {
    let paths = state.paths.clone();
    change_plugins(move || plugins::install_upload(&paths, &body)).await
}

#[utoipa::path(delete, path = "/v1/plugins/{id}", summary = "Remove one of the server's own plugins.", tag = "set-up",
    params(("id" = String, Path, description = "The plugin's id")),
    responses((status = 200, description = "Removed; what npm said", body = PluginReport),
              (status = 404, description = "No such plugin", body = ErrorBody),
              (status = 409, description = "It comes with berth and stays", body = ErrorBody)))]
pub async fn remove_plugin(
    State(state): App,
    Path(id): Path<String>,
) -> Answer<Json<PluginReport>> {
    let source = state.paths.source().map_err(internal)?;
    let own = plugin_dir(&state);
    let plugin = plugins::found(&source, &own)
        .into_iter()
        .find(|plugin| plugin.id == id)
        .ok_or_else(|| missing(format!("No plugin {id}")))?;
    if plugin.origin(&own) == PluginOrigin::Shipped {
        return Err(Fail::new(
            code::CONFLICT,
            format!("{} comes with berth and stays", plugin.label),
        ));
    }
    let paths = state.paths.clone();
    change_plugins(move || plugins::remove(&paths, &plugin)).await
}

#[utoipa::path(get, path = "/v1/plugins/updates", summary = "Ask the registries the server's own plugins came from whether they have newer versions.", tag = "set-up",
    responses((status = 200, description = "Each of the server's own plugins, with the latest version where there is a newer one", body = PluginUpdates),
              (status = 400, description = "npm could not be run", body = ErrorBody)))]
pub async fn check_plugins(State(state): App) -> Answer<Json<PluginUpdates>> {
    let source = state.paths.source().map_err(internal)?;
    let own = plugin_dir(&state);
    let checked = tokio::task::spawn_blocking(move || {
        let _one = NPM.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        plugins::check(&own, &plugins::found(&source, &own)).map_err(|error| error.to_string())
    })
    .await
    .map_err(internal)?
    .map_err(bad)?;
    Ok(Json(checked))
}

#[utoipa::path(post, path = "/v1/plugins/update", summary = "Update one of the server's own plugins, or all, to the latest version of their registry, and say which hosts are behind then.", tag = "set-up",
    request_body(content = Option<PluginUpdateSpec>, description = "The plugin, by id or npm name; all without one"),
    responses((status = 200, description = "Updated; what npm said, and the hosts that need applying", body = PluginReport),
              (status = 400, description = "It has no registry to ask, comes with berth, or npm failed; the message says which", body = ErrorBody),
              (status = 404, description = "No such plugin", body = ErrorBody)))]
pub async fn update_plugins(State(state): App, body: Bytes) -> Answer<Json<PluginReport>> {
    let asked: PluginUpdateSpec = match body.is_empty() {
        true => PluginUpdateSpec::default(),
        false => serde_json::from_slice(&body).map_err(bad)?,
    };
    let source = state.paths.source().map_err(internal)?;
    let own = plugin_dir(&state);
    let found = plugins::found(&source, &own);
    if let Some(name) = &asked.id
        && !found
            .iter()
            .any(|plugin| &plugin.id == name || &plugin.package == name)
    {
        return Err(missing(format!("No plugin {name}")));
    }
    let paths = state.paths.clone();
    let Json(mut report) =
        change_plugins(move || plugins::update(&paths, &found, asked.id.as_deref())).await?;
    report.behind = super::api::behind_hosts(&state).await;
    Ok(Json(report))
}

// ── What a set-up screen needs ──────────────────────────────────────────────

/// Something a plugin installs, for the Features page of a host.
#[derive(Serialize, ToSchema)]
pub struct FeatureInfo {
    /// `PLUGIN/FEATURE`, as a profile's `devbox_plugin_features` names it.
    pub key: String,
    pub label: String,
    /// The group of the Features page it is listed in.
    pub group: String,
    pub help: String,
}

#[derive(Serialize, ToSchema)]
pub struct Catalogue {
    /// The profile a new host starts from.
    pub template: Value,
    /// What the server's plugins can install, in the order they are applied.
    pub features: Vec<FeatureInfo>,
    /// The server's own SSH public key, which the operator of a host it sets
    /// up takes unless the profile says otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

#[utoipa::path(get, path = "/v1/setup", summary = "What a new host starts from, and what plugins can install on one.", tag = "set-up",
    responses((status = 200, description = "The starting profile and the plugins' features", body = Catalogue)))]
pub async fn catalogue(State(state): App) -> Answer<Json<Catalogue>> {
    let source = state.paths.source().map_err(internal)?;
    let template = std::fs::read(source.join(layout::EXAMPLE)).map_err(internal)?;
    let found = plugins::load(&[
        source.join("plugins"),
        state.paths.config_dir.join("plugins"),
    ]);
    let features = found
        .iter()
        .flat_map(|plugin| {
            plugin.features.iter().map(|feature| FeatureInfo {
                key: format!("{}/{}", plugin.id, feature.id),
                label: feature.label.clone(),
                group: feature.group.clone(),
                help: feature.help.to_owned(),
            })
        })
        .collect();
    let mut template: Value = serde_json::from_slice(&template).map_err(internal)?;
    // How a host is reached is part of every profile, also of one not reached yet.
    if !template["builder"].is_object() {
        template["builder"] = json!({});
    }
    Ok(Json(Catalogue {
        template,
        features,
        key: state.paths.own_public_key(),
    }))
}

/// Something for the server to make up for a profile.
#[derive(Deserialize, ToSchema)]
pub struct Generate {
    /// `hostname`, `password` or `tailscale_key`.
    pub what: String,
    /// For `password`: the password to hash. Without it one is made up and
    /// kept in the server's store with `host`, under `user`, for
    /// `hosts.credentials` to give.
    pub password: Option<String>,
    pub host: Option<String>,
    pub user: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct Generated {
    /// The host name, the password's hash, or the key.
    pub value: String,
    /// What to tell whoever asked.
    pub note: String,
    /// A password that was made up, for whoever asked to write down.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

#[utoipa::path(post, path = "/v1/setup/generate", summary = "Make up a host name, hash or make up a password, or create a one-use enrollment key at the tailnet's control server.", tag = "set-up",
    request_body(content = Generate, description = "What to make"),
    responses((status = 200, description = "What was made", body = Generated),
              (status = 400, description = "It could not be made; the message says why", body = ErrorBody)))]
pub async fn generate(State(state): App, Json(asked): Json<Generate>) -> Answer<Json<Generated>> {
    use berth_core::credentials;
    let kept = match (&asked.host, asked.what.as_str()) {
        (Some(host), "password") if asked.password.is_none() => Some(own_place(host)?),
        _ => None,
    };
    let paths = state.paths.clone();
    let made = tokio::task::spawn_blocking(move || -> Result<Generated, String> {
        let fail = |error: Box<dyn std::error::Error>| error.to_string();
        match asked.what.as_str() {
            "hostname" => Ok(Generated {
                value: credentials::hostname().map_err(fail)?,
                note: String::new(),
                password: None,
            }),
            "password" => match (asked.password, kept) {
                (Some(password), _) => Ok(Generated {
                    value: credentials::hash(&password).map_err(fail)?,
                    note: "Password hashed".into(),
                    password: None,
                }),
                (None, Some(place)) => {
                    let password = credentials::password().map_err(fail)?;
                    let value = credentials::hash(&password).map_err(fail)?;
                    let user = asked.user.filter(|user| !user.is_empty());
                    let user = user.as_deref().unwrap_or("account");
                    credentials::save(&paths, &place, user, &password).map_err(fail)?;
                    Ok(Generated {
                        value,
                        note: format!(
                            "Generated password kept in the server's store; see it with: berth credentials {}",
                            place.argument()
                        ),
                        password: Some(password),
                    })
                }
                (None, None) => {
                    Err("A made-up password is kept with a host: name the host".into())
                }
            },
            "tailscale_key" => {
                let host = asked.host.unwrap_or_default();
                let config = Place::own(&host)
                    .and_then(|place| place.load(&paths))
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                let (value, note) =
                    berth_core::tailnet::generate(&paths, &config, &host).map_err(fail)?;
                Ok(Generated {
                    value,
                    note,
                    password: None,
                })
            }
            other => Err(format!("Nothing to make for {other}")),
        }
    })
    .await
    .map_err(internal)?
    .map_err(bad)?;
    Ok(Json(made))
}
