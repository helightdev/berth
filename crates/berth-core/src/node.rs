//! Nodes: servers berth applies without a desktop, whose plugin features
//! set up what hosts machines, such as Incus or a Docker engine. A feature
//! may offer provisioners: its tasks report where the server is and what
//! pins it, the role keeps the report in the host's record of the apply, and
//! berth registers a provisioner of its own for each, naming the node it
//! lives on. A feature may also ask for berth's client certificate, which
//! berth makes once and the node then trusts.

use crate::{
    Result,
    host::Host,
    paths::Paths,
    plugins::{self, Plugin},
    profiles::Place,
    provisioner::{self, Entry},
    registry::Store,
    store::kind,
};
use berth_api::{HostType, Offer};
use serde_json::{Value, json};
use sha2::Digest;
use std::{
    collections::BTreeMap,
    net::{IpAddr, ToSocketAddrs},
};

/// The plugin features a profile names, as `PLUGIN/FEATURE`. A node names
/// none unless its profile does; a workstation's role has defaults of its own.
pub fn features(config: &Value) -> Vec<String> {
    config["ansible_vars"]["devbox_plugin_features"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

/// What the feature named `PLUGIN/FEATURE` says of the hosts it is for.
pub fn feature(plugins: &[Plugin], name: &str) -> Option<plugins::Hosting> {
    let (plugin, id) = name.split_once('/')?;
    let plugin = plugins.iter().find(|known| known.id == plugin)?;
    plugins::hosting(plugin)
        .into_iter()
        .find(|known| known.feature == id)
}

/// Why the profile of `name` names a feature that is not for a host of its
/// type, such as a server's engine on a workstation or an agent on a node.
pub fn check_features(plugins: &[Plugin], name: &str, config: &Value) -> Result<()> {
    let host_type = crate::host_type::of(config);
    let unfit: Vec<String> = features(config)
        .into_iter()
        .filter(|wanted| {
            feature(plugins, wanted).is_some_and(|found| !found.types.contains(&host_type))
        })
        .collect();
    if unfit.is_empty() {
        return Ok(());
    }
    let other = match host_type {
        HostType::Node => "a workstation",
        _ => "a node",
    };
    Err(format!(
        "{name} is a {host_type}, and {} {} for {other}: take {} out of its features",
        unfit.join(", "),
        if unfit.len() == 1 { "is" } else { "are" },
        if unfit.len() == 1 { "it" } else { "them" },
    )
    .into())
}

// ── berth's client certificates ─────────────────────────────────────────────

/// The SHA-256 of a PEM certificate's DER, in lowercase hex, as Incus
/// names a certificate it trusts.
pub fn fingerprint(pem: &str) -> Option<String> {
    let body: String = pem
        .lines()
        .skip_while(|line| !line.starts_with("-----BEGIN CERTIFICATE"))
        .skip(1)
        .take_while(|line| !line.starts_with("-----END"))
        .collect();
    let der = berth_api::unb64(body.trim())?;
    if der.is_empty() {
        return None;
    }
    Some(
        sha2::Sha256::digest(&der)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    )
}

/// A key and a self-signed certificate for berth as a client, as PEM.
fn make_client_certificate() -> Result<(String, String)> {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384)
        .map_err(|error| format!("Making a client key: {error}"))?;
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new())
        .map_err(|error| format!("Making a client certificate: {error}"))?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "berth");
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
    let certificate = params
        .self_signed(&key)
        .map_err(|error| format!("Making a client certificate: {error}"))?;
    Ok((certificate.pem(), key.serialize_pem()))
}

/// The client certificates the features of a profile ask for, by feature:
/// the certificate, its fingerprint and the name the node trusts it by. A
/// secret pair berth does not have yet is made now and kept in the store;
/// only the certificate goes to the node.
pub fn client_certificates(plugins: &[Plugin], store: &Store, config: &Value) -> Result<Value> {
    let mut found = serde_json::Map::new();
    for wanted in features(config) {
        let Some(feature) = feature(plugins, &wanted) else {
            continue;
        };
        let Some((cert_name, key_name)) = &feature.trusts else {
            continue;
        };
        let certificate = match (store.secret(cert_name)?, store.secret(key_name)?) {
            (Some(certificate), Some(key))
                if !certificate.trim().is_empty() && !key.trim().is_empty() =>
            {
                certificate
            }
            _ => {
                let (certificate, key) = make_client_certificate()?;
                store.set_secret(key_name, Some(&key))?;
                store.set_secret(cert_name, Some(&certificate))?;
                certificate
            }
        };
        let print = fingerprint(&certificate)
            .ok_or_else(|| format!("The berth secret {cert_name} is not a PEM certificate"))?;
        found.insert(
            wanted,
            json!({
                "certificate": certificate,
                "fingerprint": print,
                "name": format!("berth-{}", &print[..12]),
            }),
        );
    }
    Ok(Value::Object(found))
}

// ── Provisioners a node offers ──────────────────────────────────────────────

/// An address as the host part of a URL: an IPv6 one in brackets.
fn url_host(address: &str) -> String {
    match address.contains(':') && !address.starts_with('[') {
        true => format!("[{address}]"),
        false => address.to_owned(),
    }
}

/// A value of an offer with `{address}`, `{user}` and `{port}` filled in
/// from where berth reaches the node and as whom.
fn fill(value: &str, host: &Host) -> String {
    let access = host.access();
    let user = match access.user.is_empty() {
        true => "root".to_owned(),
        false => access.user.clone(),
    };
    let port = match access.port.is_empty() {
        true => "22".to_owned(),
        false => access.port.clone(),
    };
    value
        .replace("{address}", &url_host(&access.address))
        .replace("{user}", &user)
        .replace("{port}", &port)
}

/// The name berth registers a node's provisioner under: `NODE-PLUGIN-ID`,
/// made a valid name, with a number when it is taken by another.
fn free_name(registered: &[(String, Entry)], node: &str, kind: &str) -> String {
    let mut stem: String = format!("{node}-{}", provisioner::default_name(kind))
        .to_lowercase()
        .chars()
        .map(
            |ch| match ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                true => ch,
                false => '-',
            },
        )
        .collect();
    if !stem.starts_with(|ch: char| ch.is_ascii_lowercase()) {
        stem = format!("n-{stem}");
    }
    let mut free = stem.clone();
    let mut number = 2;
    while registered.iter().any(|(known, _)| *known == free) {
        free = format!("{stem}-{number}");
        number += 1;
    }
    free
}

