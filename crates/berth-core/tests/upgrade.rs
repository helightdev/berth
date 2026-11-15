//! A store that berth 0.5.2 wrote, with made-up hosts and fake keys, opened by
//! this berth: it is copied as it was, converted once, and the next start
//! changes nothing.

use berth_core::paths::Paths;
use berth_core::store::{self, kind};
use std::os::unix::fs::PermissionsExt;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/store-0.5.2.db");

fn installation() -> (tempfile::TempDir, Paths) {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths {
        config_dir: dir.path().join("config"),
        data_dir: dir.path().join("data"),
    };
    std::fs::create_dir_all(&paths.data_dir).unwrap();
    let file = paths.data_dir.join("berth.db");
    std::fs::copy(FIXTURE, &file).unwrap();
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
    let studio = store
        .read_json(kind::PROFILE, store::OWN, "studio")
        .unwrap()
        .unwrap();
    assert_eq!(
        berth_api::profile::host_type(&studio),
        berth_api::HostType::Workstation
    );
    assert!(studio.get("level").is_none() && studio.get("tailscale_auth_key").is_none());
    let pier = store
        .read_json(kind::PROFILE, store::OWN, "pier")
        .unwrap()
        .unwrap();
    assert_eq!(
        berth_api::profile::host_type(&pier),
        berth_api::HostType::Managed
    );
    // dock had berth 0.5 make its key, so it joins as before; pier never joined.
    let dock = store
        .read_json(kind::PROFILE, store::OWN, "dock")
        .unwrap()
        .unwrap();
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
    let secrets = store.names(kind::SECRET, store::OWN).unwrap();
    assert!(secrets.iter().any(|name| name == "tailnet_api_key"));
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
