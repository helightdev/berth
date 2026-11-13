//! What the browser does for the page, done by this program instead: the
//! clipboard, opening links, choosing a file, and the server and token it
//! keeps between runs. The screens call these by the page's names.

use crate::net;
use berth_tui::screen::Platform;
use serde_json::{Map, Value};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// The names of what is kept between runs: the token, and the server it is for.
pub const TOKEN: &str = "token";
pub const SERVER: &str = "server";

pub struct Browser;

/// The variables that carry secrets, which no helper program is handed.
pub const SECRETS: [&str; 2] = ["BERTH_TOKEN", "BERTH_SETUP_KEY"];

/// A program this one starts, such as a clipboard or a browser: without
/// the secrets of its environment, which it would pass on to what it starts.
fn helper(program: &str) -> Command {
    let mut command = Command::new(program);
    for name in SECRETS {
        command.env_remove(name);
    }
    command
}

impl Platform for Browser {
    fn copy(&mut self, text: &str) -> bool {
        copy_text(text)
    }

    fn paste(&mut self) -> Option<String> {
        paste_text()
    }

    fn open(&mut self, link: &str) -> Result<(), String> {
        let mut command = if cfg!(windows) {
            // Not through cmd, which would take the & of an address for its own.
            let mut command = helper("rundll32");
            command.args(["url.dll,FileProtocolHandler", link]);
            command
        } else if cfg!(target_os = "macos") {
            let mut command = helper("open");
            command.arg(link);
            command
        } else {
            let mut command = helper("xdg-open");
            command.arg(link);
            command
        };
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(drop)
            .map_err(|error| error.to_string())
    }
}

/// Run `program` with `text` on its standard input. `wait` for one that
/// ends once it has it; a clipboard of X keeps running to serve it.
fn feed(program: &str, args: &[&str], text: &str, wait: bool) -> bool {
    let Ok(mut child) = helper(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let written = child
        .stdin
        .take()
        .is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
    written && (!wait || child.wait().is_ok_and(|status| status.success()))
}

/// The clipboard programs of this system, with whether each is waited for.
fn copiers() -> Vec<(&'static str, &'static [&'static str], bool)> {
    if cfg!(windows) {
        // Read as UTF-8, which clip.exe would not.
        vec![(
            "powershell",
            &[
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "[Console]::InputEncoding = [Text.Encoding]::UTF8; Set-Clipboard -Value ([Console]::In.ReadToEnd())",
            ],
            true,
        )]
    } else if cfg!(target_os = "macos") {
        vec![("pbcopy", &[], true)]
    } else {
        let mut copiers = vec![];
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            copiers.push(("wl-copy", &[][..], true));
        }
        // Without a display they fail after the text is written, which would
        // read as copied; then OSC 52 does it.
        if std::env::var_os("DISPLAY").is_some() {
            copiers.push(("xclip", &["-selection", "clipboard"], false));
            copiers.push(("xsel", &["--clipboard", "--input"], false));
        }
        copiers
    }
}

fn copy_text(text: &str) -> bool {
    if copiers()
        .into_iter()
        .any(|(program, args, wait)| feed(program, args, text, wait))
    {
        return true;
    }
    // A terminal that knows OSC 52 copies for programs on other machines too.
    let mut out = std::io::stdout();
    write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes())).is_ok() && out.flush().is_ok()
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (at, byte)| n | (*byte as u32) << (16 - 8 * at));
        for at in 0..4 {
            match at <= chunk.len() {
                true => out.push(ALPHABET[(n >> (18 - 6 * at) & 63) as usize] as char),
                false => out.push('='),
            }
        }
    }
    out
}

fn paste_text() -> Option<String> {
    let pasters: Vec<(&str, &[&str])> = if cfg!(windows) {
        vec![(
            "powershell",
            &[
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "[Console]::OutputEncoding = [Text.Encoding]::UTF8; Get-Clipboard -Raw",
            ],
        )]
    } else if cfg!(target_os = "macos") {
        vec![("pbpaste", &[])]
    } else {
        let mut pasters: Vec<(&str, &[&str])> = vec![];
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            pasters.push(("wl-paste", &["--no-newline"]));
        }
        if std::env::var_os("DISPLAY").is_some() {
            pasters.push(("xclip", &["-selection", "clipboard", "-o"]));
            pasters.push(("xsel", &["--clipboard", "--output"]));
        }
        pasters
    };
    pasters.into_iter().find_map(|(program, args)| {
        let output = helper(program)
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        // Only the line end the tools add goes: pasted code keeps its indent.
        let text = String::from_utf8_lossy(&output.stdout)
            .trim_end_matches(['\r', '\n'])
            .to_owned();
        (output.status.success() && !text.is_empty()).then_some(text)
    })
}

