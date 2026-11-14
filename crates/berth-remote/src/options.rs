//! What berth-remote is started with: its options, and from the environment
//! a token or a setup key, which never go on a command line.

use berth_page::device::Linked;

pub const USAGE: &str = "\
berth-remote: the hosts of a berth serve, in this terminal

Usage: berth-remote [--server ADDRESS] [--forget]

  --server ADDRESS   the berth serve to use, such as https://berth.example
                     or http://192.0.2.10:7878 (also BERTH_SERVER)
  --forget           remove the kept server and token, and keep nothing
                     from this run
  --version          say the version

The token comes from BERTH_TOKEN, or is asked for. A setup key from
`berth setup-key` comes from BERTH_SETUP_KEY. Neither goes on the command
line, where other users of the machine could read it.

A plugin package is chosen in the file dialog of the system: on Linux that
of zenity or kdialog. Without one, BERTH_REMOTE_FILE names the file.

The server and the token are kept for this user: in
%LOCALAPPDATA%\\berth-remote on Windows, ~/Library/Application Support/berth-remote
on macOS and ~/.config/berth-remote elsewhere ($XDG_CONFIG_HOME where it is set).

Ctrl+Q quits.";

pub struct Options {
    pub linked: Linked,
    /// Remove what was kept, and keep nothing.
    pub forget: bool,
}

pub enum Parsed {
    Run(Options),
    Version,
    Help,
}

/// Read the arguments, and the variables `var` gives. An argument that is
/// not an option is not repeated in the error, since it could be a token.
pub fn parse(
    args: impl IntoIterator<Item = String>,
    var: impl Fn(&str) -> Option<String>,
) -> Result<Parsed, String> {
    let variable = |name: &str| {
        var(name)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    };
    let mut options = Options {
        linked: Linked {
            server: variable("BERTH_SERVER"),
            token: variable("BERTH_TOKEN"),
            setup_key: variable("BERTH_SETUP_KEY"),
        },
        forget: false,
    };
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let (name, value) = match arg.split_once('=') {
            Some((name, value)) if name.starts_with("--") => (name, Some(value.to_owned())),
            _ => (arg.as_str(), None),
        };
        match name {
            "--server" | "-s" => {
                let server = value
                    .or_else(|| args.next())
                    .filter(|server| !server.trim().is_empty())
                    .ok_or("--server needs an address")?;
                options.linked.server = Some(server.trim().to_owned());
            }
            "--forget" if value.is_none() => options.forget = true,
            "--version" | "-V" if value.is_none() => return Ok(Parsed::Version),
            "--help" | "-h" if value.is_none() => return Ok(Parsed::Help),
            "--forget" | "--version" | "--help" => return Err(format!("{name} takes no value")),
            other if other.starts_with('-') => {
                return Err(format!("{other} is no option of berth-remote"));
            }
            _ => {
                return Err(
                    "berth-remote takes no arguments but its options: a token comes from BERTH_TOKEN"
                        .into(),
                );
            }
        }
    }
    Ok(Parsed::Run(options))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str], vars: &[(&str, &str)]) -> Result<Options, String> {
        let vars: Vec<(String, String)> = vars
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        let parsed = parse(args.iter().map(|arg| arg.to_string()), |name| {
            vars.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        })?;
        match parsed {
            Parsed::Run(options) => Ok(options),
            _ => Err("not a run".into()),
        }
    }

    #[test]
    fn the_server_comes_from_an_option_or_the_environment() {
        let options = run(&[], &[("BERTH_SERVER", " https://studio.example ")]).unwrap();
        assert_eq!(
            options.linked.server.as_deref(),
            Some("https://studio.example")
        );
        assert!(!options.forget);
        let options = run(
            &["--server", "http://192.0.2.10:7878"],
            &[("BERTH_SERVER", "https://studio.example")],
        )
        .unwrap();
        assert_eq!(
            options.linked.server.as_deref(),
            Some("http://192.0.2.10:7878")
        );
        let options = run(&["--server=https://dock.example", "--forget"], &[]).unwrap();
        assert_eq!(
            options.linked.server.as_deref(),
            Some("https://dock.example")
        );
        assert!(options.forget);
        assert!(run(&["--server"], &[]).is_err());
        assert!(run(&["--server="], &[]).is_err());
    }

    #[test]
    fn secrets_come_from_the_environment_only() {
        let options = run(
            &[],
            &[("BERTH_TOKEN", "the-token\n"), ("BERTH_SETUP_KEY", "")],
        )
        .unwrap();
        assert_eq!(options.linked.token.as_deref(), Some("the-token"));
        assert_eq!(options.linked.setup_key, None);
        // A token given as an argument is refused, and not repeated.
        let error = run(&["s3cr3t-token"], &[]).err().unwrap();
        assert!(!error.contains("s3cr3t"), "{error}");
        let error = run(&["--token=s3cr3t-token"], &[]).err().unwrap();
        assert!(!error.contains("s3cr3t"), "{error}");
        assert!(error.contains("--token"), "{error}");
    }

    #[test]
    fn version_and_help() {
        let parsed = |args: &[&str]| parse(args.iter().map(|arg| arg.to_string()), |_| None);
        assert!(matches!(parsed(&["--version"]), Ok(Parsed::Version)));
        assert!(matches!(parsed(&["-V"]), Ok(Parsed::Version)));
        assert!(matches!(parsed(&["--help"]), Ok(Parsed::Help)));
        assert!(parsed(&["--forget=yes"]).is_err());
    }
}
