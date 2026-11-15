//! The tailnet hosts join: Tailscale's own service, or a Headscale server.
//!
//! Both take the same `tailscale` client; a Headscale host is pointed at its
//! server with `--login-server`. A host joins with a one-use key berth makes
//! through the control server's API, at the moment the host needs one: when
//! an apply finds it off the tailnet, or when a seed or installer is written.
//!
//! - `tailnet_login_server`: empty for Tailscale, the address of a Headscale
//!   server otherwise
//! - `tailnet_user`: the Headscale user a new host belongs to
//! - `tailnet_tags`: tags for new hosts, comma separated
//! - the secret `tailnet_api_key`: a Tailscale API access token, a Tailscale
//!   OAuth client as `CLIENT_ID:CLIENT_SECRET`, or a Headscale API key
//!
//! The same key looks after the hosts' devices: each apply records the
//! device's id in the store, destroying a host's machine deletes its device,
//! and the next apply after a rename with the hostname renames it.

use crate::{
    Result,
    paths::Paths,
    profiles::Place,
    registry::Store,
    store::{self, kind},
};
use berth_api::TailnetDevice;
use berth_api::profile::{self, TAILNET_API_KEY};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

/// How long a key lives that goes into a seed or an installer: long enough to
/// write the installer and let the first boot enroll.
pub const SEED_LIFETIME: u64 = 24 * 60 * 60;
/// How long a key lives that an apply uses straight away.
pub const APPLY_LIFETIME: u64 = 60 * 60;

const TAILSCALE_API: &str = "https://api.tailscale.com";

/// The role's variables of joining the tailnet, which berth sets and a
/// profile cannot.
pub const ROLE_VARS: &[&str] = &[
    "devbox_tailscale_auth_key",
    "devbox_tailscale_login_server",
    "devbox_tailscale_key_command",
    "devbox_tailnet_problem",
];

/// The settings of the tailnet, in the order the Settings tab lists them.
pub const SETTINGS: &[&str] = &["tailnet_login_server", "tailnet_user", "tailnet_tags"];

/// How to set the secret, for messages that ask for it.
fn set_the_secret() -> String {
    format!(
        "set the secret {TAILNET_API_KEY} with berth secret set {TAILNET_API_KEY}, or on the Secrets tab of the home screen"
    )
}

/// What berth logs in to the control server's API with.
#[derive(Debug, Clone, PartialEq)]
enum Credential {
    /// A Tailscale API access token or a Headscale API key.
    Token(String),
    /// A Tailscale OAuth client, which is exchanged for an access token of an
    /// hour whenever a key is made, as its own does not expire.
    Client { id: String, secret: String },
}

impl Credential {
    /// The secret as it is stored: an OAuth client is its id and secret with
    /// a colon between, which neither API keys nor client ids contain.
    fn parse(text: &str) -> Option<Credential> {
        let text = text.trim();
        if text.is_empty() || text.contains(['"', '\\', '\n', '\r', ' ']) {
            return None;
        }
        Some(match text.split_once(':') {
            Some((id, secret)) if !id.is_empty() && !secret.is_empty() => Credential::Client {
                id: id.to_owned(),
                secret: secret.to_owned(),
            },
            Some(_) => return None,
            None => Credential::Token(text.to_owned()),
        })
    }
}

/// The control server berth creates keys at, and what it needs for that.
#[derive(Debug, Clone, PartialEq)]
pub struct Control {
    /// The Headscale server; None for Tailscale.
    pub headscale: Option<String>,
    pub user: String,
    pub tags: Vec<String>,
    /// The secret, or what is wrong with it.
    credential: std::result::Result<Credential, String>,
    /// Where Tailscale's API is, other than in tests.
    tailscale_api: String,
}

fn tags(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .map(|tag| match tag.starts_with("tag:") {
            true => tag.to_owned(),
            false => format!("tag:{tag}"),
        })
        .collect()
}

/// A login server as the client takes it: no trailing slash.
fn server_address(text: &str) -> Option<String> {
    let text = text.trim().trim_end_matches('/');
    (!text.is_empty()).then(|| text.to_owned())
}

impl Control {
    /// The control server of berth's settings, with its credential.
    pub fn configured(paths: &Paths) -> Control {
        let setting = |key: &str| paths.setting(key).unwrap_or_default();
        let secret = Store::new(paths)
            .and_then(|store| store.secret(TAILNET_API_KEY))
            .ok()
            .flatten()
            .filter(|secret| !secret.is_empty());
        let credential = match secret {
            None => Err(format!("berth has no API key of the tailnet: {}", set_the_secret())),
            Some(secret) => Credential::parse(&secret).ok_or_else(|| {
                format!(
                    "The secret {TAILNET_API_KEY} is neither an API key nor an OAuth client as CLIENT_ID:CLIENT_SECRET: {}",
                    set_the_secret()
                )
            }),
        };
        Control {
            headscale: server_address(&setting("tailnet_login_server")),
            user: setting("tailnet_user"),
            tags: tags(&setting("tailnet_tags")),
            credential,
            tailscale_api: TAILSCALE_API.into(),
        }
    }

    /// What to call the control server in a message.
    pub fn name(&self) -> String {
        match &self.headscale {
            Some(url) => format!("Headscale at {url}"),
            None => "Tailscale".into(),
        }
    }

    /// Why berth cannot create keys at this control server, if it cannot.
    /// Only what berth knows without asking the server.
    pub fn problem(&self) -> Option<String> {
        if crate::simulate::active().is_some() {
            return None;
        }
        let credential = match &self.credential {
            Ok(credential) => credential,
            Err(why) => return Some(why.clone()),
        };
        match (&self.headscale, credential) {
            (Some(_), Credential::Client { .. }) => Some(format!(
                "Headscale has no OAuth clients: the secret {TAILNET_API_KEY} is one of its API keys (headscale apikeys create)"
            )),
            (Some(_), _) if self.user.is_empty() && self.tags.is_empty() => Some(
                "Headscale needs a user or tags for a new host's key: berth config set tailnet_user NAME, or berth config set tailnet_tags TAG".into(),
            ),
            (None, Credential::Client { .. }) if self.tags.is_empty() => Some(
                "Keys made by a Tailscale OAuth client carry tags: berth config set tailnet_tags with the client's tags, such as tag:berth".into(),
            ),
            _ => None,
        }
    }

    /// Whether berth has what it needs to create keys.
    pub fn can_create(&self) -> bool {
        self.problem().is_none()
    }

    /// A new pre-approved key that joins one host, once, within `lifetime` seconds.
    pub fn create_key(&self, description: &str, lifetime: u64) -> Result<String> {
        // A simulated server's keys join nothing.
        if crate::simulate::active().is_some() {
            return Ok(format!(
                "tskey-auth-simulated-{}",
                crate::provision::new_token()?
            ));
        }
        if let Some(problem) = self.problem() {
            return Err(problem.into());
        }
        let login = self.login()?;
        match &self.headscale {
            Some(url) => self.headscale_key(url, &login, lifetime),
            None => self.tailscale_key(&login, description, lifetime),
        }
    }

    /// How curl logs in to the API: a line of its configuration. An OAuth
    /// client is exchanged for an access token first.
    fn login(&self) -> Result<String> {
        let credential = self.credential.as_ref().map_err(String::clone)?;
        match (&self.headscale, credential) {
            (Some(_), Credential::Token(token)) => {
                Ok(format!("header = \"Authorization: Bearer {token}\""))
            }
            // The access token is the user name of HTTP basic authentication.
            (None, Credential::Token(token)) => Ok(format!("user = \"{token}:\"")),
            (None, Credential::Client { id, secret }) => {
                let token = self.exchange(id, secret)?;
                Ok(format!("header = \"Authorization: Bearer {token}\""))
            }
            (Some(_), Credential::Client { .. }) => Err("Headscale has no OAuth clients".into()),
        }
    }