/// The file dialog of this system, given what the accepted names end with;
/// `None` when it has none.
fn file_dialog(endings: &[String]) -> Option<Command> {
    if cfg!(windows) {
        let filter = endings
            .iter()
            .map(|ending| format!("*{ending}"))
            .collect::<Vec<_>>()
            .join(";");
        let script = format!(
            "Add-Type -AssemblyName System.Windows.Forms; \
             $dialog = New-Object System.Windows.Forms.OpenFileDialog; \
             $dialog.Filter = 'Packages ({filter})|{filter}|All files (*.*)|*.*'; \
             if ($dialog.ShowDialog() -eq 'OK') {{ [Console]::OutputEncoding = [Text.Encoding]::UTF8; $dialog.FileName }}"
        );
        let mut command = helper("powershell");
        command.args(["-NoProfile", "-NonInteractive", "-STA", "-Command", &script]);
        return Some(command);
    }
    if cfg!(target_os = "macos") {
        let mut command = helper("osascript");
        command.args(["-e", "POSIX path of (choose file)"]);
        return Some(command);
    }
    let found = |program: &str| {
        std::env::var_os("PATH")
            .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
    };
    if found("zenity") {
        let mut command = helper("zenity");
        command.args(["--file-selection", "--title=Choose a plugin package"]);
        let filter = endings
            .iter()
            .map(|ending| format!("*{ending}"))
            .collect::<Vec<_>>()
            .join(" ");
        command.arg(format!("--file-filter=Packages | {filter}"));
        return Some(command);
    }
    if found("kdialog") {
        let mut command = helper("kdialog");
        command.args(["--getopenfilename", "."]);
        return Some(command);
    }
    None
}

/// Let the user choose a file of this device, and hand over its name and
/// bytes once it is read. Nothing happens when none is chosen, or when this
/// system has no file dialog: then the path comes from `BERTH_REMOTE_FILE`.
pub fn choose_file(accept: &str, then: impl FnOnce(String, Vec<u8>) + 'static) {
    let endings: Vec<String> = accept
        .split(',')
        .map(|ending| ending.trim().to_owned())
        .filter(|ending| !ending.is_empty())
        .collect();
    net::later_blocking(
        move || {
            let path = match std::env::var_os("BERTH_REMOTE_FILE") {
                Some(path) => PathBuf::from(path),
                None => {
                    let output = file_dialog(&endings)?
                        .stdin(Stdio::null())
                        .stderr(Stdio::null())
                        .output()
                        .ok()?;
                    let chosen = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                    if !output.status.success() || chosen.is_empty() {
                        return None;
                    }
                    PathBuf::from(chosen)
                }
            };
            let bytes = std::fs::read(&path).ok()?;
            let name = path.file_name()?.to_string_lossy().into_owned();
            Some((name, bytes))
        },
        move |chosen| {
            if let Some((name, bytes)) = chosen {
                then(name, bytes)
            }
        },
    );
}

/// The folder this program keeps its settings in.
fn config_dir() -> Option<PathBuf> {
    let var = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
    let base = if cfg!(windows) {
        PathBuf::from(var("APPDATA")?)
    } else if let Some(dir) = var("XDG_CONFIG_HOME") {
        PathBuf::from(dir)
    } else if cfg!(target_os = "macos") {
        PathBuf::from(var("HOME")?).join("Library/Application Support")
    } else {
        PathBuf::from(var("HOME")?).join(".config")
    };
    Some(base.join("berth-remote"))
}

fn settings_file() -> Option<PathBuf> {
    Some(config_dir()?.join("settings.json"))
}

