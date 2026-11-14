//! The tailnet hosts join: Tailscale's own service, or a Headscale server.
//!
//! Both take the same `tailscale` client; a Headscale host is pointed at its
//! server with `--login-server`. With an API key for the control server berth
//! creates the one-use enrollment keys itself, at the moment a host needs one.
//!
//! - `tailnet_login_server`: empty for Tailscale, the address of a Headscale
//!   server otherwise. A profile's `tailscale_login_server` wins for its host
//! - `tailnet_user`: the Headscale user a new host belongs to
//! - `tailnet_tags`: tags for new hosts, comma separated
//! - the secret `tailnet_api_key`: Tailscale API access token or Headscale API
//!   key. `TS_API_KEY` in the environment works for Tailscale too

use crate::{Result, paths::Paths, registry::Store};
use berth_api::profile::{self, TAILNET_API_KEY};
use serde_json::{Value, json};
use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

/// How long a key lives that goes into a seed or an installer: long enough to
/// write the installer and let the first boot enroll.
pub const SEED_LIFETIME: u64 = 24 * 60 * 60;
/// How long a key lives that an apply uses straight away.
pub const APPLY_LIFETIME: u64 = 60 * 60;

const TAILSCALE_API: &str = "https://api.tailscale.com";

/// The settings of the tailnet, in the order the Settings tab lists them.
pub const SETTINGS: &[&str] = &["tailnet_login_server", "tailnet_user", "tailnet_tags"];

