//! Tri-dialect coverage for `DatabaseCache::delete_prefix` (#1227).
//!
//! The SQLite arm lives in `cache_db_backend_sqlite_live.rs`. This file
//! covers Postgres and MySQL, because the statement is
//! `DELETE … WHERE cache_key LIKE ? ESCAPE '!'` and `LIKE … ESCAPE` is
//! exactly the kind of construct the three backends disagree about.
//!
//! The escape character is `!` rather than the conventional `\` because
//! the `\` form is *not* portable: MySQL treats a backslash as an escape
//! inside string literals, so `ESCAPE '\'` is an unterminated literal
//! and a 1064 syntax error there, while Postgres (with
//! `standard_conforming_strings`) reads it as one backslash and accepts
//! it happily. That divergence is the reason this file exists —
//! emitting one statement for all three dialects is only safe if it is
//! exercised on all three.
//!
//! Reads `DATABASE_URL` (Postgres) and `MYSQL_TEST_URL` (MySQL). Each
//! test skips silently when its variable is unset, so the suite stays
//! green offline.

#![cfg(any(feature = "postgres", feature = "mysql"))]

use rustango::cache::{Cache, DatabaseCache};
use rustango::sql::Pool;

/// Populate a fresh table, prefix-delete one namespace, and assert only
/// that namespace went — including the wildcard-escaping cases, which
/// are the ones that would silently over-delete.
async fn assert_prefix_semantics(pool: Pool, table: &str) {
    let cache = DatabaseCache::new(pool, table);
    // Start from a known-empty table even if a previous run left rows.
    cache.ensure_table().await.expect("ensure_table");
    cache.clear().await.expect("clear");

    cache.set("tenant:acme:a", "1", None).await.unwrap();
    cache.set("tenant:acme:b", "2", None).await.unwrap();
    cache.set("tenant:acme-corp:a", "long", None).await.unwrap();
    cache.set("tenant:globex:a", "9", None).await.unwrap();
    cache.set("unrelated", "keep", None).await.unwrap();
    cache.set("a_b:key", "underscore", None).await.unwrap();
    cache.set("axb:key", "would-match", None).await.unwrap();
    cache.set("100%:key", "percent", None).await.unwrap();
    cache.set("100zz:key", "would-match", None).await.unwrap();

    cache
        .delete_prefix("tenant:acme:")
        .await
        .expect("prefix delete");

    assert_eq!(cache.get("tenant:acme:a").await.unwrap(), None);
    assert_eq!(cache.get("tenant:acme:b").await.unwrap(), None);
    assert_eq!(
        cache.get("tenant:acme-corp:a").await.unwrap().as_deref(),
        Some("long"),
        "the trailing separator must stop `acme` matching `acme-corp`"
    );
    assert_eq!(
        cache.get("tenant:globex:a").await.unwrap().as_deref(),
        Some("9"),
        "another tenant's entry must survive"
    );
    assert_eq!(
        cache.get("unrelated").await.unwrap().as_deref(),
        Some("keep")
    );

    // `_` is a single-character LIKE wildcard; unescaped, `a_b:` would
    // also sweep `axb:key`.
    cache.delete_prefix("a_b:").await.unwrap();
    assert_eq!(cache.get("a_b:key").await.unwrap(), None);
    assert_eq!(
        cache.get("axb:key").await.unwrap().as_deref(),
        Some("would-match"),
        "`_` must be escaped rather than matching any character"
    );

    // `%` is a multi-character wildcard; unescaped, `100%:` would sweep
    // everything starting with `100`.
    cache.delete_prefix("100%:").await.unwrap();
    assert_eq!(cache.get("100%:key").await.unwrap(), None);
    assert_eq!(
        cache.get("100zz:key").await.unwrap().as_deref(),
        Some("would-match"),
        "`%` must be escaped rather than matching any run of characters"
    );

    // `!` is the escape character itself, so a prefix containing one
    // must have it doubled — otherwise `!%` inside a prefix would be
    // read as an escaped wildcard and the delete would miss.
    cache.set("wow!:key", "bang", None).await.unwrap();
    cache.set("wow!x:key", "keep", None).await.unwrap();
    cache.delete_prefix("wow!:").await.unwrap();
    assert_eq!(
        cache.get("wow!:key").await.unwrap(),
        None,
        "a literal `!` in the prefix must still match"
    );
    assert_eq!(
        cache.get("wow!x:key").await.unwrap().as_deref(),
        Some("keep")
    );

    cache.clear().await.unwrap();
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn delete_prefix_semantics_on_postgres() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let pg = rustango::sql::sqlx::PgPool::connect(&url)
        .await
        .expect("connect DATABASE_URL");
    assert_prefix_semantics(Pool::Postgres(pg), "rustango_cache_prefix_pg").await;
}

#[cfg(feature = "mysql")]
#[tokio::test]
async fn delete_prefix_semantics_on_mysql() {
    let Ok(url) = std::env::var("MYSQL_TEST_URL") else {
        return;
    };
    let my = rustango::sql::sqlx::MySqlPool::connect(&url)
        .await
        .expect("connect MYSQL_TEST_URL");
    assert_prefix_semantics(Pool::Mysql(my), "rustango_cache_prefix_my").await;
}
