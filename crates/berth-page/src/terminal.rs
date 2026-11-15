//! What the screens do with the system they run on: the clipboard, opening
//! links in the system's browser, and its dialog for choosing a file.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// The variables that carry secrets, which no helper program is handed.
pub const SECRETS: [&str; 2] = ["BERTH_TOKEN", "BERTH_SETUP_KEY"];

/// Open a web address in the system's browser.
pub fn open(link: &str) -> Result<(), String> {
    let link = openable(link)?;
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
        // Without a desktop xdg-open falls back to a browser in this
        // very terminal, under the screens.
        if !has_display() {
            return Err("there is no desktop here to open it on".into());
        }
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

/// Let the user choose a file whose name ends with one of `accept`
/// (comma-separated), and hand over its name and bytes at a later turn.
/// Nothing happens when none is chosen; an error says why none can be asked for.
pub fn choose_file(
    accept: &str,
    then: impl FnOnce(String, Vec<u8>) + 'static,
) -> Result<(), String> {
    let endings = endings(accept);
    let given = std::env::var_os("BERTH_FILE").map(PathBuf::from);
    let dialog = match given {
        Some(_) => None,
        None => Some(
            file_dialog(&endings)
                .ok_or("There is no file dialog here: set BERTH_FILE to the package's path")?,
        ),
    };
    crate::net::later_blocking(
        move || {
            let path = match (given, dialog) {
                (Some(path), _) => path,
                (None, Some(mut dialog)) => {
                    let output = dialog
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
                (None, None) => return None,
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
    Ok(())
}

/// A program this one starts, such as a clipboard or a browser: without
/// the secrets of its environment, which it would pass on to what it starts.
fn helper(program: &str) -> Command {
    let mut command = Command::new(program);
    for name in SECRETS {
        command.env_remove(name);
    }
    command
}

fn has_display() -> bool {
    ["DISPLAY", "WAYLAND_DISPLAY"]
        .iter()
        .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
}

/// The link, when it is one to hand to the system's browser: a web address,
/// never a file or a program, and nothing a helper could take for an option.
fn openable(link: &str) -> Result<&str, String> {
    let lower = link.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"));
    let web = rest.is_some_and(|rest| !rest.is_empty() && !rest.starts_with('/'));
    let plain = !link
        .chars()
        .any(|ch| ch.is_whitespace() || ch.is_control() || ch == '"');
    match web && plain {
        true => Ok(link),
        false => Err("only http and https links are opened".into()),
    }
}

/// The endings an `accept` list names, such as `.tgz,.tar.gz`.
fn endings(accept: &str) -> Vec<String> {
    accept
        .split(',')
        .map(|ending| ending.trim().to_owned())
        .filter(|ending| !ending.is_empty())
        .collect()
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

type Helper = (&'static str, &'static [&'static str]);

/// The clipboard programs of this system, with whether each is waited for.
fn copiers() -> Vec<(Helper, bool)> {
    if cfg!(windows) {
        // Read as UTF-8, which clip.exe would not.
        vec![(
            (
                "powershell",
                &[
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "[Console]::InputEncoding = [Text.Encoding]::UTF8; Set-Clipboard -Value ([Console]::In.ReadToEnd())",
                ],
            ),
            true,
        )]
    } else if cfg!(target_os = "macos") {
        vec![(("pbcopy", &[]), true)]
    } else {
        let mut copiers = vec![];
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            copiers.push((("wl-copy", &[][..]), true));
        }
        // Without a display they fail after the text is written, which would
        // read as copied; then OSC 52 does it.
        if std::env::var_os("DISPLAY").is_some() {
            copiers.push((("xclip", &["-selection", "clipboard"][..]), false));
            copiers.push((("xsel", &["--clipboard", "--input"][..]), false));
        }
        copiers
    }
}

pub fn copy_text(text: &str) -> bool {
    if copiers()
        .into_iter()
        .any(|((program, args), wait)| feed(program, args, text, wait))
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

pub fn paste_text() -> Option<String> {
    let pasters: Vec<Helper> = if cfg!(windows) {
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
        let mut pasters: Vec<Helper> = vec![];
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

/// The script of the file dialog on Windows. The filter comes in an
/// environment variable, so that nothing is spliced into the script.
const WINDOWS_DIALOG: &str = "Add-Type -AssemblyName System.Windows.Forms; \
    $dialog = New-Object System.Windows.Forms.OpenFileDialog; \
    $dialog.Filter = $env:BERTH_FILTER; \
    if ($dialog.ShowDialog() -eq 'OK') { [Console]::OutputEncoding = [Text.Encoding]::UTF8; $dialog.FileName }";

/// The file dialog of this system, given what the accepted names end with;
/// `None` when it has none.
fn file_dialog(endings: &[String]) -> Option<Command> {
    let patterns = |separator: &str| {
        endings
            .iter()
            .map(|ending| format!("*{ending}"))
            .collect::<Vec<_>>()
            .join(separator)
    };
    if cfg!(windows) {
        let filter = patterns(";");
        let mut command = helper("powershell");
        command
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-STA",
                "-Command",
                WINDOWS_DIALOG,
            ])
            .env(
                "BERTH_FILTER",
                format!("Packages ({filter})|{filter}|All files (*.*)|*.*"),
            );
        return Some(command);
    }
    if cfg!(target_os = "macos") {
        let mut command = helper("osascript");
        command.args(["-e", "POSIX path of (choose file)"]);
        return Some(command);
    }
    if !has_display() {
        return None;
    }
    let found = |program: &str| {
        std::env::var_os("PATH")
            .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
    };
    if found("zenity") {
        let mut command = helper("zenity");
        command.args(["--file-selection", "--title=Choose a plugin package"]);
        command.arg(format!("--file-filter=Packages | {}", patterns(" ")));
        return Some(command);
    }
    if found("kdialog") {
        let mut command = helper("kdialog");
        command.args(["--getopenfilename", "."]);
        return Some(command);
    }
    None
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
    fn only_web_links_are_opened() {
        for link in [
            "https://studio.example/?a=1&b=2",
            "http://192.0.2.10:7878/",
            "HTTPS://studio.example",
        ] {
            assert_eq!(openable(link), Ok(link), "{link}");
        }
        for link in [
            "file:///etc/passwd",
            "C:\\Windows\\System32\\calc.exe",
            "javascript:alert(1)",
            "-a",
            "--help https://studio.example",
            "https://studio.example/ --new-window",
            "https://studio.example/\nrm",
            "ssh://studio.example",
            "https://",
            "",
        ] {
            assert!(openable(link).is_err(), "{link:?}");
        }
    }

    #[test]
    fn endings_of_accept() {
        assert_eq!(endings(".tgz, .tar.gz,"), [".tgz", ".tar.gz"]);
        assert!(endings("").is_empty());
    }
}
