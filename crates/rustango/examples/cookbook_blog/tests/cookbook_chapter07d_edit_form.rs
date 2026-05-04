//! Cookbook Chapter 7d — open + edit existing record via the
//! non-admin form path.
//!
//! Same shape as Chapter 7c's create test: spawns the binary,
//! provisions an acme tenant, then drives the edit lifecycle:
//!
//!   1. Seed an Author via POST /authors/new.
//!   2. GET  /authors/{id}/edit  — form pre-filled with the saved row.
//!   3. POST /authors/{id}/edit  — UPDATE lands; redirect to the row's
//!      retrieve endpoint; GET shows the updated values.
//!   4. POST a 2nd row, then try to UPDATE row #1's email to row #2's
//!      email — UNIQUE rejection re-renders the form with the error.
//!
//! Run: `DATABASE_URL=... cargo test --test cookbook_chapter07d_edit_form -- --test-threads=1`

use rustango::sql::sqlx;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIND: &str = "127.0.0.1:8869";
const APEX: &str = "localhost";
const SESSION_SECRET: &str = "cookbook-chapter7d-test-32bytes-please!!!!";
const DB_NAME: &str = "cookbook_ch7d_dev";

fn url() -> Option<String> { std::env::var("DATABASE_URL").ok() }

async fn db_url() -> Option<String> {
    let base = url()?;
    let trimmed = base.rsplit_once('/').map(|(prefix, _)| prefix.to_owned())?;
    Some(format!("{trimmed}/{DB_NAME}"))
}

async fn reset_db() {
    let Some(base) = url() else { return };
    let admin = sqlx::PgPool::connect(&base).await.expect("connect");
    sqlx::query(&format!("DROP DATABASE IF EXISTS {DB_NAME}")).execute(&admin).await.unwrap();
    sqlx::query(&format!("CREATE DATABASE {DB_NAME}")).execute(&admin).await.unwrap();
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
    panic!("server didn't bind within 20s");
}

#[tokio::test]
async fn open_then_edit_existing_record_with_unique_rejection() {
    let Some(db) = db_url().await else { return };
    reset_db().await;

    assert!(manage("migrate", &[], &db).status.success());
    assert!(manage("create-tenant",
        &["acme", "--display-name", "acme", "--host-pattern", &format!("acme.{APEX}")], &db,
    ).status.success());

    let mut server = spawn_server(&db);
    wait_ready().await;

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build().unwrap();
    let host = "acme.localhost";

    // §7d.1 — seed two rows via the create form.
    for (name, email) in [("ada", "ada@example.com"), ("grace", "grace@example.com")] {
        let resp = client
            .post(format!("http://{BIND}/authors/new")).header("Host", host)
            .form(&[("name", name), ("email", email), ("bio", "")])
            .send().await.unwrap();
        assert_eq!(resp.status().as_u16(), 303, "create {name}: {}", resp.status());
    }

    // §7d.2 — GET edit form for id=1 pre-fills with the saved row.
    let html = client
        .get(format!("http://{BIND}/authors/1/edit")).header("Host", host)
        .send().await.unwrap().text().await.unwrap();
    assert!(html.contains("Edit Author #1"), "edit page title; html head:\n{}", &html[..html.len().min(400)]);
    // Pre-fill: input value attributes carry the existing row's values.
    assert!(html.contains(r#"value="ada""#), "name pre-filled");
    assert!(html.contains(r#"value="ada@example.com""#), "email pre-filled");

    // §7d.2b — non-existent record returns 404.
    let resp = client
        .get(format!("http://{BIND}/authors/999/edit")).header("Host", host)
        .send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 404);

    // §7d.3 — POST the edit form with new values → redirect to retrieve.
    let resp = client
        .post(format!("http://{BIND}/authors/1/edit")).header("Host", host)
        .form(&[("name", "ada lovelace"), ("email", "ada.l@example.com"), ("bio", "first programmer")])
        .send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 303, "edit POST status: {}", resp.status());
    let location = resp.headers().get("location").and_then(|v| v.to_str().ok()).unwrap_or("");
    assert_eq!(location, "/api/authors/1");

    // GET the row back — UPDATE landed.
    let body: serde_json::Value = client
        .get(format!("http://{BIND}/api/authors/1")).header("Host", host)
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(body["name"], "ada lovelace");
    assert_eq!(body["email"], "ada.l@example.com");
    assert_eq!(body["bio"], "first programmer");

    // §7d.4 — UNIQUE constraint rejects updating row #1's email to row
    // #2's existing email; form re-renders with the DB error preserved.
    let resp = client
        .post(format!("http://{BIND}/authors/1/edit")).header("Host", host)
        .form(&[("name", "ada"), ("email", "grace@example.com"), ("bio", "")])
        .send().await.unwrap();
    assert!(resp.status().is_success(),
        "duplicate UPDATE should re-render form (200), not 5xx; got {}", resp.status());
    let body = resp.text().await.unwrap();
    assert!(body.to_lowercase().contains("unique") || body.to_lowercase().contains("duplicate"),
        "expected UNIQUE error banner; body head:\n{}", &body[..body.len().min(400)]);

    // §7d.5 — original row #1 still has its (different) email.
    let body: serde_json::Value = client
        .get(format!("http://{BIND}/api/authors/1")).header("Host", host)
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(body["email"], "ada.l@example.com",
        "rejected UPDATE must NOT have leaked through");

    let _ = server.kill();
    let _ = server.wait();
}