    /// Why berth cannot change devices at this control server, if it cannot:
    /// that needs the key alone, not the user or tags of new hosts.
    pub fn device_problem(&self) -> Option<String> {
        match (&self.headscale, &self.credential) {
            (_, Err(why)) => Some(why.clone()),
            (Some(_), Ok(Credential::Client { .. })) => Some(format!(
                "Headscale has no OAuth clients: the secret {TAILNET_API_KEY} is one of its API keys (headscale apikeys create)"
            )),
            _ => None,
        }
    }

    /// What the key needs to change devices, for a message after a refusal.
    fn permission(&self) -> String {
        match (&self.headscale, &self.credential) {
            (Some(_), _) => "Every Headscale API key may do this, so check that the key has not expired: headscale apikeys list".into(),
            (None, Ok(Credential::Client { .. })) => format!(
                "The OAuth client in {TAILNET_API_KEY} needs the devices:core scope, besides auth_keys"
            ),
            (None, _) => format!(
                "An API access token in {TAILNET_API_KEY} has the permissions of the user who made it, who has to be allowed to manage the tailnet's devices. An OAuth client with the scopes auth_keys and devices:core needs no user"
            ),
        }
    }

    /// Remove the device with this id from the tailnet: `Ok(false)` when
    /// there is no such device.
    pub fn delete_device(&self, id: &str) -> Result<bool> {
        let id = device_id(id)?;
        let url = match &self.headscale {
            Some(url) => format!("{url}/api/v1/node/{id}"),
            None => format!("{}/api/v2/device/{id}", self.tailscale_api),
        };
        let (status, reply) = request("DELETE", &url, &self.login()?, None)?;
        self.done(status, &reply, "remove the device")
    }

    /// Give the device with this id another name on the tailnet: `Ok(false)`
    /// when there is no such device.
    pub fn rename_device(&self, id: &str, name: &str) -> Result<bool> {
        let id = device_id(id)?;
        if !device_name(name) {
            return Err(format!("{name} is no name for a device on the tailnet").into());
        }
        let login = self.login()?;
        let (status, reply) = match &self.headscale {
            Some(url) => request(
                "POST",
                &format!("{url}/api/v1/node/{id}/rename/{name}"),
                &login,
                Some(Body::Json(json!({}))),
            )?,
            None => request(
                "POST",
                &format!("{}/api/v2/device/{id}/name", self.tailscale_api),
                &login,
                Some(Body::Json(json!({"name": name}))),
            )?,
        };
        self.done(status, &reply, "rename the device")
    }

    /// Whether a change of a device was done, was of no device, or was refused.
    fn done(&self, status: u16, reply: &Value, what: &str) -> Result<bool> {
        let not_found = status == 404
            || reply["message"]
                .as_str()
                .is_some_and(|message| message.to_ascii_lowercase().contains("not found"));
        match status {
            200..=299 => Ok(true),
            _ if not_found => Ok(false),
            401 | 403 => Err(format!(
                "{}. {}",
                refused_to(&self.name(), what, status, reply),
                self.permission()
            )
            .into()),
            _ => Err(refused_to(&self.name(), what, status, reply)),
        }
    }

    /// The devices on the tailnet, as berth compares them with a host.
    pub fn devices(&self) -> Result<Vec<TailnetDevice>> {
        let login = self.login()?;
        let (url, list, field) = match &self.headscale {
            Some(url) => (format!("{url}/api/v1/node"), "nodes", "id"),
            None => (
                format!("{}/api/v2/tailnet/-/devices", self.tailscale_api),
                "devices",
                "nodeId",
            ),
        };
        let (status, reply) = call(&url, &login, None)?;
        if status != 200 {
            let refused = refused_to(&self.name(), "list the devices", status, &reply);
            return Err(match status {
                401 | 403 => format!("{refused}. {}", self.permission()).into(),
                _ => refused,
            });
        }
        let text = |value: &Value| value.as_str().unwrap_or_default().to_owned();
        let found = reply[list].as_array().cloned().unwrap_or_default();
        Ok(found
            .iter()
            .map(|device| {
                let (dns_name, host_name, addresses) = match self.headscale {
                    // Headscale's name for the node on the tailnet is its given name.
                    Some(_) => (
                        text(&device["givenName"]),
                        text(&device["name"]),
                        &device["ipAddresses"],
                    ),
                    None => (
                        text(&device["name"]).trim_end_matches('.').to_owned(),
                        text(&device["hostname"]),
                        &device["addresses"],
                    ),
                };
                let id = match text(&device[field]) {
                    id if id.is_empty() => text(&device["id"]),
                    id => id,
                };
                TailnetDevice {
                    id,
                    dns_name,
                    host_name,
                    addresses: addresses
                        .as_array()
                        .map(|all| all.iter().map(text).collect())
                        .unwrap_or_default(),
                    tailnet: String::new(),
                }
            })
            .collect())
    }

    /// An access token for an OAuth client, by the client credentials grant.
    fn exchange(&self, id: &str, secret: &str) -> Result<String> {
        let form = format!(
            "grant_type=client_credentials&client_id={}&client_secret={}",
            query(id),
            query(secret)
        );
        let url = format!("{}/api/v2/oauth/token", self.tailscale_api);
        let (status, reply) = call(&url, "", Some(Body::Form(form)))?;
        match reply["access_token"].as_str() {
            Some(token)
                if status == 200
                    && !token.is_empty()
                    && !token.contains(['"', '\\', '\n', '\r']) =>
            {
                Ok(token.to_owned())
            }
            _ => {
                let reason = reply["error_description"]
                    .as_str()
                    .or(reply["error"].as_str())
                    .map(str::to_owned);
                Err(match reason {
                    Some(reason) => format!(
                        "Tailscale refused the OAuth client of {TAILNET_API_KEY} (HTTP {status}): {reason}"
                    )
                    .into(),
                    None => refused("Tailscale", status, &reply),
                })
            }
        }
    }

    fn tailscale_key(&self, login: &str, description: &str, lifetime: u64) -> Result<String> {
        let mut create = json!({"reusable": false, "ephemeral": false, "preauthorized": true});
        if !self.tags.is_empty() {
            create["tags"] = json!(self.tags);
        }
        // Tailscale takes letters, digits, spaces and hyphens, up to 50.
        let description: String = description
            .chars()
            .map(|ch| match ch.is_ascii_alphanumeric() || ch == ' ' {
                true => ch,
                false => '-',
            })
            .take(50)
            .collect();
        let request = json!({
            "capabilities": {"devices": {"create": create}},
            "expirySeconds": lifetime,
            "description": description,
        });
        let url = format!("{}/api/v2/tailnet/-/keys", self.tailscale_api);
        let (status, reply) = call(&url, login, Some(Body::Json(request)))?;
        match reply["key"].as_str() {
            Some(key) if status == 200 && !key.is_empty() => Ok(key.to_owned()),
            _ => Err(refused("Tailscale", status, &reply)),
        }
    }

    fn headscale_key(&self, url: &str, login: &str, lifetime: u64) -> Result<String> {
        let mut request = json!({
            "reusable": false,
            "ephemeral": false,
            "expiration": berth_api::rfc3339(berth_api::unix_now() + lifetime),
        });
        if !self.tags.is_empty() {
            request["aclTags"] = json!(self.tags);
        }
        match self.user.as_str() {
            "" => {}
            // Headscale's API names the user by its number.
            user if user.bytes().all(|byte| byte.is_ascii_digit()) => {
                request["user"] = json!(user);
            }
            user => {
                let lookup = format!("{url}/api/v1/user?name={}", query(user));
                let (status, reply) = call(&lookup, login, None)?;
                if status != 200 {
                    return Err(refused("Headscale", status, &reply));
                }
                let id = reply["users"]
                    .as_array()
                    .and_then(|users| users.iter().find(|found| found["name"] == user))
                    .and_then(|found| found["id"].as_str().map(str::to_owned))
                    .ok_or_else(|| format!("Headscale has no user {user}"))?;
                request["user"] = json!(id);
            }
        }
        let (status, reply) = call(
            &format!("{url}/api/v1/preauthkey"),
            login,
            Some(Body::Json(request)),
        )?;
        match reply["preAuthKey"]["key"].as_str() {
            Some(key) if status == 200 && !key.is_empty() => Ok(key.to_owned()),
            _ => Err(refused("Headscale", status, &reply)),
        }
    }
}

