//! The screens of berth: the hosts of a `berth serve`, its plugins, settings
//! and setup, through its portal, in a terminal. The server is elsewhere,
//! the socket of a `berth serve` on this machine, or one in the program.

mod admin;
mod app;
pub mod device;
mod home;
mod installed;
pub mod keys;
pub mod net;
pub mod portal;
mod remote;
pub mod screens;
pub mod settings;
mod setup;
pub mod socket;
pub mod terminal;

pub use app::App;

/// Take in what the network said since the last turn, on the thread that draws.
pub fn pump() {
    net::pump();
    socket::pump();
}
