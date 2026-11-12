//! Files, touched as the agent and only under the configured roots. Each
//! request is done by a short-lived helper that runs as the agent and opens
//! paths beneath the root's descriptor, so neither `..` nor a symlink leads out.

use crate::error::{E, R};
use crate::state::{Mode, State};
use axum::{
    Json,
    body::Body,
    extract::{Query, State as AppState},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use berth_api::{self as api, code};
use futures_util::StreamExt;
use std::{process::Stdio, sync::Arc};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

#[derive(serde::Deserialize)]
pub struct FileQuery {
    path: String,
    #[serde(default)]
    overwrite: bool,
    #[serde(default)]
    recursive: bool,
}

/// The root a path is under, and the path beneath it.
fn locate(state: &State, path: &str) -> R<(String, String)> {
    let config = state.config();
    let mut roots = config.berthd.roots.clone();
    if roots.is_empty()
        && state.mode == Mode::System
        && let Some(agent) = crate::users::account(&config.berthd.agent)
    {
        roots.push(agent.home);
    }
    if !path.starts_with('/') || path.contains('\0') {
        return Err(E::bad("path is absolute"));
    }
    for root in roots {
        let root = root.trim_end_matches('/').to_owned();
        if path == root {
            return Ok((root, String::new()));
        }
        if let Some(rest) = path.strip_prefix(&format!("{root}/")) {
            return Ok((root, rest.trim_matches('/').to_owned()));
        }
    }
    Err(E::forbidden(format!(
        "{path} is not under a folder the file calls may touch"
    )))
}

fn helper(state: &State, args: &[&str]) -> R<tokio::process::Command> {
    let program = std::env::current_exe()?;
    let mut command = std::process::Command::new(program);
    command.arg("files-helper").args(args);
    if let Some(account) = crate::users::resolve(state, "agent")? {
        crate::users::become_user(&mut command, &account);
        command.current_dir("/");
    }
    let mut command = tokio::process::Command::from(command);
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    Ok(command)
}

/// What the helper said went wrong: `CODE message` on standard error.
fn failure(errors: &[u8]) -> E {
    let text = String::from_utf8_lossy(errors);
    let (kind, message) = text
        .trim()
        .split_once(' ')
        .unwrap_or(("internal", text.trim()));
    let kind = match kind {
        "not_found" => code::NOT_FOUND,
        "forbidden" => code::FORBIDDEN,
        "conflict" => code::CONFLICT,
        "bad_request" => code::BAD_REQUEST,
        _ => code::INTERNAL,
    };
    E(kind, message.to_owned())
}

/// A folder's entries, or a file's content (with `Range`).
pub async fn get(
    AppState(state): AppState<Arc<State>>,
    Query(query): Query<FileQuery>,
    headers: HeaderMap,
) -> R<Response> {
    let (root, rest) = locate(&state, &query.path)?;
    // `bytes=FROM-TO`, either end optional.
    let range = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            let (from, to) = value.strip_prefix("bytes=")?.split_once('-')?;
            Some((from.parse::<u64>().ok()?, to.parse::<u64>().ok()))
        });
    let (from, to) = range.unwrap_or((0, None));
    let length = to.map_or("-1".to_owned(), |to| {
        (to + 1).saturating_sub(from).to_string()
    });
    let mut child = helper(&state, &["get", &root, &rest, &from.to_string(), &length])?
        .stdin(Stdio::null())
        .spawn()?;
    let mut output = BufReader::new(child.stdout.take().expect("piped"));
    let mut head = String::new();
    output.read_line(&mut head).await?;
    let Ok(head) = serde_json::from_str::<serde_json::Value>(&head) else {
        let mut errors = vec![];
        if let Some(mut stderr) = child.stderr.take() {
            let _ = stderr.read_to_end(&mut errors).await;
        }
        return Err(failure(&errors));
    };
    if head["kind"] == "dir" {
        let mut entries: Vec<api::FileEntry> =
            serde_json::from_value(head["entries"].clone()).unwrap_or_default();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        return Ok(Json(api::Listing {
            path: query.path,
            entries,
        })
        .into_response());
    }
    let size = head["size"].as_u64().unwrap_or(0);
    let sent = head["sent"].as_u64().unwrap_or(0);
    let mut response = Response::builder()
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, sent)
        .header(
            header::LAST_MODIFIED,
            head["modified"].as_str().unwrap_or(""),
        )
        .header(header::ACCEPT_RANGES, "bytes");
    if range.is_some() {
        response = response.status(StatusCode::PARTIAL_CONTENT).header(
            header::CONTENT_RANGE,
            format!("bytes {from}-{}/{size}", (from + sent).saturating_sub(1)),
        );
    }
    // The helper lives as long as the body is read.
    let stream = tokio_util::io::ReaderStream::new(output).map(move |chunk| {
        let _held = &child;
        chunk
    });
    response
        .body(Body::from_stream(stream))
        .map_err(E::internal)
}

