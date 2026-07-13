//! Cookbook Chapter 8b — real-binary admin form + ViewSet API.
//!
//! Spins up the actual `cookbook_blog` binary against an isolated DB,
//! provisions two tenants + a tenant user, drives the admin form
//! through HTTP (login → POST /admin/<table> → verify list), then
//! exercises the tenant-aware /api/authors ViewSet from both tenants
//! to confirm data isolation.
//!
//! Slow (~30s) — boots a fresh binary + applies migrations. Skips
//! silently if `DATABASE_URL` is unset OR if the bin can't be located.
//!
//! Run: `DATABASE_URL=... cargo test --test cookbook_chapter08b_browser_forms -- --test-threads=1 --nocapture`

use rustango::sql::sqlx;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIND: &str = "127.0.0.1:8867"; // not 8765 (chapter 8 playwright session port)
const APEX: &str = "localhost";
const SESSION_SECRET: &str = "cookbook-chapter8b-test-32bytes-please!!!!";
const DB_NAME: &str = "cookbook_ch8b_dev";

fn url() -> Option<String> { std::env::var("DATABASE_URL").ok() }

async fn db_url() -> Option<String> {
    let base = url()?;
    // Replace the trailing /<db> with our isolated test DB.
    let trimmed = base.rsplit_once('/').map(|(prefix, _)| prefix.to_owned())?;
    Some(format!("{trimmed}/{DB_NAME}"))
}

async fn reset_db() {
    let Some(base) = url() else { return };
    let admin_pool = sqlx::PgPool::connect(&base).await.expect("connect to admin db");
    // Terminate any lingering connections to the test DB before DROP,
    // otherwise Postgres rejects with 55006 ("being accessed by other
    // users"). Matches chapter06b's reset_db.
    let _ = sqlx::query(&format!(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE datname = '{DB_NAME}' AND pid <> pg_backend_pid()"
    ))
    .execute(&admin_pool)
    .await;
    sqlx::query(&format!("DROP DATABASE IF EXISTS {DB_NAME}"))
        .execute(&admin_pool).await.unwrap();
    sqlx::query(&format!("CREATE DATABASE {DB_NAME}"))
        .execute(&admin_pool).await.unwrap();
}

fn manage(verb: &str, args: &[&str], db: &str) -> std::process::Output {
    let bin = env!("CARGO_BIN_EXE_cookbook_blog");
    Command::new(bin)
        .arg(verb)
        .args(args)
        .env("DATABASE_URL", db)
        .env("RUSTANGO_APEX_DOMAIN", APEX)
        .env("RUSTANGO_BIND", BIND)
        .env("RUSTANGO_SESSION_SECRET", SESSION_SECRET)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("manage spawn")
}

fn spawn_server(db: &str) -> Child {
    let bin = env!("CARGO_BIN_EXE_cookbook_blog");
    Command::new(bin)
        .env("DATABASE_URL", db)
        .env("RUSTANGO_APEX_DOMAIN", APEX)
        .env("RUSTANGO_BIND", BIND)
        .env("RUSTANGO_SESSION_SECRET", SESSION_SECRET)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("server spawn")
}

async fn wait_ready() {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if reqwest::Client::new()
            .get(format!("http://{BIND}/login"))
            .send()
            .await
            .is_ok()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("server didn't become ready within 20s");
}

#[tokio::test]
async fn admin_form_creates_then_viewset_isolates_per_tenant() {
    let Some(db) = db_url().await else { return };
    reset_db().await;

    // Bootstrap registry + tenants.
    let m = manage("migrate", &[], &db);
    assert!(m.status.success(), "manage migrate: {}", String::from_utf8_lossy(&m.stderr));
    let m = manage("create-operator", &["admin", "--password", "letmein"], &db);
    assert!(m.status.success(), "create-operator: {}", String::from_utf8_lossy(&m.stderr));
    for slug in ["acme", "globex"] {
        let m = manage(
            "create-tenant",
            &[slug, "--display-name", slug, "--host-pattern", &format!("{slug}.{APEX}")],
            &db,
        );
        assert!(m.status.success(), "create-tenant {slug}: {}", String::from_utf8_lossy(&m.stderr));
    }
    let m = manage(
        "create-user",
        &["acme", "alice", "--password", "tenantpw", "--superuser"],
        &db,
    );
    assert!(m.status.success(), "create-user: {}", String::from_utf8_lossy(&m.stderr));

    // Boot the server.
    let mut server = spawn_server(&db);
    wait_ready().await;

    let client = reqwest::Client::builder()
        .cookie_store(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    // 1. Login as alice on acme tenant.
    let login_body = [("username", "alice"), ("password", "tenantpw")];
    let resp = client
        .post(format!("http://{BIND}/login"))
        .header("Host", "acme.localhost")
        .form(&login_body)
        .send().await.unwrap();
    assert!(
        resp.status().is_redirection() || resp.status().is_success(),
        "login status: {}", resp.status()
    );

    // 2. POST the admin create form for cookbook_author.
    let create_body = [
        ("name",  "ada lovelace"),
        ("email", "ada@example.com"),
        ("bio",   "first programmer"),
    ];
    let resp = client
        .post(format!("http://{BIND}/admin/cookbook_author"))
        .header("Host", "acme.localhost")
        .form(&create_body)
        .send().await.unwrap();
    assert!(
        resp.status().is_redirection() || resp.status().is_success(),
        "create POST status: {} body: {}",
        resp.status(),
        resp.text().await.unwrap_or_default()
    );

    // 3. Verify via the tenant-aware ViewSet at /api/authors.
    let acme_list: serde_json::Value = client
        .get(format!("http://{BIND}/api/authors"))
        .header("Host", "acme.localhost")
        .send().await.unwrap()
        .json().await.unwrap();
    let arr = acme_list.as_array().expect("array");
    assert_eq!(arr.len(), 1, "acme should have exactly the row we just created via the admin");
    assert_eq!(arr[0]["name"], "ada lovelace");
    assert_eq!(arr[0]["email"], "ada@example.com");

    // 4. globex sees nothing — tenant isolation.
    let globex_list: serde_json::Value = client
        .get(format!("http://{BIND}/api/authors"))
        .header("Host", "globex.localhost")
        .send().await.unwrap()
        .json().await.unwrap();
    assert_eq!(globex_list.as_array().unwrap().len(), 0,
        "globex should not see acme's row");

    // 5. Apex (no tenant subdomain) refuses tenant-scoped routes.
    let apex_resp = client
        .get(format!("http://{BIND}/api/authors"))
        .header("Host", "localhost")
        .send().await.unwrap();
    assert_eq!(apex_resp.status().as_u16(), 404,
        "apex must not serve tenant-scoped routes");

    let _ = server.kill();
    let _ = server.wait();
}
