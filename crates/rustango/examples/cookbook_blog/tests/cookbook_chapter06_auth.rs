//! Cookbook Chapter 6 — auth + permissions primitives.
//!
//! Covers password hashing (Argon2), API-key issue + verify, JWT
//! encode + decode + expiry, and the typed-permission codename
//! generator. All hot-path crypto — no DB needed.
//!
//! Run: `cargo test --test cookbook_chapter06_auth`

// §6.83 — Argon2 password hash + verify round-trip.
#[test]
fn passwords_hash_and_verify_round_trip() {
    let stored = rustango::passwords::hash("hunter2").expect("hash");
    assert!(rustango::passwords::verify("hunter2", &stored).unwrap());
    assert!(!rustango::passwords::verify("wrong-password", &stored).unwrap());
    // Two hashes of the same password differ — Argon2 includes a salt.
    let stored2 = rustango::passwords::hash("hunter2").unwrap();
    assert_ne!(stored, stored2, "salt must vary per hash");
}

// §6.83 — strength_score returns issues for short / weak passwords.
#[test]
fn passwords_strength_score_flags_weak() {
    let issues = rustango::passwords::strength_score("a");
    assert!(!issues.is_empty(), "single-char password should flag at least one issue");
    let strong = rustango::passwords::strength_score("Sup3rL0ng!Passw0rd-2026");
    assert!(strong.is_empty() || strong.len() < issues.len(),
        "strong password should have fewer issues than `a`");
}

// §6.84 — API-key issuance: generate(), then verify the secret
// against the stored hash.
#[test]
fn api_keys_issue_then_verify() {
    let (token, prefix, hash) = rustango::api_keys::generate_key().expect("generate");
    // The token format is `<prefix>.<secret>` — split surfaces both halves.
    let (split_prefix, secret) = rustango::api_keys::split_token(&token)
        .expect("token splits into prefix + secret");
    assert_eq!(split_prefix, prefix);
    assert!(rustango::api_keys::verify_key(secret, &hash).unwrap());
    assert!(!rustango::api_keys::verify_key("wrong-secret", &hash).unwrap());
}

// §6.85 — JWT issue + verify with HMAC secret + ttl.
#[test]
fn jwt_round_trips_with_ttl_and_subject() {
    use rustango::jwt::{decode, encode, Claims};
    let secret = b"cookbook-test-secret-32-bytes-or-more-please";
    let claims = Claims::new("user-42")
        .issuer("cookbook")
        .audience("blog")
        .ttl(std::time::Duration::from_secs(300));
    let token = encode(&claims, secret).expect("encode");
    let back = decode(&token, secret).expect("decode");
    assert_eq!(back.subject(), Some("user-42"));
    assert_eq!(back.get::<String>("iss").as_deref(), Some("cookbook"));
    assert_eq!(back.get::<String>("aud").as_deref(), Some("blog"));
    assert!(back.get::<u64>("exp").unwrap() > back.get::<u64>("iat").unwrap());
}

// §6.85 — JWT decode rejects wrong secret + tampered token.
#[test]
fn jwt_rejects_wrong_secret_and_tampered_token() {
    use rustango::jwt::{decode, encode, Claims};
    let secret = b"correct-secret-32-bytes-or-more!!!!!!!!!!";
    let other  = b"different-secret-32-bytes-or-more!!!!!!!!";
    let token = encode(&Claims::new("alice"), secret).expect("encode");

    // Wrong secret rejected.
    decode(&token, other).expect_err("different secret must fail HMAC verify");

    // Tampering the payload invalidates the signature too.
    let mut tampered = token.clone();
    tampered.push('x');
    decode(&tampered, secret).expect_err("tampered token must fail");
}

// §6.85 — JWT exp respected via decode_at.
#[test]
fn jwt_decode_at_rejects_expired_token() {
    use rustango::jwt::{decode_at, encode, Claims};
    let secret = b"chapter-6-secret-32-bytes-or-more!!!!!!!!";
    let claims = Claims::new("alice").expires_at(1_000);
    let token = encode(&claims, secret).expect("encode");
    decode_at(&token, secret, 999).expect("not yet expired at 999");
    decode_at(&token, secret, 2_000).expect_err("expired by t=2000");
}

// §6.78 / §6.79 — typed permission codename generator: `app.action_model`.
#[test]
fn permission_codename_for_model_resolves_app_action_model() {
    use rustango::permissions::codename_for;
    use cookbook_blog::apps::blog::models::Author;
    let view = codename_for::<Author>("view");
    let add  = codename_for::<Author>("add");
    let change = codename_for::<Author>("change");
    let delete = codename_for::<Author>("delete");

    // Codename format is `{app}.{action}_{model}` matching Django's
    // permission convention. The app label falls back to "project"
    // when the macro can't infer it from the module path; the model
    // name is the lowercased struct ident.
    for (name, action) in [(&view, "view"), (&add, "add"), (&change, "change"), (&delete, "delete")] {
        assert!(name.contains(action), "codename `{name}` should contain action `{action}`");
        assert!(name.contains("author"), "codename `{name}` should contain model `author`");
        assert!(name.contains('.'), "codename `{name}` should be {{app}}.{{action_model}}");
    }
}
