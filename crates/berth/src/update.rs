//! `berth update`: replace this program with the latest release.
//!
//! The release is the same archive `install.sh` takes, checked against the
//! SHA-256 published beside it. Hosts are not touched: their berthd and box
//! travel inside berth, so applying a host's configuration afterwards brings
//! them to the new version.

use crate::Result;
use std::{fs, path::Path, process::Command};

const REPOSITORY: &str = "https://github.com/helightdev/berth";
/// The archive of this build's kind: with the page for a browser, or without.
#[cfg(not(feature = "web"))]
const ASSET: &str = "berth-x86_64-linux.tar.gz";
#[cfg(feature = "web")]
const ASSET: &str = "berth-web-x86_64-linux.tar.gz";

/// Where the releases are: GitHub's, or `BERTH_RELEASES` for a mirror.
fn releases() -> String {
    match std::env::var("BERTH_RELEASES") {
        Ok(base) if !base.is_empty() => base.trim_end_matches('/').to_owned(),
        _ => format!("{REPOSITORY}/releases"),
    }
}

fn curl(args: &[&str]) -> Result<String> {
    let output = Command::new("curl").args(["-fsSL"]).args(args).output()?;
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(format!("curl: {said}").into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// The newest release's version, from where `releases/latest` leads.
fn latest() -> Result<String> {
    let base = releases();
    // A mirror says it in a file; GitHub redirects to the tag.
    if std::env::var_os("BERTH_RELEASES").is_some() {
        return Ok(curl(&[&format!("{base}/latest/download/VERSION")])?
            .trim_start_matches('v')
            .to_owned());
    }
    let page = curl(&[
        "-o",
        "/dev/null",
        "-w",
        "%{url_effective}",
        &format!("{base}/latest"),
    ])
    .map_err(|error| format!("No release found at {base} ({error})"))?;
    let tag = page.rsplit('/').next().unwrap_or("");
    match tag.strip_prefix('v') {
        Some(version) if !version.is_empty() => Ok(version.to_owned()),
        _ => Err(format!("{REPOSITORY} has no release yet").into()),
    }
}

/// Numbers compared part by part, so 0.10.0 is newer than 0.9.0. A release is
/// newer than its pre-releases, such as 0.4.0-rc.1.
pub(crate) fn newer(candidate: &str, current: &str) -> bool {
    let parts = |version: &str| -> (Vec<u64>, bool) {
        let numbers = version
            .split(['.', '-', '+'])
            .map_while(|part| part.parse().ok())
            .collect();
        let release = !version.split('+').next().unwrap_or("").contains('-');
        (numbers, release)
    };
    parts(candidate) > parts(current)
}

/// `berth update [--check]`
pub fn command(args: &[&str]) -> Result<()> {
    let check = match args {
        [] => false,
        ["--check"] => true,
        _ => return Err("Usage: berth update [--check]".into()),
    };
    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        return Err(format!(
            "Releases are built for Linux on x86_64. Update this one with: cargo install --locked --git {REPOSITORY} berth"
        )
        .into());
    }
    let current = berth_api::VERSION;
    let latest = latest()?;
    if !newer(&latest, current) {
        println!("berth {current} is the latest");
        return Ok(());
    }
    if check {
        println!("berth {latest} is out; this is {current}. Run berth update to install it.");
        return Ok(());
    }
    let program = fs::canonicalize(std::env::current_exe()?)?;
    let folder = program.parent().ok_or("berth has no folder")?;
    // Beside the program, so the last step is a rename on one filesystem.
    let work = tempfile::Builder::new()
        .prefix(".berth-update")
        .tempdir_in(folder)
        .map_err(|error| format!("{}: {error}", folder.display()))?;
    let url = format!("{}/download/v{latest}", releases());
    let fetch = |name: &str| -> Result<()> {
        let to = work.path().join(name);
        curl(&["-o", &to.to_string_lossy(), &format!("{url}/{name}")]).map(|_| ())
    };
    fetch(ASSET)?;
    fetch(&format!("{ASSET}.sha256"))?;
    let run = |program: &str, args: &[&str]| -> Result<()> {
        let status = Command::new(program)
            .args(args)
            .current_dir(work.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()?;
        match status.success() {
            true => Ok(()),
            false => Err(format!("{program} failed ({status})").into()),
        }
    };
    run("sha256sum", &["-c", &format!("{ASSET}.sha256")])
        .map_err(|_| "the download does not match its checksum; nothing was changed")?;
    run("tar", &["-xzf", ASSET, "berth"])?;
    replace(&work.path().join("berth"), &program)?;
    println!("berth {current} → {latest}");
    println!("Hosts get the new berthd and box when their configuration is applied again.");
    Ok(())
}

/// Put the new program in the old one's place in one step, so a running
/// berth keeps the file it has open and the next start finds the new one.
fn replace(new: &Path, program: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(new, fs::Permissions::from_mode(0o755))?;
    fs::rename(new, program).map_err(|error| format!("{}: {error}", program.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::newer;

    #[test]
    fn versions_compare_by_their_numbers() {
        assert!(newer("0.10.0", "0.9.3"));
        assert!(newer("1.0.0", "0.99.0"));
        assert!(!newer("0.2.0", "0.2.0"));
        assert!(!newer("0.1.9", "0.2.0"));
        assert!(!newer("", "0.2.0"));
        assert!(newer("0.4.0", "0.4.0-rc.1"));
        assert!(newer("0.4.0-rc.1", "0.3.0"));
        assert!(!newer("0.4.0-rc.1", "0.4.0"));
        assert!(!newer("0.3.0", "0.4.0-rc.1"));
    }
}