/// A URL or an address as what identifies a server: its host and port,
/// in lowercase, without the scheme, the user or a path.
fn server_of(value: &str) -> String {
    let rest = value.split_once("://").map_or(value, |(_, rest)| rest);
    let rest = rest.split('/').next().unwrap_or(rest);
    let rest = rest.rsplit_once('@').map_or(rest, |(_, rest)| rest);
    rest.trim().to_lowercase()
}

/// The host and the port of what [`server_of`] gives, without brackets.
fn host_port(server: &str) -> (&str, &str) {
    if let Some(rest) = server.strip_prefix('[') {
        let (host, rest) = rest.split_once(']').unwrap_or((rest, ""));
        return (host, rest.strip_prefix(':').unwrap_or(""));
    }
    match server.matches(':').count() {
        1 => server.split_once(':').unwrap_or((server, "")),
        _ => (server, ""),
    }
}

/// How berth finds the addresses of a name: as it would connect to it.
type Resolve<'a> = &'a dyn Fn(&str) -> Vec<IpAddr>;

/// The addresses a name has where berth runs.
fn resolve(name: &str) -> Vec<IpAddr> {
    if let Ok(address) = name.parse::<IpAddr>() {
        return vec![address];
    }
    (name, 0)
        .to_socket_addrs()
        .map(|found| found.map(|address| address.ip()).collect())
        .unwrap_or_default()
}

/// Whether the hosts `one` and `other` are the same machine to berth: the
/// same name, or names that berth's own lookup gives an address in common,
/// as `node` and `incus-host` both name the gateway of a server's container.
fn same_machine(one: &str, other: &str, resolve: Resolve) -> bool {
    let (one, other) = (one.trim().to_lowercase(), other.trim().to_lowercase());
    if one.is_empty() || other.is_empty() {
        return false;
    }
    if one == other {
        return true;
    }
    let theirs = resolve(&other);
    resolve(&one).iter().any(|address| theirs.contains(address))
}

/// Whether berth reaches the node `host` at the URL or address `value`.
fn on_node(value: &str, host: &Host, resolve: Resolve) -> bool {
    let server = server_of(value);
    same_machine(host_port(&server).0, &host.access().address, resolve)
}

/// Whether berth itself can tell that the provisioner `entry`, registered by
/// hand, reaches the server the node offers: a setting of it names the same
/// port on the same machine as the offer does, and that machine is where
/// berth reaches the node. Nothing else the node reports counts, as it could
/// name any server.
fn reaches_node(
    entry: &Entry,
    settings: &BTreeMap<String, String>,
    host: &Host,
    resolve: Resolve,
) -> bool {
    settings.iter().any(|(argument, offered)| {
        let kept = entry.settings.get(argument).map_or("", String::as_str);
        let (offered, kept) = (server_of(offered), server_of(kept));
        let ((offered_host, offered_port), (kept_host, kept_port)) =
            (host_port(&offered), host_port(&kept));
        offered_port == kept_port
            && same_machine(offered_host, &host.access().address, resolve)
            && same_machine(kept_host, &host.access().address, resolve)
    })
}

/// Why the node says the provisioner `entry` reaches the server it offers,
/// which berth cannot check: it pins the certificate the node reports, which
/// is public, or names a server the node gives as its own.
fn said_same(store: &Store, entry: &Entry, offer: &Offer, host: &Host) -> Option<&'static str> {
    let pins = |secret: &str, value: &str| {
        store
            .secret(secret)
            .ok()
            .flatten()
            .is_some_and(|kept| !kept.trim().is_empty() && kept.trim() == value.trim())
    };
    let pinned = offer
        .secrets
        .iter()
        .any(|(secret, value)| pins(&provisioner::secret_name(&entry.id, secret), value));
    if pinned {
        return Some("it pins the certificate the node reports");
    }
    let named = offer.settings.iter().any(|(argument, wanted)| {
        let kept = entry.settings.get(argument).map_or("", String::as_str);
        let mut reaches = vec![fill(wanted, host)];
        reaches.extend(
            offer
                .aliases
                .get(argument)
                .into_iter()
                .flatten()
                .map(|alias| fill(alias, host)),
        );
        // An empty value names the engine or server where berth runs.
        reaches
            .iter()
            .any(|reach| match (reach.is_empty(), kept.is_empty()) {
                (true, true) => true,
                (false, false) => server_of(reach) == server_of(kept),
                _ => false,
            })
    });
    named.then_some("it names a server the node gives as its own")
}

/// Register the provisioners the node `host` offers, as its last apply
/// reported them: one berth registered for the node before is brought up to
/// date, one registered by hand that berth can tell reaches the node is taken
/// over, and any other is registered under a new name. Those of the node no
/// feature offers any more are forgotten, unless they still hold resources.
/// Says what it did, a line each.
///
/// What a node reports comes from a machine berth manages, so it touches
/// only the node's own provisioners: an offer is taken only from a feature
/// its profile names, of the provisioners that feature's manifest offers,
/// with that provisioner's own arguments and secrets, and with URLs that
/// name the node.
pub fn register(
    paths: &Paths,
    store: &Store,
    host: &Host,
    offers: &[Offer],
) -> Result<Vec<String>> {
    register_by(paths, store, host, offers, &resolve)
}

