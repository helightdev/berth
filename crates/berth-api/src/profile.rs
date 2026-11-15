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
    // A node's role gave the SSH user berth's key and left sudo as it was.
    if host_type(config) == crate::HostType::Node {
        if access.link.is_some() || access.user.is_empty() || access.auth != Auth::Password {
            return false;
        }
        access.auth = Auth::Berth;
        access.write(config);
        return true;
    }
    if config["target"] == "container"
        || host_type(config) != crate::HostType::Workstation
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

/// Whether a text can be a host's SSH address: a hostname, an IP address or
/// an alias of the SSH configuration. It goes on ssh's command line, where
/// one that starts with `-` would be read as an option.
pub fn valid_address(address: &str) -> bool {
    !address.is_empty()
        && address.len() <= 255
        && !address.starts_with('-')
        && !address.contains('@')
        && !address
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control())
}

/// Whether a text can be the user berth logs in as over SSH, empty for
/// ssh's own choice. It becomes `USER@ADDRESS` on ssh's command line, where
/// one that starts with `-` would be read as an option.
pub fn valid_user(user: &str) -> bool {
    user.len() <= 64
        && !user.starts_with('-')
        && !user.contains('@')
        && !user.chars().any(|ch| ch.is_whitespace() || ch.is_control())
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
        let Access {
            address,
            user,
            port,
            ..
        } = Access::of(config);
        if !address.is_empty() && !valid_address(&address) {
            problems.push(format!(
                "access.address is a hostname, IP address or SSH alias, not {address}"
            ));
        }
        if !valid_user(&user) {
            problems.push(format!("access.user is not an SSH user: {user}"));
        }
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

/// The type a profile means its host to be: `type`, a workstation when it
/// does not say, so profiles from before types keep their meaning.
pub fn host_type(config: &Value) -> crate::HostType {
    config["type"]
        .as_str()
        .and_then(crate::HostType::parse)
        .unwrap_or_default()
}

/// Set the type a profile means its host to be. A workstation is left
/// unsaid, as profiles from before types are.
pub fn set_host_type(config: &mut Value, host_type: crate::HostType) {
    if !config.is_object() {
        *config = json!({});
    }
    match host_type {
        crate::HostType::Workstation => {
            if let Some(profile) = config.as_object_mut() {
                profile.remove("type");
            }
        }
        // A node has none of a workstation's accounts, so what described them goes.
        crate::HostType::Node => {
            if let Some(profile) = config.as_object_mut() {
                for field in ACCOUNT_FIELDS {
                    profile.remove(*field);
                }
                profile.insert("type".into(), json!("node"));
            }
            if config["target"] == "laptop" {
                config["target"] = json!("cloud");
            }
        }
        other => config["type"] = json!(other.name()),
    }
}

/// The fields of a profile that describe a workstation's agent and operator
/// accounts. A node has neither: sessions run as the SSH user.
pub const ACCOUNT_FIELDS: &[&str] = &[
    "agent_username",
    "agent_password_hash",
    "agent_passwordless_sudo",
    "agent_privileges",
    "agent_ssh_public_keys",
    "agent_ssh_public_key",
    "operator_username",
    "operator_password_hash",
    "operator_ssh_public_keys",
    "operator_ssh_public_key",
];

/// Whether a field of a profile says something: not absent, empty or false.
fn said(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::String(text) => !text.trim().is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(fields) => !fields.is_empty(),
        _ => true,
    }
}

/// What a target is installed on, as a node's role takes it: a server
/// unless the profile says a container.
pub fn node_target(config: &Value) -> &str {
    match config["target"].as_str() {
        Some("container") => "container",
        _ => "cloud",
    }
}

/// The profile of a host berth only logs in to or only manages: its type
/// and how it is reached, nothing of a workstation.
pub fn minimal(host_type: crate::HostType, access: &Access) -> Value {
    let mut config = json!({});
    set_host_type(&mut config, host_type);
    access.write(&mut config);
    config
}

