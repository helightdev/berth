//! Direct calls: one invocation of a tool the system already has, or a read
//! of /proc. berthd adds no logic of its own and models no config format.

use crate::error::{E, R};
use crate::state::State;
use axum::{
    Json,
    extract::{Path, Query, State as AppState, ws},
    http::header,
    response::{IntoResponse, Response},
};
use berth_api::{self as api, CommandLine, code};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tokio::io::{AsyncBufReadExt, BufReader};

fn done() -> Json<api::Done> {
    Json(api::Done { ok: true })
}

fn read(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Run a system tool and give back its output, or its complaint.
async fn tool(program: &str, args: &[&str]) -> R<String> {
    let output = tokio::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(Duration::from_secs(60), output)
        .await
        .map_err(|_| {
            E(
                code::TIMEOUT,
                format!("{program} did not finish within a minute"),
            )
        })?
        .map_err(|error| E::unavailable(format!("{program}: {error}")))?;
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(E::internal(if said.is_empty() {
            format!("{program} failed ({})", output.status)
        } else {
            said
        }));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn systemd(state: &State) -> R<()> {
    match state.systemd() {
        true => Ok(()),
        false => Err(E::unavailable(
            "There is no systemd to ask in single-user mode",
        )),
    }
}

/// Busy and total ticks per line of /proc/stat: all cores first, then each.
fn ticks() -> Vec<(u64, u64)> {
    read("/proc/stat")
        .lines()
        .filter(|line| line.starts_with("cpu"))
        .map(|line| {
            let values: Vec<u64> = line
                .split_whitespace()
                .skip(1)
                .filter_map(|value| value.parse().ok())
                .collect();
            let total: u64 = values.iter().take(8).sum();
            let idle = values.get(3).copied().unwrap_or(0) + values.get(4).copied().unwrap_or(0);
            (total - idle, total)
        })
        .collect()
}

fn meminfo() -> BTreeMap<String, u64> {
    read("/proc/meminfo")
        .lines()
        .filter_map(|line| {
            let (key, rest) = line.split_once(':')?;
            Some((
                key.to_owned(),
                rest.split_whitespace().next()?.parse::<u64>().ok()? * 1024,
            ))
        })
        .collect()
}

fn filesystems() -> Vec<api::Filesystem> {
    const REAL: &[&str] = &[
        "ext4", "ext3", "ext2", "xfs", "btrfs", "zfs", "vfat", "exfat", "f2fs", "ntfs", "ntfs3",
        "overlay", "fuseblk",
    ];
    let mut seen = vec![];
    let mut found = vec![];
    for line in read("/proc/mounts").lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 3
            || !REAL.contains(&fields[2])
            || seen.contains(&fields[0])
            || fields[1].starts_with("/var/lib/docker")
        {
            continue;
        }
        let Ok(path) = std::ffi::CString::new(fields[1].replace("\\040", " ")) else {
            continue;
        };
        // SAFETY: statvfs fills the structure it is given.
        let stat = unsafe {
            let mut stat: libc::statvfs = std::mem::zeroed();
            if libc::statvfs(path.as_ptr(), &mut stat) != 0 {
                continue;
            }
            stat
        };
        let block = stat.f_frsize as u64;
        seen.push(fields[0]);
        found.push(api::Filesystem {
            mount: fields[1].into(),
            device: fields[0].into(),
            kind: fields[2].into(),
            total: stat.f_blocks as u64 * block,
            used: (stat.f_blocks as u64 - stat.f_bfree as u64) * block,
            available: stat.f_bavail as u64 * block,
        });
    }
    found
}

async fn gpus() -> Vec<api::Gpu> {
    let query = [
        "--query-gpu=name,utilization.gpu,memory.used,memory.total",
        "--format=csv,noheader,nounits",
    ];
    let Ok(output) = tool("nvidia-smi", &query).await else {
        return vec![];
    };
    output
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(',').map(str::trim).collect();
            Some(api::Gpu {
                name: fields.first()?.to_string(),
                utilization: fields.get(1)?.parse().ok()?,
                memory_used: fields.get(2)?.parse::<u64>().ok()? << 20,
                memory_total: fields.get(3)?.parse::<u64>().ok()? << 20,
            })
        })
        .collect()
}

