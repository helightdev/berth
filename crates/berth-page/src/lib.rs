//! The page of `berth serve`: the hosts of a server, its plugins, settings
//! and setup, through its portal. A browser draws it (berth-web) and so does
//! a terminal (berth-remote); each gives it a [`device::Device`].

mod admin;
mod app;
pub mod device;
mod home;
mod installed;
pub mod portal;
mod remote;
mod setup;

pub use app::App;
