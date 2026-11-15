//! `berth serve --simulate`: the real server on a scratch store, whose hosts
//! and provisioners are those of [`berth_core::simulate`].

use crate::Result;
use crate::cli::args::ServerRun;
use berth_core::{
    paths::{Paths, layout},
    simulate::{self, Seed, Simulation},
};
use std::path::{Path, PathBuf};

/// Set to 1, it makes `berth serve` a simulated server, as for a container
/// whose command cannot be changed, such as a service of GitHub Actions.
pub const ASKED: &str = "BERTH_SIMULATE";
/// The seed itself, as JSON, when no file is given.
pub const SEED: &str = "BERTH_SIMULATE_SEED";

/// Whether the environment asks for a simulated server.
pub fn asked() -> bool {
    std::env::var(ASKED).is_ok_and(|value| value.trim() == "1")
}

/// The directories the simulated server keeps its state in, under `root`.
/// They are set in the environment, so that its provisioner and anything
/// else it starts find them, and never the user's own.
fn take_over(root: &Path) -> Result<Paths> {
    let run = root.join("run");
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&run)?;
    }
    // SAFETY: berth starts no thread before a server command.
    unsafe {
        std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
        std::env::set_var("XDG_DATA_HOME", root.join("data"));
        std::env::set_var("XDG_RUNTIME_DIR", &run);
        std::env::remove_var("BERTH_SERVER");
    }
    Paths::locate()
}

/// Where the simulation runs: the directory given, or a new scratch one.
fn root(given: Option<&Path>) -> Result<(PathBuf, Option<tempfile::TempDir>)> {
    let own = Paths::locate().ok();
    let (root, scratch) = match given {
        Some(dir) => (std::path::absolute(dir)?, None),
        None => {
            let scratch = tempfile::Builder::new()
                .prefix("berth-simulated-")
                .tempdir()?;
            (scratch.path().to_owned(), Some(scratch))
        }
    };
    simulate::check_dir(&root, own.as_ref())?;
    std::fs::create_dir_all(&root)?;
    std::fs::write(root.join(simulate::MARKER), "")?;
    Ok((root, scratch))
}

pub fn command(run: &ServerRun) -> Result<()> {
    let listen = run.listen.as_deref().map(super::parse_listen).transpose()?;
    let seed = match (&run.seed, std::env::var(SEED)) {
        (Some(file), _) => Seed::read(file)?,
        (None, Ok(text)) if !text.trim().is_empty() => {
            Seed::parse(&text).map_err(|error| format!("{SEED}: {error}"))?
        }
        (None, _) => Seed::default(),
    };
    if let Some(delay) = run.delay {
        if !delay.is_finite() || delay < 0.0 {
            return Err("--delay is a number of seconds, 0 or more".into());
        }
        // SAFETY: berth starts no thread before a server command.
        unsafe { std::env::set_var(simulate::DELAY, delay.to_string()) };
    }
    let (root, scratch) = root(run.data.as_deref())?;
    let paths = take_over(&root)?;
    paths.prepare()?;
    let berthd = paths.source()?.join(layout::BERTHD);
    let simulation = Simulation::begin(&paths, &berthd)?;
    simulate::write_plugin(&paths, &std::env::current_exe()?)?;
    let planted = seed.plant(&paths, simulation);
    println!(
        "Simulated berth server: its hosts and provisioners reach no machine. State in {}{}",
        root.display(),
        match (&scratch, run.keep) {
            (Some(_), false) => ", removed when it ends",
            _ => "",
        }
    );
    let served = planted.and_then(|()| super::run(&paths, listen, true));
    simulation.stop();
    if let Some(scratch) = scratch
        && run.keep
    {
        let _ = scratch.keep();
    }
    served
}
