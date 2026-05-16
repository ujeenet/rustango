//! Pluggable password-hasher chain with **upgrade-on-login** —
//! Django's `PASSWORD_HASHERS = [...]` setting.
//!
//! Every entry in the chain implements [`PasswordHasher`]. The
//! **first** entry is the "preferred" hasher: new passwords hash
//! with it, and a successful login against an OLDER hasher returns
//! a freshly-rehashed value so the caller can transparently
//! upgrade the stored hash to the preferred algorithm on the next
//! write. Migrating off bcrypt / pbkdf2 to argon2id this way is
//! the standard Django pattern.
//!
//! ```ignore
//! use rustango::password_hashers::{
//!     PasswordHasherChain, Argon2idHasher, VerifyOutcome,
//! };
//!
//! let chain = PasswordHasherChain::new()
//!     .with(Box::new(Argon2idHasher))
//!     .with(Box::new(LegacyBcryptHasher::new())); // user-provided
//!
//! match chain.verify(&form_password, &user.password_hash)? {
//!     VerifyOutcome::Match { needs_rehash: Some(new_hash) } => {
//!         // login succeeded against an older hasher — persist the
//!         // freshly minted argon2id hash so future logins are fast.
//!         user.update_password_hash(&new_hash).await?;
//!     }
//!     VerifyOutcome::Match { needs_rehash: None } => {}  // up-to-date
//!     VerifyOutcome::Mismatch => { /* 401 */ }
//! }
//! ```
//!
//! ## How `identify()` works
//!
//! Each hasher's [`PasswordHasher::identify`] returns `true` when
//! the given stored hash was produced by that hasher's algorithm.
//! For PHC-format strings the implementation matches on the
//! `"$argon2id$..."` prefix; for older formats (bcrypt's `$2b$`,
//! pbkdf2's `pbkdf2_sha256$...`, plain-text legacy data) the impl
//! pattern-matches on the leading marker.
//!
//! Chain verify walks every hasher; the first one whose
//! `identify()` returns true is used. Order doesn't matter for
//! verification — only for `hash()` (which always picks the first
//! hasher).
//!
//! Issue #54 — third piece of [`crate::auth_backends`] +
//! [`crate::password_validators`].

use std::fmt;

// ------------------------------------------------------------------ HasherError

#[derive(Debug)]
pub enum HasherError {
    /// Hashing the password failed (typically: RNG failure or
    /// memory-allocation failure in argon2).
    Hash(String),
    /// Stored hash is malformed for the hasher that
    /// `identify()`'d it. Distinct from "mismatch": malformed means
    /// the stored value is corrupted; mismatch means it parsed but
    /// the password is wrong.
    Malformed(String),
    /// No hasher in the chain identifies the stored hash format.
    /// Indicates either a corrupted DB row or a hasher that was
    /// removed from the chain without a migration step.
    NoMatchingHasher,
}

impl fmt::Display for HasherError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hash(msg) => write!(f, "password hash error: {msg}"),
            Self::Malformed(msg) => write!(f, "malformed stored hash: {msg}"),
            Self::NoMatchingHasher => {
                f.write_str("no hasher in the chain recognized the stored hash format")
            }
        }
    }
}

impl std::error::Error for HasherError {}

// ------------------------------------------------------------------ VerifyOutcome

/// Result of `PasswordHasherChain::verify`. `Match` carries an
/// optional `needs_rehash`: when the matched hasher isn't the
/// preferred one, this holds a freshly-rehashed string that the
/// caller should persist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyOutcome {
    Match { needs_rehash: Option<String> },
    Mismatch,
}

impl VerifyOutcome {
    /// Convenience: `true` when the password matched (regardless
    /// of whether a rehash is needed).
    #[must_use]
    pub fn is_match(&self) -> bool {
        matches!(self, Self::Match { .. })
    }

    /// Convenience: pull the rehash value out, if any.
    #[must_use]
    pub fn rehash(&self) -> Option<&str> {
        match self {
            Self::Match {
                needs_rehash: Some(s),
            } => Some(s.as_str()),
            _ => None,
        }
    }
}

