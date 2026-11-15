//! The server a command talks to, the same way the screens do: the one
//! named by `--server` or `BERTH_SERVER`, the one `berth login` kept, a
//! `berth serve` of this user on this machine, or one in this process for
//! as long as the command runs. Every command is a call of its API.

use super::args::Global;
use super::fail::{Fail, Result};
use berth_api::portal::{Portal, PortalStream};
use berth_page::net;
use berth_page::settings::{self, Settings};
use serde_json::{Value, json};
use std::future::Future;

/// What is done when the command ends: the server in this process waits for
/// the jobs it runs.
type Finish = Box<dyn FnOnce()>;

pub struct Client {
    /// The server's address, as `--server` takes it.
    pub url: String,
    /// The token it was opened with, for a request over HTTP and to open it again.
    token: String,
    portal: Portal,
    finish: Option<Finish>,
    /// The hosts whose sudo password the server in this process was given
    /// or found it needs none, in this run.
    sudo_ready: std::cell::RefCell<std::collections::BTreeSet<String>>,
}

/// The settings of this user that `berth login` keeps.
pub fn settings() -> Settings {
    Settings::new(settings::folder(|name| std::env::var(name).ok()))
}

/// Take in what berth-remote kept, once, before a server is looked for, and
/// say what became of it. Nothing is lost: its file goes only once berth has
/// what was in it.
pub fn take_in_remote(kept: &Settings, old: Option<&std::path::Path>) {
    use super::out;
    match kept.take_in(old) {
        Ok(settings::TakenIn::Taken(server)) => {
            out::note(format!("berth-remote's server is berth's now: {server}"))
        }
        Ok(settings::TakenIn::Left { theirs, ours, file }) => out::note(format!(
            "berth-remote's settings in {} name the server {theirs}, and berth uses {ours}: the file is left as it is. To use {theirs}: berth login {theirs}",
            file.display()
        )),
        Ok(settings::TakenIn::Nothing | settings::TakenIn::Same) => {}
        Err(error) => out::warn(format!(
            "berth-remote's settings were not taken in, and are left as they are: {error}"
        )),
    }
}

/// berth-remote's folder of settings for this user.
pub fn remote_folder() -> Option<std::path::PathBuf> {
    settings::old_folder(|name| std::env::var(name).ok())
}

fn variable(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// The token of this run: from a file, standard input, `BERTH_TOKEN`, or
/// the one `berth login` kept for that server.
fn token(global: &Global, url: &str, kept: &Settings) -> Result<String> {
    if let Some(file) = &global.token_file {
        let text = std::fs::read_to_string(file)
            .map_err(|error| Fail::usage(format!("--token-file {}: {error}", file.display())))?;
        return Ok(text.trim().to_owned());
    }
    if global.token_stdin {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)?;
        return Ok(text.trim().to_owned());
    }
    if let Some(token) = variable("BERTH_TOKEN") {
        return Ok(token);
    }
    let same = kept
        .get(settings::SERVER)
        .is_some_and(|server| server.trim_end_matches('/') == url);
    Ok(match same {
        true => kept.get(settings::TOKEN).unwrap_or_default(),
        false => String::new(),
    })
}

/// The server this run uses, if one is named: by a flag, the environment,
/// or the settings.
pub fn named(global: &Global) -> Option<String> {
    named_in(global, &settings())
}

fn named_in(global: &Global, kept: &Settings) -> Option<String> {
    global
        .server
        .clone()
        .filter(|server| !server.trim().is_empty())
        .or_else(|| variable("BERTH_SERVER"))
        .or_else(|| kept.get(settings::SERVER))
        .map(|server| berth_page::device::address(&server))
}

impl Client {
    /// Reach the server this run uses.
    pub fn open(global: &Global) -> Result<Client> {
        let kept = settings();
        take_in_remote(&kept, remote_folder().as_deref());
        Client::open_in(global, &kept)
    }

