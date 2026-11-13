use super::*;
use serde_json::json;

fn scratch() -> (tempfile::TempDir, Paths, std::sync::Arc<Store>) {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths {
        config_dir: dir.path().join("config"),
        data_dir: dir.path().join("data"),
    };
    let store = crate::store::shared(&paths).unwrap();
    (dir, paths, store)
}

fn option<'a>(args: &'a [String], name: &str) -> Vec<&'a str> {
    args.windows(2)
        .filter(|pair| pair[0] == "-o")
        .filter_map(|pair| pair[1].strip_prefix(&format!("{name}=")))
        .collect()
}

fn login_for(paths: &Paths, store: &Store, access: Value) -> Login {
    let place = Place::own("studio").unwrap();
    login(paths, store, &place, &json!({"access": access})).unwrap()
}

#[test]
fn berth_and_a_named_key_offer_that_key_alone() {
    let (_dir, paths, store) = scratch();
    let berth = login_for(
        &paths,
        &store,
        json!({"address": "192.0.2.10", "user": "ops"}),
    );
    let args = berth.options();
    assert_eq!(
        option(&args, "IdentityFile"),
        [paths.data_dir.join("keys/berth").to_str().unwrap()]
    );
    assert_eq!(option(&args, "IdentitiesOnly"), ["yes"]);
    assert_eq!(option(&args, "HostKeyAlias"), ["berth.studio"]);
    assert_eq!(option(&args, "StrictHostKeyChecking"), ["accept-new"]);
    assert!(!args.contains(&"-p".to_owned()));
    assert!(
        berth
            .env()
            .contains(&("SSH_ASKPASS_REQUIRE".into(), "never".into()))
    );
    assert_eq!(berth.target(), "ops@192.0.2.10");
    // The private half is a file now, written from the store.
    let file = paths.data_dir.join("keys/berth");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );

    ring::add(&paths, &store, "deploy", None).unwrap();
    let named = login_for(
        &paths,
        &store,
        json!({"address": "192.0.2.10", "port": "2222", "auth": "key:deploy"}),
    );
    let args = named.options();
    assert_eq!(&args[..2], ["-p", "2222"]);
    assert_eq!(
        option(&args, "IdentityFile"),
        [paths.data_dir.join("keys/deploy").to_str().unwrap()]
    );
    // A key that is not in the ring is an error, not ssh's defaults.
    let place = Place::own("studio").unwrap();
    let missing = json!({"access": {"address": "192.0.2.10", "auth": "key:gone"}});
    assert!(login(&paths, &store, &place, &missing).is_err());
}

#[test]
fn a_password_goes_through_the_helper_and_never_into_the_arguments() {
    let (_dir, paths, store) = scratch();
    let access = json!({"address": "192.0.2.10", "auth": "password"});
    let without = login_for(&paths, &store, access.clone());
    assert!(
        without
            .needs_password()
            .unwrap()
            .contains("berth secret set ssh/studio")
    );
    assert!(without.link(None).is_err());
    crate::registry::Store::own(store.clone())
        .set_secret("ssh/studio", Some("made-up-pw"))
        .unwrap();
    let with = login_for(&paths, &store, access);
    let args = with.options();
    assert_eq!(option(&args, "PubkeyAuthentication"), ["no"]);
    assert!(option(&args, "IdentityFile").is_empty());
    let spec = with.link(None).unwrap();
    assert!(!spec.args.iter().any(|arg| arg.contains("BatchMode")));
    let env: std::collections::HashMap<_, _> = spec.env.iter().cloned().collect();
    assert_eq!(env["SSH_ASKPASS_REQUIRE"], "force");
    assert_eq!(env[askpass::SOURCE], "secret:ssh/studio");
    for word in spec
        .args
        .iter()
        .chain(spec.env.iter().map(|(_, value)| value))
    {
        assert!(!word.contains("made-up-pw"), "{word}");
    }
    let inventory = with.inventory().unwrap();
    assert!(!inventory.to_string().contains("made-up-pw"));
    // A one-time password is a private file the helper reads.
    let dir = tempfile::tempdir().unwrap();
    let once = without
        .with_once(&Once::Password("one-time".into()), dir.path())
        .unwrap();
    let source = once
        .env()
        .into_iter()
        .find(|(name, _)| name == askpass::SOURCE);
    let file = source.unwrap().1;
    assert_eq!(
        std::fs::read_to_string(file.strip_prefix("file:").unwrap()).unwrap(),
        "one-time"
    );
    assert!(!once.options().iter().any(|arg| arg.contains("one-time")));
}

