//! Stores that berth 0.5.2 and 0.6.0-rc.4 wrote, with made-up hosts and fake
//! keys, opened by this berth: each is copied as it was, converted once, and
//! the next start changes nothing. The store of 0.6.0-rc.4 was made with that
//! berth's commands, and the rows its commands make only with a machine or a
//! server, such as resources, jobs and messages, were added in its layout.

use berth_core::paths::Paths;
use berth_core::store::{self, kind};
use std::os::unix::fs::PermissionsExt;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/store-0.5.2.db");
const FIXTURE_RC4: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/store-0.6.0-rc.4.db"
);

fn installation() -> (tempfile::TempDir, Paths) {
    installation_of(FIXTURE)
}

fn installation_of(fixture: &str) -> (tempfile::TempDir, Paths) {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths {
        config_dir: dir.path().join("config"),
        data_dir: dir.path().join("data"),
    };
    std::fs::create_dir_all(&paths.data_dir).unwrap();
    let file = paths.data_dir.join("berth.db");
    std::fs::copy(fixture, &file).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    (dir, paths)
}

fn schema(file: &std::path::Path) -> i64 {
    let db = rusqlite::Connection::open(file).unwrap();
    db.query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap()
}

fn rows(store: &store::Store) -> store::Rows {
    store.export(store::Table::State).unwrap()
}

