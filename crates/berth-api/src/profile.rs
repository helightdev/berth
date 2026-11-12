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

#[cfg(test)]
mod tests {
    use super::*;

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
}