/// The control server berth creates keys at, and what it needs for that.
#[derive(Debug, Clone, PartialEq)]
pub struct Control {
    /// The Headscale server; None for Tailscale.
    pub headscale: Option<String>,
    pub user: String,
    pub tags: Vec<String>,
    token: Option<String>,
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
        let headscale = server_address(&setting("tailnet_login_server"));
        let mut tags = tags(&setting("tailnet_tags"));
        let mut token = Store::new(paths)
            .and_then(|store| store.secret(TAILNET_API_KEY))
            .ok()
            .flatten()
            .filter(|token| !token.is_empty() && token != crate::server::KEPT);
        if headscale.is_none() {
            let variable = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
            token = token.or_else(|| variable("TS_API_KEY"));
            if tags.is_empty() {
                tags = self::tags(&variable("DEVBOX_TAILSCALE_TAGS").unwrap_or_default());
            }
        }
        Control {
            headscale,
            user: setting("tailnet_user"),
            tags,
            token,
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

    /// Whether berth has what it needs to create keys.
    pub fn can_create(&self) -> bool {
        self.token.is_some()
    }

    /// A new pre-approved key that joins one host, once, within `lifetime` seconds.
    pub fn create_key(&self, description: &str, lifetime: u64) -> Result<String> {
        let token = self.token.as_deref().ok_or_else(|| match &self.headscale {
            Some(_) => format!(
                "berth needs an API key of Headscale to create enrollment keys: berth secret set {TAILNET_API_KEY} (make one with: headscale apikeys create)"
            ),
            None => format!(
                "berth needs a Tailscale API access token to create enrollment keys: berth secret set {TAILNET_API_KEY}, or TS_API_KEY in the environment"
            ),
        })?;
        if token.contains(['"', '\\', '\n', '\r']) {
            return Err(format!("The secret {TAILNET_API_KEY} is not a usable API key").into());
        }
        match &self.headscale {
            Some(url) => self.headscale_key(url, token, lifetime),
            None => self.tailscale_key(token, description, lifetime),
        }
    }

    fn tailscale_key(&self, token: &str, description: &str, lifetime: u64) -> Result<String> {
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
        let (status, reply) = call(&url, &format!("user = \"{token}:\""), Some(&request))?;
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
            "expiration": rfc3339(now() + lifetime),
        });
        if !self.tags.is_empty() {
            request["aclTags"] = json!(self.tags);
        }
        match (self.user.as_str(), self.tags.is_empty()) {
            ("", true) => {
                return Err("Headscale needs a user or tags for a new host's key: berth config tailnet_user NAME, or berth config tailnet_tags TAG".into());
            }
            ("", false) => {}
            // Headscale's API names the user by its number.
            (user, _) if user.bytes().all(|byte| byte.is_ascii_digit()) => {
                request["user"] = json!(user);
            }
            (user, _) => {
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
        let (status, reply) = call(&format!("{url}/api/v1/preauthkey"), &login, Some(&request))?;
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

/// One request with curl: the status, and the answer as JSON or as text. The
/// login is a line of curl's configuration, given on its standard input so it
/// never appears in the process list.
fn call(url: &str, login: &str, body: Option<&Value>) -> Result<(u16, Value)> {
    let file = tempfile::NamedTempFile::new()?;
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
        serde_json::to_writer(&file, body)?;
        command
            .args([
                "--request",
                "POST",
                "--header",
                "Content-Type: application/json",
            ])
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

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

/// Seconds since 1970 as an RFC 3339 time in UTC.
fn rfc3339(seconds: u64) -> String {
    let (days, rest) = ((seconds / 86400) as i64, seconds % 86400);
    // Howard Hinnant's days_from_civil, the other way round.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest / 60 % 60,
        rest % 60
    )
}

/// The login server a host's client is pointed at: the profile's own, or
/// berth's. Empty for Tailscale's service.
pub fn login_server(paths: &Paths, config: &Value) -> String {
    server_address(config["tailscale_login_server"].as_str().unwrap_or(""))
        .or_else(|| server_address(&paths.setting("tailnet_login_server").unwrap_or_default()))
        .unwrap_or_default()
}

/// What a key is described as at the control server.
fn description(host: &str) -> String {
    match host {
        "" => "berth".into(),
        host => format!("berth {host}"),
    }
}

/// A key for a host to join with, from the control server of berth's
/// settings. A host whose profile names another login server gets none, as
/// that server would not take it.
pub fn create_key(paths: &Paths, config: &Value, host: &str, lifetime: u64) -> Result<String> {
    let control = Control::configured(paths);
    let wanted = login_server(paths, config);
    if control.headscale.as_deref().unwrap_or("") != wanted {
        return Err(format!(
            "{host} joins the tailnet at {}, and berth creates keys at {}: set tailnet_login_server to the same server, or paste a key",
            match wanted.as_str() {
                "" => "Tailscale",
                other => other,
            },
            control.name()
        )
        .into());
    }
    control.create_key(&description(host), lifetime)
}

/// A key for the profile's key field, as F4 makes one, and what to say of it.
pub fn generate(paths: &Paths, config: &Value, host: &str) -> Result<(String, String)> {
    let key = create_key(paths, config, host, SEED_LIFETIME)?;
    let note = format!(
        "One-use key created at {} · valid for 24 hours",
        Control::configured(paths).name()
    );
    Ok((key, note))
}

/// The key `berth tailnet-key WHAT` prints. An apply runs that on this
/// machine for a host that is not on the tailnet yet, with the name or the
/// path [`apply_vars`] was given, so it finds the host in the same place.
pub fn key_for(paths: &Paths, what: &str) -> Result<String> {
    let host = crate::host::Host::load(paths, what)?;
    create_key(paths, &host.config, &host.place.title(), APPLY_LIFETIME)
}

/// The profile as a seed carries it: the login server filled in, and for a
/// host that joins automatically a new key. Keys are created only here, when
/// a seed is written, and live for [`SEED_LIFETIME`].
pub fn seed_profile(paths: &Paths, config: &Value, host: &str) -> Result<Value> {
    let mut config = config.clone();
    if !profile::installs_tailscale(&config) {
        return Ok(config);
    }
    if profile::joins_automatically(&config) {
        config["tailscale_auth_key"] = json!(create_key(paths, &config, host, SEED_LIFETIME)?);
    }
    let server = login_server(paths, &config);
    if !server.is_empty() {
        config["tailscale_login_server"] = json!(server);
    }
    Ok(config)
}

/// A private copy of a profile for the seed builder, which reads a file.
pub fn profile_file(config: &Value) -> Result<tempfile::NamedTempFile> {
    let mut file = tempfile::NamedTempFile::new()?;
    serde_json::to_writer(&mut file, config)?;
    file.flush()?;
    Ok(file)
}

/// What an apply adds to the role's variables: the login server, and for a
/// host that joins automatically the command the role runs on this machine
/// for a key, only when the host is not on the tailnet yet. `berth` is this
/// program.
pub fn apply_vars(paths: &Paths, config: &Value, host: &str, berth: &Path) -> Value {
    let mut vars = json!({});
    if !profile::installs_tailscale(config) {
        return vars;
    }
    let server = login_server(paths, config);
    if !server.is_empty() {
        vars["devbox_tailscale_login_server"] = json!(server);
    }
    if profile::joins_automatically(config) {
        vars["devbox_tailscale_key_command"] = json!([berth, "tailnet-key", host]);
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
                let (mut length, mut authorization) = (0, String::new());
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
                        _ => {}
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                sender
                    .send(Seen {
                        line: line.trim_end().to_owned(),
                        authorization,
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
            token: Some("secret-token".into()),
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
        let before = now();
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
            [rfc3339(before + 3600), rfc3339(before + 3601)].contains(&expiry.to_owned()),
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

    #[test]
    fn times_are_written_as_rfc3339() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_798_761_599), "2026-12-31T23:59:59Z");
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
        Store::new(&paths)
            .unwrap()
            .set_secret(TAILNET_API_KEY, Some("hskey-api-x"))
            .unwrap();
        assert!(Control::configured(&paths).can_create());
        assert_eq!(
            Control::configured(&paths).token.as_deref(),
            Some("hskey-api-x")
        );
    }

    #[test]
    fn hosts_join_the_login_server_of_their_profile_or_of_berth() {
        let (_home, paths) = paths();
        let auto = json!({"tailscale_auto_key": true});
        assert_eq!(login_server(&paths, &json!({})), "");
        assert_eq!(
            apply_vars(
                &paths,
                &json!({"tailscale_auth_key": "k"}),
                "studio",
                Path::new("/b")
            ),
            json!({})
        );
        paths
            .set("tailnet_login_server", "http://hs.test:8080")
            .unwrap();
        assert_eq!(login_server(&paths, &json!({})), "http://hs.test:8080");
        let own = json!({"tailscale_login_server": "https://other.test/"});
        assert_eq!(login_server(&paths, &own), "https://other.test");
        assert_eq!(
            apply_vars(&paths, &auto, "studio", Path::new("/usr/bin/berth")),
            json!({
                "devbox_tailscale_login_server": "http://hs.test:8080",
                "devbox_tailscale_key_command": ["/usr/bin/berth", "tailnet-key", "studio"],
            })
        );
        // berth makes no key a host's own login server would not take.
        let mut other = own.clone();
        other["tailscale_auto_key"] = json!(true);
        let said = create_key(&paths, &other, "studio", 60).unwrap_err();
        assert!(said.to_string().contains("https://other.test"), "{said}");
        // Without Tailscale there is nothing to join.
        let off = json!({"tailscale_auto_key": true, "ansible_vars": {"devbox_install_tailscale": false}});
        assert_eq!(
            apply_vars(&paths, &off, "studio", Path::new("/b")),
            json!({})
        );
        assert_eq!(seed_profile(&paths, &off, "studio").unwrap(), off);
    }

    #[test]
    fn a_seed_carries_a_new_key_and_the_login_server() {
        let (_home, paths) = paths();
        let (url, seen) = fake(vec![(
            200,
            json!({"preAuthKey": {"key": "hskey-auth-seed"}}),
        )]);
        paths.set("tailnet_login_server", &url).unwrap();
        paths.set("tailnet_user", "5").unwrap();
        Store::new(&paths)
            .unwrap()
            .set_secret(TAILNET_API_KEY, Some("hskey-api-x"))
            .unwrap();
        let config = json!({"hostname": "studio", "tailscale_auto_key": true});
        let seeded = seed_profile(&paths, &config, "studio").unwrap();
        assert_eq!(seeded["tailscale_auth_key"], "hskey-auth-seed");
        assert_eq!(seeded["tailscale_login_server"], json!(url));
        let asked = seen.recv().unwrap();
        assert_eq!(asked.body["user"], "5");
        // A pasted key is used as it is, and no key is created for it.
        let pasted = json!({"tailscale_auto_key": true, "tailscale_auth_key": "hskey-auth-pasted"});
        let seeded = seed_profile(&paths, &pasted, "studio").unwrap();
        assert_eq!(seeded["tailscale_auth_key"], "hskey-auth-pasted");
        let file = profile_file(&seeded).unwrap();
        let mode = std::os::unix::fs::PermissionsExt::mode(
            &file.as_file().metadata().unwrap().permissions(),
        );
        assert_eq!(mode & 0o077, 0);
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
        Store::new(&paths)
            .unwrap()
            .set_secret(TAILNET_API_KEY, Some("hskey-api-x"))
            .unwrap();
        let config = json!({"hostname": "studio", "tailscale_auto_key": true});
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
