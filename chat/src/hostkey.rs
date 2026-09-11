//! Who the broadcaster is, without a database.
//!
//! A room's host key is `HMAC-SHA256(server secret, room name)`, truncated and
//! base32-encoded. That one line is what lets a deployment serve any number of
//! rooms with no registration, no accounts table and no state to back up: the
//! operator runs `pubsplash-chat key myshow`, pastes the result into Pubsplash,
//! and the server can verify it forever after by recomputing it. Nothing is
//! stored per room, so a room costs nothing until somebody speaks in it.
//!
//! The security argument is the same as any bearer token's: the key is as good
//! as the identity, so it travels over TLS (put this behind a reverse proxy) and
//! it is checked in constant time. It authorises exactly one thing today --
//! marking a message as the host's -- which keeps the blast radius of a leak
//! small, and is why re-keying is simply "change the secret, hand out new keys".
//!
//! Base32 (RFC 4648, no padding) rather than hex or base64 because the key gets
//! read aloud and typed: it has no case distinction to lose, and none of
//! base64's `+/=` to mangle in a URL or a shell.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Characters in a key. 20 base32 characters is 100 bits, which is far past
/// what an online guessing attack against a rate-limited endpoint can reach,
/// and still short enough to dictate over the air.
const KEY_CHARS: usize = 20;

const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// The server secret every room key is derived from.
#[derive(Clone)]
pub struct Secret(Vec<u8>);

impl Secret {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// The host key for one room.
    pub fn key_for(&self, room: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(&self.0).expect("HMAC takes a key of any length");
        mac.update(room.as_bytes());
        base32(&mac.finalize().into_bytes())
    }

    /// Whether `presented` is this room's key.
    ///
    /// Constant time in the comparison, and case-insensitive in the input: the
    /// key is a thing people type, and rejecting `abc` when the key is `ABC`
    /// would be an authentication failure that looks exactly like a wrong key.
    pub fn verifies(&self, room: &str, presented: &str) -> bool {
        let expected = self.key_for(room);
        let presented = presented.trim().to_ascii_uppercase();
        constant_time_eq(expected.as_bytes(), presented.as_bytes())
    }
}

/// Compares two byte strings without returning early on the first difference.
///
/// A plain `==` on a token leaks its prefix through timing, one byte at a time,
/// which turns 100 bits of key into a few thousand requests.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    // The lengths are not secret (the key length is published here), so
    // comparing them first is fine; the *contents* are what must not branch.
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// RFC 4648 base32 of the first bytes of `data`, truncated to [`KEY_CHARS`].
fn base32(data: &[u8]) -> String {
    let mut out = String::with_capacity(KEY_CHARS);
    let mut buffer: u32 = 0;
    let mut bits = 0u32;
    for byte in data {
        buffer = (buffer << 8) | u32::from(*byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            let index = ((buffer >> bits) & 0x1f) as usize;
            out.push(ALPHABET[index] as char);
            if out.len() == KEY_CHARS {
                return out;
            }
        }
    }
    out
}

/// Pulls the bearer token out of an `Authorization` header.
///
/// Returns `None` for anything that is not `Bearer <token>`, so a header of
/// some other scheme is an absent key rather than a rejected one -- the message
/// still goes through, just not as the host.
pub fn bearer(header: Option<&str>) -> Option<&str> {
    let value = header?.trim();
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then_some(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret() -> Secret {
        Secret::new(b"a fixed secret for tests".to_vec())
    }

    #[test]
    fn a_key_is_stable_for_its_room_and_different_for_another() {
        let s = secret();
        assert_eq!(s.key_for("myshow"), s.key_for("myshow"));
        assert_ne!(s.key_for("myshow"), s.key_for("othershow"));
        assert_eq!(s.key_for("myshow").len(), KEY_CHARS);
    }

    #[test]
    fn keys_are_typeable_and_readable_aloud() {
        let key = secret().key_for("myshow");
        assert!(
            key.bytes().all(|b| ALPHABET.contains(&b)),
            "base32 alphabet only, got {key}"
        );
    }

    #[test]
    fn a_different_secret_means_different_keys() {
        let a = Secret::new(b"one".to_vec());
        let b = Secret::new(b"two".to_vec());
        assert_ne!(a.key_for("myshow"), b.key_for("myshow"));
        assert!(!b.verifies("myshow", &a.key_for("myshow")));
    }

    #[test]
    fn verification_tolerates_case_and_surrounding_space() {
        let s = secret();
        let key = s.key_for("myshow");
        assert!(s.verifies("myshow", &key));
        assert!(s.verifies("myshow", &key.to_lowercase()));
        assert!(s.verifies("myshow", &format!("  {key}  ")));
        assert!(!s.verifies("myshow", ""));
        assert!(!s.verifies("myshow", "AAAAAAAAAAAAAAAAAAAA"));
        // A key really is per room: the right key for the wrong room fails.
        assert!(!s.verifies("othershow", &key));
    }

    #[test]
    fn bearer_reads_only_its_own_scheme() {
        assert_eq!(bearer(Some("Bearer ABC123")), Some("ABC123"));
        assert_eq!(bearer(Some("bearer ABC123")), Some("ABC123"));
        assert_eq!(bearer(Some("Basic ABC123")), None);
        assert_eq!(bearer(Some("Bearer   ")), None);
        assert_eq!(bearer(Some("ABC123")), None);
        assert_eq!(bearer(None), None);
    }
}