fn refused(who: &str, status: u16, reply: &Value) -> Box<dyn std::error::Error> {
    refused_to(who, "create a key", status, reply)
}

fn refused_to(who: &str, what: &str, status: u16, reply: &Value) -> Box<dyn std::error::Error> {
    let reason = match (reply["message"].as_str(), reply.as_str()) {
        (Some(message), _) => message.to_owned(),
        (None, Some(text)) if !text.trim().is_empty() => text.trim().chars().take(200).collect(),
        _ => "no reason given".into(),
    };
    format!("{who} refused to {what} (HTTP {status}): {reason}").into()
}

/// Whether a name can be a device's on the tailnet, and a part of a path: a
/// DNS label of lower-case letters, digits and `-`.
fn device_name(name: &str) -> bool {
    (1..=63).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

/// A device's id as it goes into a path: letters and digits only.
fn device_id(id: &str) -> Result<&str> {
    match !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        true => Ok(id),
        false => Err(format!("{id:?} is not the id of a device on the tailnet").into()),
    }
}

fn query(text: &str) -> String {
    text.bytes()
        .map(
            |byte| match byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
                true => (byte as char).to_string(),
                false => format!("%{byte:02X}"),
            },
        )
        .collect()
}

/// What a request sends.
enum Body {
    Json(Value),
    Form(String),
}

/// One request with curl: the status, and the answer as JSON or as text. The
/// login is a line of curl's configuration, given on its standard input, and
/// the body is a private file, so neither appears in the process list.
fn call(url: &str, login: &str, body: Option<Body>) -> Result<(u16, Value)> {
    let method = match body {
        Some(_) => "POST",
        None => "GET",
    };
    request(method, url, login, body)
}

/// [`call`] with another method than the one its body implies.
fn request(method: &str, url: &str, login: &str, body: Option<Body>) -> Result<(u16, Value)> {
    if crate::simulate::active().is_some() {
        return Err("A simulated server reaches no tailnet".into());
    }
    let mut file = tempfile::NamedTempFile::new()?;
    let mut command = Command::new("curl");
    command.args([
        "--silent",
        "--show-error",
        "--max-time",
        "20",
        "--config",
        "-",
        "--write-out",
        "\n%{http_code}",
    ]);
    if let Some(body) = body {
        let kind = match body {
            Body::Json(value) => {
                serde_json::to_writer(&mut file, &value)?;
                "application/json"
            }
            Body::Form(text) => {
                file.write_all(text.as_bytes())?;
                "application/x-www-form-urlencoded"
            }
        };
        file.flush()?;
        command
            .arg("--header")
            .arg(format!("Content-Type: {kind}"))
            .arg("--data-binary")
            .arg(format!("@{}", file.path().display()));
    }
    command.args(["--request", method]);
    let mut child = command
        .arg(url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("curl: {error}"))?;
    child
        .stdin
        .take()
        .ok_or("curl took no input")?
        .write_all(format!("{login}\n").as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(format!("{url}: {}", String::from_utf8_lossy(&output.stderr).trim()).into());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let (answer, status) = text.rsplit_once('\n').unwrap_or(("", &text));
    let status = status.trim().parse().unwrap_or(0);
    let answer = serde_json::from_str(answer).unwrap_or_else(|_| json!(answer));
    Ok((status, answer))
}

/// The login server hosts are pointed at: berth's setting. Empty for
/// Tailscale's service.
pub fn login_server(paths: &Paths) -> String {
    server_address(&paths.setting("tailnet_login_server").unwrap_or_default()).unwrap_or_default()
}

/// What a key is described as at the control server.
fn description(host: &str) -> String {
    match host {
        "" => "berth".into(),
        host => format!("berth {host}"),
    }
}

/// Why a host of this profile cannot join the tailnet, in words that say
/// what to do, or `None` when it can or does not join at all. It is asked
/// before anything runs, so a host is not set up halfway.
pub fn problem(paths: &Paths, config: &Value, host: &str) -> Option<String> {
    if !profile::joins_tailnet(config) {
        return None;
    }
    let control = Control::configured(paths);
    let why = control.problem()?;
    let host = match host {
        "" => "The host".to_owned(),
        host => host.to_owned(),
    };
    Some(format!(
        "{host} joins the tailnet at {}, and berth cannot make its key. {why}. Or switch Tailscale off in its profile, and berth reaches it over SSH at its address",
        control.name()
    ))
}

/// A key for a host to join with, from the control server of berth's settings.
pub fn create_key(paths: &Paths, host: &str, lifetime: u64) -> Result<String> {
    Control::configured(paths).create_key(&description(host), lifetime)
}

/// The key `berth tailnet-key WHAT` prints. An apply runs that on this
/// machine for a host that is not on the tailnet yet, with the name or the
/// path [`apply_vars`] was given, so it finds the host in the same place.
pub fn key_for(paths: &Paths, what: &str) -> Result<String> {
    let host = crate::host::Host::load(paths, what)?;
    create_key(paths, &host.place.title(), APPLY_LIFETIME)
}

/// What a seed or an installer is given to join the tailnet with: a private
/// file for the seed builder, which names it in `BERTH_TAILNET_FILE`, and
/// until when its key works.
pub struct SeedKey {
    pub file: tempfile::NamedTempFile,
    /// Seconds since 1970.
    pub until: u64,
}

impl SeedKey {
    /// What to tell whoever writes the seed or installer.
    pub fn note(&self) -> String {
        until_note(self.until)
    }

    /// Give the seed builder the key.
    pub fn pass(&self, command: &mut Command) {
        command.env("BERTH_TAILNET_FILE", self.file.path());
    }
}

/// Until when a seed or installer can join the tailnet, and what happens after.
pub fn until_note(until: u64) -> String {
    let time = berth_api::rfc3339(until);
    let shown = time
        .get(..16)
        .map(|minutes| minutes.replace('T', " "))
        .unwrap_or(time);
    format!(
        "Its key joins the tailnet until {shown} UTC, 24 hours from now. A machine first started later is set up without the tailnet; applying it from berth then joins it"
    )
}

/// A new key for a seed or an installer of a host that joins the tailnet,
/// living for [`SEED_LIFETIME`], with the login server. Keys are made only
/// here, when a seed is written, and never kept in the profile.
pub fn seed_key(paths: &Paths, config: &Value, host: &str) -> Result<Option<SeedKey>> {
    if !profile::joins_tailnet(config) {
        return Ok(None);
    }
    if let Some(problem) = problem(paths, config, host) {
        return Err(problem.into());
    }
    let key = create_key(paths, host, SEED_LIFETIME)?;
    let until = berth_api::unix_now() + SEED_LIFETIME;
    let mut file = tempfile::NamedTempFile::new()?;
    let given = json!({"key": key, "login_server": login_server(paths)});
    serde_json::to_writer(&mut file, &given)?;
    file.flush()?;
    Ok(Some(SeedKey { file, until }))
}

/// A private copy of a profile for the seed builder, which reads a file.
pub fn profile_file(config: &Value) -> Result<tempfile::NamedTempFile> {
    let mut file = tempfile::NamedTempFile::new()?;
    serde_json::to_writer(&mut file, config)?;
    file.flush()?;
    Ok(file)
}

/// What an apply adds to the role's variables for a host that joins the
/// tailnet: the login server, and the command the role runs on this machine
/// for a key, only when the host is not on the tailnet yet. When berth cannot
/// make keys the role is given why instead, and stops before it changes
/// anything on a host that is not on the tailnet. `berth` is this program.
pub fn apply_vars(paths: &Paths, config: &Value, host: &str, berth: &Path) -> Value {
    let mut vars = json!({});
    if !profile::joins_tailnet(config) {
        return vars;
    }
    let server = login_server(paths);
    if !server.is_empty() {
        vars["devbox_tailscale_login_server"] = json!(server);
    }
    let name = crate::profiles::Place::resolve(paths, host)
        .map(|place| place.title())
        .unwrap_or_else(|_| host.to_owned());
    match problem(paths, config, &name) {
        Some(problem) => vars["devbox_tailnet_problem"] = json!(problem),
        None => vars["devbox_tailscale_key_command"] = json!([berth, "tailnet-key", host]),
    }
    vars
}

/// What berth keeps of a host's device on the tailnet, in the store with
/// the host: what the last apply found, and the name a rename with the
/// hostname gave it, which the next apply gives the device.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Kept {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<TailnetDevice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rename: Option<String>,
}

