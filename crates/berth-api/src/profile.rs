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
    Access::of(config).sudo.unwrap_or(false)
}

/// The operator's name, `ops` unless the profile names another.
pub fn operator(config: &Value) -> &str {
    match config["operator_username"].as_str() {
        None | Some("") => "ops",
        Some(name) => name,
    }
}

/// What a successful apply makes true of a workstation that berth reaches
/// as its operator, written into its profile: sudo takes no password, and a
/// login with a password, which the operator's SSH no longer takes, becomes
/// one with berth's key, which the role installed. `key:NAME` stays, as the
/// role installs that key too, and so does `ssh`. An account of the
/// machine's own, a container and a host reached through its own command
/// keep what they say. Says whether it changed the profile.
pub fn settle_after_apply(config: &mut Value) -> bool {
    let mut access = Access::of(config);
    if config["profile"] == "container"
        || level(config) != crate::HostLevel::Workstation
        || access.link.is_some()
        || access.user.is_empty()
        || access.user != operator(config)
    {
        return false;
    }
    let before = access.clone();
    access.sudo = Some(false);
    if access.auth == Auth::Password {
        access.auth = Auth::Berth;
    }
    if access == before && config["access"]["sudo"] == "none" {
        return false;
    }
    access.write(config);
    true
}

/// How berth logs in to a host over SSH: `access.auth` of its profile.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Auth {
    /// The installation's own key, the key `berth` of its key ring.
    #[default]
    Berth,
    /// Another key of the key ring, by name.
    Key(String),
    /// A password, kept as the secret `ssh/HOST` or asked for.
    Password,
    /// The SSH configuration and agent of whoever runs berth, with berth's
    /// own key offered after them.
    Ssh,
}

impl Auth {
    /// What a profile says, or `None` for something berth does not know.
    pub fn parse(text: &str) -> Option<Auth> {
        Some(match text {
            "" | "berth" => Auth::Berth,
            "password" => Auth::Password,
            "ssh" => Auth::Ssh,
            _ => {
                let name = text.strip_prefix("key:")?;
                if !valid_key_name(name) {
                    return None;
                }
                Auth::Key(name.to_owned())
            }
        })
    }

    /// The key of the key ring it logs in with, if it does.
    pub fn key(&self) -> Option<&str> {
        match self {
            Auth::Berth => Some(BERTH_KEY),
            Auth::Key(name) => Some(name),
            Auth::Password | Auth::Ssh => None,
        }
    }
}

impl std::fmt::Display for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Auth::Berth => f.write_str("berth"),
            Auth::Key(name) => write!(f, "key:{name}"),
            Auth::Password => f.write_str("password"),
            Auth::Ssh => f.write_str("ssh"),
        }
    }
}

/// The name of the installation's own key in its key ring.
pub const BERTH_KEY: &str = "berth";

/// Whether a name can be a key's: a word of letters, digits, `-` and `_`,
/// as it becomes a file name and part of `key:NAME`.
pub fn valid_key_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('-')
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

/// Where a host is and how berth logs in to it: the `access` object of its
/// profile.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Access {
    /// A host name, an address or an alias of the SSH configuration.
    pub address: String,
    /// The SSH user; empty leaves it to the SSH configuration.
    pub user: String,
    /// The SSH port; empty leaves it to the SSH configuration.
    pub port: String,
    pub auth: Auth,
    /// Whether the SSH user's sudo takes a password; `None` follows the
    /// operator's sudo.
    pub sudo: Option<bool>,
    /// A command that replaces SSH, as for a container.
    pub link: Option<Vec<String>>,
}

impl Access {
    /// What a profile says. Values berth does not know read as their default,
    /// and [`Access::problems`] names them.
    pub fn of(config: &Value) -> Access {
        let access = &config["access"];
        let text = |key: &str| match &access[key] {
            Value::String(text) => text.trim().to_owned(),
            Value::Number(number) => number.to_string(),
            _ => String::new(),
        };
        Access {
            address: text("address"),
            user: text("user"),
            port: text("port"),
            auth: Auth::parse(&text("auth")).unwrap_or_default(),
            sudo: match text("sudo").as_str() {
                "password" => Some(true),
                "none" => Some(false),
                _ => None,
            },
            link: access["link"].as_array().map(|words| {
                words
                    .iter()
                    .filter_map(|word| word.as_str().map(str::to_owned))
                    .collect()
            }),
        }
    }

