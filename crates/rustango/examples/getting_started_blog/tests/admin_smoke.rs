//! Verifies the auto-admin from Step 11: it is behind a login, and once
//! signed in the home and the `posts` page load at the `/admin` mount (no
//! trailing slash) the `admin_prefix("/admin")` setting produces.
//!
//! Requires the schema to exist: run `cargo run -- migrate` first
//! (CI does this before `cargo test`).

use getting_started_blog::urls;
use rustango::admin::AdminUser;
use rustango::sql::Pool;
use rustango::test_client::TestClient;

async fn pool() -> Pool {
    let _ = dotenvy::dotenv();
    Pool::connect(&std::env::var("DATABASE_URL").unwrap())
        .await
        .unwrap()
}

fn admin_app(pool: Pool) -> axum::Router {
    urls::api().nest("/admin", urls::admin_router(pool, &rustango::config::Settings::default()))
}

/// A client signed in as a fresh superuser.
async fn signed_in() -> TestClient {
    let pool = pool().await;
    let name = format!(
        "smoke{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let mut user = AdminUser::new_with_password(&name, "smoke-pass-123", true).unwrap();
    user.insert_pool(&pool).await.unwrap();
    let client = TestClient::new(admin_app(pool));
    let r = client
        .login(
            "/admin/login",
            &[("username", &name), ("password", "smoke-pass-123")],
        )
        .await;
    assert!((300..400).contains(&r.status), "login: {} {}", r.status, r.text());
    client
}

#[tokio::test]
async fn admin_needs_a_login() {
    let client = TestClient::new(admin_app(pool().await));
    let r = client.get("/admin").send().await;
    assert!((300..400).contains(&r.status), "anonymous got {}", r.status);
    assert!(r.header("location").unwrap_or("").contains("/admin/login"));
    assert_ne!(client.get("/admin/posts").send().await.status, 200);
}

#[tokio::test]
async fn admin_home_loads() {
    let client = signed_in().await;
    let r = client.get("/admin").send().await;
    assert_eq!(r.status, 200);
    // Links use the configured `/admin` prefix (hrefs are HTML-escaped),
    // not the default `/__admin` — confirms admin_prefix matches the mount.
    assert!(!r.text().contains("__admin"));
}

#[tokio::test]
async fn admin_posts_list_loads() {
    let client = signed_in().await;
    let r = client.get("/admin/posts").send().await;
    assert_eq!(r.status, 200);
}
