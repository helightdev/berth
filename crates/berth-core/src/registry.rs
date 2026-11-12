//! Registries and secrets: where packages and images come from and go to, and
//! the logins and tokens a host is given.
//!
//! Registries are settings of the tool (`config.json`), because a server is the
//! same for every host. A Sonatype Nexus server is one registry with its
//! repositories under it. Secret values are kept apart from everything else in
//! `secrets.json`, private to this user.

use crate::{
    Result,
    paths::Paths,
    plugins::valid_name,
    server::{KEPT, Server},
};
use serde_json::{Map, Value, json};
use std::{
    fs,
    io::{IsTerminal, Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
    process::{Command, Stdio},
};

/// A Nexus server has repositories under it; the others are one registry each.
pub const TYPES: &[&str] = &["nexus", "npm", "raw", "docker"];

pub fn type_label(kind: &str) -> &'static str {
    match kind {
        "nexus" => "Sonatype Nexus",
        "npm" => "npm registry",
        "raw" => "File server (HTTP upload)",
        _ => "Docker registry",
    }
}
pub const FORMATS: &[&str] = &["npm", "raw", "docker"];

pub struct Store {
    dir: PathBuf,
    /// The berth serve these are the registries and secrets of. Changes go
    /// there, and the values of its secrets stay there.
    server: Option<Server>,
}

