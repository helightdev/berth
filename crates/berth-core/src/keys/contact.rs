//! Logging in to put keys on a host: first contact, where a one-time
//! password or key installs the host's key from the ring, and rekeying,
//! where a key of the ring is replaced on every host that takes it.
//!
//! Both change only the lines of the SSH user's `authorized_keys` that hold
//! the keys in question, and check a login with the new key before anything
//! is removed.

use super::{Login, Once, known, ring};
use crate::Result;
use crate::host::Host;
use crate::paths::Paths;
use crate::profiles::Place;
use crate::store::{OWN, Store, kind};
use berth_api::profile::{Access, Auth, BERTH_KEY};
use serde_json::Value;
use std::io::Write;
use std::process::Stdio;

/// Adds the key on the second line of standard input unless a line holds
/// the key of the first; a file that does not end in a newline gets one.
const ADD: &str = r#"umask 077; mkdir -p "$HOME/.ssh" || exit 1; f="$HOME/.ssh/authorized_keys"; IFS= read -r blob; IFS= read -r line; touch "$f" || exit 1; if grep -qF -- "$blob" "$f"; then exit 0; fi; if [ -s "$f" ] && [ -n "$(tail -c 1 "$f")" ]; then echo >> "$f"; fi; printf "%s\n" "$line" >> "$f""#;

/// Removes the lines that hold the key on standard input, and no others.
const REMOVE: &str = r#"umask 077; f="$HOME/.ssh/authorized_keys"; IFS= read -r blob; [ -f "$f" ] || exit 0; grep -qF -- "$blob" "$f" || exit 0; t="$f.berth.$$"; { grep -vF -- "$blob" "$f" || true; } > "$t" && chmod 600 "$t" && mv "$t" "$f""#;

/// Run a command on the host as berth runs one, with `input` on its
/// standard input. The error is ssh's own words, or the sentence for a
/// changed host key.
fn run(login: &Login, command: &str, input: &str) -> Result<()> {
    let mut ssh = login.batch_ssh();
    ssh.arg(login.target())
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = ssh.spawn().map_err(|error| format!("ssh: {error}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input.as_bytes());
    }
    let done = child.wait_with_output()?;
    if done.status.success() {
        return Ok(());
    }
    let said = String::from_utf8_lossy(&done.stderr);
    if let Some(changed) = known::changed_key(&login.name, &said) {
        return Err(changed.into());
    }
    let said = said
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("Warning: Permanently added"))
        .collect::<Vec<_>>()
        .join(" ");
    Err(match said.is_empty() {
        true => format!("ssh to {} failed ({})", login.name, done.status),
        false => said,
    }
    .into())
}

/// Whether a login works, by running `true`.
pub fn works(login: &Login) -> Result<()> {
    run(login, "true", "")
}

fn sh(script: &str) -> String {
    format!("sh -c '{script}'")
}

fn add_key(login: &Login, public: &str) -> Result<()> {
    run(
        login,
        &sh(ADD),
        &format!("{}\n{}\n", ring::blob(public), public.trim()),
    )
}

fn remove_key(login: &Login, public: &str) -> Result<()> {
    run(login, &sh(REMOVE), &format!("{}\n", ring::blob(public)))
}

/// What a first contact did.
#[derive(Debug)]
pub struct Trusted {
    /// The key of the ring the host takes now.
    pub key: String,
    pub fingerprint: String,
    /// Whether the host took it already, so nothing was changed.
    pub already: bool,
    /// The host's own keys, as recorded.
    pub host_keys: Vec<String>,
}

