//! The profile a host keeps of itself. Every apply and every first boot leaves
//! it on the host, so that a berth that does not have the profile, or has lost
//! it, can take the host over from the host itself: `berth adopt`.

use crate::{
    Result,
    keys::{self, Login},
    paths::Paths,
    profiles::Place,
    resource,
};
use berth_api::profile::{Access, Auth};
use serde_json::{Value, json};
use std::{io::Write, path::PathBuf, process::Stdio};

/// Where the role writes it, readable by root only.
pub const PATH: &str = "/etc/berth/profile.json";

/// The format of the document: 2 has the profile in the layout with
/// `access`. Format 1, of berth 0.4.1, had the layout before it; it is
/// converted when it is read. A berth refuses one newer than it knows.
pub const FORMAT: u64 = 2;

/// What a profile's `builder` held of how one berth reached the host, or of
/// that berth's own machine, in the layout before `access`, and what still
/// belongs to this machine alone: a served host names this berth's server,
/// and a USB drive and the last task were never meant to be kept.
const LOCAL_BUILDER: &[&str] = &[
    "ssh_host",
    "ssh_user",
    "ssh_port",
    "ssh_key",
    "ask_become_pass",
    "ask_pass",
    "link",
    "served",
    "served_only",
    "usb_device",
    "manage_task",
];

/// The profile as the host keeps it: as berth holds it, without what is
/// used once or describes only this berth's way to the host. `access` stays
/// out whole: whoever adopts the host gives its address, user and port
/// anew, a key name means a key of this berth's ring, and a link command is
/// a program of this machine.
pub fn stored(config: &Value) -> Value {
    let mut config = config.clone();
    let Some(profile) = config.as_object_mut() else {
        return json!({});
    };
    // An enrollment key is used up when the host joins, and a new one is made
    // when it has to join again.
    profile.remove("tailscale_auth_key");
    profile.remove("access");
    // The operator always has sudo without a password and berth's key.
    profile.remove("operator_passwordless_sudo");
    profile.remove("berth_ssh_key");
    if let Some(vars) = profile
        .get_mut("ansible_vars")
        .and_then(Value::as_object_mut)
    {
        vars.remove("devbox_tailscale_auth_key");
    }
    if let Some(builder) = profile.get_mut("builder").and_then(Value::as_object_mut) {
        for key in LOCAL_BUILDER {
            builder.remove(*key);
        }
    }
    config
}

/// The document the role writes, without the time of the write, which the
/// role adds. `host` is the name the host has in this berth. Only the record
/// of a machine that exists travels: one without the provisioner's id names
/// nothing yet. What says when berth last looked at it stays here.
pub fn document(host: &str, config: &Value, machine: Option<&resource::Record>) -> Value {
    let mut document = json!({
        "format": FORMAT,
        "berth": berth_api::VERSION,
        "host": host,
        "profile": stored(config),
    });
    if let Some(record) = machine.filter(|record| !record.external_id.is_empty()) {
        let mut kept = json!(record);
        if let Some(kept) = kept.as_object_mut() {
            for key in ["host", "asked", "state", "checked"] {
                kept.remove(key);
            }
        }
        document["machine"] = kept;
    }
    document
}

/// The document for a saved host, with its machine's record when berth
/// created the machine.
pub fn for_host(paths: &Paths, host: &crate::host::Host) -> Value {
    let machine = resource::of_place(paths, &host.place)
        .ok()
        .flatten()
        .map(|kept| kept.record);
    document(&host.place.title(), &host.config, machine.as_ref())
}

/// Add the document to a profile handed to the seed builder, which puts it
/// where the first boot finds it. `saved` is the profile as berth holds it.
pub fn for_seed(config: &mut Value, host: &str, saved: &Value) {
    config["berth_profile"] = document(host, saved, None);
}

/// How the adopting berth reaches the host.
#[derive(Debug, Clone, Default)]
pub struct Reach {
    pub address: String,
    pub user: String,
    pub port: String,
    /// A private key file to offer as well, for reading the profile only.
    pub key: Option<PathBuf>,
}

