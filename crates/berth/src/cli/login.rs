//! `berth login URL`: use the berth server at URL, with the token a setup
//! key collects, or a token given with `--token-file` or `--token-stdin`.
//! The key comes from a prompt or standard input, never from the command
//! line, and the server and token are kept as this user's settings.

use super::args::{Global, Login};
use super::ask;
use super::client;
use super::fail::{Fail, Result};
use super::out;
use berth_page::net;
use berth_page::settings::{self, Settings};
use serde_json::Value;

pub fn login(global: &Global, login: &Login) -> Result<()> {
    let url = berth_page::device::address(&login.url);
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(Fail::usage(format!(
            "{} is no address of a berth server: https://HOST or http://HOST:PORT",
            login.url
        )));
    }
    let settings = client::settings();
    // A token given as such is checked and kept; else a setup key collects one.
    if global.token_file.is_some() || global.token_stdin {
        let token = read_token(global)?;
        let portal = client::connect(&url, &token)?;
        net::runtime()
            .block_on(portal.call("access", None, Value::Null, None))
            .map_err(Fail::from)?;
        settings
            .set(&[(settings::SERVER, &url), (settings::TOKEN, &token)])
            .map_err(|error| Fail::failed(error.to_string()))?;
        out::done(format!("berth uses the berth server at {url} now"));
        return Ok(());
    }
    let key = ask::secret_from(
        "Setup key",
        None,
        login.setup_key_stdin,
        "--setup-key-stdin",
    )?;
    let (url, version) = claim(&settings, &url, key.trim())?;
    out::done(format!(
        "berth uses the berth server at {url} now, berth {version}"
    ));
    if std::env::var_os("BERTH_SERVER").is_some() {
        out::warn("BERTH_SERVER is set in this shell and is used before the setting");
    }
    Ok(())
}

fn read_token(global: &Global) -> Result<String> {
    if let Some(file) = &global.token_file {
        return Ok(std::fs::read_to_string(file)?.trim().to_owned());
    }
    let mut text = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)?;
    Ok(text.trim().to_owned())
}

pub fn logout() -> Result<()> {
    let settings = client::settings();
    let server = settings.get(settings::SERVER);
    settings
        .set(&[(settings::SERVER, ""), (settings::TOKEN, "")])
        .map_err(|error| Fail::failed(error.to_string()))?;
    match server {
        Some(server) => out::done(format!("berth no longer uses the berth server at {server}")),
        None => out::hint("berth used no server of its own"),
    }
    Ok(())
}

/// Claim a token with `key` at `url`: the server's address and version.
/// Nothing is kept unless all of it works.
pub fn claim(settings: &Settings, url: &str, key: &str) -> Result<(String, String)> {
    let url = url.trim().trim_end_matches('/').to_owned();
    let key = key.to_owned();
    let ask = |method: &'static str| {
        let (url, key) = (url.clone(), key.clone());
        async move {
            let (status, body) = net::http(method, &url, "/v1/setup-key", &key)
                .await
                .map_err(Fail::unreachable)?;
            let said: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            match status {
                200 => Ok(said),
                401 => Err(Fail::refused(
                    "The server did not take the setup key: it is wrong, used up or expired. Make a new one where the server runs: berth server setup-key",
                )),
                404 | 405 => Err(Fail::refused(
                    "The server knows no setup keys: it is an older berth",
                )),
                status => Err(Fail::failed(match said["error"]["message"].as_str() {
                    Some(message) => format!("HTTP {status}: {message}"),
                    None => format!("HTTP {status}"),
                })),
            }
        }
    };
    let (version, token) = net::runtime().block_on(async {
        let checked = ask("GET").await?;
        let claimed = ask("POST").await?;
        let token = claimed["token"].as_str().unwrap_or("").to_owned();
        if token.is_empty() {
            return Err(Fail::failed("The server sent no token"));
        }
        let version = checked["version"].as_str().unwrap_or("").to_owned();
        Ok::<_, Fail>((version, token))
    })?;
    // Kept only once the token is seen to open the server.
    let portal = client::connect(&url, &token)?;
    net::runtime()
        .block_on(portal.call("access", None, Value::Null, None))
        .map_err(Fail::from)?;
    settings
        .set(&[(settings::SERVER, &url), (settings::TOKEN, &token)])
        .map_err(|error| Fail::failed(error.to_string()))?;
    if let Err(error) = net::runtime().block_on(ask("DELETE")) {
        out::warn(format!(
            "The setup key could not be ended ({}). It stops by itself within a day, or with berth server setup-key --revoke where the server runs",
            error.message
        ));
    }
    Ok((url, version))
}
