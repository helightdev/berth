//! The cloud-init seed of a saved host: what a new machine is started with
//! to become the host.

use crate::{
    Result, apply,
    host::Host,
    paths::{Paths, layout},
    plugins,
    registry::{self, Store},
    tailnet,
};
use serde_json::{Value, json};
use std::{fs, path::PathBuf, process::Command};

/// A rendered cloud seed.
pub struct Seed {
    /// cloud-init user data, for the provider's field of that name.
    pub user_data: String,
    /// The release archive the machine is given instead of downloading
    /// one, when it was asked for.
    pub archive: Option<Archive>,
}

pub struct Archive {
    pub path: PathBuf,
    pub sha256: String,
}

/// Render the cloud seed of the saved host `name`. With a `release_store` in
/// the profile the release is published there first, and the profile keeps
/// the address. With `delivered`, whoever starts the machine puts the release
/// archive into it, so nothing is published or downloaded and the profile
/// needs no `release_url`.
pub fn cloud(paths: &Paths, store: &Store, name: &str, delivered: bool) -> Result<Seed> {
    let host = Host::load(paths, name)?;
    let source = paths.source()?;
    let dirs = apply::plugin_dirs(&plugins::found(
        &source,
        &plugins::own_dir(&paths.config_dir),
    ));
    let release = host.config["release_store"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    if !release.is_empty() && !delivered {
        let url = registry::publish_from(store, &paths.build_dir(), &dirs, &release)
            .map_err(|error| format!("Publishing to {release} failed: {error}"))?;
        let mut profile = host.config.clone();
        profile["release_url"] = json!(url);
        host.place.save(paths, &profile)?;
    }
    // The seed is rendered from a private copy of the profile, which holds a
    // new enrollment key for a host that joins the tailnet automatically.
    let saved: Value = host
        .place
        .load(paths)?
        .ok_or_else(|| format!("No host {name}"))?;
    let config = named(tailnet::seed_profile(paths, &saved, name)?, name);
    let seeded = tailnet::profile_file(&config)?;
    let build = paths.build_dir();
    let mut command = Command::new("python3");
    command
        .current_dir(&source)
        .args([layout::BUILDER, "render", "--kind", "cloud", "--config"])
        .arg(seeded.path())
        .env("DEVBOX_BUILD_DIR", &build)
        .env("BERTH_PLUGINS", dirs.to_string())
        .env(
            "BERTH_SSH_PUBLIC_KEY",
            crate::keys::operator_keys(paths).join("\n"),
        )
        .stdin(std::process::Stdio::null());
    if delivered {
        command.env("BERTH_RELEASE_DELIVERED", "1");
    }
    // A release that is not public: the seed carries the login of the registry it is in.
    let private = host.config["release_private"].as_bool().unwrap_or(false);
    let login = match host.config["release_login"].as_str() {
        Some(name) => Some(name),
        None if private && !release.is_empty() => release.split('/').next(),
        None => None,
    };
    if let (Some(registry), false) = (login, delivered) {
        let login = store.curl_login(registry)?.ok_or_else(|| {
            format!("Registry {registry} has no stored login for downloading the release")
        })?;
        command.env("BERTH_RELEASE_LOGIN", login);
    }
    let output = command
        .output()
        .map_err(|error| format!("python3: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr)
            .trim()
            .to_owned()
            .into());
    }
    // The builder prints the folder of the seed, and may print more around it.
    let said = String::from_utf8_lossy(&output.stdout);
    let out = said
        .lines()
        .map(|line| PathBuf::from(line.trim()))
        .find(|dir| dir.join("user-data").is_file())
        .ok_or("The seed builder said nothing of where the seed is")?;
    let user_data = fs::read_to_string(out.join("user-data"))?;
    let archive = match delivered {
        true => Some(archive(&build, name)?),
        false => None,
    };
    Ok(Seed { user_data, archive })
}

/// A copy of the release archive that was just built, for this host alone,
/// so a render for another host cannot change it before it is delivered.
fn archive(build: &std::path::Path, name: &str) -> Result<Archive> {
    use std::os::unix::fs::DirBuilderExt;
    let dir = build.join("delivered").join(name);
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    let path = dir.join("devbox-release.tar.gz");
    fs::copy(build.join("devbox-release.tar.gz"), &path)?;
    Ok(Archive {
        sha256: sha256(&path)?,
        path,
    })
}

pub fn sha256(path: &std::path::Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    std::io::copy(&mut fs::File::open(path)?, &mut hash)?;
    Ok(hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// A profile for the seed builder, with the host's name as the machine's
/// when the profile gives it none.
pub fn named(mut config: Value, name: &str) -> Value {
    if config["hostname"].as_str().unwrap_or("").is_empty() {
        config["hostname"] = json!(name);
    }
    config
}
