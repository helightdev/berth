//! Private keys in OpenSSH's own format (`openssh-key-v1`), made and read
//! without ssh-keygen. ssh-keygen refuses to run for a user who has no entry
//! in the password database, as in a container, and berth only needs a new
//! ed25519 key and the public half of a key it is given.

use crate::Result;
use berth_api::{b64, unb64};

const BEGIN: &str = "-----BEGIN OPENSSH PRIVATE KEY-----";
const END: &str = "-----END OPENSSH PRIVATE KEY-----";
const MAGIC: &[u8] = b"openssh-key-v1\0";
const ED25519: &str = "ssh-ed25519";

/// A key's two halves as text: the private key file and the public line.
pub struct Made {
    pub private: String,
    pub public: String,
}

fn put(out: &mut Vec<u8>, field: &[u8]) {
    out.extend_from_slice(&(field.len() as u32).to_be_bytes());
    out.extend_from_slice(field);
}

/// A new ed25519 key with `comment`, as `ssh-keygen -t ed25519 -N ''` makes it.
pub fn generate(comment: &str) -> Result<Made> {
    let mut seed = [0u8; 32];
    std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut seed)?;
    let mut check = [0u8; 4];
    std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut check)?;
    Ok(from_seed(&seed, check, comment))
}

fn from_seed(seed: &[u8; 32], check: [u8; 4], comment: &str) -> Made {
    let signing = ed25519_dalek::SigningKey::from_bytes(seed);
    let public = signing.verifying_key().to_bytes();
    let mut blob = vec![];
    put(&mut blob, ED25519.as_bytes());
    put(&mut blob, &public);

    let mut secret = vec![];
    secret.extend_from_slice(&check);
    secret.extend_from_slice(&check);
    put(&mut secret, ED25519.as_bytes());
    put(&mut secret, &public);
    let mut pair = seed.to_vec();
    pair.extend_from_slice(&public);
    put(&mut secret, &pair);
    put(&mut secret, comment.as_bytes());
    // Padded to the cipher's block size, which is 8 for "none".
    let mut pad = 1u8;
    while !secret.len().is_multiple_of(8) {
        secret.push(pad);
        pad += 1;
    }

    let mut file = MAGIC.to_vec();
    put(&mut file, b"none");
    put(&mut file, b"none");
    put(&mut file, b"");
    file.extend_from_slice(&1u32.to_be_bytes());
    put(&mut file, &blob);
    put(&mut file, &secret);

    let encoded = b64(&file);
    let mut private = format!("{BEGIN}\n");
    for line in encoded.as_bytes().chunks(70) {
        private.push_str(std::str::from_utf8(line).unwrap_or_default());
        private.push('\n');
    }
    private.push_str(END);
    private.push('\n');
    let public = match comment {
        "" => format!("{ED25519} {}", b64(&blob)),
        comment => format!("{ED25519} {} {comment}", b64(&blob)),
    };
    Made { private, public }
}

/// Whether `text` is a private key in OpenSSH's own format.
pub fn is_openssh(text: &str) -> bool {
    text.contains(BEGIN)
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn bytes(&mut self, count: usize) -> std::result::Result<&'a [u8], &'static str> {
        if self.0.len() < count {
            return Err(SHORT);
        }
        let (taken, rest) = self.0.split_at(count);
        self.0 = rest;
        Ok(taken)
    }
    fn u32(&mut self) -> std::result::Result<u32, &'static str> {
        let bytes = self.bytes(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }
    fn string(&mut self) -> std::result::Result<&'a [u8], &'static str> {
        let count = self.u32()? as usize;
        self.bytes(count)
    }
}

const SHORT: &str = "That private key is cut short or damaged";

