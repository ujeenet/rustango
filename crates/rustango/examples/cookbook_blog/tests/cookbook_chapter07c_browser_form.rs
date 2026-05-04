//! Cookbook Chapter 7c — non-admin user-facing form via playwright-grade HTTP.
//!
//! `/authors/new` is a tenant-aware, non-admin form route in
//! `apps/blog/urls.rs` that hand-rolls HTML, parses the POST body
//! with `ModelFormFor<Author>`, and saves via the per-tenant
//! connection. This test boots the actual binary, drives the form,
//! and asserts the server-side validations a real browser session
//! would surface:
//!
//! 1. GET renders an HTML form.
//! 2. POST with empty body → re-rendered form + "required field"
//!    errors (covers the empty-string-non-null-String fix shipped
//!    in this slice).
//! 3. POST with valid fields → 303 redirect to /api/authors and the
//!    new row appears in the listing.
//! 4. POST with a duplicate email → re-rendered form + a UNIQUE
//!    violation message (covers the create_table UNIQUE DDL fix
//!    shipped in this slice).
//!
//! Run: `DATABASE_URL=... cargo test --test cookbook_chapter07c_browser_form -- --test-threads=1`

use rustango::sql::sqlx;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIND: &str = "127.0.0.1:8868"; // distinct from chapter 8b
const APEX: &str = "localhost";
const SESSION_SECRET: &str = "cookbook-chapter7c-test-32bytes-please!!!!";
const DB_NAME: &str = "cookbook_ch7c_dev";

fn url() -> Option<String> { std::env::var("DATABASE_URL").ok() }

async fn db_url() -> Option<String> {
    let base = url()?;
    let trimmed = base.rsplit_once('/').map(|(prefix, _)| prefix.to_owned())?;
    Some(format!("{trimmed}/{DB_NAME}"))
}

async fn reset_db() {
    let Some(base) = url() else { return };
    let admin = sqlx::PgPool::connect(&base).await.expect("connect");
    sqlx::query(&format!("DROP DATABASE IF EXISTS {DB_NAME}"))
        .execute(&admin).await.unwrap();
    sqlx::query(&format!("CREATE DATABASE {DB_NAME}"))
        .execute(&admin).await.unwrap();
}

fn manage(verb: &str, args: &[&str], db: &str) -> std::process::Output {
    let bin = env!("CARGO_BIN_EXE_cookbook_blog");
    Command::new(bin).arg(verb).args(args)
        .env("DATABASE_URL", db).env("RUSTANGO_APEX_DOMAIN", APEX)
        .env("RUSTANGO_BIND", BIND).env("RUSTANGO_SESSION_SECRET", SESSION_SECRET)
        .stdout(Stdio::piped()).stderr(Stdio::piped())
        .output().expect("manage spawn")
}

fn spawn_server(db: &str) -> Child {
    let bin = env!("CARGO_BIN_EXE_cookbook_blog");
    Command::new(bin)
        .env("DATABASE_URL", db).env("RUSTANGO_APEX_DOMAIN", APEX)
        .env("RUSTANGO_BIND", BIND).env("RUSTANGO_SESSION_SECRET", SESSION_SECRET)
        .stdout(Stdio::null()).stderr(Stdio::null())
        .spawn().expect("server spawn")
}

async fn wait_ready() {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if reqwest::Client::new()
            .get(format!("http://{BIND}/login"))
            .send().await.is_ok() { return; }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("server did not bind within 20s");
}

#[tokio::test]
async fn non_admin_form_renders_validates_inserts_and_unique_rejects() {
    let Some(db) = db_url().await else { return };
    reset_db().await;

    assert!(manage("migrate", &[], &db).status.success());
    assert!(manage(
        "create-tenant",
        &["acme", "--display-name", "acme", "--host-pattern", &format!("acme.{APEX}")],
        &db,
    ).status.success());

    let mut server = spawn_server(&db);
    wait_ready().await;

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build().unwrap();

    // §7c.1 — GET /authors/new returns the HTML form.
    let html = client
        .get(format!("http://{BIND}/authors/new"))
        .header("Host", "acme.localhost")
        .send().await.unwrap()
        .text().await.unwrap();
    assert!(html.contains("New Author") && html.contains("name=\"name\"") && html.contains("name=\"email\""),
        "form HTML missing expected fields:\n{html}");

    // §7c.2 — POST with empty fields returns the form with server-side errors.
    let resp = client
        .post(format!("http://{BIND}/authors/new"))
        .header("Host", "acme.localhost")
        .form(&[("name", ""), ("email", ""), ("bio", "")])
        .send().await.unwrap();
    assert!(resp.status().is_success(), "validation re-render should be 200; got {}", resp.status());
    let body = resp.text().await.unwrap();
    assert!(body.contains("`name`") && body.contains("`email`"),
        "empty-field error banner missing both `name` + `email`. body head:\n{}",
        &body[..body.len().min(500)]);

    // §7c.3 — POST with valid fields → 303 redirect to /api/authors.
    let resp = client
        .post(format!("http://{BIND}/authors/new"))
        .header("Host", "acme.localhost")
        .form(&[("name", "ada"), ("email", "ada@example.com"), ("bio", "first")])
        .send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 303, "expected redirect; got {}", resp.status());
    let location = resp.headers().get("location").and_then(|v| v.to_str().ok()).unwrap_or("");
    assert_eq!(location, "/api/authors");

    let listed: serde_json::Value = client
        .get(format!("http://{BIND}/api/authors"))
        .header("Host", "acme.localhost")
        .send().await.unwrap()
        .json().await.unwrap();
    let arr = listed.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["email"], "ada@example.com");

    // §7c.4 — UNIQUE rejects duplicate email; form re-renders with DB error.
    let resp = client
        .post(format!("http://{BIND}/authors/new"))
        .header("Host", "acme.localhost")
        .form(&[("name", "ada-dup"), ("email", "ada@example.com"), ("bio", "duplicate")])
        .send().await.unwrap();
    assert!(resp.status().is_success(),
        "duplicate insert should re-render the form (200), not 500/redirect; got {}",
        resp.status());
    let body = resp.text().await.unwrap();
    assert!(body.to_lowercase().contains("unique") || body.to_lowercase().contains("duplicate"),
        "expected UNIQUE / duplicate-key error banner; body head:\n{}",
        &body[..body.len().min(400)]);

    // Confirm only one row landed.
    let listed: serde_json::Value = client
        .get(format!("http://{BIND}/api/authors"))
        .header("Host", "acme.localhost")
        .send().await.unwrap()
        .json().await.unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 1, "duplicate must NOT have inserted");

    let _ = server.kill();
    let _ = server.wait();
}