// ------------------------------------------------------------------ PasswordHasher

/// One hasher's algorithm-specific interface. The trait is sync —
/// hashing is CPU-bound and current implementations don't need an
/// executor.
pub trait PasswordHasher: Send + Sync {
    /// Algorithm identifier (`"argon2id"`, `"bcrypt"`, …). Used in
    /// telemetry and tests; not part of the wire format.
    fn algorithm(&self) -> &'static str;

    /// Produce a stored-hash string for `password`.
    fn hash(&self, password: &str) -> Result<String, HasherError>;

    /// Constant-time verify against a stored hash that THIS hasher
    /// produced. The chain calls this only after [`Self::identify`]
    /// returns true, so implementations can assume the format is
    /// theirs to parse.
    fn verify(&self, password: &str, stored: &str) -> Result<bool, HasherError>;

    /// `true` if `stored` was produced by this hasher's algorithm.
    /// Implementations typically pattern-match a leading marker
    /// (`"$argon2id$"`, `"$2b$"`, `"pbkdf2_sha256$"`, …).
    fn identify(&self, stored: &str) -> bool;
}

// ------------------------------------------------------------------ PasswordHasherChain

/// Ordered list of hashers. First entry = preferred hasher.
#[derive(Default)]
pub struct PasswordHasherChain {
    hashers: Vec<Box<dyn PasswordHasher>>,
}

impl PasswordHasherChain {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a hasher at the end of the chain. The first hasher
    /// registered is the preferred one (used for new hashes + the
    /// rehash target).
    #[must_use]
    pub fn with(mut self, h: Box<dyn PasswordHasher>) -> Self {
        self.hashers.push(h);
        self
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.hashers.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hashers.is_empty()
    }

    /// Hash with the preferred hasher (first in the chain).
    pub fn hash(&self, password: &str) -> Result<String, HasherError> {
        let preferred = self.hashers.first().ok_or_else(|| {
            HasherError::Hash("PasswordHasherChain has no hashers registered".into())
        })?;
        preferred.hash(password)
    }

    /// Verify `password` against `stored`. Walks every hasher to
    /// find the one whose `identify()` returns true; if that
    /// hasher isn't the preferred one, the returned outcome
    /// carries a freshly-rehashed string so the caller can upgrade.
    pub fn verify(&self, password: &str, stored: &str) -> Result<VerifyOutcome, HasherError> {
        let (idx, hasher) = self
            .hashers
            .iter()
            .enumerate()
            .find(|(_, h)| h.identify(stored))
            .ok_or(HasherError::NoMatchingHasher)?;

        if !hasher.verify(password, stored)? {
            return Ok(VerifyOutcome::Mismatch);
        }
        // Match. Rehash if the matched hasher isn't the preferred one.
        let needs_rehash = if idx == 0 {
            None
        } else {
            // Use the PREFERRED hasher to mint the new hash so the
            // caller can transparently upgrade.
            Some(self.hashers[0].hash(password)?)
        };
        Ok(VerifyOutcome::Match { needs_rehash })
    }

    /// `true` if the chain knows the format of `stored`.
    #[must_use]
    pub fn identifies(&self, stored: &str) -> bool {
        self.hashers.iter().any(|h| h.identify(stored))
    }
}

// ------------------------------------------------------------------ Argon2idHasher

/// Argon2id hasher wrapping [`crate::passwords::hash`] /
/// [`crate::passwords::verify`]. Default and preferred for every
/// rustango deployment.
#[cfg(feature = "passwords")]
pub struct Argon2idHasher;

