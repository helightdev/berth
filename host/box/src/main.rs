//! `box`: the command for agents and plugin programs on a berth host.

mod desktop;
mod net;
mod serve;

use serde_json::{Map, Value, json};
use std::io::Write;

const USAGE: &str = "usage:
  box desktop screenshot [--output PATH|-] | click X Y [BUTTON] | type TEXT | key CHORD | scroll up|down [STEPS] | clipboard | pause | resume
  box mcp                               the desktop tools as an MCP server on stdio
  box exec [COMMAND...]                 run a command, or a shell, with the desktop's environment
  box tasks                             the tasks this user may run
  box run TASK [NAME=VALUE...]          run one; output here, its exit code as box's
  box publish TOPIC [JSON] [--keep none|last|log]
  box listen TOPIC-PREFIX [--since ID]  one JSON message per line
  box services                          what can be called, and whether it is up
  box call SERVICE METHOD [JSON]        result on stdout
  box serve SERVICE -- COMMAND...       per call: COMMAND METHOD, body on stdin, result on stdout";

/// Remove `--name VALUE` from the arguments.
pub fn take_flag(args: &mut Vec<String>, name: &str) -> Result<Option<String>, String> {
    let Some(at) = args.iter().position(|a| a == name) else {
        return Ok(None);
    };
    if at + 1 >= args.len() {
        return Err(format!("{name} needs a value"));
    }
    let value = args.remove(at + 1);
    args.remove(at);
    Ok(Some(value))
}

/// `NAME=VALUE` words as a JSON object of strings.
pub fn assignments(words: &[String]) -> Result<Map<String, Value>, String> {
    words
        .iter()
        .map(|w| match w.split_once('=') {
            Some((k, v)) if !k.is_empty() => Ok((k.to_string(), Value::String(v.to_string()))),
            _ => Err(format!("expected NAME=VALUE, got {w}")),
        })
        .collect()
}

/// JSON if it parses, else the text as a JSON string.
pub fn json_or_string(text: &str) -> Value {
    serde_json::from_str(text.trim())
        .unwrap_or_else(|_| Value::String(text.trim_end_matches('\n').to_string()))
}

/// The arguments object for a desktop tool from the words after its name.
pub fn tool_args(command: &str, words: &[String]) -> Result<Value, String> {
    let num = |s: &String| {
        s.parse::<i64>()
            .map_or(Value::String(s.clone()), Value::from)
    };
    let bad = || format!("usage: box desktop {command} …");
    Ok(match (command, words) {
        ("screenshot" | "clipboard", []) => json!({}),
        ("click", [x, y]) => json!({"x": num(x), "y": num(y)}),
        ("click", [x, y, b]) => json!({"x": num(x), "y": num(y), "button": num(b)}),
        ("type", [text]) => json!({"text": text}),
        ("type", [_, ..]) => json!({"text": words.join(" ")}),
        ("key", [key]) => json!({"key": key}),
        ("scroll", [d]) => json!({"direction": d}),
        ("scroll", [d, n]) => json!({"direction": d, "steps": num(n)}),
        _ => return Err(bad()),
    })
}

fn desktop_cli(mut words: Vec<String>) -> Result<(), String> {
    let output = take_flag(&mut words, "--output")?;
    let json_args = take_flag(&mut words, "--args")?;
    if words.is_empty() {
        return Err(USAGE.into());
    }
    let command = words.remove(0);
    if command == "pause" || command == "resume" {
        desktop::set_paused(command == "pause")?;
        println!("{command}");
        return Ok(());
    }
    let args = match json_args {
        Some(text) => serde_json::from_str(&text).map_err(|e| e.to_string())?,
        None => tool_args(&command, &words)?,
    };
    let result = desktop::perform(&command, &args)?;
    match (result["type"] == "image", output) {
        (true, Some(path)) => {
            let bytes = berth_api::unb64(result["data"].as_str().unwrap_or("")).unwrap_or_default();
            if path == "-" {
                let mut out = std::io::stdout().lock();
                out.write_all(&bytes)
                    .and_then(|_| out.flush())
                    .map_err(|e| e.to_string())?;
            } else {
                std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
                println!("{path}");
            }
        }
        _ => println!("{result}"),
    }
    Ok(())
}

