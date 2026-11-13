//! Registries and secrets: where packages and images come from and go to, and
//! the logins and tokens a host is given.
//!
//! Registries are settings of the tool, because a server is the same for
//! every host. A Sonatype Nexus server is one registry with its repositories
//! under it. Both are kept in berth's store: registries as their JSON,
//! secrets as their text, sealed when the store is encrypted. What is known
//! of a berth server's is kept under the server's scope, with its secret
//! values standing in as [`KEPT`].

use crate::{
    Result,
    paths::Paths,
    plugins::valid_name,
    server::{KEPT, Server},
    store::{self, kind},
};
use serde_json::{Map, Value, json};
use std::{
    fs,
    io::{IsTerminal, Read, Write},
    process::{Command, Stdio},
    sync::Arc,
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

/// The registries and secrets of this installation, or what is known of a
/// berth server's.
#[derive(Clone)]
pub struct Store {
    store: Arc<store::Store>,
    /// [`store::OWN`], or the scope of the server.
    scope: String,
    /// The berth serve these are the registries and secrets of. Changes go
    /// there, and the values of its secrets stay there.
    server: Option<Server>,
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
    /// Those berth uses: the server's when one is configured, else its own.
    pub fn new(paths: &Paths) -> Result<Self> {
        Ok(Store::of(store::shared(paths)?, Server::configured(paths)))
    }

    /// This installation's own, whatever server its settings name: what
    /// `berth serve` keeps and sends to its hosts.
    pub fn own(store: Arc<store::Store>) -> Self {
        Store::of(store, None)
    }

    /// What is known of `server`'s, or this installation's own without one.
    pub fn of(store: Arc<store::Store>, server: Option<Server>) -> Self {
        let scope = match &server {
            Some(server) => server.scope(),
            None => store::OWN.to_owned(),
        };
        Store {
            store,
            scope,
            server,
        }
    }

    /// The server whose store this is.
    pub fn server(&self) -> Option<&Server> {
        self.server.as_ref()
    }

    /// Whether the values are sealed in the store.
    pub fn encrypted(&self) -> bool {
        self.store.encrypted()
    }

    /// Fails for a server's store, whose secrets have their values there.
    fn own_values(&self, what: &str) -> Result<()> {
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
        self.store
            .entries(kind::REGISTRY, &self.scope)?
            .into_iter()
            .map(|(name, value)| {
                let registry = serde_json::from_slice(&value)
                    .map_err(|error| format!("Registry {name} in the store: {error}"))?;
                Ok((name, registry))
            })
            .collect()
    }

    /// The rows of the registries, for one change of the store.
    fn registry_rows(registries: &Map<String, Value>) -> Vec<(&str, &str, Vec<u8>)> {
        registries
            .iter()
            .map(|(name, registry)| {
                (
                    kind::REGISTRY,
                    name.as_str(),
                    registry.to_string().into_bytes(),
                )
            })
            .collect()
    }

    /// Send the changes of the registries to the server, when there is one.
    fn send_registries(&self, registries: &Map<String, Value>) -> Result<()> {
        if let Some(server) = &self.server {
            let known = self.registries()?;
            for (name, registry) in registries {
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
        Ok(())
    }

    /// Make these the registries, in one change of the store.
    pub fn save_registries(&self, registries: Map<String, Value>) -> Result<()> {
        self.send_registries(&registries)?;
        let rows = Self::registry_rows(&registries);
        Ok(self.store.replace(&self.scope, &[kind::REGISTRY], &rows)?)
    }

    /// Give a registry another name, with its stored login. Here it is one
    /// change of the store; a server keeps logins by the registry's name and
    /// drops the old one with it, so for a server's registry the login has
    /// to be given again. Returns whether the login went with the name.
    pub fn rename_registry(&self, old: &str, new: &str) -> Result<bool> {
        let mut registries = self.registries()?;
        let registry = registries
            .remove(old)
            .ok_or_else(|| format!("No registry {old}"))?;
        if registries.contains_key(new) {
            return Err(format!("There is a registry {new} already").into());
        }
        registries.insert(new.to_owned(), registry.clone());
        let (old_login, new_login) = (format!("registry/{old}"), format!("registry/{new}"));
        let login = match &self.server {
            Some(_) => None,
            None => self.store.read(kind::SECRET, &self.scope, &old_login)?,
        };
        self.send_registries(&registries)?;
        let mut writes = vec![(
            kind::REGISTRY,
            self.scope.as_str(),
            new,
            registry.to_string().into_bytes(),
        )];
        if let Some(login) = &login {
            writes.push((kind::SECRET, &self.scope, &new_login, login.clone()));
        }
        self.store.change(
            &writes,
            &[
                (kind::REGISTRY, &self.scope, old),
                (kind::SECRET, &self.scope, &old_login),
            ],
        )?;
        Ok(login.is_some() || self.server.is_none())
    }

    fn registry(&self, name: &str) -> Result<Value> {
        self.store
            .read_json(kind::REGISTRY, &self.scope, name)?
            .ok_or_else(|| format!("No registry {name}. See: berth registry").into())
    }

    /// The names of every secret, with the logins of registries, in order.
    /// Nothing is unsealed.
    pub fn all_secret_names(&self) -> Result<Vec<String>> {
        Ok(self.store.names(kind::SECRET, &self.scope)?)
    }

    /// The secrets the user named, without the passwords of registries and
    /// those berth logs in to hosts with.
    pub fn secret_names(&self) -> Vec<String> {
        self.all_secret_names()
            .unwrap_or_default()
            .into_iter()
            .filter(|name| {
                !["registry/", "ssh/", "sudo/"]
                    .iter()
                    .any(|kept| name.starts_with(kept))
            })
            .collect()
    }

    /// Whether a secret has a value, here or on the server. Nothing is unsealed.
    pub fn has_secret(&self, name: &str) -> Result<bool> {
        Ok(self.all_secret_names()?.iter().any(|known| known == name))
    }

    pub fn secret(&self, name: &str) -> Result<Option<String>> {
        match self.store.read(kind::SECRET, &self.scope, name)? {
            Some(value) => Ok(Some(
                String::from_utf8(value).map_err(|_| format!("Secret {name} is not text"))?,
            )),
            None => Ok(None),
        }
    }

    pub fn set_secret(&self, name: &str, value: Option<&str>) -> Result<()> {
        let value = match (&self.server, value) {
            (Some(server), Some(value)) => {
                let data = Some(json!({"value": value}));
                server.call("secrets.set", json!({"name": name}), data)?;
                Some(KEPT)
            }
            (Some(server), None) => {
                if self.has_secret(name)? {
                    server.call("secrets.remove", json!({"name": name}), None)?;
                }
                None
            }
            (None, value) => value,
        };
        match value {
            Some(value) => self
                .store
                .write(kind::SECRET, &self.scope, name, value.as_bytes())?,
            None => {
                self.store.forget(kind::SECRET, &self.scope, name)?;
            }
        }
        Ok(())
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
        self.own_values("Sending secrets to a host")?;
        let list = |value: &Value| value.as_array().cloned().unwrap_or_default();
        let mut env = Map::new();
        let mut files = vec![];
        for entry in list(&profile["secrets"]) {
            let name = entry["name"].as_str().unwrap_or("");
            if name == berth_api::profile::TAILNET_API_KEY {
                return Err(format!(
                    "{name} is the API key berth creates enrollment keys with, and is never sent to a host"
                )
                .into());
            }
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
        self.own_values("Logging in to a registry")?;
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
        let stored = store.has_secret(&format!("registry/{name}"))?;
        let login = match (registry["username"].as_str(), stored) {
            (Some(user), true) => format!("login {user}"),
            (Some(user), false) => format!("login {user}, no password yet"),
            (None, true) => "token".to_owned(),
            (None, false) => "no login".to_owned(),
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
    let store = Store::new(paths)?;
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
    let store = Store::new(paths)?;
    match *args {
        [] => {
            for name in store.all_secret_names()? {
                println!("{name}");
            }
            Ok(())
        }
        // A host's SSH or sudo password is kept under the host's name.
        ["set", name]
            if valid_name(name)
                || name
                    .strip_prefix("ssh/")
                    .or_else(|| name.strip_prefix("sudo/"))
                    .is_some_and(crate::profiles::valid_name) =>
        {
            store.set_secret(name, Some(&read_secret(&format!("Value of {name}"))?))
        }
        ["remove", name] => store.set_secret(name, None),
        _ => Err("Usage: berth secret [set NAME | remove NAME]  (names: lower-case letters, digits, - and _; ssh/HOST and sudo/HOST are a host's SSH and sudo passwords)".into()),
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
        &Store::new(paths)?,
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
        .args([crate::paths::layout::BUILDER, "release"])
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
    let store = Store::new(paths)?;
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

    fn opened() -> (tempfile::TempDir, Arc<store::Store>) {
        let home = tempfile::tempdir().unwrap();
        let store = store::Store::open(&home.path().join("berth.db")).unwrap();
        (home, Arc::new(store))
    }

    #[test]
    fn a_host_gets_its_secrets_and_the_logins_of_its_registries() {
        let (_home, opened) = opened();
        let store = Store::own(opened.clone());
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
        assert_eq!(
            opened
                .read(kind::SECRET, store::OWN, "registry/nexus")
                .unwrap()
                .unwrap(),
            b"pw"
        );
        assert_eq!(store.secret_names(), ["cert", "github"]);

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

    #[test]
    fn a_renamed_registry_keeps_its_login() {
        let (_home, opened) = opened();
        let store = Store::own(opened);
        let mut registries = Map::new();
        registries.insert(
            "old".into(),
            json!({"type": "npm", "url": "https://npm.example", "username": "ci"}),
        );
        registries.insert("other".into(), json!({"type": "docker", "url": "ghcr.io"}));
        store.save_registries(registries).unwrap();
        store.set_secret("registry/old", Some("pw")).unwrap();
        assert!(store.rename_registry("old", "other").is_err());
        assert!(store.rename_registry("old", "new").unwrap());
        let names: Vec<_> = store.registries().unwrap().keys().cloned().collect();
        assert_eq!(names, ["new", "other"]);
        assert_eq!(store.secret("registry/new").unwrap().as_deref(), Some("pw"));
        assert!(!store.has_secret("registry/old").unwrap());
        // Saving the registries replaces them all, in one change.
        let mut registries = Map::new();
        registries.insert(
            "only".into(),
            json!({"type": "raw", "url": "https://files.example"}),
        );
        store.save_registries(registries).unwrap();
        let names: Vec<_> = store.registries().unwrap().keys().cloned().collect();
        assert_eq!(names, ["only"]);
    }

    #[test]
    fn a_servers_scope_is_apart_and_holds_no_values() {
        let (_home, opened) = opened();
        let own = Store::own(opened.clone());
        own.set_secret("github", Some("ghp-made-up")).unwrap();
        // A server that does not answer: nothing is written for it.
        let server = Server {
            url: "unix:/nonexistent/serve.sock".into(),
            token: None,
            paths: None,
        };
        let served = Store::of(opened.clone(), Some(server.clone()));
        assert!(served.set_secret("deploy", Some("value")).is_err());
        assert!(served.all_secret_names().unwrap().is_empty());
        // What a mirror wrote is seen there, and not here.
        opened
            .replace(
                &server.scope(),
                &[kind::SECRET, kind::REGISTRY],
                &[
                    (kind::SECRET, "deploy", KEPT.as_bytes().to_vec()),
                    (
                        kind::REGISTRY,
                        "hub",
                        br#"{"type":"docker","url":"ghcr.io"}"#.to_vec(),
                    ),
                ],
            )
            .unwrap();
        assert_eq!(served.secret_names(), ["deploy"]);
        assert_eq!(served.secret("deploy").unwrap().as_deref(), Some(KEPT));
        assert!(served.registries().unwrap().contains_key("hub"));
        assert!(served.host_payload(&json!({})).is_err());
        assert_eq!(own.secret_names(), ["github"]);
        assert!(own.registries().unwrap().is_empty());
        assert_eq!(
            own.secret("github").unwrap().as_deref(),
            Some("ghp-made-up")
        );
    }
}
