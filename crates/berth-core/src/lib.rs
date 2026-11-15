//! What berth knows on the workstation apart from its screen: where things are
//! kept, hosts and how to reach them, plugins, registries and secrets, jobs.

pub mod apply;
pub mod credentials;
pub mod discovery;
pub mod host;
pub mod host_type;
pub mod import;
pub mod job;
pub mod kept;
pub mod kept_profile;
pub mod keys;
pub mod link;
pub mod node;
pub mod paths;
pub mod plugin_state;
pub mod plugins;
pub mod profiles;
pub mod provision;
pub mod provisioner;
pub mod registry;
pub mod relay;
pub mod resource;
pub mod rollout;
pub mod runner;
pub mod seal;
pub mod seed;
pub mod server;
pub mod server_image;
pub mod simulate;
pub mod store;
pub mod tailnet;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// A reason found before anything began not to begin: the host or this
/// server cannot do it now, as when a secret it needs is not set. A caller
/// says it as a refusal rather than as a request that was wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

/// Whether `error` is a refusal before anything began: [`Refused`], or a
/// host of a type that cannot do it.
pub fn is_refused(error: &(dyn std::error::Error + 'static)) -> bool {
    error.is::<Refused>()
        || error.is::<host_type::Refusal>()
        || error.downcast_ref::<store::Error>().is_some_and(|error| {
            matches!(error, store::Error::Taken { .. } | store::Error::Given(_))
        })
}
