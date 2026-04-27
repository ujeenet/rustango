//! Live test of `rustango::migrate::apply_all` against a real Postgres.
//!
//! Reads `DATABASE_URL`. If unset, every test returns silently — same
//! convention as other live tests. The tests in this file run in a
//! separate binary, so the inventory registry contains *only* the
//! models defined here. That makes `apply_all` deterministic.

use rustango::core::Column as _;
use rustango::migrate;
use rustango::sql::{sqlx, Fetcher};
use rustango::Model;
use tokio::sync::Mutex;

#[derive(Model, Debug, PartialEq, Eq, Clone)]
#[rustango(table = "mig_user")]
pub struct MigUser {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 32)]
    name: String,
    #[rustango(min = 0, max = 150)]
    age: i32,
    is_active: bool,
}

#[derive(Model, Debug, PartialEq, Eq, Clone)]
#[rustango(table = "mig_post")]
pub struct MigPost {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 200)]
    title: String,
    #[rustango(fk = "mig_user", on = "id")]
    author_id: i64,
}

fn live_lock() -> &'static Mutex<()> {
    static M: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    Some(
        sqlx::PgPool::connect(&url)
            .await
            .expect("connect to DATABASE_URL"),
    )
}

#[tokio::test]
async fn apply_all_creates_every_registered_table() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };

    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();

    // Tables exist by virtue of insert/fetch round-tripping.
    MigUser {
        id: 1,
        name: "alice".into(),
        age: 30,
        is_active: true,
    }
    .insert(&pool)
    .await
    .unwrap();

    MigPost {
        id: 1,
        title: "hello".into(),
        author_id: 1,
    }
    .insert(&pool)
    .await
    .unwrap();

    let users: Vec<MigUser> = MigUser::objects().fetch(&pool).await.unwrap();
    assert_eq!(users.len(), 1);
    assert_eq!(users[0].name, "alice");

    let posts: Vec<MigPost> = MigPost::objects().fetch(&pool).await.unwrap();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].title, "hello");

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn fk_constraint_is_enforced_by_db() {
    // Bounded.author_id has fk = "mig_user". An insert with a non-existent
    // author should fail at the DB level (rustango doesn't pre-check FKs).
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };

    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();

    let bad = MigPost {
        id: 99,
        title: "orphan".into(),
        author_id: 999, // no such user
    };
    let err = bad.insert(&pool).await.unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("foreign key") || msg.contains("violates"),
        "expected FK violation, got: {msg}",
    );

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn check_constraint_is_enforced_by_db() {
    // age has min = 0, max = 150 — translated to a CHECK constraint.
    // rustango's pre-DB validation would catch this first; bypass it by
    // hitting the DB directly through sqlx with a raw INSERT.
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };

    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();

    let result =
        sqlx::query("INSERT INTO mig_user (id, name, age, is_active) VALUES ($1, $2, $3, $4)")
            .bind(1_i64)
            .bind("alice")
            .bind(200_i32) // > max = 150
            .bind(true)
            .execute(&pool)
            .await;
    assert!(result.is_err(), "expected CHECK violation, got: {result:?}");
    let msg = format!("{}", result.unwrap_err());
    assert!(
        msg.contains("check") || msg.contains("violates"),
        "expected CHECK violation, got: {msg}",
    );

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn varchar_length_is_enforced_by_db() {
    // name has max_length = 32 → VARCHAR(32). Bypass rustango validation
    // with a raw INSERT to confirm the DB-level limit.
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };

    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();

    let result =
        sqlx::query("INSERT INTO mig_user (id, name, age, is_active) VALUES ($1, $2, $3, $4)")
            .bind(2_i64)
            .bind("a".repeat(64))
            .bind(30_i32)
            .bind(true)
            .execute(&pool)
            .await;
    assert!(
        result.is_err(),
        "expected length violation, got: {result:?}"
    );

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn registered_models_returns_what_we_defined() {
    let names: Vec<&'static str> = migrate::registered_models()
        .into_iter()
        .map(|m| m.name)
        .collect();
    // Linker order isn't guaranteed; just check both are present.
    assert!(names.contains(&"MigUser"), "missing MigUser: {names:?}");
    assert!(names.contains(&"MigPost"), "missing MigPost: {names:?}");
}

#[tokio::test]
async fn apply_all_is_safe_to_call_after_drop_all() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };

    // First cycle.
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();
    // Second cycle.
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();

    // Schema is fresh; confirm a basic insert works.
    MigUser {
        id: 42,
        name: "fresh".into(),
        age: 25,
        is_active: true,
    }
    .insert(&pool)
    .await
    .unwrap();
    let count = MigUser::objects()
        .where_(MigUser::id.eq(42_i64))
        .fetch(&pool)
        .await
        .unwrap()
        .len();
    assert_eq!(count, 1);

    migrate::drop_all(&pool).await.unwrap();
}
