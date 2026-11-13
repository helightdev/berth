//! What a host's profile says of the SSH keys its accounts take and of sudo.

use serde_json::{Value, json};

/// The public keys an account takes: `operator` or `agent`. A profile lists
/// them in `ACCOUNT_ssh_public_keys`; an older one has a single key in
/// `ACCOUNT_ssh_public_key`.
pub fn account_keys(config: &Value, account: &str) -> Vec<String> {
    let mut keys: Vec<String> = config[format!("{account}_ssh_public_keys")]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|key| key.trim().to_owned())
        .collect();
    let single = config[format!("{account}_ssh_public_key")]
        .as_str()
        .unwrap_or("");
    for key in single.lines().map(str::trim) {
        if !keys.iter().any(|known| known == key) {
            keys.push(key.to_owned());
        }
    }
    keys.retain(|key| !key.is_empty());
    keys
}

/// Write an account's keys, in the form profiles have now.
pub fn set_account_keys(config: &mut Value, account: &str, keys: &[String]) {
    if !config.is_object() {
        *config = json!({});
    }
    config[format!("{account}_ssh_public_keys")] = json!(keys);
    if let Some(profile) = config.as_object_mut() {
        profile.remove(&format!("{account}_ssh_public_key"));
    }
}

/// Whether the SSH user's sudo asks for a password, so berth asks for it
/// first. The operator's sudo never asks, so only a profile that says so
/// does: one applied for the first time as an account of the machine's own.
pub fn asks_become_pass(config: &Value) -> bool {
    let stored = &config["builder"]["ask_become_pass"];
    // Older profiles stored the switch as the strings "true" and "false".
    stored
        .as_bool()
        .or_else(|| stored.as_str().map(|text| text == "true"))
        .unwrap_or(false)
}

/// The operator's name, `ops` unless the profile names another.
pub fn operator(config: &Value) -> &str {
    match config["operator_username"].as_str() {
        None | Some("") => "ops",
        Some(name) => name,
    }
}

/// Make a profile stop asking for the sudo password once it has been applied,
/// when berth reaches the host as its operator, whose sudo then needs none.
/// Says whether it changed the profile.
pub fn settle_after_apply(config: &mut Value) -> bool {
    let ssh_user = config["builder"]["ssh_user"].as_str().unwrap_or("");
    if config["profile"] == "container" || ssh_user != operator(config) || !asks_become_pass(config)
    {
        return false;
    }
    if let Some(builder) = config["builder"].as_object_mut() {
        builder.remove("ask_become_pass");
    }
    true
}

/// The secret that holds the API key of the tailnet's control server. berth
/// uses it itself, to create enrollment keys, and never gives it to a host.
pub const TAILNET_API_KEY: &str = "tailnet_api_key";

/// Whether the host gets Tailscale, which it does unless the profile says not.
pub fn installs_tailscale(config: &Value) -> bool {
    config["ansible_vars"]["devbox_install_tailscale"]
        .as_bool()
        .unwrap_or(true)
}

/// Whether berth creates the host's enrollment key itself when one is needed:
/// switched on, and no key pasted into the profile.
pub fn joins_automatically(config: &Value) -> bool {
    installs_tailscale(config)
        && config["tailscale_auto_key"].as_bool().unwrap_or(false)
        && config["tailscale_auth_key"]
            .as_str()
            .unwrap_or("")
            .is_empty()
}

/// Where a QEMU guest on the user-mode network sees the machine it runs on.
pub const QEMU_HOST: &str = "10.0.2.2/32";

