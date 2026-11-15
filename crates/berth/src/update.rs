//! `berth update`: replace this program with the latest release.
//!
//! The release is the same archive `install.sh` takes, checked against the
//! SHA-256 published beside it. Hosts are not touched: their berthd and box
//! travel inside berth, so applying a host's configuration afterwards brings
//! them to the new version.

use crate::Result;
use crate::cli::args::Update;
use crate::cli::out;
use std::{fs, path::Path, process::Command};

const REPOSITORY: &str = "https://github.com/helightdev/berth";

/// The archive of the release for this system, when there is one.
fn asset() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("berth-x86_64-linux.tar.gz"),
        ("linux", "aarch64") => Some("berth-aarch64-linux.tar.gz"),
        ("macos", "aarch64") => Some("berth-aarch64-macos.tar.gz"),
        ("macos", "x86_64") => Some("berth-x86_64-macos.tar.gz"),
        _ => None,
    }
}

/// The setting of this user's that names a mirror of the releases.
pub const RELEASES: &str = "releases";

/// Where the releases are: a mirror given with `--from` or kept as the
/// `releases` setting, or else GitHub's.
fn releases(from: Option<&str>) -> Option<String> {
    from.map(str::to_owned)
        .or_else(|| crate::cli::client::settings().get(RELEASES))
        .filter(|base| !base.is_empty())
        .map(|base| base.trim_end_matches('/').to_owned())
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
fn latest(mirror: Option<&str>) -> Result<String> {
    // A mirror says it in a file; GitHub redirects to the tag.
    if let Some(base) = mirror {
        return Ok(curl(&[&format!("{base}/latest/download/VERSION")])?
            .trim_start_matches('v')
            .to_owned());
    }
    let base = format!("{REPOSITORY}/releases");
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

/// `berth update [--check] [--from URL]`
pub fn command(update: &Update) -> crate::cli::fail::Result<()> {
    Ok(install(update)?)
}

fn install(update: &Update) -> Result<()> {
    let check = update.check;
    let mirror = releases(update.from.as_deref());
    if cfg!(windows) {
        return Err("On Windows the new release is installed with install.ps1: irm https://raw.githubusercontent.com/helightdev/berth/main/install.ps1 | iex".into());
    }
    let Some(asset) = asset() else {
        return Err(format!(
            "No release is built for this system. Update this one with: cargo install --locked --git {REPOSITORY} berth"
        )
        .into());
    };
    let current = berth_api::VERSION;
    let latest = latest(mirror.as_deref())?;
    if !newer(&latest, current) {
        out::line(format!("berth {current} is the latest"));
        return Ok(());
    }
    if check {
        out::line(format!(
            "berth {latest} is out; this is {current}. berth update installs it"
        ));
        return Ok(());
    }
    let program = fs::canonicalize(std::env::current_exe()?)?;
    let folder = program.parent().ok_or("berth has no folder")?;
    // Beside the program, so the last step is a rename on one filesystem.
    let work = tempfile::Builder::new()
        .prefix(".berth-update")
        .tempdir_in(folder)
        .map_err(|error| format!("{}: {error}", folder.display()))?;
    let base = mirror.unwrap_or_else(|| format!("{REPOSITORY}/releases"));
    let url = format!("{base}/download/v{latest}");
    let fetch = |name: &str| -> Result<()> {
        let to = work.path().join(name);
        curl(&["-o", &to.to_string_lossy(), &format!("{url}/{name}")]).map(|_| ())
    };
    fetch(asset)?;
    fetch(&format!("{asset}.sha256"))?;
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
    let sums = format!("{asset}.sha256");
    // macOS has shasum rather than sha256sum.
    let checked = match cfg!(target_os = "macos") {
        true => run("shasum", &["-a", "256", "-c", &sums]),
        false => run("sha256sum", &["-c", &sums]),
    };
    checked.map_err(|_| "the download does not match its checksum; nothing was changed")?;
    run("tar", &["-xzf", asset, "berth"])?;
    replace(&work.path().join("berth"), &program)?;
    out::done(format!("berth {current} → {latest}"));
    if cfg!(target_os = "linux") {
        out::hint("Hosts get the new berthd and box when they are applied again: berth rollout");
    }
    Ok(())
}

/// Put the new program in the old one's place in one step, so a running
/// berth keeps the file it has open and the next start finds the new one.
fn replace(new: &Path, program: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(new, fs::Permissions::from_mode(0o755))?;
    }
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
