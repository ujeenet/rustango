//! Static files, security headers and the test client build and work
//! without the `admin` feature. Only compiles with `admin` off:
//!   cargo test -p rustango --no-default-features \
//!     --features sqlite,static-files,security-headers,test-client --test http_without_admin
#![cfg(all(
    feature = "static-files",
    feature = "security-headers",
    feature = "test-client",
    not(feature = "admin")
))]

use rustango::security_headers::{CspBuilder, SecurityHeadersLayer, SecurityHeadersRouterExt};
use rustango::static_files::{static_router, StaticFiles};
use rustango::test_client::TestClient;

#[tokio::test]
async fn a_wasm_app_serves_with_its_csp_and_no_admin() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("app.wasm"), b"\0asm").unwrap();
    let csp = CspBuilder::strict_wasm_app().build();
    let app = axum::Router::new()
        .nest("/static", static_router(StaticFiles::new(dir.path())))
        .security_headers(SecurityHeadersLayer::strict().csp(csp.clone()));
    let res = TestClient::new(app).get("/static/app.wasm").send().await;
    assert_eq!(res.status, 200);
    assert_eq!(res.header("content-type"), Some("application/wasm"));
    assert_eq!(res.header("content-security-policy"), Some(csp.as_str()));
}