impl Reach {
    /// `[USER@]ADDRESS`, with the port apart.
    pub fn parse(address: &str, port: &str, key: Option<&str>) -> Result<Reach> {
        let (user, address) = match address.rsplit_once('@') {
            Some((user, address)) => (user, address),
            None => ("", address),
        };
        // The same checks as for a saved host, so that what is read can be applied.
        keys::check(address, port)?;
        if !berth_api::profile::valid_user(user) {
            return Err(format!("Not an SSH user: {user}").into());
        }
        let key = key.map(crate::discovery::expand_path);
        if let Some(key) = key.as_ref().filter(|key| !key.is_file()) {
            return Err(format!("No identity file {}", key.display()).into());
        }
        Ok(Reach {
            address: address.to_owned(),
            user: user.to_owned(),
            port: port.to_owned(),
            key,
        })
    }

    /// Where a saved host is reached, as its profile says.
    pub fn of(access: &Access) -> Reach {
        Reach {
            address: access.address.clone(),
            user: access.user.clone(),
            port: access.port.clone(),
            key: None,
        }
    }

    /// `user@address`, or the address alone.
    pub fn target(&self) -> String {
        crate::host::target(&self.address, &self.user)
    }

    fn access(&self, auth: Auth) -> Access {
        Access {
            address: self.address.clone(),
            user: self.user.clone(),
            port: self.port.clone(),
            auth,
            ..Access::default()
        }
    }
}

/// A host taken over from what it keeps.
#[derive(Debug, Clone)]
pub struct Adopted {
    /// The profile to save, reached the way it was adopted.
    pub profile: Value,
    pub machine: Option<resource::Record>,
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

/// What berth 0.4.1 kept of the host's machine, beside the profile.
#[derive(serde::Deserialize)]
struct Instance {
    provisioner: String,
    #[serde(default)]
    id: String,
    token: String,
    #[serde(default)]
    arguments: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    created: String,
}

/// The host's document read back as a profile for this berth, reached with
/// `access`: how the adopting berth logged in, not how the one before did.
pub fn adopted(paths: &Paths, title: &str, kept: &Value, access: &Access) -> Result<Adopted> {
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
    // berth 0.4.1 kept a profile of the layout before `access`; what is left
    // of that becomes the new layout, and then the adopting berth's way in.
    crate::import::convert_profile(paths, title, &mut profile);
    profile = stored(&profile);
    if !profile["builder"].is_object() {
        profile["builder"] = json!({});
    }
    access.write(&mut profile);
    let machine = match (&kept["machine"], &kept["instance"]) {
        (Value::Null, Value::Null) => None,
        (Value::Null, old) => {
            let old: Instance = serde_json::from_value(old.clone())
                .map_err(|error| format!("The record of the host's machine: {error}"))?;
            Some(resource::Record {
                id: resource::new_id()?,
                provisioner: old.provisioner,
                kind: crate::plugins::MACHINE.into(),
                external_id: old.id,
                token: old.token,
                arguments: old.arguments,
                created: old.created,
                ..resource::Record::default()
            })
        }
        (record, _) => Some(
            serde_json::from_value(record.clone())
                .map_err(|error| format!("The record of the host's machine: {error}"))?,
        ),
    };
    let text = |key: &str| kept[key].as_str().unwrap_or("").to_owned();
    Ok(Adopted {
        profile,
        machine,
        host: text("host"),
        berth: text("berth"),
        written: text("written"),
    })
}

/// What ssh said when it could not log in, with the ways in.
fn refused(reach: &Reach, said: &str) -> String {
    format!(
        "Could not log in to {} with this berth's key, your SSH agent or your own keys: {said}\n\
         Give the private key of the berth that set the host up with --key FILE: the file keys/berth in its data directory, or id_ed25519 there for a berth before 0.5.\n\
         Or put this berth's public key, which berth key show prints, into ~/.ssh/authorized_keys of the operator on the machine, from its console or any login you have.",
        reach.target()
    )
}

/// What a read of the kept profile found, and how it got in.
#[derive(Debug)]
pub struct Read {
    pub kept: Value,
    /// `berth` when berth's own key let it in, `ssh` when the user's SSH
    /// configuration, agent or a key given once did.
    pub auth: Auth,
    /// Whether sudo asked for a password.
    pub sudo_asked: bool,
}

/// Run `remote` over SSH with `login`, with `input` on its standard input.
fn run(login: &Login, remote: &str, input: Option<&str>) -> Result<std::process::Output> {
    let mut command = login.batch_ssh();
    command.arg(login.target()).arg(remote);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let Some(input) = input else {
        return Ok(command.output()?);
    };
    command.stdin(Stdio::piped());
    let mut child = command.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(input.as_bytes())?;
    }
    Ok(child.wait_with_output()?)
}

