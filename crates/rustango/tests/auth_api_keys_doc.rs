//! Backing test for `docs/auth-api-keys.md` — the standalone `api_keys` helper
//! (generate → store prefix+hash → verify). Pure, no DB.
//!
//! Run: `cargo test -p rustango --test auth_api_keys_doc`

#![cfg(feature = "api_keys")]

use rustango::api_keys::{generate_key, hash_secret, split_token, verify_key};

#[test]
fn generate_split_verify_roundtrip() {
    // generate_key() returns (full_token, prefix, hash). The full token —
    // "{prefix}.{secret}" — is shown to the user ONCE; you persist only the
    // 8-char prefix (lookup index) and the argon2id hash of the secret.
    let (token, prefix, hash) = generate_key().unwrap();
    assert_eq!(prefix.len(), 8, "prefix is an 8-char lookup key");
    assert!(token.starts_with(&prefix));
    assert!(token.contains('.'), "token is prefix.secret");
    assert!(hash.starts_with("$argon2id$"), "secret is argon2id-hashed");

    // On an incoming request: split the token, look up the row by prefix,
    // then verify the secret against the stored hash.
    let (p, secret) = split_token(&token).unwrap();
    assert_eq!(p, prefix);
    assert!(
        verify_key(secret, &hash).unwrap(),
        "correct secret verifies"
    );
    assert!(
        !verify_key("not-the-secret", &hash).unwrap(),
        "wrong secret is rejected"
    );
}

#[test]
fn split_token_rejects_malformed_tokens() {
    assert!(split_token("no-dot-here").is_none());
    assert!(split_token("short.secret").is_none()); // prefix must be 8 chars
    assert!(split_token("abcd1234.").is_none()); // empty secret
    assert!(split_token("abcd1234.realsecret").is_some());
}

#[test]
fn each_key_is_unique_and_hashing_is_salted() {
    let (t1, p1, _) = generate_key().unwrap();
    let (t2, p2, _) = generate_key().unwrap();
    assert_ne!(t1, t2);
    assert_ne!(p1, p2);

    // Hashing the same secret twice yields different PHC strings (random
    // per-hash salt), yet both verify.
    let h1 = hash_secret("same-secret").unwrap();
    let h2 = hash_secret("same-secret").unwrap();
    assert_ne!(h1, h2);
    assert!(verify_key("same-secret", &h1).unwrap());
    assert!(verify_key("same-secret", &h2).unwrap());
}