/// First contact: make the host take its key of the ring (the key `berth`
/// for a host that logs in with a password or the SSH configuration). The
/// key is tried first; when the host refuses it, `once` logs in one time to
/// add it to the SSH user's `authorized_keys`, and a login with the key is
/// checked. The one-time credential is dropped, unless it is a password and
/// `keep_password` says to keep it as the secret `ssh/HOST`; else a host
/// that logged in with a password logs in with the key from now on.
pub fn trust_key(
    paths: &Paths,
    host: &Host,
    once: Option<Once>,
    keep_password: bool,
) -> Result<Trusted> {
    if host.served(paths).is_some() {
        return Err(format!(
            "{} is a host of a berth server, which logs in to it: ask there",
            host.name
        )
        .into());
    }
    let access = Access::of(&host.config);
    if access.link.is_some() {
        return Err(format!(
            "{} is reached through its own command, not SSH, and takes no key",
            host.name
        )
        .into());
    }
    if access.address.is_empty() {
        return Err(format!("{} has no address yet", host.name).into());
    }
    let store = crate::store::shared(paths)?;
    let key = access.auth.key().unwrap_or(BERTH_KEY).to_owned();
    let public = ring::public(paths, &store, &key)?;
    let base = super::login(paths, &store, &host.place, &host.config)?;
    let files = super::key_files(paths, &store, &key)?;
    let by_key = base.with_keys(files);
    let mut record = known::Record::load(&store, &host.place)?;
    // A changed host key is said as it is, and no credential goes to a
    // machine that may not be the host.
    let changed = known::changed_key(&host.name, "Host key verification failed");
    let already = match works(&by_key) {
        Ok(()) => true,
        Err(error) if Some(error.to_string()) == changed => return Err(error),
        Err(_) => false,
    };
    if !already {
        let Some(once) = once else {
            return Err(format!(
                "{} does not take berth's key {key} yet. Log in once with a password or a private key to install it: berth key trust {}",
                host.name,
                host.place.argument()
            )
            .into());
        };
        let dir = ring::scratch_dir(paths)?;
        let one_time = base.with_once(&once, dir.path())?;
        add_key(&one_time, &public.public)
            .map_err(|error| format!("Logging in to {} once failed: {error}", host.name))?;
        works(&by_key).map_err(|error| {
            format!(
                "The key {key} is in the authorized_keys of {} now, but logging in with it fails: {error}",
                host.name
            )
        })?;
        if let (Once::Password(password), true) = (&once, keep_password) {
            crate::registry::Store::own(store.clone())
                .set_secret(&super::password_secret(&host.place), Some(password))?;
        }
        // The directory with the one-time credential goes here.
        drop(dir);
    }
    record = known::Record {
        host_keys: known::absorb(paths, &store, &host.place)?,
        ..record
    };
    record.trust(&public.fingerprint, None);
    record.save(&store, &host.place)?;
    if access.auth == Auth::Password && !keep_password {
        let mut profile = host.config.clone();
        let mut changed = access.clone();
        changed.auth = Auth::Berth;
        changed.write(&mut profile);
        host.place.save(paths, &profile)?;
    }
    Ok(Trusted {
        key,
        fingerprint: public.fingerprint,
        already,
        host_keys: record.host_keys,
    })
}

/// What `berth key host HOST` shows, and with `accept` does: the recorded
/// host keys, or, accepting a changed key, the ones the host has now.
pub fn host_key(paths: &Paths, host: &Host, accept: bool) -> Result<Vec<String>> {
    let store = crate::store::shared(paths)?;
    if !accept {
        return Ok(known::Record::load(&store, &host.place)?.host_keys);
    }
    known::forget_host_key(paths, &store, &host.place)?;
    let login = super::login(paths, &store, &host.place, &host.config)?;
    if let Some(why) = login.needs_password() {
        return Err(why.into());
    }
    works(&login).map_err(|error| {
        format!(
            "The old key is forgotten, and logging in to {} to record the new one failed: {error}",
            host.name
        )
    })?;
    known::absorb(paths, &store, &host.place)
}

/// A host that still takes the old key of a rotation, and why.
#[derive(Debug, PartialEq, Eq)]
pub struct Pending {
    pub host: String,
    pub why: String,
}

/// What a run of a rotation did.
#[derive(Debug, Default)]
pub struct Rotation {
    pub done: Vec<String>,
    pub pending: Vec<Pending>,
    /// The new key replaced the old one, which is deleted.
    pub finished: bool,
}

