//! Verifies the auto-admin renders against a real Postgres: it is behind a
//! login, the home lists the models, the posts list shows seeded rows + the
//! configured filters, and the post detail page shows the comments inline.
//! Needs `DATABASE_URL` (the schema is created by `cargo run -- migrate`; CI
//! does that before `cargo test`).
//!
//! One test on purpose — the shared seed runs once, avoiding a parallel race.

use admin_demo::{admin_router, seed};
use rustango::admin::AdminUser;
use rustango::sql::sqlx::PgPool;
use rustango::test_client::TestClient;

#[tokio::test]
async fn admin_renders_list_filters_and_inline() {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let pool = PgPool::connect(&url).await.expect("connect");
    seed(&pool).await.expect("seed");

    // The router the server mounts, so this pins its login.
    let app = axum::Router::new().nest("/admin", admin_router(pool.clone()));
    let client = TestClient::new(app);

    // Anonymous: sent to the login page.
    let anon = client.get("/admin").send().await;
    assert!((300..400).contains(&anon.status), "anonymous got {}", anon.status);
    assert!(anon.header("location").unwrap_or("").contains("/admin/login"));

    let name = format!(
        "smoke{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let mut user = AdminUser::new_with_password(&name, "smoke-pass-123", true).unwrap();
    user.insert_pool(&pool.clone().into()).await.expect("admin user");
    let login = client
        .login(
            "/admin/login",
            &[("username", &name), ("password", "smoke-pass-123")],
        )
        .await;
    assert!((300..400).contains(&login.status), "login: {}", login.status);

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
