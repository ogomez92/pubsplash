//! User-scoped encryption for credentials kept in the settings file.
//!
//! Speech engines need API keys, and a plaintext `config.json` is a key anyone
//! who copies the file can use. The ciphertext is tied to the signed-in user
//! account, so a copied settings file is inert on another machine — which is
//! the realistic threat here, not a determined local attacker (Pubsplash runs
//! as the user and can always decrypt its own secrets).
//!
//! [`Secret`] is the serde-facing type. It writes `enc:<base64>` and reads
//! either that or a bare string, so settings written by an older build — or
//! hand-edited by a user — still load.
//!
//! The platform seam is exactly two functions, [`protect`] and [`unprotect`],
//! which take and return bare bytes. Everything above them — the serde
//! impls, the `enc:` marker, the legacy-plaintext migration, the
//! never-print `Debug` — is portable and shared.
//!
//! Windows has a call for precisely this shape, DPAPI's `CryptProtectData`, so
//! there the seam is a thin wrapper. macOS does not: the Keychain stores
//! *named items*, not opaque blobs, and `Secret` does not know its own name —
//! it is a field type, and the same type is used for a dozen different
//! credentials. So the macOS side keeps one random master key **in** the
//! Keychain and encrypts each value with it, which reproduces DPAPI's actual
//! property: self-contained ciphertext in `config.json` that is worthless
//! without this login account.

use crate::b64;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Marks a value this module encrypted, so a plaintext value written by an
/// older build is distinguishable from ciphertext.
const PREFIX: &str = "enc:";

/// A credential that is encrypted at rest.
///
/// `Debug` is implemented by hand: these end up inside `Config`, which is
/// logged and dumped in a few places, and a derived `Debug` would put every
/// API key in the log file.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.is_empty() {
            f.write_str("Secret(empty)")
        } else {
            f.write_str("Secret(set)")
        }
    }
}

impl Serialize for Secret {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.0.is_empty() {
            return serializer.serialize_str("");
        }
        match protect(self.0.as_bytes()) {
            Some(bytes) => serializer.serialize_str(&format!("{PREFIX}{}", b64::encode(&bytes))),
            // Losing the user's typed key because the platform keystore was
            // unavailable would be worse than storing it as-is; the next load
            // reads it back as plaintext.
            None => {
                log::warn!("Could not encrypt a stored credential; writing it unencrypted");
                serializer.serialize_str(&self.0)
            }
        }
    }
}

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        let Some(encoded) = raw.strip_prefix(PREFIX) else {
            // Plaintext from an older build, or hand-edited. Take it; the next
            // save re-writes it encrypted.
            return Ok(Self(raw));
        };
        let plain = b64::decode(encoded)
            .and_then(|bytes| unprotect(&bytes))
            .and_then(|bytes| String::from_utf8(bytes).ok());
        match plain {
            Some(value) => Ok(Self(value)),
            None => {
                // Encrypted under a different user account, or corrupt.
                // Returning an error here would fail the whole settings load.
                log::warn!("A stored credential could not be decrypted; treating it as unset");
                Ok(Self::default())
            }
        }
    }
}

/// The seam. Exactly one `imp` below is compiled in.
use imp::{protect, unprotect};

