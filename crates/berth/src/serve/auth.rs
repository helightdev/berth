//! Who may do what on `berth serve` over the network.
//!
//! A caller shows one of two things, as a bearer or as the protocol of a
//! WebSocket: the server's own token, which may do everything, or an access
//! token of the OAuth 2.0 issuer the server is set to trust. Such a token is a
//! JWT signed with one of the issuer's keys, and a claim of it names the role:
//! `berth:view`, `berth:operate` or `berth:admin`. berth keeps no users; who
//! gets which role is the issuer's to say.

use berth_api::{
    code,
    ops::{self, Kind, Op, Role},
};
use berth_core::paths::Paths;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, jwk::Jwk};
use serde_json::Value;
use std::time::{Duration, Instant};

/// A refusal on its way out: the kind of error and what to say.
pub type Refusal = ((u16, &'static str), String);

/// What a caller was let in as.
#[derive(Debug, Clone, PartialEq)]
pub struct Access {
    pub role: Role,
    /// The `sub` of an access token.
    pub subject: Option<String>,
}

impl Access {
    /// A caller on this machine's socket, who is the user berth serve runs as.
    pub fn local() -> Access {
        Access {
            role: Role::Admin,
            subject: None,
        }
    }
}

pub struct Auth {
    /// The server's own token, when it is taken.
    token: Option<String>,
    /// Whether set-up is open to callers over the network.
    setup: bool,
    oauth: Option<Oauth>,
}

fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0, |all, (x, y)| all | (x ^ y))
            == 0
}

impl Auth {
    /// As the settings and the environment say. `token` is the server's own.
    pub fn configured(paths: &Paths, token: Option<String>) -> Result<Auth, String> {
        let setting = |key| paths.setting(key);
        let off = |key| setting(key).is_some_and(|value| value == "off");
        let oauth = setting("oauth_issuer").map(|issuer| Oauth {
            issuer,
            audience: setting("oauth_audience"),
            claim: setting("oauth_claim").unwrap_or_else(|| "scope".into()),
            jwks: setting("oauth_jwks"),
            keys: tokio::sync::Mutex::new(Keys::default()),
        });
        if off("serve_token") && oauth.is_none() {
            return Err(
                "serve_token is off and no oauth_issuer is set: nobody could get in".into(),
            );
        }
        Ok(Auth {
            token: token.filter(|_| !off("serve_token")),
            setup: !off("serve_setup"),
            oauth,
        })
    }

    /// Only the server's own token, with set-up open.
    #[cfg(test)]
    pub fn token(token: Option<String>) -> Auth {
        Auth {
            token,
            setup: true,
            oauth: None,
        }
    }

    /// The server's token and an issuer whose keys are at an address.
    #[cfg(test)]
    pub fn trusting(token: &str, issuer: &str, jwks: &str, setup: bool) -> Auth {
        Auth {
            token: Some(token.to_owned()),
            setup,
            oauth: Some(Oauth {
                issuer: issuer.to_owned(),
                audience: Some("berth".into()),
                claim: "scope".into(),
                jwks: Some(jwks.to_owned()),
                keys: tokio::sync::Mutex::new(Keys::default()),
            }),
        }
    }

    /// What tells a client where to get an access token (RFC 9728).
    pub fn resource(&self) -> Option<Value> {
        let oauth = self.oauth.as_ref()?;
        Some(serde_json::json!({
            "resource": oauth.audience,
            "authorization_servers": [oauth.issuer],
            "bearer_methods_supported": ["header"],
        }))
    }

    /// What the callers over the network are told about set-up.
    pub fn setup(&self) -> bool {
        self.setup
    }

    /// How a caller gets in, for the line `berth serve` prints.
    pub fn describe(&self) -> String {
        let mut ways = vec![];
        if self.token.is_some() {
            ways.push("the token".to_owned());
        }
        if let Some(oauth) = &self.oauth {
            ways.push(format!("access tokens of {}", oauth.issuer));
        }
        ways.join(" or ")
    }

    /// Let a caller in by what it shows. With several things shown, the first
    /// that is good counts, and is returned to be named in a handshake.
    pub async fn admit(&self, shown: &[String]) -> Result<(Access, String), Refusal> {
        let mut problem = None;
        for given in shown {
            if let Some(token) = &self.token
                && same(given, token)
            {
                return Ok((Access::local(), given.clone()));
            }
            // An access token is three parts with dots between.
            if let Some(oauth) = &self.oauth
                && given.matches('.').count() == 2
            {
                match oauth.verify(given).await {
                    Ok(access) => return Ok((access, given.clone())),
                    Err(refusal) => problem = Some(refusal),
                }
            }
        }
        Err(problem.unwrap_or_else(|| {
            let what = match (&self.token, &self.oauth) {
                (Some(_), None) => "the token of berth serve",
                (None, _) => "an access token",
                (Some(_), Some(_)) => "the token of berth serve or an access token",
            };
            (code::UNAUTHORIZED, format!("Send {what} as a bearer"))
        }))
    }

