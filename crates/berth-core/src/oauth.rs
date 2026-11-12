//! berth as an OAuth 2.0 client, for a `berth serve` that takes access tokens
//! of an issuer. Two ways to a token, neither of which needs a browser here:
//!
//! - `berth login`: the device grant (RFC 8628). berth shows an address and a
//!   code, a person approves on any device, and berth renews the token itself
//!   from then on.
//! - A client secret in the settings: the client credentials grant, for berth
//!   as a service, with nobody there.
//!
//! The issuer is the `oauth_issuer` setting, or the one the server names
//! (RFC 9728). Tokens are kept per server in `oauth.json` of the data
//! directory, private to the user.

use crate::{paths::Paths, server::http};
use berth_api::escape::encode;
use serde_json::{Map, Value, json};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Where a `berth serve` says which issuer it takes tokens of.
pub const RESOURCE: &str = "/.well-known/oauth-protected-resource";

const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// A token is renewed when it has less than this long left.
const MARGIN: u64 = 60;

/// berth as the issuer knows it.
#[derive(Debug, Clone, PartialEq)]
pub struct Client {
    pub issuer: String,
    pub id: String,
    pub secret: Option<String>,
    pub scope: Option<String>,
    pub audience: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tokens {
    pub access: String,
    pub refresh: Option<String>,
    /// Seconds since 1970; 0 when the issuer did not say.
    pub expires: u64,
}

/// What went wrong, in words: these run on tasks, where an error has to be
/// something that can be sent.
pub type Result<T> = std::result::Result<T, String>;

fn said(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

async fn get(url: &str) -> Result<Value> {
    match http("GET", url, None, vec![]).await.map_err(said)? {
        (200, body) => serde_json::from_slice(&body).map_err(said),
        (status, _) => Err(format!("{url}: HTTP {status}")),
    }
}

/// A form sent to the issuer. An error it answers with comes back as JSON too.
async fn post(url: &str, form: &[(&str, &str)]) -> Result<(u16, Value)> {
    let body: Vec<String> = form
        .iter()
        .map(|(name, value)| format!("{name}={}", encode(value)))
        .collect();
    let kind = Some("application/x-www-form-urlencoded");
    let (status, answer) = http("POST", url, kind, body.join("&").into_bytes())
        .await
        .map_err(said)?;
    let answer = serde_json::from_slice(&answer)
        .map_err(|_| format!("{url}: HTTP {status}, and no JSON"))?;
    Ok((status, answer))
}

/// What the issuer says went wrong, in its words.
fn problem(answer: &Value) -> String {
    let error = answer["error"].as_str().unwrap_or("no reason given");
    match answer["error_description"].as_str() {
        Some(description) => format!("{error}: {description}"),
        None => error.to_owned(),
    }
}

impl Client {
    /// From the settings, with the issuer the server names when none is set.
    /// None when berth has not been made a client.
    pub async fn configured(paths: &Paths, server: &str) -> Result<Option<Client>> {
        let Some(id) = paths.setting("oauth_client_id") else {
            return Ok(None);
        };
        let (mut issuer, mut audience) = (
            paths.setting("oauth_issuer"),
            paths.setting("oauth_audience"),
        );
        if issuer.is_none() {
            let named = get(&format!("{server}{RESOURCE}")).await.map_err(|error| {
                format!("The berth server does not say which issuer it takes tokens of ({error}). Set oauth_issuer.")
            })?;
            issuer = named["authorization_servers"][0]
                .as_str()
                .map(str::to_owned);
            audience = audience.or_else(|| named["resource"].as_str().map(str::to_owned));
        }
        Ok(Some(Client {
            issuer: issuer.ok_or("The berth server names no issuer. Set oauth_issuer.")?,
            id,
            secret: paths.setting("oauth_client_secret"),
            scope: paths.setting("oauth_scope"),
            audience,
        }))
    }

    /// The issuer's metadata: as OpenID Connect has it, then as RFC 8414.
    async fn metadata(&self) -> Result<Value> {
        let issuer = self.issuer.trim_end_matches('/');
        let mut addresses = vec![format!("{issuer}/.well-known/openid-configuration")];
        if let Some((scheme, rest)) = issuer.split_once("://") {
            let (authority, path) = match rest.split_once('/') {
                Some((authority, path)) => (authority, format!("/{path}")),
                None => (rest, String::new()),
            };
            addresses.push(format!(
                "{scheme}://{authority}/.well-known/oauth-authorization-server{path}"
            ));
        }
        let mut last = None;
        for address in addresses {
            match get(&address).await {
                Ok(metadata) => return Ok(metadata),
                Err(error) => last = Some(error),
            }
        }
        Err(last.unwrap_or_else(|| "the issuer has no metadata".into()))
    }

    /// What every request to the issuer carries.
    fn form<'a>(&'a self, grant: &'a str) -> Vec<(&'a str, &'a str)> {
        let mut form = vec![("grant_type", grant), ("client_id", self.id.as_str())];
        let optional = [
            ("client_secret", &self.secret),
            ("scope", &self.scope),
            ("audience", &self.audience),
        ];
        for (name, value) in optional {
            if let Some(value) = value {
                form.push((name, value.as_str()));
            }
        }
        form
    }

    fn tokens(answer: &Value, kept: Option<&str>) -> Result<Tokens> {
        let access = answer["access_token"]
            .as_str()
            .ok_or("The issuer answered without an access token")?;
        Ok(Tokens {
            access: access.to_owned(),
            // An issuer that does not send a new one means the old one stays.
            refresh: answer["refresh_token"].as_str().or(kept).map(str::to_owned),
            expires: answer["expires_in"].as_u64().map_or(0, |left| now() + left),
        })
    }

    async fn grant(&self, form: &[(&str, &str)], kept: Option<&str>) -> Result<Tokens> {
        let endpoint = self.metadata().await?["token_endpoint"]
            .as_str()
            .ok_or("The issuer names no token endpoint")?
            .to_owned();
        match post(&endpoint, form).await? {
            (200, answer) => Self::tokens(&answer, kept),
            (_, answer) => Err(format!("The issuer gave no token: {}", problem(&answer))),
        }
    }

    /// A token for berth itself, with its secret.
    pub async fn credentials(&self) -> Result<Tokens> {
        self.grant(&self.form("client_credentials"), None).await
    }

    pub async fn refresh(&self, token: &str) -> Result<Tokens> {
        let mut form = self.form("refresh_token");
        form.push(("refresh_token", token));
        self.grant(&form, Some(token)).await
    }

    /// The device grant. `show` is given what the person needs: the issuer's
    /// answer with `verification_uri`, `user_code` and often
    /// `verification_uri_complete`. Returns once they approved.
    pub async fn device(&self, show: impl FnOnce(&Value)) -> Result<Tokens> {
        let metadata = self.metadata().await?;
        let endpoint = |name: &str| {
            metadata[name].as_str().map(str::to_owned).ok_or_else(|| {
                format!("The issuer names no {name}: it does not offer this way to log in")
            })
        };
        let (start, token) = (
            endpoint("device_authorization_endpoint")?,
            endpoint("token_endpoint")?,
        );
        let mut form = self.form(DEVICE_GRANT);
        form.remove(0);
        let asked = match post(&start, &form).await? {
            (200, asked) => asked,
            (_, answer) => return Err(format!("The issuer refused: {}", problem(&answer))),
        };
        let code = asked["device_code"]
            .as_str()
            .ok_or("The issuer answered without a device code")?
            .to_owned();
        show(&asked);
        let mut wait = asked["interval"].as_u64().unwrap_or(5);
        let until = now() + asked["expires_in"].as_u64().unwrap_or(600);
        let mut form = self.form(DEVICE_GRANT);
        form.push(("device_code", &code));
        while now() < until {
            tokio::time::sleep(Duration::from_secs(wait)).await;
            let (status, answer) = post(&token, &form).await?;
            match (status, answer["error"].as_str()) {
                (200, _) => return Self::tokens(&answer, None),
                (_, Some("authorization_pending")) => {}
                (_, Some("slow_down")) => wait += 5,
                _ => return Err(format!("Not logged in: {}", problem(&answer))),
            }
        }
        Err("Not logged in: nobody approved in time".into())
    }
}

fn file(paths: &Paths) -> std::path::PathBuf {
    paths.data_dir.join("oauth.json")
}

fn kept(paths: &Paths) -> Map<String, Value> {
    std::fs::read(file(paths))
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default()
}

/// The tokens kept for a server.
pub fn load(paths: &Paths, server: &str) -> Option<Tokens> {
    let all = kept(paths);
    let entry = all.get(server)?;
    Some(Tokens {
        access: entry["access"].as_str()?.to_owned(),
        refresh: entry["refresh"].as_str().map(str::to_owned),
        expires: entry["expires"].as_u64().unwrap_or(0),
    })
}

/// Keep a server's tokens, or forget them with `None`.
pub fn save(paths: &Paths, server: &str, tokens: Option<&Tokens>) -> Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let mut all = kept(paths);
    match tokens {
        Some(tokens) => all.insert(
            server.to_owned(),
            json!({"access": tokens.access, "refresh": tokens.refresh, "expires": tokens.expires}),
        ),
        None => all.remove(server),
    };
    std::fs::create_dir_all(&paths.data_dir).map_err(said)?;
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(file(paths))
        .map_err(said)?;
    let text = serde_json::to_string_pretty(&all).map_err(said)? + "\n";
    out.write_all(text.as_bytes()).map_err(said)
}