fn register_by(
    paths: &Paths,
    store: &Store,
    host: &Host,
    offers: &[Offer],
    resolve: Resolve,
) -> Result<Vec<String>> {
    let plugins = provision_plugins(paths);
    let wanted = features(&host.config);
    let node = host.place.key();
    let mut said = vec![];
    let mut kept: Vec<String> = vec![];
    for offer in offers {
        let Some((plugin_id, _)) = offer.feature.split_once('/') else {
            continue;
        };
        let hosting = feature(&plugins, &offer.feature);
        let Some(hosting) = hosting.filter(|_| wanted.contains(&offer.feature)) else {
            said.push(format!(
                "! {} reports what {} offers, which is not among its features; it is left out",
                host.name, offer.feature
            ));
            continue;
        };
        for id in &offer.provisioners {
            let kind = format!("{plugin_id}/{id}");
            if !hosting.offers.contains(id) {
                said.push(format!(
                    "! {} offers {kind}, which {} does not offer; it is left out",
                    host.name, offer.feature
                ));
                continue;
            }
            let Ok((_, declared)) = crate::provision::find(&plugins, &kind) else {
                said.push(format!(
                    "! {} offers {kind}, which no plugin here has",
                    host.name
                ));
                continue;
            };
            let settings: BTreeMap<String, String> = offer
                .settings
                .iter()
                .filter(|(argument, _)| declared.own.contains(argument))
                .map(|(argument, value)| (argument.clone(), fill(value, host)))
                // An empty value is the argument's default, as berth keeps no empty settings.
                .filter(|(_, value)| !value.is_empty())
                .collect();
            if let Some((argument, value)) = settings
                .iter()
                .find(|(_, value)| value.contains("://") && !on_node(value, host, resolve))
            {
                said.push(format!(
                    "! {} offers {kind} with {argument}={value}, which is not where berth reaches {}; it is left out",
                    host.name, host.name
                ));
                continue;
            }
            let registered = provisioner::entries(paths)?;
            let ours = registered
                .iter()
                .find(|(_, entry)| entry.node == node && entry.kind == kind);
            let by_hand = || {
                registered.iter().find(|(_, entry)| {
                    entry.node.is_empty()
                        && entry.kind == kind
                        && reaches_node(entry, &settings, host, resolve)
                })
            };
            let found = ours.or_else(by_hand);
            if found.is_none() {
                // One the node only says is the same is left to its owner, and no second one is made beside it.
                let doubt = registered.iter().find_map(|(name, entry)| {
                    (entry.kind == kind && entry.node != node)
                        .then(|| said_same(store, entry, offer, host).map(|why| (name, entry, why)))
                        .flatten()
                });
                match doubt {
                    Some((name, entry, _)) if !entry.node.is_empty() => {
                        said.push(format!(
                            "! {} offers {kind}, and says it is the server of {name}, which is {}'s; both are left as they are",
                            host.name,
                            node_name(paths, &entry.node)
                        ));
                        continue;
                    }
                    Some((name, _, why)) => {
                        said.push(format!(
                            "! {} offers {kind}, and {name} may reach the same server ({why}), which berth cannot check; both are left as they are. To make {name} {}'s: berth provisioner run {name} adopt node={}",
                            host.name, host.name, host.name
                        ));
                        continue;
                    }
                    None => {}
                }
            }
            let (name, how) = match found {
                Some((name, entry)) if entry.node.is_empty() => (name.clone(), "took over"),
                Some((name, _)) => (name.clone(), "kept"),
                None => (free_name(&registered, &host.name, &kind), "registered"),
            };
            let before = provisioner::entry(paths, &name)?;
            // One taken over reaches the server already, as it was set up by hand.
            let mut values = before
                .as_ref()
                .map(|entry| entry.settings.clone())
                .unwrap_or_default();
            if how != "took over" {
                values.extend(settings.clone());
            }
            let entry = Entry {
                kind: kind.clone(),
                settings: values,
                node: node.clone(),
                feature: offer.feature.clone(),
                ..before.clone().unwrap_or_default()
            };
            let id = provisioner::save_entry(paths, &name, &entry)?;
            // The provisioner is given its own secrets alone: those the node
            // reports, and the client certificate its feature makes the node trust.
            let mut given: Vec<(String, String)> = offer.secrets.clone().into_iter().collect();
            if let Some((cert, key)) = &hosting.trusts {
                for secret in [cert, key] {
                    if let Some(value) = store.secret(secret)? {
                        given.push((secret.clone(), value));
                    }
                }
            }
            for (secret, value) in &given {
                if !declared.secrets.iter().any(|known| &known.name == secret) {
                    continue;
                }
                let own = provisioner::secret_name(&id, secret);
                if store.secret(&own)?.as_deref() != Some(value.as_str()) {
                    store.set_secret(&own, Some(value))?;
                }
            }
            match (how, before.as_ref() == Some(&entry)) {
                ("kept", true) => {}
                ("kept", false) => {
                    said.push(format!("✓ Updated the provisioner {name} of {}", host.name))
                }
                ("took over", _) => said.push(format!(
                    "✓ The provisioner {name} reaches the same server, and is {}'s now",
                    host.name
                )),
                _ => said.push(format!(
                    "✓ Registered the provisioner {name} ({kind}) of {}{}",
                    host.name,
                    match offer.fingerprint.is_empty() {
                        true => String::new(),
                        false => format!(
                            ", which pins the server's certificate {}",
                            offer.fingerprint
                        ),
                    }
                )),
            }
            kept.push(name);
        }
    }
    for (name, entry) in provisioner::entries(paths)? {
        if entry.node != node || kept.contains(&name) {
            continue;
        }
        match provisioner::remove_entry(paths, store, &name) {
            Ok(()) => said.push(format!(
                "✓ Forgot the provisioner {name}: no feature of {} offers it any more",
                host.name
            )),
            Err(error) => said.push(format!(
                "! {} no longer offers {name}, which is kept: {error}",
                host.name
            )),
        }
    }
    Ok(said)
}

