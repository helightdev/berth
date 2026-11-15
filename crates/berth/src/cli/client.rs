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
    portal: Portal,
    finish: Option<Finish>,
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
        let portal = connect(&url, &token)?;
        Ok(Client {
            url,
            portal,
            finish,
        })
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

    /// One operation of a host's controller, through the server.
    pub fn on(&self, host: &str, op: &str, args: Value, data: Option<Value>) -> Result<Value> {
        self.wait(self.portal.call(op, Some(host), args, data))
            .map_err(Fail::from)
    }

    /// Open a stream of the server, or of a host's controller.
    pub fn stream(&self, op: &str, host: Option<&str>, args: Value) -> Result<PortalStream> {
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
        let (op, args) = match subject {
            Subject::Host(name) => ("hosts.act", json!({"name": name, "action": action})),
            Subject::Provisioner(name) => {
                ("provisioners.act", json!({"name": name, "action": action}))
            }
            Subject::Resource(id) => ("resources.act", json!({"id": id, "action": action})),
            Subject::Installation => ("installation.act", json!({"action": action})),
        };
        let acted = self.call(op, args, Some(serde_json::to_value(act)?))?;
        Ok(serde_json::from_value(acted)?)
    }

    /// The actions of a subject.
    pub fn actions(&self, subject: &Subject) -> Result<Vec<berth_api::Action>> {
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
