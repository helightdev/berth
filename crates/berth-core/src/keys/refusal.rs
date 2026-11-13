//! Logins a host refused. sshd counts failed logins per source address, and
//! OpenSSH 10 turns that address away for a while once there were a few
//! (`PerSourcePenalties`, on by default), so a berth that keeps trying a
//! host that does not take its key shuts itself out of that host and of
//! every other one behind the same address.
//!
//! berth therefore remembers a refusal beside the host, with the login it
//! refused, and nothing berth does by itself (the server's channel, the
//! host list, the overview opening) logs in with that login again for
//! [`BACKOFF`]. Whatever the user asks for tries once. A changed profile or
//! key is another login, and a login that works forgets the refusal.

use super::known::Record;
use crate::Result;
use crate::paths::Paths;
use crate::profiles::Place;
use serde::{Deserialize, Serialize};

/// How long nothing automatic tries a login a host refused.
pub const BACKOFF: u64 = 30 * 60;

/// How long nothing automatic tries a host whose sshd turned this address away.
pub const PENALTY_WAIT: u64 = 2 * 60;

/// A refused login, kept in the host's row of kind `access`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    /// When, in seconds since 1970.
    pub at: u64,
    /// Which login: [`super::Login::identity`].
    pub login: String,
    /// What ssh said.
    pub why: String,
    /// Whether sshd turned the address away, rather than refusing the login.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub penalty: bool,
}

impl Refusal {
    fn lasts(&self) -> u64 {
        match self.penalty {
            true => PENALTY_WAIT,
            false => BACKOFF,
        }
    }

    /// Whether it still keeps automatic logins away at `now`.
    pub fn holds(&self, now: u64) -> bool {
        now < self.at.saturating_add(self.lasts())
    }

    /// Why a login is not tried by itself, for whoever sees the host.
    pub fn sentence(&self, name: &str, now: u64) -> String {
        let left = self.at.saturating_add(self.lasts()).saturating_sub(now);
        let minutes = left.div_ceil(60).max(1);
        match self.penalty {
            true => format!(
                "{} berth tries {name} again by itself in about {minutes} min, or when you ask",
                penalty_sentence(name)
            ),
            false => format!(
                "{name} did not take berth's login ({}). berth does not try it again by itself for about {minutes} min: Try again, or install the key with: berth key trust {name}",
                self.why.trim_end_matches('.')
            ),
        }
    }
}

/// What ssh's words say of a failed login.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// The host does not take the login.
    Refused,
    /// The host's sshd hung up during the key exchange, as it does for an
    /// address it penalises.
    Penalised,
    /// Anything else, such as a host that does not answer.
    Other,
}

/// Tell the failures apart by what ssh wrote.
pub fn classify(said: &str) -> Failure {
    if said.contains("Permission denied (") || said.contains("Too many authentication failures") {
        Failure::Refused
    } else if said.contains("kex_exchange_identification")
        || said.contains("banner exchange")
        || (said.contains("Connection reset by peer") && said.contains("kex"))
    {
        Failure::Penalised
    } else {
        Failure::Other
    }
}

/// What a connection that sshd dropped during the key exchange means.
pub fn penalty_sentence(name: &str) -> String {
    format!(
        "{name}'s sshd closed the connection before the login: it turns an address away for a while after failed logins from it (OpenSSH's PerSourcePenalties). Wait a minute and try again; after many failed logins it can take up to 10 minutes"
    )
}

/// Whether a sentence is [`penalty_sentence`]'s.
pub fn is_penalty(text: &str) -> bool {
    text.contains("PerSourcePenalties")
}

/// Seconds since 1970.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

/// Where a login's refusals are kept: the installation's store, beside the
/// host, and which login it is.
#[derive(Debug, Clone)]
pub struct Guard {
    pub paths: Paths,
    pub place: Place,
    pub login: String,
    pub name: String,
}

impl Guard {
    /// The refusal that keeps automatic logins away now, if one does.
    pub fn holds(&self) -> Option<Refusal> {
        let store = crate::store::shared(&self.paths).ok()?;
        let refused = Record::load(&store, &self.place).ok()?.refused?;
        (refused.login == self.login && refused.holds(now())).then_some(refused)
    }

    /// [`Guard::holds`] as the sentence to show.
    pub fn refused(&self) -> Option<String> {
        self.holds()
            .map(|refusal| refusal.sentence(&self.name, now()))
    }

