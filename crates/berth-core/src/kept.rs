//! The profile a host keeps of itself. Every apply and every first boot leaves
//! it on the host, so that a berth that does not have the profile, or has lost
//! it, can take the host over from the host itself.

use crate::{
    Result,
    host::{self, Host},
    paths::Paths,
    provision::Record,
};
use serde_json::{Value, json};
use std::{io::Write, path::PathBuf, process::Stdio};

/// Where the role writes it, readable by root only.
pub const PATH: &str = "/etc/berth/profile.json";

/// The format of the document. A berth refuses one newer than it knows.
pub const FORMAT: u64 = 1;

/// What says how this berth reaches the host, and means nothing to another:
/// the address, user and port are given anew when a host is adopted, a key
/// file and a link command are paths and programs of this machine, and a
/// served host names this berth's server. The last two were never meant to
/// be kept.
const REACH: &[&str] = &[
    "ssh_host",
    "ssh_user",
    "ssh_port",
    "ssh_key",
    "link",
    "served",
    "served_only",
    "usb_device",
    "manage_task",
];

/// The profile as the host keeps it: as berth holds it, without what is
/// used once or describes only this berth's way to the host.
pub fn stored(config: &Value) -> Value {
    let mut config = config.clone();
    let Some(profile) = config.as_object_mut() else {
        return json!({});
    };
    // An enrollment key is used up when the host joins, and a new one is made
    // when it has to join again.
    profile.remove("tailscale_auth_key");
    if let Some(vars) = profile
        .get_mut("ansible_vars")
        .and_then(Value::as_object_mut)
    {
        vars.remove("devbox_tailscale_auth_key");
    }
    if let Some(builder) = profile.get_mut("builder").and_then(Value::as_object_mut) {
        for key in REACH {
            builder.remove(*key);
        }
    }
    config
}

/// The document the role writes, without the time of the write, which the
/// role adds. `host` is the name the host has in this berth. Only a record
/// of a machine that exists travels: one without an id names nothing yet.
pub fn document(host: &str, config: &Value, instance: Option<&Record>) -> Value {
    let mut document = json!({
        "format": FORMAT,
        "berth": berth_api::VERSION,
        "host": host,
        "profile": stored(config),
    });
    if let Some(record) = instance.filter(|record| !record.id.is_empty()) {
        document["instance"] = json!(record);
    }
    document
}

/// The document for a saved host, with its machine's record when berth
/// created the machine.
pub fn for_host(paths: &Paths, host: &Host) -> Value {
    let record = Record::load(paths, &host.name).ok().flatten();
    document(&saved_name(host), &host.config, record.as_ref())
}

/// The name a host is saved under, also for one given by the path of its profile.
fn saved_name(host: &Host) -> String {
    host.path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| host.name.clone())
}

/// Add the document to a profile handed to the seed builder, which puts it
/// where the first boot finds it. `saved` is the profile as berth holds it.
pub fn for_seed(config: &mut Value, host: &str, saved: &Value) {
    config["berth_profile"] = document(host, saved, None);
}

/// How the adopting berth reaches the host.
#[derive(Debug, Clone, Default)]
pub struct Reach {
    pub host: String,
    pub user: String,
    pub port: String,
    /// An identity file to offer as well, for reading the profile only.
    pub key: Option<PathBuf>,
}

impl Reach {
    /// `[USER@]ADDRESS`, with the port apart.
    pub fn parse(address: &str, port: &str, key: Option<&str>) -> Result<Reach> {
        let (user, host) = match address.rsplit_once('@') {
            Some((user, host)) => (user, host),
            None => ("", address),
        };
        // The same checks as for a saved host, so that what is read can be applied.
        host::inventory(host, user, port, "")?;
        if user.starts_with('-') || user.chars().any(|ch| ch.is_whitespace() || ch.is_control()) {
            return Err(format!("Not an SSH user: {user}").into());
        }
        let key = key.map(crate::discovery::expand_path);
        if let Some(key) = key.as_ref().filter(|key| !key.is_file()) {
            return Err(format!("No identity file {}", key.display()).into());
        }
        Ok(Reach {
            host: host.to_owned(),
            user: user.to_owned(),
            port: port.to_owned(),
            key,
        })
    }

