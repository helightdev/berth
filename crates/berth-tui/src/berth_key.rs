//! The SSH keys of berth's key ring: a row each on the Settings tab, what
//! the Information box says of one, and copying a public half from there.

use crate::screen::Platform;
use crate::setup::short_key;
use crate::{Code, Key, theme};
use ratatui::{
    style::Style,
    text::{Line, Span},
};

pub const LABEL: &str = "This berth's SSH key";

/// The label of a key of the ring other than berth's own.
fn label(name: &str) -> String {
    match name {
        "berth" => LABEL.to_owned(),
        name => format!("SSH key {name}"),
    }
}

/// A key that copies it, besides Enter.
pub fn is_copy(key: &Key) -> bool {
    key.code == Code::Char('c') && !key.ctrl && !key.alt
}

/// The row of a key of the ring on the Settings tab.
pub fn row(name: &str, public: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{}  ", label(name)), Style::new().fg(theme::TEXT)),
        Span::styled(short_key(public), Style::new().fg(theme::ACCENT)),
    ])
}

/// The Information box of a key's row on the Settings tab. `server` is the
/// berth server whose key it is, for a berth that uses one.
pub fn help(name: &str, public: &str, server: Option<&str>) -> String {
    let whose = match (name, server) {
        ("berth", Some(server)) => format!(
            "The SSH key of the berth server at {server}. The server sets its hosts up and reaches them, so this is the key they take."
        ),
        ("berth", None) => "The SSH key berth made for itself on this machine. A host that logs in with berth gets this key, and no other.".to_owned(),
        (name, Some(server)) => format!(
            "The key {name} of the key ring of the berth server at {server}. Hosts whose login is key:{name} are logged in to with it."
        ),
        (name, None) => format!(
            "The key {name} of berth's key ring. Hosts whose login is key:{name} are logged in to with it."
        ),
    };
    let more = match name {
        "berth" => {
            " The operator of every host set up from here takes it too, always, so berth can always reach the hosts it set up."
        }
        _ => "",
    };
    format!(
        "{whose}\n\nAdd it as an authorized key at your cloud provider, or to `~/.ssh/authorized_keys` on a host, and berth gets in without a key of yours; `berth key trust HOST` does that with a password used once.{more} The private half stays with berth: `berth key` lists the keys.\n\nEnter copies it.\n\n`{public}`"
    )
}

/// Put the key on the clipboard. Returns what to say.
pub fn copy(platform: &mut dyn Platform, public: &str) -> String {
    if public.is_empty() {
        return "The key of this berth is not known".into();
    }
    match platform.copy(public) {
        true => "Copied this berth's SSH public key".into(),
        false => "Could not reach the clipboard".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Clipboard {
        text: Option<String>,
        reachable: bool,
    }

    impl Platform for Clipboard {
        fn copy(&mut self, text: &str) -> bool {
            if self.reachable {
                self.text = Some(text.to_owned());
            }
            self.reachable
        }

        fn paste(&mut self) -> Option<String> {
            self.text.clone()
        }

        fn open(&mut self, _: &str) -> Result<(), String> {
            Err("no browser".into())
        }
    }

    const PUBLIC: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExampleKeyOfBerth1234 berth@studio";

    #[test]
    fn the_whole_key_is_copied_and_a_short_one_shown() {
        let mut clipboard = Clipboard {
            reachable: true,
            ..Clipboard::default()
        };
        assert_eq!(
            copy(&mut clipboard, PUBLIC),
            "Copied this berth's SSH public key"
        );
        assert_eq!(clipboard.text.as_deref(), Some(PUBLIC));
        let shown: String = row("berth", PUBLIC)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(shown.starts_with(LABEL) && shown.contains("…erth1234 berth@studio"));
        assert!(!shown.contains(PUBLIC));
        assert!(help("berth", PUBLIC, None).contains(PUBLIC));
        assert!(help("berth", PUBLIC, Some("https://192.0.2.7:8443")).contains("192.0.2.7"));
        assert!(help("deploy", PUBLIC, None).contains("key:deploy"));
        let other: String = row("deploy", PUBLIC)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(other.starts_with("SSH key deploy"));
    }

    #[test]
    fn nothing_is_copied_without_a_key_or_a_clipboard() {
        let mut clipboard = Clipboard::default();
        assert_eq!(
            copy(&mut clipboard, PUBLIC),
            "Could not reach the clipboard"
        );
        clipboard.reachable = true;
        assert_eq!(
            copy(&mut clipboard, ""),
            "The key of this berth is not known"
        );
        assert!(clipboard.text.is_none());
        assert!(is_copy(&Key::plain(Code::Char('c'))));
        assert!(!is_copy(&Key::plain(Code::Char(' '))));
    }
}