    /// Reach the server this run uses, with these settings.
    pub fn open_in(global: &Global, kept: &Settings) -> Result<Client> {
        let (url, token, finish) = match named_in(global, kept) {
            Some(url) => {
                let token = token(global, &url, kept)?;
                (url, token, None)
            }
            None => match crate::local::server(false)? {
                Some((url, finish)) => (url, String::new(), Some(finish)),
                None => {
                    return Err(Fail::unreachable(
                        "No berth server is set. Name one with berth login URL, or with --server URL for one command",
                    ));
                }
            },
        };
        let in_process = finish.is_some();
        let client = Client {
            portal: connect(&url, &token)?,
            url,
            token,
            finish,
            sudo_ready: Default::default(),
        };
        // One in this process is this berth; any other may be older.
        if !in_process {
            let (version, api) = client.server_version()?;
            if let Some(older) = berth_api::compat::older_server(&client.url, &version, api) {
                return Err(Fail::refused(older));
            }
        }
        Ok(client)
    }

    /// Reach the server this run uses, whatever its version, only to ask it.
    pub fn open_any(global: &Global, kept: &Settings) -> Result<Client> {
        let url =
            named_in(global, kept).ok_or_else(|| Fail::unreachable("No berth server is set"))?;
        let token = token(global, &url, kept)?;
        Ok(Client {
            portal: connect(&url, &token)?,
            url,
            token,
            finish: None,
            sudo_ready: Default::default(),
        })
    }

    /// The same server again, as when it was replaced and answers anew.
    pub fn reopen(&self) -> Result<Client> {
        Ok(Client {
            portal: connect(&self.url, &self.token)?,
            url: self.url.clone(),
            token: self.token.clone(),
            finish: None,
            sudo_ready: Default::default(),
        })
    }

    /// The server's version and the number of its API.
    pub fn server_version(&self) -> Result<(String, u32)> {
        let said = self.call("version", json!({}), None)?;
        let version = said["version"].as_str().unwrap_or("").to_owned();
        let api = said["api"].as_u64().unwrap_or(0) as u32;
        Ok((version, api))
    }

    /// Whether the server runs in this process, and ends with it.
    pub fn in_process(&self) -> bool {
        self.url == berth_page::device::HERE
    }

    pub fn wait<F: Future>(&self, future: F) -> F::Output {
        net::runtime().block_on(future)
    }

    /// One operation of the server, waited for.
    pub fn call(&self, op: &str, args: Value, data: Option<Value>) -> Result<Value> {
        self.wait(self.portal.call(op, None, args, data))
            .map_err(Fail::from)
    }

    /// Before the server in this process reaches `host`, give it the host's
    /// sudo password when its sudo asks for one and no secret `sudo/HOST`
    /// keeps it: from `--sudo-password-stdin`, or asked once on the terminal.
    /// A server elsewhere has its own.
    fn sudo_for(&self, host: &str) -> Result<()> {
        if !self.in_process() || !self.sudo_ready.borrow_mut().insert(host.to_owned()) {
            return Ok(());
        }
        if let Some(password) = super::host::sudo_password(self, host)? {
            crate::local::give_sudo_password(host, &password);
        }
        Ok(())
    }

    /// One operation of a host's controller, through the server.
    pub fn on(&self, host: &str, op: &str, args: Value, data: Option<Value>) -> Result<Value> {
        self.sudo_for(host)?;
        self.wait(self.portal.call(op, Some(host), args, data))
            .map_err(Fail::from)
    }

    /// Open a stream of the server, or of a host's controller.
    pub fn stream(&self, op: &str, host: Option<&str>, args: Value) -> Result<PortalStream> {
        if let Some(host) = host {
            self.sudo_for(host)?;
        }
        self.wait(self.portal.open(op, host, args))
            .map_err(Fail::from)
    }

    pub fn portal(&self) -> &Portal {
        &self.portal
    }

