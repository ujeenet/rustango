#![cfg(feature = "postgres")]
//! Live tests for the v0.28.2 change-password / change-operator-password
//! CLI verbs. Exercises the current-password verification path end-to-end:
//! create user → change with correct current → re-verify hash; create
//! user → change with wrong current → assert rejection.
//!
//! Reads `DATABASE_URL`. Skips silently when unset.

#![cfg(feature = "tenancy")]

use rustango::sql::sqlx;
use rustango::tenancy::manage::run_with_writer;
use rustango::tenancy::{TenantPools, TenantPoolsConfig};

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    sqlx::PgPool::connect(&url).await.ok()
}

async fn fresh(pool: &sqlx::PgPool) {
    rustango::migrate::drop_all(pool).await.unwrap();
    rustango::migrate::apply_all(pool).await.unwrap();
}

fn args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_owned()).collect()
}

#[tokio::test]
async fn change_operator_password_round_trip() {
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;
    let url = std::env::var("DATABASE_URL").unwrap();
    let pools = TenantPools::new(pool.clone()).config(TenantPoolsConfig::default());
    let dir = std::env::temp_dir();
    let mut buf = Vec::new();

    // Create operator with --password
    run_with_writer(
        &pools,
        &url,
        &dir,
        args(&["create-operator", "alice", "--password", "old-secret-123"]),
        &mut buf,
    )
    .await
    .unwrap();

    // Change with correct current
    let mut buf2 = Vec::new();
    run_with_writer(
        &pools,
        &url,
        &dir,
        args(&[
            "change-operator-password",
            "alice",
            "--current",
            "old-secret-123",
            "--password",
            "new-secret-456",
        ]),
        &mut buf2,
    )
    .await
    .unwrap();
    assert!(
        String::from_utf8_lossy(&buf2).contains("password changed for operator `alice`"),
        "missing success line: {}",
        String::from_utf8_lossy(&buf2)
    );

    // The new password should hash-verify against the stored row
    let stored: Option<String> =
        sqlx::query_scalar("SELECT password_hash FROM rustango_operators WHERE username = 'alice'")
            .fetch_optional(&pool)
            .await
            .unwrap();
    let stored_hash = stored.expect("operator row");
    assert!(
        rustango::tenancy::password::verify("new-secret-456", &stored_hash).unwrap(),
        "new password failed to verify against updated hash"
    );
    assert!(
        !rustango::tenancy::password::verify("old-secret-123", &stored_hash).unwrap(),
        "old password still verifies — UPDATE didn't take"
    );

    // Wrong current → rejected
    let mut buf3 = Vec::new();
    let err = run_with_writer(
        &pools,
        &url,
        &dir,
        args(&[
            "change-operator-password",
            "alice",
            "--current",
            "wrong-current",
            "--password",
            "another-secret",
        ]),
        &mut buf3,
    )
    .await
    .unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("current password did not match"),
        "expected mismatch error, got: {msg}"
    );
}

#[tokio::test]
async fn create_operator_with_generate_emits_random_password() {
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;
    let url = std::env::var("DATABASE_URL").unwrap();
    let pools = TenantPools::new(pool.clone()).config(TenantPoolsConfig::default());
    let dir = std::env::temp_dir();
    let mut buf = Vec::new();

    run_with_writer(
        &pools,
        &url,
        &dir,
        args(&["create-operator", "bob", "--generate"]),
        &mut buf,
    )
    .await
    .unwrap();

    let out = String::from_utf8_lossy(&buf).to_string();
    assert!(
        out.contains("generated password:"),
        "expected `generated password:` line, got: {out}"
    );
    // Extract the printed password and verify it round-trips through
    // the stored hash.
    let line = out
        .lines()
        .find(|l| l.contains("generated password:"))
        .unwrap();
    let generated = line.split("generated password:").nth(1).unwrap().trim();
    assert!(
        generated.len() >= 16,
        "generated password too short: {generated}"
    );

    let stored: Option<String> =
        sqlx::query_scalar("SELECT password_hash FROM rustango_operators WHERE username = 'bob'")
            .fetch_optional(&pool)
            .await
            .unwrap();
    let stored_hash = stored.expect("operator row");
    assert!(
        rustango::tenancy::password::verify(generated, &stored_hash).unwrap(),
        "printed password didn't verify against stored hash"
    );
}

#[tokio::test]
async fn generate_and_password_are_mutually_exclusive() {
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;
    let url = std::env::var("DATABASE_URL").unwrap();
    let pools = TenantPools::new(pool).config(TenantPoolsConfig::default());
    let dir = std::env::temp_dir();
    let mut buf = Vec::new();

    let err = run_with_writer(
        &pools,
        &url,
        &dir,
        args(&["create-operator", "carol", "--password", "x", "--generate"]),
        &mut buf,
    )
    .await
    .unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("mutually exclusive"),
        "expected mutually-exclusive error, got: {msg}"
    );
}