fn said(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).trim().to_owned()
}

/// Whether ssh itself was refused, rather than the command it ran failing.
fn login_refused(output: &std::process::Output) -> bool {
    output.status.code() == Some(255) && said(output).contains("Permission denied")
}

/// Read what the host keeps over SSH, as a user who can become root, and
/// without berthd, which may be down. berth's own key is tried first; when
/// the host refuses it, the user's SSH configuration and agent are, with the
/// key given once. `place` is where the host is or will be saved, whose
/// known host key the login records.
pub fn read(
    paths: &Paths,
    place: &Place,
    reach: &Reach,
    password: &dyn Fn() -> Result<String>,
) -> Result<Read> {
    let store = crate::store::shared(paths)?;
    let login_with = |auth: Auth| -> Result<Login> {
        let mut config = json!({"builder": {}});
        reach.access(auth).write(&mut config);
        keys::login(paths, &store, place, &config)
    };
    let mut auth = Auth::Berth;
    let mut login = login_with(Auth::Berth)?;
    let look = format!("sudo -n cat {PATH}");
    let mut output = run(&login, &look, None)?;
    if login_refused(&output) {
        login.noted(Some(&said(&output)));
        auth = Auth::Ssh;
        login = login_with(Auth::Ssh)?;
        if let Some(key) = &reach.key {
            login = login.offering(key.clone());
        }
        output = run(&login, &look, None)?;
    }
    let mut sudo_asked = false;
    if !output.status.success() && said(&output).contains("password is required") {
        // The password goes to sudo on standard input, never on a command line.
        let password = password()?;
        sudo_asked = true;
        output = run(
            &login,
            &format!("sudo -S -k -p '' cat {PATH}"),
            Some(&format!("{password}\n")),
        )?;
    }
    if !output.status.success() {
        let said = said(&output);
        return Err(match () {
            _ if login_refused(&output) => login.noted(Some(&said)).unwrap_or_else(|| refused(reach, &said)),
            _ if said.contains("No such file") => format!(
                "{} keeps no profile at {PATH}. A host keeps it once a berth of version 0.4.1 or later has applied it or set it up",
                reach.address
            ),
            _ => format!("Reading {PATH} on {} failed: {said}", reach.target()),
        }
        .into());
    }
    login.noted(None);
    // The host key the login recorded is the host's from now on.
    let _ = keys::known::absorb(paths, &store, place);
    let kept = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("{PATH} on {} is not JSON: {error}", reach.address))?;
    Ok(Read {
        kept,
        auth,
        sudo_asked,
    })
}