    fn host(&self, own: Option<PathBuf>) -> Host {
        Host {
            name: self.host.clone(),
            path: PathBuf::new(),
            config: json!({"builder": {"ssh_host": self.host, "ssh_user": self.user, "ssh_port": self.port}}),
            key: own,
        }
    }

    pub fn target(&self) -> String {
        host::target(&self.host, &self.user)
    }
}

/// A host taken over from what it keeps.
#[derive(Debug, Clone)]
pub struct Adopted {
    /// The profile to save, reached the way it was adopted.
    pub profile: Value,
    pub instance: Option<Record>,
    /// The name and berth of the write, and when it was.
    pub host: String,
    pub berth: String,
    pub written: String,
}

impl Adopted {
    /// The secrets the profile names, by name: their values never travel.
    pub fn secrets(&self) -> Vec<String> {
        let names = self.profile["secrets"].as_array().into_iter().flatten();
        names
            .filter_map(|entry| entry["name"].as_str().map(str::to_owned))
            .collect()
    }

    /// The registries the profile names, whose servers and logins are
    /// settings of each berth.
    pub fn registries(&self) -> Vec<String> {
        let names = self.profile["registries"].as_array().into_iter().flatten();
        names
            .filter_map(|name| name.as_str().map(str::to_owned))
            .collect()
    }
}

/// The host's document read back as a profile for this berth.
pub fn adopted(kept: &Value, reach: &Reach) -> Result<Adopted> {
    let format = kept["format"]
        .as_u64()
        .ok_or("Not a profile a host keeps: it says no format")?;
    if format > FORMAT {
        return Err(format!(
            "The host keeps its profile in format {format}, which a newer berth wrote. Update berth: berth update"
        )
        .into());
    }
    let mut profile = kept["profile"].clone();
    if !profile.is_object() {
        return Err("The profile the host keeps is not a JSON object".into());
    }
    if !profile["builder"].is_object() {
        profile["builder"] = json!({});
    }
    profile["builder"]["ssh_host"] = json!(reach.host);
    for (key, value) in [("ssh_user", &reach.user), ("ssh_port", &reach.port)] {
        if !value.is_empty() {
            profile["builder"][key] = json!(value);
        }
    }
    let instance = match &kept["instance"] {
        Value::Null => None,
        record => Some(
            serde_json::from_value(record.clone())
                .map_err(|error| format!("The record of the host's machine: {error}"))?,
        ),
    };
    let text = |key: &str| kept[key].as_str().unwrap_or("").to_owned();
    Ok(Adopted {
        profile,
        instance,
        host: text("host"),
        berth: text("berth"),
        written: text("written"),
    })
}

/// What ssh said when it could not log in, with the two ways in.
fn refused(reach: &Reach, said: &str) -> String {
    format!(
        "Could not log in to {} with this berth's key, your SSH agent or your own keys: {said}\n\
         Bring over the key of the berth that set the host up: copy id_ed25519 and id_ed25519.pub from its data directory into this berth's, which is ~/.local/share/berth.\n\
         Or put this berth's public key, which berth key prints, into ~/.ssh/authorized_keys of the operator on the machine, from its console or any login you have.",
        reach.target()
    )
}

/// Read what the host keeps over SSH, as a user who can become root, and
/// without berthd, which may be down. `own` is this berth's own key.
pub fn read(
    reach: &Reach,
    own: Option<PathBuf>,
    password: &dyn Fn() -> Result<String>,
) -> Result<Value> {
    use std::io::IsTerminal;
    let host = reach.host(own);
    let command = |remote: &str| {
        let mut command = host.ssh();
        if let Some(key) = &reach.key {
            command.arg("-i").arg(key);
        }
        for option in [
            "ConnectTimeout=10",
            "PasswordAuthentication=no",
            "KbdInteractiveAuthentication=no",
        ] {
            command.args(["-o", option]);
        }
        // On a terminal ssh may ask whether to trust a host it does not know yet.
        if !std::io::stdin().is_terminal() {
            command.args(["-o", "BatchMode=yes"]);
        }
        command.arg("-T").arg(reach.target()).arg(remote);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        command
    };
    let mut output = command(&format!("sudo -n cat {PATH}")).output()?;
    let said =
        |output: &std::process::Output| String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if !output.status.success() && said(&output).contains("password is required") {
        // The password goes to sudo on standard input, never on a command line.
        let password = password()?;
        let mut asking = command(&format!("sudo -S -k -p '' cat {PATH}"));
        asking.stdin(Stdio::piped());
        let mut child = asking.spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(format!("{password}\n").as_bytes())?;
        }
        output = child.wait_with_output()?;
    }
    if !output.status.success() {
        let said = said(&output);
        return Err(match output.status.code() {
            Some(255) if said.contains("Permission denied") => refused(reach, &said),
            _ if said.contains("No such file") => format!(
                "{} keeps no profile at {PATH}. A host keeps it once this berth, or a later one, has applied it or set it up",
                reach.host
            ),
            _ => format!("Reading {PATH} on {} failed: {said}", reach.target()),
        }
        .into());
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("{PATH} on {} is not JSON: {error}", reach.host).into())
}