    /// The role a caller acts with: set-up may be closed to the network.
    pub fn role(&self, access: &Access) -> Role {
        match (self.setup, access.role) {
            (false, Role::Admin) => Role::Operate,
            (_, role) => role,
        }
    }

    pub fn allows(&self, access: &Access, op: &Op) -> Result<(), Refusal> {
        if self.role(access) >= op.role {
            return Ok(());
        }
        let message = if access.role >= op.role {
            format!(
                "{} is set-up, which this berth serve does not take over the network",
                op.name
            )
        } else {
            format!(
                "{} needs the role {}; this caller has {}",
                op.name,
                op.role.name(),
                access.role.name()
            )
        };
        Err((code::FORBIDDEN, message))
    }
}

/// The operation a request is, for a table lookup of the role it needs.
/// `Err` is a request below a host that no operation is known for.
pub fn operation(method: &str, path: &str, stream: bool) -> Result<Option<&'static Op>, ()> {
    let kind = if stream { Kind::Stream } else { Kind::Call };
    if let Some((op, _)) = ops::recognise(ops::SERVE, kind, method, path) {
        return Ok(Some(op));
    }
    let Some((_, rest)) = path
        .strip_prefix("/v1/hosts/")
        .and_then(|below| below.split_once('/'))
    else {
        return Ok(None);
    };
    match ops::recognise(ops::HOST, kind, method, &format!("/v1/{rest}")) {
        Some((op, _)) => Ok(Some(op)),
        None => Err(()),
    }
}

// ── Access tokens ───────────────────────────────────────────────────────────

/// Signatures with a key pair; a shared secret is not something an issuer publishes.
const ALGORITHMS: &[Algorithm] = &[
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::PS256,
    Algorithm::PS384,
    Algorithm::PS512,
    Algorithm::ES256,
    Algorithm::ES384,
    Algorithm::EdDSA,
];

/// The issuer's keys are asked for again at most this often.
const ASK_AGAIN: Duration = Duration::from_secs(30);

#[derive(Default)]
struct Keys {
    known: Vec<Jwk>,
    asked: Option<Instant>,
}

struct Oauth {
    issuer: String,
    audience: Option<String>,
    claim: String,
    /// Where the keys are, when the issuer's metadata is not to say.
    jwks: Option<String>,
    keys: tokio::sync::Mutex<Keys>,
}

fn refused(message: impl std::fmt::Display) -> Refusal {
    (
        code::UNAUTHORIZED,
        format!("The access token is not taken: {message}"),
    )
}

impl Oauth {
    async fn verify(&self, token: &str) -> Result<Access, Refusal> {
        let header = jsonwebtoken::decode_header(token).map_err(refused)?;
        if !ALGORITHMS.contains(&header.alg) {
            return Err(refused("its signature is of a kind berth does not take"));
        }
        let key = self.key(header.kid.as_deref()).await.map_err(refused)?;
        let key = DecodingKey::from_jwk(&key).map_err(refused)?;
        let mut validation = Validation::new(header.alg);
        validation.set_issuer(&[&self.issuer]);
        validation.set_required_spec_claims(&["exp", "iss"]);
        match &self.audience {
            Some(audience) => validation.set_audience(&[audience]),
            None => validation.validate_aud = false,
        }
        let claims = jsonwebtoken::decode::<Value>(token, &key, &validation)
            .map_err(refused)?
            .claims;
        let role = role(&claims, &self.claim).ok_or_else(|| {
            (
                code::FORBIDDEN,
                format!(
                    "The access token names no role of berth in its {}",
                    self.claim
                ),
            )
        })?;
        Ok(Access {
            role,
            subject: claims["sub"].as_str().map(str::to_owned),
        })
    }

    /// The issuer's key with this id; the only one when the token names none.
    async fn key(&self, id: Option<&str>) -> Result<Jwk, String> {
        let mut keys = self.keys.lock().await;
        let find = |known: &[Jwk]| match id {
            Some(id) => known
                .iter()
                .find(|key| key.common.key_id.as_deref() == Some(id))
                .cloned(),
            None if known.len() == 1 => known.first().cloned(),
            None => None,
        };
        if let Some(key) = find(&keys.known) {
            return Ok(key);
        }
        // A key that is not known may be new: issuers change theirs.
        if keys.asked.is_none_or(|asked| asked.elapsed() >= ASK_AGAIN) {
            keys.asked = Some(Instant::now());
            keys.known = self.fetch().await?;
        }
        find(&keys.known).ok_or_else(|| "it is signed with a key the issuer does not list".into())
    }

