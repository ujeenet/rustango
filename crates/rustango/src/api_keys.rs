//! Create and check API keys.
//!
//! This is the standalone helper. If you want keys stored and
//! checked for you, see [`crate::tenancy::auth_backends`].
//!
//! ## Format
//!
//! A key is `{prefix}.{secret}`:
//! - `prefix` — 8 hex chars, public. Index it so you can find the
//!   row without scanning the table.
//! - `secret` — 32 hex chars. Never store it. Store only the
//!   argon2id hash, and show the plaintext to the user once, at
//!   creation.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::api_keys::{generate_key, verify_key, hash_secret};
//!
//! // Issuing a new key:
//! let (full_token, prefix, hash) = generate_key()?;
//! // Send `full_token` to the user once. Store `prefix` + `hash` in your DB.
//!
//! // Verifying an inbound key:
//! let inbound = "abc12345.f9a7d2..."; // from request header
//! let parts = inbound.split_once('.').ok_or("bad format")?;
//! // Look up the row by parts.0 (prefix), then:
//! if verify_key(parts.1, &stored_hash)? {
//!     // authenticated
//! }
//! ```

// This module owns the sync calls the lint bans elsewhere.
#![allow(clippy::disallowed_methods)]

use rand::{rngs::OsRng, RngCore};

use crate::passwords::PasswordError;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ApiKeyError {
    #[error("hashing failed: {0}")]
    Hash(String),
    #[error("verification error: {0}")]
    Verify(String),
    /// No hashing slot freed up in time; see [`PasswordError::Busy`].
    #[error("password hashing is busy")]
    Busy,
}

impl From<PasswordError> for ApiKeyError {
    fn from(e: PasswordError) -> Self {
        match e {
            PasswordError::Hash(m) => Self::Hash(m),
            PasswordError::Verify(m) => Self::Verify(m),
            PasswordError::Busy => Self::Busy,
        }
    }
}

/// Make a new API key. Returns `(full_token, prefix, hash)`.
///
/// Give `full_token` to the user. Store only `prefix` and `hash`;
/// storing the token would let anyone who reads your database use
/// the key. From async code use [`generate_key_async`].
///
/// # Errors
/// [`ApiKeyError::Hash`] if argon2 fails, which is very rare.
pub fn generate_key() -> Result<(String, String, String), ApiKeyError> {
    let (prefix, secret) = new_token();
    let hash = hash_secret(&secret)?;
    Ok((format!("{prefix}.{secret}"), prefix, hash))
}

/// [`generate_key`] with the hash on the blocking pool.
///
/// # Errors
/// As [`generate_key`], or [`ApiKeyError::Busy`].
pub async fn generate_key_async() -> Result<(String, String, String), ApiKeyError> {
    let (prefix, secret) = new_token();
    let hash = hash_secret_async(&secret).await?;
    Ok((format!("{prefix}.{secret}"), prefix, hash))
}

/// A fresh `(prefix, secret)` pair from `OsRng`: the secret is a
/// bearer credential, so it needs a cryptographic source.
fn new_token() -> (String, String) {
    let mut prefix_bytes: [u8; 4] = [0; 4];
    OsRng.fill_bytes(&mut prefix_bytes);
    let mut secret_bytes: [u8; 16] = [0; 16];
    OsRng.fill_bytes(&mut secret_bytes);
    (to_hex(&prefix_bytes), to_hex(&secret_bytes))
}

/// Hash a secret with argon2id via [`crate::passwords::hash`]. The
/// result is a PHC string (`$argon2id$v=19$...`). Store this, never
/// the secret. From async code use [`hash_secret_async`].
///
/// # Errors
/// [`ApiKeyError::Hash`] if argon2 fails.
pub fn hash_secret(secret: &str) -> Result<String, ApiKeyError> {
    Ok(crate::passwords::hash(secret)?)
}

/// [`hash_secret`] on the blocking pool.
///
/// # Errors
/// As [`hash_secret`], or [`ApiKeyError::Busy`].
pub async fn hash_secret_async(secret: &str) -> Result<String, ApiKeyError> {
    Ok(crate::passwords::hash_async(secret).await?)
}

/// Check a plaintext secret against a stored argon2 hash, in constant
/// time. Never use `==`. `Ok(true)` means the secret matches. From
/// async code use [`verify_key_async`].
///
/// # Errors
/// [`ApiKeyError::Verify`] if `stored_hash` is not a valid argon2
/// PHC string.
pub fn verify_key(secret: &str, stored_hash: &str) -> Result<bool, ApiKeyError> {
    Ok(crate::passwords::verify(secret, stored_hash)?)
}

/// [`verify_key`] on the blocking pool.
///
/// # Errors
/// As [`verify_key`], or [`ApiKeyError::Busy`].
pub async fn verify_key_async(secret: &str, stored_hash: &str) -> Result<bool, ApiKeyError> {
    Ok(crate::passwords::verify_async(secret, stored_hash).await?)
}

/// Split a `{prefix}.{secret}` token, or `None` if it is malformed.
#[must_use]
pub fn split_token(token: &str) -> Option<(&str, &str)> {
    let (prefix, secret) = token.split_once('.')?;
    if prefix.len() != 8 || secret.is_empty() {
        return None;
    }
    Some((prefix, secret))
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_returns_well_formed_token() {
        let (token, prefix, hash) = generate_key().unwrap();
        assert_eq!(prefix.len(), 8);
        assert!(token.starts_with(&prefix));
        assert!(token.contains('.'));
        assert!(hash.starts_with("$argon2id$"));
    }

    #[test]
    fn each_generation_is_unique() {
        let (t1, p1, _) = generate_key().unwrap();
        let (t2, p2, _) = generate_key().unwrap();
        assert_ne!(t1, t2);
        assert_ne!(p1, p2);
    }

    #[test]
    fn verify_key_succeeds_for_correct_secret() {
        let (token, _, hash) = generate_key().unwrap();
        let (_, secret) = split_token(&token).unwrap();
        assert!(verify_key(secret, &hash).unwrap());
    }

    #[test]
    fn verify_key_fails_for_wrong_secret() {
        let (_, _, hash) = generate_key().unwrap();
        assert!(!verify_key("wrong-secret", &hash).unwrap());
    }

    #[tokio::test]
    async fn async_variants_match_the_sync_ones() {
        let (token, _, hash) = generate_key_async().await.unwrap();
        let (_, secret) = split_token(&token).unwrap();
        assert!(verify_key_async(secret, &hash).await.unwrap());
        assert!(verify_key(secret, &hash_secret_async(secret).await.unwrap()).unwrap());
        assert!(!verify_key_async("wrong", &hash).await.unwrap());
    }

    #[test]
    fn verify_invalid_hash_returns_error() {
        let r = verify_key("anything", "not-a-valid-hash");
        assert!(r.is_err());
    }

    #[test]
    fn split_token_valid_format() {
        let result = split_token("abcd1234.deadbeef");
        assert_eq!(result, Some(("abcd1234", "deadbeef")));
    }

    #[test]
    fn split_token_missing_dot() {
        assert_eq!(split_token("noTdotHere"), None);
    }

    #[test]
    fn split_token_wrong_prefix_length() {
        assert_eq!(split_token("short.secret"), None);
        assert_eq!(split_token("toolongprefix.secret"), None);
    }

    #[test]
    fn split_token_empty_secret() {
        assert_eq!(split_token("abcd1234."), None);
    }
}
