//! What berth knows on the workstation apart from its screen: where things are
//! kept, hosts and how to reach them, plugins, registries and secrets, jobs.

pub mod apply;
pub mod credentials;
pub mod discovery;
pub mod host;
pub mod import;
pub mod job;
pub mod kept;
pub mod keys;
pub mod link;
pub mod oauth;
pub mod paths;
pub mod plugin_state;
pub mod plugins;
pub mod profiles;
pub mod provision;
pub mod registry;
pub mod rollout;
pub mod runner;
pub mod seal;
pub mod seed;
pub mod server;
pub mod store;
pub mod tailnet;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
