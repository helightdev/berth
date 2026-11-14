//! What the screens need from the device they run on: the clipboard, links,
//! files, what is kept between runs, plain HTTP and the portal's socket. The program installs its
//! [`Device`] before it makes the [`App`](crate::App).

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

/// What the portal's socket says, handed over on the thread that draws.
pub enum Said {
    Open,
    Text(String),
    Bytes(Vec<u8>),
    /// The socket is closed, or never opened.
    Closed,
}

/// An open socket; dropping it closes it.
pub trait Socket {
    fn send_text(&self, text: &str) -> bool;
    fn send_bytes(&self, bytes: &[u8]) -> bool;
}

pub type Answer = Box<dyn FnOnce(Result<(u16, String), String>)>;

pub trait Device {
    fn copy(&self, text: &str) -> bool;
    /// What is on the clipboard, where it can be read right away.
    fn paste(&self) -> Option<String>;
    fn open(&self, link: &str) -> Result<(), String>;
    /// Let the user choose a file whose name ends with one of `accept`
    /// (comma-separated), and hand over its name and bytes. Nothing happens
    /// when none is chosen; an error says why this device cannot ask.
    fn choose_file(
        &self,
        accept: &str,
        then: Box<dyn FnOnce(String, Vec<u8>)>,
    ) -> Result<(), String>;
    fn stored(&self, key: Kept) -> Option<String>;
    /// Keep `value`, or forget what was kept when it is empty.
    fn store(&self, key: Kept, value: &str);
    fn linked(&self) -> Linked;
    /// One plain HTTP request to `path` of `server` with a bearer; `then`
    /// gets its status and body.
    fn request(&self, method: &str, server: &str, path: &str, bearer: &str, then: Answer);
    /// Open a WebSocket to `path` of `server` with `token` as its protocol.
    /// `said` gets what it says, on the thread that draws, from a later turn on.
    fn connect(
        &self,
        server: &str,
        path: &str,
        token: &str,
        said: Box<dyn Fn(Said)>,
    ) -> Result<Box<dyn Socket>, String>;
}

thread_local! {
    static DEVICE: RefCell<Option<Rc<dyn Device>>> = const { RefCell::new(None) };
}

/// Make `device` the one the screens use on this thread.
pub fn install(device: impl Device + 'static) {
    DEVICE.set(Some(Rc::new(device)));
}

pub(crate) fn device() -> Rc<dyn Device> {
    DEVICE
        .with_borrow(Clone::clone)
        .expect("the program installs its device before the screens start")
}

/// The device as the shared screens of berth-tui take it.
pub(crate) struct Here;

impl Platform for Here {
    fn copy(&mut self, text: &str) -> bool {
        device().copy(text)
    }

    fn paste(&mut self) -> Option<String> {
        device().paste()
    }

    fn open(&mut self, link: &str) -> Result<(), String> {
        device().open(link)
    }
}

pub(crate) fn linked() -> Linked {
    device().linked()
}

pub(crate) fn stored(key: Kept) -> Option<String> {
    device().stored(key).filter(|value| !value.is_empty())
}

pub(crate) fn store(key: Kept, value: &str) {
    device().store(key, value)
}

pub(crate) fn choose_file(
    accept: &str,
    then: impl FnOnce(String, Vec<u8>) + 'static,
) -> Result<(), String> {
    device().choose_file(accept, Box::new(then))
}

pub(crate) fn request(
    method: &str,
    server: &str,
    path: &str,
    bearer: &str,
    then: impl FnOnce(Result<(u16, String), String>) + 'static,
) {
    device().request(method, server, path, bearer, Box::new(then))
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