    async fn fetch(&self) -> Result<Vec<Jwk>, String> {
        let get = |url: String| async move {
            let body =
                tokio::time::timeout(Duration::from_secs(15), berth_core::server::fetch(&url))
                    .await
                    .map_err(|_| format!("{url}: no answer in time"))?
                    .map_err(|error| error.to_string())?;
            serde_json::from_slice::<Value>(&body).map_err(|error| format!("{url}: {error}"))
        };
        let address = match &self.jwks {
            Some(address) => address.clone(),
            None => {
                let mut found = Err("the issuer has no metadata".to_owned());
                for address in metadata_addresses(&self.issuer) {
                    found = get(address).await;
                    if found.is_ok() {
                        break;
                    }
                }
                let metadata = found?;
                if metadata["issuer"]
                    .as_str()
                    .map(|name| name.trim_end_matches('/'))
                    != Some(self.issuer.trim_end_matches('/'))
                {
                    return Err("the issuer's metadata is of another issuer".into());
                }
                metadata["jwks_uri"]
                    .as_str()
                    .ok_or("the issuer's metadata names no jwks_uri")?
                    .to_owned()
            }
        };
        let set = get(address).await?;
        // One key berth cannot read does not cost the others.
        Ok(set["keys"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|key| serde_json::from_value(key.clone()).ok())
            .collect())
    }
}

/// Where an issuer's metadata is: as OpenID Connect has it, then as RFC 8414.
fn metadata_addresses(issuer: &str) -> Vec<String> {
    let issuer = issuer.trim_end_matches('/');
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
    addresses
}

/// The highest role a claim names. The claim is a name, or a way into objects
/// with dots; its value a text of words or a list. In `scope` only `berth:ROLE`
/// counts, in a claim of its own the bare role does too.
pub fn role(claims: &Value, claim: &str) -> Option<Role> {
    let value = match claims.get(claim) {
        Some(value) => value,
        None => claim
            .split('.')
            .try_fold(claims, |inside, name| inside.get(name))?,
    };
    let words: Vec<&str> = match value {
        Value::String(text) => text
            .split(|ch: char| ch.is_whitespace() || ch == ',')
            .collect(),
        Value::Array(list) => list.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    };
    let bare = !matches!(claim, "scope" | "scp");
    words
        .into_iter()
        .filter_map(|word| match word.strip_prefix("berth:") {
            Some(name) => Role::named(name),
            None if bare => Role::named(word),
            None => None,
        })
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_claim_names_the_role() {
        let claims = json!({
            "scope": "openid berth:view berth:operate admin",
            "roles": ["user", "admin"],
            "realm_access": {"roles": ["berth:view"]},
            "https://example.com/role": "operate",
        });
        assert_eq!(role(&claims, "scope"), Some(Role::Operate));
        assert_eq!(role(&claims, "roles"), Some(Role::Admin));
        assert_eq!(role(&claims, "realm_access.roles"), Some(Role::View));
        assert_eq!(
            role(&claims, "https://example.com/role"),
            Some(Role::Operate)
        );
        assert_eq!(role(&claims, "groups"), None);
        assert_eq!(role(&json!({"scope": "openid"}), "scope"), None);
    }

    #[test]
    fn a_request_is_an_operation_with_a_role() {
        let name = |method, path, stream| operation(method, path, stream).unwrap().unwrap().name;
        assert_eq!(name("GET", "/v1/hosts", false), "hosts.list");
        assert_eq!(name("POST", "/v1/hosts/a/apply", false), "hosts.apply");
        assert_eq!(name("GET", "/v1/hosts/a/sessions", false), "sessions.list");
        assert_eq!(
            name("GET", "/v1/hosts/a/sessions/x", true),
            "sessions.attach"
        );
        assert_eq!(
            name("PUT", "/v1/secrets/registry%2Fmain", false),
            "secrets.set"
        );
        // The routes of berth serve below a host are its own, not the host's.
        assert_eq!(
            name("POST", "/v1/hosts/a/instance", false),
            "instances.create"
        );
        assert_eq!(
            name("POST", "/v1/hosts/a/instance/stop", false),
            "instances.power"
        );
        assert!(operation("GET", "/v1/portal", true).unwrap().is_none());
        // What nobody knows below a host is not let through on a guess.
        assert!(operation("POST", "/v1/hosts/a/nothing", false).is_err());

        let auth = Auth {
            token: None,
            setup: false,
            oauth: None,
        };
        let admin = Access::local();
        let apply = ops::find(ops::SERVE, "hosts.apply").unwrap();
        let run = ops::find(ops::HOST, "tasks.run").unwrap();
        assert_eq!(auth.role(&admin), Role::Operate);
        assert!(auth.allows(&admin, run).is_ok());
        assert!(auth.allows(&admin, apply).unwrap_err().1.contains("set-up"));
        let viewer = Access {
            role: Role::View,
            subject: None,
        };
        assert!(auth.allows(&viewer, run).unwrap_err().1.contains("operate"));
        assert_eq!(
            metadata_addresses("https://id.example/realms/x")[1],
            "https://id.example/.well-known/oauth-authorization-server/realms/x"
        );
    }
}
