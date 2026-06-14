//! Verifies the getting-started JWT (Step 14) and security-middleware
//! (Step 15) snippets compile and run against the real framework API.

use std::time::Duration;

// ---- Step 14: JWT issue / verify with the standalone `rustango::jwt` ----
use rustango::jwt::{decode, encode, Claims};

#[test]
fn jwt_issue_and_verify_roundtrip() {
    let secret = b"verification-only-secret-at-least-32-bytes-long-xxxxx";

    // Issue: bake claims (subject = user id, custom roles) into a signed token.
    let mut claims = Claims::new("42");
    claims.set("roles", vec!["editor"]);
    let token = encode(&claims.ttl(Duration::from_secs(900)), secret).unwrap();

    // Verify: decode + check signature/expiry, then read the claims back.
    let decoded = decode(&token, secret).unwrap();
    assert_eq!(decoded.subject(), Some("42"));
    let roles: Vec<String> = decoded.get("roles").unwrap();
    assert_eq!(roles, vec!["editor".to_string()]);

    // A tampered secret must be rejected.
    assert!(decode(&token, b"the-wrong-secret-the-wrong-secret-xxxxx").is_err());
}

// ---- Step 15: the full security-middleware stack ----
use getting_started_blog::{post_view_set::PostViewSet, urls};
use rustango::access_log::{AccessLogLayer, AccessLogRouterExt};
use rustango::cors::{CorsLayer, CorsRouterExt};
use rustango::health::health_router;
use rustango::rate_limit::{RateLimitLayer, RateLimitRouterExt};
use rustango::request_id::{RequestIdLayer, RequestIdRouterExt};
use rustango::security_headers::{CspBuilder, SecurityHeadersLayer, SecurityHeadersRouterExt};
use rustango::sql::sqlx::PgPool;
use rustango::test_client::TestClient;

async fn secure_app() -> axum::Router {
    let pool = PgPool::connect(&std::env::var("DATABASE_URL").unwrap())
        .await
        .unwrap();

    urls::api()
        .nest("/admin", urls::admin_router(pool.clone()))
        .merge(PostViewSet::router("/api/posts", pool.clone()))
        .merge(health_router(pool.clone())) // /health, /ready
        .request_id(RequestIdLayer::default())
        .access_log(AccessLogLayer::default()) // PII-redacted
        .rate_limit(RateLimitLayer::per_ip(60, Duration::from_secs(60)))
        .cors(
            CorsLayer::new()
                .allow_origins(vec!["https://app.example.com"])
                .allow_methods(vec!["GET", "POST", "PUT", "PATCH", "DELETE"]),
        )
        .security_headers(SecurityHeadersLayer::strict().csp(CspBuilder::strict_starter().build()))
}

#[tokio::test]
async fn security_stack_serves_requests() {
    let client = TestClient::new(secure_app().await);
    let r = client.get("/api/posts").send().await;
    assert_eq!(r.status, 200);
}
