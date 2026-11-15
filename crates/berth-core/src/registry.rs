//! Registries and secrets: where packages and images come from and go to, and
//! the logins and tokens a host is given.
//!
//! Registries are settings of the tool, because a server is the same for
//! every host. A Sonatype Nexus server is one registry with its repositories
//! under it. Both are kept in berth's store: registries as their JSON,
//! secrets as their text, sealed when the store is encrypted.

use crate::{
    Result,
    paths::Paths,
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

/// Where a secret is kept.
enum Located {
    /// A global secret: its id when it is there, and its name.
    Global(Option<String>, String),
    /// A secret that belongs to something, by the name of its row.
    Owned(String),
    /// One that would belong to something there is not, and why.
    NoOwner(String),
}

/// The row a secret is kept in, as given by its name or id, or by the
/// address of a secret that belongs to something: `registry/REGISTRY`
/// for a registry's login, `ssh/HOST` and `sudo/HOST` for the passwords
/// berth logs in to a host with, and `provisioner/PROVISIONER/SECRET`
/// for a provisioner's own.
fn locate(store: &store::Store, scope: &str, given: &str) -> Result<Located> {
    let owned = |kind: &str, owner: &str, label: &str| -> Result<Located> {
        Ok(match store.find(kind, scope, owner)? {
            Some(found) => Located::Owned(format!("{}/{label}", found.id)),
            None => Located::NoOwner(format!(
                "No {} {owner}, which the secret {given} would belong to",
                store::noun(kind)
            )),
        })
    };
    if let Some(registry) = given.strip_prefix("registry/") {
        return owned(kind::REGISTRY, registry, "login");
    }
    for (prefix, label) in [("ssh/", "ssh"), ("sudo/", "sudo")] {
        if let Some(host) = given.strip_prefix(prefix) {
            // A profile in a file keeps its passwords by its path.
            if host.contains('/') || host.ends_with(".json") {
                return Ok(Located::Owned(given.to_owned()));
            }
            return owned(kind::PROFILE, host, label);
        }
    }
    // A provisioner is named by its name, its id or its type.
    if let Some((provisioner, secret)) = given
        .strip_prefix("provisioner/")
        .and_then(|rest| rest.rsplit_once('/'))
    {
        return Ok(match crate::provisioner::find_in(store, provisioner)? {
            Some(found) => Located::Owned(format!("{}/{secret}", found.id)),
            None => Located::NoOwner(format!(
                "No provisioner {provisioner}, which the secret {given} would belong to"
            )),
        });
    }
    Ok(match store.find(kind::SECRET, scope, given)? {
        Some(found) => Located::Global(Some(found.id), found.name),
        None => Located::Global(None, given.to_owned()),
    })
}

/// The value of a secret in a store that is open, by its name or id, or by
/// the address of one that belongs to something, such as `registry/NAME`.
pub fn read_secret_in(store: &store::Store, scope: &str, given: &str) -> Result<Option<String>> {
    let row = match locate(store, scope, given)? {
        Located::Global(Some(id), _) => id,
        Located::Owned(row) => row,
        Located::Global(None, _) | Located::NoOwner(_) => return Ok(None),
    };
    Ok(store
        .read(kind::SECRET, scope, &row)?
        .map(|value| String::from_utf8_lossy(&value).into_owned()))
}

/// Whether a name can be a global secret's: no address of one that
/// belongs to something, and not in the shape of an id.
pub fn valid_secret_name(name: &str) -> bool {
    !name.trim().is_empty() && !name.contains('/') && !berth_api::ids::looks_like_id(name)
}

/// The registries and secrets of this installation.
#[derive(Clone)]
pub struct Store {
    store: Arc<store::Store>,
    /// [`store::OWN`].
    scope: String,
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
    /// This installation's.
    pub fn new(paths: &Paths) -> Result<Self> {
        Ok(Store::own(store::shared(paths)?))
    }

    /// This installation's, in a store that is open: what `berth serve`
    /// keeps and sends to its hosts.
    pub fn own(store: Arc<store::Store>) -> Self {
        Store {
            store,
            scope: store::OWN.to_owned(),
        }
    }

    /// Whether the values are sealed in the store.
    pub fn encrypted(&self) -> bool {
        self.store.encrypted()
    }

    pub fn registries(&self) -> Result<Map<String, Value>> {
        self.store
            .labelled_entries(kind::REGISTRY, &self.scope)?
            .into_iter()
            .map(|(found, value)| {
                let registry = serde_json::from_slice(&value)
                    .map_err(|error| format!("Registry {} in the store: {error}", found.name))?;
                Ok((found.name, registry))
            })
            .collect()
    }

    /// The registries' names and ids, by name.
    pub fn registry_ids(&self) -> Result<Vec<store::Found>> {
        Ok(self.store.labels(kind::REGISTRY, &self.scope)?)
    }

    /// The registry a name or id gives.
    pub fn find_registry(&self, given: &str) -> Result<Option<store::Found>> {
        Ok(self.store.find(kind::REGISTRY, &self.scope, given)?)
    }

    /// Make these the registries, in one change of the store: one of a name
    /// kept keeps its id and login, a new one gets an id, and one left out
    /// goes with its login. Profiles that name a registry refer to its id.
    pub fn save_registries(&self, registries: Map<String, Value>) -> Result<()> {
        let before = self.registry_ids()?;
        let mut batch = store::Batch::default();
        for (name, registry) in &registries {
            let mut registry = registry.clone();
            if let Some(fields) = registry.as_object_mut() {
                fields.remove("id");
            }
            let id = match before.iter().find(|found| &found.name == name) {
                Some(found) => found.id.clone(),
                None => {
                    let id = store::new_id();
                    crate::profiles::swap_references(
                        &self.store,
                        &mut batch,
                        &self.scope,
                        kind::REGISTRY,
                        name,
                        &id,
                    )?;
                    id
                }
            };
            batch.write_labelled(
                kind::REGISTRY,
                &self.scope,
                &id,
                name,
                registry.to_string().into_bytes(),
            );
        }
        for gone in before
            .iter()
            .filter(|found| !registries.contains_key(&found.name))
        {
            batch
                .remove(kind::REGISTRY, &self.scope, &gone.id)
                .forget_owned(&self.scope, &gone.id);
            crate::profiles::swap_references(
                &self.store,
                &mut batch,
                &self.scope,
                kind::REGISTRY,
                &gone.id,
                &gone.name,
            )?;
        }
        Ok(self.store.apply(&batch)?)
    }

    /// Give a registry another name. Its login and the hosts that use it
    /// stay with it, as both refer to its id. Returns whether it has a login.
    pub fn rename_registry(&self, old: &str, new: &str) -> Result<bool> {
        let found = self
            .find_registry(old)?
            .ok_or_else(|| format!("No registry {old}"))?;
        if found.name == new {
            return Err(format!("{new} has that name already").into());
        }
        match self
            .store
            .relabel(kind::REGISTRY, &self.scope, &found.id, new)
        {
            Err(store::Error::Taken { .. }) => {
                return Err(format!("There is a registry {new} already").into());
            }
            result => result?,
        };
        self.has_secret(&format!("registry/{}", found.id))
    }

    fn registry(&self, name: &str) -> Result<Value> {
        let found = self
            .find_registry(name)?
            .ok_or_else(|| format!("No registry {name}. See: berth registry"))?;
        self.store
            .read_json(kind::REGISTRY, &self.scope, &found.id)?
            .ok_or_else(|| format!("No registry {name}. See: berth registry").into())
    }

    fn locate(&self, given: &str) -> Result<Located> {
        locate(&self.store, &self.scope, given)
    }

    /// The global secrets, without those that belong to something, by name.
    pub fn secret_ids(&self) -> Result<Vec<store::Found>> {
        Ok(self.store.labels(kind::SECRET, &self.scope)?)
    }

    /// The names of the global secrets, and the addresses of the secrets
    /// that belong to registries, hosts and provisioners, in order. Nothing
    /// is unsealed.
    pub fn all_secret_names(&self) -> Result<Vec<String>> {
        let mut names: Vec<String> = self
            .secret_ids()?
            .into_iter()
            .map(|found| found.name)
            .collect();
        let owners = [
            (kind::REGISTRY, "registry"),
            (kind::PROFILE, "host"),
            (kind::PROVISIONER_ENTRY, "provisioner"),
        ];
        let mut labels = std::collections::HashMap::new();
        for (kind, of) in owners {
            for found in self.store.labels(kind, &self.scope)? {
                labels.insert(found.id, (of, found.name));
            }
        }
        for row in self.store.names(kind::SECRET, &self.scope)? {
            match row.split_once('/') {
                Some((owner, label)) if berth_api::ids::is_id(owner) => {
                    match (labels.get(owner), label) {
                        (Some(("registry", name)), _) => names.push(format!("registry/{name}")),
                        (Some(("host", name)), label) => names.push(format!("{label}/{name}")),
                        (Some((_, name)), label) => {
                            names.push(format!("provisioner/{name}/{label}"))
                        }
                        (None, _) => {}
                    }
                }
                // A file's passwords, by its path.
                Some(_) => names.push(row),
                None => {}
            }
        }
        names.sort();
        names.dedup();
        Ok(names)
    }

    /// The secrets the user named, without the passwords of registries and
    /// those berth logs in to hosts with.
    pub fn secret_names(&self) -> Vec<String> {
        self.secret_ids()
            .unwrap_or_default()
            .into_iter()
            .map(|found| found.name)
            .collect()
    }

    /// Whether a secret has a value. Nothing is unsealed.
    pub fn has_secret(&self, name: &str) -> Result<bool> {
        Ok(match self.locate(name)? {
            Located::Global(id, _) => id.is_some(),
            Located::Owned(row) => self.store.names(kind::SECRET, &self.scope)?.contains(&row),
            Located::NoOwner(_) => false,
        })
    }

    pub fn secret(&self, name: &str) -> Result<Option<String>> {
        let row = match self.locate(name)? {
            Located::Global(Some(id), _) => id,
            Located::Owned(row) => row,
            Located::Global(None, _) | Located::NoOwner(_) => return Ok(None),
        };
        match self.store.read(kind::SECRET, &self.scope, &row)? {
            Some(value) => Ok(Some(
                String::from_utf8(value).map_err(|_| format!("Secret {name} is not text"))?,
            )),
            None => Ok(None),
        }
    }

    /// Set a secret, or forget it with `None`. A new global secret gets an
    /// id, and profiles that name it refer to that; one forgotten is named
    /// by them again.
    pub fn set_secret(&self, name: &str, value: Option<&str>) -> Result<()> {
        let mut batch = store::Batch::default();
        match (self.locate(name)?, value) {
            (Located::NoOwner(why), Some(_)) => return Err(why.into()),
            (Located::NoOwner(_), None) | (Located::Global(None, _), None) => return Ok(()),
            (Located::Owned(row), Some(value)) => {
                batch.write(kind::SECRET, &self.scope, &row, value.as_bytes().to_vec());
            }
            (Located::Owned(row), None) => {
                batch.remove(kind::SECRET, &self.scope, &row);
            }
            (Located::Global(Some(id), label), Some(value)) => {
                batch.write_labelled(
                    kind::SECRET,
                    &self.scope,
                    &id,
                    &label,
                    value.as_bytes().to_vec(),
                );
            }
            (Located::Global(None, label), Some(value)) => {
                if !valid_secret_name(&label) {
                    return Err(format!("Not a name for a secret: {label}").into());
                }
                let id = store::new_id();
                batch.write_labelled(
                    kind::SECRET,
                    &self.scope,
                    &id,
                    &label,
                    value.as_bytes().to_vec(),
                );
                crate::profiles::swap_references(
                    &self.store,
                    &mut batch,
                    &self.scope,
                    kind::SECRET,
                    &label,
                    &id,
                )?;
            }
            (Located::Global(Some(id), label), None) => {
                batch.remove(kind::SECRET, &self.scope, &id);
                crate::profiles::swap_references(
                    &self.store,
                    &mut batch,
                    &self.scope,
                    kind::SECRET,
                    &id,
                    &label,
                )?;
            }
        }
        Ok(self.store.apply(&batch)?)
    }

    /// Give a global secret another name. The profiles that hand it to
    /// hosts refer to its id, and go on doing so.
    pub fn rename_secret(&self, old: &str, new: &str) -> Result<()> {
        let Located::Global(Some(id), label) = self.locate(old)? else {
            return Err(format!(
                "No secret {old}. A secret that belongs to a registry, host or provisioner keeps its name"
            )
            .into());
        };
        if label == new {
            return Err(format!("{new} has that name already").into());
        }
        if !valid_secret_name(new) {
            return Err(format!("Not a name for a secret: {new}").into());
        }
        match self.store.relabel(kind::SECRET, &self.scope, &id, new) {
            Err(store::Error::Taken { .. }) => {
                Err(format!("There is a secret {new} already").into())
            }
            result => Ok(result.map(|_| ())?),
        }
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

/// Split `REGISTRY` or `REGISTRY/REPOSITORY`, and find the registry.
fn target(store: &Store, target: &str) -> Result<(String, Value, String)> {
    let (name, repository) = target.split_once('/').unwrap_or((target, ""));
    Ok((
        name.to_owned(),
        store.registry(name)?,
        repository.to_owned(),
    ))
}

/// `berth registry publish REGISTRY/REPOSITORY`: put the release archive in a Nexus raw
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

/// `berth registry push-image REGISTRY[/REPOSITORY] [TAG]`: push the container image
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
            "--username",
            user,
            "--password-stdin",
            "--",
            docker_host(&address),
        ]);
        run(login, Some(password))?;
    }
    let image = format!("{address}/berth-desktop:{tag}");
    let mut retag = Command::new("docker");
    retag.args(["tag", "--", "berth-desktop", &image]);
    run(retag, None).map_err(
        |_| "No local image berth-desktop. Build it first with the Docker install method.",
    )?;
    let mut push = Command::new("docker");
    push.args(["push", "--", &image]);
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
        // The login belongs to the registry, by its id.
        let nexus = store.find_registry("nexus").unwrap().unwrap().id;
        assert_eq!(
            opened
                .read(kind::SECRET, store::OWN, &format!("{nexus}/login"))
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
    fn a_host_is_not_sent_a_secret_without_a_value_nor_the_tailnets_api_key() {
        let (_home, opened) = opened();
        let store = Store::own(opened);
        let wanting = json!({"secrets": [{"name": "github", "env": "GH_TOKEN"}]});
        let said = store.host_payload(&wanting).unwrap_err().to_string();
        assert!(said.contains("github"), "{said}");
        store.set_secret("github", Some("t0ken")).unwrap();
        assert_eq!(
            store.host_payload(&wanting).unwrap()["env"]["GH_TOKEN"],
            "t0ken"
        );
        store
            .set_secret(berth_api::profile::TAILNET_API_KEY, Some("hskey"))
            .unwrap();
        let giving =
            json!({"secrets": [{"name": berth_api::profile::TAILNET_API_KEY, "env": "KEY"}]});
        let said = store.host_payload(&giving).unwrap_err().to_string();
        assert!(said.contains("never sent to a host"), "{said}");
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
}
