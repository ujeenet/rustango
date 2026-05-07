//! Argon2id password hashing — used by the registry-scoped
//! [`super::Operator`] and the per-tenant [`super::User`] models.
//!
//! Both identity domains share the same crypto: hashes are stored as
//! the standard PHC string (`$argon2id$v=19$m=...,t=...,p=...$salt$hash`)
//! so verification is self-describing — the parameters travel with the
//! hash. Default parameters come from `argon2::Argon2::default()` —
//! Argon2id with the OWASP-recommended cost (m=19456, t=2, p=1 as of
//! 2026); good enough for hobby/demo deployments. Operators can opt
//! into stronger parameters via [`hash_with`].

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};

use super::error::TenancyError;

/// Hash a plaintext password with default Argon2id parameters.
///
/// Returns the PHC-format string suitable for storing in
/// `Operator.password_hash` / `User.password_hash`.
///
/// # Errors
/// Returns [`TenancyError::Validation`] for empty passwords (refused
/// up front to catch the trivial misuse) or for a downstream argon2
/// error (extremely unlikely).
pub fn hash(plaintext: &str) -> Result<String, TenancyError> {
    if plaintext.is_empty() {
        return Err(TenancyError::Validation(
            "password must not be empty".into(),
        ));
    }
    let salt = SaltString::generate(&mut OsRng);
    let hasher = Argon2::default();
    let phc = hasher
        .hash_password(plaintext.as_bytes(), &salt)
        .map_err(|e| TenancyError::Validation(format!("argon2 hash failed: {e}")))?;
    Ok(phc.to_string())
}

/// Verify a plaintext password against a PHC-format hash.
///
/// Returns `true` for a match, `false` for a mismatch. **Constant-
/// time** in the matching path (argon2's default verifier is
/// constant-time over the digest comparison); we don't add additional
/// timing protections at this layer.
///
/// # Errors
/// Returns [`TenancyError::Validation`] when `phc_hash` is malformed
/// (not a valid PHC string).
pub fn verify(plaintext: &str, phc_hash: &str) -> Result<bool, TenancyError> {
    let parsed = PasswordHash::new(phc_hash)
        .map_err(|e| TenancyError::Validation(format!("malformed password hash: {e}")))?;
    Ok(Argon2::default()
        .verify_password(plaintext.as_bytes(), &parsed)
        .is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_and_verify_round_trip() {
        let h = hash("hunter2").unwrap();
        assert!(h.starts_with("$argon2id$"));
        assert!(verify("hunter2", &h).unwrap());
        assert!(!verify("wrong", &h).unwrap());
    }

    #[test]
    fn hash_rejects_empty() {
        let err = hash("").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("must not be empty"), "got: {msg}");
    }

    #[test]
    fn verify_rejects_malformed_hash() {
        let err = verify("hunter2", "not-a-phc-string").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("malformed"), "got: {msg}");
    }

    #[test]
    fn two_hashes_of_same_password_differ() {
        // Salts are random — same plaintext, different stored hashes,
        // both verify.
        let h1 = hash("same").unwrap();
        let h2 = hash("same").unwrap();
        assert_ne!(h1, h2);
        assert!(verify("same", &h1).unwrap());
        assert!(verify("same", &h2).unwrap());
    }
}