impl Kept {
    /// What the store keeps of the host `name` in `scope`; nothing when it has nothing.
    pub fn read(store: &store::Store, scope: &str, name: &str) -> Result<Kept> {
        Ok(match store.read_json(kind::TAILNET, scope, name)? {
            Some(value) => serde_json::from_value(value)
                .map_err(|error| format!("The record of {name} on the tailnet: {error}"))?,
            None => Kept::default(),
        })
    }

    /// What the store keeps of the host at `place`.
    pub fn of(paths: &Paths, place: &Place) -> Result<Kept> {
        let (scope, name) = place.side();
        Kept::read(store::shared(paths)?.as_ref(), &scope, &name)
    }

    fn save(&self, paths: &Paths, place: &Place) -> Result<()> {
        let (scope, name) = place.side();
        store::shared(paths)?.write_json(
            kind::TAILNET,
            &scope,
            &name,
            &serde_json::to_value(self)?,
        )?;
        Ok(())
    }
}

/// A device for people: its MagicDNS name, or its id.
fn label(device: &TailnetDevice) -> String {
    match device.dns_name.as_str() {
        "" => device.id.clone(),
        name => name.to_owned(),
    }
}

/// The devices of `devices` that can be the host of this profile: those at
/// its address, or else those with its hostname, as its device's name or as
/// the machine's own.
fn matching<'a>(
    devices: &'a [TailnetDevice],
    config: &Value,
    name: &str,
) -> Vec<&'a TailnetDevice> {
    let same = |one: &str, other: &str| {
        let (one, other) = (one.trim_end_matches('.'), other.trim_end_matches('.'));
        !one.is_empty() && one.eq_ignore_ascii_case(other)
    };
    let address = profile::Access::of(config).address;
    let at: Vec<_> = devices
        .iter()
        .filter(|device| {
            // Headscale lists a node's name without the tailnet's domain.
            let short = !device.dns_name.contains('.')
                && address.parse::<std::net::IpAddr>().is_err()
                && address
                    .split_once('.')
                    .is_some_and(|(first, _)| same(first, &device.dns_name));
            short
                || same(&device.dns_name, &address)
                || device.addresses.iter().any(|known| same(known, &address))
        })
        .collect();
    if !at.is_empty() {
        return at;
    }
    let hostname = config["hostname"]
        .as_str()
        .filter(|hostname| !hostname.is_empty())
        .unwrap_or(name);
    devices
        .iter()
        .filter(|device| same(device.name(), hostname) || same(&device.host_name, hostname))
        .collect()
}