/// The public line (`TYPE BASE64 COMMENT`) of an OpenSSH private key, as
/// `ssh-keygen -y` writes it. The private half is checked against the public
/// one, fully for ed25519; a key with a passphrase is refused, since berth
/// logs in where nobody can type one.
pub fn public_of(text: &str) -> std::result::Result<String, &'static str> {
    let start = text
        .find(BEGIN)
        .ok_or("That is not an OpenSSH private key")?
        + BEGIN.len();
    let end = text[start..].find(END).ok_or(SHORT)? + start;
    let body: String = text[start..end].split_whitespace().collect();
    let file = unb64(&body).ok_or("That private key is not valid base64")?;
    let mut reader = Reader(
        file.strip_prefix(MAGIC)
            .ok_or("That is not an OpenSSH private key")?,
    );
    let cipher = reader.string()?;
    let kdf = reader.string()?;
    reader.string()?;
    if cipher != b"none" || kdf != b"none" {
        return Err("That key has a passphrase");
    }
    if reader.u32()? != 1 {
        return Err("That file holds more than one key. Add them one at a time");
    }
    let blob = reader.string()?;
    let mut secret = Reader(reader.string()?);
    // Two equal numbers start the private section; they differ only in a
    // damaged file, as cipher and kdf are none.
    if secret.u32()? != secret.u32()? {
        return Err(SHORT);
    }

    let mut public = Reader(blob);
    let kind = public.string()?;
    if secret.string()? != kind {
        return Err("The private and public halves of that key do not match");
    }
    let kind = std::str::from_utf8(kind).map_err(|_| "That key's type is not text")?;
    let mut fields = vec![];
    while !public.0.is_empty() {
        fields.push(public.string()?);
    }
    // The private section repeats the public fields first, except that RSA
    // has n before e where the public key has e before n.
    let mut repeated = vec![];
    for _ in 0..fields.len() {
        repeated.push(secret.string()?);
    }
    if kind == "ssh-rsa" && repeated.len() == 2 {
        repeated.swap(0, 1);
    }
    if fields.is_empty() || repeated != fields || kind.contains("-cert-") {
        return Err("The private and public halves of that key do not match");
    }
    if kind == ED25519 {
        let pair = secret.string()?;
        let seed: [u8; 32] = pair
            .get(..32)
            .and_then(|seed| seed.try_into().ok())
            .ok_or(SHORT)?;
        let derived = ed25519_dalek::SigningKey::from_bytes(&seed)
            .verifying_key()
            .to_bytes();
        if fields[0] != derived || pair.get(32..) != Some(&derived[..]) {
            return Err("The private and public halves of that key do not match");
        }
    } else {
        // The private fields of the other types: RSA's d, iqmp, p and q,
        // ECDSA's and DSA's one, a security key's flags, handle and reserved.
        let rest = match kind {
            "ssh-rsa" => 4,
            "ssh-dss" => 1,
            kind if kind.starts_with("ecdsa-sha2-") => 1,
            kind if kind.starts_with("sk-") => 0,
            _ => return Err("berth does not know that type of key"),
        };
        for _ in 0..rest {
            secret.string()?;
        }
        if kind.starts_with("sk-") {
            secret.bytes(1)?;
            secret.string()?;
            secret.string()?;
        }
    }
    let comment = String::from_utf8_lossy(secret.string()?).into_owned();
    let comment = comment.trim();
    let line = format!("{kind} {}", b64(blob));
    Ok(match comment {
        "" => line,
        comment if comment.chars().any(char::is_control) => line,
        comment => format!("{line} {comment}"),
    })
}

/// A real ed25519 key with fixed bytes, for tests: no randomness, no
/// ssh-keygen.
#[cfg(test)]
pub fn fixed(comment: &str) -> Made {
    from_seed(&[7; 32], [1, 2, 3, 4], comment)
}

/// A key file in OpenSSH's format from its parts, for tests of keys berth
/// does not make: `secret` is the private section without its padding.
#[cfg(test)]
pub fn assemble(cipher: &str, blob: &[u8], secret: &[u8]) -> String {
    let mut padded = secret.to_vec();
    let mut pad = 1u8;
    while !padded.len().is_multiple_of(8) {
        padded.push(pad);
        pad += 1;
    }
    let kdf: &[u8] = if cipher == "none" { b"none" } else { b"bcrypt" };
    let mut file = MAGIC.to_vec();
    put(&mut file, cipher.as_bytes());
    put(&mut file, kdf);
    put(&mut file, b"");
    file.extend_from_slice(&1u32.to_be_bytes());
    put(&mut file, blob);
    put(&mut file, &padded);
    format!("{BEGIN}\n{}\n{END}\n", b64(&file))
}