    /// Write it into a profile, leaving out what is empty.
    pub fn write(&self, config: &mut Value) {
        if !config.is_object() {
            *config = json!({});
        }
        let mut access = serde_json::Map::new();
        for (key, value) in [
            ("address", &self.address),
            ("user", &self.user),
            ("port", &self.port),
        ] {
            if !value.is_empty() {
                access.insert(key.into(), json!(value));
            }
        }
        access.insert("auth".into(), json!(self.auth.to_string()));
        if let Some(asks) = self.sudo {
            access.insert("sudo".into(), json!(if asks { "password" } else { "none" }));
        }
        if let Some(link) = &self.link {
            access.insert("link".into(), json!(link));
        }
        config["access"] = Value::Object(access);
    }

    /// Whether berth reaches the host: at an address, or through its own command.
    pub fn reachable(&self) -> bool {
        !self.address.is_empty() || self.link.is_some()
    }

    /// What is wrong with the `access` object of a profile, in sentences.
    pub fn problems(config: &Value) -> Vec<String> {
        let access = &config["access"];
        let mut problems = vec![];
        if access.is_null() {
            return problems;
        }
        if !access.is_object() {
            return vec!["access must be an object".into()];
        }
        let auth = access["auth"].as_str().unwrap_or("");
        if Auth::parse(auth).is_none() {
            problems.push(format!(
                "access.auth is berth, key:NAME, password or ssh, not {auth}"
            ));
        }
        let sudo = access["sudo"].as_str().unwrap_or("");
        if !["", "none", "password"].contains(&sudo) {
            problems.push(format!("access.sudo is none or password, not {sudo}"));
        }
        let port = Access::of(config).port;
        if !port.is_empty() && !port.parse::<u16>().is_ok_and(|port| port != 0) {
            problems.push("access.port must be between 1 and 65535".into());
        }
        if !access["link"].is_null()
            && !access["link"]
                .as_array()
                .is_some_and(|words| !words.is_empty() && words.iter().all(Value::is_string))
        {
            problems.push("access.link is a command: a list of words".into());
        }
        problems
    }
}

/// The level a profile means its host to have: `level`, a workstation when
/// it does not say, so profiles from before levels keep their meaning.
pub fn level(config: &Value) -> crate::HostLevel {
    config["level"]
        .as_str()
        .and_then(crate::HostLevel::parse)
        .unwrap_or_default()
}

/// Set the level a profile means its host to have. A workstation is left
/// unsaid, as profiles from before levels are.
pub fn set_level(config: &mut Value, level: crate::HostLevel) {
    if !config.is_object() {
        *config = json!({});
    }
    match level {
        crate::HostLevel::Workstation => {
            if let Some(profile) = config.as_object_mut() {
                profile.remove("level");
            }
        }
        other => config["level"] = json!(other.name()),
    }
}

/// The profile of a host berth only reaches or only controls: its level and
/// how it is reached, nothing of a workstation.
pub fn minimal(level: crate::HostLevel, access: &Access) -> Value {
    let mut config = json!({});
    set_level(&mut config, level);
    access.write(&mut config);
    config
}

/// What is wrong with a profile as a whole, in sentences: its level, its
/// `access`, and for a workstation the `builder` its set-up keeps. A host
/// of a lower level needs only to be reachable.
pub fn problems(config: &Value) -> Vec<String> {
    if !config.is_object() {
        return vec!["A profile is an object".into()];
    }
    let mut problems = vec![];
    let level = match &config["level"] {
        Value::Null => Some(crate::HostLevel::Workstation),
        Value::String(text) => crate::HostLevel::parse(text),
        _ => None,
    };
    let Some(level) = level else {
        return vec![format!(
            "level is reachable, controller or workstation, not {}",
            config["level"]
        )];
    };
    problems.extend(Access::problems(config));
    match level {
        crate::HostLevel::Workstation if !config["builder"].is_object() => {
            problems.push("A workstation's profile has a builder object".into())
        }
        crate::HostLevel::Workstation => {}
        _ if !Access::of(config).reachable() => problems.push(format!(
            "A {level} host needs access.address, where berth reaches it"
        )),
        _ => {}
    }
    problems
}

/// What a profile of berth 0.4 said under `builder` of how it is reached,
/// taken out of there by [`take_builder_access`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OldAccess {
    /// `builder.ssh_key`: the path of a private key on the machine that ran berth.
    pub key_file: String,
    /// `builder.ask_pass`: Ansible asked for an SSH password.
    pub ask_pass: bool,
}

