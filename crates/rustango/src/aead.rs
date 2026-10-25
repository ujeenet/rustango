//! XChaCha20-Poly1305 under a key derived from `RUSTANGO_SECRET_KEY`.
//!
//! Shared by the `EncryptedString` cast and sealed tenant secrets, so the
//! two cannot drift. Stored form: base64(`nonce(24) ‖ ciphertext+tag`).

use base64::Engine as _;
use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{Key, KeyInit, XChaCha20Poly1305, XNonce};
use sha2::{Digest, Sha256};

/// 192-bit XChaCha nonce: random per write, so reuse is not a concern.
const NONCE_LEN: usize = 24;

/// The environment variable the key is derived from.
pub(crate) const KEY_VAR: &str = "RUSTANGO_SECRET_KEY";

/// Why sealing or opening failed. Callers word their own message.
#[derive(Debug)]
pub(crate) enum AeadError {
    MissingKey,
    Encrypt(String),
    Decode(String),
    Short,
    Decrypt,
}

/// The current key. Read per call so key rotation and tests see the live value.
fn env_key() -> Option<String> {
    std::env::var(KEY_VAR).ok()
}

fn cipher(secret: Option<&str>) -> Result<XChaCha20Poly1305, AeadError> {
    // SHA-256 of the secret, so any length works.
    let digest = Sha256::digest(secret.ok_or(AeadError::MissingKey)?.as_bytes());
    Ok(XChaCha20Poly1305::new(Key::from_slice(&digest)))
}

/// Encrypt `plaintext`, binding `aad` (empty for the cast).
pub(crate) fn encrypt(plaintext: &[u8], aad: &[u8]) -> Result<String, AeadError> {
    encrypt_with(env_key().as_deref(), plaintext, aad)
}

/// Open what [`encrypt`] produced under the same `aad`.
pub(crate) fn decrypt(stored: &str, aad: &[u8]) -> Result<Vec<u8>, AeadError> {
    decrypt_with(env_key().as_deref(), stored, aad)
}

fn encrypt_with(secret: Option<&str>, plaintext: &[u8], aad: &[u8]) -> Result<String, AeadError> {
    let cipher = cipher(secret)?;
    let mut nonce = [0u8; NONCE_LEN];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut nonce);
    let ciphertext = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|e| AeadError::Encrypt(e.to_string()))?;
    let mut framed = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    framed.extend_from_slice(&nonce);
    framed.extend_from_slice(&ciphertext);
    Ok(base64::engine::general_purpose::STANDARD.encode(framed))
}

fn decrypt_with(secret: Option<&str>, stored: &str, aad: &[u8]) -> Result<Vec<u8>, AeadError> {
    let framed = base64::engine::general_purpose::STANDARD
        .decode(stored)
        .map_err(|e| AeadError::Decode(e.to_string()))?;
    if framed.len() < NONCE_LEN {
        return Err(AeadError::Short);
    }
    let (nonce, ciphertext) = framed.split_at(NONCE_LEN);
    cipher(secret)?
        .decrypt(
            XNonce::from_slice(nonce),
            Payload {
                msg: ciphertext,
                aad,
            },
        )
        .map_err(|_| AeadError::Decrypt)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: Option<&str> = Some("aead-unit-test-key");

    /// Ciphertext written before this module existed (no AAD) still opens.
    #[test]
    fn empty_aad_matches_the_plain_slice_api() {
        let digest = Sha256::digest(KEY.unwrap().as_bytes());
        let cipher = XChaCha20Poly1305::new(Key::from_slice(&digest));
        let nonce = [7u8; NONCE_LEN];
        let ct = cipher
            .encrypt(XNonce::from_slice(&nonce), b"legacy".as_slice())
            .unwrap();
        let framed = [nonce.as_slice(), &ct].concat();
        let stored = base64::engine::general_purpose::STANDARD.encode(framed);
        assert_eq!(decrypt_with(KEY, &stored, b"").unwrap(), b"legacy");
    }

    #[test]
    fn a_different_aad_does_not_open() {
        let stored = encrypt_with(KEY, b"x", b"one").unwrap();
        assert!(matches!(
            decrypt_with(KEY, &stored, b"two"),
            Err(AeadError::Decrypt)
        ));
        assert_eq!(decrypt_with(KEY, &stored, b"one").unwrap(), b"x");
    }

    #[test]
    fn a_missing_key_is_its_own_error() {
        assert!(matches!(
            encrypt_with(None, b"x", b""),
            Err(AeadError::MissingKey)
        ));
        let stored = encrypt_with(KEY, b"x", b"").unwrap();
        assert!(matches!(
            decrypt_with(None, &stored, b""),
            Err(AeadError::MissingKey)
        ));
    }
}