/// DPAPI. `CryptProtectData` ties the ciphertext to the Windows user account
/// and needs no key management of our own.
#[cfg(windows)]
mod imp {
    use windows::Win32::Foundation::LocalFree;
    use windows::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };

    pub fn protect(plain: &[u8]) -> Option<Vec<u8>> {
        unsafe {
            let input = CRYPT_INTEGER_BLOB {
                cbData: plain.len() as u32,
                pbData: plain.as_ptr() as *mut u8,
            };
            let mut output = CRYPT_INTEGER_BLOB::default();
            CryptProtectData(
                &input,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
            .ok()?;
            Some(take_blob(&mut output))
        }
    }

    pub fn unprotect(cipher: &[u8]) -> Option<Vec<u8>> {
        unsafe {
            let input = CRYPT_INTEGER_BLOB {
                cbData: cipher.len() as u32,
                pbData: cipher.as_ptr() as *mut u8,
            };
            let mut output = CRYPT_INTEGER_BLOB::default();
            CryptUnprotectData(
                &input,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
            .ok()?;
            Some(take_blob(&mut output))
        }
    }

    /// Copies a blob DPAPI allocated and frees it. DPAPI hands back
    /// `LocalAlloc` memory that the caller owns.
    unsafe fn take_blob(blob: &mut CRYPT_INTEGER_BLOB) -> Vec<u8> {
        unsafe {
            let bytes = std::slice::from_raw_parts(blob.pbData, blob.cbData as usize).to_vec();
            let _ = LocalFree(Some(windows::Win32::Foundation::HLOCAL(
                blob.pbData as *mut _,
            )));
            blob.pbData = std::ptr::null_mut();
            blob.cbData = 0;
            bytes
        }
    }
}

/// AES-256-GCM under a master key held in the login Keychain.
///
/// The key is created on first use and never leaves this account: the Keychain
/// item is not marked synchronizable, so it does not ride iCloud to another
/// Mac, and a settings file copied anywhere else decrypts to nothing — the same
/// end state DPAPI produces on Windows.
///
/// `aes-gcm` is already in the tree for the sound-pack format, and `rand` for
/// the nonces there, so this adds no cryptographic dependency — only
/// `security-framework` for the Keychain itself.
#[cfg(target_os = "macos")]
mod imp {
    use aes_gcm::{Aes256Gcm, KeyInit, Nonce, aead::Aead};
    use rand::{RngCore, rngs::OsRng};
    use std::sync::OnceLock;

    /// What the Keychain item is filed under. Neither string carries a version
    /// or a path: every copy of Pubsplash this user runs must find the same key,
    /// or one of them would be unable to read the other's settings file.
    const SERVICE: &str = "Pubsplash";
    const ACCOUNT: &str = "settings-encryption-key";

    /// AES-GCM's standard nonce width, and the prefix length of every blob this
    /// module writes.
    const NONCE_LEN: usize = 12;

    /// Read once per process. A Keychain lookup is a round trip to
    /// `securityd`, and `Secret` is deserialized once per credential in the
    /// settings file — a dozen of them on a normal load.
    ///
    /// `None` is cached too, and deliberately: if the Keychain is unavailable
    /// (locked, or denied) the answer will not change within this run, and the
    /// callers above already handle a `None` by keeping the value in plaintext
    /// rather than losing it.
    fn key() -> Option<&'static [u8; 32]> {
        static KEY: OnceLock<Option<[u8; 32]>> = OnceLock::new();
        KEY.get_or_init(load_or_create_key).as_ref()
    }

    fn load_or_create_key() -> Option<[u8; 32]> {
        use security_framework::passwords::{get_generic_password, set_generic_password};

        if let Ok(stored) = get_generic_password(SERVICE, ACCOUNT) {
            if let Ok(key) = <[u8; 32]>::try_from(stored.as_slice()) {
                return Some(key);
            }
            // Present but the wrong length -- something else wrote it, or an
            // older format did. Replacing it would silently orphan every
            // credential already encrypted under whatever this is, so leave it
            // alone and let the callers fall back to plaintext.
            log::warn!(
                "The Pubsplash Keychain item is not a 256-bit key; stored credentials cannot be encrypted"
            );
            return None;
        }

        let mut key = [0u8; 32];
        OsRng.fill_bytes(&mut key);
        match set_generic_password(SERVICE, ACCOUNT, &key) {
            Ok(()) => {
                log::info!("Created the Pubsplash Keychain key for stored credentials");
                Some(key)
            }
            Err(e) => {
                log::warn!("Could not create a Keychain key for stored credentials: {e}");
                None
            }
        }
    }

    pub fn protect(plain: &[u8]) -> Option<Vec<u8>> {
        let cipher = Aes256Gcm::new_from_slice(key()?).ok()?;
        let mut nonce = [0u8; NONCE_LEN];
        OsRng.fill_bytes(&mut nonce);
        let sealed = cipher.encrypt(Nonce::from_slice(&nonce), plain).ok()?;
        // Nonce first, then ciphertext-and-tag. It is not a secret; it only has
        // to be unique per message under the same key, which a fresh 96 bits
        // from the OS is.
        let mut out = Vec::with_capacity(NONCE_LEN + sealed.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&sealed);
        Some(out)
    }

    pub fn unprotect(blob: &[u8]) -> Option<Vec<u8>> {
        if blob.len() <= NONCE_LEN {
            return None;
        }
        let cipher = Aes256Gcm::new_from_slice(key()?).ok()?;
        let (nonce, sealed) = blob.split_at(NONCE_LEN);
        // A wrong key fails the GCM tag check here rather than returning
        // garbage, which is what lets the caller report "could not be
        // decrypted" and treat it as unset.
        cipher.decrypt(Nonce::from_slice(nonce), sealed).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_round_trips_through_json() {
        let secret = Secret::new("sk-a-test-api-key");
        let json = serde_json::to_string(&secret).unwrap();
        assert!(
            json.contains(PREFIX) && !json.contains("sk-a-test-api-key"),
            "the key leaked into the serialized form: {json}"
        );
        let back: Secret = serde_json::from_str(&json).unwrap();
        assert_eq!(back.as_str(), "sk-a-test-api-key");
    }

    /// Settings files written before credentials were encrypted must still load.
    #[test]
    fn plaintext_values_still_load() {
        let back: Secret = serde_json::from_str("\"plain-key\"").unwrap();
        assert_eq!(back.as_str(), "plain-key");
    }

    /// A key encrypted by another user account must not fail the whole load.
    #[test]
    fn undecryptable_values_read_as_unset() {
        let back: Secret = serde_json::from_str("\"enc:bm90LWEtcmVhbC1ibG9i\"").unwrap();
        assert!(back.is_empty());
    }

    #[test]
    fn empty_secrets_stay_empty_and_unencrypted() {
        let json = serde_json::to_string(&Secret::default()).unwrap();
        assert_eq!(json, "\"\"");
    }

    #[test]
    fn debug_never_prints_the_value() {
        let rendered = format!("{:?}", Secret::new("sk-secret"));
        assert!(!rendered.contains("sk-secret"), "{rendered}");
    }
}
