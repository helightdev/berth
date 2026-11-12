//! With the `web` feature, builds the page `berth serve --web` shows in a
//! browser: berth-web for wasm32, and the JavaScript that loads it, into
//! OUT_DIR for the program to carry. It needs the target once:
//! `rustup target add wasm32-unknown-unknown`.

#[cfg(not(feature = "web"))]
fn main() {}

#[cfg(feature = "web")]
fn main() {
    page::build();
}

#[cfg(feature = "web")]
mod page {

    use std::{
        env, fs,
        hash::{DefaultHasher, Hash, Hasher},
        path::Path,
        process::Command,
    };

    const TARGET: &str = "wasm32-unknown-unknown";

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

    pub fn build() {
        let out = env::var("OUT_DIR").expect("OUT_DIR");
        let manifest = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
        let root = Path::new(&manifest).parent().expect("the checkout");
        for entry in [
            "web",
            "crates/berth-tui",
            "crates/berth-api",
            "cli/vendor/tui-input",
            "Cargo.lock",
        ] {
            println!("cargo:rerun-if-changed=../{entry}");
        }
        // Its own target directory: the outer build holds the lock on the usual one.
        let target = root.join("target/web");
        let mut build = cargo();
        build
            .current_dir(root)
            .args(["build", "--profile", "web", "--locked", "--target", TARGET])
            .args(["-p", "berth-web", "--target-dir"])
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
        let status = build.status().expect("cargo is needed to build the page");
        assert!(
            status.success(),
            "building the page failed; the target is added with: rustup target add {TARGET}"
        );
        let wasm = target.join(TARGET).join("web/berth-web.wasm");
        let page = Path::new(&out).join("web");
        let _ = fs::remove_dir_all(&page);
        wasm_bindgen_cli_support::Bindgen::new()
            .input_path(&wasm)
            .web(true)
            .expect("a page for the web")
            .typescript(false)
            .generate(&page)
            .expect("the JavaScript that loads the page");
        // A copy at a place that does not move, for the documentation site.
        let dist = root.join("target/web-dist");
        fs::create_dir_all(&dist).expect("target/web-dist");
        for name in ["berth-web.js", "berth-web_bg.wasm"] {
            fs::copy(page.join(name), dist.join(name)).expect("copying the page");
        }
        // Named in the page's addresses, so a browser never runs the script of one
        // build with the wasm of another.
        let mut hasher = DefaultHasher::new();
        fs::read(page.join("berth-web_bg.wasm"))
            .expect("the page's wasm")
            .hash(&mut hasher);
        println!("cargo:rustc-env=BERTH_WEB_BUILD={:016x}", hasher.finish());
    }
}
