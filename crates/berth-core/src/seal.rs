//! Encrypts single values before they go into the database. The key lives in
//! a private file; an installation without one stores values plain.

use std::fmt;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use sha2::{Digest, Sha256};

const MAGIC: &[u8; 6] = b"berth1";
const ID_LEN: usize = 8;
const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;
const HEADER_LEN: usize = MAGIC.len() + ID_LEN + NONCE_LEN;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// No key with this id is known.
    NoKey([u8; 8]),
    /// The bytes are not a sealed record, or they do not open.
    Damaged,
    /// A key file that is unreadable, has the wrong mode or is not 64 hex digits.
    Key(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoKey(id) => write!(f, "no key with id {} to open this value", hex(id)),
            Error::Damaged => f.write_str("the value is not a sealed record or does not open"),
            Error::Key(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// Bytes as lower-case hex digits, as key ids are shown.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn random(buf: &mut [u8]) -> std::io::Result<()> {
    fs::File::open("/dev/urandom")?.read_exact(buf)
}

/// A 32-byte key. It never shows up in `Debug` or `Display`.
pub struct Key([u8; 32]);

impl Key {
    pub fn generate() -> Result<Key> {
        let mut bytes = [0; 32];
        random(&mut bytes).map_err(|e| Error::Key(format!("cannot read /dev/urandom: {e}")))?;
        Ok(Key(bytes))
    }

    /// 64 hex digits, with whitespace around them allowed.
    pub fn from_hex(text: &str) -> Result<Key> {
        let text = text.trim().as_bytes();
        if text.len() != 64 {
            return Err(Error::Key("a key is 64 hex digits".into()));
        }
        let mut bytes = [0; 32];
        for (out, pair) in bytes.iter_mut().zip(text.chunks(2)) {
            let digit = |c: u8| (c as char).to_digit(16);
            match (digit(pair[0]), digit(pair[1])) {
                (Some(high), Some(low)) => *out = (high * 16 + low) as u8,
                _ => return Err(Error::Key("a key is 64 hex digits".into())),
            }
        }
        Ok(Key(bytes))
    }

    pub fn to_hex(&self) -> String {
        hex(&self.0)
    }

    /// The first 8 bytes of the SHA-256 of the key: tells keys apart without giving anything away.
    pub fn id(&self) -> [u8; 8] {
        let digest = Sha256::digest(self.0);
        let mut id = [0; ID_LEN];
        id.copy_from_slice(&digest[..ID_LEN]);
        id
    }

    /// `None` when the file does not exist.
    pub fn read(path: &Path) -> Result<Option<Key>> {
        let shown = path.display();
        let mut file = match fs::File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::Key(format!("cannot read key file {shown}: {e}"))),
        };
        let mode = file
            .metadata()
            .map_err(|e| Error::Key(format!("cannot read key file {shown}: {e}")))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            return Err(Error::Key(format!(
                "key file {shown} can be read by others: chmod 600 it"
            )));
        }
        let mut text = String::new();
        file.read_to_string(&mut text)
            .map_err(|e| Error::Key(format!("cannot read key file {shown}: {e}")))?;
        Key::from_hex(&text)
            .map(Some)
            .map_err(|_| Error::Key(format!("key file {shown} does not hold 64 hex digits")))
    }

    /// Writes the hex and a newline to a new file with mode 0600; fails if the file exists.
    pub fn write_new(&self, path: &Path) -> Result<()> {
        let shown = path.display();
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| Error::Key(format!("cannot create key file {shown}: {e}")))?;
        writeln!(file, "{}", self.to_hex())
            .map_err(|e| Error::Key(format!("cannot write key file {shown}: {e}")))
    }

    fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new((&self.0).into())
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        // Volatile writes, so the compiler does not drop the clearing as dead.
        for byte in self.0.iter_mut() {
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
    }
}

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Key({})", hex(&self.id()))
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "key {}", hex(&self.id()))
    }
}

/// The keys an installation has: the one to seal with, and any others that may still open old values.
pub struct Keys {
    current: Key,
    others: Vec<Key>,
}

impl Keys {
    pub fn new(current: Key, others: Vec<Key>) -> Keys {
        Keys { current, others }
    }

    pub fn current(&self) -> &Key {
        &self.current
    }

    /// `context` binds the value to its place as associated data.
    pub fn seal(&self, context: &[u8], plain: &[u8]) -> Result<Vec<u8>> {
        let mut nonce = [0; NONCE_LEN];
        random(&mut nonce).map_err(|e| Error::Key(format!("cannot read /dev/urandom: {e}")))?;
        seal_with(&self.current, &nonce, context, plain)
    }

    /// Opens with whichever key has the id in the record.
    pub fn open(&self, context: &[u8], sealed: &[u8]) -> Result<Vec<u8>> {
        let id = key_id(sealed).ok_or(Error::Damaged)?;
        let key = std::iter::once(&self.current)
            .chain(&self.others)
            .find(|key| key.id() == id)
            .ok_or(Error::NoKey(id))?;
        let nonce = &sealed[MAGIC.len() + ID_LEN..HEADER_LEN];
        key.cipher()
            .decrypt(
                XNonce::from_slice(nonce),
                Payload {
                    msg: &sealed[HEADER_LEN..],
                    aad: context,
                },
            )
            .map_err(|_| Error::Damaged)
    }
}

