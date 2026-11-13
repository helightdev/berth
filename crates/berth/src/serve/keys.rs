//! The routes of the server's key ring: its keys with their public halves,
//! making or taking in a key, removing one, and rotating one as a job. A
//! private half comes in, as the bytes of a request, and never goes out.
//! All of them need the role `admin`.

use super::api::{Answer, App, Fail, internal, job_info};
use axum::{
    Json,
    body::Bytes,
    extract::{Path, State},
};
use berth_api::{Acted, Done, ErrorBody, code};
use berth_core::{
    keys::{self, contact, ring},
    runner::{Request, Subject, Work},
    store,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// A key of the ring, by its public half.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct KeyInfo {
    pub name: String,
    /// The public half, a line of `authorized_keys`.
    pub public: String,
    /// Its SHA256 fingerprint, as `ssh-keygen -l` shows it.
    pub fingerprint: String,
    /// The key replacing it while a rotation runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(no_recursion)]
    pub pending: Option<Box<KeyInfo>>,
    /// The server's hosts that log in with it.
    #[serde(default)]
    pub hosts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct KeyList {
    pub keys: Vec<KeyInfo>,
}

fn info(key: ring::Public, hosts: Vec<String>) -> KeyInfo {
    KeyInfo {
        name: key.name,
        public: key.public,
        fingerprint: key.fingerprint,
        pending: key.pending.map(|next| {
            Box::new(KeyInfo {
                name: next.name,
                public: next.public,
                fingerprint: next.fingerprint,
                pending: None,
                hosts: vec![],
            })
        }),
        hosts,
    }
}

/// The ring with the hosts that use each key.
pub fn listing(paths: &berth_core::paths::Paths) -> berth_core::Result<KeyList> {
    let store = store::shared(paths)?;
    let mut keys = vec![];
    for key in ring::list(paths, &store)? {
        let hosts = keys::users(&store, &key.name)?;
        keys.push(info(key, hosts));
    }
    Ok(KeyList { keys })
}

fn refused(error: Box<dyn std::error::Error>) -> Fail {
    let text = error.to_string();
    let kind = if text.starts_with("No key") {
        code::NOT_FOUND
    } else if text.contains("already")
        || text.contains("log in with")
        || text.contains("stays")
        || text.contains("being rotated")
    {
        code::CONFLICT
    } else {
        code::BAD_REQUEST
    };
    Fail::new(kind, text)
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> berth_core::Result<T> + Send + 'static,
) -> Answer<T> {
    // A key's halves come from ssh-keygen and the store, which both wait.
    let done = tokio::task::spawn_blocking(move || work().map_err(|error| error.to_string()))
        .await
        .map_err(internal)?;
    done.map_err(|error| refused(error.into()))
}

#[utoipa::path(get, path = "/v1/keys", summary = "The SSH keys of the server's key ring, by their public halves, with the hosts that log in with each and the key that replaces it while it rotates.", tag = "set-up",
    responses((status = 200, description = "The keys; the key berth is first", body = KeyList)))]
pub async fn list(State(state): App) -> Answer<Json<KeyList>> {
    let paths = state.paths.clone();
    blocking(move || listing(&paths)).await.map(Json)
}

#[utoipa::path(put, path = "/v1/keys/{name}", summary = "Add a key to the ring: a new ed25519 key without a body, or the OpenSSH private key in the body, which may have no passphrase. The answer has its public half only.", tag = "set-up",
    params(("name" = String, Path, description = "The key's name: letters, digits, - and _")),
    request_body(content = Vec<u8>, content_type = "application/octet-stream", description = "An OpenSSH private key, or nothing"),
    responses((status = 200, description = "Added", body = KeyInfo),
              (status = 400, description = "Not a name, not a private key, or a key with a passphrase", body = ErrorBody),
              (status = 409, description = "There is a key of that name", body = ErrorBody)))]
pub async fn add(
    State(state): App,
    Path(name): Path<String>,
    body: Bytes,
) -> Answer<Json<KeyInfo>> {
    let paths = state.paths.clone();
    blocking(move || {
        let store = store::shared(&paths)?;
        let private = (!body.is_empty()).then_some(&body[..]);
        let key = ring::add(&paths, &store, &name, private)?;
        Ok(info(key, vec![]))
    })
    .await
    .map(Json)
}

#[utoipa::path(delete, path = "/v1/keys/{name}", summary = "Remove a key from the ring. Refused for the key berth, while a host logs in with the key, and while it rotates.", tag = "set-up",
    params(("name" = String, Path, description = "The key")),
    responses((status = 200, description = "Removed", body = Done),
              (status = 404, description = "No such key", body = ErrorBody),
              (status = 409, description = "It is berth's, in use or rotating", body = ErrorBody)))]
pub async fn remove(State(state): App, Path(name): Path<String>) -> Answer<Json<Done>> {
    let paths = state.paths.clone();
    blocking(move || {
        let store = store::shared(&paths)?;
        ring::remove(&paths, &store, &name)
    })
    .await?;
    Ok(Json(Done { ok: true }))
}

#[utoipa::path(post, path = "/v1/keys/{name}/rotate", summary = "Replace a key on every host that takes it, as a job: a new key beside it, and on each host the new one added, a login with it checked, and the old one removed. A host that does not answer stays pending; running it again goes on from there. When none is pending the old key is deleted.", tag = "set-up",
    params(("name" = String, Path, description = "The key")),
    responses((status = 200, description = "The job", body = Acted),
              (status = 404, description = "No such key", body = ErrorBody),
              (status = 409, description = "A rotation runs already", body = ErrorBody)))]
pub async fn rotate(State(state): App, Path(name): Path<String>) -> Answer<Json<Acted>> {
    start_rotation(&state, name, None).map(Json)
}

/// Start the rotation of a key as a job: for this route and for the
/// installation's action `rotate-key`.
pub(super) fn start_rotation(
    state: &std::sync::Arc<super::Serve>,
    name: String,
    token: Option<String>,
) -> Answer<Acted> {
    let store = store::shared(&state.paths).map_err(internal)?;
    if ring::read(&store, &name, false)
        .map_err(internal)?
        .is_none()
        && name != berth_api::profile::BERTH_KEY
    {
        return Err(Fail::new(code::NOT_FOUND, format!("No key {name}")));
    }
    let paths = state.paths.clone();
    let mut request = Request::new(Subject::Installation, &format!("rotate-key:{name}"));
    request.changes = true;
    request.token = token;
    let work = Work::Function(Box::new(move |out| {
        match contact::rotate(&paths, &name, out) {
            Ok(rotation) => rotation.finished,
            Err(error) => {
                use std::io::Write;
                let _ = writeln!(out, "✗ {error}");
                false
            }
        }
    }));
    let job = state
        .runner
        .start(request, work)
        .map_err(|error| Fail::new(code::CONFLICT, error.to_string()))?;
    Ok(Acted {
        job: Some(job_info(job)),
        ..Acted::default()
    })
}