/// Give a profile of berth 0.5 or earlier the names fields have now:
/// `level` (`reachable`, `controller`, `workstation`) becomes `type`
/// (`ssh`, `managed`, a workstation left unsaid), and `profile`, what the
/// host is installed on, becomes `target`. What a later berth wrote stays.
/// The ways of joining the tailnet berth has no more go too
/// ([`take_old_tailnet`]), so a pasted key is never read again. Says whether
/// it changed the profile.
pub fn take_old_names(config: &mut Value) -> bool {
    let mut changed = take_old_tailnet(config).is_some();
    let Some(profile) = config.as_object_mut() else {
        return false;
    };
    if let Some(level) = profile.remove("level") {
        changed = true;
        if !profile.contains_key("type") {
            match level
                .as_str()
                .map(|text| (text, crate::HostType::from_level(text)))
            {
                Some((_, Some(crate::HostType::Workstation))) => {}
                Some((_, Some(host_type))) => {
                    profile.insert("type".into(), json!(host_type.name()));
                }
                // One berth never wrote is kept, so that problems() names it.
                _ => {
                    profile.insert("type".into(), level);
                }
            }
        }
    }
    if profile.get("profile").is_some_and(Value::is_string) {
        let target = profile.remove("profile").unwrap_or_default();
        changed = true;
        profile.entry("target").or_insert(target);
    }
    changed
}

/// Whether `name` can be a setting of the role in a profile's
/// `ansible_vars`: `devbox_` and a word. Ansible's own variables, such as
/// `ansible_connection` or `ansible_ssh_common_args`, say how and where
/// commands run, which berth sets itself from `access`.
pub fn role_variable(name: &str) -> bool {
    name.strip_prefix("devbox_").is_some_and(|rest| {
        !rest.is_empty()
            && rest
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
    })
}

/// Whether `name` can be a plugin's feature, as `PLUGIN/FEATURE`.
pub fn feature_name(name: &str) -> bool {
    let part = |part: &str| {
        part.starts_with(|ch: char| ch.is_ascii_lowercase())
            && part
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' || ch == '_')
    };
    name.split_once('/')
        .is_some_and(|(plugin, feature)| part(plugin) && part(feature))
}

/// What of a profile's `ansible_vars` an apply leaves out: names that are
/// not settings of the role, and features that are not `PLUGIN/FEATURE`.
pub fn left_out(config: &Value) -> Vec<String> {
    let Some(vars) = config["ansible_vars"].as_object() else {
        return vec![];
    };
    let mut out: Vec<String> = vars
        .keys()
        .filter(|name| !role_variable(name))
        .cloned()
        .collect();
    out.extend(
        vars.get("devbox_plugin_features")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|feature| !feature.as_str().is_some_and(feature_name))
            .map(|feature| format!("the feature {feature}")),
    );
    out
}

/// What is wrong with a profile's `ansible_vars`, in sentences.
fn vars_problems(config: &Value) -> Vec<String> {
    let vars = &config["ansible_vars"];
    if vars.is_null() {
        return vec![];
    }
    let Some(vars) = vars.as_object() else {
        return vec!["ansible_vars must be an object".into()];
    };
    let mut problems = vec![];
    let foreign: Vec<&str> = vars
        .keys()
        .filter(|name| !role_variable(name))
        .map(String::as_str)
        .collect();
    if !foreign.is_empty() {
        problems.push(format!(
            "ansible_vars holds settings of the role, whose names start with devbox_, not {}. berth sets how it reaches the host from access",
            foreign.join(", ")
        ));
    }
    match vars.get("devbox_plugin_features") {
        None => {}
        Some(Value::Array(features)) => {
            let bad: Vec<String> = features
                .iter()
                .filter(|feature| !feature.as_str().is_some_and(feature_name))
                .map(Value::to_string)
                .collect();
            if !bad.is_empty() {
                problems.push(format!(
                    "devbox_plugin_features names features as PLUGIN/FEATURE, not {}",
                    bad.join(", ")
                ));
            }
        }
        Some(_) => problems.push("devbox_plugin_features must be a list".into()),
    }
    problems
}

