//! What the screens take from the program and the system they run on: what
//! the program was started with, what is kept between runs, the clipboard,
//! links, files, and plain HTTP.

use crate::settings::{self, Settings};
use crate::{net, terminal};
use berth_tui::screen::Platform;
use std::cell::RefCell;
use std::rc::Rc;

/// What is kept between runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kept {
    /// The server the screens were last signed in to.
    Server,
    /// The token for that server.
    Token,
}

/// What the program was started with.
#[derive(Clone, Default)]
pub struct Linked {
    pub server: Option<String>,
    pub token: Option<String>,
    /// A key that collects the token, once, after an install.
    pub setup_key: Option<String>,
}

impl Linked {
    /// What the environment `var` gives names: `BERTH_SERVER` and
    /// `BERTH_TOKEN` this once, and `BERTH_SETUP_KEY`. Secrets come from the
    /// environment, not the command line, where other users could read them.
    pub fn from(var: impl Fn(&str) -> Option<String>) -> Linked {
        let variable = |name: &str| {
            var(name)
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };
        Linked {
            server: variable("BERTH_SERVER"),
            token: variable("BERTH_TOKEN"),
            setup_key: variable("BERTH_SETUP_KEY"),
        }
    }

    /// Whether no server is named, here or in `settings`, so that the
    /// screens use one on this machine.
    pub fn local(&self, settings: &Settings) -> bool {
        self.server.is_none() && settings.get(settings::SERVER).is_none()
    }
}

/// What the screens keep between runs, and what the program was started
/// with. The program installs it before it makes the [`App`](crate::App).
struct Device {
    linked: Linked,
    settings: Settings,
}

thread_local! {
    static DEVICE: RefCell<Option<Rc<Device>>> = const { RefCell::new(None) };
}

/// Start the screens of this thread with `linked`, keeping the server and
/// its token in `settings`.
pub fn install(linked: Linked, settings: Settings) {
    DEVICE.set(Some(Rc::new(Device { linked, settings })));
}

fn device() -> Rc<Device> {
    DEVICE
        .with_borrow(Clone::clone)
        .expect("the program installs its device before the screens start")
}

/// The system as the shared screens of berth-tui take it.
pub(crate) struct Here;

impl Platform for Here {
    fn copy(&mut self, text: &str) -> bool {
        terminal::copy_text(text)
    }

    fn paste(&mut self) -> Option<String> {
        terminal::paste_text()
    }

    fn open(&mut self, link: &str) -> Result<(), String> {
        terminal::open(link)
    }
}

pub(crate) fn linked() -> Linked {
    device().linked.clone()
}

fn setting(key: Kept) -> &'static str {
    match key {
        Kept::Server => settings::SERVER,
        Kept::Token => settings::TOKEN,
    }
}

pub(crate) fn stored(key: Kept) -> Option<String> {
    device().settings.get(setting(key))
}

/// Keep `value`, or forget what was kept when it is empty. The screens go
/// on without it when it cannot be written: the next run asks again.
pub(crate) fn store(key: Kept, value: &str) {
    let _ = device().settings.set(&[(setting(key), value)]);
}

pub(crate) fn choose_file(
    accept: &str,
    then: impl FnOnce(String, Vec<u8>) + 'static,
) -> Result<(), String> {
    terminal::choose_file(accept, then)
}

/// One plain HTTP request to `path` of `server` with a bearer; `then` gets
/// its status and body at a later turn.
pub(crate) fn request(
    method: &str,
    server: &str,
    path: &str,
    bearer: &str,
    then: impl FnOnce(Result<(u16, String), String>) + 'static,
) {
    let (method, server, path, bearer) = (
        method.to_owned(),
        server.to_owned(),
        path.to_owned(),
        bearer.to_owned(),
    );
    net::later(
        async move { net::http(&method, &server, &path, &bearer).await },
        then,
    );
}

/// The server that runs in the program itself, as the screens name it.
pub const HERE: &str = "here:";

/// Whether the server is on this machine, reached without a token: the one
/// in the program, or the socket of a `berth serve` of this user.
pub fn local(server: &str) -> bool {
    server == HERE || server.starts_with("unix:")
}

/// A server's address as typed, made whole: without a scheme it is https,
/// or http for this machine itself.
pub fn address(typed: &str) -> String {
    let typed = typed.trim().trim_end_matches('/');
    if typed.is_empty()
        || typed.starts_with("http://")
        || typed.starts_with("https://")
        || local(typed)
    {
        return typed.to_owned();
    }
    let local = ["localhost", "127.", "[::1]"]
        .iter()
        .any(|name| typed.starts_with(name));
    format!("{}://{typed}", if local { "http" } else { "https" })
}

/// A server as it is shown: `host:port`, or where on this machine it is.
pub fn shown(server: &str) -> &str {
    if server == HERE {
        return "this machine";
    }
    if server.starts_with("unix:") {
        return "berth serve on this machine";
    }
    server
        .trim_start_matches("https://")
        .trim_start_matches("http://")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_server_named_or_kept_is_used_and_else_one_here() {
        let scratch = tempfile::tempdir().unwrap();
        let settings = Settings::new(Some(scratch.path().to_owned()));
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value.to_string())
            }
        };
        assert!(Linked::from(env(&[])).local(&settings));
        assert!(Linked::from(env(&[("BERTH_SERVER", " ")])).local(&settings));
        let named = Linked::from(env(&[
            ("BERTH_SERVER", "https://studio.example"),
            ("BERTH_TOKEN", "s3cret\n"),
        ]));
        assert!(!named.local(&settings));
        assert_eq!(named.token.as_deref(), Some("s3cret"));
        settings
            .set(&[(settings::SERVER, "http://192.0.2.10:7878")])
            .unwrap();
        assert!(!Linked::from(env(&[])).local(&settings));
    }

    #[test]
    fn addresses() {
        assert_eq!(
            address("studio.example:8080"),
            "https://studio.example:8080"
        );
        assert_eq!(address("127.0.0.1:7878/"), "http://127.0.0.1:7878");
        assert_eq!(address(" https://192.0.2.10/ "), "https://192.0.2.10");
        assert_eq!(shown("https://studio.example:8080"), "studio.example:8080");
        assert_eq!(address("here:"), "here:");
        assert_eq!(
            address("unix:/run/berth/serve.sock"),
            "unix:/run/berth/serve.sock"
        );
        assert!(local(HERE) && !local("https://studio.example"));
        assert_eq!(shown(HERE), "this machine");
    }
}
