//! The setup key: a short-lived key, made where `berth serve` runs, that lets
//! the first user collect the server's token. Whoever installs the server can
//! hand it on without ever holding the token, and it ends when the user says
//! they are done or after a day. It is kept beside `serve.token` and read on
//! every request, so a running server takes a new one without a restart.

use super::{
    Serve,
    api::{Answer, App, Fail, internal},
    auth,
};
use crate::Result;
use axum::{Json, extract::State, http::HeaderMap};
use berth_api::{Done, ErrorBody, SetupClaim, SetupKey, code, rfc3339, unix_now};
use berth_core::paths::Paths;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// How long a setup key works.
pub const LIFETIME: u64 = 24 * 3600;

#[derive(Serialize, Deserialize)]
struct Stored {
    key: String,
    /// Seconds since the epoch.
    expires: u64,
}

fn path(paths: &Paths) -> PathBuf {
    paths.data_dir.join("serve.setup-key")
}

/// Whether the settings leave `berth serve` without its own token over the network.
fn token_off(paths: &Paths) -> bool {
    paths
        .setting("serve_token")
        .is_some_and(|value| value == "off")
}

/// Make a new setup key in place of any earlier one: the key and when it expires.
pub fn make(paths: &Paths) -> Result<(String, u64)> {
    if token_off(paths) {
        return Err(
            "serve_token is off, so berth serve has no token to hand out: a setup key would let nobody in"
                .into(),
        );
    }
    // The token the key collects is there before the server is first asked.
    super::token(paths)?;
    let mut bytes = [0u8; 32];
    std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut bytes)?;
    let key: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    let expires = unix_now() + LIFETIME;
    let text = serde_json::to_string(&Stored {
        key: key.clone(),
        expires,
    })?;
    // A file of its own, private from the start, that takes the old one's place whole.
    let mut file = tempfile::NamedTempFile::new_in(&paths.data_dir)?;
    std::io::Write::write_all(&mut file, format!("{text}\n").as_bytes())?;
    file.persist(path(paths)).map_err(|error| error.error)?;
    Ok((key, expires))
}

/// Delete the setup key; whether there was one.
pub fn remove(paths: &Paths) -> Result<bool> {
    match std::fs::remove_file(path(paths)) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// The setup key there is now. One that has expired is deleted.
fn current(paths: &Paths) -> Option<Stored> {
    let stored: Stored = serde_json::from_slice(&std::fs::read(path(paths)).ok()?).ok()?;
    if stored.expires <= unix_now() {
        let _ = std::fs::remove_file(path(paths));
        return None;
    }
    Some(stored).filter(|stored| stored.key.len() >= 32)
}

/// `berth setup-key [--remove]`, where `berth serve` keeps its data.
pub fn command(paths: &Paths, args: &[&str]) -> Result<()> {
    match args {
        [] => {
            let (key, expires) = make(paths)?;
            println!("{key}");
            eprintln!(
                "This setup key lets the first user collect the token of this berth server, until {} or until they are done.\n\
                 In a terminal:  berth connect URL   and paste the key when asked.\n\
                 In a browser:   URL/#setup-key=KEY\n\
                 URL is the server's address, such as https://studio.example.ts.net.",
                rfc3339(expires)
            );
            Ok(())
        }
        ["--remove"] => {
            match remove(paths)? {
                true => println!("Removed the setup key"),
                false => println!("There was no setup key"),
            }
            Ok(())
        }
        _ => Err("Usage: berth setup-key [--remove]".into()),
    }
}

/// The setup key a request shows, when it is the one there is now. Every
/// refusal is the same, so a caller learns nothing of why.
fn admit(state: &Serve, headers: &HeaderMap) -> Answer<Stored> {
    let shown = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .unwrap_or("");
    match current(&state.paths) {
        Some(stored) if !shown.is_empty() && auth::same(shown, &stored.key) => Ok(stored),
        _ => Err(Fail::new(
            code::UNAUTHORIZED,
            "Send a setup key of this berth serve as a bearer",
        )),
    }
}

fn described(stored: &Stored) -> SetupKey {
    SetupKey {
        version: berth_api::VERSION.into(),
        api: berth_api::API,
        expires: rfc3339(stored.expires),
    }
}

#[utoipa::path(get, path = "/v1/setup-key", summary = "Whether a setup key works, and the berth serve it is for.", tag = "setup_key",
    responses((status = 200, description = "The server's version and when the key expires", body = SetupKey),
              (status = 401, description = "The key is wrong, missing or expired", body = ErrorBody)))]
pub async fn check(State(state): App, headers: HeaderMap) -> Answer<Json<SetupKey>> {
    Ok(Json(described(&admit(&state, &headers)?)))
}

#[utoipa::path(post, path = "/v1/setup-key", summary = "Collect the server's token with a setup key. It can be asked again until the key is done.", tag = "setup_key",
    responses((status = 200, description = "The server's token", body = SetupClaim),
              (status = 401, description = "The key is wrong, missing or expired", body = ErrorBody),
              (status = 501, description = "This berth serve takes no token over the network", body = ErrorBody)))]
pub async fn claim(State(state): App, headers: HeaderMap) -> Answer<Json<SetupClaim>> {
    let stored = admit(&state, &headers)?;
    let token = state.auth.own_token().ok_or_else(|| {
        Fail::new(
            code::NOT_AVAILABLE,
            "This berth serve takes no token over the network",
        )
    })?;
    let about = described(&stored);
    Ok(Json(SetupClaim {
        token: token.to_owned(),
        version: about.version,
        api: about.api,
        expires: about.expires,
    }))
}

#[utoipa::path(delete, path = "/v1/setup-key", summary = "End the setup key, once its user has the token.", tag = "setup_key",
    responses((status = 200, description = "The key is deleted", body = Done),
              (status = 401, description = "The key is wrong, missing or expired", body = ErrorBody)))]
pub async fn done(State(state): App, headers: HeaderMap) -> Answer<Json<Done>> {
    admit(&state, &headers)?;
    remove(&state.paths).map_err(internal)?;
    Ok(Json(Done { ok: true }))
}