/// Write one file, creating its folders.
pub async fn put(
    AppState(state): AppState<Arc<State>>,
    Query(query): Query<FileQuery>,
    body: Body,
) -> R<Json<api::Done>> {
    let (root, rest) = locate(&state, &query.path)?;
    if rest.is_empty() {
        return Err(E::bad("path names a file under a root"));
    }
    let mode = if query.overwrite { "overwrite" } else { "new" };
    let mut child = helper(&state, &["put", &root, &rest, mode])?
        .stdin(Stdio::piped())
        .spawn()?;
    let mut input = child.stdin.take().expect("piped");
    let mut chunks = body.into_data_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|error| E::bad(error.to_string()))?;
        if input.write_all(&chunk).await.is_err() {
            break;
        }
    }
    drop(input);
    let output = child.wait_with_output().await?;
    match output.status.success() {
        true => Ok(Json(api::Done { ok: true })),
        false => Err(failure(&output.stderr)),
    }
}

/// Remove a file or a folder; never a root itself.
pub async fn delete(
    AppState(state): AppState<Arc<State>>,
    Query(query): Query<FileQuery>,
) -> R<Json<api::Done>> {
    let (root, rest) = locate(&state, &query.path)?;
    if rest.is_empty() {
        return Err(E::forbidden("A root itself is not removed"));
    }
    let mode = if query.recursive {
        "recursive"
    } else {
        "plain"
    };
    let output = helper(&state, &["delete", &root, &rest, mode])?
        .stdin(Stdio::null())
        .output()
        .await?;
    match output.status.success() {
        true => Ok(Json(api::Done { ok: true })),
        false => Err(failure(&output.stderr)),
    }
}

// ── The helper, run as the agent ────────────────────────────────────────────

