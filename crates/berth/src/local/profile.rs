//! A host's profile as berth reads and keeps it, for the commands that
//! change one.

use crate::Result;
use berth_core::{paths::Paths, profiles::Place};
use serde_json::{Value, json};

/// The saved profile of `name`, and where it is kept.
pub fn load(paths: &Paths, name: &str) -> Result<(Place, Value)> {
    let place = Place::resolve(paths, name)?;
    let profile = place
        .load(paths)?
        .ok_or_else(|| format!("No host {name}. See: berth list"))?;
    Ok((place, profile))
}

/// Keep `profile` as the host's: on the berth server it belongs to as well,
/// for one of a server. Says where it is kept.
pub fn save(paths: &Paths, place: &Place, mut profile: Value) -> Result<String> {
    if let Some(url) = place.server_url() {
        let server = berth_core::server::Server::configured(paths)
            .filter(|server| server.url == url)
            .ok_or("The berth server of this host is not set any more")?;
        server.save_host(&place.title(), &profile)?;
        profile["builder"]["served"] = json!(url);
    }
    place.save(paths, &profile)?;
    Ok(format!("Saved {}", place.describe()))
}

/// Read a profile and fill in what a profile of berth has: the defaults of a
/// new one, and the names of now for those of older berths.
pub fn parse_config(data: &[u8]) -> Result<Value> {
    let mut config: Value = serde_json::from_slice(data)?;
    if !config.is_object() {
        return Err("Configuration must be a JSON object".into());
    }
    berth_api::profile::take_old_names(&mut config);
    berth_api::profile::retire_qemu(&mut config);
    for field in ["wifi", "ansible_vars", "builder"] {
        if !config[field].is_null() && !config[field].is_object() {
            return Err(format!("{field} must be a JSON object or null").into());
        }
    }
    for key in ["ansible_vars", "builder"] {
        if config[key].is_null() {
            config[key] = json!({});
        }
    }
    for (key, value) in [
        ("target", json!("laptop")),
        ("keyboard", json!("de")),
        ("operator_username", json!("ops")),
        ("agent_username", json!("agent")),
        ("agent_passwordless_sudo", json!(true)),
        ("target_disk_ssd", json!("any")),
    ] {
        if config[key].is_null() {
            config[key] = value;
        }
    }
    // Placeholders from older example profiles count as unset.
    for key in [
        "operator_password_hash",
        "target_disk_serial",
        "release_url",
    ] {
        if config[key].as_str().is_some_and(|v| v.contains("REPLACE")) {
            config[key] = json!("");
        }
    }
    if config["target_disk_mode"].is_null() {
        let has_serial = config["target_disk_serial"]
            .as_str()
            .is_some_and(|serial| !serial.is_empty());
        config["target_disk_mode"] = json!(if has_serial { "serial" } else { "auto" });
    }
    if config["wifi_enabled"].is_null() {
        let configured = config["wifi"].as_object().is_some_and(|w| !w.is_empty());
        config["wifi_enabled"] = json!(configured);
    }
    // The coding agents, T3 Code and GitHub's CLI were switches once and are
    // features of plugins that come with berth now. They go first: later
    // plugins, such as Unity's, set themselves up for the agents they find.
    // Only a profile of that layout is converted: one with the old switches,
    // or one with no list of features at all, which had every switch on. A
    // profile that lists its features is left as it is, marked or not.
    const OLD_AGENT_SWITCHES: [&str; 5] = [
        "devbox_install_agents",
        "devbox_agent_claude",
        "devbox_agent_codex",
        "devbox_agent_pi",
        "devbox_install_t3",
    ];
    let old_layout = OLD_AGENT_SWITCHES
        .iter()
        .any(|key| !config["ansible_vars"][*key].is_null())
        || config["ansible_vars"]["devbox_plugin_features"].is_null();
    if config["builder"]["shipped_plugins"] != true && old_layout {
        let vars = &config["ansible_vars"];
        let on = |key: &str, default: bool| vars[key].as_bool().unwrap_or(default);
        let agents = on("devbox_install_agents", true);
        let mut features: Vec<Value> = [
            ("claude/cli", agents && on("devbox_agent_claude", true)),
            ("codex/cli", agents && on("devbox_agent_codex", true)),
            ("github/cli", true),
            ("pi/cli", agents && on("devbox_agent_pi", true)),
            ("t3/server", on("devbox_install_t3", agents)),
        ]
        .into_iter()
        .filter(|(_, chosen)| *chosen)
        .map(|(feature, _)| json!(feature))
        .collect();
        for feature in vars["devbox_plugin_features"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if !features.contains(feature) {
                features.push(feature.clone());
            }
        }
        if !config["ansible_vars"].is_object() {
            config["ansible_vars"] = json!({});
        }
        let vars = config["ansible_vars"].as_object_mut().unwrap();
        for old in OLD_AGENT_SWITCHES {
            vars.remove(old);
        }
        vars.insert("devbox_plugin_features".into(), json!(features));
        config["builder"]["shipped_plugins"] = json!(true);
    }
    // Profiles from before the privilege levels keep what they had: sudo for
    // everything without a password is `full`. One whose agent types a
    // password stays as it is until a level is chosen.
    if config["agent_privileges"].is_null() && config["agent_passwordless_sudo"] == true {
        config["agent_privileges"] = json!("full");
    }
    // The action last run from the overview was kept once; the host lists its own now.
    if let Some(builder) = config["builder"].as_object_mut() {
        builder.remove("manage_task");
    }
    // Device names change between sessions, so the drive to write is chosen each time.
    if let Some(builder) = config["builder"].as_object_mut() {
        builder.remove("usb_device");
    }
    // The operator always has sudo without a password and always takes the key
    // of berth; profiles once had switches for both.
    if let Some(profile) = config.as_object_mut() {
        profile.remove("operator_passwordless_sudo");
        profile.remove("berth_ssh_key");
    }
    let problems = berth_api::profile::Access::problems(&config);
    if !problems.is_empty() {
        return Err(problems.join("; ").into());
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_agent_switches_become_plugin_features() {
        // Everything was on unless switched off; T3 followed the agents.
        let features =
            |config: &str| parse_config(config.as_bytes()).unwrap()["ansible_vars"].clone();
        assert_eq!(
            features("{}")["devbox_plugin_features"],
            json!([
                "claude/cli",
                "codex/cli",
                "github/cli",
                "pi/cli",
                "t3/server"
            ])
        );
        let old = features(
            r#"{"ansible_vars": {"devbox_agent_codex": false, "devbox_install_t3": false,
                "devbox_plugin_features": ["unity/cli"], "devbox_tools_web": true}}"#,
        );
        assert_eq!(
            old,
            json!({"devbox_tools_web": true,
                   "devbox_plugin_features": ["claude/cli", "github/cli", "pi/cli", "unity/cli"]})
        );
        let none = features(r#"{"ansible_vars": {"devbox_install_agents": false}}"#);
        assert_eq!(none["devbox_plugin_features"], json!(["github/cli"]));
        // Done once: what is switched off afterwards stays off.
        let kept = features(
            r#"{"builder": {"shipped_plugins": true}, "ansible_vars": {"devbox_plugin_features": []}}"#,
        );
        assert_eq!(kept["devbox_plugin_features"], json!([]));
    }

    #[test]
    fn new_profiles_default_to_auto_with_wifi_off() {
        let config = parse_config(br#"{}"#).unwrap();
        assert_eq!(config["target_disk_mode"], "auto");
        assert_eq!(config["wifi_enabled"], false);
        assert!(config.get("operator_passwordless_sudo").is_none());
        assert!(config["access"]["sudo"].is_null());
        // The switches of a profile from before are dropped, whatever they said.
        let old = parse_config(br#"{"operator_passwordless_sudo": false, "berth_ssh_key": false}"#)
            .unwrap();
        assert!(
            old.get("operator_passwordless_sudo").is_none() && old.get("berth_ssh_key").is_none()
        );
        let legacy = parse_config(
            br#"{"target_disk_serial":"OLD_SSD","wifi":{"ssid":"saved"},
                "release_url":"https://REPLACE.example/x"}"#,
        )
        .unwrap();
        assert_eq!(legacy["target_disk_mode"], "serial");
        assert_eq!(legacy["wifi_enabled"], true);
        assert_eq!(legacy["release_url"], "");
        // A profile of the qemu target there once was shows as cloud, reached from the QEMU host.
        let qemu = parse_config(br#"{"target": "qemu"}"#).unwrap();
        assert_eq!(qemu["target"], "cloud");
        assert_eq!(
            qemu["ansible_vars"]["devbox_bootstrap_ssh_cidr"],
            "10.0.2.2/32"
        );
    }
}
