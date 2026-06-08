//! `EncryptedString` — the reference [`CastValue`] built-in (#819).
//! Encrypts at rest with XChaCha20-Poly1305 (AEAD), key derived from the
//! `RUSTANGO_SECRET_KEY` environment variable.
//!
//! Stored form: base64(`nonce(24) ‖ ciphertext+tag`). The 24-byte
//! XChaCha nonce is random per write, so nonce reuse isn't a concern even
//! across many encryptions under one key.

use base64::Engine as _;
use chacha20poly1305::aead::Aead;
use chacha20poly1305::{Key, KeyInit, XChaCha20Poly1305, XNonce};
use sha2::{Digest, Sha256};

use super::{CastError, CastValue};

/// 192-bit XChaCha nonce.
const NONCE_LEN: usize = 24;

/// Marker type for the encrypted-string cast — use as
/// [`Cast<EncryptedString>`](super::Cast). The logical value is a
/// `String`; the column stores AEAD ciphertext.
pub enum EncryptedString {}

impl CastValue for EncryptedString {
    type Value = String;

    fn to_db(value: &String) -> String {
        // Infallible by contract; a missing key is a deploy-time
        // misconfiguration and fails fast (like a missing DB URL).
        encrypt(value.as_bytes()).unwrap_or_else(|e| {
            panic!("encrypted cast write failed: {e}");
        })
    }

    fn from_db(stored: &str) -> Result<String, CastError> {
        let plaintext = decrypt(stored)?;
        String::from_utf8(plaintext)
            .map_err(|e| CastError(format!("decrypted bytes not UTF-8: {e}")))
    }
}

/// 32-byte key = SHA-256 of `RUSTANGO_SECRET_KEY` (accepts any-length
/// secret). Read per call so tests / key rotation see the current value.
fn derive_key() -> Result<[u8; 32], CastError> {
    let secret = std::env::var("RUSTANGO_SECRET_KEY").map_err(|_| {
        CastError("RUSTANGO_SECRET_KEY is not set (required for `EncryptedString` casts)".into())
    })?;
    let digest = Sha256::digest(secret.as_bytes());
    let mut key = [0u8; 32];
    key.copy_from_slice(&digest);
    Ok(key)
}

fn cipher() -> Result<XChaCha20Poly1305, CastError> {
    let key = derive_key()?;
    Ok(XChaCha20Poly1305::new(Key::from_slice(&key)))
}

fn encrypt(plaintext: &[u8]) -> Result<String, CastError> {
    let cipher = cipher()?;
    let mut nonce = [0u8; NONCE_LEN];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut nonce);
    let ciphertext = cipher
        .encrypt(XNonce::from_slice(&nonce), plaintext)
        .map_err(|e| CastError(format!("encrypt: {e}")))?;
    let mut framed = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    framed.extend_from_slice(&nonce);
    framed.extend_from_slice(&ciphertext);
    Ok(base64::engine::general_purpose::STANDARD.encode(framed))
}

fn decrypt(stored: &str) -> Result<Vec<u8>, CastError> {
    let framed = base64::engine::general_purpose::STANDARD
        .decode(stored)
        .map_err(|e| CastError(format!("base64 decode: {e}")))?;
    if framed.len() < NONCE_LEN {
        return Err(CastError("ciphertext shorter than nonce".into()));
    }
    let (nonce, ciphertext) = framed.split_at(NONCE_LEN);
    let cipher = cipher()?;
    cipher
        .decrypt(XNonce::from_slice(nonce), ciphertext)
        .map_err(|_| CastError("decryption failed (wrong key or tampered data)".into()))
}

#[cfg(test)]
mod tests {
    use super::super::{Cast, CastValue};
    use super::EncryptedString;

    /// Set a key for the duration of these (serial) unit tests.
    fn with_key() {
        // SAFETY: single-threaded unit test process for this module.
        std::env::set_var("RUSTANGO_SECRET_KEY", "unit-test-secret-key");
    }

    #[test]
    fn round_trips_plaintext() {
        with_key();
        let stored = EncryptedString::to_db(&"hello world".to_owned());
        assert_ne!(stored, "hello world", "stored form must be ciphertext");
        let back = EncryptedString::from_db(&stored).unwrap();
        assert_eq!(back, "hello world");
    }

    #[test]
    fn distinct_nonce_per_encryption() {
        with_key();
        let a = EncryptedString::to_db(&"same".to_owned());
        let b = EncryptedString::to_db(&"same".to_owned());
        assert_ne!(a, b, "random nonce → ciphertext differs each write");
        assert_eq!(EncryptedString::from_db(&a).unwrap(), "same");
        assert_eq!(EncryptedString::from_db(&b).unwrap(), "same");
    }

    #[test]
    fn tampered_ciphertext_fails() {
        with_key();
        let mut stored = EncryptedString::to_db(&"secret".to_owned());
        // Flip a character in the base64 body.
        stored.replace_range(30..31, if &stored[30..31] == "A" { "B" } else { "A" });
        assert!(EncryptedString::from_db(&stored).is_err());
    }

    #[test]
    fn cast_wrapper_into_sqlvalue_is_ciphertext_string() {
        with_key();
        let c: Cast<EncryptedString> = Cast::new("x".to_owned());
        let v: crate::core::SqlValue = c.into();
        match v {
            crate::core::SqlValue::String(s) => assert_ne!(s, "x"),
            other => panic!("expected SqlValue::String, got {other:?}"),
        }
    }
}
