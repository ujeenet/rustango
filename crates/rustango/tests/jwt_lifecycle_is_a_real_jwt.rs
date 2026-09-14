//! `JwtLifecycle` issues actual JWTs (#1397).
//!
//! It used to emit `base64url(payload).base64url(signature)` — two
//! segments, no JOSE header, no `alg`, signed over the payload alone.
//! Every doc called them JWTs, the types are called `JwtLifecycle` /
//! `JwtBackend`, the route is `jwt_router`, and `/api/auth/login`
//! returned them. Nothing outside rustango could read one: not jwt.io,
//! not any language's standard library, not an API gateway asked to
//! validate a JWT — and not `rustango::jwt::decode`, which rejected the
//! framework's own tokens as malformed.
//!
//! The decisive test is `rustangos_own_decoder_accepts_the_token`. Two
//! JWT implementations in one crate, only one of them producing JWTs,
//! is what made this invisible: each had tests, and neither had a test
//! that crossed.

#![cfg(feature = "tenancy")]

use base64::Engine as _;
use rustango::tenancy::jwt_lifecycle::JwtLifecycle;

fn secret() -> Vec<u8> {
    vec![7u8; 32]
}

fn b64(part: &str) -> Vec<u8> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(part)
        .expect("segment is base64url")
}

/// Three segments, and the first one is a JOSE header naming HS256.
#[test]
fn issued_tokens_have_a_jose_header() {
    let life = JwtLifecycle::new(secret());
    let pair = life.issue_pair(42);

    let parts: Vec<&str> = pair.access.split('.').collect();
    assert_eq!(
        parts.len(),
        3,
        "a JWT is three segments; got {}: {}",
        parts.len(),
        pair.access
    );

    let header: serde_json::Value = serde_json::from_slice(&b64(parts[0])).expect("header is JSON");
    assert_eq!(header["alg"], "HS256", "header must name the algorithm");
    assert_eq!(header["typ"], "JWT");
}

/// The claims still say what they said — this changed the envelope, not
/// the contents.
#[test]
fn the_payload_still_carries_the_reserved_claims() {
    let life = JwtLifecycle::new(secret());
    let pair = life.issue_pair(42);
    let parts: Vec<&str> = pair.access.split('.').collect();

    let payload: serde_json::Value =
        serde_json::from_slice(&b64(parts[1])).expect("payload is JSON");
    assert_eq!(payload["sub"], 42);
    assert_eq!(payload["typ"], "access");
    assert!(payload["exp"].is_i64());
    assert!(payload["jti"].is_string());
}

/// The one that matters: rustango's *other* JWT implementation can read
/// these now. It could not before — the framework rejected its own
/// tokens, which is the sharpest possible statement that they were not
/// JWTs.
///
/// Gated on `jwt` rather than gating the file: `tenancy::jwt_lifecycle`
/// needs only `tenancy`, so gating the whole file would drop the other
/// ten tests from every build without the `jwt` feature — including the
/// MCP ones, which do not need it.
#[cfg(feature = "jwt")]
#[test]
fn rustangos_own_decoder_accepts_the_token() {
    let life = JwtLifecycle::new(secret());
    let pair = life.issue_pair(42);

    let claims = rustango::jwt::decode(&pair.access, &secret())
        .expect("rustango::jwt::decode must accept a token rustango issued");

    assert_eq!(
        claims.get::<i64>("sub"),
        Some(42),
        "the standard decoder must read the same subject the issuer put in"
    );
}

/// Round trip through the issuer's own verifier still works.
#[tokio::test]
async fn issued_tokens_still_verify() {
    let life = JwtLifecycle::new(secret());
    let pair = life.issue_pair(42);

    let claims = life
        .verify_access(&pair.access)
        .await
        .expect("freshly issued token must verify");
    assert_eq!(claims.sub, 42);
    assert_eq!(claims.typ, "access");
}

/// A token in the pre-#1397 two-segment shape still verifies, so
/// upgrading the framework does not log out everyone holding one that
/// has not expired.
///
/// Built by hand rather than by an old code path, because the old code
/// path is gone — which is the point: this pins the compatibility
/// window rather than the implementation that needed it.
#[tokio::test]
async fn legacy_two_segment_tokens_still_verify() {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    let life = JwtLifecycle::new(secret());
    let exp = chrono::Utc::now().timestamp() + 600;
    let payload = serde_json::json!({
        "sub": 42, "exp": exp, "jti": "legacy-jti", "typ": "access",
    });
    let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&payload).unwrap());

    // Legacy signing input: the payload alone, no header.
    let mut mac = <Hmac<Sha256>>::new_from_slice(&secret()).unwrap();
    mac.update(payload_b64.as_bytes());
    let sig_b64 =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());

    let legacy = format!("{payload_b64}.{sig_b64}");
    assert_eq!(legacy.split('.').count(), 2);

    let claims = life
        .verify_access(&legacy)
        .await
        .expect("a token issued before #1397 must still verify");
    assert_eq!(claims.sub, 42);
}