pub async fn stats(AppState(state): AppState<Arc<State>>) -> Json<api::Stats> {
    let before = ticks();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let busy: Vec<f64> = ticks()
        .iter()
        .zip(&before)
        .map(|(now, then)| match now.1.saturating_sub(then.1) {
            0 => 0.0,
            total => (now.0.saturating_sub(then.0) as f64 / total as f64 * 1000.0).round() / 10.0,
        })
        .collect();
    let memory = meminfo();
    let value = |key: &str| memory.get(key).copied().unwrap_or(0);
    let load: Vec<f64> = read("/proc/loadavg")
        .split_whitespace()
        .take(3)
        .filter_map(|value| value.parse().ok())
        .collect();
    let config = state.config();
    let mut units = vec![];
    if state.systemd() && !config.berthd.units.is_empty() {
        let names: Vec<&str> = config.berthd.units.iter().map(String::as_str).collect();
        let output = tokio::process::Command::new("systemctl")
            .arg("is-active")
            .args(&names)
            .output()
            .await;
        let text = output
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
            .unwrap_or_default();
        let mut states = text.lines();
        for unit in names {
            units.push(api::UnitState {
                unit: unit.into(),
                active: states.next().unwrap_or("unknown").into(),
            });
        }
    }
    Json(api::Stats {
        hostname: read("/proc/sys/kernel/hostname").trim().into(),
        load: [
            load.first().copied().unwrap_or(0.0),
            load.get(1).copied().unwrap_or(0.0),
            load.get(2).copied().unwrap_or(0.0),
        ],
        cpu: busy.first().copied().unwrap_or(0.0),
        cores: busy.iter().skip(1).copied().collect(),
        memory: api::Memory {
            total: value("MemTotal"),
            available: value("MemAvailable"),
            used: value("MemTotal").saturating_sub(value("MemAvailable")),
        },
        swap: api::Memory {
            total: value("SwapTotal"),
            available: value("SwapFree"),
            used: value("SwapTotal").saturating_sub(value("SwapFree")),
        },
        filesystems: filesystems(),
        network: read("/proc/net/dev")
            .lines()
            .skip(2)
            .filter_map(|line| {
                let (name, rest) = line.split_once(':')?;
                let values: Vec<u64> = rest
                    .split_whitespace()
                    .filter_map(|value| value.parse().ok())
                    .collect();
                if name.trim() == "lo" || values.len() < 9 {
                    return None;
                }
                Some(api::Interface {
                    name: name.trim().into(),
                    rx_bytes: values[0],
                    tx_bytes: values[8],
                })
            })
            .collect(),
        uptime: read("/proc/uptime")
            .split('.')
            .next()
            .and_then(|seconds| seconds.parse().ok())
            .unwrap_or(0),
        gpus: gpus().await,
        reboot_required: std::path::Path::new("/var/run/reboot-required").exists(),
        units,
        sessions_running: state.sessions.running() as u32,
    })
}

/// The shared desktop as PNG, taken by `box` as the agent.
pub async fn screenshot(AppState(state): AppState<Arc<State>>) -> R<Response> {
    let program = ["/usr/local/bin/box", "box"]
        .into_iter()
        .find(|path| !path.contains('/') || std::path::Path::new(path).exists());
    let line = CommandLine::Args(
        [
            program.unwrap_or("box"),
            "desktop",
            "screenshot",
            "--output",
            "-",
        ]
        .map(String::from)
        .to_vec(),
    );
    let (code, image, errors) = crate::users::capture(&state, "agent", &line, 30, 64 << 20).await?;
    if code != 0 || image.is_empty() {
        return Err(E::unavailable(format!(
            "No screenshot: {}",
            if errors.is_empty() {
                "box gave none"
            } else {
                &errors
            }
        )));
    }
    Ok(([(header::CONTENT_TYPE, "image/png")], image).into_response())
}

fn valid_unit(unit: &str) -> R<()> {
    let fine = !unit.is_empty()
        && !unit.starts_with('-')
        && unit
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "@._:\\-".contains(ch));
    fine.then_some(()).ok_or_else(|| E::bad("Not a unit name"))
}