fn read(path: PathBuf) -> Result<Map<String, Value>> {
    match fs::read(path) {
        Ok(data) => Ok(serde_json::from_slice(&data)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(error) => Err(error.into()),
    }
}

fn write(path: PathBuf, data: &Map<String, Value>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all((serde_json::to_string_pretty(data)? + "\n").as_bytes())?;
    Ok(())
}

pub fn base64(data: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut text = String::new();
    for chunk in data.chunks(3) {
        let bits = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        for index in 0..4 {
            text.push(if index <= chunk.len() {
                TABLE[(bits >> (18 - 6 * index) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    text
}

/// The address of a registry without its scheme or a trailing slash.
fn bare(url: &str) -> &str {
    let url = url.trim_end_matches('/');
    url.split_once("://").map_or(url, |(_, rest)| rest)
}

/// Where Docker reaches a Nexus Docker repository: the address given for a
/// connector with a port or host of its own, otherwise the server's host with
/// the repository as the first part of the path (Nexus' path based routing).
pub fn docker_address(registry: &Value, repository: &Value) -> String {
    match repository["address"]
        .as_str()
        .filter(|address| !address.is_empty())
    {
        Some(address) => address.to_owned(),
        None => format!(
            "{}/{}",
            bare(registry["url"].as_str().unwrap_or("")),
            repository["name"].as_str().unwrap_or("")
        ),
    }
}

/// The host of a Docker address, which is what a login belongs to.
fn docker_host(address: &str) -> &str {
    address.split('/').next().unwrap_or(address)
}

/// Where a Nexus repository is served.
pub fn repository_url(registry: &Value, name: &str) -> String {
    format!(
        "{}/repository/{name}/",
        registry["url"].as_str().unwrap_or("").trim_end_matches('/')
    )
}

impl Store {
    pub fn new(paths: &Paths) -> Self {
        Store {
            dir: paths.store_dir(),
            server: Server::configured(paths),
        }
    }

    /// The store in a directory: a server's when it is where berth keeps one.
    pub fn at(dir: PathBuf) -> Self {
        let server = Paths::locate().ok().and_then(|paths| {
            Server::configured(&paths).filter(|server| server.dir(&paths) == dir)
        });
        Store { dir, server }
    }

    /// The server whose store this is.
    pub fn server(&self) -> Option<&Server> {
        self.server.as_ref()
    }

    /// Fails for a server's store, whose secrets have their values there.
    fn own(&self, what: &str) -> Result<()> {
        match &self.server {
            Some(server) => Err(format!(
                "{what} needs the secrets, which stay on the berth server at {}",
                server.url
            )
            .into()),
            None => Ok(()),
        }
    }

    pub fn registries(&self) -> Result<Map<String, Value>> {
        let settings = read(self.dir.join("config.json"))?;
        Ok(settings
            .get("registries")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default())
    }

    pub fn save_registries(&self, registries: Map<String, Value>) -> Result<()> {
        if let Some(server) = &self.server {
            let known = self.registries()?;
            for (name, registry) in &registries {
                if known.get(name) != Some(registry) {
                    server.call(
                        "registries.save",
                        json!({"name": name}),
                        Some(registry.clone()),
                    )?;
                }
            }
            for name in known.keys().filter(|name| !registries.contains_key(*name)) {
                server.call("registries.remove", json!({"name": name}), None)?;
            }
        }
        let mut settings = read(self.dir.join("config.json"))?;
        settings.insert("registries".into(), Value::Object(registries));
        write(self.dir.join("config.json"), &settings)
    }

    fn registry(&self, name: &str) -> Result<Value> {
        self.registries()?
            .remove(name)
            .ok_or_else(|| format!("No registry {name}. See: berth registry").into())
    }

    pub fn secrets(&self) -> Result<Map<String, Value>> {
        read(self.dir.join("secrets.json"))
    }

    /// The secrets the user named, without the passwords of registries.
    pub fn secret_names(&self) -> Vec<String> {
        self.secrets()
            .unwrap_or_default()
            .keys()
            .filter(|name| !name.starts_with("registry/"))
            .cloned()
            .collect()
    }

    pub fn secret(&self, name: &str) -> Result<Option<String>> {
        Ok(self
            .secrets()?
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_owned))
    }

    pub fn set_secret(&self, name: &str, value: Option<&str>) -> Result<()> {
        let mut secrets = self.secrets()?;
        let value = match (&self.server, value) {
            (Some(server), Some(value)) => {
                let data = Some(json!({"value": value}));
                server.call("secrets.set", json!({"name": name}), data)?;
                Some(KEPT)
            }
            (Some(server), None) => {
                if secrets.contains_key(name) {
                    server.call("secrets.remove", json!({"name": name}), None)?;
                }
                None
            }
            (None, value) => value,
        };
        match value {
            Some(value) => secrets.insert(name.into(), json!(value)),
            None => secrets.remove(name),
        };
        write(self.dir.join("secrets.json"), &secrets)
    }

    /// `user:password` of a registry in base64, when its login is known.
    fn auth(&self, name: &str, registry: &Value) -> Result<Option<String>> {
        let user = registry["username"].as_str().unwrap_or("");
        Ok(match self.secret(&format!("registry/{name}"))? {
            Some(password) if !user.is_empty() => {
                Some(base64(format!("{user}:{password}").as_bytes()))
            }
            _ => None,
        })
    }

    /// The stored secret of a registry without a user name, which is a token.
    fn token(&self, name: &str, registry: &Value) -> Result<Option<String>> {
        match registry["username"].as_str().unwrap_or("") {
            "" => self.secret(&format!("registry/{name}")),
            _ => Ok(None),
        }
    }

    /// The Docker addresses a registry offers, as (repository, address).
    fn docker_addresses(registry: &Value) -> Vec<(String, String)> {
        if registry["type"] == "docker" {
            return vec![(
                String::new(),
                bare(registry["url"].as_str().unwrap_or("")).to_owned(),
            )];
        }
        registry["repositories"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[])
            .iter()
            .filter(|repository| registry["type"] == "nexus" && repository["format"] == "docker")
            .map(|repository| {
                let name = repository["name"].as_str().unwrap_or("");
                (name.to_owned(), docker_address(registry, repository))
            })
            .collect()
    }

    /// What `devbox-secrets` on a host installs for a profile: the profile's
    /// secrets as environment variables and files, and the logins of its registries.
    pub fn host_payload(&self, profile: &Value) -> Result<Value> {
        self.own("Sending secrets to a host")?;
        let list = |value: &Value| value.as_array().cloned().unwrap_or_default();
        let mut env = Map::new();
        let mut files = vec![];
        for entry in list(&profile["secrets"]) {
            let name = entry["name"].as_str().unwrap_or("");
            let value = self.secret(name)?.ok_or_else(|| {
                format!("Secret {name} has no value. Set it: berth secret set {name}")
            })?;
            match (entry["env"].as_str(), entry["file"].as_str()) {
                (Some(variable), _) => {
                    env.insert(variable.into(), json!(value));
                }
                (None, Some(path)) => files.push(json!({
                    "path": path,
                    "content": value,
                    "mode": entry["mode"].as_str().unwrap_or("0600"),
                })),
                _ => return Err(format!("Secret {name} needs an env or a file").into()),
            }
        }
        let (mut npm, mut docker) = (vec![], vec![]);
        for name in list(&profile["registries"]) {
            let name = name.as_str().unwrap_or("");
            let registry = self.registry(name)?;
            let auth = self.auth(name, &registry)?;
            let token = self.token(name, &registry)?;
            if registry["type"] == "npm" {
                let url = format!(
                    "{}/",
                    registry["url"].as_str().unwrap_or("").trim_end_matches('/')
                );
                npm.push(json!({"registry": url, "auth": auth, "token": token}));
            }
            for repository in list(&registry["repositories"])
                .iter()
                .filter(|_| registry["type"] == "nexus")
            {
                if repository["format"] == "npm" {
                    let url = repository_url(&registry, repository["name"].as_str().unwrap_or(""));
                    npm.push(json!({"registry": url, "auth": auth, "token": token}));
                }
            }
            if let Some(auth) = &auth {
                for (_, address) in Self::docker_addresses(&registry) {
                    let entry = json!({"address": docker_host(&address), "auth": auth});
                    if !docker.contains(&entry) {
                        docker.push(entry);
                    }
                }
            }
        }
        Ok(json!({"env": env, "files": files, "npm": npm, "docker": docker}))
    }
}

/// A secret typed without echo, or read from a pipe.
pub fn read_secret(prompt: &str) -> Result<String> {
    let mut value = String::new();
    if std::io::stdin().is_terminal() {
        eprint!("{prompt}: ");
        let _ = Command::new("stty").arg("-echo").status();
        let read = std::io::stdin().read_line(&mut value);
        let _ = Command::new("stty").arg("echo").status();
        eprintln!();
        read?;
    } else {
        std::io::stdin().read_to_string(&mut value)?;
    }
    let value = value.trim_end_matches(['\n', '\r']).to_owned();
    if value.is_empty() {
        return Err("Nothing entered".into());
    }
    Ok(value)
}

fn run(mut command: Command, input: Option<String>) -> Result<()> {
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command.spawn()?;
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        stdin.write_all(input.as_bytes())?;
    }
    let status = child.wait()?;
    if !status.success() {
        let program = command.get_program().to_string_lossy().into_owned();
        return Err(format!("{program} failed ({status})").into());
    }
    Ok(())
}

/// The HTTP status of a GET, with the registry's login when it has one.
/// The login goes to curl on standard input.
fn http_status(url: &str, login: &Option<String>) -> std::result::Result<u16, String> {
    let quote = |text: &str| text.replace('\\', "\\\\").replace('"', "\\\"");
    let mut config = format!(
        "silent\nmax-time = 15\noutput = \"/dev/null\"\nwrite-out = \"%{{http_code}}\"\nurl = \"{}\"\n",
        quote(url)
    );
    if let Some(login) = login {
        config += &format!("{login}\n");
    }
    let mut child = Command::new("curl")
        .args(["--config", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("curl: {error}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(config.as_bytes());
    }
    let output = child
        .wait_with_output()
        .map_err(|error| error.to_string())?;
    match String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u16>()
    {
        Ok(0) | Err(_) => Err("no answer (address, network or certificate)".into()),
        Ok(code) => Ok(code),
    }
}

impl Store {
    /// A registry's login as a line of curl configuration, when one is stored.
    pub fn curl_login(&self, name: &str) -> Result<Option<String>> {
        self.own("Logging in to a registry")?;
        let registry = self.registry(name)?;
        let quote = |text: &str| text.replace('\\', "\\\\").replace('"', "\\\"");
        Ok(
            match (
                registry["username"].as_str(),
                self.secret(&format!("registry/{name}"))?,
            ) {
                (Some(user), Some(password)) => Some(format!(
                    "user = \"{}\"",
                    quote(&format!("{user}:{password}"))
                )),
                (None, Some(token)) => Some(format!(
                    "header = \"Authorization: Bearer {}\"",
                    quote(&token)
                )),
                _ => None,
            },
        )
    }
}

/// Check that a registry answers and accepts its login. Reads only: whether
/// the account may also upload is not tried. One line per thing checked,
/// starting with ✓, ! or ✗.
pub fn check(store: &Store, name: &str) -> Result<Vec<String>> {
    // A server's registry is checked from the server, which has its login.
    if let Some(server) = &store.server {
        let checked = server.call("registries.check", json!({"name": name}), None)?;
        return Ok(checked["lines"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|line| line.as_str().map(str::to_owned))
            .collect());
    }
    let registry = store.registry(name)?;
    let login = store.curl_login(name)?;
    let with_login = if login.is_some() {
        "with the stored login"
    } else {
        "without a login"
    };
    let url = registry["url"]
        .as_str()
        .unwrap_or("")
        .trim_end_matches('/')
        .to_owned();
    // What a status means for something that should be readable.
    let readable = |what: &str, status: std::result::Result<u16, String>| match status {
        Ok(200..=299) => format!("✓ {what}: readable {with_login}"),
        Ok(401) => format!("✗ {what}: login not accepted (HTTP 401)"),
        Ok(403) => format!("✗ {what}: this account may not read it (HTTP 403)"),
        Ok(404) => format!("✗ {what}: not found on the server (HTTP 404)"),
        Ok(code) => format!("! {what}: HTTP {code}"),
        Err(error) => format!("✗ {what}: {error}"),
    };
    let docker = |what: &str, address: &str| {
        let host = docker_host(address);
        match http_status(&format!("https://{host}/v2/"), &login) {
            Ok(200..=299) => format!("✓ {what}: Docker API answers {with_login} ({address})"),
            Ok(401) if login.is_some() => format!(
                "! {what}: reachable, login not accepted as sent. Registries that hand out tokens (ghcr.io, Docker Hub) cannot be checked this way."
            ),
            Ok(401) => format!("! {what}: reachable, wants a login ({address})"),
            Ok(code) => format!("! {what}: HTTP {code} at {host}/v2/"),
            Err(error) => format!("✗ {what}: {error}"),
        }
    };
    let mut lines = vec![];
    match registry["type"].as_str().unwrap_or("") {
        "nexus" => {
            match http_status(&format!("{url}/service/rest/v1/status"), &None) {
                Ok(200) => lines.push("✓ Server: up".to_owned()),
                Ok(code) => lines.push(format!("✗ Server: HTTP {code} from its status page")),
                Err(error) => {
                    lines.push(format!("✗ Server: {error}"));
                    return Ok(lines);
                }
            }
            if login.is_none() {
                lines.push("! No login stored: checked as an anonymous visitor".to_owned());
            }
            for repository in registry["repositories"]
                .as_array()
                .cloned()
                .unwrap_or_default()
            {
                let repo = repository["name"].as_str().unwrap_or("");
                let format = repository["format"].as_str().unwrap_or("");
                let what = format!("{repo} ({format})");
                let listing = format!("{url}/service/rest/v1/components?repository={repo}");
                lines.push(readable(&what, http_status(&listing, &login)));
                if format == "docker" {
                    lines.push(docker(&what, &docker_address(&registry, &repository)));
                }
            }
        }
        "npm" => {
            lines.push(readable(
                "Registry",
                http_status(&format!("{url}/-/ping"), &login),
            ));
            if login.is_some() {
                lines.push(match http_status(&format!("{url}/-/whoami"), &login) {
                    Ok(200) => "✓ Login: accepted".to_owned(),
                    Ok(code) => format!("! Login: HTTP {code} from /-/whoami"),
                    Err(error) => format!("✗ Login: {error}"),
                });
            }
        }
        "raw" => lines.push(match http_status(&format!("{url}/"), &login) {
            Ok(401) => "✗ Address: login not accepted (HTTP 401)".to_owned(),
            Ok(code) => format!("✓ Address: answers (HTTP {code}). Uploading is not tried."),
            Err(error) => format!("✗ Address: {error}"),
        }),
        _ => lines.push(docker("Registry", bare(&url))),
    }
    Ok(lines)
}

impl Store {
    /// The places a release archive can be published to: file servers by name,
    /// raw repositories of a Nexus server as `REGISTRY/REPOSITORY`.
    pub fn release_stores(&self) -> Vec<String> {
        let mut stores = vec![];
        for (name, registry) in self.registries().unwrap_or_default() {
            if registry["type"] == "raw" {
                stores.push(name.clone());
            }
            for repository in registry["repositories"]
                .as_array()
                .cloned()
                .unwrap_or_default()
            {
                if registry["type"] == "nexus" && repository["format"] == "raw" {
                    stores.push(format!(
                        "{name}/{}",
                        repository["name"].as_str().unwrap_or("")
                    ));
                }
            }
        }
        stores
    }
}

/// Lines describing the registries, for `berth registry` and the settings page.
pub fn describe(store: &Store) -> Result<Vec<String>> {
    let mut lines = vec![];
    for (name, registry) in store.registries()? {
        let login = match (
            registry["username"].as_str(),
            store.secret(&format!("registry/{name}"))?,
        ) {
            (Some(user), Some(_)) => format!("login {user}"),
            (Some(user), None) => format!("login {user}, no password yet"),
            _ => "no login".to_owned(),
        };
        let login = match (
            registry["username"].as_str(),
            store.token(&name, &registry)?,
        ) {
            (None, Some(_)) => "token".to_owned(),
            _ => login,
        };
        let kind = type_label(registry["type"].as_str().unwrap_or(""));
        lines.push(format!(
            "{name}  {kind}  {}  ({login})",
            registry["url"].as_str().unwrap_or("")
        ));
        let repositories = match registry["type"] == "nexus" {
            true => registry["repositories"]
                .as_array()
                .cloned()
                .unwrap_or_default(),
            false => vec![],
        };
        for (index, repository) in repositories.iter().enumerate() {
            let branch = if index + 1 < repositories.len() {
                "├─"
            } else {
                "└─"
            };
            let format = repository["format"].as_str().unwrap_or("");
            let repo = repository["name"].as_str().unwrap_or("");
            let place = match format {
                "docker" => docker_address(&registry, repository),
                _ => repository_url(&registry, repo),
            };
            lines.push(format!("  {branch} {format:<7}{repo}  {place}"));
        }
    }
    Ok(lines)
}

/// `berth registry ...`
pub fn registry_command(paths: &Paths, args: &[&str]) -> Result<()> {
    let store = Store::new(paths);
    let mut registries = store.registries()?;
    match *args {
        [] => {
            let lines = describe(&store)?;
            if lines.is_empty() {
                println!(
                    "No registries. Add one: berth registry add NAME {} URL [USERNAME]",
                    TYPES.join("|")
                );
            }
            lines.iter().for_each(|line| println!("{line}"));
            return Ok(());
        }
        ["add", name, kind, url] | ["add", name, kind, url, _] => {
            if !valid_name(name) || !TYPES.contains(&kind) {
                return Err(format!("Usage: berth registry add NAME {} URL [USERNAME]  (without a user name the login is a token)", TYPES.join("|")).into());
            }
            if kind != "docker" && !url.starts_with("https://") && !url.starts_with("http://") {
                return Err("This kind of registry needs its address with https://".into());
            }
            let mut registry = json!({"type": kind, "url": url.trim_end_matches('/')});
            if let Some(user) = args.get(4) {
                registry["username"] = json!(user);
            }
            // Adding again changes the address or user and keeps the repositories.
            if let Some(known) = registries.get(name) {
                registry["repositories"] = known["repositories"].clone();
            }
            registries.insert(name.to_owned(), registry);
        }
        ["repo", name, format, repository] | ["repo", name, format, repository, _] => {
            let registry = registries
                .get_mut(name)
                .filter(|registry| registry["type"] == "nexus")
                .ok_or_else(|| format!("{name} is not a Nexus registry"))?;
            if !FORMATS.contains(&format) {
                return Err(format!("Formats: {}", FORMATS.join(", ")).into());
            }
            let mut entry = json!({"format": format, "name": repository});
            if let Some(address) = args.get(4) {
                entry["address"] = json!(bare(address));
            }
            let mut repositories = registry["repositories"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            repositories.retain(|known| known["name"] != repository);
            repositories.push(entry);
            registry["repositories"] = json!(repositories);
        }
        ["check"] => {
            for name in registries.keys() {
                println!("{name}");
                check(&store, name)?
                    .iter()
                    .for_each(|line| println!("  {line}"));
            }
            return Ok(());
        }
        ["check", name] => {
            check(&store, name)?
                .iter()
                .for_each(|line| println!("{line}"));
            return Ok(());
        }
        ["login", name] => {
            store.registry(name)?;
            let password = read_secret(&format!("Password or token for {name}"))?;
            return store.set_secret(&format!("registry/{name}"), Some(&password));
        }
        ["remove", name] => {
            registries
                .remove(name)
                .ok_or_else(|| format!("No registry {name}"))?;
            store.set_secret(&format!("registry/{name}"), None)?;
        }
        _ => {
            return Err("Usage: berth registry [add NAME TYPE URL [USERNAME] | repo NAME FORMAT REPOSITORY [ADDRESS] | login NAME | check [NAME] | remove NAME]".into());
        }
    }
    store.save_registries(registries)
}

/// `berth secret ...`
pub fn secret_command(paths: &Paths, args: &[&str]) -> Result<()> {
    let store = Store::new(paths);
    match *args {
        [] => {
            for name in store.secrets()?.keys() {
                println!("{name}");
            }
            Ok(())
        }
        ["set", name] if valid_name(name) => {
            store.set_secret(name, Some(&read_secret(&format!("Value of {name}"))?))
        }
        ["remove", name] => store.set_secret(name, None),
        _ => Err("Usage: berth secret [set NAME | remove NAME]  (names: lower-case letters, digits, - and _)".into()),
    }
}

/// Split `REGISTRY` or `REGISTRY/REPOSITORY`, and find the registry.
fn target(store: &Store, target: &str) -> Result<(String, Value, String)> {
    let (name, repository) = target.split_once('/').unwrap_or((target, ""));
    Ok((
        name.to_owned(),
        store.registry(name)?,
        repository.to_owned(),
    ))
}

/// `berth publish REGISTRY/REPOSITORY`: put the release archive in a Nexus raw
/// repository. Returns the address it is served at.
pub fn publish(paths: &Paths, destination: &str) -> Result<String> {
    let dirs: Map<String, Value> =
        crate::plugins::load(&["plugins".into(), paths.config_dir.join("plugins")])
            .into_iter()
            .map(|plugin| (plugin.id, json!(plugin.dir)))
            .collect();
    publish_from(
        &Store::new(paths),
        &paths.build_dir(),
        &Value::Object(dirs),
        destination,
    )
}

/// The same with the build directory and plugins of an open host.
pub fn publish_from(
    store: &Store,
    build: &std::path::Path,
    plugins: &Value,
    destination: &str,
) -> Result<String> {
    let (name, registry, repository) = target(store, destination)?;
    let base = if registry["type"] == "raw" && repository.is_empty() {
        format!(
            "{}/",
            registry["url"].as_str().unwrap_or("").trim_end_matches('/')
        )
    } else if registry["type"] == "nexus"
        && registry["repositories"].as_array().is_some_and(|all| {
            all.iter()
                .any(|known| known["name"] == repository.as_str() && known["format"] == "raw")
        })
    {
        repository_url(&registry, &repository)
    } else {
        return Err(format!(
            "{destination} is not a file server or a raw repository. See: berth registry"
        )
        .into());
    };
    let mut release = Command::new("python3");
    release
        .args(["bin/devbox.py", "release"])
        .env("DEVBOX_BUILD_DIR", build);
    release.env("BERTH_PLUGINS", plugins.to_string());
    release.stdout(Stdio::null());
    run(release, None)?;
    let digest = fs::read_to_string(build.join("release.sha256"))?
        .trim()
        .to_owned();
    let url = format!(
        "{base}berth/devbox-release-{}.tar.gz",
        &digest[..16.min(digest.len())]
    );
    // The login goes to curl on standard input, not on its command line.
    let quote = |text: &str| text.replace('\\', "\\\\").replace('"', "\\\"");
    let mut config = format!(
        "fail\nsilent\nshow-error\nupload-file = \"{}\"\nurl = \"{}\"\n",
        quote(&build.join("devbox-release.tar.gz").to_string_lossy()),
        quote(&url)
    );
    if let (Some(user), Some(password)) = (
        registry["username"].as_str(),
        store.secret(&format!("registry/{name}"))?,
    ) {
        config += &format!("user = \"{}\"\n", quote(&format!("{user}:{password}")));
    } else if let Some(token) = store.token(&name, &registry)? {
        config += &format!("header = \"Authorization: Bearer {}\"\n", quote(&token));
    }
    let mut curl = Command::new("curl");
    curl.args(["--config", "-"]);
    run(curl, Some(config))?;
    Ok(url)
}

/// `berth image push REGISTRY[/REPOSITORY] [TAG]`: push the container image
/// built by the Docker action.
pub fn push_image(paths: &Paths, destination: &str, tag: &str) -> Result<String> {
    let store = Store::new(paths);
    let (name, registry, repository) = target(&store, destination)?;
    let address = Store::docker_addresses(&registry)
        .into_iter()
        .find(|(known, _)| *known == repository)
        .map(|(_, address)| address)
        .ok_or_else(|| {
            format!("{destination} is not a Docker registry or repository. See: berth registry")
        })?;
    if let (Some(user), Some(password)) = (
        registry["username"].as_str(),
        store.secret(&format!("registry/{name}"))?,
    ) {
        let mut login = Command::new("docker");
        login.args([
            "login",
            docker_host(&address),
            "--username",
            user,
            "--password-stdin",
        ]);
        run(login, Some(password))?;
    }
    let image = format!("{address}/berth-desktop:{tag}");
    let mut retag = Command::new("docker");
    retag.args(["tag", "berth-desktop", &image]);
    run(retag, None).map_err(
        |_| "No local image berth-desktop. Build it first with the Docker install method.",
    )?;
    let mut push = Command::new("docker");
    push.args(["push", &image]);
    run(push, None)?;
    Ok(image)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_pads() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"a"), "YQ==");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"user:pass"), "dXNlcjpwYXNz");
    }

    #[test]
    fn a_host_gets_its_secrets_and_the_logins_of_its_registries() {
        let home = tempfile::tempdir().unwrap();
        let store = Store::at(home.path().to_owned());
        let mut registries = Map::new();
        registries.insert(
            "nexus".into(),
            json!({"type": "nexus", "url": "https://nexus.example", "username": "ci", "repositories": [
                {"format": "npm", "name": "npm-group"},
                {"format": "raw", "name": "releases"},
                {"format": "docker", "name": "images"},
                {"format": "docker", "name": "docker-hosted", "address": "nexus.example:8083"}]}),
        );
        registries.insert(
            "hub".into(),
            json!({"type": "docker", "url": "https://ghcr.io", "username": "me"}),
        );
        registries.insert(
            "verdaccio".into(),
            json!({"type": "npm", "url": "https://npm.example"}),
        );
        store.save_registries(registries).unwrap();
        store.set_secret("registry/nexus", Some("pw")).unwrap();
        store.set_secret("registry/verdaccio", Some("tok")).unwrap();
        store.set_secret("github", Some("token")).unwrap();
        store.set_secret("cert", Some("PEM")).unwrap();
        let mode = fs::metadata(home.path().join("secrets.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);

        let profile = json!({
            "secrets": [{"name": "github", "env": "GH_TOKEN"}, {"name": "cert", "file": "~/.cert"}],
            "registries": ["nexus", "hub", "verdaccio"]});
        let auth = base64(b"ci:pw");
        assert_eq!(
            store.host_payload(&profile).unwrap(),
            json!({
                "env": {"GH_TOKEN": "token"},
                "files": [{"path": "~/.cert", "content": "PEM", "mode": "0600"}],
                "npm": [
                    {"registry": "https://nexus.example/repository/npm-group/", "auth": auth, "token": null},
                    {"registry": "https://npm.example/", "auth": null, "token": "tok"}],
                // The registry without a stored password gets no login.
                "docker": [
                    {"address": "nexus.example", "auth": auth},
                    {"address": "nexus.example:8083", "auth": auth}]})
        );
        // A secret without a value, or an unknown registry, stops the delivery.
        assert!(
            store
                .host_payload(&json!({"secrets": [{"name": "missing", "env": "X"}]}))
                .is_err()
        );
        assert!(
            store
                .host_payload(&json!({"registries": ["nope"]}))
                .is_err()
        );
        let lines = describe(&store).unwrap();
        assert!(
            lines[0]
                .starts_with("hub  Docker registry  https://ghcr.io  (login me, no password yet)")
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("└─ docker docker-hosted  nexus.example:8083"))
        );
    }
}
