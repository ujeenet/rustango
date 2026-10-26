//! `EncryptedString` — the reference [`CastValue`] built-in (#819).
//! Encrypts at rest with XChaCha20-Poly1305 (AEAD), key derived from the
//! `RUSTANGO_SECRET_KEY` environment variable.
//!
//! Stored form: base64(`nonce(24) ‖ ciphertext+tag`). The 24-byte
//! XChaCha nonce is random per write, so nonce reuse isn't a concern even
//! across many encryptions under one key.

use crate::aead::AeadError;

use super::{CastError, CastValue};

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

/// The cast's wording for each failure; unchanged since the code moved.
fn cast_error(e: AeadError) -> CastError {
    CastError(match e {
        AeadError::MissingKey => {
            "RUSTANGO_SECRET_KEY is not set (required for `EncryptedString` casts)".into()
        }
        AeadError::Encrypt(e) => format!("encrypt: {e}"),
        AeadError::Decode(e) => format!("base64 decode: {e}"),
        AeadError::Short => "ciphertext shorter than nonce".into(),
        AeadError::Decrypt => "decryption failed (wrong key or tampered data)".into(),
    })
}

/// No AAD: rows written before AEAD was shared must still open.
pub(crate) fn encrypt(plaintext: &[u8]) -> Result<String, CastError> {
    crate::aead::encrypt(plaintext, b"").map_err(cast_error)
}

fn decrypt(stored: &str) -> Result<Vec<u8>, CastError> {
    crate::aead::decrypt(stored, b"").map_err(cast_error)
}

#[cfg(test)]
mod tests {
    use super::super::{Cast, CastValue};
    use super::EncryptedString;

    /// Set a key for the duration of these (serial) unit tests.
    fn with_key() {
        // SAFETY: single-threaded unit test process for this module.
        crate::aead::set_test_key();
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

    /// Mid-rotation, a value written under the old key still opens.
    #[test]
    fn the_previous_key_opens_old_values() {
        let old = crate::aead::encrypt_with(Some("old-key"), b"client-secret", b"").unwrap();
        let open = |cur, prev| crate::aead::decrypt_either(cur, prev, &old, b"");
        assert_eq!(
            open(Some("new-key"), Some("old-key")).unwrap(),
            b"client-secret"
        );
        assert!(open(Some("new-key"), None).is_err());
        assert!(open(Some("new-key"), Some("wrong")).is_err());
    }

    /// The cast itself opens a value sealed under the previous key.
    #[test]
    fn the_cast_opens_a_value_under_the_previous_key() {
        with_key();
        let old = crate::aead::encrypt_with(Some("rotated-out"), b"client-secret", b"").unwrap();
        assert!(EncryptedString::from_db(&old).is_err());
        std::env::set_var(crate::aead::PREVIOUS_KEY_VAR, "rotated-out");
        let back = EncryptedString::from_db(&old);
        std::env::remove_var(crate::aead::PREVIOUS_KEY_VAR);
        assert_eq!(back.unwrap(), "client-secret");
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