pub async fn units(AppState(state): AppState<Arc<State>>) -> R<Json<Vec<api::Unit>>> {
    systemd(&state)?;
    let output = tool(
        "systemctl",
        &["list-units", "--all", "--no-pager", "--output=json"],
    )
    .await?;
    let rows: Vec<BTreeMap<String, serde_json::Value>> =
        serde_json::from_str(&output).map_err(E::internal)?;
    let text = |row: &BTreeMap<String, serde_json::Value>, key: &str| {
        row.get(key)
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_owned()
    };
    Ok(Json(
        rows.iter()
            .map(|row| api::Unit {
                unit: text(row, "unit"),
                load: text(row, "load"),
                active: text(row, "active"),
                sub: text(row, "sub"),
                description: text(row, "description"),
            })
            .collect(),
    ))
}

pub async fn unit(
    AppState(state): AppState<Arc<State>>,
    Path(unit): Path<String>,
) -> R<Json<BTreeMap<String, String>>> {
    systemd(&state)?;
    valid_unit(&unit)?;
    let output = tool("systemctl", &["show", "--no-pager", "--", &unit]).await?;
    Ok(Json(
        output
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect(),
    ))
}

pub async fn unit_verb(
    AppState(state): AppState<Arc<State>>,
    Path((unit, verb)): Path<(String, String)>,
) -> R<Json<api::Done>> {
    systemd(&state)?;
    valid_unit(&unit)?;
    if !["start", "stop", "restart", "reload", "enable", "disable"].contains(&verb.as_str()) {
        return Err(E::bad(
            "The verb is start, stop, restart, reload, enable or disable",
        ));
    }
    tool("systemctl", &[&verb, "--", &unit]).await?;
    Ok(done())
}

fn entry(line: &str) -> Option<api::LogEntry> {
    let row: serde_json::Value = serde_json::from_str(line).ok()?;
    let text = |key: &str| match &row[key] {
        serde_json::Value::String(text) => text.clone(),
        // journalctl gives what is not UTF-8 as a list of bytes.
        serde_json::Value::Array(bytes) => {
            let bytes: Vec<u8> = bytes
                .iter()
                .filter_map(|byte| byte.as_u64().map(|byte| byte as u8))
                .collect();
            String::from_utf8_lossy(&bytes).into_owned()
        }
        _ => String::new(),
    };
    let micros: u64 = text("__REALTIME_TIMESTAMP").parse().ok()?;
    let unit = [text("_SYSTEMD_UNIT"), text("SYSLOG_IDENTIFIER")]
        .into_iter()
        .find(|name| !name.is_empty())
        .unwrap_or_default();
    Some(api::LogEntry {
        time: api::rfc3339(micros / 1_000_000),
        unit,
        priority: text("PRIORITY").parse().unwrap_or(6),
        message: text("MESSAGE"),
        cursor: text("__CURSOR"),
    })
}

#[derive(serde::Deserialize)]
pub struct LogQuery {
    unit: Option<String>,
    since: Option<String>,
    limit: Option<usize>,
}

fn journal(query: &LogQuery, follow: bool) -> R<Vec<String>> {
    let mut args = vec!["-o".to_owned(), "json".into(), "--no-pager".into()];
    if let Some(unit) = &query.unit {
        valid_unit(unit)?;
        args.extend(["-u".into(), unit.clone()]);
    }
    match &query.since {
        Some(cursor) if cursor.starts_with("s=") => args.push(format!("--after-cursor={cursor}")),
        Some(time) => args.push(format!("--since={time}")),
        None => {}
    }
    let limit = query.limit.unwrap_or(100).clamp(1, 1000);
    if follow {
        args.push("-f".into());
        if query.since.is_none() {
            args.extend(["-n".into(), limit.to_string()]);
        }
    } else if query.since.is_some() {
        // From a point on: the first `limit` entries after it.
        args.extend(["-n".into(), format!("+{limit}")]);
    } else {
        args.extend(["-n".into(), limit.to_string()]);
    }
    Ok(args)
}

