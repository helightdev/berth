//! berth on a system other than Linux, where it is a client of a berth
//! serve elsewhere: every command but those of the server's own machine.

use crate::Result;
use crate::cli::args::{Global, Ssh};
use crate::cli::fail::{Fail, Result as Done};

/// Only berth on Linux answers for ssh.
pub fn askpass(_: &[String]) -> Result<bool> {
    Ok(false)
}

/// No server runs here: a command needs one named, and the screens ask for one.
pub fn server(_ask: bool) -> Result<Option<crate::Local>> {
    Ok(None)
}

/// No server runs in this process to give a sudo password to.
pub fn give_sudo_password(_: &str, _: &str) -> bool {
    false
}

/// Plain SSH needs the server's key, which is on the machine it runs on.
pub fn ssh(_: &Global, ssh: &Ssh) -> Done<()> {
    Err(Fail::refused(format!(
        "berth ssh logs in with the server's own key, on the Linux machine it runs on. berth session attach {} reaches the host through the server",
        ssh.host
    )))
}
