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
    provisioner::{self, Entry},
    registry::Store,
};
use berth_api::{HostType, Offer};
use serde_json::{Value, json};
use sha2::Digest;
use std::collections::BTreeMap;

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

/// How surely a provisioner registered by hand reaches the server an offer
/// is of, 0 when it does not: it pins the same certificate as its own (4),
/// names the server the same way (2), or is given the same certificate that
/// all of its type share (1), which says less when several share it.
fn same_server(store: &Store, name: &str, entry: &Entry, offer: &Offer, host: &Host) -> u8 {
    let pins = |secret: &str, value: &str| {
        store
            .secret(secret)
            .ok()
            .flatten()
            .is_some_and(|kept| !kept.trim().is_empty() && kept.trim() == value.trim())
    };
    let own = offer
        .secrets
        .iter()
        .any(|(secret, value)| pins(&provisioner::secret_name(name, secret), value));
    let shared = offer.secrets.iter().any(|(secret, value)| {
        store
            .secret(&provisioner::secret_name(name, secret))
            .ok()
            .flatten()
            .is_none()
            && pins(secret, value)
    });
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
    4 * u8::from(own) + 2 * u8::from(named) + u8::from(shared)
}

/// Register the provisioners the node `host` offers, as its last apply
/// reported them: one berth registered for the node before is brought up to
/// date, one registered by hand that reaches the same server is taken over,
/// and any other is registered under a new name. Those of the node no
/// feature offers any more are forgotten, unless they still hold resources.
/// Says what it did, a line each.
pub fn register(
    paths: &Paths,
    store: &Store,
    host: &Host,
    offers: &[Offer],
) -> Result<Vec<String>> {
    let plugins = provision_plugins(paths);
    let mut said = vec![];
    let mut kept: Vec<String> = vec![];
    for offer in offers {
        let Some((plugin_id, _)) = offer.feature.split_once('/') else {
            continue;
        };
        for id in &offer.provisioners {
            let kind = format!("{plugin_id}/{id}");
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
            let registered = provisioner::entries(paths)?;
            let ours = registered
                .iter()
                .find(|(_, entry)| entry.node == host.name && entry.kind == kind);
            let theirs = || {
                registered
                    .iter()
                    .filter(|(_, entry)| entry.node.is_empty() && entry.kind == kind)
                    .map(|found| (same_server(store, &found.0, &found.1, offer, host), found))
                    .filter(|(score, _)| *score > 0)
                    .max_by_key(|(score, _)| *score)
                    .map(|(_, found)| found)
            };
            let (name, how) = match ours.or_else(theirs) {
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
                node: host.name.clone(),
                feature: offer.feature.clone(),
            };
            provisioner::save_entry(paths, &name, &entry)?;
            for (secret, value) in &offer.secrets {
                if !declared.secrets.iter().any(|known| &known.name == secret) {
                    continue;
                }
                let own = provisioner::secret_name(&name, secret);
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
        if entry.node != host.name || kept.contains(&name) {
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
        if entry.node != host.name || (applied && wanted.contains(&entry.feature)) {
            continue;
        }
        let held = holds(paths, &name)?;
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

/// The resources a registered provisioner holds, by id.
fn holds(paths: &Paths, name: &str) -> Result<Vec<String>> {
    Ok(crate::resource::list(paths)?
        .into_iter()
        .filter(|kept| kept.record.provisioner == name)
        .map(|kept| kept.record.id)
        .collect())
}

/// Forget the provisioners of the node `name` before the host goes: refused
/// while one still holds resources, as these would have no provisioner.
pub fn forget(paths: &Paths, store: &Store, name: &str) -> Result<()> {
    let ours: Vec<String> = provisioner::entries(paths)?
        .into_iter()
        .filter(|(_, entry)| entry.node == name)
        .map(|(provisioner, _)| provisioner)
        .collect();
    for provisioner in &ours {
        let held = holds(paths, provisioner)?;
        if !held.is_empty() {
            return Err(format!(
                "{name} is the node of the provisioner {provisioner}, which still holds {}. Remove {} first: berth resource ID remove",
                held.join(", "),
                if held.len() == 1 { "it" } else { "them" }
            )
            .into());
        }
    }
    for provisioner in ours {
        provisioner::remove_entry(paths, store, &provisioner)?;
    }
    Ok(())
}

/// Give the provisioners of the node `old` its new name.
pub fn rename(paths: &Paths, old: &str, new: &str) -> Result<()> {
    for (name, mut entry) in provisioner::entries(paths)? {
        if entry.node == old {
            entry.node = new.to_owned();
            provisioner::save_entry(paths, &name, &entry)?;
        }
    }
    Ok(())
}

/// What the node `host` reported at the end of its last apply, asked of its
/// controller until it answers: the provisioners its features offer.
pub fn reported(paths: &Paths, host: &Host) -> Result<Vec<Offer>> {
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
            Ok(Ok(version)) => {
                return Ok(version
                    .applied
                    .map(|applied| applied.offers)
                    .unwrap_or_default());
            }
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
            let place = Place::own("big").unwrap();
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
                    provisioner: provisioner.into(),
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
            said[1].contains("fake/gone, which no plugin here has"),
            "{said:?}"
        );
        let entry = provisioner::entry(&world.paths, "big-fake-vm")
            .unwrap()
            .unwrap();
        assert_eq!(
            (entry.node.as_str(), entry.feature.as_str()),
            ("big", "fake/engine")
        );
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

    #[test]
    fn one_registered_by_hand_for_the_same_server_is_taken_over() {
        let world = World::new();
        let host = world.host(&["fake/engine"]);
        // As the recipe registered it: its own name and setting, the shared certificate.
        let hand = Entry {
            kind: "fake/vm".into(),
            settings: [("server".into(), "https://incus-host:8444".into())].into(),
            ..Entry::default()
        };
        provisioner::save_entry(&world.paths, "incus-vm", &hand).unwrap();
        world.store.set_secret("server_cert", Some("CERT")).unwrap();
        let other = Entry {
            kind: "fake/vm".into(),
            settings: [("server".into(), "https://192.0.2.99:8444".into())].into(),
            ..Entry::default()
        };
        provisioner::save_entry(&world.paths, "elsewhere", &other).unwrap();
        let said = register(&world.paths, &world.store, &host, &[offer()]).unwrap();
        assert!(
            said.iter()
                .any(|line| line.contains("incus-vm reaches the same server, and is big's now")),
            "{said:?}"
        );
        let names: Vec<String> = provisioner::entries(&world.paths)
            .unwrap()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["elsewhere", "incus-vm"]);
        let taken = provisioner::entry(&world.paths, "incus-vm")
            .unwrap()
            .unwrap();
        assert_eq!(taken.node, "big");
        // Its setting reaches the server already, and stays.
        assert_eq!(taken.settings["server"], "https://incus-host:8444");
        assert!(
            provisioner::entry(&world.paths, "elsewhere")
                .unwrap()
                .unwrap()
                .node
                .is_empty()
        );

        // Without a certificate to compare, the address it names is enough.
        let world = World::new();
        let host = world.host(&["fake/engine"]);
        let named = Entry {
            kind: "fake/vm".into(),
            settings: [("server".into(), "https://192.0.2.20:8444/".into())].into(),
            ..Entry::default()
        };
        provisioner::save_entry(&world.paths, "by-address", &named).unwrap();
        let mut unpinned = offer();
        unpinned.secrets.clear();
        register(&world.paths, &world.store, &host, &[unpinned]).unwrap();
        assert_eq!(provisioner::entries(&world.paths).unwrap().len(), 1);
        assert_eq!(
            provisioner::entry(&world.paths, "by-address")
                .unwrap()
                .unwrap()
                .node,
            "big"
        );

        // An engine where berth runs is the node's only when the node says so.
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
        register(&world.paths, &world.store, &host, &[local.clone()]).unwrap();
        assert!(
            provisioner::entry(&world.paths, "local")
                .unwrap()
                .unwrap()
                .node
                .is_empty()
        );
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
        local.aliases.insert("server".into(), vec![String::new()]);
        register(&world.paths, &world.store, &host, &[local]).unwrap();
        assert_eq!(
            provisioner::entry(&world.paths, "local")
                .unwrap()
                .unwrap()
                .node,
            "big"
        );
        assert_eq!(provisioner::entries(&world.paths).unwrap().len(), 1);
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
        assert_eq!(
            provisioner::entry(&world.paths, "big-fake-vm")
                .unwrap()
                .unwrap()
                .node,
            "huge"
        );
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
            place: Place::own("big").unwrap(),
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