/// Compatibility is not credulity: a two-segment token signed with the
/// wrong key is still refused.
///
/// Without this, "accept the old shape" could have meant "accept
/// anything shaped like the old one".
#[tokio::test]
async fn a_legacy_token_with_a_bad_signature_is_refused() {
    let life = JwtLifecycle::new(secret());
    let exp = chrono::Utc::now().timestamp() + 600;
    let payload = serde_json::json!({
        "sub": 42, "exp": exp, "jti": "forged", "typ": "access",
    });
    let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&payload).unwrap());
    let forged = format!("{payload_b64}.{}", "A".repeat(43));

    assert!(
        life.verify_access(&forged).await.is_none(),
        "a legacy-shaped token with a bad signature must be refused"
    );
}

/// `alg: none` is rejected as such. The HMAC check over `header.payload`
/// would catch it anyway — an attacker without the key cannot produce a
/// matching signature — but rejecting at the header means the failure
/// names the real reason.
#[tokio::test]
async fn alg_none_is_refused() {
    let life = JwtLifecycle::new(secret());
    let exp = chrono::Utc::now().timestamp() + 600;

    let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&serde_json::json!({"alg": "none", "typ": "JWT"})).unwrap());
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&serde_json::json!({
            "sub": 42, "exp": exp, "jti": "x", "typ": "access",
        }))
        .unwrap(),
    );

    assert!(
        life.verify_access(&format!("{header}.{payload}."))
            .await
            .is_none(),
        "alg: none must be refused"
    );
}

// ------------------------------------------------------------- claims

/// Custom claims survive the envelope change, and a *standard* decoder
/// can read them — including the non-scalar ones, which is what a client
/// actually needs.
///
/// Changing the envelope must not change the contents. Asserting through
/// `rustango::jwt::decode` rather than the issuer's own verifier is the
/// point: it is the standard reader that had never been able to see any
/// of this.
#[cfg(feature = "jwt")]
#[test]
fn custom_claims_round_trip_through_a_standard_decoder() {
    let life = JwtLifecycle::new(secret());
    let mut custom = serde_json::Map::new();
    custom.insert("tenant".into(), serde_json::json!("acme"));
    custom.insert("role".into(), serde_json::json!("editor"));
    custom.insert(
        "scopes".into(),
        serde_json::json!(["read:posts", "write:posts"]),
    );
    custom.insert("nested".into(), serde_json::json!({"a": {"b": 1}}));

    let token = life
        .issue_access_with(42, custom)
        .expect("non-reserved claims are accepted");

    let claims = rustango::jwt::decode(&token, &secret()).expect("decodes as a standard JWT");

    assert_eq!(claims.get::<i64>("sub"), Some(42));
    assert_eq!(claims.get::<String>("tenant").as_deref(), Some("acme"));
    assert_eq!(claims.get::<String>("role").as_deref(), Some("editor"));
    assert_eq!(
        claims.get::<Vec<String>>("scopes"),
        Some(vec!["read:posts".to_owned(), "write:posts".to_owned()]),
        "array claims must survive — a scope list is the common case"
    );
    assert_eq!(
        claims
            .get::<serde_json::Value>("nested")
            .and_then(|v| v["a"]["b"].as_i64()),
        Some(1),
        "nested objects too"
    );
}

/// And the issuer's own verifier still reads them back typed.
#[tokio::test]
async fn custom_claims_round_trip_through_the_issuer() {
    let life = JwtLifecycle::new(secret());
    let mut custom = serde_json::Map::new();
    custom.insert("tenant".into(), serde_json::json!("acme"));
    custom.insert("scopes".into(), serde_json::json!(["a", "b"]));

    let token = life.issue_access_with(7, custom).expect("issue");
    let claims = life.verify_access(&token).await.expect("verify");

    assert_eq!(claims.sub, 7);
    assert_eq!(
        claims.get_custom::<String>("tenant").as_deref(),
        Some("acme")
    );
    assert_eq!(
        claims.get_custom::<Vec<String>>("scopes"),
        Some(vec!["a".to_owned(), "b".to_owned()])
    );
}

