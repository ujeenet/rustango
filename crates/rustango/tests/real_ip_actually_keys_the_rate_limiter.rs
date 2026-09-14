//! Per-IP rate limiting works behind a proxy, and only for declared
//! proxies (#1398).
//!
//! `docs/security.md` used to prescribe pairing `per_ip` with `real_ip`.
//! That did nothing: `RealIpLayer` inserts a `RealIp` extension and never
//! touches `ConnectInfo`, and neither limiter had heard of `RealIp`. So
//! every client behind the proxy shared the proxy's single bucket — one
//! noisy client throttled everyone, and no attacker was ever individually
//! limited.
//!
//! The obvious repair is a trap, and it is the reason this file exists.
//! Simply keying the limiter on `RealIp` would have traded a coarse limit
//! for **no limit at all**: `X-Forwarded-For` is set by whoever sends it,
//! so any client could mint a fresh bucket per request by varying a
//! header. `RealIpLayer` has no trusted-proxy check of its own — the
//! `HeaderStrategy` picks *which header to read*, not *whom to believe*.
//!
//! So the trusted address is a distinct extension, `TrustedRealIp`, which
//! only appears when the connecting socket matches
//! `RealIpLayer::trust_proxies`. The limiter keys on that and never on
//! the bare claim. The operator names the hops; nothing is inferred.

#![cfg(feature = "admin")]

use std::net::SocketAddr;
use std::time::Duration;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::Router;
use rustango::rate_limit::{RateLimitLayer, RateLimitRouterExt};
use rustango::real_ip::{HeaderStrategy, RealIpLayer, RealIpRouterExt};
use tower::ServiceExt;

/// The operator's ingress, named in `trust_proxies` below.
const PROXY: &str = "10.0.0.1:443";
/// Someone talking to the server directly, not via the ingress.
const DIRECT: &str = "198.51.100.9:51000";

/// One request per minute per key, so the second request from the same
/// bucket is unambiguously a 429 and nothing refills mid-test.
fn app(real_ip: Option<RealIpLayer>) -> Router {
    let app = Router::new()
        .route("/", get(|| async { "ok" }))
        .rate_limit(RateLimitLayer::per_ip(1, Duration::from_secs(60)));
    // `.layer` applies outermost-LAST, so the real-ip layer is added
    // after the limiter in order to run before it.
    match real_ip {
        Some(layer) => app.real_ip(layer),
        None => app,
    }
}

fn trusting() -> RealIpLayer {
    RealIpLayer::new(HeaderStrategy::XForwardedFor)
        .trust_proxies(["10.0.0.0/8"])
        .expect("valid CIDR")
}

/// A request arriving from `peer`, claiming to be forwarded on behalf of
/// `forwarded_for`.
async fn get_via(app: &Router, peer: &str, forwarded_for: &str) -> StatusCode {
    let mut req = Request::builder()
        .uri("/")
        .header("x-forwarded-for", forwarded_for)
        .body(Body::empty())
        .unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
    app.clone().oneshot(req).await.unwrap().status()
}

/// The common case: everything arrives through the declared ingress.
async fn get_as(app: &Router, forwarded_for: &str) -> StatusCode {
    get_via(app, PROXY, forwarded_for).await
}

/// What the page promised. Two clients behind one declared proxy get two
/// buckets.
#[tokio::test]
async fn two_clients_behind_a_trusted_proxy_do_not_share_a_bucket() {
    let app = app(Some(trusting()));

    assert_eq!(get_as(&app, "203.0.113.7").await, StatusCode::OK);
    assert_eq!(
        get_as(&app, "203.0.113.8").await,
        StatusCode::OK,
        "a second client behind the same proxy must have its own bucket — \
         this is what per-IP limiting behind a proxy is supposed to buy"
    );
}

/// The guard against "give everyone their own bucket and never limit".
#[tokio::test]
async fn one_client_behind_a_trusted_proxy_is_still_limited() {
    let app = app(Some(trusting()));

    assert_eq!(get_as(&app, "203.0.113.7").await, StatusCode::OK);
    assert_eq!(
        get_as(&app, "203.0.113.7").await,
        StatusCode::TOO_MANY_REQUESTS,
        "the limiter must still fire for a client that exceeds its own limit"
    );
}

/// **The one that matters.** A client talking to the server directly
/// cannot mint itself a bucket per request by varying the header.
///
/// This is the whole reason the trusted address is a separate extension.
/// Key the limiter on the bare `RealIp` and this test is how you find
/// out: two requests, two invented header values, no limit ever reached.
#[tokio::test]
async fn a_direct_client_cannot_spoof_its_way_out_of_the_limit() {
    let app = app(Some(trusting()));

    assert_eq!(
        get_via(&app, DIRECT, "203.0.113.7").await,
        StatusCode::OK,
        "first request from an untrusted peer is allowed"
    );
    assert_eq!(
        get_via(&app, DIRECT, "203.0.113.8").await,
        StatusCode::TOO_MANY_REQUESTS,
        "an untrusted peer must stay on its socket's bucket — a forwarding \
         header from a client is a claim, not an address"
    );
}

/// Declaring no proxies means believing no headers, even with the layer
/// mounted. Pinned because it is the default, and because an operator who
/// mounts the layer for logging must not silently change how the limiter
/// buckets.
#[tokio::test]
async fn without_trust_proxies_the_header_does_not_key_the_limiter() {
    let app = app(Some(RealIpLayer::new(HeaderStrategy::XForwardedFor)));

    assert_eq!(get_as(&app, "203.0.113.7").await, StatusCode::OK);
    assert_eq!(
        get_as(&app, "203.0.113.8").await,
        StatusCode::TOO_MANY_REQUESTS,
        "RealIp alone is a claim; only trust_proxies makes it an address"
    );
}

/// Without the layer at all, the limiter keys on the socket exactly as
/// before. The fallback is what makes this safe to ship.
#[tokio::test]
async fn without_the_layer_a_forwarded_header_is_ignored() {
    let app = app(None);

    assert_eq!(get_as(&app, "203.0.113.7").await, StatusCode::OK);
    assert_eq!(
        get_as(&app, "203.0.113.8").await,
        StatusCode::TOO_MANY_REQUESTS,
        "an unmounted RealIpLayer must mean the header is not trusted at all"
    );
}
