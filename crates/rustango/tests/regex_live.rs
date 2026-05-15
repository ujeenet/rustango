#![cfg(feature = "postgres")]
//! Live PG tests for `__regex` / `__iregex` lookups (issue #26).
//! Verifies the typed `.regex()` / `.iregex()` / negated variants on
//! `Column` plus the Django-shape `.filter("name__regex", pattern)`
//! parser route at runtime against PG's native POSIX operators
//! (`~`, `~*`, `!~`, `!~*`). Skips silently when `DATABASE_URL` is unset.

use std::sync::OnceLock;

use rustango::core::Column as _;
use rustango::sql::{sqlx, Auto, FetcherPool, Pool};
use rustango::Model;
use tokio::sync::Mutex;

#[derive(Model, Debug, Clone)]
#[rustango(table = "rx_user_live")]
#[allow(dead_code)]
pub struct User {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub name: String,
}

fn lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn fresh_pool() -> Option<Pool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pg = sqlx::PgPool::connect(&url).await.ok()?;
    sqlx::query(r#"DROP TABLE IF EXISTS "rx_user_live" CASCADE"#)
        .execute(&pg)
        .await
        .unwrap();
    sqlx::query(
        r#"
        CREATE TABLE "rx_user_live" (
            id BIGSERIAL PRIMARY KEY,
            name VARCHAR(64) NOT NULL
        )
        "#,
    )
    .execute(&pg)
    .await
    .unwrap();
    let pool = Pool::Postgres(pg);
    for name in ["alice", "Alice-2", "bob", "Bob-3", "admin", "ADMIN-root"] {
        let mut u = User {
            id: Auto::default(),
            name: name.into(),
        };
        u.insert_pool(&pool).await.unwrap();
    }
    Some(pool)
}

fn names(rows: &[User]) -> Vec<String> {
    let mut v: Vec<String> = rows.iter().map(|u| u.name.clone()).collect();
    v.sort();
    v
}

/// Case-sensitive `__regex` — PG emits `name ~ $1`. Pattern `^al.*`
/// matches `alice` but not `Alice-2` (capital A).
#[tokio::test]
async fn regex_case_sensitive_matches_lowercase_only() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let rows: Vec<User> = User::objects()
        .where_(User::name.regex("^al.*"))
        .fetch_pool(&pool)
        .await
        .unwrap();

    assert_eq!(names(&rows), vec!["alice".to_string()]);
}

/// Case-insensitive `__iregex` — PG emits `name ~* $1`. Same pattern
/// now picks up `alice` AND `Alice-2`.
#[tokio::test]
async fn iregex_case_insensitive_matches_both_cases() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let rows: Vec<User> = User::objects()
        .where_(User::name.iregex("^al.*"))
        .fetch_pool(&pool)
        .await
        .unwrap();

    assert_eq!(
        names(&rows),
        vec!["Alice-2".to_string(), "alice".to_string()]
    );
}

/// Negated `__regex` — PG `!~`. `^admin` excludes only lowercase
/// "admin" (5 rows survive: alice, Alice-2, bob, Bob-3, ADMIN-root).
#[tokio::test]
async fn not_regex_excludes_case_sensitive_matches() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let rows: Vec<User> = User::objects()
        .where_(User::name.not_regex("^admin"))
        .fetch_pool(&pool)
        .await
        .unwrap();

    assert_eq!(
        names(&rows),
        vec![
            "ADMIN-root".to_string(),
            "Alice-2".to_string(),
            "Bob-3".to_string(),
            "alice".to_string(),
            "bob".to_string(),
        ]
    );
}

/// Negated `__iregex` — PG `!~*`. `^admin` now excludes BOTH
/// "admin" and "ADMIN-root" (4 rows survive).
#[tokio::test]
async fn not_iregex_excludes_both_cases() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let rows: Vec<User> = User::objects()
        .where_(User::name.not_iregex("^admin"))
        .fetch_pool(&pool)
        .await
        .unwrap();

    assert_eq!(
        names(&rows),
        vec![
            "Alice-2".to_string(),
            "Bob-3".to_string(),
            "alice".to_string(),
            "bob".to_string(),
        ]
    );
}

/// Django-shape `.filter("name__iregex", "...")` — string-keyed
/// parser routes to Op::IRegex and runs against PG `~*` at runtime.
#[tokio::test]
async fn filter_string_iregex_routes_at_runtime() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let rows: Vec<User> = User::objects()
        .filter("name__iregex", "^bob")
        .fetch_pool(&pool)
        .await
        .unwrap();

    assert_eq!(names(&rows), vec!["Bob-3".to_string(), "bob".to_string()]);
}