#[test]
fn the_ssh_configuration_comes_first_and_berths_key_after() {
    let (_dir, paths, store) = scratch();
    let ssh = login_for(
        &paths,
        &store,
        json!({"address": "studio-alias", "auth": "ssh"}),
    );
    let args = ssh.options();
    assert!(option(&args, "IdentitiesOnly").is_empty());
    let berth = paths.data_dir.join("keys/berth");
    assert_eq!(
        option(&args, "IdentityFile").last().copied(),
        berth.to_str()
    );
    assert_eq!(ssh.target(), "studio-alias");
}

#[test]
fn the_link_and_the_inventory_take_the_same_options() {
    let (_dir, paths, store) = scratch();
    for (access, port) in [
        (json!({"address": "192.0.2.10", "user": "ops"}), None),
        (
            json!({"address": "192.0.2.10", "user": "ops", "port": "2222"}),
            Some(2222),
        ),
    ] {
        let login = login_for(&paths, &store, access);
        let spec = login.link(None).unwrap();
        assert_eq!(spec.program, "ssh");
        assert_eq!(spec.args.last().unwrap(), "sudo -n berthd connect");
        assert!(spec.args.contains(&"ops@192.0.2.10".to_owned()));
        assert_eq!(option(&spec.args, "BatchMode"), ["yes"]);
        assert!(option(&spec.args, "ControlPath")[0].ends_with("ssh-%C"));
        for option in login.options() {
            assert!(spec.args.contains(&option));
        }
        let sudo = login.link(Some("secret")).unwrap();
        assert_eq!(sudo.first_line.as_deref(), Some("secret"));
        assert!(sudo.args.last().unwrap().contains("sudo -S -k"));
        assert!(!sudo.args.iter().any(|arg| arg.contains("secret")));

        let inventory = login.inventory().unwrap();
        let target = &inventory["all"]["hosts"]["devbox_target"];
        assert_eq!(target["ansible_user"], "ops");
        assert_eq!(target["ansible_port"], json!(port));
        let common = target["ansible_ssh_common_args"].as_str().unwrap();
        assert!(common.contains("HostKeyAlias=berth.studio") && !common.contains("-p "));
        assert!(common.contains(&format!(
            "IdentityFile={}",
            paths.data_dir.join("keys/berth").display()
        )));
    }
    let bad = |access: Value| login_for(&paths, &store, access).inventory().is_err();
    assert!(bad(json!({"address": "-oProxyCommand=x"})));
    assert!(bad(json!({"address": "box", "port": "0"})));
}

#[test]
fn a_pending_key_is_offered_beside_the_current_one() {
    let (_dir, paths, store) = scratch();
    ring::begin_rotation(&paths, &store, BERTH_KEY).unwrap();
    let login = login_for(&paths, &store, json!({"address": "192.0.2.10"}));
    assert_eq!(option(&login.options(), "IdentityFile").len(), 2);
    assert_eq!(operator_keys(&paths).len(), 2);
}

