//! `berth key …`: the key ring, and the logins that put its keys on hosts.
//! With a berth server set, the ring is the server's and its private halves
//! stay there; first contact and host keys are then the server's to do.

use crate::Result;
use crate::serve::keys::{KeyInfo, KeyList, listing};
use berth_core::host::Host;
use berth_core::keys::{Once, contact, ring};
use berth_core::paths::Paths;
use berth_core::server::Server;
use serde_json::json;
use std::io::Write;

const USAGE: &str = "Usage: berth key [new NAME | add NAME FILE|- | remove NAME | show [NAME] | rotate [NAME] | trust HOST [--key FILE|NAME] [--keep-password] | host HOST [--accept]]";

/// The keys of this installation's ring.
pub fn local(paths: &Paths) -> Result<Vec<KeyInfo>> {
    Ok(listing(paths)?.keys)
}

/// The keys of a berth server's ring.
pub fn listed(server: &Server) -> Result<Vec<KeyInfo>> {
    let listed: KeyList = serde_json::from_value(server.call("keys.list", json!({}), None)?)?;
    Ok(listed.keys)
}

fn describe(keys: &[KeyInfo], out: &mut dyn Write) -> Result<()> {
    for key in keys {
        writeln!(out, "{}\t{}\t{}", key.name, key.fingerprint, key.public)?;
        if !key.hosts.is_empty() {
            writeln!(out, "\tused by {}", key.hosts.join(", "))?;
        }
        if let Some(next) = &key.pending {
            writeln!(
                out,
                "\tbeing replaced by {}; berth key rotate {} goes on",
                next.fingerprint, key.name
            )?;
        }
    }
    Ok(())
}

/// A private key from a file, or from standard input for `-`.
fn read_private(file: &str) -> Result<Vec<u8>> {
    match file {
        "-" => {
            let mut data = vec![];
            std::io::Read::read_to_end(&mut std::io::stdin(), &mut data)?;
            Ok(data)
        }
        file => std::fs::read(file).map_err(|error| format!("{file}: {error}").into()),
    }
}

/// `berth key …`.
pub fn command(paths: &Paths, args: &[&str]) -> Result<()> {
    let server = Server::configured(paths);
    let out = &mut std::io::stdout();
    match (args, &server) {
        ([], Some(server)) => describe(&listed(server)?, out),
        ([], None) => describe(&local(paths)?, out),
        (["show"], _) => show(paths, server.as_ref(), berth_api::profile::BERTH_KEY),
        (["show", name], _) => show(paths, server.as_ref(), name),
        (["new", name], Some(server)) => {
            let key = server.call("keys.add", json!({"name": name}), None)?;
            say_added(&serde_json::from_value(key)?, Some(server))
        }
        (["new", name], None) => {
            let store = berth_core::store::shared(paths)?;
            let key = ring::add(paths, &store, name, None)?;
            say_added(&from_public(key), None)
        }
        (["add", name, file], Some(server)) => {
            let data = json!({"base64": berth_api::b64(&read_private(file)?)});
            let key = server.call("keys.add", json!({"name": name}), Some(data))?;
            say_added(&serde_json::from_value(key)?, Some(server))
        }
        (["add", name, file], None) => {
            let store = berth_core::store::shared(paths)?;
            let key = ring::add(paths, &store, name, Some(&read_private(file)?))?;
            say_added(&from_public(key), None)
        }
        (["remove", name], Some(server)) => {
            server.call("keys.remove", json!({"name": name}), None)?;
            println!(
                "Removed the key {name} from the berth server at {}",
                server.url
            );
            Ok(())
        }
        (["remove", name], None) => {
            let store = berth_core::store::shared(paths)?;
            ring::remove(paths, &store, name)?;
            println!("Removed the key {name}");
            Ok(())
        }
        (["rotate", rest @ ..], _) if rest.len() <= 1 => {
            let name = rest
                .first()
                .copied()
                .unwrap_or(berth_api::profile::BERTH_KEY);
            match &server {
                Some(server) => {
                    let act = berth_api::Act {
                        arguments: [("name".to_owned(), name.to_owned())].into(),
                        ..berth_api::Act::default()
                    };
                    let acted = crate::commands::act_on(server, None, "rotate-key", &act)?;
                    let code = crate::commands::follow_server_job(server, acted, out)?;
                    std::process::exit(code);
                }
                None => {
                    let rotation = contact::rotate(paths, name, out)?;
                    if !rotation.finished {
                        std::process::exit(1);
                    }
                    Ok(())
                }
            }
        }
        (["trust", host, rest @ ..], _) => trust(paths, host, rest),
        (["host", host], _) => host_key(paths, host, false),
        (["host", host, "--accept"], _) => host_key(paths, host, true),
        _ => Err(USAGE.into()),
    }
}

fn from_public(key: ring::Public) -> KeyInfo {
    KeyInfo {
        name: key.name,
        public: key.public,
        fingerprint: key.fingerprint,
        pending: None,
        hosts: vec![],
    }
}

fn say_added(key: &KeyInfo, server: Option<&Server>) -> Result<()> {
    match server {
        Some(server) => println!(
            "Added the key {} to the berth server at {}: {}",
            key.name, server.url, key.fingerprint
        ),
        None => println!("Added the key {}: {}", key.name, key.fingerprint),
    }
    println!("{}", key.public);
    Ok(())
}