fn seal_with(key: &Key, nonce: &[u8; NONCE_LEN], context: &[u8], plain: &[u8]) -> Result<Vec<u8>> {
    let body = key
        .cipher()
        .encrypt(
            XNonce::from_slice(nonce),
            Payload {
                msg: plain,
                aad: context,
            },
        )
        .map_err(|_| Error::Damaged)?;
    let mut out = Vec::with_capacity(HEADER_LEN + body.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&key.id());
    out.extend_from_slice(nonce);
    out.extend_from_slice(&body);
    Ok(out)
}

/// The id of the key a sealed record was made with, or `None` if the bytes are not a sealed record.
pub fn key_id(sealed: &[u8]) -> Option<[u8; 8]> {
    if sealed.len() < HEADER_LEN + TAG_LEN || !sealed.starts_with(MAGIC) {
        return None;
    }
    sealed[MAGIC.len()..MAGIC.len() + ID_LEN].try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> Keys {
        Keys::new(Key::generate().unwrap(), vec![])
    }

    #[test]
    fn round_trip_and_fresh_nonce() {
        let keys = keys();
        let a = keys.seal(b"ctx", b"secret").unwrap();
        let b = keys.seal(b"ctx", b"secret").unwrap();
        assert_ne!(a, b);
        assert_eq!(keys.open(b"ctx", &a).unwrap(), b"secret");
        assert_eq!(keys.open(b"ctx", &b).unwrap(), b"secret");
        let empty = keys.seal(b"", b"").unwrap();
        assert_eq!(keys.open(b"", &empty).unwrap(), b"");
    }

    #[test]
    fn open_refuses_damage() {
        let keys = keys();
        let sealed = keys.seal(b"ctx", b"secret").unwrap();
        assert_eq!(keys.open(b"other", &sealed), Err(Error::Damaged));
        // Nonce and ciphertext, and the tag at the end.
        for at in [
            HEADER_LEN - 1,
            MAGIC.len() + ID_LEN,
            HEADER_LEN,
            sealed.len() - 1,
        ] {
            let mut bad = sealed.clone();
            bad[at] ^= 1;
            assert_eq!(keys.open(b"ctx", &bad), Err(Error::Damaged), "byte {at}");
        }
        let mut bad = sealed.clone();
        bad[MAGIC.len()] ^= 1;
        assert!(matches!(keys.open(b"ctx", &bad), Err(Error::NoKey(_))));
        let mut bad = sealed.clone();
        bad[0] ^= 1;
        assert_eq!(keys.open(b"ctx", &bad), Err(Error::Damaged));
        assert_eq!(
            keys.open(b"ctx", &sealed[..HEADER_LEN]),
            Err(Error::Damaged)
        );
        assert_eq!(keys.open(b"ctx", b""), Err(Error::Damaged));
        assert_eq!(keys.open(b"ctx", b"plain text"), Err(Error::Damaged));
        assert_eq!(key_id(b"plain text"), None);
    }

    #[test]
    fn old_key_opens_and_new_key_seals() {
        let old = Key::generate().unwrap();
        let new = Key::generate().unwrap();
        let sealed = Keys::new(Key::from_hex(&old.to_hex()).unwrap(), vec![])
            .seal(b"ctx", b"v")
            .unwrap();
        assert_eq!(key_id(&sealed), Some(old.id()));
        let new_id = new.id();
        let keys = Keys::new(new, vec![old]);
        assert_eq!(keys.open(b"ctx", &sealed).unwrap(), b"v");
        let again = keys.seal(b"ctx", b"v").unwrap();
        assert_eq!(key_id(&again), Some(new_id));
        assert_eq!(keys.current().id(), new_id);
    }

    #[test]
    fn missing_key() {
        let sealed = keys().seal(b"ctx", b"v").unwrap();
        let id = key_id(&sealed).unwrap();
        let err = keys().open(b"ctx", &sealed).unwrap_err();
        assert_eq!(err, Error::NoKey(id));
        assert!(err.to_string().contains(&hex(&id)));
    }

    #[test]
    fn key_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        assert!(Key::read(&path).unwrap().is_none());
        let key = Key::generate().unwrap();
        key.write_new(&path).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(key.write_new(&path).is_err());
        assert_eq!(Key::read(&path).unwrap().unwrap().to_hex(), key.to_hex());

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let err = Key::read(&path).unwrap_err().to_string();
        assert!(err.contains("chmod 600") && err.contains(&path.display().to_string()));
        assert!(!err.contains(&key.to_hex()));

        let bad = dir.path().join("bad");
        fs::write(&bad, "not hex\n").unwrap();
        fs::set_permissions(&bad, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(Key::read(&bad), Err(Error::Key(_))));
        assert!(Key::from_hex(&"zz".repeat(32)).is_err());
        assert!(Key::from_hex(&format!(" {}\n", key.to_hex())).is_ok());
    }

    #[test]
    fn debug_hides_the_key() {
        let key = Key::generate().unwrap();
        assert!(!format!("{key:?}").contains(&key.to_hex()));
        assert!(!format!("{key}").contains(&key.to_hex()));
    }

    #[test]
    fn fixed_record() {
        let key = Key::from_hex(&"00112233445566778899aabbccddeeff".repeat(2)).unwrap();
        let nonce = [7u8; NONCE_LEN];
        let sealed = seal_with(&key, &nonce, b"kind\0scope\0name", b"hunter2").unwrap();
        assert_eq!(
            hex(&sealed),
            "6265727468314773d12e2371bb930707070707070707070707070707070707070707070707076dcebca35d44a81fd226a44ea2e4e896a0001c718ed8d7"
        );
        let keys = Keys::new(key, vec![]);
        assert_eq!(
            keys.open(b"kind\0scope\0name", &sealed).unwrap(),
            b"hunter2"
        );
    }
}