// --------------------------------------------------------- MCP agents

/// MCP agent authentication rides the same issuer, so it changed shape
/// too. Its tokens carry five custom claims — two of them arrays — and
/// a tenant that is security-relevant rather than decorative.
///
/// Split from the standard-decoder assertion below so this half keeps
/// running in builds without `jwt`: `mcp` does not imply that feature,
/// and the agent-resolution path is worth checking either way.
#[cfg(feature = "mcp")]
#[tokio::test]
async fn mcp_agent_tokens_are_jwt_shaped_and_still_resolve() {
    use rustango::mcp::auth::{issue_agent_token, verify_agent_token};

    let life = JwtLifecycle::new(secret());
    let skills = vec!["search".to_owned(), "summarise".to_owned()];
    let tools = vec!["fetch".to_owned()];

    let token =
        issue_agent_token(&life, 99, "acme", &skills, &tools, Some(5)).expect("agent token issues");

    assert_eq!(
        token.split('.').count(),
        3,
        "an agent token is a JWT like any other"
    );

    let agent = verify_agent_token(&life, &token, "acme")
        .await
        .expect("agent token must still verify");
    assert_eq!(agent.agent_id, 99);
    assert_eq!(agent.tenant, "acme");
    assert_eq!(agent.skills, skills);
    assert_eq!(agent.tools, tools);
    assert_eq!(agent.user_id, Some(5));
}

/// And every one of those claims is readable by a standard decoder —
/// the half that needs `rustango::jwt`.
#[cfg(all(feature = "mcp", feature = "jwt"))]
#[test]
fn mcp_agent_claims_are_readable_by_a_standard_decoder() {
    use rustango::mcp::auth::issue_agent_token;

    let life = JwtLifecycle::new(secret());
    let skills = vec!["search".to_owned(), "summarise".to_owned()];
    let tools = vec!["fetch".to_owned()];
    let token =
        issue_agent_token(&life, 99, "acme", &skills, &tools, Some(5)).expect("agent token issues");

    let std_claims = rustango::jwt::decode(&token, &secret()).expect("standard decode");
    assert_eq!(std_claims.get::<String>("kind").as_deref(), Some("agent"));
    assert_eq!(std_claims.get::<String>("tenant").as_deref(), Some("acme"));
    assert_eq!(std_claims.get::<Vec<String>>("skills"), Some(skills));
    assert_eq!(std_claims.get::<Vec<String>>("tools"), Some(tools));
    assert_eq!(std_claims.get::<i64>("uid"), Some(5));
}

/// Tenant pinning still refuses a token minted for someone else. The
/// envelope change must not have loosened the check that stops one
/// tenant's agent acting on another's data.
#[cfg(feature = "mcp")]
#[tokio::test]
async fn an_mcp_agent_token_is_still_pinned_to_its_tenant() {
    use rustango::mcp::auth::{issue_agent_token, verify_agent_token};

    let life = JwtLifecycle::new(secret());
    let token = issue_agent_token(&life, 99, "acme", &[], &[], None).expect("issue");

    assert!(
        verify_agent_token(&life, &token, "globex").await.is_none(),
        "a token minted for acme must not verify against globex"
    );
}

/// A plain user token must not pass as an agent token. `kind` is the
/// discriminator and it lives in the payload the signature now covers.
#[cfg(feature = "mcp")]
#[tokio::test]
async fn a_non_agent_token_is_refused_by_the_mcp_path() {
    use rustango::mcp::auth::verify_agent_token;

    let life = JwtLifecycle::new(secret());
    let pair = life.issue_pair(42); // ordinary login token, no `kind`

    assert!(
        verify_agent_token(&life, &pair.access, "acme")
            .await
            .is_none(),
        "an ordinary access token must not be accepted as an agent"
    );
}

/// A tampered payload fails, whichever shape it is in — the signature
/// now covers the header too.
#[tokio::test]
async fn tampering_with_the_payload_invalidates_the_token() {
    let life = JwtLifecycle::new(secret());
    let pair = life.issue_pair(42);
    let parts: Vec<&str> = pair.access.split('.').collect();

    let mut payload: serde_json::Value = serde_json::from_slice(&b64(parts[1])).unwrap();
    payload["sub"] = serde_json::Value::from(1);
    let swapped = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&payload).unwrap());

    let tampered = format!("{}.{}.{}", parts[0], swapped, parts[2]);
    assert!(
        life.verify_access(&tampered).await.is_none(),
        "swapping the subject must invalidate the signature"
    );
}
