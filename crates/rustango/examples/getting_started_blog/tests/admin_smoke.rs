//! Verifies the auto-admin from Step 11 — the home renders and the
//! `posts` model page loads, both at the `/admin` mount (no trailing
//! slash) the `admin_prefix("/admin")` setting produces.
//!
//! Requires the schema to exist: run `cargo run -- migrate` first
//! (CI does this before `cargo test`).

use getting_started_blog::urls;
use rustango::sql::sqlx::PgPool;
use rustango::test_client::TestClient;

async fn admin_app() -> axum::Router {
    let pool = PgPool::connect(&std::env::var("DATABASE_URL").unwrap())
        .await
        .unwrap();
    urls::api().nest("/admin", urls::admin_router(pool))
}

#[tokio::test]
async fn admin_home_loads() {
    let client = TestClient::new(admin_app().await);
    let r = client.get("/admin").send().await;
    assert_eq!(r.status, 200);
    // Links use the configured `/admin` prefix (hrefs are HTML-escaped),
    // not the default `/__admin` — confirms admin_prefix matches the mount.
    assert!(!r.text().contains("__admin"));
}

#[tokio::test]
async fn admin_posts_list_loads() {
    let client = TestClient::new(admin_app().await);
    let r = client.get("/admin/posts").send().await;
    assert_eq!(r.status, 200);
}
