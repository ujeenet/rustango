//! Live end-to-end test against a real Postgres instance.
//!
//! Reads `DATABASE_URL`. If unset, the test returns successfully without
//! connecting — so `cargo test` stays green offline. CI sets `DATABASE_URL`
//! and runs through the full path: create table → insert → fetch via
//! `QuerySet`/`Fetcher` → assert row contents.

use rustango::core::{Op, SqlValue};
use rustango::sql::sqlx;
use rustango::sql::Fetcher;
use rustango::Model;

#[derive(Model, Debug, PartialEq, Eq)]
#[rustango(table = "rustango_live_user")]
struct LiveUser {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(column = "user_name")]
    name: String,
    is_active: bool,
}

#[tokio::test]
async fn end_to_end_pipeline_against_postgres() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let pool = sqlx::PgPool::connect(&url)
        .await
        .expect("connect to DATABASE_URL");

    sqlx::query("DROP TABLE IF EXISTS rustango_live_user")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        r"
        CREATE TABLE rustango_live_user (
            id BIGINT PRIMARY KEY,
            user_name TEXT NOT NULL,
            is_active BOOLEAN NOT NULL
        )
        ",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Seed three rows via the derive-generated `insert()` rather than raw SQL —
    // exercises the write path, the FromRow path follows on read.
    for user in [
        LiveUser {
            id: 1,
            name: "alice".into(),
            is_active: true,
        },
        LiveUser {
            id: 2,
            name: "bob".into(),
            is_active: false,
        },
        LiveUser {
            id: 3,
            name: "carol".into(),
            is_active: true,
        },
    ] {
        user.insert(&pool).await.unwrap();
    }

    // 1. Equality filter — pulls active users.
    let actives: Vec<LiveUser> = LiveUser::objects()
        .filter("is_active", Op::Eq, true)
        .fetch(&pool)
        .await
        .unwrap();
    let mut names: Vec<&str> = actives.iter().map(|u| u.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, vec!["alice", "carol"]);
    assert!(actives.iter().all(|u| u.is_active));

    // 2. IN filter — explicit ID set.
    let picked: Vec<LiveUser> = LiveUser::objects()
        .filter(
            "id",
            Op::In,
            SqlValue::List(vec![SqlValue::I64(1), SqlValue::I64(3)]),
        )
        .fetch(&pool)
        .await
        .unwrap();
    let mut ids: Vec<i64> = picked.iter().map(|u| u.id).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 3]);

    // 3. No filter — all rows.
    let all: Vec<LiveUser> = LiveUser::objects().fetch(&pool).await.unwrap();
    assert_eq!(all.len(), 3);

    sqlx::query("DROP TABLE rustango_live_user")
        .execute(&pool)
        .await
        .unwrap();
}