/// Make a profile of the `qemu` kind there once was a `cloud` one. That kind
/// let SSH in only from the QEMU host when no recovery network was named, so
/// it gets that network rather than SSH open to every address.
pub fn retire_qemu(config: &mut Value) {
    if config["profile"] != "qemu" {
        return;
    }
    config["profile"] = json!("cloud");
    if !config["ansible_vars"].is_object() {
        config["ansible_vars"] = json!({});
    }
    let cidr = &mut config["ansible_vars"]["devbox_bootstrap_ssh_cidr"];
    if cidr.as_str().unwrap_or("").is_empty() {
        *cidr = json!(QEMU_HOST);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pasted_key_wins_over_the_automatic_one() {
        assert!(!joins_automatically(&json!({})));
        assert!(joins_automatically(&json!({"tailscale_auto_key": true})));
        assert!(!joins_automatically(
            &json!({"tailscale_auto_key": true, "tailscale_auth_key": "tskey-1"})
        ));
        assert!(!joins_automatically(&json!({
            "tailscale_auto_key": true,
            "ansible_vars": {"devbox_install_tailscale": false},
        })));
    }

    #[test]
    fn keys_come_from_the_list_and_from_an_older_profile() {
        let config = json!({
            "operator_ssh_public_keys": ["ssh-ed25519 AAAA one", " "],
            "operator_ssh_public_key": "ssh-ed25519 BBBB two\nssh-ed25519 AAAA one",
        });
        assert_eq!(
            account_keys(&config, "operator"),
            ["ssh-ed25519 AAAA one", "ssh-ed25519 BBBB two"]
        );
        assert!(account_keys(&config, "agent").is_empty());
        let mut config = config;
        set_account_keys(&mut config, "operator", &["ssh-ed25519 CCCC".into()]);
        assert_eq!(
            config,
            json!({"operator_ssh_public_keys": ["ssh-ed25519 CCCC"]})
        );
    }

    #[test]
    fn sudo_asks_for_a_password_only_when_the_profile_says_so() {
        assert!(!asks_become_pass(&json!({})));
        assert!(!asks_become_pass(&json!({"builder": {"ssh_user": "ops"}})));
        // A profile from when the operator's sudo could ask is not taken to.
        let old = json!({"operator_passwordless_sudo": false, "builder": {"ssh_user": "ops"}});
        assert!(!asks_become_pass(&old));
        assert!(asks_become_pass(
            &json!({"builder": {"ask_become_pass": true}})
        ));
        // Also in the older form.
        assert!(asks_become_pass(
            &json!({"builder": {"ask_become_pass": "true"}})
        ));
        assert!(!asks_become_pass(
            &json!({"builder": {"ask_become_pass": "false"}})
        ));
    }

    #[test]
    fn an_apply_as_the_operator_stops_the_asking() {
        let mut first = json!({"builder": {"ssh_user": "ops", "ask_become_pass": true}});
        assert!(settle_after_apply(&mut first));
        assert!(!asks_become_pass(&first));
        assert!(!settle_after_apply(&mut first));
        // An account of the machine's own keeps a sudo that asks.
        let mut own = json!({"operator_username": "ops", "builder": {"ssh_user": "admin", "ask_become_pass": true}});
        assert!(!settle_after_apply(&mut own));
        assert!(asks_become_pass(&own));
        let mut named = json!({"operator_username": "admin", "builder": {"ssh_user": "admin", "ask_become_pass": "true"}});
        assert!(settle_after_apply(&mut named));
        assert!(!asks_become_pass(&named));
        // A container has no operator.
        let mut container = json!({"profile": "container", "builder": {"ssh_user": "ops", "ask_become_pass": true}});
        assert!(!settle_after_apply(&mut container));
    }

    #[test]
    fn a_qemu_profile_is_a_cloud_one_reached_from_the_qemu_host() {
        let mut old = json!({"profile": "qemu"});
        retire_qemu(&mut old);
        assert_eq!(
            old,
            json!({"profile": "cloud", "ansible_vars": {"devbox_bootstrap_ssh_cidr": "10.0.2.2/32"}})
        );
        // A network it names stays, and other kinds are left as they are.
        let mut named = json!({"profile": "qemu", "ansible_vars": {"devbox_bootstrap_ssh_cidr": "192.0.2.0/24"}});
        retire_qemu(&mut named);
        assert_eq!(named["profile"], "cloud");
        assert_eq!(
            named["ansible_vars"]["devbox_bootstrap_ssh_cidr"],
            "192.0.2.0/24"
        );
        for kept in [json!({"profile": "laptop"}), json!({"profile": "cloud"})] {
            let mut profile = kept.clone();
            retire_qemu(&mut profile);
            assert_eq!(profile, kept);
        }
    }
}
