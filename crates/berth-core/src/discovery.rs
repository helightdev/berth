use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub type Choices = Vec<(String, String)>;

pub fn expand_path(value: &str) -> PathBuf {
    if let Some(rest) = value.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(value)
}

pub fn is_public_key(value: &str) -> bool {
    let mut parts = value.split_whitespace();
    matches!(
        parts.next(),
        Some("ssh-ed25519" | "ssh-rsa" | "ecdsa-sha2-nistp256")
    ) && parts.next().is_some()
        && !value.contains(['\n', '\r'])
}

pub fn keys(directory: &Path) -> (Choices, Choices) {
    let mut public = vec![];
    let mut private = vec![];
    let Ok(entries) = fs::read_dir(directory) else {
        return (public, private);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = path.metadata() else {
            continue;
        };
        if !metadata.is_file() || metadata.len() > 65536 {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        if path.extension().is_some_and(|ext| ext == "pub") && is_public_key(content.trim()) {
            public.push((path.display().to_string(), content.trim().into()));
        } else if content.lines().next().is_some_and(|line| {
            line.starts_with("-----BEGIN ") && line.ends_with("PRIVATE KEY-----")
        }) {
            private.push((path.display().to_string(), path.display().to_string()));
        }
    }
    public.sort();
    private.sort();
    (public, private)
}

pub fn agent_keys() -> Choices {
    if std::env::var_os("SSH_AUTH_SOCK").is_none() {
        return vec![];
    }
    let Ok(output) = Command::new("ssh-add")
        .arg("-L")
        .stdin(Stdio::null())
        .output()
    else {
        return vec![];
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| is_public_key(line))
        .map(|line| {
            (
                format!(
                    "ssh-agent · {}",
                    line.split_whitespace().nth(2).unwrap_or("key")
                ),
                line.to_owned(),
            )
        })
        .collect()
}

pub fn hosts(config: &Path) -> Choices {
    let mut aliases = BTreeSet::new();
    read_hosts(config, &mut BTreeSet::new(), &mut aliases);
    aliases
        .into_iter()
        .map(|alias| (alias.clone(), alias))
        .collect()
}

fn read_hosts(path: &Path, seen: &mut BTreeSet<PathBuf>, aliases: &mut BTreeSet<String>) {
    if seen.len() >= 64 || !seen.insert(path.to_owned()) {
        return;
    }
    let Ok(text) = fs::read_to_string(path) else {
        return;
    };
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").replace('=', " ");
        let mut words = line.split_whitespace();
        match words.next().unwrap_or("").to_ascii_lowercase().as_str() {
            "host" => {
                for alias in words {
                    if !alias.contains(['*', '?', '!']) {
                        aliases.insert(alias.to_owned());
                    }
                }
            }
            "include" => {
                for file in words {
                    let expanded = expand_path(file.trim_matches('"'));
                    let included = if expanded.is_absolute() {
                        expanded
                    } else {
                        path.parent().unwrap_or(Path::new(".")).join(expanded)
                    };
                    if included.to_string_lossy().contains('*') {
                        // Common config.d/* layout. Match the filename's prefix and suffix.
                        let Some(name) = included.file_name().and_then(|name| name.to_str()) else {
                            continue;
                        };
                        let Some((prefix, suffix)) = name.split_once('*') else {
                            continue;
                        };
                        if let Ok(entries) =
                            fs::read_dir(included.parent().unwrap_or(Path::new(".")))
                        {
                            for entry in entries.flatten() {
                                let name = entry.file_name();
                                let name = name.to_string_lossy();
                                if name.starts_with(prefix) && name.ends_with(suffix) {
                                    read_hosts(&entry.path(), seen, aliases);
                                }
                            }
                        }
                    } else {
                        read_hosts(&included, seen, aliases);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Linux machines on this machine's tailnet, as (name and address, address).
pub fn tailnet_hosts() -> Choices {
    let Ok(output) = Command::new("tailscale")
        .args(["status", "--json"])
        .stdin(Stdio::null())
        .output()
    else {
        return vec![];
    };
    let Ok(status) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return vec![];
    };
    let mut hosts: Choices = status["Peer"]
        .as_object()
        .into_iter()
        .flat_map(|peers| peers.values())
        .filter(|peer| peer["OS"] == "linux")
        .filter_map(|peer| {
            let address = peer["TailscaleIPs"][0].as_str()?;
            let name = peer["HostName"].as_str()?;
            Some((format!("{name} · {address} (tailnet)"), address.to_owned()))
        })
        .collect();
    hosts.sort();
    hosts
}

/// Connected USB drives, as (description, device path). Whole disks only.
pub fn usb_drives() -> Choices {
    let Ok(output) = Command::new("lsblk")
        .args(["-dnP", "-o", "PATH,TYPE,TRAN,SIZE,MODEL"])
        .stdin(Stdio::null())
        .output()
    else {
        return vec![];
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let value = |key: &str| {
                let rest = &line[line.find(&format!("{key}=\""))? + key.len() + 2..];
                Some(rest[..rest.find('"')?].trim())
            };
            // An ejected stick is still listed, with no medium and so no size.
            if value("TYPE")? != "disk" || value("TRAN")? != "usb" || value("SIZE")? == "0B" {
                return None;
            }
            let path = value("PATH")?;
            let model = value("MODEL").filter(|model| !model.is_empty());
            Some((
                format!(
                    "{path} · {} · {}",
                    model.unwrap_or("USB drive"),
                    value("SIZE")?
                ),
                path.to_owned(),
            ))
        })
        .collect()
}

/// Wi-Fi networks saved in NetworkManager on this machine, as (SSID, password).
/// The password is empty for open networks and when it cannot be read.
///
/// Saved passwords are only readable with administrator rights. `privileged` says
/// sudo was authorised at start-up; without it only the names are read, because
/// asking NetworkManager for secrets would pop up its own authorisation prompt.
pub fn wifi_networks(privileged: bool) -> Choices {
    let nmcli = |args: &[&str]| {
        let mut command = if privileged {
            let mut sudo = Command::new("sudo");
            sudo.args(["-n", "nmcli"]);
            sudo
        } else {
            Command::new("nmcli")
        };
        command
            .args(["-w", "3"])
            .args(args)
            .stdin(Stdio::null())
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
    };
    let Some(connections) = nmcli(&["-t", "-f", "UUID,TYPE", "connection", "show"]) else {
        return vec![];
    };
    let mut networks: Choices = vec![];
    for line in connections.lines() {
        let Some((uuid, "802-11-wireless")) = line.split_once(':') else {
            continue;
        };
        let details = if privileged {
            let fields = "802-11-wireless.ssid,802-11-wireless-security.psk";
            nmcli(&["-s", "-g", fields, "connection", "show", uuid])
        } else {
            nmcli(&["-g", "802-11-wireless.ssid", "connection", "show", uuid])
        };
        let Some(details) = details else {
            continue;
        };
        let mut values = details.lines();
        let ssid = values.next().unwrap_or("").to_owned();
        let password = values.next().unwrap_or("").to_owned();
        if !ssid.is_empty() && !networks.iter().any(|(known, _)| *known == ssid) {
            networks.push((ssid, password));
        }
    }
    networks.sort();
    networks
}