#[test]
fn a_store_of_berth_0_5_2_is_copied_then_converted_once() {
    let (_dir, paths) = installation();
    let file = paths.data_dir.join("berth.db");
    assert_eq!(schema(&file), 4);

    let store = store::open_converting(&paths).unwrap();
    let copy = paths.data_dir.join("berth.db.berth-0.5.2-schema-4");
    assert!(copy.is_file());
    assert_eq!(
        std::fs::metadata(&copy).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // The copy is the store as berth 0.5.2 left it, which that berth opens.
    assert_eq!(schema(&copy), 4);
    let old = rusqlite::Connection::open(&copy).unwrap();
    let level: String = old
        .query_row(
            "SELECT CAST(value AS TEXT) FROM state WHERE kind = 'profile' AND scope = '' AND name = 'studio'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(level.contains("\"level\":\"workstation\""), "{level}");
    assert!(level.contains("tskey-auth-kFAKE"), "{level}");

    // The store itself is converted: names of now, no pasted key, every marker.
    assert!(store.schema_version().unwrap() > 4);
    // Hosts are kept by their ids, with their names beside.
    let profile = |name: &str| {
        let id = store
            .id_of(kind::PROFILE, store::OWN, name)
            .unwrap()
            .unwrap();
        store
            .read_json(kind::PROFILE, store::OWN, &id)
            .unwrap()
            .unwrap()
    };
    let studio = profile("studio");
    assert_eq!(
        berth_api::profile::host_type(&studio),
        berth_api::HostType::Workstation
    );
    assert!(studio.get("level").is_none() && studio.get("tailscale_auth_key").is_none());
    let pier = profile("pier");
    assert_eq!(
        berth_api::profile::host_type(&pier),
        berth_api::HostType::Managed
    );
    // dock had berth 0.5 make its key, so it joins as before; pier never joined.
    let dock = profile("dock");
    assert!(
        dock["ansible_vars"]
            .get("devbox_tailnet_optional")
            .is_none(),
        "{dock}"
    );
    assert_eq!(pier["ansible_vars"]["devbox_tailnet_optional"], true);
    let names = store.names(kind::META, store::OWN).unwrap();
    for marker in berth_core::import::MARKERS {
        assert!(names.iter().any(|name| name == marker), "{marker}");
    }
    ids_are_given(&paths, &store);
    let converted = rows(&store);
    drop(store);

    // The next start finds nothing to do, and makes no copy.
    std::fs::remove_file(&copy).unwrap();
    let store = store::open_converting(&paths).unwrap();
    assert_eq!(rows(&store), converted);
    assert!(!copy.exists());
    let left: Vec<String> = std::fs::read_dir(&paths.data_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("schema-"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

/// Everything kept under a name has an id, and what refers to it or
/// belongs to it uses the id.
fn ids_are_given(paths: &Paths, store: &store::Store) {
    use berth_core::profiles::Place;
    let id = |kind: &str, scope: &str, name: &str| {
        let id = store.id_of(kind, scope, name).unwrap().unwrap();
        assert!(berth_api::ids::is_id(&id), "{kind} {name}: {id}");
        id
    };
    for host in ["studio", "dock", "pier"] {
        id(kind::PROFILE, store::OWN, host);
    }
    for host in ["harbor", "wharf"] {
        id(kind::PROFILE, "served:http://127.0.0.1:17777", host);
    }
    let nexus = id(kind::REGISTRY, store::OWN, "nexus");
    id(kind::REGISTRY, store::OWN, "hub");
    let npm = id(kind::SECRET, store::OWN, "npm_token");
    let deploy = id(kind::KEY, store::OWN, "deploy");
    id(kind::KEY, store::OWN, "berth");
    id(kind::SECRET, store::OWN, "tailnet_api_key");
    // The store refers to the registry, the secrets and the key by id; a
    // profile read says their names.
    let studio = id(kind::PROFILE, store::OWN, "studio");
    let kept = store
        .read_json(kind::PROFILE, store::OWN, &studio)
        .unwrap()
        .unwrap();
    assert_eq!(kept["registries"], serde_json::json!([nexus]));
    assert_eq!(kept["secrets"][0]["name"], npm.as_str());
    let read = Place::own(paths, "studio")
        .unwrap()
        .load(paths)
        .unwrap()
        .unwrap();
    assert_eq!(read["registries"], serde_json::json!(["nexus"]));
    assert_eq!(read["secrets"][0]["name"], "npm_token");
    let dock = id(kind::PROFILE, store::OWN, "dock");
    let kept = store
        .read_json(kind::PROFILE, store::OWN, &dock)
        .unwrap()
        .unwrap();
    assert_eq!(kept["access"]["auth"], format!("key:{deploy}"));
    let read = Place::own(paths, "dock")
        .unwrap()
        .load(paths)
        .unwrap()
        .unwrap();
    assert_eq!(read["access"]["auth"], "key:deploy");
    // The registry's login belongs to it.
    let secrets = berth_core::registry::Store::new(paths).unwrap();
    assert_eq!(
        store
            .read(kind::SECRET, store::OWN, &format!("{nexus}/login"))
            .unwrap()
            .is_some(),
        secrets.has_secret("registry/nexus").unwrap()
    );
    assert!(secrets.has_secret("registry/nexus").unwrap());
    // Owned secrets are not among the global ones.
    assert!(
        secrets
            .secret_names()
            .iter()
            .all(|name| !name.contains('/'))
    );
    // Jobs are about the hosts by id.
    let jobs = store.jobs(10).unwrap();
    assert!(
        jobs.iter()
            .any(|job| job.subject == format!("host:{studio}") && job.host == studio),
        "{jobs:?}"
    );
    assert!(jobs.iter().all(|job| !job.subject.ends_with(":studio")));
}

#[test]
fn only_the_latest_copy_is_kept() {
    let (_dir, paths) = installation();
    let earlier = paths.data_dir.join("berth.db.berth-0.4.1-schema-3");
    std::fs::write(&earlier, b"an earlier copy").unwrap();
    let said = store::keep_before_converting(&paths.data_dir.join("berth.db"))
        .unwrap()
        .unwrap();
    assert!(
        said.contains("berth 0.5.2 needs that copy to go back"),
        "{said}"
    );
    assert!(!earlier.exists());
    assert!(
        paths
            .data_dir
            .join("berth.db.berth-0.5.2-schema-4")
            .is_file()
    );
    // A store of this berth needs no copy.
    store::open_converting(&paths).unwrap();
    assert_eq!(
        store::keep_before_converting(&paths.data_dir.join("berth.db")).unwrap(),
        None
    );
}

#[test]
fn a_store_of_berth_0_6_0_rc_4_is_converted_to_ids_once() {
    use berth_core::{profiles::Place, provisioner, registry, resource};
    let (_dir, paths) = installation_of(FIXTURE_RC4);
    // The plugins of this source, which say what each provisioner and feature takes.
    paths
        .set("source", concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
        .unwrap();
    let file = paths.data_dir.join("berth.db");
    assert_eq!(schema(&file), 6);
    let store = store::open_converting(&paths).unwrap();
    assert!(
        paths
            .data_dir
            .join("berth.db.berth-0.6.0-rc.4-schema-6")
            .is_file()
    );
    let host = |name: &str| Place::find(&paths, store::OWN, name).unwrap().unwrap();
    let (studio, dock) = (host("studio"), host("dock"));
    assert!(berth_api::ids::is_id(&studio.key()) && berth_api::ids::is_id(&dock.key()));
    let lab = provisioner::entry(&paths, "lab").unwrap().unwrap();
    let node = provisioner::entry(&paths, "dock-incus-vm")
        .unwrap()
        .unwrap();
    assert!(berth_api::ids::is_id(&lab.id));
    // A node's provisioner refers to the node by id.
    assert_eq!(node.node, dock.key());
    assert_eq!(node.feature, "incus/server");

    // Each provisioner keeps its own server certificate, and has its own copy
    // of the client certificate it took from those all share.
    let secrets = registry::Store::new(&paths).unwrap();
    let secret = |name: &str| secrets.secret(name).unwrap();
    let client = secret("incus_client_cert").unwrap();
    for provisioner in ["lab", "dock-incus-vm"] {
        assert_eq!(
            secret(&format!("provisioner/{provisioner}/incus_client_cert")).as_deref(),
            Some(client.as_str())
        );
        assert!(secret(&format!("provisioner/{provisioner}/incus_client_key")).is_some());
    }
    assert!(
        secret("provisioner/lab/incus_server_cert")
            .unwrap()
            .contains("made-up-lab")
    );
    assert!(
        secret("provisioner/dock-incus-vm/incus_server_cert")
            .unwrap()
            .contains("made-up-dock")
    );
    // The client certificate stays as well: the feature of the node trusts it.
    assert_eq!(
        secrets.secret_names(),
        [
            "github_token",
            "incus_client_cert",
            "incus_client_key",
            "incus_server_cert",
            "tailnet_api_key"
        ]
    );
    // What belongs to a host or a registry is kept by its id.
    assert_eq!(secret("sudo/studio").as_deref(), Some("sudo-pw-made-up"));
    assert_eq!(
        store
            .read(kind::SECRET, store::OWN, &format!("{}/sudo", studio.key()))
            .unwrap()
            .as_deref(),
        Some(&b"sudo-pw-made-up"[..])
    );
    assert_eq!(
        secret("registry/nexus").as_deref(),
        Some("nexus-pw-made-up")
    );
    // A profile in a file keeps its password by its path.
    assert_eq!(
        secret("ssh//srv/made-up/attic.json").as_deref(),
        Some("attic-pw-made-up")
    );
    assert_eq!(
        berth_core::credentials::load(&paths, &studio).unwrap()["ops"],
        "made-up-ops-pw"
    );
    let known = berth_core::keys::known::Record::load(&store, &studio).unwrap();
    assert_eq!(known.host_keys, ["ssh-ed25519 AAAAmadeuphostkey"]);
    let profile = studio.load(&paths).unwrap().unwrap();
    assert_eq!(profile["registries"], serde_json::json!(["nexus"]));
    assert_eq!(profile["secrets"][0]["name"], "github_token");

    // Resources have ids of the new shape, and refer to their provisioner and host by id.
    let machine = resource::of_place(&paths, &studio).unwrap().unwrap();
    assert!(resource::valid_id(&machine.record.id));
    assert_eq!(machine.record.provisioner, lab.id);
    assert_eq!(machine.record.external_id, "berth-studio");
    let web = resource::list(&paths)
        .unwrap()
        .into_iter()
        .find(|kept| kept.record.name == "web")
        .unwrap();
    assert_eq!(web.record.provisioner, node.id);
    assert_eq!(web.spec(&paths).unwrap().as_deref(), Some("services: {}"));

    // Jobs and messages are about them by id.
    let job = |id: &str| store.job(id).unwrap().unwrap();
    assert_eq!(
        job("jstudio000001").subject,
        format!("host:{}", studio.key())
    );
    assert_eq!(
        job("jlab000000002").subject,
        format!("provisioner:{}", lab.id)
    );
    assert_eq!(
        job("jres000000003").subject,
        format!("resource:{}", machine.record.id)
    );
    assert_eq!(
        job("jroll00000004").host,
        format!("{}, {}", studio.key(), dock.key())
    );
    let (messages, _) = store
        .inbox(&store::Filter {
            limit: 10,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(messages[0].host, studio.key());
    assert_eq!(store.cursor(&studio.key()).unwrap(), 1);
    let converted = rows(&store);
    drop(store);

    // Opened again, nothing changes.
    let store = store::open_converting(&paths).unwrap();
    assert_eq!(rows(&store), converted);

    // A rename changes the name alone.
    provisioner::rename(&paths, &secrets, "lab", "lab2").unwrap();
    assert_eq!(
        secret("provisioner/lab2/incus_server_cert"),
        secret(&format!("provisioner/{}/incus_server_cert", lab.id))
    );
    assert!(secret("provisioner/lab2/incus_server_cert").is_some());
    let machine = resource::of_place(&paths, &studio).unwrap().unwrap();
    assert_eq!(machine.record.provisioner, lab.id);
    assert_eq!(
        provisioner::label(&paths, &machine.record.provisioner),
        "lab2"
    );
}
