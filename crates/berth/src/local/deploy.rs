//! The berth server's image built from this berth: for a node's
//! `upgrade-server source=here`, with this machine's engine
//! (`berth server image`), or on any machine with Docker over SSH
//! (`berth server deploy`), which then runs the server from it.

use crate::cli::args;
use crate::cli::fail::{Fail, Result as Done};
use crate::cli::out;
use berth_core::paths::Paths;
use berth_core::server_image::{self, Context, Cpu};
use std::io::Write;
use std::process::{Command, Stdio};

/// The helper that runs a server's container from another image, which a
/// node has installed and `deploy` sends.
const HELPER: &str =
    include_str!("../../../../provision/ansible/roles/devbox/files/berth-server-upgrade.py");

/// The port a server made by `deploy` listens on, on that machine only.
const PORT: u16 = 7777;

fn context(cpu: Cpu) -> Done<Context> {
    let source = Paths::locate()?.source()?;
    Context::of_this_berth(&source, cpu).map_err(Fail::refused)
}

fn cpu_named(machine: &str, host: &str) -> Done<Cpu> {
    Cpu::named(machine).ok_or_else(|| {
        Fail::refused(format!(
            "{host} has the CPU {machine}, and berth is released for x86_64 and aarch64 alone"
        ))
    })
}

/// The build context of this berth for a host whose `uname -m` is `machine`,
/// and the SHA-256 of the program in it.
pub fn server_context(machine: &str) -> Done<(Vec<u8>, String)> {
    let cpu = cpu_named(machine.trim(), "The host")?;
    let context = context(cpu)?;
    Ok((context.bytes, context.sha256))
}

/// Run `command` with `input` on its standard input, its output to ours.
fn fed(mut command: Command, input: &[u8], what: &str) -> Done<()> {
    let mut child = command
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|error| Fail::failed(format!("{what}: {error}")))?;
    let mut stdin = child.stdin.take().expect("piped");
    let written = stdin.write_all(input);
    drop(stdin);
    let status = child.wait()?;
    match (status.success(), written) {
        (true, Ok(())) => Ok(()),
        (true, Err(error)) => Err(Fail::failed(format!("{what}: {error}"))),
        (false, _) => Err(Fail::exit(status.code().unwrap_or(1))),
    }
}

/// `berth server image`: the image of this berth, built by this machine's engine.
pub fn image(tag: Option<String>) -> Done<()> {
    let cpu = Cpu::here().ok_or_else(|| {
        Fail::refused(
            "berth is released for x86_64 and aarch64 alone, and this machine has neither",
        )
    })?;
    let context = context(cpu)?;
    let tag = tag.unwrap_or_else(server_image::tag);
    let mut build = Command::new("docker");
    build
        .args(["build", "--tag", &tag])
        .args(context.build_args())
        .arg("-");
    fed(build, &context.bytes, "docker build")?;
    out::done(format!(
        "Built {tag} of berth {} ({}) with this machine's engine",
        berth_api::VERSION,
        &context.sha256[..12]
    ));
    Ok(())
}

/// A word for a POSIX shell, quoted when it needs to be.
fn quoted(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "-_.,:/=@%+".contains(ch));
    match plain {
        true => word.to_owned(),
        false => format!("'{}'", word.replace('\'', r"'\''")),
    }
}