    /// Run an action of a subject: a host, a provisioner, a resource, or
    /// the installation.
    pub fn act(
        &self,
        subject: &Subject,
        action: &str,
        act: &berth_api::Act,
    ) -> Result<berth_api::Acted> {
        if let Subject::Host(name) = subject {
            self.sudo_for(name)?;
        }
        let (op, args) = match subject {
            Subject::Host(name) => ("hosts.act", json!({"name": name, "action": action})),
            Subject::Provisioner(name) => {
                ("provisioners.act", json!({"name": name, "action": action}))
            }
            Subject::Resource(id) => ("resources.act", json!({"id": id, "action": action})),
            Subject::Installation => ("installation.act", json!({"action": action})),
        };
        let large = act
            .input_b64
            .as_ref()
            .is_some_and(|input| input.len() > berth_api::ops::PORTAL_BYTES);
        if let (true, Subject::Host(name)) = (large, subject) {
            return self.act_over_http(name, action, act);
        }
        let acted = self.call(op, args, Some(serde_json::to_value(act)?))?;
        Ok(serde_json::from_value(acted)?)
    }

    /// Run a host's action with more input than the portal carries: the
    /// same request, over HTTP.
    fn act_over_http(
        &self,
        host: &str,
        action: &str,
        act: &berth_api::Act,
    ) -> Result<berth_api::Acted> {
        let path = format!(
            "/v1/hosts/{}/actions/{}",
            berth_api::client::encode(host),
            berth_api::client::encode(action)
        );
        let body = serde_json::to_vec(act)?;
        let answer = self
            .wait(net::send("POST", &self.url, &path, &self.token, body))
            .map_err(Fail::unreachable)?;
        answer.json().map_err(Fail::from)
    }

    /// The actions of a subject.
    pub fn actions(&self, subject: &Subject) -> Result<Vec<berth_api::Action>> {
        if let Subject::Host(name) = subject {
            self.sudo_for(name)?;
        }
        let (op, args) = match subject {
            Subject::Host(name) => ("hosts.actions", json!({"name": name})),
            Subject::Provisioner(name) => ("provisioners.actions", json!({"name": name})),
            Subject::Resource(id) => ("resources.actions", json!({"id": id})),
            Subject::Installation => ("installation.actions", json!({})),
        };
        Ok(serde_json::from_value(self.call(op, args, None)?)?)
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Some(finish) = self.finish.take() {
            finish();
        }
    }
}

/// What an action is done to.
#[derive(Debug, Clone, PartialEq)]
pub enum Subject {
    Host(String),
    Provisioner(String),
    Resource(String),
    Installation,
}

impl Subject {
    /// How a job of it is named in its record, as `host:studio`.
    pub fn shown(&self) -> String {
        match self {
            Subject::Host(name) => name.clone(),
            Subject::Provisioner(name) => format!("provisioner {name}"),
            Subject::Resource(id) => format!("resource {id}"),
            Subject::Installation => "the installation".into(),
        }
    }
}

/// Open the portal of the server at `url` with `token`.
pub fn connect(url: &str, token: &str) -> Result<Portal> {
    let target = net::target(url, berth_api::ops::PORTAL).map_err(Fail::usage)?;
    net::runtime().block_on(async {
        let stream = net::connect(&target).await.map_err(|error| {
            Fail::unreachable(format!("The berth server at {url} cannot be reached: {error}"))
        })?;
        let socket = berth_api::client::websocket_at(stream, &target.authority, &target.path, token)
            .await
            .map_err(|error| match error.status() {
                401 | 403 => Fail::refused(format!(
                    "The berth server at {url} did not take the token. Log in again with: berth login {url}"
                )),
                _ => Fail::unreachable(format!(
                    "The berth server at {url} cannot be reached: {}",
                    net::describe(&error)
                )),
            })?;
        Ok(Portal::over(socket))
    })
}