/// What is wrong with a profile as a whole, in sentences: its type, its
/// `access` and `ansible_vars`, and for a workstation the `builder` its set-up keeps. A host
/// of another type needs only to be reachable.
pub fn problems(config: &Value) -> Vec<String> {
    if !config.is_object() {
        return vec!["A profile is an object".into()];
    }
    let mut problems = vec![];
    let host_type = match &config["type"] {
        Value::Null => Some(crate::HostType::Workstation),
        Value::String(text) => crate::HostType::parse(text),
        _ => None,
    };
    let Some(host_type) = host_type else {
        return vec![format!(
            "type is {}, not {}",
            crate::HostType::choices(),
            config["type"]
        )];
    };
    problems.extend(Access::problems(config));
    problems.extend(vars_problems(config));
    match host_type {
        crate::HostType::Workstation if !config["builder"].is_object() => {
            problems.push("A workstation's profile has a builder object".into())
        }
        crate::HostType::Workstation => {}
        _ if !Access::of(config).reachable() => problems.push(format!(
            "A host of the type {host_type} needs access.address, where berth reaches it"
        )),
        _ => {}
    }
    if host_type == crate::HostType::Node {
        let described: Vec<&str> = ACCOUNT_FIELDS
            .iter()
            .copied()
            .filter(|field| said(&config[*field]))
            .collect();
        if !described.is_empty() {
            problems.push(format!(
                "A node has no agent or operator accounts, so {} {} for a workstation: sessions on a node run as the SSH user. Remove {}, or make the host a workstation",
                described.join(", "),
                if described.len() == 1 { "is" } else { "are" },
                if described.len() == 1 { "it" } else { "them" }
            ));
        }
        if config["target"] == "laptop" {
            problems
                .push("A node is a server: its target is cloud or container, not laptop".into());
        }
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

/// Whether the host gets Tailscale and joins the tailnet, with a key berth
/// makes, which it does unless the profile says not.
pub fn joins_tailnet(config: &Value) -> bool {
    config["ansible_vars"]["devbox_install_tailscale"]
        .as_bool()
        .unwrap_or(true)
}

/// What a profile of an older berth said of joining the tailnet in ways
/// berth has no more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OldTailnet {
    /// It held an enrollment key pasted into it.
    pub pasted: bool,
    /// It was to be joined by hand: berth was not to make its key.
    pub by_hand: bool,
}

/// Take out of a profile what older berths kept of joining the tailnet: a
/// pasted enrollment key (`tailscale_auth_key`, also as the role's variable),
/// the switch for keys berth makes (`tailscale_auto_key`), which is always on
/// now, and a login server of the host's own (`tailscale_login_server`), as
/// berth makes keys only at the one of its settings. Says what there was, or
/// `None` when there was nothing.
pub fn take_old_tailnet(config: &mut Value) -> Option<OldTailnet> {
    let joins = joins_tailnet(config);
    let profile = config.as_object_mut()?;
    let filled = |value: &Option<Value>| {
        value
            .as_ref()
            .and_then(Value::as_str)
            .is_some_and(|key| !key.is_empty())
    };
    let key = profile.remove("tailscale_auth_key");
    let switch = profile.remove("tailscale_auto_key");
    let server = profile.remove("tailscale_login_server");
    let variable = profile
        .get_mut("ansible_vars")
        .and_then(Value::as_object_mut)
        .and_then(|vars| vars.remove("devbox_tailscale_auth_key"));
    if key.is_none() && switch.is_none() && server.is_none() && variable.is_none() {
        return None;
    }
    let pasted = filled(&key) || filled(&variable);
    // Older profiles stored the switch as a string too.
    let automatic = matches!(&switch, Some(Value::Bool(true)))
        || matches!(&switch, Some(Value::String(text)) if text == "true");
    Some(OldTailnet {
        pasted,
        by_hand: joins && !automatic,
    })
}

/// Where a QEMU guest on the user-mode network sees the machine it runs on.
pub const QEMU_HOST: &str = "10.0.2.2/32";

/// Make a profile of the `qemu` target there once was a `cloud` one. That
/// target let SSH in only from the QEMU host when no recovery network was named, so
/// it gets that network rather than SSH open to every address.
pub fn retire_qemu(config: &mut Value) {
    if config["target"] != "qemu" {
        return;
    }
    config["target"] = json!("cloud");
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
    fn an_address_or_user_cannot_be_an_option_of_ssh() {
        for good in ["192.0.2.10", "studio.example", "studio", "2001:db8::1"] {
            assert!(valid_address(good), "{good}");
        }
        for bad in ["", "-oProxyCommand=x", "a b", "ops@studio", "a\nb"] {
            assert!(!valid_address(bad), "{bad}");
        }
        assert!(valid_user("") && valid_user("ops") && valid_user("first.last"));
        for bad in ["-oProxyCommand=touch /tmp/x;#", "a b", "a@b", "a\tb"] {
            assert!(!valid_user(bad), "{bad}");
        }
        let problems = |access: Value| problems(&json!({"type": "ssh", "access": access}));
        assert!(problems(json!({"address": "192.0.2.10", "user": "ops"})).is_empty());
        assert_eq!(problems(json!({"address": "-oProxyCommand=x"})).len(), 1);
        assert_eq!(
            problems(json!({"address": "192.0.2.10", "user": "-oProxyCommand=x"})).len(),
            1
        );
    }

    #[test]
    fn a_host_joins_the_tailnet_unless_its_profile_says_not() {
        assert!(joins_tailnet(&json!({})));
        assert!(!joins_tailnet(
            &json!({"ansible_vars": {"devbox_install_tailscale": false}})
        ));
    }

    #[test]
    fn a_pasted_key_and_the_switch_of_older_profiles_are_dropped_on_reading() {
        let mut pasted = json!({
            "hostname": "studio",
            "tailscale_auth_key": "tskey-auth-kFAKE-0000",
            "tailscale_auto_key": false,
            "tailscale_login_server": "https://hs.example.org",
            "ansible_vars": {"devbox_geometry": "1x1", "devbox_tailscale_auth_key": "tskey-auth-kFAKE-1111"},
        });
        assert!(take_old_names(&mut pasted));
        assert_eq!(
            pasted,
            json!({"hostname": "studio", "ansible_vars": {"devbox_geometry": "1x1"}})
        );
        assert!(!take_old_names(&mut pasted));
        let said = |mut config: Value| take_old_tailnet(&mut config);
        assert_eq!(
            said(json!({"tailscale_auth_key": "tskey-auth-kFAKE-0000"})),
            Some(OldTailnet {
                pasted: true,
                by_hand: true
            })
        );
        // A host that was to be joined by hand joins with berth's key from now on.
        assert_eq!(
            said(json!({"tailscale_auto_key": false, "tailscale_auth_key": ""})),
            Some(OldTailnet {
                pasted: false,
                by_hand: true
            })
        );
        assert_eq!(
            said(json!({"tailscale_auto_key": "true"})),
            Some(OldTailnet {
                pasted: false,
                by_hand: false
            })
        );
        // Without Tailscale nobody was joining it by hand.
        assert_eq!(
            said(
                json!({"tailscale_auto_key": false, "ansible_vars": {"devbox_install_tailscale": false}})
            ),
            Some(OldTailnet {
                pasted: false,
                by_hand: false
            })
        );
        assert_eq!(
            said(json!({"tailscale_login_server": "https://hs.example.org"})),
            Some(OldTailnet {
                pasted: false,
                by_hand: true
            })
        );
        assert_eq!(said(json!({"hostname": "studio"})), None);
        assert_eq!(said(json!([])), None);
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
        // A container has no operator, and a host of another type was not set up by berth.
        let mut container =
            json!({"target": "container", "access": {"user": "ops", "sudo": "password"}});
        assert!(!settle_after_apply(&mut container));
        let mut managed = json!({"type": "managed", "access": {"address": "192.0.2.4", "user": "ops", "sudo": "password"}});
        assert!(!settle_after_apply(&mut managed));
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
    fn a_profile_without_a_type_is_a_workstation() {
        use crate::HostType;
        assert_eq!(host_type(&json!({"builder": {}})), HostType::Workstation);
        assert_eq!(host_type(&json!({"type": "managed"})), HostType::Managed);
        // One berth does not know reads as the default, and is named a problem.
        assert_eq!(host_type(&json!({"type": "server"})), HostType::Workstation);
        assert_eq!(
            problems(&json!({"type": "server"})),
            ["type is ssh, managed, node or workstation, not \"server\""]
        );
        let access = Access {
            address: "192.0.2.10".into(),
            user: "ops".into(),
            ..Access::default()
        };
        let ssh = minimal(HostType::Ssh, &access);
        assert_eq!(
            ssh,
            json!({"type": "ssh", "access": {"address": "192.0.2.10", "user": "ops", "auth": "berth"}})
        );
        assert!(problems(&ssh).is_empty());
        // Only a workstation needs the set-up's part; another type needs an address.
        assert_eq!(
            problems(&json!({"access": {"address": "studio"}})),
            ["A workstation's profile has a builder object"]
        );
        assert!(problems(&json!({"type": "managed"}))[0].contains("access.address"));
        let mut changed = ssh.clone();
        set_host_type(&mut changed, HostType::Workstation);
        assert!(changed.get("type").is_none());
    }

    #[test]
    fn a_node_has_no_accounts_of_a_workstation() {
        use crate::HostType;
        let node = json!({"type": "node", "target": "cloud",
            "access": {"address": "192.0.2.20", "user": "root"},
            "ansible_vars": {"devbox_plugin_features": ["incus/server"]}});
        assert_eq!(host_type(&node), HostType::Node);
        assert!(problems(&node).is_empty(), "{:?}", problems(&node));
        // Empty fields, as a profile from the example has them, say nothing.
        let mut quiet = node.clone();
        quiet["agent_password_hash"] = json!("");
        quiet["operator_ssh_public_keys"] = json!([]);
        assert!(problems(&quiet).is_empty());
        let mut desktop = node.clone();
        desktop["agent_username"] = json!("agent");
        desktop["operator_ssh_public_keys"] = json!(["ssh-ed25519 KEY"]);
        desktop["target"] = json!("laptop");
        let found = problems(&desktop);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found[0].starts_with(
            "A node has no agent or operator accounts, so agent_username, operator_ssh_public_keys are for a workstation"
        ));
        assert!(found[1].contains("not laptop"));
        // Making a workstation a node takes its accounts out of the profile.
        let mut workstation = desktop.clone();
        set_host_type(&mut workstation, HostType::Workstation);
        set_host_type(&mut workstation, HostType::Node);
        assert!(
            problems(&workstation).is_empty(),
            "{:?}",
            problems(&workstation)
        );
        assert_eq!(workstation["ansible_vars"], node["ansible_vars"]);
        assert_eq!(node_target(&workstation), "cloud");
        assert_eq!(node_target(&json!({"target": "container"})), "container");
    }

    #[test]
    fn a_node_applied_with_a_password_is_logged_in_to_with_berths_key_after() {
        let mut node = json!({"type": "node",
            "access": {"address": "192.0.2.20", "user": "root", "auth": "password"}});
        assert!(settle_after_apply(&mut node));
        assert_eq!(Access::of(&node).auth, Auth::Berth);
        assert!(!settle_after_apply(&mut node));
        let mut keyed = json!({"type": "node",
            "access": {"address": "192.0.2.20", "user": "root", "auth": "ssh", "sudo": "password"}});
        assert!(!settle_after_apply(&mut keyed));
    }

    #[test]
    fn a_profile_of_berth_05_takes_the_names_of_now() {
        for (level, written) in [
            ("reachable", json!("ssh")),
            ("controller", json!("managed")),
            ("workstation", Value::Null),
            ("server", json!("server")),
        ] {
            let mut old =
                json!({"level": level, "profile": "cloud", "access": {"address": "192.0.2.4"}});
            assert!(take_old_names(&mut old), "{level}");
            assert_eq!(old["type"], written, "{level}");
            assert_eq!(old["target"], "cloud");
            assert!(old.get("level").is_none() && old.get("profile").is_none());
            assert!(!take_old_names(&mut old), "{level}");
        }
        // What a later berth wrote wins, and the old field goes.
        let mut both = json!({"type": "managed", "level": "reachable", "target": "laptop", "profile": "cloud"});
        assert!(take_old_names(&mut both));
        assert_eq!(both, json!({"type": "managed", "target": "laptop"}));
        // A profile of now is left as it is.
        let mut now = json!({"type": "ssh", "target": "cloud", "builder": {}});
        assert!(!take_old_names(&mut now));
        assert_eq!(
            now,
            json!({"type": "ssh", "target": "cloud", "builder": {}})
        );
        assert!(!take_old_names(&mut json!([])));
    }

    #[test]
    fn a_qemu_profile_is_a_cloud_one_reached_from_the_qemu_host() {
        let mut old = json!({"target": "qemu"});
        retire_qemu(&mut old);
        assert_eq!(
            old,
            json!({"target": "cloud", "ansible_vars": {"devbox_bootstrap_ssh_cidr": "10.0.2.2/32"}})
        );
        // A network it names stays, and other kinds are left as they are.
        let mut named = json!({"target": "qemu", "ansible_vars": {"devbox_bootstrap_ssh_cidr": "192.0.2.0/24"}});
        retire_qemu(&mut named);
        assert_eq!(named["target"], "cloud");
        assert_eq!(
            named["ansible_vars"]["devbox_bootstrap_ssh_cidr"],
            "192.0.2.0/24"
        );
        for kept in [json!({"target": "laptop"}), json!({"target": "cloud"})] {
            let mut profile = kept.clone();
            retire_qemu(&mut profile);
            assert_eq!(profile, kept);
        }
    }

    #[test]
    fn ansible_vars_hold_settings_of_the_role_alone() {
        let with = |vars: Value| problems(&json!({"builder": {}, "ansible_vars": vars}));
        assert!(
            with(json!({"devbox_geometry": "1x1", "devbox_plugin_features": ["claude/cli"]}))
                .is_empty()
        );
        assert_eq!(
            with(
                json!({"ansible_become_exe": "/tmp/x", "ansible_python_interpreter": "/tmp/y", "devbox_ok": 1})
            ),
            [
                "ansible_vars holds settings of the role, whose names start with devbox_, not ansible_become_exe, ansible_python_interpreter. berth sets how it reaches the host from access"
            ]
        );
        for name in [
            "devbox_",
            "devbox_X",
            "devbox_a-b",
            "other",
            "ansible_shell_executable",
        ] {
            assert!(!role_variable(name), "{name}");
        }
        assert_eq!(with(json!([])), ["ansible_vars must be an object"]);
        assert_eq!(
            with(json!({"devbox_plugin_features": ["claude/cli", "../x/y", "a/../b", "plain"]})),
            [
                r#"devbox_plugin_features names features as PLUGIN/FEATURE, not "../x/y", "a/../b", "plain""#
            ]
        );
        assert_eq!(
            with(json!({"devbox_plugin_features": "claude/cli"})),
            ["devbox_plugin_features must be a list"]
        );
        assert!(feature_name("t3/server") && feature_name("my_plugin/a-b"));
    }
}