fn line(words: &[String]) -> String {
    words
        .iter()
        .map(|word| quoted(word))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The machine at `target`, reached with the user's own ssh and its settings.
struct Remote {
    target: String,
}

impl Remote {
    fn command(&self, words: &[String]) -> Command {
        let mut ssh = Command::new("ssh");
        // The target is never read as an option of ssh.
        ssh.arg("--").arg(&self.target).arg(line(words));
        ssh
    }

    /// What a command there writes, or why it failed.
    fn output(&self, words: &[String]) -> Done<std::result::Result<String, String>> {
        let done = self
            .command(words)
            .stdin(Stdio::null())
            .output()
            .map_err(|error| Fail::failed(format!("ssh: {error}")))?;
        match (done.status.code(), done.status.success()) {
            // ssh itself: the machine was not reached.
            (Some(255), _) => Err(Fail::unreachable(format!(
                "ssh to {} failed: {}",
                self.target,
                String::from_utf8_lossy(&done.stderr).trim()
            ))),
            (_, true) => Ok(Ok(String::from_utf8_lossy(&done.stdout).trim().to_owned())),
            (_, false) => Ok(Err(String::from_utf8_lossy(&done.stderr).trim().to_owned())),
        }
    }

    fn run(&self, words: &[String], input: &[u8], what: &str) -> Done<()> {
        fed(self.command(words), input, what)
    }
}

fn words(list: &[&str]) -> Vec<String> {
    list.iter().map(|word| (*word).to_owned()).collect()
}

/// The helper, run there with these arguments: it is sent on standard input.
fn helper(arguments: &[String]) -> Vec<String> {
    let mut command = words(&["python3", "-"]);
    command.extend(arguments.iter().cloned());
    command
}

/// `berth server deploy`: the server on a machine with Docker, run from this
/// berth's image built there, or from a release with `--release`.
pub fn deploy(deploy: &args::Deploy) -> Done<()> {
    if deploy.target.starts_with('-') || deploy.target.is_empty() {
        return Err(Fail::usage("berth server deploy takes [USER@]HOST"));
    }
    let remote = Remote {
        target: deploy.target.clone(),
    };
    let name = &deploy.container;
    let found = remote.output(&words(&[
        "docker",
        "container",
        "inspect",
        "--format",
        "{{.Config.Image}}",
        name,
    ]))?;
    let image = match &deploy.release {
        Some(version) => format!("{}:{version}", server_image::BASELINE),
        None => server_image::tag(),
    };
    if deploy.release.is_none() {
        let machine = remote
            .output(&words(&["uname", "-m"]))?
            .map_err(|error| Fail::failed(format!("uname -m on {}: {error}", deploy.target)))?;
        let context = context(cpu_named(&machine, &deploy.target)?)?;
        let held = remote.output(&words(&[
            "docker",
            "image",
            "inspect",
            "--format",
            &format!(
                "{{{{index .Config.Labels \"{}\"}}}}",
                server_image::label::BINARY
            ),
            &image,
        ]))?;
        if held.as_deref() == Ok(context.sha256.as_str()) {
            out::note(format!(
                "{image} on {} holds this berth already",
                deploy.target
            ));
        } else {
            out::note(format!(
                "Building {image} on {} from this berth ({}, {:.1} MB)",
                deploy.target,
                &context.sha256[..12],
                context.bytes.len() as f64 / 1e6
            ));
            let mut build = words(&["docker", "build", "--tag", &image]);
            build.extend(context.build_args());
            build.push("-".into());
            remote.run(&build, &context.bytes, "docker build")?;
        }
    }
    let publish = |port: u16| format!("127.0.0.1:{port}:7777");
    match found {
        // A server runs there: the helper makes its container again from the image.
        Ok(_) if deploy.release.is_some() && deploy.port.is_some() => {
            return Err(Fail::usage(format!(
                "{name} runs on {} already: --release keeps its port, and --port goes with an image built from this berth",
                deploy.target
            )));
        }
        Ok(_) => {
            let mut arguments = match &deploy.release {
                Some(version) => vec![name.clone(), version.clone()],
                None => vec![name.clone(), "--image".into(), image.clone()],
            };
            if let Some(port) = deploy.port {
                arguments.extend(["--publish".into(), publish(port)]);
            }
            remote.run(&helper(&arguments), HELPER.as_bytes(), "the upgrade helper")?;
        }
        Err(_) => {
            let port = deploy.port.unwrap_or(PORT);
            let mut run = words(&[
                "docker",
                "run",
                "--detach",
                "--name",
                name,
                "--restart",
                "unless-stopped",
            ]);
            run.extend(["--publish".into(), publish(port)]);
            for (volume, path) in [("config", "/config"), ("data", "/data")] {
                run.extend(["--volume".into(), format!("{name}-{volume}:{path}")]);
            }
            if deploy.release.is_some() {
                run.extend(["--pull".into(), "always".into()]);
            }
            run.push(image.clone());
            remote.run(&run, &[], "docker run")?;
            out::done(format!(
                "{name} runs {image} on {}, at 127.0.0.1:{port} there, with its data in the volumes {name}-config and {name}-data",
                deploy.target
            ));
            out::hint(format!(
                "A setup key for berth login: ssh {} docker exec {name} berth server setup-key",
                deploy.target
            ));
            return Ok(());
        }
    }
    out::done(format!("{name} on {} runs {image}", deploy.target));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_for_the_remote_shell_are_quoted() {
        assert_eq!(quoted("berth:0.6.0"), "berth:0.6.0");
        assert_eq!(quoted("{{.Config.Image}}"), "'{{.Config.Image}}'");
        assert_eq!(quoted("it's"), r"'it'\''s'");
        assert_eq!(quoted(""), "''");
        let line = line(&words(&[
            "docker",
            "build",
            "--build-arg",
            "BERTH_VERSION=0.6.0",
            "-",
        ]));
        assert_eq!(line, "docker build --build-arg BERTH_VERSION=0.6.0 -");
    }

    #[test]
    fn the_helper_it_sends_is_the_nodes() {
        assert!(HELPER.contains("def build(name, version, context)"));
        assert_eq!(
            helper(&words(&["berth", "--image", "berth:0.6.0"])),
            words(&["python3", "-", "berth", "--image", "berth:0.6.0"])
        );
    }
}