/// The device of a host berth has no record of, when exactly one device on
/// the tailnet can be it; otherwise the line that says why berth does nothing.
fn look_up(
    control: &Control,
    config: &Value,
    name: &str,
) -> std::result::Result<TailnetDevice, String> {
    let devices = control.devices().map_err(|error| {
        format!("! berth has no record of {name}'s device on the tailnet, and could not look for it: {error}. Remove it there if it is there")
    })?;
    match matching(&devices, config, name).as_slice() {
        [device] => Ok((*device).clone()),
        [] => Err(format!(
            "berth has no record of {name}'s device on the tailnet, and none there has its address or hostname: nothing is removed"
        )),
        several => Err(format!(
            "! berth has no record of {name}'s device on the tailnet, and {} devices there could be it: {}. It removes none of them: remove the right one there",
            several.len(),
            several
                .iter()
                .map(|device| label(device))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// After the machine of the host at `place` was destroyed: remove its
/// device from the tailnet, the one the last apply recorded or the one
/// that alone has the host's address or hostname. Says what it did, or why
/// it did not; it never fails, as the machine is gone either way. `None`
/// for a host that was never on the tailnet.
pub fn remove_device(paths: &Paths, place: &Place, config: &Value) -> Option<String> {
    remove_device_at(&Control::configured(paths), paths, place, config)
}

fn remove_device_at(
    control: &Control,
    paths: &Paths,
    place: &Place,
    config: &Value,
) -> Option<String> {
    let name = place.title();
    let kept = match Kept::of(paths, place) {
        Ok(kept) => kept,
        Err(error) => return Some(format!("! {name}'s device on the tailnet stays: {error}")),
    };
    if kept.device.is_none() && !profile::joins_tailnet(config) {
        return None;
    }
    let at = control.name();
    if let Some(why) = control.device_problem() {
        return Some(format!(
            "! {name}'s device on the tailnet at {at} stays, as berth cannot remove it: {why}. Remove it there"
        ));
    }
    let device = match kept.device {
        Some(device) => device,
        None => match look_up(control, config, &name) {
            Ok(device) => device,
            Err(line) => return Some(line),
        },
    };
    let shown = label(&device);
    Some(match control.delete_device(&device.id) {
        Ok(true) => format!("✓ Removed {name}'s device {shown} from the tailnet at {at}"),
        Ok(false) => format!("{name}'s device {shown} is not on the tailnet at {at} any more"),
        Err(error) => {
            format!("! {name}'s device {shown} stays on the tailnet: {error}. Remove it there")
        }
    })
}

/// A MagicDNS name with its first label replaced.
fn renamed(dns_name: &str, to: &str) -> String {
    match dns_name.split_once('.') {
        Some((_, rest)) => format!("{to}.{rest}"),
        None => to.to_owned(),
    }
}

/// After an apply of `host` that joins the tailnet: keep the device it
/// reported, and give the device the name a rename with the hostname asked
/// for, now that the machine has that hostname. A profile that reaches the
/// host by the device's old name is given the new one. Says what it did, a
/// line each.
pub fn after_apply(
    paths: &Paths,
    host: &crate::host::Host,
    reported: Option<TailnetDevice>,
) -> Result<Vec<String>> {
    after_apply_at(&Control::configured(paths), paths, host, reported)
}

fn after_apply_at(
    control: &Control,
    paths: &Paths,
    host: &crate::host::Host,
    reported: Option<TailnetDevice>,
) -> Result<Vec<String>> {
    let name = &host.name;
    let mut kept = Kept::of(paths, &host.place)?;
    let Some(mut device) = reported else {
        return Ok(match &kept.rename {
            Some(to) => vec![format!(
                "! {name} did not say which device it is on the tailnet, so its device is named {to} at a later apply"
            )],
            None => vec![],
        });
    };
    let mut lines = vec![];
    if let Some(to) = kept.rename.clone() {
        let old = label(&device);
        if device.name() == to {
            kept.rename = None;
        } else if let Some(why) = control.device_problem() {
            lines.push(format!(
                "! {name}'s device on the tailnet is still {old}, as berth cannot rename it: {why}. berth renames it at the next apply"
            ));
        } else {
            match control.rename_device(&device.id, &to) {
                Ok(true) => {
                    let was = device.dns_name.clone();
                    device.dns_name = renamed(&was, &to);
                    kept.rename = None;
                    lines.push(format!(
                        "✓ Renamed {name}'s device on the tailnet from {old} to {}",
                        label(&device)
                    ));
                    if let Some(line) = follow(paths, host, &was, &device.dns_name)? {
                        lines.push(line);
                    }
                }
                Ok(false) => lines.push(format!(
                    "! {name}'s device {old} is not on the tailnet at {} any more, so berth renames it at the next apply",
                    control.name()
                )),
                Err(error) => lines.push(format!(
                    "! {name}'s device on the tailnet is still {old}: {error}. berth renames it at the next apply"
                )),
            }
        }
    }
    kept.device = Some(device);
    kept.save(paths, &host.place)?;
    Ok(lines)
}

/// Reach the host by the device's new name when its profile named the old
/// one, the whole MagicDNS name or its first label.
fn follow(paths: &Paths, host: &crate::host::Host, was: &str, now: &str) -> Result<Option<String>> {
    let Some(mut config) = host.place.load(paths)? else {
        return Ok(None);
    };
    let address = profile::Access::of(&config).address;
    let short = |name: &str| name.split('.').next().unwrap_or_default().to_owned();
    let new = if !was.is_empty() && address.trim_end_matches('.').eq_ignore_ascii_case(was) {
        now.to_owned()
    } else if !was.is_empty() && address.eq_ignore_ascii_case(&short(was)) {
        short(now)
    } else {
        return Ok(None);
    };
    config["access"]["address"] = json!(new);
    host.place.save(paths, &config)?;
    Ok(Some(format!("{} is reached at {new} now", host.name)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Read},
        net::TcpListener,
        sync::mpsc,
    };

    /// A request as the fake server saw it.
    #[derive(Debug)]
    struct Seen {
        line: String,
        authorization: String,
        kind: String,
        text: String,
        body: Value,
    }

    /// An HTTP server on a free port that gives each answer once, in order,
    /// and tells what it was asked.
    fn fake(answers: Vec<(u16, Value)>) -> (String, mpsc::Receiver<Seen>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (sender, seen) = mpsc::channel();
        std::thread::spawn(move || {
            for (status, answer) in answers {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let (mut length, mut authorization, mut kind) = (0, String::new(), String::new());
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap();
                    let header = header.trim_end();
                    if header.is_empty() {
                        break;
                    }
                    let (name, value) = header.split_once(':').unwrap();
                    match name.to_ascii_lowercase().as_str() {
                        "content-length" => length = value.trim().parse().unwrap(),
                        "authorization" => authorization = value.trim().to_owned(),
                        "content-type" => kind = value.trim().to_owned(),
                        _ => {}
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                sender
                    .send(Seen {
                        line: line.trim_end().to_owned(),
                        authorization,
                        kind,
                        text: String::from_utf8_lossy(&body).into_owned(),
                        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
                    })
                    .unwrap();
                let text = answer.to_string();
                let mut stream = stream;
                write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                    text.len()
                )
                .unwrap();
            }
        });
        (url, seen)
    }

    fn control(headscale: Option<&str>, tailscale_api: &str) -> Control {
        Control {
            headscale: headscale.map(str::to_owned),
            user: String::new(),
            tags: vec![],
            credential: Ok(Credential::Token("secret-token".into())),
            tailscale_api: tailscale_api.into(),
        }
    }

    #[test]
    fn tailscale_makes_a_one_use_key() {
        let (url, seen) = fake(vec![(200, json!({"key": "tskey-auth-new"}))]);
        let mut control = control(None, &url);
        control.tags = tags("berth, tag:agents");
        let key = control.create_key("berth studio.lan", 3600).unwrap();
        assert_eq!(key, "tskey-auth-new");
        let asked = seen.recv().unwrap();
        assert_eq!(asked.line, "POST /api/v2/tailnet/-/keys HTTP/1.1");
        // The access token is the user name of HTTP basic authentication.
        assert_eq!(
            asked.authorization,
            format!("Basic {}", crate::registry::base64(b"secret-token:"))
        );
        let create = &asked.body["capabilities"]["devices"]["create"];
        assert_eq!(create["reusable"], false);
        assert_eq!(create["ephemeral"], false);
        assert_eq!(create["preauthorized"], true);
        assert_eq!(create["tags"], json!(["tag:berth", "tag:agents"]));
        assert_eq!(asked.body["expirySeconds"], 3600);
        assert_eq!(asked.body["description"], "berth studio-lan");
    }

    #[test]
    fn tailscale_says_why_it_refused() {
        let (url, _seen) = fake(vec![(
            403,
            json!({"message": "calling actor does not have enough permissions"}),
        )]);
        let said = control(None, &url).create_key("berth", 60).unwrap_err();
        assert!(said.to_string().contains("HTTP 403"), "{said}");
        assert!(said.to_string().contains("enough permissions"), "{said}");
    }

    #[test]
    fn an_oauth_client_is_exchanged_for_a_token_of_its_own_each_time() {
        let (url, seen) = fake(vec![
            (
                200,
                json!({"access_token": "tskey-access-FAKE", "token_type": "Bearer", "expires_in": 3600}),
            ),
            (200, json!({"key": "tskey-auth-by-client"})),
        ]);
        let mut control = control(None, &url);
        control.credential =
            Ok(Credential::parse("kFAKE1CNTRL:tskey-client-kFAKE1CNTRL-0000").unwrap());
        // Its keys carry tags, and berth says so before it asks.
        let said = control.create_key("berth studio", 60).unwrap_err();
        assert!(said.to_string().contains("tailnet_tags"), "{said}");
        control.tags = tags("berth");
        assert_eq!(
            control.create_key("berth studio", 60).unwrap(),
            "tskey-auth-by-client"
        );
        let exchange = seen.recv().unwrap();
        assert_eq!(exchange.line, "POST /api/v2/oauth/token HTTP/1.1");
        assert_eq!(exchange.kind, "application/x-www-form-urlencoded");
        assert_eq!(
            exchange.text,
            "grant_type=client_credentials&client_id=kFAKE1CNTRL&client_secret=tskey-client-kFAKE1CNTRL-0000"
        );
        assert_eq!(exchange.authorization, "");
        let create = seen.recv().unwrap();
        assert_eq!(create.line, "POST /api/v2/tailnet/-/keys HTTP/1.1");
        assert_eq!(create.authorization, "Bearer tskey-access-FAKE");
        assert_eq!(
            create.body["capabilities"]["devices"]["create"]["tags"],
            json!(["tag:berth"])
        );
    }

    #[test]
    fn a_refused_oauth_client_says_why() {
        let (url, _seen) = fake(vec![(
            401,
            json!({"error": "invalid_client", "error_description": "client secret is invalid"}),
        )]);
        let mut control = control(None, &url);
        control.credential = Ok(Credential::parse("kFAKE:tskey-client-kFAKE-0000").unwrap());
        control.tags = tags("berth");
        let said = control.create_key("berth", 60).unwrap_err().to_string();
        assert!(
            said.contains("HTTP 401") && said.contains("client secret is invalid"),
            "{said}"
        );
    }

    #[test]
    fn the_secret_is_a_token_or_a_client_and_nothing_else() {
        assert_eq!(
            Credential::parse(" tskey-api-kFAKE-0000\n"),
            Some(Credential::Token("tskey-api-kFAKE-0000".into()))
        );
        assert_eq!(
            Credential::parse("hsFAKE.0000"),
            Some(Credential::Token("hsFAKE.0000".into()))
        );
        assert_eq!(
            Credential::parse("kFAKE:tskey-client-kFAKE-0000"),
            Some(Credential::Client {
                id: "kFAKE".into(),
                secret: "tskey-client-kFAKE-0000".into()
            })
        );
        for bad in ["", ":secret", "id:", "two words", "a\"b"] {
            assert_eq!(Credential::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn headscale_finds_the_user_and_makes_a_one_use_key() {
        let (url, seen) = fake(vec![
            (200, json!({"users": [{"id": "7", "name": "agents"}]})),
            (
                200,
                json!({"preAuthKey": {"key": "hskey-auth-new", "reusable": false}}),
            ),
        ]);
        let mut control = control(Some(&url), "http://127.0.0.1:9");
        control.user = "agents".into();
        control.tags = tags("berth");
        let before = berth_api::unix_now();
        assert_eq!(
            control.create_key("berth studio", 3600).unwrap(),
            "hskey-auth-new"
        );
        let lookup = seen.recv().unwrap();
        assert_eq!(lookup.line, "GET /api/v1/user?name=agents HTTP/1.1");
        assert_eq!(lookup.authorization, "Bearer secret-token");
        let create = seen.recv().unwrap();
        assert_eq!(create.line, "POST /api/v1/preauthkey HTTP/1.1");
        assert_eq!(create.authorization, "Bearer secret-token");
        assert_eq!(create.body["user"], "7");
        assert_eq!(create.body["reusable"], false);
        assert_eq!(create.body["ephemeral"], false);
        assert_eq!(create.body["aclTags"], json!(["tag:berth"]));
        let expiry = create.body["expiration"].as_str().unwrap();
        assert!(
            [
                berth_api::rfc3339(before + 3600),
                berth_api::rfc3339(before + 3601)
            ]
            .contains(&expiry.to_owned()),
            "{expiry}"
        );
    }

    #[test]
    fn headscale_takes_tags_without_a_user_and_needs_one_of_them() {
        let (url, seen) = fake(vec![(
            200,
            json!({"preAuthKey": {"key": "hskey-auth-tagged"}}),
        )]);
        let mut control = control(Some(&url), "");
        let said = control.create_key("berth", 60).unwrap_err();
        assert!(said.to_string().contains("tailnet_user"), "{said}");
        control.tags = tags("tag:berth");
        assert_eq!(
            control.create_key("berth", 60).unwrap(),
            "hskey-auth-tagged"
        );
        let create = seen.recv().unwrap();
        assert!(create.body.get("user").is_none(), "{:?}", create.body);
        // An OAuth client is Tailscale's alone.
        control.credential = Ok(Credential::parse("kFAKE:tskey-client-kFAKE-0000").unwrap());
        assert!(
            control
                .problem()
                .unwrap()
                .contains("Headscale has no OAuth clients")
        );
    }

    #[test]
    fn headscale_without_the_user_or_the_right_key_says_so() {
        let (url, _seen) = fake(vec![
            (200, json!({"users": []})),
            (401, json!("Unauthorized")),
        ]);
        let mut control = control(Some(&url), "");
        control.user = "nobody".into();
        let said = control.create_key("berth", 60).unwrap_err();
        assert_eq!(said.to_string(), "Headscale has no user nobody");
        control.user = "3".into();
        let said = control.create_key("berth", 60).unwrap_err();
        assert!(said.to_string().contains("HTTP 401"), "{said}");
        assert!(said.to_string().contains("Unauthorized"), "{said}");
    }

    fn paths() -> (tempfile::TempDir, Paths) {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: home.path().join("config"),
            data_dir: home.path().join("data"),
        };
        paths.prepare().unwrap();
        (home, paths)
    }

    fn set_secret(paths: &Paths, value: Option<&str>) {
        Store::new(paths)
            .unwrap()
            .set_secret(TAILNET_API_KEY, value)
            .unwrap();
    }

    #[test]
    fn the_settings_and_the_secret_make_the_control_server() {
        let (_home, paths) = paths();
        paths
            .set("tailnet_login_server", "https://hs.example.org/")
            .unwrap();
        paths.set("tailnet_user", "agents").unwrap();
        paths.set("tailnet_tags", "berth").unwrap();
        let control = Control::configured(&paths);
        assert_eq!(control.headscale.as_deref(), Some("https://hs.example.org"));
        assert_eq!(
            (control.user.as_str(), control.tags.clone()),
            ("agents", vec!["tag:berth".to_owned()])
        );
        assert!(!control.can_create());
        let said = control.create_key("berth", 60).unwrap_err();
        assert!(
            said.to_string()
                .contains("berth secret set tailnet_api_key"),
            "{said}"
        );
        set_secret(&paths, Some("hskey-api-x"));
        assert!(Control::configured(&paths).can_create());
        assert_eq!(
            Control::configured(&paths).credential,
            Ok(Credential::Token("hskey-api-x".into()))
        );
        set_secret(&paths, Some("not one"));
        assert!(
            Control::configured(&paths)
                .problem()
                .unwrap()
                .contains("CLIENT_ID:CLIENT_SECRET")
        );
    }

    #[test]
    fn a_missing_api_key_is_a_problem_before_anything_runs() {
        let (_home, paths) = paths();
        let joins = json!({"hostname": "studio"});
        let said = problem(&paths, &joins, "studio").unwrap();
        assert!(
            said.starts_with("studio joins the tailnet at Tailscale"),
            "{said}"
        );
        assert!(said.contains("berth secret set tailnet_api_key"), "{said}");
        assert!(said.contains("Secrets tab"), "{said}");
        assert!(said.contains("over SSH at its address"), "{said}");
        // A host that does not join needs nothing.
        let off = json!({"ansible_vars": {"devbox_install_tailscale": false}});
        assert_eq!(problem(&paths, &off, "studio"), None);
        // An apply gives the role the problem in place of the way to a key.
        let vars = apply_vars(&paths, &joins, "studio", Path::new("/b"));
        assert_eq!(vars["devbox_tailnet_problem"], json!(said));
        assert!(vars.get("devbox_tailscale_key_command").is_none());
        // A seed is not written at all.
        let said = seed_key(&paths, &joins, "studio")
            .err()
            .unwrap()
            .to_string();
        assert!(said.contains("berth secret set tailnet_api_key"), "{said}");
        assert!(seed_key(&paths, &off, "studio").unwrap().is_none());
        set_secret(&paths, Some("tskey-api-kFAKE-0000"));
        assert_eq!(problem(&paths, &joins, "studio"), None);
    }

    #[test]
    fn hosts_join_the_login_server_of_berth() {
        let (_home, paths) = paths();
        set_secret(&paths, Some("hskey-api-x"));
        assert_eq!(login_server(&paths), "");
        assert_eq!(
            apply_vars(&paths, &json!({}), "studio", Path::new("/usr/bin/berth")),
            json!({"devbox_tailscale_key_command": ["/usr/bin/berth", "tailnet-key", "studio"]})
        );
        paths
            .set("tailnet_login_server", "http://hs.test:8080")
            .unwrap();
        paths.set("tailnet_user", "1").unwrap();
        assert_eq!(login_server(&paths), "http://hs.test:8080");
        assert_eq!(
            apply_vars(&paths, &json!({}), "studio", Path::new("/usr/bin/berth")),
            json!({
                "devbox_tailscale_login_server": "http://hs.test:8080",
                "devbox_tailscale_key_command": ["/usr/bin/berth", "tailnet-key", "studio"],
            })
        );
        // Without Tailscale there is nothing to join.
        let off = json!({"ansible_vars": {"devbox_install_tailscale": false}});
        assert_eq!(
            apply_vars(&paths, &off, "studio", Path::new("/b")),
            json!({})
        );
    }

    #[test]
    fn a_seed_gets_a_new_key_in_a_private_file_and_says_until_when() {
        let (_home, paths) = paths();
        let (url, seen) = fake(vec![(
            200,
            json!({"preAuthKey": {"key": "hskey-auth-seed"}}),
        )]);
        paths.set("tailnet_login_server", &url).unwrap();
        paths.set("tailnet_user", "5").unwrap();
        set_secret(&paths, Some("hskey-api-x"));
        let before = berth_api::unix_now();
        let given = seed_key(&paths, &json!({"hostname": "studio"}), "studio")
            .unwrap()
            .unwrap();
        let asked = seen.recv().unwrap();
        assert_eq!(asked.body["user"], "5");
        let expiry = asked.body["expiration"].as_str().unwrap().to_owned();
        assert!(
            [
                berth_api::rfc3339(before + SEED_LIFETIME),
                berth_api::rfc3339(before + SEED_LIFETIME + 1)
            ]
            .contains(&expiry),
            "{expiry}"
        );
        assert!((before + SEED_LIFETIME..=before + SEED_LIFETIME + 1).contains(&given.until));
        let file: Value =
            serde_json::from_slice(&std::fs::read(given.file.path()).unwrap()).unwrap();
        assert_eq!(file, json!({"key": "hskey-auth-seed", "login_server": url}));
        let mode = std::os::unix::fs::PermissionsExt::mode(
            &given.file.as_file().metadata().unwrap().permissions(),
        );
        assert_eq!(mode & 0o077, 0);
        let mut command = Command::new("true");
        given.pass(&mut command);
        assert!(
            command
                .get_envs()
                .any(|(name, value)| name == "BERTH_TAILNET_FILE"
                    && value == Some(given.file.path().as_os_str()))
        );
    }

    #[test]
    fn the_note_of_an_installer_says_until_when_it_can_join() {
        // 2026-11-16 10:30:00 UTC.
        let note = until_note(1_794_825_000);
        assert!(
            note.starts_with("Its key joins the tailnet until 2026-11-16 10:30 UTC"),
            "{note}"
        );
        assert!(note.contains("24 hours"), "{note}");
        assert!(
            note.contains("applying it from berth then joins it"),
            "{note}"
        );
    }

    #[test]
    fn the_key_command_of_an_apply_finds_its_host_by_name_and_by_path() {
        let (home, paths) = paths();
        let (url, seen) = fake(vec![
            (200, json!({"preAuthKey": {"key": "hskey-auth-named"}})),
            (200, json!({"preAuthKey": {"key": "hskey-auth-file"}})),
        ]);
        paths.set("tailnet_login_server", &url).unwrap();
        paths.set("tailnet_user", "5").unwrap();
        set_secret(&paths, Some("hskey-api-x"));
        let config = json!({"hostname": "studio"});
        let stored = crate::profiles::Place::own(&paths, "studio").unwrap();
        let file = crate::profiles::Place::File(home.path().join("attic.json"));
        let mut keys = vec![];
        for place in [&stored, &file] {
            place.save(&paths, &config).unwrap();
            let vars = apply_vars(&paths, &config, &place.argument(), Path::new("/b"));
            let what = vars["devbox_tailscale_key_command"][2].as_str().unwrap();
            // The child of the apply has the same settings, so it looks in the same place.
            assert_eq!(
                &crate::profiles::Place::resolve(&paths, what).unwrap(),
                place
            );
            keys.push(key_for(&paths, what).unwrap());
            assert!(seen.recv().unwrap().line.starts_with("POST"));
        }
        assert_eq!(keys, ["hskey-auth-named", "hskey-auth-file"]);
        let by_path = file.argument();
        assert_eq!(
            crate::profiles::Place::resolve(&paths, &by_path).unwrap(),
            file
        );
    }

    fn device(id: &str, dns_name: &str, addresses: &[&str]) -> TailnetDevice {
        TailnetDevice {
            id: id.into(),
            dns_name: dns_name.into(),
            host_name: dns_name.split('.').next().unwrap().into(),
            addresses: addresses
                .iter()
                .map(|address| address.to_string())
                .collect(),
            tailnet: String::new(),
        }
    }

    /// The host `studio`, saved with this profile.
    fn studio(paths: &Paths, config: Value) -> crate::host::Host {
        let place = Place::own(paths, "studio").unwrap();
        place.save(paths, &config).unwrap();
        crate::host::Host {
            name: "studio".into(),
            place,
            config,
        }
    }

    fn record(paths: &Paths, place: &Place, kept: &Kept) {
        kept.save(paths, place).unwrap();
    }

    #[test]
    fn a_destroyed_hosts_recorded_device_is_removed_from_tailscale_and_headscale() {
        let (_home, paths) = paths();
        let host = studio(&paths, json!({"hostname": "studio"}));
        let recorded = |id: &str| Kept {
            device: Some(device(id, "studio.example.ts.net", &["100.64.0.7"])),
            rename: None,
        };
        record(&paths, &host.place, &recorded("nFAKE1CNTRL"));
        let (url, seen) = fake(vec![(200, json!({}))]);
        let said =
            remove_device_at(&control(None, &url), &paths, &host.place, &host.config).unwrap();
        assert_eq!(
            said,
            "✓ Removed studio's device studio.example.ts.net from the tailnet at Tailscale"
        );
        let asked = seen.recv().unwrap();
        assert_eq!(asked.line, "DELETE /api/v2/device/nFAKE1CNTRL HTTP/1.1");
        assert_eq!(
            asked.authorization,
            format!("Basic {}", crate::registry::base64(b"secret-token:"))
        );

        record(&paths, &host.place, &recorded("12"));
        let (url, seen) = fake(vec![(200, json!({}))]);
        let said =
            remove_device_at(&control(Some(&url), ""), &paths, &host.place, &host.config).unwrap();
        assert!(said.starts_with("✓ Removed studio's device"), "{said}");
        let asked = seen.recv().unwrap();
        assert_eq!(asked.line, "DELETE /api/v1/node/12 HTTP/1.1");
        assert_eq!(asked.authorization, "Bearer secret-token");
    }

    #[test]
    fn a_device_gone_already_or_one_berth_may_not_remove_is_said_and_never_fails() {
        let (_home, paths) = paths();
        let host = studio(&paths, json!({"hostname": "studio"}));
        let kept = Kept {
            device: Some(device("nFAKE1CNTRL", "studio.example.ts.net", &[])),
            rename: None,
        };
        record(&paths, &host.place, &kept);
        let (url, _seen) = fake(vec![
            (404, json!({"message": "not found"})),
            (
                403,
                json!({"message": "calling actor does not have enough permissions to perform this function"}),
            ),
        ]);
        let mut tailscale = control(None, &url);
        let said = remove_device_at(&tailscale, &paths, &host.place, &host.config).unwrap();
        assert_eq!(
            said,
            "studio's device studio.example.ts.net is not on the tailnet at Tailscale any more"
        );
        tailscale.credential = Ok(Credential::parse("kFAKE:tskey-client-kFAKE-0000").unwrap());
        // The client's token comes first; the fake answers it with the 403 of the next call.
        let (url, _seen) = fake(vec![
            (200, json!({"access_token": "tskey-access-FAKE"})),
            (
                403,
                json!({"message": "calling actor does not have enough permissions to perform this function"}),
            ),
        ]);
        tailscale.tailscale_api = url;
        let said = remove_device_at(&tailscale, &paths, &host.place, &host.config).unwrap();
        assert!(said.starts_with("! studio's device studio.example.ts.net stays on the tailnet: Tailscale refused to remove the device (HTTP 403)"), "{said}");
        assert!(said.contains("needs the devices:core scope"), "{said}");
        assert!(said.ends_with("Remove it there"), "{said}");
        // A refused Headscale key is named as such.
        let (url, _seen) = fake(vec![(401, json!("Unauthorized"))]);
        let said =
            remove_device_at(&control(Some(&url), ""), &paths, &host.place, &host.config).unwrap();
        assert!(
            said.contains("HTTP 401") && said.contains("headscale apikeys list"),
            "{said}"
        );
    }

    #[test]
    fn without_the_key_or_the_tailnet_berth_removes_nothing() {
        let (_home, paths) = paths();
        // A host that never joined, with no record, has nothing on the tailnet.
        let off = studio(
            &paths,
            json!({"ansible_vars": {"devbox_install_tailscale": false}}),
        );
        assert_eq!(remove_device(&paths, &off.place, &off.config), None);
        let host = studio(&paths, json!({"hostname": "studio"}));
        let said = remove_device(&paths, &host.place, &host.config).unwrap();
        assert!(
            said.starts_with("! studio's device on the tailnet at Tailscale stays, as berth cannot remove it: berth has no API key"),
            "{said}"
        );
        assert!(said.contains("berth secret set tailnet_api_key"), "{said}");
    }

    #[test]
    fn a_host_without_a_record_is_looked_up_and_its_device_removed_only_when_alone() {
        let (_home, paths) = paths();
        let tailscale_list = |devices: Value| (200, json!({"devices": devices}));
        let listed = json!([
            {"id": "111", "nodeId": "nSTUDIO", "name": "studio.example.ts.net", "hostname": "studio", "addresses": ["100.64.0.7"]},
            {"id": "222", "nodeId": "nSTUDIO1", "name": "studio-1.example.ts.net", "hostname": "studio", "addresses": ["100.64.0.9"]},
            {"id": "333", "nodeId": "nDOCK", "name": "dock.example.ts.net", "hostname": "dock", "addresses": ["100.64.0.8"]},
        ]);

        // At its address: that device alone, though two have its hostname.
        let host = studio(
            &paths,
            json!({"hostname": "studio", "access": {"address": "100.64.0.9"}}),
        );
        let (url, seen) = fake(vec![tailscale_list(listed.clone()), (200, json!({}))]);
        let said =
            remove_device_at(&control(None, &url), &paths, &host.place, &host.config).unwrap();
        assert_eq!(
            seen.recv().unwrap().line,
            "GET /api/v2/tailnet/-/devices HTTP/1.1"
        );
        assert_eq!(
            seen.recv().unwrap().line,
            "DELETE /api/v2/device/nSTUDIO1 HTTP/1.1"
        );
        assert!(said.contains("studio-1.example.ts.net"), "{said}");

        // By its hostname, two devices could be it: berth removes neither.
        let host = studio(
            &paths,
            json!({"hostname": "studio", "access": {"address": "192.0.2.10"}}),
        );
        let (url, seen) = fake(vec![tailscale_list(listed.clone())]);
        let said =
            remove_device_at(&control(None, &url), &paths, &host.place, &host.config).unwrap();
        assert!(
            said.contains(
                "2 devices there could be it: studio.example.ts.net, studio-1.example.ts.net"
            ),
            "{said}"
        );
        assert!(said.contains("removes none of them"), "{said}");
        seen.recv().unwrap();
        assert!(
            seen.recv_timeout(std::time::Duration::from_millis(300))
                .is_err()
        );

        // None has its address or hostname: nothing to remove.
        let host = studio(
            &paths,
            json!({"hostname": "pier", "access": {"address": "192.0.2.10"}}),
        );
        let (url, _seen) = fake(vec![tailscale_list(listed)]);
        let said =
            remove_device_at(&control(None, &url), &paths, &host.place, &host.config).unwrap();
        assert!(
            said.contains("none there has its address or hostname"),
            "{said}"
        );

        // On Headscale, by its MagicDNS name, the node's given name.
        let host = studio(
            &paths,
            json!({"hostname": "pier", "access": {"address": "studio.hs.example.org"}}),
        );
        let (url, seen) = fake(vec![
            (
                200,
                json!({"nodes": [
                    {"id": "12", "name": "studio", "givenName": "studio", "ipAddresses": ["100.64.0.7"]},
                    {"id": "13", "name": "dock", "givenName": "dock", "ipAddresses": ["100.64.0.8"]},
                ]}),
            ),
            (200, json!({})),
        ]);
        let said =
            remove_device_at(&control(Some(&url), ""), &paths, &host.place, &host.config).unwrap();
        assert_eq!(seen.recv().unwrap().line, "GET /api/v1/node HTTP/1.1");
        assert_eq!(seen.recv().unwrap().line, "DELETE /api/v1/node/12 HTTP/1.1");
        assert!(said.starts_with("✓ Removed studio's device"), "{said}");
    }

    #[test]
    fn an_apply_records_the_device_and_renames_it_after_a_rename_with_the_hostname() {
        let (_home, paths) = paths();
        let reported = device("nFAKE1CNTRL", "studio.example.ts.net", &["100.64.0.7"]);
        let host = studio(
            &paths,
            json!({"hostname": "studio", "access": {"address": "studio.example.ts.net"}}),
        );
        // Nothing to rename: the device is recorded and nothing is asked.
        let lines = after_apply_at(
            &control(None, "http://127.0.0.1:9"),
            &paths,
            &host,
            Some(reported.clone()),
        )
        .unwrap();
        assert!(lines.is_empty(), "{lines:?}");
        let kept = Kept::of(&paths, &host.place).unwrap();
        assert_eq!(
            kept,
            Kept {
                device: Some(reported.clone()),
                rename: None
            }
        );

        // A rename with the hostname leaves the device for the apply.
        let place = crate::profiles::rename(&paths, &host.place, "pier", true).unwrap();
        let kept = Kept::of(&paths, &place).unwrap();
        assert_eq!(kept.rename.as_deref(), Some("pier"));
        assert_eq!(kept.device.as_ref(), Some(&reported));
        assert_eq!(Kept::of(&paths, &host.place).unwrap(), Kept::default());
        let pier = crate::host::Host::load_at(&paths, place.clone()).unwrap();

        // A refusal keeps the rename for the next apply.
        let (url, _seen) = fake(vec![(403, json!({"message": "not enough permissions"}))]);
        let lines =
            after_apply_at(&control(None, &url), &paths, &pier, Some(reported.clone())).unwrap();
        assert!(lines[0].starts_with("! pier's device on the tailnet is still studio.example.ts.net: Tailscale refused to rename the device (HTTP 403)"), "{lines:?}");
        assert_eq!(
            Kept::of(&paths, &place).unwrap().rename.as_deref(),
            Some("pier")
        );

        let (url, seen) = fake(vec![(200, json!({}))]);
        let lines =
            after_apply_at(&control(None, &url), &paths, &pier, Some(reported.clone())).unwrap();
        let asked = seen.recv().unwrap();
        assert_eq!(asked.line, "POST /api/v2/device/nFAKE1CNTRL/name HTTP/1.1");
        assert_eq!(asked.body, json!({"name": "pier"}));
        assert_eq!(
            lines,
            [
                "✓ Renamed pier's device on the tailnet from studio.example.ts.net to pier.example.ts.net",
                "pier is reached at pier.example.ts.net now",
            ]
        );
        let kept = Kept::of(&paths, &place).unwrap();
        assert_eq!(kept.rename, None);
        assert_eq!(kept.device.unwrap().dns_name, "pier.example.ts.net");
        assert_eq!(
            place.load(&paths).unwrap().unwrap()["access"]["address"],
            "pier.example.ts.net"
        );

        // On Headscale the new name is in the path.
        let place = crate::profiles::rename(&paths, &place, "attic", true).unwrap();
        let attic = crate::host::Host::load_at(&paths, place.clone()).unwrap();
        let (url, seen) = fake(vec![(
            200,
            json!({"node": {"id": "12", "givenName": "attic"}}),
        )]);
        after_apply_at(
            &control(Some(&url), ""),
            &paths,
            &attic,
            Some(device("12", "pier.hs.example.org", &[])),
        )
        .unwrap();
        assert_eq!(
            seen.recv().unwrap().line,
            "POST /api/v1/node/12/rename/attic HTTP/1.1"
        );
        assert_eq!(Kept::of(&paths, &place).unwrap().rename, None);
    }

    #[test]
    fn a_rename_without_the_hostname_a_copy_and_a_forgotten_host_leave_the_tailnet_alone() {
        let (_home, paths) = paths();
        let host = studio(&paths, json!({"hostname": "studio"}));
        let reported = device("nFAKE1CNTRL", "studio.example.ts.net", &[]);
        let kept = Kept {
            device: Some(reported.clone()),
            rename: None,
        };
        record(&paths, &host.place, &kept);
        let place = crate::profiles::rename(&paths, &host.place, "pier", false).unwrap();
        assert_eq!(Kept::of(&paths, &place).unwrap(), kept);
        // Nothing is asked of the control server: its address goes nowhere.
        let pier = crate::host::Host::load_at(&paths, place.clone()).unwrap();
        let lines = after_apply_at(
            &control(None, "http://127.0.0.1:9"),
            &paths,
            &pier,
            Some(reported),
        )
        .unwrap();
        assert!(lines.is_empty(), "{lines:?}");
        // A copy starts with no device of its own.
        let (copy, _) = crate::profiles::duplicate(&paths, &place, "lab").unwrap();
        assert_eq!(Kept::of(&paths, &copy).unwrap(), Kept::default());
        // Forgetting the host forgets its record, and asks nothing of the tailnet.
        crate::profiles::remove(&paths, &place).unwrap();
        assert_eq!(Kept::of(&paths, &place).unwrap(), Kept::default());
    }

    #[test]
    fn names_and_ids_are_checked_before_they_go_into_a_path() {
        let control = control(None, "http://127.0.0.1:9");
        assert!(control.delete_device("../keys").is_err());
        assert!(control.rename_device("nFAKE", "Pier/x").is_err());
        assert!(control.rename_device("nFAKE", "-pier").is_err());
        assert_eq!(
            renamed("studio.example.ts.net", "pier"),
            "pier.example.ts.net"
        );
        assert_eq!(renamed("studio", "pier"), "pier");
    }
}