mod helper {
    use std::{
        ffi::{CStr, CString},
        fs::File,
        io::{self, Read, Seek, Write},
        os::fd::{AsRawFd, FromRawFd, OwnedFd},
    };

    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }

    const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
    const RESOLVE_BENEATH: u64 = 0x08;

    fn last() -> io::Error {
        io::Error::last_os_error()
    }

    fn c(text: &str) -> io::Result<CString> {
        CString::new(text).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
    }

    /// Open `path` beneath `dir`. The kernel refuses anything that would lead
    /// out: `..` past the top, an absolute symlink, a symlink pointing outside.
    fn beneath(dir: &OwnedFd, path: &str, flags: i32, mode: u32) -> io::Result<OwnedFd> {
        let path = c(if path.is_empty() { "." } else { path })?;
        let how = OpenHow {
            flags: (flags | libc::O_CLOEXEC) as u64,
            mode: mode as u64,
            resolve: RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS,
        };
        // SAFETY: the structure and the path live through the call; the result is a new descriptor.
        unsafe {
            let fd = libc::syscall(
                libc::SYS_openat2,
                dir.as_raw_fd(),
                path.as_ptr(),
                &how,
                std::mem::size_of::<OpenHow>(),
            );
            if fd < 0 {
                Err(last())
            } else {
                Ok(OwnedFd::from_raw_fd(fd as i32))
            }
        }
    }

    fn stat(dir: &OwnedFd, name: &CStr) -> io::Result<libc::stat> {
        // SAFETY: fstatat fills the structure it is given.
        unsafe {
            let mut stat: libc::stat = std::mem::zeroed();
            match libc::fstatat(
                dir.as_raw_fd(),
                name.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            ) {
                0 => Ok(stat),
                _ => Err(last()),
            }
        }
    }

    fn kind(mode: u32) -> &'static str {
        match mode & libc::S_IFMT {
            libc::S_IFREG => "file",
            libc::S_IFDIR => "dir",
            libc::S_IFLNK => "link",
            _ => "other",
        }
    }

    /// The names in a folder.
    fn names(dir: &OwnedFd) -> io::Result<Vec<CString>> {
        let copy = dir.try_clone()?;
        // SAFETY: fdopendir takes over the descriptor; closedir gives it back to the system.
        unsafe {
            let stream = libc::fdopendir(copy.as_raw_fd());
            if stream.is_null() {
                return Err(last());
            }
            std::mem::forget(copy);
            let mut names = vec![];
            loop {
                let entry = libc::readdir(stream);
                if entry.is_null() {
                    break;
                }
                let name = CStr::from_ptr((*entry).d_name.as_ptr());
                if name.to_bytes() != b"." && name.to_bytes() != b".." {
                    names.push(name.to_owned());
                }
            }
            libc::closedir(stream);
            Ok(names)
        }
    }

    fn get(root: &OwnedFd, path: &str, from: u64, length: i64) -> io::Result<()> {
        let fd = beneath(root, path, libc::O_RDONLY | libc::O_NONBLOCK, 0)?;
        let meta = File::from(fd.try_clone()?).metadata()?;
        let mut out = io::stdout().lock();
        if meta.is_dir() {
            let mut entries = vec![];
            for name in names(&fd)? {
                let Ok(stat) = stat(&fd, &name) else { continue };
                entries.push(serde_json::json!({
                    "name": name.to_string_lossy(),
                    "kind": kind(stat.st_mode),
                    "size": stat.st_size,
                    "modified": berth_api::rfc3339(stat.st_mtime.max(0) as u64),
                }));
            }
            return writeln!(
                out,
                "{}",
                serde_json::json!({"kind": "dir", "entries": entries})
            );
        }
        if !meta.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Not a file or a folder",
            ));
        }
        use std::os::unix::fs::MetadataExt;
        let size = meta.len();
        let from = from.min(size);
        let sent = if length < 0 {
            size - from
        } else {
            (length as u64).min(size - from)
        };
        let modified = berth_api::rfc3339(meta.mtime().max(0) as u64);
        writeln!(
            out,
            "{}",
            serde_json::json!({"kind": "file", "size": size, "sent": sent, "modified": modified})
        )?;
        let mut file = File::from(fd);
        file.seek(io::SeekFrom::Start(from))?;
        io::copy(&mut file.take(sent), &mut out)?;
        out.flush()
    }

    /// The folder a path's last part is in, and that part.
    fn parent<'a>(root: &OwnedFd, path: &'a str, create: bool) -> io::Result<(OwnedFd, &'a str)> {
        let (folders, name) = path.rsplit_once('/').unwrap_or(("", path));
        if name.is_empty() || name == "." || name == ".." {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        let mut dir = root.try_clone()?;
        for part in folders.split('/').filter(|part| !part.is_empty()) {
            if create {
                // SAFETY: a plain system call; an existing folder is fine.
                unsafe { libc::mkdirat(dir.as_raw_fd(), c(part)?.as_ptr(), 0o755) };
            }
            dir = beneath(&dir, part, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
        }
        Ok((dir, name))
    }

    fn put(root: &OwnedFd, path: &str, overwrite: bool) -> io::Result<()> {
        let (dir, name) = parent(root, path, true)?;
        let how = if overwrite {
            libc::O_TRUNC
        } else {
            libc::O_EXCL
        };
        let fd = beneath(&dir, name, libc::O_WRONLY | libc::O_CREAT | how, 0o644)?;
        let mut file = File::from(fd);
        io::copy(&mut io::stdin().lock(), &mut file)?;
        file.flush()
    }

    fn remove_tree(dir: &OwnedFd, name: &CStr) -> io::Result<()> {
        // SAFETY: plain system calls on a descriptor and a name that live through them.
        unsafe {
            let fd = libc::openat(
                dir.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            );
            if fd < 0 {
                return Err(last());
            }
            let folder = OwnedFd::from_raw_fd(fd);
            for entry in names(&folder)? {
                if stat(&folder, &entry)?.st_mode & libc::S_IFMT == libc::S_IFDIR {
                    remove_tree(&folder, &entry)?;
                } else if libc::unlinkat(folder.as_raw_fd(), entry.as_ptr(), 0) != 0 {
                    return Err(last());
                }
            }
            match libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) {
                0 => Ok(()),
                _ => Err(last()),
            }
        }
    }

    fn delete(root: &OwnedFd, path: &str, recursive: bool) -> io::Result<()> {
        let (dir, name) = parent(root, path, false)?;
        let name = c(name)?;
        let folder = stat(&dir, &name)?.st_mode & libc::S_IFMT == libc::S_IFDIR;
        if folder && recursive {
            return remove_tree(&dir, &name);
        }
        // SAFETY: a plain system call.
        match unsafe {
            libc::unlinkat(
                dir.as_raw_fd(),
                name.as_ptr(),
                if folder { libc::AT_REMOVEDIR } else { 0 },
            )
        } {
            0 => Ok(()),
            _ => Err(last()),
        }
    }

    /// `berthd files-helper get|put|delete ROOT PATH …`. Exit code 0, or 1 with
    /// `CODE message` on standard error.
    pub fn main(args: &[String]) -> i32 {
        let run = || -> io::Result<()> {
            let [operation, root, path, rest @ ..] = args else {
                return Err(io::Error::from(io::ErrorKind::InvalidInput));
            };
            let root = OwnedFd::from(File::open(root)?);
            let flag = rest.first().map(String::as_str).unwrap_or("");
            match operation.as_str() {
                "get" => get(
                    &root,
                    path,
                    flag.parse().unwrap_or(0),
                    rest.get(1)
                        .and_then(|length| length.parse().ok())
                        .unwrap_or(-1),
                ),
                "put" => put(&root, path, flag == "overwrite"),
                "delete" => delete(&root, path, flag == "recursive"),
                _ => Err(io::Error::from(io::ErrorKind::InvalidInput)),
            }
        };
        let Err(error) = run() else {
            return 0;
        };
        let (kind, message) = match error.raw_os_error() {
            Some(libc::ENOENT) => ("not_found", "No such file or folder".to_owned()),
            // What the kernel says when a path would leave the root.
            Some(libc::EXDEV) | Some(libc::ELOOP) => (
                "forbidden",
                "The path leads out of the folder the file calls may touch".to_owned(),
            ),
            Some(libc::EACCES) | Some(libc::EPERM) => {
                ("forbidden", "The agent account may not do that".to_owned())
            }
            Some(libc::EEXIST) => (
                "conflict",
                "It exists already; overwrite=true replaces a file".to_owned(),
            ),
            Some(libc::ENOTEMPTY) => (
                "conflict",
                "The folder is not empty; recursive=true removes it with its contents".to_owned(),
            ),
            Some(libc::EISDIR) | Some(libc::ENOTDIR) => ("bad_request", error.to_string()),
            _ if error.kind() == io::ErrorKind::InvalidInput => {
                ("bad_request", "Not a usable path".to_owned())
            }
            _ => ("internal", error.to_string()),
        };
        eprintln!("{kind} {message}");
        1
    }
}

pub use helper::main as helper_main;