fn settings() -> Map<String, Value> {
    settings_file()
        .and_then(|file| std::fs::read(file).ok())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

/// What is kept between runs: the token, and the server it is for.
pub fn stored(key: &str) -> Option<String> {
    if options().ephemeral {
        return None;
    }
    settings()
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// Keep `value`, or forget it when it is empty. With `--forget` nothing new
/// is kept, but what an earlier run kept can still be forgotten.
pub fn store(key: &str, value: &str) {
    if options().ephemeral && !value.is_empty() {
        return;
    }
    let mut settings = settings();
    let had = settings.get(key).and_then(Value::as_str).unwrap_or("");
    if had == value {
        return;
    }
    match value.is_empty() {
        true => settings.remove(key),
        false => settings.insert(key.to_owned(), Value::String(value.to_owned())),
    };
    let (Some(dir), Some(file)) = (config_dir(), settings_file()) else {
        return;
    };
    let text = Value::Object(settings).to_string();
    let _ = private_dir(&dir).and_then(|_| private_write(&file, text.as_bytes()));
}

/// The folder of the settings, which only its owner can open.
fn private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)?;
    // A folder an earlier version made keeps its mode otherwise.
    #[cfg(unix)]
    std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    Ok(())
}

/// Write a file only its owner can read, since it holds the token.
fn private_write(file: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let partial = file.with_extension("json.partial");
    // A file left over would keep its own mode.
    let _ = std::fs::remove_file(&partial);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    // On Windows the folder of the user's profile is the user's alone.
    let mut out = options.open(&partial)?;
    out.write_all(bytes)?;
    out.sync_all()?;
    drop(out);
    std::fs::rename(partial, file)
}

/// This program is never given by a server: it always asks which one.
pub fn embedded() -> bool {
    false
}

/// What the program was started with: the server, and from the environment
/// a token or a setup key, which never go on a command line.
#[derive(Default, Clone)]
pub struct Linked {
    pub server: Option<String>,
    pub token: Option<String>,
    /// A key that collects the token, once, after an install.
    pub setup_key: Option<String>,
}

#[derive(Default)]
pub struct Options {
    pub linked: Linked,
    /// Keep nothing between runs.
    pub ephemeral: bool,
}

static OPTIONS: OnceLock<Options> = OnceLock::new();

/// Set what the program was started with, before the screens start.
pub fn set_options(options: Options) {
    let _ = OPTIONS.set(options);
}

fn options() -> &'static Options {
    OPTIONS.get_or_init(Options::default)
}

pub fn linked() -> Linked {
    options().linked.clone()
}

/// One plain HTTP request with a bearer, and then its status and body. For
/// what the portal cannot carry because the caller has no token yet.
pub fn request(
    method: &str,
    url: &str,
    bearer: &str,
    then: impl FnOnce(Result<(u16, String), String>) + 'static,
) {
    let (method, url, bearer) = (method.to_owned(), url.to_owned(), bearer.to_owned());
    net::later(async move { net::http(&method, &url, &bearer).await }, then);
}

/// The address the page came from, which this program has none of.
pub fn origin() -> String {
    String::new()
}

/// The page a link for a phone opens: the one `berth serve --web` gives at
/// the server this program is signed in to.
pub fn page() -> String {
    stored(SERVER)
        .map(|server| format!("{}/", server.trim_end_matches('/')))
        .unwrap_or_default()
}

/// A server's address as typed, made whole: without a scheme it is https,
/// or http for this machine itself.
pub fn address(typed: &str) -> String {
    let typed = typed.trim().trim_end_matches('/');
    if typed.is_empty() || typed.starts_with("http://") || typed.starts_with("https://") {
        return typed.to_owned();
    }
    let local = ["localhost", "127.", "[::1]"]
        .iter()
        .any(|name| typed.starts_with(name));
    format!("{}://{typed}", if local { "http" } else { "https" })
}

/// A server as it is shown: `host:port`.
pub fn shown(server: &str) -> &str {
    server
        .trim_start_matches("https://")
        .trim_start_matches("http://")
}

/// The address of the portal of a server.
pub fn portal_url(server: &str) -> String {
    let scheme = match server.starts_with("https://") {
        true => "wss",
        false => "ws",
    };
    format!("{scheme}://{}{}", shown(server), berth_api::ops::PORTAL)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_pads() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn addresses() {
        assert_eq!(
            address("studio.example:8080"),
            "https://studio.example:8080"
        );
        assert_eq!(address("127.0.0.1:7878/"), "http://127.0.0.1:7878");
        assert_eq!(
            portal_url("https://studio.example"),
            "wss://studio.example/v1/portal"
        );
        assert_eq!(
            portal_url("http://192.0.2.10:7878"),
            "ws://192.0.2.10:7878/v1/portal"
        );
    }
}
