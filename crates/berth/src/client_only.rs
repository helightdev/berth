//! berth on a system other than Linux, where it is a client of a berth
//! serve elsewhere: the screens, `berth connect` and `berth update`.

use crate::Result;

/// Only berth on Linux answers for ssh.
pub fn askpass(_: &[&str]) -> Result<bool> {
    Ok(false)
}

/// No server runs here: the screens ask for one and its token.
pub fn server() -> Result<Option<crate::Local>> {
    Ok(None)
}

/// The commands that need an installation are berth's on Linux, beside its
/// server.
pub fn command(args: &[&str]) -> Result<()> {
    match args {
        ["help" | "--help" | "-h", ..] => {
            println!("{}", crate::USAGE);
            Ok(())
        }
        [verb, ..] => Err(format!(
            "berth {verb} runs on Linux, where berth serve and its hosts are. On this system berth is a client: berth opens the screens of a server, which berth connect URL or BERTH_SERVER names"
        )
        .into()),
        [] => Ok(()),
    }
}
