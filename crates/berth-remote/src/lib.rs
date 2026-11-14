//! berth-remote: the screens of a berth serve in a terminal, for a machine
//! that only looks after the hosts of a berth serve elsewhere. It runs on
//! Linux, macOS and Windows: the screens are berth-page's, and this crate
//! gives them a terminal and a network. It is a
//! library as well as the program so that berth's tests can sign it in to a
//! server of their own.

pub mod device;
pub mod keys;
pub mod net;
pub mod options;
pub mod screens;
pub mod settings;
pub mod socket;

/// Take in what the network said since the last turn, on the thread that draws.
pub fn pump() {
    net::pump();
    socket::pump();
}
