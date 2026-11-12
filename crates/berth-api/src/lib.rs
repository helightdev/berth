//! What goes over the wire between berth, berthd and box: the types, the
//! table of streams, and a client that works over any byte stream.

#[cfg(feature = "client")]
pub mod client;
pub mod escape;
pub mod ops;
#[cfg(feature = "client")]
pub mod portal;
pub mod streams;
mod types;

pub use types::*;

/// The version of the programs.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Rises when something is added to the API.
pub const API: u32 = 1;

/// Where berthd listens in system mode.
pub const RUN_DIR: &str = "/run/berth";
pub const CONTROL_SOCKET: &str = "control.sock";
pub const AGENT_SOCKET: &str = "agent.sock";

/// Standard base64, for the `_b64` fields.
pub fn b64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        for (index, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if index <= chunk.len() {
                out.push(TABLE[(n >> shift & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

pub fn unb64(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let (mut bits, mut count) = (0u32, 0);
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' | b'\n' | b'\r' => continue,
            _ => return None,
        };
        bits = bits << 6 | value as u32;
        count += 1;
        if count == 4 {
            out.extend_from_slice(&bits.to_be_bytes()[1..]);
            (bits, count) = (0, 0);
        }
    }
    match count {
        0 => {}
        2 => out.push((bits >> 4) as u8),
        3 => out.extend_from_slice(&((bits >> 2) as u16).to_be_bytes()),
        _ => return None,
    }
    Some(out)
}

/// Seconds since 1970 as RFC 3339, UTC.
pub fn rfc3339(seconds: u64) -> String {
    let days = seconds / 86400;
    let (hour, minute, second) = (seconds % 86400 / 3600, seconds % 3600 / 60, seconds % 60);
    // Civil date from days, after Howard Hinnant.
    let z = days as i64 + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

pub fn now() -> String {
    rfc3339(unix_now())
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|time| time.as_secs())
        .unwrap_or(0)
}

/// `90`, `90s`, `5m`, `2h`, `7d` as seconds.
pub fn duration(text: &str) -> Option<u64> {
    let (number, unit) = match text.find(|ch: char| !ch.is_ascii_digit()) {
        Some(at) => text.split_at(at),
        None => (text, "s"),
    };
    let scale = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => return None,
    };
    number.parse::<u64>().ok()?.checked_mul(scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_both_ways() {
        for text in ["", "a", "ab", "abc", "abcd", "\0\u{ff}\n"] {
            assert_eq!(unb64(&b64(text.as_bytes())).unwrap(), text.as_bytes());
        }
        assert_eq!(b64(b"hi"), "aGk=");
        assert!(unb64("a").is_none());
    }

    #[test]
    fn times_and_durations() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_794_441_600), "2026-11-12T00:00:00Z");
        assert_eq!(duration("5m"), Some(300));
        assert_eq!(duration("12"), Some(12));
        assert_eq!(duration("1w"), None);
    }
}
