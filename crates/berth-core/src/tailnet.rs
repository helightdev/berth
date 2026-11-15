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

use crate::{Result, paths::Paths, registry::Store};
use berth_api::profile::{self, TAILNET_API_KEY};
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
            .filter(|secret| !secret.is_empty() && secret != crate::server::KEPT);
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
        let credential = match &self.credential {
            Ok(credential) => credential,
            Err(why) => return Some(why.clone()),
        };
        match (&self.headscale, credential) {
            (Some(_), Credential::Client { .. }) => Some(format!(
                "Headscale has no OAuth clients: the secret {TAILNET_API_KEY} is one of its API keys (headscale apikeys create)"
            )),
            (Some(_), _) if self.user.is_empty() && self.tags.is_empty() => Some(
                "Headscale needs a user or tags for a new host's key: berth config tailnet_user NAME, or berth config tailnet_tags TAG".into(),
            ),
            (None, Credential::Client { .. }) if self.tags.is_empty() => Some(
                "Keys made by a Tailscale OAuth client carry tags: berth config tailnet_tags with the client's tags, such as tag:berth".into(),
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
        if let Some(problem) = self.problem() {
            return Err(problem.into());
        }
        let credential = self.credential.as_ref().map_err(String::clone)?;
        match (&self.headscale, credential) {
            (Some(url), Credential::Token(token)) => self.headscale_key(url, token, lifetime),
            (None, Credential::Token(token)) => {
                self.tailscale_key(&format!("user = \"{token}:\""), description, lifetime)
            }
            (None, Credential::Client { id, secret }) => {
                let token = self.exchange(id, secret)?;
                let login = format!("header = \"Authorization: Bearer {token}\"");
                self.tailscale_key(&login, description, lifetime)
            }
            (Some(_), Credential::Client { .. }) => Err("Headscale has no OAuth clients".into()),
        }
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

    fn headscale_key(&self, url: &str, token: &str, lifetime: u64) -> Result<String> {
        let login = format!("header = \"Authorization: Bearer {token}\"");
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
                let (status, reply) = call(&lookup, &login, None)?;
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
            &login,
            Some(Body::Json(request)),
        )?;
        match reply["preAuthKey"]["key"].as_str() {
            Some(key) if status == 200 && !key.is_empty() => Ok(key.to_owned()),
            _ => Err(refused("Headscale", status, &reply)),
        }
    }
}

fn refused(who: &str, status: u16, reply: &Value) -> Box<dyn std::error::Error> {
    let reason = match (reply["message"].as_str(), reply.as_str()) {
        (Some(message), _) => message.to_owned(),
        (None, Some(text)) if !text.trim().is_empty() => text.trim().chars().take(200).collect(),
        _ => "no reason given".into(),
    };
    format!("{who} refused to create a key (HTTP {status}): {reason}").into()
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
            .args(["--request", "POST", "--header"])
            .arg(format!("Content-Type: {kind}"))
            .arg("--data-binary")
            .arg(format!("@{}", file.path().display()));
    }
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
        let stored = crate::profiles::Place::own("studio").unwrap();
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
        // A berth that uses a berth server has none of its own hosts by name;
        // berth serve refuses to start like that, so its children never do.
        paths.set("server", "http://192.0.2.7:7777").unwrap();
        let said = key_for(&paths, "studio").unwrap_err().to_string();
        assert_eq!(said, "No host studio");
        let by_path = file.argument();
        assert_eq!(
            crate::profiles::Place::resolve(&paths, &by_path).unwrap(),
            file
        );
    }
}