const AGENT_ENV: &str = "/usr/local/bin/agent-env";

/// The desktop commands need the display, its Xauthority and the session bus.
/// `agent-env` has them, so box starts itself again through it, once, and the
/// commands work from any shell of the agent.
fn enter_desktop_environment(args: &[String]) {
    use std::os::unix::process::CommandExt;
    if std::env::var_os("BOX_DESKTOP_ENV").is_some() || !std::path::Path::new(AGENT_ENV).exists() {
        return;
    }
    let Ok(this) = std::env::current_exe() else {
        return;
    };
    // Returns only when it could not start; the command then runs as it is.
    let _ = std::process::Command::new(AGENT_ENV)
        .arg(this)
        .args(&args[1..])
        .arg0(AGENT_ENV)
        .env("BOX_DESKTOP_ENV", "1")
        .exec();
}

fn real_main(mut args: Vec<String>) -> Result<i32, String> {
    let name = std::path::Path::new(&args[0])
        .file_name()
        .map(|n| n.to_string_lossy().into_owned());
    // The old name of `box desktop`, with `--mcp` for `box mcp`.
    if name.as_deref() == Some("devbox-desktop") {
        let verb = match args.iter().any(|a| a == "--mcp") {
            true => vec!["mcp".to_owned()],
            false => vec!["desktop".to_owned()],
        };
        let rest = args.split_off(1).into_iter().filter(|a| a != "--mcp");
        args.extend(verb.into_iter().chain(rest));
    }
    if matches!(args.get(1).map(String::as_str), Some("desktop" | "mcp")) {
        enter_desktop_environment(&args);
    }
    args.remove(0);
    if args.is_empty() {
        return Err(USAGE.into());
    }
    let verb = args.remove(0);
    match verb.as_str() {
        "desktop" => desktop_cli(args)?,
        "mcp" => desktop::serve_mcp(),
        "exec" => {
            use std::os::unix::process::CommandExt;
            let command = args.iter().skip_while(|arg| *arg == "--");
            let error = std::process::Command::new(AGENT_ENV).args(command).exec();
            return Err(format!("{AGENT_ENV}: {error}"));
        }
        "help" | "--help" | "-h" => println!("{USAGE}"),
        _ => return net::run_verb(&verb, args),
    }
    Ok(0)
}

fn main() {
    let code = match real_main(std::env::args().collect()) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("{message}");
            1
        }
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn flags_are_removed() {
        let mut args = words(&["a", "--keep", "log", "b"]);
        assert_eq!(
            take_flag(&mut args, "--keep").unwrap().as_deref(),
            Some("log")
        );
        assert_eq!(args, words(&["a", "b"]));
        assert_eq!(take_flag(&mut args, "--keep").unwrap(), None);
        let mut args = words(&["--keep"]);
        assert!(take_flag(&mut args, "--keep").is_err());
    }

    #[test]
    fn assignments_split_on_first_equals() {
        let map = assignments(&words(&["a=1", "b=x=y", "c="])).unwrap();
        assert_eq!(Value::Object(map), json!({"a": "1", "b": "x=y", "c": ""}));
        assert!(assignments(&words(&["nope"])).is_err());
        assert!(assignments(&words(&["=v"])).is_err());
    }

    #[test]
    fn json_or_string_falls_back() {
        assert_eq!(json_or_string("{\"a\":1}\n"), json!({"a": 1}));
        assert_eq!(json_or_string("hello world\n"), json!("hello world"));
        assert_eq!(json_or_string(""), json!(""));
    }

    #[test]
    fn tool_args_from_words() {
        assert_eq!(
            tool_args("click", &words(&["3", "4"])).unwrap(),
            json!({"x": 3, "y": 4})
        );
        assert_eq!(
            tool_args("click", &words(&["3", "4", "2"])).unwrap()["button"],
            2
        );
        assert_eq!(tool_args("click", &words(&["a", "4"])).unwrap()["x"], "a");
        assert_eq!(
            tool_args("type", &words(&["a", "b"])).unwrap(),
            json!({"text": "a b"})
        );
        assert_eq!(
            tool_args("scroll", &words(&["up", "5"])).unwrap(),
            json!({"direction": "up", "steps": 5})
        );
        assert!(tool_args("click", &words(&["1"])).is_err());
    }
}
