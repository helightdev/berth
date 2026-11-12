//! Accounts, and commands that run as one.

use crate::error::{E, R};
use crate::state::{Mode, State};
use berth_api::CommandLine;
use std::{collections::BTreeMap, ffi::CString, os::unix::process::CommandExt, process::Command};

#[derive(Debug, Clone)]
pub struct Account {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: String,
    pub shell: String,
}

/// An account from /etc/passwd, by name or by number.
pub fn account(wanted: &str) -> Option<Account> {
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    passwd.lines().find_map(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() < 7 || (fields[0] != wanted && fields[2] != wanted) {
            return None;
        }
        Some(Account {
            name: fields[0].into(),
            uid: fields[2].parse().ok()?,
            gid: fields[3].parse().ok()?,
            home: fields[5].into(),
            shell: fields[6].into(),
        })
    })
}

pub fn own_uid() -> u32 {
    // SAFETY: getuid cannot fail.
    unsafe { libc::getuid() }
}

/// Who `agent`, `operator` or `root` is on this host. `None` means: whoever
/// runs berthd, which is the only answer in single-user mode.
pub fn resolve(state: &State, user: &str) -> R<Option<Account>> {
    let config = state.config();
    if state.mode == Mode::Single {
        return match user {
            "agent" => Ok(None),
            other => Err(E::unavailable(format!(
                "In single-user mode everything runs as the user berthd runs as; it cannot run as {other}"
            ))),
        };
    }
    let name = match user {
        "agent" => config.berthd.agent.clone(),
        "operator" if !config.berthd.operator.is_empty() => config.berthd.operator.clone(),
        "operator" => return Err(E::bad("This host has no SSH user configured")),
        "root" => "root".into(),
        other => {
            return Err(E::bad(format!(
                "user is agent, operator or root, not {other}"
            )));
        }
    };
    account(&name)
        .map(Some)
        .ok_or_else(|| E::internal(format!("No account {name} on this host")))
}

/// Make `command` run as `account`, with that account's plain environment.
pub fn become_user(command: &mut Command, account: &Account) {
    command.env_clear();
    for (key, value) in [
        ("HOME", account.home.as_str()),
        ("USER", &account.name),
        ("LOGNAME", &account.name),
        ("SHELL", &account.shell),
        (
            "PATH",
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        ),
    ] {
        command.env(key, value);
    }
    command.env(
        "LANG",
        std::env::var("LANG").unwrap_or_else(|_| "C.UTF-8".into()),
    );
    command.current_dir(&account.home);
    if account.uid == own_uid() {
        return;
    }
    let (name, uid, gid) = (
        CString::new(account.name.clone()).unwrap_or_default(),
        account.uid,
        account.gid,
    );
    // SAFETY: only async-signal-safe calls between fork and exec (initgroups
    // reads /etc/group, which is accepted practice in a single-purpose child).
    unsafe {
        command.pre_exec(move || {
            if libc::initgroups(name.as_ptr(), gid) != 0
                || libc::setgid(gid) != 0
                || libc::setuid(uid) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// The command for a session step: `line` as `user`, in `cwd`, with `env` added.
pub fn command(
    state: &State,
    user: &str,
    line: &CommandLine,
    cwd: Option<&str>,
    env: &BTreeMap<String, String>,
) -> R<Command> {
    let account = resolve(state, user)?;
    let mut words: Vec<String> = match line {
        CommandLine::Args(args) if args.is_empty() => return Err(E::bad("command is empty")),
        CommandLine::Args(args) => args.clone(),
        CommandLine::Shell(script) => {
            let shell = if std::path::Path::new("/bin/bash").exists() {
                "bash"
            } else {
                "sh"
            };
            vec![shell.into(), "-c".into(), script.clone()]
        }
    };
    // The agent gets its display and tool paths from the wrapper the role installs.
    let wrapper = state.config().berthd.agent_env.clone();
    if user == "agent" && account.is_some() && std::path::Path::new(&wrapper).exists() {
        words.insert(0, wrapper);
    }
    let mut command = Command::new(&words[0]);
    command.args(&words[1..]);
    if let Some(account) = &account {
        become_user(&mut command, account);
    }
    if let Some(cwd) = cwd.filter(|cwd| !cwd.is_empty()) {
        command.current_dir(cwd);
    }
    command.envs(env);
    Ok(command)
}

/// Run a short command as `user` and give back its exit code and output.
pub async fn capture(
    state: &State,
    user: &str,
    line: &CommandLine,
    seconds: u64,
    limit: usize,
) -> R<(i32, Vec<u8>, String)> {
    let mut command =
        tokio::process::Command::from(command(state, user, line, None, &BTreeMap::new())?);
    command
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let run = tokio::time::timeout(std::time::Duration::from_secs(seconds), command.output());
    let output = run
        .await
        .map_err(|_| {
            E(
                berth_api::code::TIMEOUT,
                format!("No answer within {seconds} seconds"),
            )
        })?
        .map_err(|error| E::internal(format!("Could not run it: {error}")))?;
    let mut stdout = output.stdout;
    stdout.truncate(limit);
    Ok((
        output.status.code().unwrap_or(1),
        stdout,
        String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    ))
}