/// Save an adopted host under `name` in this berth's own store.
pub fn save(paths: &Paths, name: &str, adopted: &Adopted, force: bool) -> Result<PathBuf> {
    if name.contains('/') || name.ends_with(".json") {
        return Err(format!("Not a host name: {name}").into());
    }
    let path = paths.profile(name)?;
    if path.exists() && !force {
        return Err(format!(
            "There is a host {name} already. Choose another name, or replace it with --force"
        )
        .into());
    }
    paths.prepare()?;
    crate::seed::save_profile(&path, &adopted.profile)?;
    match &adopted.instance {
        Some(record) => record.save(paths, name)?,
        None => Record::remove(paths, name)?,
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply::ansible_vars;

    fn profile() -> Value {
        json!({
            "hostname": "studio", "profile": "cloud",
            "operator_username": "ops", "operator_password_hash": "$6$salt$hash",
            "operator_ssh_public_keys": ["ssh-ed25519 AAAA user"],
            "agent_username": "agent", "agent_password_hash": "$6$salt$agent",
            "tailscale_auth_key": "tskey-auth-once", "tailscale_auto_key": false,
            "tailscale_login_server": "https://hs.example.org",
            "secrets": [{"name": "github", "env": "GH_TOKEN"}],
            "registries": ["nexus"],
            "ansible_vars": {"devbox_geometry": "1920x1080", "devbox_tailscale_auth_key": "tskey-old"},
            "builder": {
                "ssh_host": "192.0.2.10", "ssh_user": "ops", "ssh_port": "2222",
                "ssh_key": "~/.ssh/studio", "link": ["docker", "exec", "-i", "studio", "berthd", "connect"],
                "served": "https://berth.example.org", "served_only": true,
                "ask_pass": false, "shipped_plugins": true,
            },
        })
    }

    #[test]
    fn what_is_used_once_or_is_this_berths_way_in_stays_out() {
        let kept = stored(&profile());
        assert!(kept.get("tailscale_auth_key").is_none());
        assert!(
            kept["ansible_vars"]
                .get("devbox_tailscale_auth_key")
                .is_none()
        );
        assert_eq!(kept["ansible_vars"]["devbox_geometry"], "1920x1080");
        assert_eq!(
            kept["builder"],
            json!({"ask_pass": false, "shipped_plugins": true})
        );
        // Hashes are in /etc/shadow already, and an agent without one would be locked.
        assert_eq!(kept["operator_password_hash"], "$6$salt$hash");
        assert_eq!(kept["agent_password_hash"], "$6$salt$agent");
        // Secrets and registries travel by name; their values are never in a profile.
        assert_eq!(kept["secrets"], profile()["secrets"]);
        assert_eq!(kept["registries"], json!(["nexus"]));
        assert!(!kept.to_string().contains("tskey"));
    }

    #[test]
    fn only_the_record_of_a_machine_that_exists_travels() {
        let mut record = Record {
            provisioner: "incus/vm".into(),
            token: "0123abcd".into(),
            ..Record::default()
        };
        let none = document("studio", &profile(), Some(&record));
        assert!(none.get("instance").is_none());
        record.id = "berth-studio".into();
        let made = document("studio", &profile(), Some(&record));
        assert_eq!(made["instance"]["id"], "berth-studio");
        assert_eq!(
            (made["format"].as_u64(), made["host"].as_str()),
            (Some(FORMAT), Some("studio"))
        );
        assert_eq!(made["berth"], berth_api::VERSION);
    }

    #[test]
    fn a_kept_profile_read_back_applies_as_the_original() {
        let original = profile();
        let mut kept = document("studio", &original, None);
        kept["written"] = json!("2026-11-13T10:00:00Z");
        let reach = Reach::parse("ops@192.0.2.10", "2222", None).unwrap();
        let back = adopted(&kept, &reach).unwrap();
        let dirs = json!({"demo": "/p/demo"});
        let own = Some("ssh-ed25519 OWN berth");
        let mut wanted = ansible_vars(&original, &dirs, own);
        // What is used once is not applied again.
        wanted
            .as_object_mut()
            .unwrap()
            .remove("devbox_tailscale_auth_key");
        assert_eq!(ansible_vars(&back.profile, &dirs, own), wanted);
        // Another berth applies the same keys of the profile and gives its own
        // apart, in a block of its own, so the first berth's block stays.
        let other = ansible_vars(&back.profile, &dirs, Some("ssh-ed25519 NEW berth"));
        assert_eq!(
            other["devbox_operator_ssh_public_key"],
            wanted["devbox_operator_ssh_public_key"]
        );
        assert_eq!(
            other["devbox_berth_ssh_public_key"],
            "ssh-ed25519 NEW berth"
        );
        assert_eq!(back.profile["builder"]["ssh_host"], "192.0.2.10");
        assert_eq!(back.profile["builder"]["ssh_user"], "ops");
        assert_eq!(back.profile["builder"]["ssh_port"], "2222");
        assert!(back.profile["builder"].get("ssh_key").is_none());
        assert_eq!(
            (back.host.as_str(), back.written.as_str()),
            ("studio", "2026-11-13T10:00:00Z")
        );
        assert_eq!(back.secrets(), ["github"]);
        assert_eq!(back.registries(), ["nexus"]);
        // Kept again, it is the same document.
        assert_eq!(
            document("studio", &back.profile, None),
            document("studio", &original, None)
        );
    }

    #[test]
    fn a_newer_format_or_no_profile_is_refused() {
        let reach = Reach::parse("192.0.2.10", "", None).unwrap();
        assert!(adopted(&json!({"format": FORMAT + 1, "profile": {}}), &reach).is_err());
        assert!(adopted(&json!({"format": 1, "profile": []}), &reach).is_err());
        assert!(adopted(&json!({"profile": {}}), &reach).is_err());
        let back = adopted(&json!({"format": 1, "profile": {}}), &reach).unwrap();
        assert_eq!(back.profile["builder"], json!({"ssh_host": "192.0.2.10"}));
        assert!(Reach::parse("-oProxyCommand=x", "", None).is_err());
        assert!(Reach::parse("-o@192.0.2.10", "", None).is_err());
        assert!(Reach::parse("192.0.2.10", "0", None).is_err());
    }

    #[test]
    fn an_adopted_host_is_saved_with_its_record_and_not_over_another() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: home.path().join("config"),
            data_dir: home.path().join("data"),
        };
        let reach = Reach::parse("192.0.2.10", "", None).unwrap();
        let record = Record {
            provisioner: "incus/vm".into(),
            id: "berth-studio".into(),
            token: "0123abcd".into(),
            ..Record::default()
        };
        let kept = document("old", &profile(), Some(&record));
        let back = adopted(&kept, &reach).unwrap();
        let path = save(&paths, "studio", &back, false).unwrap();
        let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved, back.profile);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o077,
            0
        );
        assert_eq!(Record::load(&paths, "studio").unwrap(), Some(record));
        let again = save(&paths, "studio", &back, false).unwrap_err();
        assert!(again.to_string().contains("--force"), "{again}");
        assert!(save(&paths, "studio", &back, true).is_ok());
        assert!(save(&paths, "../studio", &back, true).is_err());
    }

    #[test]
    fn a_refused_login_says_both_ways_in() {
        let reach = Reach::parse("ops@192.0.2.10", "", None).unwrap();
        let said = refused(&reach, "ops@192.0.2.10: Permission denied (publickey).");
        assert!(said.contains("id_ed25519"), "{said}");
        assert!(said.contains("berth key"), "{said}");
        assert!(said.contains("authorized_keys"), "{said}");
    }
}
