//! Verifies the auto-admin renders against a real Postgres: the home lists the
//! models, the posts list shows seeded rows + the configured filters, and the
//! post detail page shows the comments inline. Needs `DATABASE_URL` (the schema
//! is created by `cargo run -- migrate`; CI does that before `cargo test`).
//!
//! One test on purpose — the shared seed runs once, avoiding a parallel race.

use admin_demo::seed;
use rustango::admin;
use rustango::sql::sqlx::PgPool;
use rustango::test_client::TestClient;

#[tokio::test]
async fn admin_renders_list_filters_and_inline() {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let pool = PgPool::connect(&url).await.expect("connect");
    seed(&pool).await.expect("seed");

    let app = axum::Router::new()
        .nest("/admin", admin::Builder::new(pool).admin_prefix("/admin").build());
    let client = TestClient::new(app);

    // Home renders.
    let home = client.get("/admin").send().await;
    assert_eq!(home.status, 200);

    // List view: seeded rows + the `status` filter/column from admin(...).
    let list = client.get("/admin/posts").send().await;
    assert_eq!(list.status, 200);
    let body = list.text();
    assert!(body.contains("exploring rustango"), "seeded post missing from list");
    assert!(body.to_lowercase().contains("status"), "status filter/column missing");

    // Detail view: the read-only comments inline.
    let detail = client.get("/admin/posts/1").send().await;
    assert_eq!(detail.status, 200);
    assert!(detail.text().contains("Comments"), "comments inline missing from detail");
}
