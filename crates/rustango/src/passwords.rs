//! Generic password hashing + strength checking.
//!
//! argon2id hashing plus a small strength heuristic, with no tenancy
//! types involved. For the tenancy-integrated helpers see
//! [`crate::tenancy::password`].
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
//!
//! From async code call [`hash_async`] / [`verify_async`] /
//! [`verify_dummy_async`]; the sync calls block a runtime worker.

// This module owns the sync calls the lint bans elsewhere.
#![allow(clippy::disallowed_methods)]

#[derive(Debug, thiserror::Error)]
pub enum PasswordError {
    #[error("hashing failed: {0}")]
    Hash(String),
    #[error("verification error: {0}")]
    Verify(String),
}

/// Hash a password with argon2id. Returns a standard PHC string.
///
/// argon2id is deliberately slow and memory-hungry, and every hash
/// gets a fresh random salt, so a stolen table cannot be attacked with
/// precomputed or shared work — each password must be guessed on its
/// own, slowly. Never store a plain or fast hash instead.
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

/// Verify a password against an argon2 PHC hash. The comparison is
/// constant time, so timing does not reveal how much of the hash the
/// guess got right.
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

/// A valid argon2id hash of a fixed throwaway password, built once on
/// first use at the same cost as a real stored hash. Backs
/// [`verify_dummy`].
fn dummy_hash() -> &'static str {
    use std::sync::OnceLock;
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY
        .get_or_init(|| {
            hash("rustango-timing-equalization-dummy")
                .expect("argon2id hashing of a fixed dummy input cannot fail")
        })
        .as_str()
}

/// Do one verification's worth of work and throw the result away.
///
/// Call this on the **user-not-found** and inactive branches of a
/// login. Without it an unknown user answers fast while a real one
/// pays for argon2, and that difference in response time tells an
/// attacker which accounts exist.
pub fn verify_dummy(password: &str) {
    let _ = verify(password, dummy_hash());
}

// ------------------------------------------------------------------ Async variants

/// [`hash`] on the blocking pool. Use this from async code: an inline
/// argon2 call parks a runtime worker for the whole hash.
///
/// # Errors
/// As [`hash`].
pub async fn hash_async(password: &str) -> Result<String, PasswordError> {
    let password = password.to_owned();
    off_runtime(move || hash(&password)).await
}

/// [`verify`] on the blocking pool.
///
/// # Errors
/// As [`verify`].
pub async fn verify_async(password: &str, stored_hash: &str) -> Result<bool, PasswordError> {
    let (password, stored_hash) = (password.to_owned(), stored_hash.to_owned());
    off_runtime(move || verify(&password, &stored_hash)).await
}

/// [`verify_dummy`] on the blocking pool.
pub async fn verify_dummy_async(password: &str) {
    let password = password.to_owned();
    off_runtime(move || verify_dummy(&password)).await;
}

/// Run argon2 work on the blocking pool, at most one job per CPU at once.
/// A panic in `f` resumes in the caller, as it would inline.
pub(crate) async fn off_runtime<T, F>(f: F) -> T
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    use std::sync::{Arc, OnceLock};
    use tokio::sync::Semaphore;
    static SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    let slots = SLOTS.get_or_init(|| {
        let n = std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get);
        Arc::new(Semaphore::new(n))
    });
    // The permit moves into the job, so a dropped caller still holds
    // its slot until the hash finishes.
    let permit = Arc::clone(slots)
        .acquire_owned()
        .await
        .expect("password semaphore is never closed");
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        f()
    })
    .await
    .unwrap_or_else(|e| std::panic::resume_unwind(e.into_panic()))
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

/// Score a candidate password. An empty `Vec` means no issue found.
///
/// The rules are deliberately simple — nudges, not a policy gate. Pair
/// them with HIBP / pwned-passwords for a real deployment.
/// - Length < 12 → [`StrengthIssue::TooShort`]
/// - No digit or symbol → [`StrengthIssue::NoDigitsOrSymbols`]
/// - Lowercase letters only → [`StrengthIssue::NoVariety`]
/// - On the built-in weak list → [`StrengthIssue::KnownWeak`]
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

/// Top weak passwords from public breach lists. Tiny on purpose; real
/// apps should also check HIBP's pwned-passwords API.
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

/// Awaits `fut` next to a 1 ms ticker and returns its output with the
/// ticks seen meanwhile. On a current-thread runtime inline argon2 sees 0.
#[cfg(test)]
pub(crate) async fn ticks_while<F: std::future::Future>(fut: F) -> (F::Output, usize) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    let ticks = Arc::new(AtomicUsize::new(0));
    let t = Arc::clone(&ticks);
    let ticker = tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            t.fetch_add(1, Ordering::Relaxed);
        }
    });
    tokio::task::yield_now().await;
    let before = ticks.load(Ordering::Relaxed);
    let out = fut.await;
    let n = ticks.load(Ordering::Relaxed) - before;
    ticker.abort();
    (out, n)
}

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
    fn dummy_hash_is_valid_and_verify_dummy_does_real_work() {
        // The dummy hash must be a valid PHC string. Otherwise
        // verify() returns Err early, skips the argon2 work, and the
        // timing gap it exists to close comes back.
        assert!(!verify("whatever-an-attacker-types", dummy_hash()).unwrap());
        // The public entry point never panics.
        verify_dummy("whatever-an-attacker-types");
    }

    /// On a current-thread runtime an inline hash freezes every other
    /// task; off the runtime a 1 ms ticker keeps running (#1709).
    #[tokio::test(flavor = "current_thread")]
    async fn async_variants_do_not_block_the_runtime() {
        let (h, n) = ticks_while(hash_async("correct horse battery staple")).await;
        assert!(n >= 2, "hash_async stalled the runtime ({n} ticks)");
        let h = h.unwrap();
        let (ok, n) = ticks_while(verify_async("correct horse battery staple", &h)).await;
        assert!(ok.unwrap());
        assert!(n >= 2, "verify_async stalled the runtime ({n} ticks)");
        let ((), n) = ticks_while(verify_dummy_async("nobody")).await;
        assert!(n >= 2, "verify_dummy_async stalled the runtime ({n} ticks)");
    }

    #[tokio::test]
    async fn off_runtime_resumes_the_original_panic() {
        struct Marker;
        let err = tokio::spawn(off_runtime(|| std::panic::panic_any(Marker)))
            .await
            .unwrap_err();
        assert!(err.into_panic().downcast::<Marker>().is_ok());
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