/// Make the provisioner `name`, registered by hand, the node `node`'s, on
/// the word of the person who knows it reaches the node's server: the next
/// apply of the node brings its settings and pinned certificate up to date.
pub fn adopt(paths: &Paths, name: &str, node: &str) -> Result<String> {
    let mut entry = provisioner::entry(paths, name)?
        .ok_or_else(|| format!("No provisioner {name} is registered"))?;
    let name = entry.name.clone();
    if !entry.node.is_empty() {
        return Err(format!(
            "{name} is the node {}'s already",
            node_name(paths, &entry.node)
        )
        .into());
    }
    let found = Place::find(paths, crate::store::OWN, node)?;
    let host = Host::all(paths)
        .into_iter()
        .find(|host| Some(&host.place) == found.as_ref())
        .ok_or_else(|| format!("No host {node}"))?;
    let node = host.name.clone();
    let node_id = host.place.key();
    if crate::host_type::of(&host.config) != HostType::Node {
        return Err(format!("{node} is not a node").into());
    }
    let plugins = provision_plugins(paths);
    let (plugin_id, id) = entry
        .kind
        .split_once('/')
        .unwrap_or((entry.kind.as_str(), ""));
    let offering = features(&host.config).into_iter().find(|wanted| {
        wanted
            .split_once('/')
            .is_some_and(|(plugin, _)| plugin == plugin_id)
            && feature(&plugins, wanted)
                .is_some_and(|found| found.offers.iter().any(|offered| offered == id))
    });
    let Some(offering) = offering else {
        return Err(format!(
            "No feature of {node} offers {}: add the one that does to its features first",
            entry.kind
        )
        .into());
    };
    if let Some((other, _)) = provisioner::entries(paths)?
        .into_iter()
        .find(|(_, known)| known.node == node_id && known.kind == entry.kind)
    {
        return Err(format!(
            "{node} has the provisioner {other} of the type {} already. Remove it first: berth provisioner run {other} remove",
            entry.kind
        )
        .into());
    }
    entry.node = node_id;
    entry.feature = offering;
    provisioner::save_entry(paths, &entry.id.clone(), &entry)?;
    Ok(format!(
        "{name} is {node}'s now. The next apply of {node} brings its settings and the certificate it pins up to date: berth apply {node}"
    ))
}

/// The plugins whose provisioners berth runs here.
fn provision_plugins(paths: &Paths) -> Vec<Plugin> {
    crate::provision::found(paths)
}

/// Why applying `host` now must not go on: its profile no longer names the
/// feature that offers a provisioner of the node that still holds resources.
pub fn check_removal(paths: &Paths, host: &Host) -> Result<()> {
    let wanted = features(&host.config);
    let applied = crate::host_type::Capability::Plugins.allowed(crate::host_type::of(&host.config));
    for (name, entry) in provisioner::entries(paths)? {
        if entry.node != host.place.key() || (applied && wanted.contains(&entry.feature)) {
            continue;
        }
        let held = holds(paths, &entry.id)?;
        if !held.is_empty() {
            return Err(format!(
                "{} offers the provisioner {name} through {}, which still holds {}. Remove {} first (berth resource ID remove), or keep {} among the features of {}",
                host.name,
                entry.feature,
                held.join(", "),
                if held.len() == 1 { "it" } else { "them" },
                entry.feature,
                host.name
            )
            .into());
        }
    }
    Ok(())
}

/// The resources the registered provisioner with the id holds, by id.
fn holds(paths: &Paths, id: &str) -> Result<Vec<String>> {
    Ok(crate::resource::list(paths)?
        .into_iter()
        .filter(|kept| kept.record.provisioner == id)
        .map(|kept| kept.record.id)
        .collect())
}

/// Forget the provisioners of the node with the id `node` before the host
/// goes: refused while one still holds resources, as these would have no
/// provisioner.
pub fn forget(paths: &Paths, store: &Store, node: &str) -> Result<()> {
    let ours: Vec<Entry> = provisioner::entries(paths)?
        .into_iter()
        .filter(|(_, entry)| entry.node == node)
        .map(|(_, entry)| entry)
        .collect();
    for provisioner in &ours {
        let held = holds(paths, &provisioner.id)?;
        if !held.is_empty() {
            return Err(format!(
                "{} is the node of the provisioner {}, which still holds {}. Remove {} first: berth resource ID remove",
                node_name(paths, node),
                provisioner.name,
                held.join(", "),
                if held.len() == 1 { "it" } else { "them" }
            )
            .into());
        }
    }
    for provisioner in ours {
        provisioner::remove_entry(paths, store, &provisioner.id)?;
    }
    Ok(())
}

/// The name of the host with the id, or the id when there is none.
pub fn node_name(paths: &Paths, id: &str) -> String {
    crate::store::shared(paths)
        .ok()
        .and_then(|store| {
            store
                .label_of(kind::PROFILE, crate::store::OWN, id)
                .ok()
                .flatten()
        })
        .unwrap_or_else(|| id.to_owned())
}

/// What the node `host` reported at the end of its last apply, asked of its
/// controller until it answers: the provisioners its features offer.
pub fn reported(paths: &Paths, host: &Host) -> Result<Vec<Offer>> {
    Ok(applied(paths, host)?
        .map(|applied| applied.offers)
        .unwrap_or_default())
}

