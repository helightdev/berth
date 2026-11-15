//! The ids of what berth keeps: a UUID of version 7 as 32 lowercase hex
//! characters. Wherever an id is taken, it may carry the prefix of its kind,
//! such as `prov_`, which is dropped; replies give it bare.

/// The kinds of things with an id, each with the prefix its ids may carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Host,
    Provisioner,
    Resource,
    Registry,
    Secret,
    Key,
}

impl Kind {
    pub const ALL: [Kind; 6] = [
        Kind::Host,
        Kind::Provisioner,
        Kind::Resource,
        Kind::Registry,
        Kind::Secret,
        Kind::Key,
    ];

    /// The prefix an id of this kind may be given with, `_` included.
    pub fn prefix(self) -> &'static str {
        match self {
            Kind::Host => "host_",
            Kind::Provisioner => "prov_",
            Kind::Resource => "res_",
            Kind::Registry => "reg_",
            Kind::Secret => "sec_",
            Kind::Key => "key_",
        }
    }

    /// What people call one, for messages.
    pub fn noun(self) -> &'static str {
        match self {
            Kind::Host => "host",
            Kind::Provisioner => "provisioner",
            Kind::Resource => "resource",
            Kind::Registry => "registry",
            Kind::Secret => "secret",
            Kind::Key => "key",
        }
    }
}

/// How long an id is.
pub const LENGTH: usize = 32;

/// Whether a text is an id in its canonical form.
pub fn is_id(text: &str) -> bool {
    text.len() == LENGTH
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// An id in its canonical form, from the bare id in either case or the
/// hyphenated form of a UUID.
fn canonical(text: &str) -> Option<String> {
    let bare: String = match text.len() {
        36 => {
            let bytes = text.as_bytes();
            if [8, 13, 18, 23].iter().any(|&at| bytes[at] != b'-') {
                return None;
            }
            text.chars().filter(|&ch| ch != '-').collect()
        }
        _ => text.to_owned(),
    };
    let bare = bare.to_ascii_lowercase();
    is_id(&bare).then_some(bare)
}

/// Whether a name looks like an id, with or without a prefix, so that it
/// cannot be given as one.
pub fn looks_like_id(name: &str) -> bool {
    let hex = |text: &str| text.len() == LENGTH && text.bytes().all(|b| b.is_ascii_hexdigit());
    hex(name)
        || Kind::ALL.iter().any(|kind| {
            name.get(..kind.prefix().len())
                .is_some_and(|start| start.eq_ignore_ascii_case(kind.prefix()))
                && hex(&name[kind.prefix().len()..])
        })
}

/// What a value given for a thing of `kind` is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Given {
    /// An id with the prefix of `kind`: only that id, never a name.
    Prefixed(String),
    /// A bare id: that id, if there is a thing with it, else a name.
    Bare(String),
    /// A name.
    Name(String),
}

/// Read a value given for a thing of `kind`. A prefix of another kind is refused.
pub fn read(kind: Kind, given: &str) -> Result<Given, String> {
    for other in Kind::ALL {
        let Some(rest) = given
            .get(..other.prefix().len())
            .filter(|start| start.eq_ignore_ascii_case(other.prefix()))
            .map(|_| &given[other.prefix().len()..])
        else {
            continue;
        };
        let Some(id) = canonical(rest) else {
            continue;
        };
        if other != kind {
            return Err(format!(
                "{given} is the id of a {}, and a {} was asked for",
                other.noun(),
                kind.noun()
            ));
        }
        return Ok(Given::Prefixed(id));
    }
    Ok(match canonical(given) {
        Some(id) => Given::Bare(id),
        None => Given::Name(given.to_owned()),
    })
}

/// The id a value gives when it is one, bare or with the prefix of `kind`.
pub fn id_in(kind: Kind, given: &str) -> Option<String> {
    match read(kind, given) {
        Ok(Given::Prefixed(id) | Given::Bare(id)) => Some(id),
        _ => None,
    }
}

/// A new id from the time in milliseconds and 10 random bytes: a UUID of
/// version 7, which sorts in the order the ids were made.
pub fn from_parts(millis: u64, random: [u8; 10]) -> String {
    let mut bytes = [0u8; 16];
    bytes[..6].copy_from_slice(&millis.to_be_bytes()[2..]);
    bytes[6..].copy_from_slice(&random);
    bytes[6] = 0x70 | (bytes[6] & 0x0f);
    bytes[8] = 0x80 | (bytes[8] & 0x3f);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "01972c4e8f3a7b219d4e5a6b7c8d9e0f";

    #[test]
    fn an_id_is_32_lowercase_hex_characters_of_a_version_7_uuid() {
        let id = from_parts(0x0197_2c4e_8f3a, [0xff; 10]);
        assert!(is_id(&id), "{id}");
        assert_eq!(&id[..12], "01972c4e8f3a");
        assert_eq!(&id[12..13], "7");
        assert!("89ab".contains(&id[16..17]));
        assert!(!is_id(&ID.to_uppercase()) && !is_id("r-k3x9p2qa"));
    }

    #[test]
    fn a_value_is_an_id_with_its_prefix_a_bare_id_or_a_name() {
        let prefixed = format!("prov_{ID}");
        assert_eq!(
            read(Kind::Provisioner, &prefixed),
            Ok(Given::Prefixed(ID.into()))
        );
        assert_eq!(
            read(Kind::Provisioner, &format!("PROV_{}", ID.to_uppercase())),
            Ok(Given::Prefixed(ID.into()))
        );
        assert_eq!(read(Kind::Host, ID), Ok(Given::Bare(ID.into())));
        assert_eq!(
            read(Kind::Host, "01972c4e-8f3a-7b21-9d4e-5a6b7c8d9e0f"),
            Ok(Given::Bare(ID.into()))
        );
        assert_eq!(read(Kind::Host, "studio"), Ok(Given::Name("studio".into())));
        // A prefix of another kind is refused.
        let error = read(Kind::Host, &prefixed).unwrap_err();
        assert!(error.contains("provisioner"), "{error}");
        // A prefix without an id after it is a name.
        assert_eq!(
            read(Kind::Host, "host_studio"),
            Ok(Given::Name("host_studio".into()))
        );
        assert_eq!(id_in(Kind::Secret, &format!("sec_{ID}")), Some(ID.into()));
        assert_eq!(id_in(Kind::Secret, "github_token"), None);
    }

    #[test]
    fn names_in_the_shape_of_an_id_are_told_apart() {
        assert!(looks_like_id(ID));
        assert!(looks_like_id(&ID.to_uppercase()));
        assert!(looks_like_id(&format!("prov_{ID}")));
        assert!(looks_like_id(&format!("Host_{ID}")));
        for name in [
            "studio",
            "incus-vm",
            &ID[1..],
            "abcdefabcdefabcdefabcdefabcdefag",
        ] {
            assert!(!looks_like_id(name), "{name}");
        }
    }
}