/// Save an adopted host at `place`, with the record of its machine in place
/// of one kept there before.
pub fn save(paths: &Paths, place: &Place, adopted: &Adopted) -> Result<()> {
    place.save(paths, &adopted.profile)?;
    if let Some(old) = resource::of_place(paths, place)? {
        old.forget(paths)?;
    }
    if let Some(record) = &adopted.machine {
        let (scope, host) = place.side();
        let mut record = record.clone();
        record.host = host;
        record.name = place.title();
        // An id this berth has already, or none of its making, is not taken over.
        let taken = resource::list(paths)?
            .iter()
            .any(|kept| kept.record.id == record.id);
        if taken || !resource::valid_id(&record.id) {
            record.id = resource::new_id()?;
        }
        resource::Kept { scope, record }.save(paths)?;
    }
    Ok(())
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
            "access": {
                "address": "192.0.2.10", "user": "ops", "port": "2222",
                "auth": "key:studio", "sudo": "none",
            },
            "builder": {
                "served": "https://berth.example.org", "served_only": true,
                "shipped_plugins": true,
            },
        })
    }

    fn scratch() -> (tempfile::TempDir, Paths) {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: home.path().join("config"),
            data_dir: home.path().join("data"),
        };
        paths.prepare().unwrap();
        (home, paths)
    }

    fn reached() -> Access {
        Reach::parse("ops@192.0.2.10", "2222", None)
            .unwrap()
            .access(Auth::Berth)
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
        assert_eq!(kept["builder"], json!({"shipped_plugins": true}));
        // How this berth logs in, and the name of a key of its ring, mean nothing to another.
        assert!(kept.get("access").is_none());
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
        let mut record = resource::Record {
            id: "r-abcdefgh".into(),
            provisioner: "incus/vm".into(),
            kind: "machine".into(),
            host: "studio".into(),
            token: "0123abcd".into(),
            state: "running".into(),
            ..resource::Record::default()
        };
        let none = document("studio", &profile(), Some(&record));
        assert!(none.get("machine").is_none());
        record.external_id = "berth-studio".into();
        let made = document("studio", &profile(), Some(&record));
        assert_eq!(made["machine"]["external_id"], "berth-studio");
        assert!(made["machine"].get("host").is_none() && made["machine"].get("state").is_none());
        assert_eq!(
            (made["format"].as_u64(), made["host"].as_str()),
            (Some(FORMAT), Some("studio"))
        );
        assert_eq!(made["berth"], berth_api::VERSION);
    }

    #[test]
    fn a_kept_profile_read_back_applies_as_the_original() {
        let (_home, paths) = scratch();
        let original = profile();
        let mut kept = document("studio", &original, None);
        kept["written"] = json!("2026-11-13T10:00:00Z");
        let back = adopted(&paths, "studio", &kept, &reached()).unwrap();
        let dirs = json!({"demo": "/p/demo"});
        let own = ["ssh-ed25519 OWN berth".to_owned()];
        let mut wanted = ansible_vars(&original, &dirs, &own);
        // What is used once is not applied again.
        wanted
            .as_object_mut()
            .unwrap()
            .remove("devbox_tailscale_auth_key");
        assert_eq!(ansible_vars(&back.profile, &dirs, &own), wanted);
        // Another berth applies the same keys of the profile and gives its own
        // apart, in a block of its own, so the first berth's block stays.
        let other = ansible_vars(&back.profile, &dirs, &["ssh-ed25519 NEW berth".to_owned()]);
        assert_eq!(
            other["devbox_operator_ssh_public_key"],
            wanted["devbox_operator_ssh_public_key"]
        );
        assert_eq!(
            other["devbox_berth_ssh_public_key"],
            "ssh-ed25519 NEW berth"
        );
        // It is reached as the adopting berth reached it.
        assert_eq!(
            back.profile["access"],
            json!({"address": "192.0.2.10", "user": "ops", "port": "2222", "auth": "berth"})
        );
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
    fn a_profile_kept_by_berth_041_is_taken_in_the_new_layout() {
        let (_home, paths) = scratch();
        // What berth 0.4.1 wrote: the layout before access, with what it left
        // of builder, and the record of the machine beside it.
        let kept = json!({
            "format": 1, "berth": "0.4.1", "host": "studio", "written": "2026-11-13T10:00:00Z",
            "profile": {
                "hostname": "studio", "profile": "cloud", "operator_username": "ops",
                "operator_ssh_public_keys": ["ssh-ed25519 AAAA user"],
                "builder": {"ask_become_pass": true, "ask_pass": false, "shipped_plugins": true},
            },
            "instance": {"provisioner": "incus/vm", "id": "berth-studio", "token": "0123abcd",
                         "arguments": {"server": "https://192.0.2.20:8443"}, "created": "2026-11-01T09:00:00Z"},
        });
        let back = adopted(&paths, "studio", &kept, &reached()).unwrap();
        assert_eq!(back.profile["builder"], json!({"shipped_plugins": true}));
        assert_eq!(back.profile["access"]["address"], "192.0.2.10");
        assert_eq!(back.profile["access"]["auth"], "berth");
        // sudo is how the adopting berth found it, not what the old switch said.
        assert!(back.profile["access"].get("sudo").is_none());
        let machine = back.machine.unwrap();
        assert_eq!(
            (
                machine.external_id.as_str(),
                machine.kind.as_str(),
                machine.token.as_str()
            ),
            ("berth-studio", "machine", "0123abcd")
        );
        assert!(resource::valid_id(&machine.id));
        assert_eq!(machine.arguments["server"], "https://192.0.2.20:8443");
    }

    #[test]
    fn a_newer_format_or_no_profile_is_refused() {
        let (_home, paths) = scratch();
        let access = Reach::parse("192.0.2.10", "", None)
            .unwrap()
            .access(Auth::Ssh);
        let adopt = |kept: Value| adopted(&paths, "studio", &kept, &access);
        assert!(adopt(json!({"format": FORMAT + 1, "profile": {}})).is_err());
        assert!(adopt(json!({"format": 1, "profile": []})).is_err());
        assert!(adopt(json!({"profile": {}})).is_err());
        let back = adopt(json!({"format": 2, "profile": {}})).unwrap();
        assert_eq!(back.profile["builder"], json!({}));
        assert_eq!(
            back.profile["access"],
            json!({"address": "192.0.2.10", "auth": "ssh"})
        );
        assert!(Reach::parse("-oProxyCommand=x", "", None).is_err());
        assert!(Reach::parse("-o@192.0.2.10", "", None).is_err());
        assert!(Reach::parse("192.0.2.10", "0", None).is_err());
        assert!(Reach::parse("192.0.2.10", "", Some("/nonexistent/key")).is_err());
    }

    #[test]
    fn an_adopted_host_is_saved_with_its_machine_in_place_of_the_old_one() {
        let (_home, paths) = scratch();
        let record = resource::Record {
            id: "r-abcdefgh".into(),
            provisioner: "incus/vm".into(),
            kind: "machine".into(),
            external_id: "berth-studio".into(),
            token: "0123abcd".into(),
            ..resource::Record::default()
        };
        let kept = document("old", &profile(), Some(&record));
        let back = adopted(&paths, "studio", &kept, &reached()).unwrap();
        let place = Place::own("studio").unwrap();
        save(&paths, &place, &back).unwrap();
        assert_eq!(place.load(&paths).unwrap().unwrap(), back.profile);
        let machine = resource::of_place(&paths, &place).unwrap().unwrap();
        assert_eq!(machine.record.id, "r-abcdefgh");
        assert_eq!(
            (machine.record.host.as_str(), machine.record.name.as_str()),
            ("studio", "studio")
        );
        // Saved again under another name, the id this berth has already is not taken twice.
        let attic = Place::own("attic").unwrap();
        save(&paths, &attic, &back).unwrap();
        let other = resource::of_place(&paths, &attic).unwrap().unwrap();
        assert_ne!(other.record.id, "r-abcdefgh");
        // Adopted again without a machine, the old record goes.
        let bare = adopted(
            &paths,
            "studio",
            &document("old", &profile(), None),
            &reached(),
        )
        .unwrap();
        save(&paths, &place, &bare).unwrap();
        assert!(resource::of_place(&paths, &place).unwrap().is_none());
    }

    #[test]
    fn a_refused_login_says_the_ways_in() {
        let reach = Reach::parse("ops@192.0.2.10", "", None).unwrap();
        let said = refused(&reach, "ops@192.0.2.10: Permission denied (publickey).");
        assert!(said.contains("--key"), "{said}");
        assert!(said.contains("berth key show"), "{said}");
        assert!(said.contains("authorized_keys"), "{said}");
    }
}