/// The record the last apply left on `host`, asked of its controller until
/// it answers, as the controller may restart at the end of an apply.
pub fn applied(paths: &Paths, host: &Host) -> Result<Option<berth_api::Applied>> {
    let password = host.kept_sudo_password(paths);
    let link = crate::link::Link::new(host.link(paths, password.as_deref())?);
    let runtime = crate::link::runtime()?;
    let started = std::time::Instant::now();
    let patience = std::time::Duration::from_secs(60);
    loop {
        let asked = runtime.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(15), link.version()).await
        });
        match asked {
            Ok(Ok(version)) => return Ok(version.applied),
            Ok(Err(error)) if started.elapsed() >= patience => {
                return Err(error.to_string().into());
            }
            Err(_) if started.elapsed() >= patience => return Err("no answer in time".into()),
            _ => std::thread::sleep(std::time::Duration::from_secs(1)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::Place;
    use std::fs;

    /// A berth with a source whose plugin has a node's feature that offers
    /// its provisioner, a workstation's feature, and the node `big`.
    struct World {
        _home: tempfile::TempDir,
        paths: Paths,
        store: Store,
    }

    impl World {
        fn new() -> World {
            let home = tempfile::tempdir().unwrap();
            let paths = Paths {
                config_dir: home.path().join("config"),
                data_dir: home.path().join("data"),
            };
            paths.prepare().unwrap();
            let source = home.path().join("source");
            fs::create_dir_all(source.join("provision/bin")).unwrap();
            fs::write(source.join(crate::paths::layout::BUILDER), "").unwrap();
            paths.set("source", source.to_str().unwrap()).unwrap();
            let plugin = source.join("plugins/fake");
            fs::create_dir_all(plugin.join("bin")).unwrap();
            fs::write(plugin.join("bin/provision"), "#!/bin/sh\n").unwrap();
            fs::write(
                plugin.join("package.json"),
                r#"{"name": "berth-plugin-fake", "berth": {
                    "features": [
                      {"id": "engine", "types": ["node"], "offers": ["vm"],
                       "trusts": {"certificate": "fake_client_cert", "key": "fake_client_key"}},
                      {"id": "desk"}],
                    "provisioners": [
                      {"id": "vm", "label": "Fake VM", "command": "bin/provision", "protocol": 2,
                       "secrets": [{"name": "server_cert", "required": false}],
                       "arguments": [{"name": "server", "required": true}],
                       "kinds": [{"id": "machine", "setup": "apply"}]}]}}"#,
            )
            .unwrap();
            let store = Store::own(crate::store::shared(&paths).unwrap());
            World {
                _home: home,
                paths,
                store,
            }
        }

        fn host(&self, features: &[&str]) -> Host {
            let config = json!({"type": "node",
                "access": {"address": "192.0.2.20", "user": "root"},
                "ansible_vars": {"devbox_plugin_features": features}});
            let place = Place::own(&self.paths, "big").unwrap();
            place.save(&self.paths, &config).unwrap();
            Host {
                name: "big".into(),
                place,
                config,
            }
        }

        fn plugins(&self) -> Vec<Plugin> {
            crate::provision::found(&self.paths)
        }

        fn holding(&self, provisioner: &str) {
            crate::resource::Kept {
                scope: crate::store::OWN.into(),
                record: crate::resource::Record {
                    id: "r1".into(),
                    provisioner: provisioner::reference(&self.paths, provisioner).unwrap(),
                    kind: "machine".into(),
                    name: "studio-2".into(),
                    ..Default::default()
                },
            }
            .save(&self.paths)
            .unwrap();
        }
    }

    fn offer() -> Offer {
        Offer {
            feature: "fake/engine".into(),
            provisioners: vec!["vm".into(), "gone".into()],
            settings: [("server".into(), "https://{address}:8444".into())].into(),
            secrets: [("server_cert".into(), "CERT".into())].into(),
            fingerprint: "ab12".into(),
            ..Offer::default()
        }
    }

    #[test]
    fn a_node_registers_what_its_features_offer_once() {
        let world = World::new();
        let host = world.host(&["fake/engine"]);
        let said = register(&world.paths, &world.store, &host, &[offer()]).unwrap();
        assert_eq!(said.len(), 2, "{said:?}");
        assert!(said[0].starts_with("✓ Registered the provisioner big-fake-vm (fake/vm) of big, which pins the server's certificate ab12"));
        assert!(
            said[1] == "! big offers fake/gone, which fake/engine does not offer; it is left out",
            "{said:?}"
        );
        let entry = provisioner::entry(&world.paths, "big-fake-vm")
            .unwrap()
            .unwrap();
        assert_eq!(
            (entry.node.as_str(), entry.feature.as_str()),
            (host.place.key().as_str(), "fake/engine")
        );
        assert_eq!(node_name(&world.paths, &entry.node), "big");
        assert_eq!(entry.settings["server"], "https://192.0.2.20:8444");
        assert_eq!(
            world
                .store
                .secret("provisioner/big-fake-vm/server_cert")
                .unwrap()
                .as_deref(),
            Some("CERT")
        );
        let info = provisioner::info(&world.paths, &world.store, "big-fake-vm").unwrap();
        assert_eq!(info.node, "big");
        // The same report again changes nothing and says nothing.
        let again = register(&world.paths, &world.store, &host, &[offer()]).unwrap();
        assert_eq!(again.len(), 1, "{again:?}");
        assert_eq!(provisioner::entries(&world.paths).unwrap().len(), 1);
        // A new certificate is pinned in its place.
        let mut renewed = offer();
        renewed.secrets.insert("server_cert".into(), "NEW".into());
        register(&world.paths, &world.store, &host, &[renewed]).unwrap();
        assert_eq!(
            world
                .store
                .secret("provisioner/big-fake-vm/server_cert")
                .unwrap()
                .as_deref(),
            Some("NEW")
        );
    }

    /// The lookup of a server's container on the node, where `node` and
    /// `incus-host` both name the node's address.
    fn lookup(name: &str) -> Vec<IpAddr> {
        match name {
            "node" | "incus-host" => vec!["192.0.2.20".parse().unwrap()],
            "other.example" => vec!["192.0.2.30".parse().unwrap()],
            other => other.parse().into_iter().collect(),
        }
    }

    fn hand(world: &World, name: &str, server: &str) {
        let entry = Entry {
            kind: "fake/vm".into(),
            settings: [("server".into(), server.into())].into(),
            ..Entry::default()
        };
        provisioner::save_entry(&world.paths, name, &entry).unwrap();
    }

    fn names(world: &World) -> Vec<String> {
        provisioner::entries(&world.paths)
            .unwrap()
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    }

    #[test]
    fn one_registered_by_hand_for_the_node_is_taken_over() {
        let world = World::new();
        let host = world.host(&["fake/engine"]);
        // As the recipe registered it: its own name and setting, the shared certificate.
        hand(&world, "incus-vm", "https://incus-host:8444");
        // A secret all share of the name is never the provisioner's.
        world.store.set_secret("server_cert", Some("CERT")).unwrap();
        hand(&world, "elsewhere", "https://192.0.2.99:8444");
        let said = register_by(&world.paths, &world.store, &host, &[offer()], &lookup).unwrap();
        assert!(
            said.iter()
                .any(|line| line.contains("incus-vm reaches the same server, and is big's now")),
            "{said:?}"
        );
        assert_eq!(names(&world), ["elsewhere", "incus-vm"]);
        let taken = provisioner::entry(&world.paths, "incus-vm")
            .unwrap()
            .unwrap();
        assert_eq!(
            (taken.node.as_str(), taken.feature.as_str()),
            (host.place.key().as_str(), "fake/engine")
        );
        // Its setting reaches the server already, and stays.
        assert_eq!(taken.settings["server"], "https://incus-host:8444");
        assert!(
            provisioner::entry(&world.paths, "elsewhere")
                .unwrap()
                .unwrap()
                .node
                .is_empty()
        );

        // The address berth reaches the node at is enough, with or without a certificate.
        let world = World::new();
        let host = world.host(&["fake/engine"]);
        hand(&world, "by-address", "https://192.0.2.20:8444/");
        let mut unpinned = offer();
        unpinned.secrets.clear();
        register_by(&world.paths, &world.store, &host, &[unpinned], &lookup).unwrap();
        assert_eq!(names(&world), ["by-address"]);
        assert_eq!(
            provisioner::entry(&world.paths, "by-address")
                .unwrap()
                .unwrap()
                .node,
            host.place.key()
        );

        // Another port of the same machine is another server.
        let world = World::new();
        let host = world.host(&["fake/engine"]);
        hand(&world, "other-port", "https://192.0.2.20:9000");
        register_by(&world.paths, &world.store, &host, &[offer()], &lookup).unwrap();
        assert_eq!(names(&world), ["big-fake-vm", "other-port"]);
    }

    #[test]
    fn one_the_node_alone_says_is_the_same_waits_for_its_owner() {
        let world = World::new();
        // Reached by its tailnet name, which berth does not find at incus-host.
        let mut host = world.host(&["fake/engine"]);
        host.config["access"]["address"] = json!("big.example.org");
        hand(&world, "incus-vm", "https://incus-host:8444");
        // A secret all share of the name is never the provisioner's.
        world.store.set_secret("server_cert", Some("CERT")).unwrap();
        let mut aliased = offer();
        aliased
            .aliases
            .insert("server".into(), vec!["https://incus-host:8444".into()]);
        let said = register_by(
            &world.paths,
            &world.store,
            &host,
            &[aliased.clone()],
            &lookup,
        )
        .unwrap();
        assert!(
            said.iter().any(|line| line
                == "! big offers fake/vm, and incus-vm may reach the same server (it names a server the node gives as its own), which berth cannot check; both are left as they are. To make incus-vm big's: berth provisioner run incus-vm adopt node=big"),
            "{said:?}"
        );
        // Nothing changed, and there is no second one beside it.
        assert_eq!(names(&world), ["incus-vm"]);
        let left = provisioner::entry(&world.paths, "incus-vm")
            .unwrap()
            .unwrap();
        assert!(left.node.is_empty());
        assert_eq!(
            world
                .store
                .secret("provisioner/incus-vm/server_cert")
                .unwrap(),
            None
        );

        // The owner's word makes it the node's, and the next apply brings it up to date.
        let message = adopt(&world.paths, "incus-vm", "big").unwrap();
        assert!(message.starts_with("incus-vm is big's now."), "{message}");
        let adopted = provisioner::entry(&world.paths, "incus-vm")
            .unwrap()
            .unwrap();
        assert_eq!(
            (adopted.node.as_str(), adopted.feature.as_str()),
            (host.place.key().as_str(), "fake/engine")
        );
        let said = register_by(&world.paths, &world.store, &host, &[aliased], &lookup).unwrap();
        assert!(
            said.iter()
                .any(|line| line == "✓ Updated the provisioner incus-vm of big"),
            "{said:?}"
        );
        let updated = provisioner::entry(&world.paths, "incus-vm")
            .unwrap()
            .unwrap();
        assert_eq!(updated.settings["server"], "https://big.example.org:8444");
        assert_eq!(
            world
                .store
                .secret("provisioner/incus-vm/server_cert")
                .unwrap()
                .as_deref(),
            Some("CERT")
        );

        // A local engine is the node's only on the owner's word, too.
        let world = World::new();
        let host = world.host(&["fake/engine"]);
        provisioner::save_entry(
            &world.paths,
            "local",
            &Entry {
                kind: "fake/vm".into(),
                ..Entry::default()
            },
        )
        .unwrap();
        let mut local = offer();
        local.secrets.clear();
        local.aliases.insert("server".into(), vec![String::new()]);
        let said = register_by(&world.paths, &world.store, &host, &[local], &lookup).unwrap();
        assert!(
            said.iter().any(|line| line.contains(
                "local may reach the same server (it names a server the node gives as its own)"
            )),
            "{said:?}"
        );
        assert_eq!(names(&world), ["local"]);
        assert!(
            provisioner::entry(&world.paths, "local")
                .unwrap()
                .unwrap()
                .node
                .is_empty()
        );
    }

    #[test]
    fn a_node_does_not_reach_a_provisioner_of_another_server() {
        let world = World::new();
        let host = world.host(&["fake/engine"]);
        hand(&world, "other", "https://other.example:8444");
        world
            .store
            .set_secret("provisioner/other/server_cert", Some("REAL-CERT"))
            .unwrap();
        let untouched = |world: &World| {
            let other = provisioner::entry(&world.paths, "other").unwrap().unwrap();
            assert!(other.node.is_empty());
            assert_eq!(other.settings["server"], "https://other.example:8444");
            assert_eq!(
                world
                    .store
                    .secret("provisioner/other/server_cert")
                    .unwrap()
                    .as_deref(),
                Some("REAL-CERT")
            );
        };
        // It names the other server among its aliases, with a certificate of its own.
        let mut hostile = offer();
        hostile
            .aliases
            .insert("server".into(), vec!["https://other.example:8444".into()]);
        hostile.secrets = [("server_cert".into(), "FORGED-CERT".into())].into();
        let said = register_by(
            &world.paths,
            &world.store,
            &host,
            &[hostile.clone()],
            &lookup,
        )
        .unwrap();
        assert!(
            said.iter().any(|line| line
                .starts_with("! big offers fake/vm, and other may reach the same server")),
            "{said:?}"
        );
        untouched(&world);
        // It reports the other server's certificate, which is public.
        hostile.secrets = [("server_cert".into(), "REAL-CERT".into())].into();
        register_by(
            &world.paths,
            &world.store,
            &host,
            &[hostile.clone()],
            &lookup,
        )
        .unwrap();
        untouched(&world);
        // It names the other server as its own.
        hostile.aliases.clear();
        hostile.settings = [("server".into(), "https://other.example:8444".into())].into();
        let said = register_by(&world.paths, &world.store, &host, &[hostile], &lookup).unwrap();
        assert!(
            said.iter().any(|line| line
                == "! big offers fake/vm with server=https://other.example:8444, which is not where berth reaches big; it is left out"),
            "{said:?}"
        );
        untouched(&world);
        // Dropping the feature removes only what is the node's.
        let said = register_by(&world.paths, &world.store, &host, &[], &lookup).unwrap();
        assert!(said.is_empty(), "{said:?}");
        untouched(&world);
        assert_eq!(names(&world), ["other"]);

        // One of another node is that node's.
        let world = World::new();
        let host = world.host(&["fake/engine"]);
        provisioner::save_entry(
            &world.paths,
            "dock-fake-vm",
            &Entry {
                kind: "fake/vm".into(),
                settings: [("server".into(), "https://192.0.2.40:8444".into())].into(),
                node: "dock".into(),
                feature: "fake/engine".into(),
                ..Entry::default()
            },
        )
        .unwrap();
        let mut claims = offer();
        claims
            .aliases
            .insert("server".into(), vec!["https://192.0.2.40:8444".into()]);
        let said = register_by(&world.paths, &world.store, &host, &[claims], &lookup).unwrap();
        assert!(
            said.iter().any(|line| line
                == "! big offers fake/vm, and says it is the server of dock-fake-vm, which is dock's; both are left as they are"),
            "{said:?}"
        );
        assert_eq!(names(&world), ["dock-fake-vm"]);
        let theirs = provisioner::entry(&world.paths, "dock-fake-vm")
            .unwrap()
            .unwrap();
        assert_eq!(
            (theirs.node.as_str(), theirs.settings["server"].as_str()),
            ("dock", "https://192.0.2.40:8444")
        );
    }

    #[test]
    fn an_offer_holds_what_the_features_manifest_offers() {
        let world = World::new();
        let host = world.host(&["fake/engine"]);
        // A feature the profile does not name.
        let mut unnamed = offer();
        unnamed.feature = "fake/desk".into();
        let said = register_by(&world.paths, &world.store, &host, &[unnamed], &lookup).unwrap();
        assert_eq!(
            said,
            [
                "! big reports what fake/desk offers, which is not among its features; it is left out"
            ]
        );
        // Settings that are not the provisioner's own arguments.
        let mut extra = offer();
        extra.provisioners = vec!["vm".into()];
        extra.settings.insert("command".into(), "/bin/sh".into());
        extra.secrets.insert("other_secret".into(), "x".into());
        register_by(&world.paths, &world.store, &host, &[extra], &lookup).unwrap();
        let entry = provisioner::entry(&world.paths, "big-fake-vm")
            .unwrap()
            .unwrap();
        assert_eq!(entry.settings.keys().collect::<Vec<_>>(), ["server"]);
        assert_eq!(
            world
                .store
                .secret("provisioner/big-fake-vm/other_secret")
                .unwrap(),
            None
        );
        assert_eq!(names(&world), ["big-fake-vm"]);
    }

    #[test]
    fn a_provisioner_is_made_a_nodes_only_where_that_fits() {
        let world = World::new();
        world.host(&["fake/engine"]);
        hand(&world, "incus-vm", "https://incus.example.net:8444");
        assert_eq!(
            adopt(&world.paths, "nothing", "big")
                .unwrap_err()
                .to_string(),
            "No provisioner nothing is registered"
        );
        assert_eq!(
            adopt(&world.paths, "incus-vm", "dock")
                .unwrap_err()
                .to_string(),
            "No host dock"
        );
        world.host(&[]);
        assert!(
            adopt(&world.paths, "incus-vm", "big")
                .unwrap_err()
                .to_string()
                .starts_with("No feature of big offers fake/vm")
        );
        let host = world.host(&["fake/engine"]);
        register_by(&world.paths, &world.store, &host, &[offer()], &lookup).unwrap();
        assert_eq!(
            adopt(&world.paths, "incus-vm", "big")
                .unwrap_err()
                .to_string(),
            "big has the provisioner big-fake-vm of the type fake/vm already. Remove it first: berth provisioner run big-fake-vm remove"
        );
        assert_eq!(
            adopt(&world.paths, "big-fake-vm", "big")
                .unwrap_err()
                .to_string(),
            "big-fake-vm is the node big's already"
        );
        let mut station = world.host(&["fake/engine"]);
        station.config["type"] = json!("workstation");
        station.place.save(&world.paths, &station.config).unwrap();
        assert_eq!(
            adopt(&world.paths, "incus-vm", "big")
                .unwrap_err()
                .to_string(),
            "big is not a node"
        );
    }

    #[test]
    fn one_registered_by_hand_has_the_action_that_makes_it_a_nodes() {
        let world = World::new();
        world.host(&["fake/engine"]);
        hand(&world, "incus-vm", "https://incus.example.net:8444");
        let listed = provisioner::actions(&world.paths, &world.store, "incus-vm").unwrap();
        let found = listed
            .iter()
            .find(|action| action.name == provisioner::ADOPT)
            .unwrap();
        assert_eq!(found.role, berth_api::ops::Role::Admin);
        let given = [("node".to_owned(), "big".to_owned())].into();
        let acting = provisioner::prepare(
            &world.paths,
            &world.store,
            "incus-vm",
            provisioner::ADOPT,
            &given,
        )
        .unwrap();
        let answered = acting
            .run(
                &world.paths,
                &crate::provision::Options::default(),
                &mut std::io::sink(),
            )
            .unwrap();
        assert!(answered.message.starts_with("incus-vm is big's now"));
        // A node's provisioner has no such action.
        let listed = provisioner::actions(&world.paths, &world.store, "incus-vm").unwrap();
        assert!(
            listed
                .iter()
                .all(|action| action.name != provisioner::ADOPT)
        );
    }

    #[test]
    fn what_a_node_no_longer_offers_goes_unless_it_holds_resources() {
        let world = World::new();
        let host = world.host(&["fake/engine"]);
        register(&world.paths, &world.store, &host, &[offer()]).unwrap();
        world.holding("big-fake-vm");
        // Applying without the feature is refused while the machine is there.
        let without = world.host(&[]);
        let refused = check_removal(&world.paths, &without)
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains(
                "big offers the provisioner big-fake-vm through fake/engine, which still holds r1"
            ),
            "{refused}"
        );
        assert!(check_removal(&world.paths, &host).is_ok());
        // Made a managed host, it would no longer be applied with it either.
        let mut managed = world.host(&["fake/engine"]);
        managed.config["type"] = json!("managed");
        assert!(check_removal(&world.paths, &managed).is_err());
        // A report without it keeps it, and says why.
        let said = register(&world.paths, &world.store, &host, &[]).unwrap();
        assert!(
            said[0].starts_with("! big no longer offers big-fake-vm, which is kept"),
            "{said:?}"
        );
        assert!(
            provisioner::entry(&world.paths, "big-fake-vm")
                .unwrap()
                .is_some()
        );
        // The node is not forgotten while it holds them, nor renamed apart from them.
        let refused = crate::profiles::remove(&world.paths, &host.place)
            .unwrap_err()
            .to_string();
        assert!(
            refused
                .contains("big is the node of the provisioner big-fake-vm, which still holds r1"),
            "{refused}"
        );
        let renamed = crate::profiles::rename(&world.paths, &host.place, "huge", false).unwrap();
        let node = provisioner::entry(&world.paths, "big-fake-vm")
            .unwrap()
            .unwrap()
            .node;
        assert_eq!(node, host.place.key());
        assert_eq!(node_name(&world.paths, &node), "huge");
        crate::profiles::rename(&world.paths, &renamed, "big", false).unwrap();
        // Once the machine is gone, so is the provisioner.
        crate::store::shared(&world.paths)
            .unwrap()
            .forget(crate::store::kind::RESOURCE, crate::store::OWN, "r1")
            .unwrap();
        let said = register(&world.paths, &world.store, &host, &[]).unwrap();
        assert!(
            said[0].starts_with("✓ Forgot the provisioner big-fake-vm"),
            "{said:?}"
        );
        assert!(
            provisioner::entry(&world.paths, "big-fake-vm")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            world
                .store
                .secret("provisioner/big-fake-vm/server_cert")
                .unwrap(),
            None
        );
        // Forgetting the node forgets its provisioners.
        register(&world.paths, &world.store, &host, &[offer()]).unwrap();
        crate::profiles::remove(&world.paths, &host.place).unwrap();
        assert!(provisioner::entries(&world.paths).unwrap().is_empty());
    }

    #[test]
    fn a_feature_is_for_the_types_its_manifest_names() {
        let world = World::new();
        let plugins = world.plugins();
        let node = world.host(&["fake/engine", "fake/desk"]);
        let refused = check_features(&plugins, "big", &node.config)
            .unwrap_err()
            .to_string();
        assert_eq!(
            refused,
            "big is a node, and fake/desk is for a workstation: take it out of its features"
        );
        let workstation = json!({"builder": {}, "ansible_vars": {"devbox_plugin_features": ["fake/engine", "fake/desk"]}});
        let refused = check_features(&plugins, "studio", &workstation)
            .unwrap_err()
            .to_string();
        assert_eq!(
            refused,
            "studio is a workstation, and fake/engine is for a node: take it out of its features"
        );
        assert!(
            check_features(
                &plugins,
                "big",
                &world.host(&["fake/engine", "other/x"]).config
            )
            .is_ok()
        );
    }

    #[test]
    fn berth_makes_the_client_certificate_a_feature_trusts_once() {
        let world = World::new();
        let plugins = world.plugins();
        let config = world.host(&["fake/engine"]).config;
        let first = client_certificates(&plugins, &world.store, &config).unwrap();
        let made = &first["fake/engine"];
        let print = made["fingerprint"].as_str().unwrap();
        assert_eq!(made["name"], format!("berth-{}", &print[..12]));
        assert!(made.get("key").is_none());
        assert!(
            world
                .store
                .secret("fake_client_key")
                .unwrap()
                .unwrap()
                .contains("PRIVATE KEY")
        );
        let again = client_certificates(&plugins, &world.store, &config).unwrap();
        assert_eq!(again, first);
        assert_eq!(
            client_certificates(&plugins, &world.store, &world.host(&[]).config).unwrap(),
            json!({})
        );
    }

    #[test]
    fn a_fingerprint_is_of_the_certificate_itself() {
        let (certificate, key) = make_client_certificate().unwrap();
        assert!(certificate.starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(key.contains("PRIVATE KEY"));
        let print = fingerprint(&certificate).unwrap();
        assert_eq!(print.len(), 64);
        assert_eq!(fingerprint(&format!("\n{certificate}\n")).unwrap(), print);
        assert!(fingerprint("not a certificate").is_none());
    }

    #[test]
    fn offered_values_name_where_berth_reaches_the_node() {
        let host = |access: Value| Host {
            name: "big".into(),
            place: Place::at("", "0197000000007000800000000000000c", "big"),
            config: json!({"type": "node", "access": access}),
        };
        let root = host(json!({"address": "192.0.2.20", "user": "root"}));
        assert_eq!(
            fill("https://{address}:8444", &root),
            "https://192.0.2.20:8444"
        );
        assert_eq!(
            fill("ssh://{user}@{address}:{port}", &root),
            "ssh://root@192.0.2.20:22"
        );
        let six = host(json!({"address": "2001:db8::20", "user": "ops", "port": "2222"}));
        assert_eq!(
            fill("https://{address}:8444", &six),
            "https://[2001:db8::20]:8444"
        );
        assert_eq!(
            fill("ssh://{user}@{address}:{port}", &six),
            "ssh://ops@[2001:db8::20]:2222"
        );
        assert_eq!(server_of("https://Big.example:8444/x"), "big.example:8444");
        assert_eq!(server_of("ssh://root@192.0.2.20"), "192.0.2.20");
        assert_eq!(free_name(&[], "big", "incus/vm"), "big-incus-vm");
        let taken = vec![("big-incus-vm".to_owned(), Entry::default())];
        assert_eq!(free_name(&taken, "big", "incus/vm"), "big-incus-vm-2");
        assert_eq!(
            free_name(&[], "7.node", "docker/container"),
            "n-7-node-docker-container"
        );
    }
}
