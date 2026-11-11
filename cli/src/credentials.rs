use crate::Result;
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    path::Path,
    process::{Command, Stdio},
};

fn random_hex(bytes: usize) -> Result<String> {
    let mut data = vec![0; bytes];
    fs::File::open("/dev/urandom")?.read_exact(&mut data)?;
    Ok(data.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Authorise sudo for this terminal session, with `password` or, when `None`,
/// only if no password is needed (cached or NOPASSWD). On success the
/// authorisation is kept alive until the program exits.
pub fn sudo_unlock(password: Option<&str>) -> bool {
    let mut command = Command::new("sudo");
    match password {
        Some(_) => command.args(["-S", "-p", "", "-v"]).stdin(Stdio::piped()),
        None => command.args(["-n", "-v"]).stdin(Stdio::null()),
    };
    let Ok(mut child) = command.stdout(Stdio::null()).stderr(Stdio::null()).spawn() else {
        return false;
    };
    if let (Some(password), Some(mut stdin)) = (password, child.stdin.take()) {
        // A wrong password makes sudo ask again; the closed pipe ends that.
        let _ = stdin.write_all(format!("{password}\n").as_bytes());
    }
    if !child.wait().is_ok_and(|status| status.success()) {
        return false;
    }
    std::thread::spawn(|| {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(240));
            let refreshed = Command::new("sudo")
                .args(["-n", "-v"])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .status();
            if !refreshed.is_ok_and(|status| status.success()) {
                break;
            }
        }
    });
    true
}

pub fn hostname() -> Result<String> {
    Ok(format!("devbox-{}", random_hex(5)?))
}
pub fn password() -> Result<String> {
    random_hex(18)
}

pub fn hash(password: &str) -> Result<String> {
    if password.is_empty() || password.contains(['\n', '\r', '\0']) {
        return Err("Enter a non-empty single-line password".into());
    }
    let mut child = Command::new("openssl")
        .args(["passwd", "-6", "-stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child.stdin.take().unwrap().write_all(password.as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned().into());
    }
    let hash = String::from_utf8(output.stdout)?.trim().to_owned();
    if !hash.starts_with("$6$") {
        return Err("Password hashing failed".into());
    }
    Ok(hash)
}

pub fn save(path: &Path, user: &str, password: &str) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let mut data: Value = match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(error) => return Err(error.into()),
    };
    data.as_object_mut()
        .ok_or("Credential file must contain an object")?
        .insert(user.into(), json!(password));
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut file, &data)?;
    file.persist(path)?;
    Ok(())
}

/// SHA-256 of a file, using coreutils so large images do not need a hashing dependency.
pub fn sha256(path: &Path) -> Result<String> {
    let output = Command::new("sha256sum").arg("--").arg(path).output()?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr)
            .trim()
            .to_owned()
            .into());
    }
    let text = String::from_utf8(output.stdout)?;
    let digest = text.split_whitespace().next().unwrap_or("");
    if digest.len() != 64 {
        return Err("sha256sum returned no digest".into());
    }
    Ok(digest.to_owned())
}

/// Create a one-use, pre-approved Tailscale auth key through the Tailscale API.
///
/// The `tailscale` command cannot create keys itself, so this needs an API access
/// token from the admin console in `TS_API_KEY`. Tailnets that only allow tagged
/// keys can name the tags in `DEVBOX_TAILSCALE_TAGS` (comma separated).
pub fn tailscale_auth_key(description: &str) -> Result<String> {
    let token = std::env::var("TS_API_KEY")
        .ok()
        .filter(|token| !token.is_empty() && !token.contains(['"', '\\', '\n']))
        .ok_or("Set TS_API_KEY to a Tailscale API access token to create keys here")?;
    let tags: Vec<String> = std::env::var("DEVBOX_TAILSCALE_TAGS")
        .unwrap_or_default()
        .split(',')
        .map(|tag| tag.trim().to_owned())
        .filter(|tag| !tag.is_empty())
        .collect();
    let mut create = json!({"reusable": false, "ephemeral": false, "preauthorized": true});
    if !tags.is_empty() {
        create["tags"] = json!(tags);
    }
    let request = json!({
        "capabilities": {"devices": {"create": create}},
        // Long enough to write the installer and let the first boot enroll.
        "expirySeconds": 24 * 60 * 60,
        "description": description,
    });
    let body = tempfile::NamedTempFile::new()?;
    serde_json::to_writer(&body, &request)?;
    // The token goes to curl on stdin so it never appears in the process list.
    let mut child = Command::new("curl")
        .args([
            "--silent",
            "--show-error",
            "--max-time",
            "20",
            "--config",
            "-",
        ])
        .args([
            "--request",
            "POST",
            "--header",
            "Content-Type: application/json",
        ])
        .arg("--data-binary")
        .arg(format!("@{}", body.path().display()))
        .arg("https://api.tailscale.com/api/v2/tailnet/-/keys")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("user = \"{token}:\"\n").as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr)
            .trim()
            .to_owned()
            .into());
    }
    let reply: Value = serde_json::from_slice(&output.stdout)
        .map_err(|_| "Tailscale returned an unreadable reply")?;
    match reply["key"].as_str() {
        Some(key) if !key.is_empty() => Ok(key.to_owned()),
        _ => Err(format!(
            "Tailscale refused to create a key: {}",
            reply["message"].as_str().unwrap_or("no reason given")
        )
        .into()),
    }
}