#[test]
fn the_ring_takes_keys_ssh_keygen_reads_and_no_passphrase() {
    let (dir, paths, store) = scratch();
    let made = dir.path().join("made");
    let keygen = |args: &[&str]| {
        assert!(
            Command::new("ssh-keygen")
                .args(args)
                .arg("-f")
                .arg(&made)
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap()
                .status
                .success()
        );
    };
    keygen(&["-q", "-t", "ecdsa", "-N", "", "-C", "made-up"]);
    let private = std::fs::read(&made).unwrap();
    let added = ring::add(&paths, &store, "deploy", Some(&private)).unwrap();
    assert_eq!(
        ring::blob(&added.public),
        ring::blob(&std::fs::read_to_string(made.with_extension("pub")).unwrap())
    );
    assert!(added.fingerprint.starts_with("SHA256:"));
    assert!(ring::add(&paths, &store, "deploy", Some(&private)).is_err());
    assert!(ring::add(&paths, &store, "bad name", None).is_err());
    assert!(ring::add(&paths, &store, "junk", Some(b"not a key")).is_err());
    std::fs::remove_file(&made).unwrap();
    std::fs::remove_file(made.with_extension("pub")).unwrap();
    keygen(&["-q", "-t", "ed25519", "-N", "made-up-phrase"]);
    let said = ring::add(
        &paths,
        &store,
        "locked",
        Some(&std::fs::read(&made).unwrap()),
    )
    .unwrap_err()
    .to_string();
    assert!(said.contains("passphrase"), "{said}");

    // The list shows public halves, berth first; no private half is in it.
    let listed = ring::list(&paths, &store).unwrap();
    let names: Vec<_> = listed.iter().map(|key| key.name.as_str()).collect();
    assert_eq!(names, ["berth", "deploy"]);
    assert!(!serde_json::to_string(&listed).unwrap().contains("PRIVATE"));

    // A key a host logs in with stays.
    Place::own("studio")
        .unwrap()
        .save(
            &paths,
            &json!({"access": {"address": "192.0.2.10", "auth": "key:deploy"}}),
        )
        .unwrap();
    let said = ring::remove(&paths, &store, "deploy")
        .unwrap_err()
        .to_string();
    assert!(said.contains("studio"), "{said}");
    assert!(ring::remove(&paths, &store, BERTH_KEY).is_err());
    Place::own("studio")
        .unwrap()
        .save(&paths, &json!({"access": {"address": "192.0.2.10"}}))
        .unwrap();
    ring::file(&paths, &store, "deploy", false).unwrap();
    ring::remove(&paths, &store, "deploy").unwrap();
    assert!(!paths.data_dir.join("keys/deploy").exists());
    assert_eq!(ring::names(&store).unwrap(), ["berth"]);
}

#[test]
fn the_store_wins_over_a_key_file_that_differs() {
    let (_dir, paths, store) = scratch();
    let file = ring::file(&paths, &store, BERTH_KEY, false)
        .unwrap()
        .unwrap();
    let kept = std::fs::read(&file).unwrap();
    std::fs::write(&file, "something else").unwrap();
    ring::file(&paths, &store, BERTH_KEY, false).unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), kept);
    assert_eq!(
        std::fs::read_to_string(paths.data_dir.join("keys/berth.replaced")).unwrap(),
        "something else"
    );
}

/// The environment a Command of the login gives its ssh child.
fn child_env(command: &Command) -> std::collections::HashMap<String, String> {
    command
        .get_envs()
        .filter_map(|(name, value)| Some((name.to_str()?.to_owned(), value?.to_str()?.to_owned())))
        .collect()
}

#[test]
fn no_child_ssh_can_reach_a_password_dialog_of_the_desktop() {
    let (_dir, paths, store) = scratch();
    let helper = std::env::current_exe().unwrap().display().to_string();
    // A password berth has: ssh asks berth, and nothing else.
    crate::registry::Store::own(store.clone())
        .set_secret("ssh/studio", Some("made-up-pw"))
        .unwrap();
    let with = login_for(
        &paths,
        &store,
        json!({"address": "192.0.2.10", "auth": "password"}),
    );
    for command in [with.ssh(), with.batch_ssh()] {
        let env = child_env(&command);
        assert_eq!(env["SSH_ASKPASS"], helper);
        assert_eq!(env["SSH_ASKPASS_REQUIRE"], "force");
        assert_eq!(env[askpass::SOURCE], "secret:ssh/studio");
        assert!(!env.values().any(|value| value.contains("made-up-pw")));
    }
    let link: std::collections::HashMap<_, _> = with.link(None).unwrap().env.into_iter().collect();
    assert_eq!(link["SSH_ASKPASS_REQUIRE"], "force");
    // Without one, and for every key, ssh asks no program at all: a terminal
    // session may prompt on the terminal, berth's own logins fail.
    crate::registry::Store::own(store.clone())
        .set_secret("ssh/studio", None)
        .unwrap();
    for access in [
        json!({"address": "192.0.2.10", "auth": "password"}),
        json!({"address": "192.0.2.10"}),
        json!({"address": "192.0.2.10", "auth": "ssh"}),
    ] {
        let login = login_for(&paths, &store, access);
        for command in [login.ssh(), login.batch_ssh()] {
            let env = child_env(&command);
            assert_eq!(env["SSH_ASKPASS_REQUIRE"], "never");
            assert_eq!(env["SSH_ASKPASS"], "/bin/false");
        }
    }
    let without = login_for(
        &paths,
        &store,
        json!({"address": "192.0.2.10", "auth": "password"}),
    );
    assert!(without.link(None).is_err());
}
