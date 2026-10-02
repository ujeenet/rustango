//! Backing test for `docs/security.md` — the Stripe webhook example.
//!
//! The snippet between the markers is compiled here and must appear verbatim
//! in every locale of the page, so the published code is the tested code.
#![cfg(feature = "webhook")]

#[path = "support/doc_snippet.rs"]
mod doc_snippet;

// doc-snippet:start
use axum::{body::Bytes, http::HeaderMap, http::StatusCode};
use rustango::webhook::{verify_signature, SignatureFormat};

/// Stripe sends `Stripe-Signature: t=<unix>,v1=<hex>[,v1=<hex>]`
/// and signs `"{t}.{body}"`, not the body alone.
fn verify_stripe(secret: &[u8], header: &str, body: &[u8], now: i64, tolerance: i64) -> bool {
    let mut ts = None;
    let mut sigs = Vec::new();
    for part in header.split(',') {
        match part.split_once('=') {
            Some(("t", t)) => ts = Some(t),
            Some(("v1", sig)) => sigs.push(sig),
            _ => {}
        }
    }
    let Some(ts) = ts else { return false };
    let Ok(t) = ts.parse::<i64>() else {
        return false;
    };
    if (now - t).abs() > tolerance {
        return false; // stale or future-dated: a replay
    }
    let mut signed = format!("{ts}.").into_bytes();
    signed.extend_from_slice(body);
    sigs.iter()
        .any(|sig| verify_signature(SignatureFormat::HexSha256, secret, &signed, sig))
}

async fn handle_stripe_webhook(headers: HeaderMap, body: Bytes) -> StatusCode {
    let Ok(secret) = std::env::var("STRIPE_WEBHOOK_SECRET") else {
        return StatusCode::INTERNAL_SERVER_ERROR;
    };
    let header = headers
        .get("stripe-signature")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    if !verify_stripe(secret.as_bytes(), header, &body, now, 300) {
        return StatusCode::UNAUTHORIZED;
    }
    // ... process the verified payload
    StatusCode::OK
}
// doc-snippet:end

const SECRET: &[u8] = b"whsec_test_secret";
const BODY: &[u8] = br#"{"id":"evt_test_webhook","object":"event"}"#;
const T: i64 = 1_700_000_000;
/// HMAC-SHA256(SECRET, "1700000000." + BODY), computed with Python's `hmac`.
const V1: &str = "d95c6b7477fbd7e9f90b1b0ef5f9c7ac25abca5382460e0d988c2b2a5b71b990";
/// HMAC-SHA256(SECRET, BODY) — the body-only signature Stripe never sends.
const BODY_ONLY: &str = "f1b27e09364fc7362c461d1314a4853fc2827219e604dfd604e46e40e5fb7b2f";

fn header(t: i64, sigs: &[&str]) -> String {
    let mut h = format!("t={t}");
    for s in sigs {
        h.push_str(&format!(",v1={s}"));
    }
    h
}

#[test]
fn fixed_vector_verifies() {
    assert!(verify_stripe(SECRET, &header(T, &[V1]), BODY, T + 10, 300));
}

#[test]
fn the_old_documented_call_cannot_verify_stripe() {
    // `verify_signature` over the body against the whole header was the page's example.
    let h = header(T, &[V1]);
    assert!(!verify_signature(
        SignatureFormat::HexSha256,
        SECRET,
        BODY,
        &h
    ));
    assert!(!verify_signature(
        SignatureFormat::HexSha256,
        SECRET,
        BODY,
        V1
    ));
}

#[test]
fn rejects_stale_tampered_and_body_only() {
    assert!(!verify_stripe(
        SECRET,
        &header(T, &[V1]),
        BODY,
        T + 301,
        300
    ));
    assert!(!verify_stripe(
        SECRET,
        &header(T, &[V1]),
        BODY,
        T - 301,
        300
    ));
    assert!(!verify_stripe(SECRET, &header(T, &[V1]), b"{}", T, 300));
    assert!(!verify_stripe(SECRET, &header(T + 1, &[V1]), BODY, T, 300));
    assert!(!verify_stripe(
        SECRET,
        &header(T, &[BODY_ONLY]),
        BODY,
        T,
        300
    ));
    assert!(!verify_stripe(SECRET, &format!("v1={V1}"), BODY, T, 300));
    assert!(!verify_stripe(
        b"whsec_other",
        &header(T, &[V1]),
        BODY,
        T,
        300
    ));
}

#[test]
fn any_v1_entry_may_match_during_secret_rotation() {
    assert!(verify_stripe(
        SECRET,
        &header(T, &[BODY_ONLY, V1]),
        BODY,
        T,
        300
    ));
}

#[tokio::test]
async fn handler_rejects_an_unsigned_request() {
    // Without the env var the handler fails closed; with it, a bad signature is 401.
    let status = handle_stripe_webhook(HeaderMap::new(), Bytes::from_static(BODY)).await;
    assert!(matches!(
        status,
        StatusCode::UNAUTHORIZED | StatusCode::INTERNAL_SERVER_ERROR
    ));
}

#[test]
fn every_locale_publishes_the_tested_snippet() {
    doc_snippet::assert_published(include_str!("webhook_stripe_doc.rs"), "security.md");
}