/// The access token to show a server: the one kept while it lasts, renewed
/// when it runs out, or a new one for a client with a secret. None when berth
/// is no client and nobody has logged in.
pub async fn token(server: &str) -> Result<Option<String>> {
    let paths = Paths::locate().map_err(said)?;
    let known = load(&paths, server);
    if let Some(tokens) = &known
        && (tokens.expires == 0 || tokens.expires > now() + MARGIN)
    {
        return Ok(Some(tokens.access.clone()));
    }
    let Some(client) = Client::configured(&paths, server).await? else {
        return Ok(None);
    };
    let renewed = match known.as_ref().and_then(|tokens| tokens.refresh.as_deref()) {
        Some(refresh) => client.refresh(refresh).await,
        None if client.secret.is_some() => client.credentials().await,
        None if known.is_some() => Err("it has run out".into()),
        None => return Ok(None),
    };
    // A login that can no longer be renewed is said so, not tried with an old token.
    let tokens = match (renewed, &client.secret, &known) {
        (Ok(tokens), _, _) => tokens,
        (Err(_), Some(_), Some(_)) => client.credentials().await?,
        (Err(error), _, _) => {
            return Err(format!(
                "The login at {} is over ({error}). Log in again: berth login",
                client.issuer
            ));
        }
    };
    save(&paths, server, Some(&tokens))?;
    Ok(Some(tokens.access))
}