/// A key with a passphrase, as far as its format shows it.
#[cfg(test)]
pub fn locked() -> String {
    let made = fixed("made-up");
    let blob = unb64(made.public.split_whitespace().nth(1).unwrap()).unwrap();
    assemble("aes256-ctr", &blob, &[9; 48])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(kind: &str, values: &[&[u8]]) -> Vec<u8> {
        let mut out = vec![];
        put(&mut out, kind.as_bytes());
        for value in values {
            put(&mut out, value);
        }
        out
    }

    #[test]
    fn a_made_key_reads_back_with_its_public_half() {
        let made = generate("berth@studio").unwrap();
        assert!(made.private.starts_with(BEGIN) && made.private.ends_with(&format!("{END}\n")));
        assert!(
            made.public
                .starts_with("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA")
        );
        assert!(made.public.ends_with(" berth@studio"));
        assert_eq!(public_of(&made.private).unwrap(), made.public);
        assert!(made.private.lines().all(|line| line.len() <= 70));
        assert_ne!(generate("").unwrap().public, made.public);
        // The fixed key is a real one too.
        let fixed = fixed("made-up");
        assert_eq!(public_of(&fixed.private).unwrap(), fixed.public);
    }

    #[test]
    fn keys_of_other_types_give_their_public_half() {
        let (curve, point, scalar) = (b"nistp256".as_slice(), [4u8; 65], [5u8; 32]);
        let kind = "ecdsa-sha2-nistp256";
        let blob = fields(kind, &[curve, &point]);
        let mut secret = [6u8; 4].repeat(2);
        secret.extend(fields(kind, &[curve, &point, &scalar, b"made-up"]));
        let public = public_of(&assemble("none", &blob, &secret)).unwrap();
        assert_eq!(public, format!("{kind} {} made-up", b64(&blob)));

        // RSA's private section has n before e.
        let (e, n) = ([1u8, 0, 1].as_slice(), [8u8; 129]);
        let blob = fields("ssh-rsa", &[e, &n]);
        let mut secret = [6u8; 4].repeat(2);
        let private: &[&[u8]] = &[&n, e, &[1; 128], &[2; 64], &[3; 65], &[4; 65], b""];
        secret.extend(fields("ssh-rsa", private));
        let public = public_of(&assemble("none", &blob, &secret)).unwrap();
        assert_eq!(public, format!("ssh-rsa {}", b64(&blob)));
        // In the order of the public key it does not match.
        let mut swapped = [6u8; 4].repeat(2);
        swapped.extend(fields(
            "ssh-rsa",
            &[e, &n, &[1; 128], &[2; 64], &[3; 65], &[4; 65], b""],
        ));
        assert!(public_of(&assemble("none", &blob, &swapped)).is_err());
    }

    #[test]
    fn damaged_keys_and_mismatched_halves_are_refused() {
        let made = fixed("made-up");
        let blob = unb64(made.public.split_whitespace().nth(1).unwrap()).unwrap();
        // The private half of another key under this public half.
        let other = from_seed(&[8; 32], [0; 4], "");
        let theirs = unb64(other.public.split_whitespace().nth(1).unwrap()).unwrap();
        let pk = &theirs[theirs.len() - 32..];
        let mut pair = [8u8; 32].to_vec();
        pair.extend_from_slice(pk);
        let mut secret = [0u8; 4].repeat(2);
        secret.extend(fields(ED25519, &[pk, &pair, b""]));
        assert!(public_of(&assemble("none", &blob, &secret)).is_err());
        // Check numbers that differ: a wrong passphrase, or a damaged file.
        let mut secret = vec![1, 2, 3, 4, 4, 3, 2, 1];
        secret.extend(fields(ED25519, &[pk, &pair, b""]));
        assert!(public_of(&assemble("none", &blob, &secret)).is_err());
        assert!(public_of(&format!("{BEGIN}\nAAAA\n{END}\n")).is_err());
        assert!(public_of("not a key").is_err());
        let said = public_of(&locked()).unwrap_err();
        assert!(said.contains("passphrase"), "{said}");
    }
}
