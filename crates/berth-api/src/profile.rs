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

/// Whether the operator also takes the key of the berth that sets the host
/// up, which is so unless the profile says otherwise.
pub fn takes_own_key(config: &Value) -> bool {
    config["berth_ssh_key"].as_bool().unwrap_or(true)
}

/// Whether the operator uses sudo without a password, which is so unless the
/// profile says otherwise.
pub fn operator_passwordless_sudo(config: &Value) -> bool {
    config["operator_passwordless_sudo"]
        .as_bool()
        .unwrap_or(true)
}

/// Whether the SSH user's sudo asks for a password, so berth asks for it
/// first. A profile that does not say follows the operator's sudo, as the
/// SSH user is the operator once the host is set up; root needs no sudo.
pub fn asks_become_pass(config: &Value) -> bool {
    let builder = &config["builder"];
    let stored = &builder["ask_become_pass"];
    // Older profiles stored the switch as the strings "true" and "false".
    if let Some(asks) = stored
        .as_bool()
        .or_else(|| stored.as_str().map(|text| text == "true"))
    {
        return asks;
    }
    builder["ssh_user"] != "root" && !operator_passwordless_sudo(config)
}

/// The keys the operator account gets: the profile's, and with `own` the key
/// of the berth that sets the host up.
pub fn operator_keys(config: &Value, own: Option<&str>) -> Vec<String> {
    let mut keys = account_keys(config, "operator");
    if let Some(own) = own.map(str::trim).filter(|own| !own.is_empty())
        && takes_own_key(config)
        && !keys.iter().any(|known| known == own)
    {
        keys.push(own.to_owned());
    }
    keys
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
    fn the_operator_takes_the_key_of_its_berth_unless_told_not_to() {
        let own = Some("ssh-ed25519 OWN berth");
        let config = json!({"operator_ssh_public_keys": ["ssh-ed25519 AAAA one"]});
        assert_eq!(operator_keys(&config, own).len(), 2);
        assert_eq!(operator_keys(&config, None).len(), 1);
        let config = json!({"berth_ssh_key": false});
        assert!(operator_keys(&config, own).is_empty());
    }

    #[test]
    fn sudo_asks_for_a_password_only_when_the_operator_needs_one() {
        assert!(operator_passwordless_sudo(&json!({})));
        assert!(!asks_become_pass(&json!({})));
        assert!(!asks_become_pass(&json!({"builder": {"ssh_user": "ops"}})));
        let typed = json!({"operator_passwordless_sudo": false, "builder": {"ssh_user": "ops"}});
        assert!(asks_become_pass(&typed));
        let root = json!({"operator_passwordless_sudo": false, "builder": {"ssh_user": "root"}});
        assert!(!asks_become_pass(&root));
        // What the profile says wins, also in the older form.
        assert!(asks_become_pass(
            &json!({"builder": {"ask_become_pass": true}})
        ));
        assert!(!asks_become_pass(&json!({
            "operator_passwordless_sudo": false,
            "builder": {"ask_become_pass": "false"}
        })));
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
