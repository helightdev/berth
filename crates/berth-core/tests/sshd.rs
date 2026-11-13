//! First contact, rekeying and host keys against a real sshd: the system's,
//! started as the current user on a free port of 127.0.0.1 in a scratch
//! directory, logging in as that user. sshd sets `HOME` of its sessions to
//! the scratch directory, so the keys berth installs land there and not in
//! the user's own `~/.ssh`, and ssh is given no configuration file. Without
//! sshd the test says so and passes.

use berth_core::host::Host;
use berth_core::keys::{self, Once, contact, known, ring};
use berth_core::paths::Paths;
use berth_core::profiles::Place;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

fn find(program: &str, also: &[&str]) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .chain(also.iter().map(PathBuf::from))
        .find(|candidate| candidate.is_file())
}

struct Sshd {
    dir: PathBuf,
    port: u16,
    child: Option<Child>,
}

impl Sshd {
    fn host_key(&self, name: &str) -> PathBuf {
        let key = self.dir.join(name);
        if !key.exists() {
            keygen(&key);
        }
        key
    }

    fn start(&mut self, sshd: &Path, host_key: &str) {
        self.stop();
        let key = self.host_key(host_key);
        let config = self.dir.join("sshd_config");
        let home = self.dir.join("home");
        fs::create_dir_all(home.join(".ssh")).unwrap();
        fs::write(
            &config,
            format!(
                "Port {port}\nListenAddress 127.0.0.1\nHostKey {key}\nPidFile {dir}/sshd.pid\n\
                 AuthorizedKeysFile {home}/.ssh/authorized_keys\nStrictModes no\nUsePAM no\n\
                 PasswordAuthentication no\nKbdInteractiveAuthentication no\nSetEnv HOME={home}\n",
                port = self.port,
                key = key.display(),
                dir = self.dir.display(),
                home = home.display(),
            ),
        )
        .unwrap();
        let child = Command::new(sshd)
            .args(["-D", "-e", "-f"])
            .arg(&config)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(self.dir.join("sshd.log")).unwrap())
            .spawn()
            .unwrap();
        self.child = Some(child);
        for _ in 0..50 {
            if std::net::TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        panic!(
            "sshd did not start: {}",
            fs::read_to_string(self.dir.join("sshd.log")).unwrap_or_default()
        );
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn authorized(&self) -> String {
        fs::read_to_string(self.dir.join("home/.ssh/authorized_keys")).unwrap_or_default()
    }
}

impl Drop for Sshd {
    fn drop(&mut self) {
        self.stop();
    }
}

fn keygen(file: &Path) {
    let made = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "made-up", "-f"])
        .arg(file)
        .stdin(Stdio::null())
        .status()
        .unwrap();
    assert!(made.success());
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn host(paths: &Paths, name: &str, port: u16) -> Host {
    let user = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout)
        .unwrap()
        .trim()
        .to_owned();
    let place = Place::own(name).unwrap();
    let config =
        json!({"access": {"address": "127.0.0.1", "port": port.to_string(), "user": user}});
    place.save(paths, &config).unwrap();
    Host::load(paths, name).unwrap()
}