#[cfg(feature = "passwords")]
impl PasswordHasher for Argon2idHasher {
    fn algorithm(&self) -> &'static str {
        "argon2id"
    }
    fn hash(&self, password: &str) -> Result<String, HasherError> {
        crate::passwords::hash(password).map_err(|e| HasherError::Hash(e.to_string()))
    }
    fn verify(&self, password: &str, stored: &str) -> Result<bool, HasherError> {
        crate::passwords::verify(password, stored)
            .map_err(|e| HasherError::Malformed(e.to_string()))
    }
    fn identify(&self, stored: &str) -> bool {
        stored.starts_with("$argon2id$")
    }
}

// ------------------------------------------------------------------ Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Test-only legacy hasher that stores `"legacy$<password>"`
    /// verbatim. Lets us exercise chain semantics without pulling
    /// bcrypt as a dep just for tests.
    struct LegacyPlainHasher {
        // Count rehash calls to verify chain doesn't re-hash unnecessarily.
        hash_calls: Mutex<usize>,
    }
    impl LegacyPlainHasher {
        fn new() -> Self {
            Self {
                hash_calls: Mutex::new(0),
            }
        }
    }
    impl PasswordHasher for LegacyPlainHasher {
        fn algorithm(&self) -> &'static str {
            "legacy"
        }
        fn hash(&self, password: &str) -> Result<String, HasherError> {
            *self.hash_calls.lock().unwrap() += 1;
            Ok(format!("legacy${password}"))
        }
        fn verify(&self, password: &str, stored: &str) -> Result<bool, HasherError> {
            let expected = stored
                .strip_prefix("legacy$")
                .ok_or_else(|| HasherError::Malformed(stored.into()))?;
            Ok(expected == password)
        }
        fn identify(&self, stored: &str) -> bool {
            stored.starts_with("legacy$")
        }
    }

    /// Second test hasher with a different format — lets us verify
    /// the chain identifies the right hasher per stored format.
    struct AltLegacyHasher;
    impl PasswordHasher for AltLegacyHasher {
        fn algorithm(&self) -> &'static str {
            "alt_legacy"
        }
        fn hash(&self, password: &str) -> Result<String, HasherError> {
            Ok(format!("alt!{password}"))
        }
        fn verify(&self, password: &str, stored: &str) -> Result<bool, HasherError> {
            let expected = stored
                .strip_prefix("alt!")
                .ok_or_else(|| HasherError::Malformed(stored.into()))?;
            Ok(expected == password)
        }
        fn identify(&self, stored: &str) -> bool {
            stored.starts_with("alt!")
        }
    }

    #[test]
    fn empty_chain_hash_errors() {
        let chain = PasswordHasherChain::new();
        assert!(chain.is_empty());
        let err = chain.hash("anything").unwrap_err();
        assert!(matches!(err, HasherError::Hash(_)));
    }

    #[test]
    fn hash_uses_preferred_hasher() {
        let chain = PasswordHasherChain::new()
            .with(Box::new(LegacyPlainHasher::new()))
            .with(Box::new(AltLegacyHasher));
        let h = chain.hash("hunter2").unwrap();
        // Preferred = LegacyPlainHasher → "legacy$..."
        assert!(h.starts_with("legacy$"), "got: {h}");
    }

    #[test]
    fn verify_match_against_preferred_returns_no_rehash() {
        let chain = PasswordHasherChain::new().with(Box::new(LegacyPlainHasher::new()));
        let outcome = chain.verify("hunter2", "legacy$hunter2").unwrap();
        assert_eq!(outcome, VerifyOutcome::Match { needs_rehash: None });
        assert!(outcome.is_match());
        assert!(outcome.rehash().is_none());
    }

    #[test]
    fn verify_match_against_older_hasher_returns_rehash() {
        // Preferred = AltLegacyHasher, legacy = LegacyPlainHasher.
        let chain = PasswordHasherChain::new()
            .with(Box::new(AltLegacyHasher))
            .with(Box::new(LegacyPlainHasher::new()));
        let outcome = chain.verify("hunter2", "legacy$hunter2").unwrap();
        match outcome {
            VerifyOutcome::Match {
                needs_rehash: Some(new_hash),
            } => {
                // New hash uses the preferred ("alt!") format.
                assert!(new_hash.starts_with("alt!"), "got: {new_hash}");
                assert_eq!(new_hash, "alt!hunter2");
            }
            other => panic!("expected Match with rehash, got: {other:?}"),
        }
    }

    #[test]
    fn verify_mismatch_against_known_format() {
        let chain = PasswordHasherChain::new().with(Box::new(LegacyPlainHasher::new()));
        let outcome = chain.verify("wrong", "legacy$hunter2").unwrap();
        assert_eq!(outcome, VerifyOutcome::Mismatch);
        assert!(!outcome.is_match());
    }

    #[test]
    fn verify_unknown_format_errors_with_no_matching_hasher() {
        let chain = PasswordHasherChain::new().with(Box::new(LegacyPlainHasher::new()));
        let err = chain.verify("anything", "$2b$bcrypt-style").unwrap_err();
        assert!(matches!(err, HasherError::NoMatchingHasher));
    }

    #[test]
    fn identifies_walks_chain() {
        let chain = PasswordHasherChain::new()
            .with(Box::new(LegacyPlainHasher::new()))
            .with(Box::new(AltLegacyHasher));
        assert!(chain.identifies("legacy$x"));
        assert!(chain.identifies("alt!x"));
        assert!(!chain.identifies("$argon2id$x"));
    }

    #[test]
    fn chain_picks_right_hasher_per_format() {
        let chain = PasswordHasherChain::new()
            .with(Box::new(LegacyPlainHasher::new()))
            .with(Box::new(AltLegacyHasher));
        // "alt!..." routes to AltLegacyHasher (idx 1) → needs rehash.
        let outcome = chain.verify("hunter2", "alt!hunter2").unwrap();
        match outcome {
            VerifyOutcome::Match {
                needs_rehash: Some(new),
            } => assert!(new.starts_with("legacy$"), "got: {new}"),
            other => panic!("expected Match w/ rehash, got: {other:?}"),
        }
    }

    #[test]
    fn hasher_error_display() {
        assert_eq!(
            format!("{}", HasherError::Hash("rng".into())),
            "password hash error: rng"
        );
        assert_eq!(
            format!("{}", HasherError::Malformed("nope".into())),
            "malformed stored hash: nope"
        );
        assert!(format!("{}", HasherError::NoMatchingHasher).contains("no hasher"));
    }

    // ---------- Argon2idHasher (real impl) — gated on `passwords` feature ----------

    #[cfg(feature = "passwords")]
    #[test]
    fn argon2id_hasher_round_trip() {
        let h = Argon2idHasher;
        let stored = h.hash("CorrectHorseBatteryStaple!42").unwrap();
        assert!(h.identify(&stored));
        assert!(h.verify("CorrectHorseBatteryStaple!42", &stored).unwrap());
        assert!(!h.verify("wrong", &stored).unwrap());
    }

    #[cfg(feature = "passwords")]
    #[test]
    fn argon2id_identify_rejects_other_formats() {
        let h = Argon2idHasher;
        assert!(!h.identify("$2b$bcrypt"));
        assert!(!h.identify("legacy$plain"));
        assert!(!h.identify(""));
    }

    #[cfg(feature = "passwords")]
    #[test]
    fn chain_with_argon2id_as_preferred_upgrades_legacy() {
        let chain = PasswordHasherChain::new()
            .with(Box::new(Argon2idHasher))
            .with(Box::new(LegacyPlainHasher::new()));
        // Stored as legacy; logging in upgrades to argon2id.
        let outcome = chain.verify("hunter2", "legacy$hunter2").unwrap();
        let VerifyOutcome::Match {
            needs_rehash: Some(new_hash),
        } = outcome
        else {
            panic!("expected upgrade-on-login, got: {outcome:?}");
        };
        assert!(new_hash.starts_with("$argon2id$"));
        // Verifying the new hash works with the chain too.
        let re = chain.verify("hunter2", &new_hash).unwrap();
        assert_eq!(re, VerifyOutcome::Match { needs_rehash: None });
    }
}
