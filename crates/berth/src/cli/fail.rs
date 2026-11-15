//! Why a command did not do what it was asked, as one sentence and the exit
//! status that says what kind of failure it was.

use berth_api::client::Error;

/// The work failed: a job failed, a host did not answer.
pub const FAILED: i32 = 1;
/// The command line was wrong: an unknown word, a missing value, a bad flag.
pub const USAGE: i32 = 2;
/// Refused: the role is too low, it is not available with this server, or
/// a confirmation was declined.
pub const REFUSED: i32 = 3;
/// The server could not be reached.
pub const UNREACHABLE: i32 = 4;

#[derive(Debug, Clone, PartialEq)]
pub struct Fail {
    pub status: i32,
    /// The API's error code, or berth's own word for the kind of failure.
    pub code: String,
    /// One sentence with what to do next; empty when there is nothing to
    /// say, as for a command on a host that exits with its own status.
    pub message: String,
}

pub type Result<T> = std::result::Result<T, Fail>;

impl Fail {
    fn with(status: i32, code: &str, message: impl Into<String>) -> Fail {
        Fail {
            status,
            code: code.to_owned(),
            message: message.into(),
        }
    }

    pub fn failed(message: impl Into<String>) -> Fail {
        Fail::with(FAILED, "failed", message)
    }

    pub fn usage(message: impl Into<String>) -> Fail {
        Fail::with(USAGE, "usage", message)
    }

    pub fn refused(message: impl Into<String>) -> Fail {
        Fail::with(REFUSED, "refused", message)
    }

    pub fn unreachable(message: impl Into<String>) -> Fail {
        Fail::with(UNREACHABLE, "unreachable", message)
    }

    /// An exit status passed through with nothing to say, such as that of a
    /// command run on a host.
    pub fn exit(status: i32) -> Fail {
        Fail::with(status, "exit", "")
    }

    /// The same failure, said with more in front.
    pub fn context(mut self, before: &str) -> Fail {
        if !self.message.is_empty() {
            self.message = format!("{before}: {}", self.message);
        }
        self
    }
}

/// The exit status an error of the API stands for.
pub fn status_of(status: u16) -> i32 {
    match status {
        400 | 412 | 413 => USAGE,
        401 | 403 | 501 => REFUSED,
        503 => UNREACHABLE,
        _ => FAILED,
    }
}

impl From<Error> for Fail {
    fn from(error: Error) -> Fail {
        match error {
            Error::Api {
                status,
                code,
                message,
            } => Fail {
                status: status_of(status),
                code,
                message,
            },
            Error::Transport(message) => Fail::unreachable(message),
        }
    }
}

impl From<std::io::Error> for Fail {
    fn from(error: std::io::Error) -> Fail {
        Fail::failed(error.to_string())
    }
}

impl From<serde_json::Error> for Fail {
    fn from(error: serde_json::Error) -> Fail {
        Fail::failed(error.to_string())
    }
}

impl From<String> for Fail {
    fn from(message: String) -> Fail {
        Fail::failed(message)
    }
}

impl From<&str> for Fail {
    fn from(message: &str) -> Fail {
        Fail::failed(message)
    }
}

impl From<Box<dyn std::error::Error>> for Fail {
    fn from(error: Box<dyn std::error::Error>) -> Fail {
        Fail::failed(error.to_string())
    }
}

impl From<clap::Error> for Fail {
    fn from(error: clap::Error) -> Fail {
        Fail::usage(error.to_string().trim().trim_start_matches("error: "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_of_the_api_have_the_status_of_their_kind() {
        let api = |status, code: &str| Fail::from(Error::api((status, code), "said"));
        assert_eq!(api(400, "bad_request").status, USAGE);
        assert_eq!(api(403, "forbidden").status, REFUSED);
        assert_eq!(api(501, "not_available").status, REFUSED);
        assert_eq!(api(502, "unreachable").status, FAILED);
        assert_eq!(api(404, "not_found").code, "not_found");
        let lost = Fail::from(Error::Transport("refused".into()));
        assert_eq!(lost.status, UNREACHABLE);
    }
}
