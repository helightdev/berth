//! What berth knows on the workstation apart from its screen: where things are
//! kept, hosts and how to reach them, plugins, registries and secrets, jobs.

pub mod apply;
pub mod credentials;
pub mod discovery;
pub mod host;
pub mod job;
pub mod link;
pub mod oauth;
pub mod paths;
pub mod plugins;
pub mod provision;
pub mod registry;
pub mod rollout;
pub mod seed;
pub mod server;
pub mod store;
pub mod tailnet;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