/// The saved hosts a rotation of `name` goes to: those that log in with the
/// key, and those berth knows to take it. A host that is done already, or
/// that berth knows nothing of and does not log in with it, is left out.
fn rotated(store: &Store, name: &str, old: &str, new: &str) -> Result<Vec<(Place, Value)>> {
    let mut hosts = vec![];
    for (host, value) in store.entries(kind::PROFILE, OWN)? {
        let Ok(config) = serde_json::from_slice::<Value>(&value) else {
            continue;
        };
        let Ok(place) = Place::own(&host) else {
            continue;
        };
        let record = known::Record::load(store, &place)?;
        let trusts = |fingerprint: &str| record.trusts.iter().any(|known| known == fingerprint);
        let access = Access::of(&config);
        let uses = access.auth.key() == Some(name);
        let ssh = access.link.is_none() && !access.address.is_empty();
        let wanted = trusts(old) || (uses && ssh && !trusts(new));
        if wanted {
            hosts.push((place, config));
        }
    }
    Ok(hosts)
}

/// Replace a key of the ring on every host that takes it: a new key is made
/// beside it, and on each host berth logs in, adds the new key, checks a
/// login with it, and removes the old one. A host that fails stays pending
/// and is offered both keys; running it again goes on from there. When no
/// host is pending, the new key takes the old one's place and the old one
/// is deleted. Each step is said on `out`.
pub fn rotate(paths: &Paths, name: &str, out: &mut dyn Write) -> Result<Rotation> {
    let store = crate::store::shared(paths)?;
    let next = ring::begin_rotation(paths, &store, name)?;
    let old = ring::read(&store, name, false)?.ok_or("The key went away")?;
    let (old_fp, new_fp) = (
        ring::fingerprint(&old.public),
        ring::fingerprint(&next.public),
    );
    let _ = writeln!(out, "▸ Replacing the key {name} ({old_fp}) with {new_fp}");
    let old_file = ring::file(paths, &store, name, false)?.ok_or("no file")?;
    let new_file = ring::file(paths, &store, name, true)?.ok_or("no file")?;
    let mut rotation = Rotation::default();
    for (place, config) in rotated(&store, name, &old_fp, &new_fp)? {
        let host = place.argument();
        let step = || -> Result<()> {
            let access = Access::of(&config);
            if access.link.is_some() || access.address.is_empty() {
                return Err("it has no SSH address".into());
            }
            let base = super::login(paths, &store, &place, &config)?;
            let both = base.with_keys(vec![old_file.clone(), new_file.clone()]);
            let by_new = base.with_keys(vec![new_file.clone()]);
            let mut record = known::Record::load(&store, &place)?;
            if works(&by_new).is_err() {
                add_key(&both, &next.public)?;
                works(&by_new).map_err(|error| {
                    format!("the new key is there, but logging in with it fails: {error}")
                })?;
            }
            record.trust(&new_fp, None);
            record.save(&store, &place)?;
            remove_key(&by_new, &old.public)?;
            record.trust(&new_fp, Some(&old_fp));
            record.save(&store, &place)?;
            Ok(())
        };
        match step() {
            Ok(()) => {
                let _ = writeln!(out, "✓ {host} takes the new key");
                rotation.done.push(host);
            }
            Err(error) => {
                let _ = writeln!(out, "! {host} stays pending: {error}");
                rotation.pending.push(Pending {
                    host,
                    why: error.to_string(),
                });
            }
        }
    }
    if rotation.pending.is_empty() {
        ring::finish_rotation(paths, &store, name)?;
        rotation.finished = true;
        let _ = writeln!(
            out,
            "✓ The key {name} is {new_fp} now; the old one is deleted"
        );
    } else {
        let _ = writeln!(
            out,
            "! {} hosts still take the old key, and are offered both until then. Run it again to go on: berth key rotate {name}",
            rotation.pending.len()
        );
    }
    Ok(rotation)
}
