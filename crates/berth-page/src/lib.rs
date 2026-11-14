//! The screens of a `berth serve` elsewhere: the hosts of a server, its
//! plugins, settings and setup, through its portal. berth-remote draws them
//! in a terminal and gives them a [`device::Device`].

mod admin;
mod app;
pub mod device;
mod home;
mod installed;
pub mod portal;
mod remote;
mod setup;

pub use app::App;
