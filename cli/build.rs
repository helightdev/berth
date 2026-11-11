//! Packs the provisioning source into the program, so an installed berth
//! carries the playbook, scripts and plugins it was built with.

use std::{env, path::Path, process::Command};

fn main() {
    let out = env::var("OUT_DIR").expect("OUT_DIR");
    let manifest = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let root = Path::new(&manifest).parent().expect("the checkout");
    // What bin/devbox.py puts in a release.
    for entry in [
        "ansible", "bin", "bootstrap", "container", "plugins", "tests",
        "requirements.txt", "ansible.cfg", "README.md", "LICENSE", "Makefile",
        "config/example.json",
    ] {
        println!("cargo:rerun-if-changed=../{entry}");
    }
    let status = Command::new("python3")
        .arg(root.join("bin/devbox.py"))
        .arg("release")
        .env("DEVBOX_BUILD_DIR", &out)
        .env_remove("BERTH_PLUGINS")
        .status()
        .expect("python3 is needed to pack the provisioning source");
    assert!(status.success(), "bin/devbox.py release failed");
}
