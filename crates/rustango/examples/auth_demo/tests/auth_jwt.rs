//! Backing test for `docs/auth-jwt.md` — standalone HS256 JWTs. Pure, no DB.
//!
//! Run: `cargo test -p auth_demo --test auth_jwt`

use std::time::Duration;

use rustango::jwt::{decode, decode_at, encode, Claims, JwtError};

// HS256 is symmetric: the same secret signs and verifies. Must be >= 32 bytes.
const SECRET: &[u8] = b"a-shared-signing-secret-at-least-32-bytes!!";

#[test]
fn issue_and_verify_with_custom_claims() {
    let mut claims = Claims::new("user-42")
        .ttl(Duration::from_secs(900))
        .issuer("api.example.com");
    claims.set("roles", vec!["editor", "author"]);

    let token = encode(&claims, SECRET).unwrap();
    assert_eq!(token.matches('.').count(), 2, "header.payload.signature");

    let v = decode(&token, SECRET).unwrap();
    assert_eq!(v.subject(), Some("user-42"));
    assert_eq!(
        v.get::<Vec<String>>("roles").unwrap(),
        vec!["editor".to_string(), "author".to_string()]
    );
    // IMPORTANT: decode() verifies signature + exp/nbf but does NOT check
    // `iss`/`aud` — you must validate those yourself against expected values.
    assert_eq!(v.get::<String>("iss").as_deref(), Some("api.example.com"));
}

#[test]
fn wrong_secret_and_tampering_are_rejected() {
    let token = encode(&Claims::new("alice"), SECRET).unwrap();

    // A different secret fails the signature check.
    assert!(matches!(
        decode(&token, b"the-wrong-secret-also-32-bytes-long-xx!"),
        Err(JwtError::BadSignature)
    ));

    // Flip a byte of the signature → tamper detected (no valid forgery).
    let mut bytes = token.into_bytes();
    let last = bytes.len() - 1;
    bytes[last] ^= 0x01;
    let tampered = String::from_utf8(bytes).unwrap();
    assert!(decode(&tampered, SECRET).is_err());
}

#[test]
fn expiry_is_enforced() {
    // `decode_at` lets the test pin "now"; in real code use `decode`.
    let token = encode(&Claims::new("x").expires_at(1000), SECRET).unwrap();
    assert!(decode_at(&token, SECRET, 500).is_ok()); // before exp
    assert!(matches!(
        decode_at(&token, SECRET, 2000),
        Err(JwtError::Expired(_))
    )); // after exp
}

#[test]
fn a_short_secret_is_refused_at_sign_time() {
    // A sub-32-byte key is guessable/forgeable; encode refuses to sign with it.
    assert!(encode(&Claims::new("x"), b"too-short").is_err());
}
