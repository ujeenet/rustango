//! Generic password hashing + strength checking.
//!
//! For the tenancy-integrated user-password helpers, see
//! [`crate::tenancy::password`]. This module is the lower-level standalone
//! version — argon2id hashing + a minimal strength heuristic that doesn't
//! require importing tenancy types.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::passwords::{hash, verify, strength_score, StrengthIssue};
//!
//! // Signup:
//! let issues = strength_score(&new_password);
//! if !issues.is_empty() {
//!     return Err(format!("password too weak: {:?}", issues));
//! }
//! let hashed = hash(&new_password)?;
//! // Store `hashed` in user row.
//!
//! // Login:
//! let user = users::find_by_email(&email).await?;
//! if !verify(&attempted, &user.password_hash)? {
//!     return Err("bad credentials");
//! }
//! ```

#[derive(Debug, thiserror::Error)]
pub enum PasswordError {
    #[error("hashing failed: {0}")]
    Hash(String),
    #[error("verification error: {0}")]
    Verify(String),
}

/// Hash a password with argon2id. Returns the standard PHC string format.
///
/// # Errors
/// [`PasswordError::Hash`] on argon2 failures.
pub fn hash(password: &str) -> Result<String, PasswordError> {
    use argon2::password_hash::{rand_core::OsRng, PasswordHasher, SaltString};
    use argon2::Argon2;

    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| PasswordError::Hash(e.to_string()))
}

/// Verify a password against an argon2 PHC hash.
///
/// # Errors
/// [`PasswordError::Verify`] when `stored_hash` isn't a valid PHC string.
pub fn verify(password: &str, stored_hash: &str) -> Result<bool, PasswordError> {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    use argon2::Argon2;

    let parsed =
        PasswordHash::new(stored_hash).map_err(|e| PasswordError::Verify(e.to_string()))?;
    Ok(Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok())
}

// ------------------------------------------------------------------ Strength check

/// One thing wrong with a candidate password.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StrengthIssue {
    /// Shorter than the recommended 12 characters.
    TooShort,
    /// Contains only letters (no digits or symbols).
    NoDigitsOrSymbols,
    /// Uses only lowercase letters (no uppercase, digits, or symbols).
    NoVariety,
    /// Matches a list of well-known weak passwords.
    KnownWeak,
}

/// Score a candidate password. Returns an empty `Vec` when strong enough.
///
/// Heuristics (intentionally simple — encourage users without being a
/// hard policy gate; pair with HIBP / pwned-passwords for serious deployments):
/// - Length < 12 → [`StrengthIssue::TooShort`]
/// - All-letter (no digit/symbol) → [`StrengthIssue::NoDigitsOrSymbols`]
/// - All-lowercase letters → [`StrengthIssue::NoVariety`]
/// - In the small built-in weak-password list → [`StrengthIssue::KnownWeak`]
#[must_use]
pub fn strength_score(password: &str) -> Vec<StrengthIssue> {
    let mut issues = Vec::new();

    if password.chars().count() < 12 {
        issues.push(StrengthIssue::TooShort);
    }

    let has_digit = password.chars().any(|c| c.is_ascii_digit());
    let has_symbol = password
        .chars()
        .any(|c| !c.is_alphanumeric() && !c.is_whitespace());
    let has_upper = password.chars().any(|c| c.is_ascii_uppercase());
    let has_lower = password.chars().any(|c| c.is_ascii_lowercase());

    if !has_digit && !has_symbol {
        issues.push(StrengthIssue::NoDigitsOrSymbols);
    }
    if !has_digit && !has_symbol && !has_upper && has_lower {
        issues.push(StrengthIssue::NoVariety);
    }

    let lower = password.to_ascii_lowercase();
    if KNOWN_WEAK.iter().any(|&w| w == lower) {
        issues.push(StrengthIssue::KnownWeak);
    }

    issues
}

/// Top weak passwords from public breach lists. Intentionally tiny —
/// real apps should pair with HIBP's pwned-passwords API.
const KNOWN_WEAK: &[&str] = &[
    "password",
    "password1",
    "password123",
    "12345678",
    "123456789",
    "qwerty",
    "qwerty123",
    "letmein",
    "admin",
    "admin123",
    "welcome",
    "welcome1",
    "iloveyou",
    "monkey",
    "abc123",
    "111111",
    "000000",
    "passw0rd",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_and_verify_match() {
        let h = hash("CorrectHorseBatteryStaple!42").unwrap();
        assert!(verify("CorrectHorseBatteryStaple!42", &h).unwrap());
    }

    #[test]
    fn verify_rejects_wrong_password() {
        let h = hash("real-password").unwrap();
        assert!(!verify("wrong-password", &h).unwrap());
    }

    #[test]
    fn verify_invalid_hash_errors() {
        let r = verify("anything", "not-a-valid-hash");
        assert!(r.is_err());
    }

    #[test]
    fn strong_password_has_no_issues() {
        let issues = strength_score("Tr0ub4dor&3-CorrectBattery");
        assert!(issues.is_empty(), "got issues: {:?}", issues);
    }

    #[test]
    fn short_password_flagged() {
        let issues = strength_score("aB3!");
        assert!(issues.contains(&StrengthIssue::TooShort));
    }

    #[test]
    fn all_letter_password_flagged() {
        let issues = strength_score("abcdefghijklmnop");
        assert!(issues.contains(&StrengthIssue::NoDigitsOrSymbols));
        assert!(issues.contains(&StrengthIssue::NoVariety));
    }

    #[test]
    fn mixed_case_no_digits_only_flags_no_digits() {
        let issues = strength_score("ABCDEFGHIJKLMnop");
        assert!(issues.contains(&StrengthIssue::NoDigitsOrSymbols));
        assert!(!issues.contains(&StrengthIssue::NoVariety));
    }

    #[test]
    fn known_weak_password_flagged() {
        let issues = strength_score("password123");
        assert!(issues.contains(&StrengthIssue::KnownWeak));
    }

    #[test]
    fn known_weak_check_is_case_insensitive() {
        let issues = strength_score("PASSWORD123");
        assert!(issues.contains(&StrengthIssue::KnownWeak));
    }

    #[test]
    fn long_password_with_digit_passes_length_and_variety_check() {
        let issues = strength_score("ThisIsLongEnough1");
        assert!(!issues.contains(&StrengthIssue::TooShort));
        assert!(!issues.contains(&StrengthIssue::NoDigitsOrSymbols));
    }
}
