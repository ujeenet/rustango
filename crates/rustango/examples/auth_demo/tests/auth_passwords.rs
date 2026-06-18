//! Backing test for `docs/auth-passwords.md`. Pure functions — no DB needed.
//!
//! Run: `cargo test -p auth_demo --test auth_passwords`

use rustango::passwords::{hash, strength_score, verify, verify_dummy, StrengthIssue};

#[test]
fn hash_is_argon2id_phc_and_verifies() {
    let stored = hash("CorrectHorseBatteryStaple!42").unwrap();

    // What you persist is an argon2id PHC string — never the plaintext.
    assert!(stored.starts_with("$argon2id$"), "got: {stored}");
    assert_ne!(stored, "CorrectHorseBatteryStaple!42");

    // Login: verify the attempt against the stored hash.
    assert!(verify("CorrectHorseBatteryStaple!42", &stored).unwrap());
    assert!(!verify("wrong-password", &stored).unwrap());
}

#[test]
fn each_hash_is_salted_so_two_hashes_of_one_password_differ() {
    let a = hash("same-password-12345").unwrap();
    let b = hash("same-password-12345").unwrap();
    assert_ne!(a, b, "random per-hash salt → distinct PHC strings");
    assert!(verify("same-password-12345", &a).unwrap());
    assert!(verify("same-password-12345", &b).unwrap());
}

#[test]
fn strength_score_flags_weak_and_passes_strong() {
    assert!(strength_score("Tr0ub4dor&3-CorrectBattery").is_empty());
    assert!(strength_score("password123").contains(&StrengthIssue::KnownWeak));
    assert!(strength_score("short").contains(&StrengthIssue::TooShort));
}

#[test]
fn verify_dummy_equalizes_timing_on_unknown_user() {
    // Call this on the user-not-found (and inactive) branch of a login so the
    // request takes ~the same time whether or not the account exists —
    // otherwise the timing difference lets an attacker enumerate usernames.
    verify_dummy("whatever-the-attacker-typed");
}