/// `berth key show [NAME]`: the public half alone, for an `authorized_keys`.
fn show(paths: &Paths, server: Option<&Server>, name: &str) -> Result<()> {
    let keys = match server {
        Some(server) => listed(server)?,
        None => local(paths)?,
    };
    let key = keys
        .iter()
        .find(|key| key.name == name)
        .ok_or_else(|| format!("No key {name}. See: berth key"))?;
    println!("{}", key.public);
    Ok(())
}

/// The one-time credential of a first contact: a private key from a file,
/// or what is typed without echo or comes on standard input, which is a
/// private key when it looks like one and a password otherwise.
fn credential(paths: &Paths, host: &str, key_file: Option<&str>) -> Result<Once> {
    let text = match key_file {
        Some(file) => read_private(file)?,
        None => berth_core::registry::read_secret(&format!(
            "A password, or a private key on standard input, that logs in to {host} once"
        ))?
        .into_bytes(),
    };
    if String::from_utf8_lossy(&text).contains("PRIVATE KEY-----") {
        return Ok(Once::Key(ring::validate(paths, &text)?));
    }
    Ok(Once::Password(
        String::from_utf8(text).map_err(|_| "A password is text")?,
    ))
}

fn local_host(paths: &Paths, name: &str) -> Result<Host> {
    let host = Host::load(paths, name)?;
    if let Some(server) = host.served(paths) {
        return Err(format!(
            "{name} is a host of the berth server at {}, which logs in to it with its own keys",
            server.url
        )
        .into());
    }
    Ok(host)
}

/// `berth key trust HOST`: first contact.
pub(crate) fn trust(paths: &Paths, name: &str, rest: &[&str]) -> Result<()> {
    let mut key_file = None;
    let mut keep = false;
    let mut rest = rest.iter();
    while let Some(arg) = rest.next() {
        match *arg {
            "--key" => key_file = Some(*rest.next().ok_or("--key needs a file")?),
            "--keep-password" => keep = true,
            _ => return Err(USAGE.into()),
        }
    }
    let host = Host::load(paths, name)?;
    if let Some(server) = host.served(paths) {
        return trust_served(&server, name, key_file, keep);
    }
    let host = local_host(paths, name)?;
    let trusted = match contact::trust_key(paths, &host, None, false) {
        Ok(trusted) => trusted,
        // Refused the key, it needs a login once; anything else is said as it is.
        Err(error) if !error.to_string().contains("berth key trust") => return Err(error),
        Err(_) => {
            let once = credential(paths, name, key_file)?;
            contact::trust_key(paths, &host, Some(once), keep)?
        }
    };
    match trusted.already {
        true => println!("{name} takes the key {} already", trusted.key),
        false => println!(
            "{name} takes the key {} now: {}",
            trusted.key, trusted.fingerprint
        ),
    }
    for key in &trusted.host_keys {
        println!("Its host key is {}", ring::fingerprint(key));
    }
    Ok(())
}

/// First contact for a host of a berth server, which logs in to it: its
/// key is tried first, and when the host refuses it a password typed here
/// logs in once. With a server `--key` names a key of the server's ring.
fn trust_served(server: &Server, name: &str, key: Option<&str>, keep: bool) -> Result<()> {
    let run = |password: Option<String>| -> Result<(i32, String)> {
        let mut arguments = std::collections::BTreeMap::from([(
            "keep_password".to_owned(),
            if keep { "yes" } else { "no" }.to_owned(),
        )]);
        if let Some(key) = key {
            arguments.insert("key".into(), key.into());
        }
        if let Some(password) = password {
            arguments.insert("password".into(), password);
        }
        let act = berth_api::Act {
            arguments,
            ..berth_api::Act::default()
        };
        let acted = crate::commands::act_on(server, Some(name), "trust-key", &act)?;
        let mut said = vec![];
        let code = crate::commands::follow_server_job(server, acted, &mut said)?;
        let said = String::from_utf8_lossy(&said).into_owned();
        print!("{said}");
        Ok((code, said))
    };
    let (mut code, said) = run(None)?;
    // Refused the key, it needs a login once.
    if code != 0 && key.is_none() && said.contains("berth key trust") {
        let password =
            berth_core::registry::read_secret(&format!("A password that logs in to {name} once"))?;
        code = run(Some(password))?.0;
    }
    match code {
        0 => Ok(()),
        _ => Err(format!("{name} does not take the server's key").into()),
    }
}

/// `berth key host HOST [--accept]`.
fn host_key(paths: &Paths, name: &str, accept: bool) -> Result<()> {
    let host = Host::load(paths, name)?;
    let keys: Vec<(String, String)> = match host.served(paths) {
        Some(server) => {
            let act = berth_api::Act {
                arguments: [(
                    "accept".to_owned(),
                    if accept { "yes" } else { "no" }.to_owned(),
                )]
                .into(),
                ..berth_api::Act::default()
            };
            let acted = crate::commands::act_on(&server, Some(name), "host-key", &act)?;
            let result = acted.result.unwrap_or_default();
            result["host_keys"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|key| {
                    let text = |field: &str| key[field].as_str().unwrap_or("").to_owned();
                    (text("type"), text("fingerprint"))
                })
                .collect()
        }
        None => {
            let host = local_host(paths, name)?;
            contact::host_key(paths, &host, accept)?
                .iter()
                .map(|key| {
                    let kind = key.split_whitespace().next().unwrap_or("").to_owned();
                    (kind, ring::fingerprint(key))
                })
                .collect()
        }
    };
    if keys.is_empty() {
        println!("berth has no host key of {name} yet; the first login records it");
    }
    for (kind, fingerprint) in keys {
        println!("{kind}\t{fingerprint}");
    }
    if accept {
        println!("Accepted the host key {name} has now");
    }
    Ok(())
}
