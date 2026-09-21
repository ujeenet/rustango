//! Pluggable password-hasher chain with **upgrade-on-login**.
//!
//! Every entry implements [`PasswordHasher`]. The **first** entry is
//! the preferred hasher: it writes all new hashes. When a login
//! matches an older hasher, the outcome carries a fresh hash in the
//! preferred format, so the caller can store the upgrade. That is how
//! you move a user base from bcrypt or pbkdf2 to argon2id without
//! asking anyone to reset a password.
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
//! [`PasswordHasher::identify`] returns `true` when the stored hash
//! was written by that hasher. Implementations match a leading
//! marker: `"$argon2id$"`, bcrypt's `"$2b$"`, `"pbkdf2_sha256$"`.
//!
//! Verification uses the first hasher that claims the hash, so order
//! only matters for `hash()`, which always takes the first entry.
//!
//! [`PasswordHasher`]: crate::password_hashers::PasswordHasher
//! [`PasswordHasher::identify`]: crate::password_hashers::PasswordHasher::identify

use std::fmt;

// ------------------------------------------------------------------ HasherError

#[derive(Debug)]
pub enum HasherError {
    /// Hashing failed, usually an RNG or allocation failure.
    Hash(String),
    /// The stored hash is corrupt: the hasher claimed the format but
    /// could not parse it. Not the same as a mismatch, which means it
    /// parsed and the password was wrong.
    Malformed(String),
    /// No hasher in the chain knows this format. Either the row is
    /// corrupt, or a hasher was dropped from the chain before its
    /// users were migrated.
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

/// Result of `PasswordHasherChain::verify`. When the hash that
/// matched came from an older hasher, `needs_rehash` holds a fresh
/// hash in the preferred format for the caller to store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyOutcome {
    Match { needs_rehash: Option<String> },
    Mismatch,
}

impl VerifyOutcome {
    /// `true` when the password matched, rehash needed or not.
    #[must_use]
    pub fn is_match(&self) -> bool {
        matches!(self, Self::Match { .. })
    }

    /// The rehash value, if there is one.
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

/// One hasher's algorithm. Sync: hashing is CPU-bound and needs no
/// executor.
pub trait PasswordHasher: Send + Sync {
    /// Algorithm name (`"argon2id"`, `"bcrypt"`, …), for telemetry and
    /// tests. Not part of the stored format.
    fn algorithm(&self) -> &'static str;

    /// Produce a stored-hash string for `password`. Use a slow, salted
    /// algorithm with a fresh salt per password — argon2id, bcrypt, or
    /// pbkdf2 at a high iteration count. A plain digest is guessable
    /// at billions of tries per second.
    fn hash(&self, password: &str) -> Result<String, HasherError>;

    /// Verify against a hash THIS hasher produced. Compare in constant
    /// time so timing cannot leak how close a guess was. The chain
    /// calls this only after [`Self::identify`] says yes, so the
    /// format is yours to parse.
    fn verify(&self, password: &str, stored: &str) -> Result<bool, HasherError>;

    /// `true` if this hasher produced `stored`. Usually a match on the
    /// leading marker (`"$argon2id$"`, `"$2b$"`, `"pbkdf2_sha256$"`).
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

    /// Append a hasher. The first one registered is the preferred
    /// hasher: it writes new hashes and is the rehash target.
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

    /// Verify `password` against `stored`, using the first hasher
    /// that identifies the format. If that is not the preferred
    /// hasher, the outcome carries a fresh hash to store.
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
        // Matched. Rehash unless it was the preferred hasher.
        let needs_rehash = if idx == 0 {
            None
        } else {
            // The preferred hasher mints the upgrade.
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

/// Argon2id hasher over [`crate::passwords::hash`] /
/// [`crate::passwords::verify`]. Put it first in the chain.
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

    /// Test-only hasher storing `"legacy$<password>"` verbatim, so the
    /// chain can be tested without a bcrypt dev-dependency.
    struct LegacyPlainHasher {
        // Counts rehash calls.
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

    /// Second test hasher with another format, to check the chain
    /// picks the right one per stored format.
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
                // The new hash uses the preferred "alt!" format.
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
        // "alt!…" routes to AltLegacyHasher at index 1, so it rehashes.
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

    // ---------- Argon2idHasher, gated on the `passwords` feature ----------

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
        // The new hash verifies through the chain too.
        let re = chain.verify("hunter2", &new_hash).unwrap();
        assert_eq!(re, VerifyOutcome::Match { needs_rehash: None });
    }
}
