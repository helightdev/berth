//! Packs the provisioning source into the program, so an installed berth
//! carries the playbook, scripts and plugins it was built with.
//!
//! berthd and box run on the hosts, so they are built here first as static
//! programs that need nothing from the system they land on, and put among the
//! role's files. That makes `cargo build -p berth` the whole build. It needs
//! the musl target once: `rustup target add x86_64-unknown-linux-musl`.
//!
//! A release build of berth carries release builds of them. Any other build
//! carries quicker ones from the `host-dev` profile, which do for development
//! and the tests.

use std::{env, fs, path::Path, process::Command};

const HOST_TARGET: &str = "x86_64-unknown-linux-musl";

/// A cargo that knows rustup's targets: a system cargo found first on the path does not.
fn cargo() -> Command {
    let rustup = |tool: &str| {
        let output = Command::new("rustup").args(["which", tool]).output().ok()?;
        let path = String::from_utf8(output.stdout).ok()?.trim().to_owned();
        (output.status.success() && !path.is_empty()).then_some(path)
    };
    match (rustup("cargo"), rustup("rustc")) {
        (Some(cargo), Some(rustc)) => {
            let mut command = Command::new(cargo);
            command.env("RUSTC", rustc);
            command
        }
        _ => Command::new(env::var("CARGO").unwrap_or_else(|_| "cargo".into())),
    }
}

fn host_programs(root: &Path) {
    for entry in [
        "crates/berthd",
        "crates/box",
        "crates/berth-api",
        "crates/berth-pty",
        "Cargo.lock",
    ] {
        println!("cargo:rerun-if-changed=../../{entry}");
    }
    // The version they say they are, which a release sets.
    println!("cargo:rerun-if-env-changed=BERTH_BUILD_VERSION");
    let profile = match env::var("PROFILE").as_deref() {
        Ok("release") => "release",
        _ => "host-dev",
    };
    // Its own target directory: the outer build holds the lock on the usual one.
    let target = root.join("target/host");
    let mut build = cargo();
    build
        .current_dir(root)
        .args(["build", "--locked", "--target", HOST_TARGET])
        .args(["--profile", profile])
        .args(["-p", "berthd", "-p", "box", "--target-dir"])
        .arg(&target);
    // Flags and wrappers of the outer build (clippy's among them) are for another target.
    for variable in [
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTFLAGS",
        "CARGO_BUILD_TARGET",
        "TARGET",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
    ] {
        build.env_remove(variable);
    }
    let status = build
        .status()
        .expect("cargo is needed to build berthd and box");
    assert!(
        status.success(),
        "building berthd and box failed; the target is added with: rustup target add {HOST_TARGET}"
    );
    let bin = root.join("provision/ansible/roles/devbox/files/bin");
    fs::create_dir_all(&bin).expect("the role's bin directory");
    for name in ["berthd", "box"] {
        let built = target.join(HOST_TARGET).join(profile).join(name);
        let placed = bin.join(name);
        // Left alone when unchanged, so the archive is not packed again for nothing.
        if fs::read(&built).ok() != fs::read(&placed).ok() {
            // The copy keeps the time of the file it replaces. This script watches
            // provision/ansible/, and a file written while it runs would run it again on the
            // next build for nothing.
            let time = fs::metadata(&placed).and_then(|meta| meta.modified()).ok();
            fs::copy(&built, &placed).expect("copying a host program");
            if let Some(time) = time {
                fs::File::options()
                    .write(true)
                    .open(&placed)
                    .and_then(|file| file.set_modified(time))
                    .expect("the time of a host program");
            }
        }
    }
}

fn main() {
    let out = env::var("OUT_DIR").expect("OUT_DIR");
    let manifest = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let root = Path::new(&manifest)
        .ancestors()
        .nth(2)
        .expect("the checkout");
    host_programs(root);
    // What provision/bin/devbox.py puts in a release.
    for entry in [
        "provision/ansible",
        "provision/bin",
        "provision/bootstrap",
        "provision/requirements.txt",
        "provision/ansible.cfg",
        "provision/config/example.json",
        "container",
        "plugins",
        "tests",
        "README.md",
        "LICENSE-MIT",
        "LICENSE-APACHE",
    ] {
        println!("cargo:rerun-if-changed=../../{entry}");
    }
    let status = Command::new("python3")
        .arg(root.join("provision/bin/devbox.py"))
        .arg("release")
        .env("DEVBOX_BUILD_DIR", &out)
        .env_remove("BERTH_PLUGINS")
        .status()
        .expect("python3 is needed to pack the provisioning source");
    assert!(status.success(), "provision/bin/devbox.py release failed");
}