    /// Note a failed login by what ssh said. Returns what to say instead of
    /// ssh's words, when that is clearer.
    pub fn failed(&self, said: &str) -> Option<String> {
        let failure = classify(said);
        let penalty = match failure {
            Failure::Other => return None,
            Failure::Penalised => true,
            Failure::Refused => false,
        };
        let _ = self.note(Refusal {
            at: now(),
            login: self.login.clone(),
            why: said.lines().last().unwrap_or(said).trim().to_owned(),
            penalty,
        });
        match failure {
            Failure::Penalised => Some(penalty_sentence(&self.name)),
            _ => None,
        }
    }

    fn note(&self, refusal: Refusal) -> Result<()> {
        let store = crate::store::shared(&self.paths)?;
        let mut record = Record::load(&store, &self.place)?;
        record.refused = Some(refusal);
        record.save(&store, &self.place)
    }

    /// A login worked: forget what was refused before.
    pub fn worked(&self) {
        let Ok(store) = crate::store::shared(&self.paths) else {
            return;
        };
        if let Ok(mut record) = Record::load(&store, &self.place)
            && record.refused.is_some()
        {
            record.refused = None;
            let _ = record.save(&store, &self.place);
        }
    }
}

/// Forget a host's refusal, as when the user asks for a login again.
pub fn forget(paths: &Paths, place: &Place) -> Result<()> {
    let store = crate::store::shared(paths)?;
    let mut record = Record::load(&store, place)?;
    if record.refused.take().is_some() {
        record.save(&store, place)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: dir.path().join("config"),
            data_dir: dir.path().join("data"),
        };
        (dir, paths)
    }

    #[test]
    fn what_ssh_said_is_a_refusal_a_penalty_or_neither() {
        assert_eq!(
            classify("ops@192.0.2.10: Permission denied (publickey)."),
            Failure::Refused
        );
        for penalised in [
            "kex_exchange_identification: read: Connection reset by peer\r\nConnection reset by 192.0.2.10 port 22",
            "kex_exchange_identification: Connection closed by remote host",
            "banner exchange: Connection to 192.0.2.10 port 22: Connection reset by peer",
        ] {
            assert_eq!(classify(penalised), Failure::Penalised, "{penalised}");
        }
        for other in [
            "ssh: connect to host 192.0.2.10 port 22: Connection refused",
            "ssh: connect to host 192.0.2.10 port 22: Connection timed out",
            "bash: berthd: command not found",
        ] {
            assert_eq!(classify(other), Failure::Other, "{other}");
        }
        let said = penalty_sentence("studio");
        assert!(said.contains("PerSourcePenalties") && is_penalty(&said));
        assert!(said.contains("Wait a minute"));
    }

    #[test]
    fn a_refused_login_waits_for_the_back_off_and_another_login_does_not() {
        let (_dir, paths) = scratch();
        let place = Place::own("studio").unwrap();
        let guard = Guard {
            paths: paths.clone(),
            place: place.clone(),
            login: "one".into(),
            name: "studio".into(),
        };
        assert!(guard.holds().is_none());
        assert_eq!(
            guard.failed("ssh: connect to host studio port 22: No route to host"),
            None
        );
        assert!(guard.holds().is_none());
        assert_eq!(
            guard.failed("ops@studio: Permission denied (publickey)."),
            None
        );
        let refused = guard.holds().unwrap();
        assert!(!refused.penalty);
        let said = guard.refused().unwrap();
        assert!(
            said.contains("berth key trust studio") && said.contains("30 min"),
            "{said}"
        );
        // Another key or address is another login, and is tried.
        let other = Guard {
            login: "two".into(),
            ..guard.clone()
        };
        assert!(other.holds().is_none());
        // Once the back-off passed it is tried again.
        assert!(refused.holds(refused.at + BACKOFF - 1));
        assert!(!refused.holds(refused.at + BACKOFF));
        // A login that works forgets it; so does the user asking.
        guard.worked();
        assert!(guard.holds().is_none());
        guard.failed("Permission denied (publickey)");
        forget(&paths, &place).unwrap();
        assert!(guard.holds().is_none());
    }

    #[test]
    fn a_penalty_is_explained_and_waits_a_short_while() {
        let (_dir, paths) = scratch();
        let guard = Guard {
            paths,
            place: Place::own("studio").unwrap(),
            login: "one".into(),
            name: "studio".into(),
        };
        let said = guard
            .failed("kex_exchange_identification: read: Connection reset by peer")
            .unwrap();
        assert!(is_penalty(&said));
        let held = guard.holds().unwrap();
        assert!(held.penalty);
        assert!(!held.holds(held.at + PENALTY_WAIT));
        assert!(guard.refused().unwrap().contains("PerSourcePenalties"));
    }
}
