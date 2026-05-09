//! Generic API key generation and verification.
//!
//! For the tenancy-integrated version (with DB-backed `rustango_api_keys`
//! table + `ApiKeyBackend`), see [`crate::tenancy::auth_backends`]. This
//! module is the lower-level standalone helper for apps that want to
//! manage API keys themselves.
//!
//! ## Format
//!
//! API keys are `{prefix}.{secret}`:
//! - `prefix` — 8-char hex, public. Stored alongside the hash so you can
//!   look up the key in O(1) without a full table scan.
//! - `secret` — 32-char hex, kept secret. Hashed with argon2id; the
//!   plaintext is only available at creation time.
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

use rand::{rngs::OsRng, RngCore};

#[derive(Debug, thiserror::Error)]
pub enum ApiKeyError {
    #[error("hashing failed: {0}")]
    Hash(String),
    #[error("verification error: {0}")]
    Verify(String),
}

/// Generate a fresh API key. Returns `(full_token, prefix, hash)`:
/// - `full_token` — `{prefix}.{secret}` to give to the user
/// - `prefix` — 8-char hex prefix for the lookup column
/// - `hash` — argon2id hash of the secret for the password column
///
/// # Errors
/// [`ApiKeyError::Hash`] on argon2 failures (extremely rare).
pub fn generate_key() -> Result<(String, String, String), ApiKeyError> {
    // v0.30.12 — use OsRng directly. Cryptographic secret bytes
    // for the user's bearer token; consistent with the rest of
    // the framework (csrf.rs / passwords.rs / session.rs).
    let mut prefix_bytes: [u8; 4] = [0; 4];
    OsRng.fill_bytes(&mut prefix_bytes);
    let prefix = to_hex(&prefix_bytes);
    let mut secret_bytes: [u8; 16] = [0; 16];
    OsRng.fill_bytes(&mut secret_bytes);
    let secret = to_hex(&secret_bytes);
    let hash = hash_secret(&secret)?;
    let token = format!("{prefix}.{secret}");
    Ok((token, prefix, hash))
}

/// Hash a secret with argon2id. Returns the standard PHC string format
/// (`$argon2id$v=19$...`) suitable for storing in a varchar column.
///
/// # Errors
/// [`ApiKeyError::Hash`] on argon2 failures.
pub fn hash_secret(secret: &str) -> Result<String, ApiKeyError> {
    use argon2::password_hash::{rand_core::OsRng, PasswordHasher, SaltString};
    use argon2::Argon2;

    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    argon2
        .hash_password(secret.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| ApiKeyError::Hash(e.to_string()))
}

/// Verify a plaintext secret against a stored argon2 hash.
///
/// Returns `Ok(true)` for a match, `Ok(false)` for a mismatch, `Err`
/// when the stored hash isn't a valid argon2 PHC string.
pub fn verify_key(secret: &str, stored_hash: &str) -> Result<bool, ApiKeyError> {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    use argon2::Argon2;

    let parsed = PasswordHash::new(stored_hash).map_err(|e| ApiKeyError::Verify(e.to_string()))?;
    Ok(Argon2::default()
        .verify_password(secret.as_bytes(), &parsed)
        .is_ok())
}

/// Split a `{prefix}.{secret}` token. Returns `None` for malformed input.
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