/// The fields of `builder` that `access` took over.
const OLD_ACCESS: &[&str] = &[
    "ssh_host",
    "ssh_user",
    "ssh_port",
    "ssh_key",
    "ask_become_pass",
    "ask_pass",
    "link",
];

/// Move the access fields of an older profile from `builder` into `access`,
/// leaving `access.auth` to the caller, which knows what the key file was.
/// What `access` has already stays. `None` when there was nothing to move.
pub fn take_builder_access(config: &mut Value) -> Option<OldAccess> {
    let builder = config.get_mut("builder")?.as_object_mut()?;
    if !OLD_ACCESS.iter().any(|key| builder.contains_key(*key)) {
        return None;
    }
    let mut taken = serde_json::Map::new();
    for key in OLD_ACCESS {
        if let Some(value) = builder.remove(*key) {
            taken.insert((*key).to_owned(), value);
        }
    }
    let text = |key: &str| match taken.get(key) {
        Some(Value::String(text)) => text.trim().to_owned(),
        Some(Value::Number(number)) => number.to_string(),
        _ => String::new(),
    };
    // Older profiles stored the switches as the strings "true" and "false".
    let switch = |key: &str| match taken.get(key) {
        Some(Value::Bool(on)) => Some(*on),
        Some(Value::String(text)) if text == "true" || text == "false" => Some(text == "true"),
        _ => None,
    };
    let mut access = Access::of(config);
    let had_access = config["access"].is_object();
    for (field, key) in [
        (&mut access.address, "ssh_host"),
        (&mut access.user, "ssh_user"),
        (&mut access.port, "ssh_port"),
    ] {
        if field.is_empty() {
            *field = text(key);
        }
    }
    if access.sudo.is_none() {
        access.sudo = switch("ask_become_pass");
    }
    if access.link.is_none()
        && let Some(Value::Array(words)) = taken.get("link")
    {
        access.link = Some(
            words
                .iter()
                .filter_map(|word| word.as_str().map(str::to_owned))
                .collect(),
        );
    }
    let old = OldAccess {
        key_file: text("ssh_key"),
        ask_pass: switch("ask_pass").unwrap_or(false),
    };
    // An access object written by a later berth keeps its own way of logging in.
    let auth = config["access"]["auth"].clone();
    access.write(config);
    if had_access && auth.is_string() {
        config["access"]["auth"] = auth;
    } else if let Some(access) = config["access"].as_object_mut() {
        access.remove("auth");
    }
    Some(old)
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
        assert!(!asks_become_pass(&json!({"access": {"user": "ops"}})));
        // A profile from when the operator's sudo could ask is not taken to.
        let old = json!({"operator_passwordless_sudo": false, "access": {"user": "ops"}});
        assert!(!asks_become_pass(&old));
        assert!(asks_become_pass(&json!({"access": {"sudo": "password"}})));
        assert!(!asks_become_pass(&json!({"access": {"sudo": "none"}})));
    }

    #[test]
    fn an_apply_as_the_operator_settles_its_login() {
        let mut first = json!({"builder": {}, "access": {"address": "192.0.2.10", "user": "ops", "auth": "password", "sudo": "password"}});
        assert!(settle_after_apply(&mut first));
        assert!(!asks_become_pass(&first));
        assert_eq!(
            first["access"],
            json!({"address": "192.0.2.10", "user": "ops", "auth": "berth", "sudo": "none"})
        );
        assert!(!settle_after_apply(&mut first));
        // A key of the ring and the user's SSH configuration stay as they are.
        for auth in ["key:admin", "ssh"] {
            let mut kept = json!({"builder": {}, "access": {"user": "ops", "auth": auth}});
            assert!(settle_after_apply(&mut kept));
            assert_eq!(kept["access"]["auth"], auth);
        }
        // An account of the machine's own keeps a sudo that asks.
        let mut own = json!({"operator_username": "ops", "builder": {}, "access": {"user": "admin", "sudo": "password"}});
        assert!(!settle_after_apply(&mut own));
        assert!(asks_become_pass(&own));
        let mut named = json!({"operator_username": "admin", "builder": {}, "access": {"user": "admin", "sudo": "password"}});
        assert!(settle_after_apply(&mut named));
        assert!(!asks_become_pass(&named));
        // A container has no operator, and a host below a workstation was not set up by berth.
        let mut container =
            json!({"profile": "container", "access": {"user": "ops", "sudo": "password"}});
        assert!(!settle_after_apply(&mut container));
        let mut controller = json!({"level": "controller", "access": {"address": "192.0.2.4", "user": "ops", "sudo": "password"}});
        assert!(!settle_after_apply(&mut controller));
    }

    #[test]
    fn the_access_fields_move_out_of_builder_once() {
        let mut old = json!({"builder": {
            "ssh_host": "192.0.2.10", "ssh_user": "ops", "ssh_port": "2222",
            "ssh_key": "~/.ssh/studio", "ask_become_pass": "true", "ask_pass": false,
            "iso": "/x.iso"
        }});
        let taken = take_builder_access(&mut old).unwrap();
        assert_eq!(taken.key_file, "~/.ssh/studio");
        assert_eq!(
            old,
            json!({"builder": {"iso": "/x.iso"},
                   "access": {"address": "192.0.2.10", "user": "ops", "port": "2222", "sudo": "password"}})
        );
        assert!(take_builder_access(&mut old).is_none());
        let access = Access::of(&old);
        assert_eq!((access.auth, access.sudo), (Auth::Berth, Some(true)));
        // A link moves too, and sudo left unsaid stays so.
        let mut container =
            json!({"builder": {"link": ["docker", "exec", "-i", "c", "berthd", "connect"]}});
        take_builder_access(&mut container).unwrap();
        assert_eq!(Access::of(&container).link.unwrap().len(), 6);
        assert!(container["access"]["sudo"].is_null());
        // What a later berth wrote stays.
        let mut both = json!({"access": {"address": "192.0.2.20", "auth": "key:deploy"},
                              "builder": {"ssh_host": "192.0.2.10"}});
        take_builder_access(&mut both).unwrap();
        assert_eq!(
            both["access"],
            json!({"address": "192.0.2.20", "auth": "key:deploy"})
        );
    }

    #[test]
    fn access_reads_what_it_knows_and_names_the_rest() {
        for (auth, parsed) in [
            ("", Some(Auth::Berth)),
            ("berth", Some(Auth::Berth)),
            ("key:deploy", Some(Auth::Key("deploy".into()))),
            ("password", Some(Auth::Password)),
            ("ssh", Some(Auth::Ssh)),
            ("key:", None),
            ("key:../x", None),
            ("agent", None),
        ] {
            assert_eq!(Auth::parse(auth), parsed, "{auth}");
            if let Some(parsed) = parsed.filter(|_| !auth.is_empty()) {
                assert_eq!(parsed.to_string(), auth);
            }
        }
        let config = json!({"access": {"address": "studio", "port": 2222, "auth": "agent", "sudo": "maybe"}});
        assert_eq!(Access::of(&config).port, "2222");
        let problems = Access::problems(&config);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(Access::problems(&json!({"access": {"port": "70000"}}))[0].contains("port"));
        let mut written = json!({});
        Access::of(&config).write(&mut written);
        assert_eq!(
            written["access"],
            json!({"address": "studio", "port": "2222", "auth": "berth"})
        );
    }

    #[test]
    fn a_profile_without_a_level_is_a_workstation() {
        use crate::HostLevel;
        assert_eq!(level(&json!({"builder": {}})), HostLevel::Workstation);
        assert_eq!(
            level(&json!({"level": "controller"})),
            HostLevel::Controller
        );
        // One berth does not know reads as the default, and is named a problem.
        assert_eq!(level(&json!({"level": "server"})), HostLevel::Workstation);
        assert!(problems(&json!({"level": "server"}))[0].contains("not \"server\""));
        let access = Access {
            address: "192.0.2.10".into(),
            user: "ops".into(),
            ..Access::default()
        };
        let reachable = minimal(HostLevel::Reachable, &access);
        assert_eq!(
            reachable,
            json!({"level": "reachable", "access": {"address": "192.0.2.10", "user": "ops", "auth": "berth"}})
        );
        assert!(problems(&reachable).is_empty());
        // Only a workstation needs the set-up's part; a lower level needs an address.
        assert_eq!(
            problems(&json!({"access": {"address": "studio"}})),
            ["A workstation's profile has a builder object"]
        );
        assert!(problems(&json!({"level": "controller"}))[0].contains("access.address"));
        let mut changed = reachable.clone();
        set_level(&mut changed, HostLevel::Workstation);
        assert!(changed.get("level").is_none());
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