#[test]
fn first_contact_host_keys_and_rekeying_against_sshd() {
    let Some(sshd) = find("sshd", &["/usr/sbin/sshd", "/usr/local/sbin/sshd"]) else {
        eprintln!("no sshd on this machine: skipped");
        return;
    };
    let ssh = find("ssh", &[]).expect("ssh");
    let scratch = tempfile::tempdir().unwrap();
    // ssh of the tests reads no configuration of the user's.
    let bin = scratch.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let wrapper = bin.join("ssh");
    fs::write(
        &wrapper,
        format!("#!/bin/sh\nexec {} -F /dev/null \"$@\"\n", ssh.display()),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut dirs = vec![bin.clone()];
    dirs.extend(std::env::split_paths(&path));
    // SAFETY: this is the only test of its program, and nothing else runs yet.
    unsafe {
        std::env::set_var("PATH", std::env::join_paths(dirs).unwrap());
        std::env::set_var("XDG_RUNTIME_DIR", scratch.path().join("runtime"));
    }
    let paths = Paths {
        config_dir: scratch.path().join("config"),
        data_dir: scratch.path().join("data"),
    };
    let mut server = Sshd {
        dir: scratch.path().join("sshd"),
        port: free_port(),
        child: None,
    };
    fs::create_dir_all(&server.dir).unwrap();
    server.start(&sshd, "host_a");
    let store = berth_core::store::shared(&paths).unwrap();

    // The provider's key is in authorized_keys, without a newline at the end.
    let admin = server.dir.join("admin");
    keygen(&admin);
    let admin_public = fs::read_to_string(admin.with_extension("pub")).unwrap();
    fs::write(
        server.dir.join("home/.ssh/authorized_keys"),
        admin_public.trim(),
    )
    .unwrap();
    let studio = host(&paths, "studio", server.port);

    // berth's key is refused, and nothing is installed without a credential.
    let said = contact::trust_key(&paths, &studio, None, false)
        .unwrap_err()
        .to_string();
    assert!(said.contains("berth key trust studio"), "{said}");
    let once = Once::Key(ring::validate(&paths, &fs::read(&admin).unwrap()).unwrap());
    let trusted = contact::trust_key(&paths, &studio, Some(once), false).unwrap();
    assert!(!trusted.already && trusted.key == "berth");
    assert_eq!(trusted.host_keys.len(), 1);
    let berth = ring::public(&paths, &store, "berth").unwrap();
    let lines: Vec<String> = server.authorized().lines().map(str::to_owned).collect();
    assert_eq!(
        lines,
        [admin_public.trim().to_owned(), berth.public.clone()]
    );
    // Again, nothing changes; the one-time key is gone from berth's data.
    let again = contact::trust_key(&paths, &studio, None, false).unwrap();
    assert!(again.already);
    assert_eq!(server.authorized().lines().count(), 2);
    assert!(fs::read_dir(&paths.data_dir).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".berth-")
    }));
    let record = known::Record::load(&store, &studio.place).unwrap();
    assert_eq!(record.trusts, std::slice::from_ref(&berth.fingerprint));

    // The link's command logs in with the key alone.
    let login = keys::login_of(&paths, &studio).unwrap();
    contact::works(&login).unwrap();

    // A changed host key is refused until it is accepted.
    server.start(&sshd, "host_b");
    let said = contact::works(&keys::login_of(&paths, &studio).unwrap())
        .unwrap_err()
        .to_string();
    assert!(said.contains("berth key host studio --accept"), "{said}");
    // First contact says so too, without a credential and with one, and the
    // credential is not sent to a machine that may not be the host.
    let said = contact::trust_key(&paths, &studio, None, false)
        .unwrap_err()
        .to_string();
    assert!(said.starts_with("The SSH host key of studio"), "{said}");
    let authorized = server.authorized();
    let once = Once::Key(ring::validate(&paths, &fs::read(&admin).unwrap()).unwrap());
    let said = contact::trust_key(&paths, &studio, Some(once), false)
        .unwrap_err()
        .to_string();
    assert!(said.starts_with("The SSH host key of studio"), "{said}");
    assert!(said.contains("berth key host studio --accept"), "{said}");
    assert_eq!(server.authorized(), authorized);
    let before = contact::host_key(&paths, &studio, false).unwrap();
    let after = contact::host_key(&paths, &studio, true).unwrap();
    assert_eq!(before.len(), 1);
    assert_eq!(after.len(), 1);
    assert_ne!(before, after);
    contact::works(&keys::login_of(&paths, &studio).unwrap()).unwrap();

    // A rotation with a host that does not answer leaves it pending, and
    // offers both keys until it is done.
    let attic = host(&paths, "attic", free_port());
    let mut out = vec![];
    let first = contact::rotate(&paths, "berth", &mut out).unwrap();
    assert_eq!(first.done, ["studio"], "{}", String::from_utf8_lossy(&out));
    assert_eq!(first.pending.len(), 1);
    assert_eq!(first.pending[0].host, "attic");
    assert!(!first.finished);
    let rotating = ring::public(&paths, &store, "berth").unwrap();
    assert_eq!(rotating.public, berth.public);
    let next = rotating.pending.clone().unwrap();
    let authorized = server.authorized();
    assert!(authorized.contains(&ring::blob(&next.public)));
    assert!(!authorized.contains(&ring::blob(&berth.public)));
    assert!(authorized.contains(admin_public.trim()));
    contact::works(&keys::login_of(&paths, &studio).unwrap()).unwrap();
    assert_eq!(keys::operator_keys(&paths, &json!({})).unwrap().len(), 2);

    // Run again once the host answers, it goes on from there and ends.
    host(&paths, "attic", server.port);
    let second = contact::rotate(&paths, "berth", &mut out).unwrap();
    assert_eq!(second.done, ["attic"]);
    assert!(second.finished, "{}", String::from_utf8_lossy(&out));
    let rotated = ring::public(&paths, &store, "berth").unwrap();
    assert_eq!(rotated.public, next.public);
    assert!(rotated.pending.is_none());
    let file = ring::file(&paths, &store, "berth", false).unwrap().unwrap();
    assert!(!paths.data_dir.join("keys/berth.pending").exists());
    assert!(
        fs::read_to_string(ring::public_path(&file))
            .unwrap()
            .contains(&ring::blob(&next.public))
    );
    contact::works(&keys::login_of(&paths, &studio).unwrap()).unwrap();
    drop(attic);
}
