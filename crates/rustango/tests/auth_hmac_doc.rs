//! Backing test for `docs/auth-hmac.md` — HMAC request signing: a client signs
//! (method + path + sorted query + X-Date + body-hash), the `HmacAuthLayer`
//! verifies. Tamper with anything and it's a 401.
//!
//! Run: `cargo test -p rustango --test auth_hmac_doc`

#![cfg(feature = "hmac-auth")]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::routing::post;
use axum::Router;
use rustango::hmac_auth::{sign_now, HmacAuthLayer, KeyResolver};
use tower::{Layer, ServiceExt};

const KEY_ID: &str = "k_demo";
const SECRET: &[u8] = b"shared-secret-at-least-32-bytes-long!!";

/// Server side: map a key id to its secret. `None` → 401 "unknown key id".
fn resolver() -> KeyResolver {
    Arc::new(|key_id: &str| (key_id == KEY_ID).then(|| SECRET.to_vec()))
}

/// Wrap a one-route app in the HMAC layer and drive one request through it.
async fn call(req: Request<Body>) -> StatusCode {
    let app = Router::new().route("/api/run", post(|| async { "ok" }));
    let svc = HmacAuthLayer::new(resolver()).layer(app.into_service::<Body>());
    svc.oneshot(req).await.unwrap().status()
}

/// Client side: sign with `sign_now`, then attach the two headers it returns.
fn signed(method: &str, path: &str, query: &str, body: &[u8]) -> Request<Body> {
    let (x_date, authorization) = sign_now(KEY_ID, SECRET, method, path, query, body);
    let uri = if query.is_empty() {
        path.to_owned()
    } else {
        format!("{path}?{query}")
    };
    Request::builder()
        .method(method)
        .uri(uri)
        .header("x-date", x_date)
        .header(header::AUTHORIZATION, authorization)
        .body(Body::from(body.to_vec()))
        .unwrap()
}

#[tokio::test]
async fn correctly_signed_request_passes() {
    let req = signed("POST", "/api/run", "", br#"{"x":1}"#);
    assert_eq!(call(req).await, StatusCode::OK);
}

#[tokio::test]
async fn query_order_does_not_matter() {
    // The signer and verifier both sort the query, so reordering is fine.
    let req = signed("POST", "/api/run", "b=2&a=1", b"");
    assert_eq!(call(req).await, StatusCode::OK);
}

#[tokio::test]
async fn tampered_body_is_rejected() {
    // Sign one body, ship a different one → signature no longer matches.
    let (x_date, authorization) = sign_now(KEY_ID, SECRET, "POST", "/api/run", "", br#"{"x":1}"#);
    let req = Request::builder()
        .method("POST")
        .uri("/api/run")
        .header("x-date", x_date)
        .header(header::AUTHORIZATION, authorization)
        .body(Body::from(&b"{\"x\":999}"[..]))
        .unwrap();
    assert_eq!(call(req).await, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn missing_date_and_unknown_key_are_rejected() {
    // No X-Date header → 401.
    let (_x_date, authorization) = sign_now(KEY_ID, SECRET, "POST", "/api/run", "", b"");
    let no_date = Request::builder()
        .method("POST")
        .uri("/api/run")
        .header(header::AUTHORIZATION, authorization)
        .body(Body::empty())
        .unwrap();
    assert_eq!(call(no_date).await, StatusCode::UNAUTHORIZED);

    // Signed with a key the resolver doesn't know → 401.
    let unknown = {
        let (x_date, authorization) = sign_now("k_unknown", SECRET, "POST", "/api/run", "", b"");
        Request::builder()
            .method("POST")
            .uri("/api/run")
            .header("x-date", x_date)
            .header(header::AUTHORIZATION, authorization)
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(call(unknown).await, StatusCode::UNAUTHORIZED);
}