/// Journal entries, or with a WebSocket handshake those and then what is logged.
pub async fn logs(
    AppState(state): AppState<Arc<State>>,
    Query(query): Query<LogQuery>,
    upgrade: Result<ws::WebSocketUpgrade, ws::rejection::WebSocketUpgradeRejection>,
) -> R<Response> {
    systemd(&state)?;
    let Ok(upgrade) = upgrade else {
        let args = journal(&query, false)?;
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let entries: Vec<api::LogEntry> = tool("journalctl", &args)
            .await?
            .lines()
            .filter_map(entry)
            .collect();
        return Ok(Json(entries).into_response());
    };
    let args = journal(&query, true)?;
    let mut child = tokio::process::Command::new("journalctl")
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| E::unavailable(format!("journalctl: {error}")))?;
    Ok(upgrade.on_upgrade(move |mut socket| async move {
        let mut lines = BufReader::new(child.stdout.take().expect("piped")).lines();
        loop {
            tokio::select! {
                line = lines.next_line() => {
                    let Ok(Some(line)) = line else { break };
                    let Some(entry) = entry(&line) else { continue };
                    let text = serde_json::to_string(&entry).unwrap_or_default();
                    if socket.send(ws::Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                frame = socket.recv() => if !matches!(frame, Some(Ok(_))) { break },
            }
        }
        let _ = child.kill().await;
    }))
}

pub async fn processes() -> Json<Vec<api::Process>> {
    // SAFETY: sysconf only reads.
    let (tick, page) = unsafe {
        (
            libc::sysconf(libc::_SC_CLK_TCK).max(1) as f64,
            libc::sysconf(libc::_SC_PAGESIZE).max(1) as u64,
        )
    };
    let mut names: BTreeMap<u32, String> = BTreeMap::new();
    let mut found = vec![];
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let stat = read(&format!("/proc/{pid}/stat"));
        // The name is in brackets and may itself contain brackets and spaces.
        let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) else {
            continue;
        };
        let fields: Vec<&str> = stat[close + 1..].split_whitespace().collect();
        let number = |index: usize| {
            fields
                .get(index)
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(0)
        };
        let command = read(&format!("/proc/{pid}/cmdline"))
            .replace('\0', " ")
            .trim()
            .to_owned();
        use std::os::unix::fs::MetadataExt;
        let uid = entry.metadata().map(|meta| meta.uid()).unwrap_or(0);
        let user = names
            .entry(uid)
            .or_insert_with(|| {
                crate::users::account(&uid.to_string())
                    .map_or(uid.to_string(), |account| account.name)
            })
            .clone();
        found.push(api::Process {
            pid,
            parent: number(1) as u32,
            user,
            command: if command.is_empty() {
                format!("[{}]", &stat[open + 1..close])
            } else {
                command
            },
            cpu_time: ((number(11) + number(12)) as f64 / tick * 100.0).round() / 100.0,
            memory: number(21) * page,
        });
    }
    found.sort_by_key(|process| process.pid);
    Json(found)
}

pub async fn process_signal(
    Path(pid): Path<i32>,
    Json(signal): Json<api::Signal>,
) -> R<Json<api::Done>> {
    let number = match signal.signal.trim_start_matches("SIG") {
        "INT" => libc::SIGINT,
        "TERM" => libc::SIGTERM,
        "HUP" => libc::SIGHUP,
        "KILL" => libc::SIGKILL,
        "STOP" => libc::SIGSTOP,
        "CONT" => libc::SIGCONT,
        "USR1" => libc::SIGUSR1,
        "USR2" => libc::SIGUSR2,
        _ => {
            return Err(E::bad(
                "signal is INT, TERM, HUP, KILL, STOP, CONT, USR1 or USR2",
            ));
        }
    };
    if pid <= 1 || pid as u32 == std::process::id() {
        return Err(E::bad("Not a process to signal"));
    }
    // SAFETY: a plain system call.
    if unsafe { libc::kill(pid, number) } != 0 {
        let error = std::io::Error::last_os_error();
        return Err(match error.raw_os_error() {
            Some(libc::ESRCH) => E::missing(format!("process {pid}")),
            _ => E::forbidden(error.to_string()),
        });
    }
    Ok(done())
}

#[derive(serde::Deserialize)]
pub struct Force {
    #[serde(default)]
    force: bool,
}

pub async fn power(
    AppState(state): AppState<Arc<State>>,
    Path(verb): Path<String>,
    Query(query): Query<Force>,
) -> R<Json<api::Done>> {
    systemd(&state)?;
    if !["reboot", "poweroff"].contains(&verb.as_str()) {
        return Err(E::bad("The verb is reboot or poweroff"));
    }
    let running = state.sessions.running();
    if running > 0 && !query.force {
        return Err(E(
            code::CONFLICT,
            format!("{running} sessions are running; force=true ends them"),
        ));
    }
    // A moment later, so this answer still gets out.
    tool(
        "systemd-run",
        &["--quiet", "--on-active=2", "systemctl", &verb],
    )
    .await?;
    Ok(done())
}
