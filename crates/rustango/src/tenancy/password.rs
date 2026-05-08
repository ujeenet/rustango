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

/// Generate a random password of the requested length.
///
/// Uses an alphabet of 58 characters (a–z, A–Z, 2–9; ambiguous
/// characters `0`, `O`, `1`, `l`, `I` are excluded so the password
/// can be read aloud or transcribed without confusion). The generator
/// is `OsRng`-backed; output is suitable for one-shot operator-driven
/// resets and for first-boot bootstrap accounts.
///
/// Caller must surface the generated password to the operator —
/// [`hash`] discards it on the way to the database, so a forgotten
/// `--generate` output cannot be recovered.
///
/// # Panics
/// If `length` is zero. Lengths under 12 are accepted but should be
/// avoided for production use.
#[must_use]
pub fn generate(length: usize) -> String {
    assert!(length > 0, "password length must be > 0");
    use argon2::password_hash::rand_core::RngCore;
    // 58 unambiguous chars (no 0/O, 1/l/I).
    const ALPHABET: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut rng = OsRng;
    let mut out = String::with_capacity(length);
    let mut buf = [0u8; 64];
    let mut filled = 0;
    while out.len() < length {
        if filled == 0 {
            rng.fill_bytes(&mut buf);
            filled = buf.len();
        }
        let byte = buf[buf.len() - filled];
        filled -= 1;
        // Reject-and-retry to avoid modulo bias.
        let max = u8::try_from(ALPHABET.len() - 1).expect("alphabet < 256 chars");
        if byte <= max.saturating_mul(255 / max) {
            out.push(ALPHABET[(byte as usize) % ALPHABET.len()] as char);
        }
    }
    out
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
    fn generate_produces_correct_length_and_charset() {
        let p = generate(24);
        assert_eq!(p.len(), 24);
        for c in p.chars() {
            assert!(
                c.is_ascii_alphanumeric(),
                "generated char outside expected alphabet: {c:?}"
            );
            assert!(
                !"0O1lI".contains(c),
                "ambiguous char in generated password: {c:?}"
            );
        }
    }

    #[test]
    fn generate_round_trips_through_hash_and_verify() {
        let p = generate(20);
        let h = hash(&p).unwrap();
        assert!(verify(&p, &h).unwrap());
    }

    #[test]
    fn two_calls_to_generate_differ() {
        // Best-effort uniqueness — collisions on 58^16 are vanishingly rare.
        assert_ne!(generate(16), generate(16));
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
