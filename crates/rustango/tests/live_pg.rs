//! Live end-to-end test against a real Postgres instance.
//!
//! Reads `DATABASE_URL`. If unset, every test returns silently — so
//! `cargo test` stays green offline. CI sets `DATABASE_URL` and exercises
//! the full pipeline: create table → insert → fetch → update → delete.
//!
//! All tests share one table, so they're serialized via a tokio mutex to
//! avoid races when run in parallel.

use std::sync::OnceLock;

use rustango::core::{Op, QueryError, SqlValue};
use rustango::sql::{sqlx, Deleter, ExecError, Fetcher, Updater};
use rustango::Model;
use tokio::sync::Mutex;

#[derive(Model, Debug, PartialEq, Eq, Clone)]
#[rustango(table = "rustango_live_user")]
struct LiveUser {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(column = "user_name")]
    name: String,
    is_active: bool,
}

fn live_lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

/// Connect, drop+recreate the table, and seed three rows via the derive's
/// `.insert()`. Returns `None` if `DATABASE_URL` is unset (offline).
async fn fresh_pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
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

    for user in seed() {
        user.insert(&pool).await.unwrap();
    }
    Some(pool)
}

fn seed() -> Vec<LiveUser> {
    vec![
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
    ]
}

async fn count(pool: &sqlx::PgPool) -> i64 {
    use sqlx::Row;
    let row = sqlx::query("SELECT COUNT(*) FROM rustango_live_user")
        .fetch_one(pool)
        .await
        .unwrap();
    row.try_get::<i64, _>(0).unwrap()
}

#[tokio::test]
async fn read_pipeline_filters_and_in_clause() {
    let _g = live_lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let actives: Vec<LiveUser> = LiveUser::objects()
        .filter("is_active", Op::Eq, true)
        .fetch(&pool)
        .await
        .unwrap();
    let mut names: Vec<&str> = actives.iter().map(|u| u.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, vec!["alice", "carol"]);
    assert!(actives.iter().all(|u| u.is_active));

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

    let all: Vec<LiveUser> = LiveUser::objects().fetch(&pool).await.unwrap();
    assert_eq!(all.len(), 3);
}

#[tokio::test]
async fn bulk_update_changes_only_matching_rows() {
    let _g = live_lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let affected = LiveUser::objects()
        .eq("name", "alice")
        .update()
        .set("is_active", false)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(affected, 1);

    let alice = LiveUser::objects()
        .eq("id", 1_i64)
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(alice.len(), 1);
    assert!(!alice[0].is_active);

    // Bob & Carol untouched.
    let bob = LiveUser::objects()
        .eq("id", 2_i64)
        .fetch(&pool)
        .await
        .unwrap();
    assert!(!bob[0].is_active); // bob was already inactive
    let carol = LiveUser::objects()
        .eq("id", 3_i64)
        .fetch(&pool)
        .await
        .unwrap();
    assert!(carol[0].is_active);
}

#[tokio::test]
async fn bulk_update_with_no_filter_touches_every_row() {
    let _g = live_lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let affected = LiveUser::objects()
        .update()
        .set("is_active", true)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(affected, 3);

    let all: Vec<LiveUser> = LiveUser::objects().fetch(&pool).await.unwrap();
    assert_eq!(all.len(), 3);
    assert!(all.iter().all(|u| u.is_active));
}

#[tokio::test]
async fn bulk_update_with_multiple_set_columns() {
    let _g = live_lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let affected = LiveUser::objects()
        .eq("id", 2_i64)
        .update()
        .set("name", "BOB")
        .set("is_active", true)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(affected, 1);

    let bob = LiveUser::objects()
        .eq("id", 2_i64)
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(bob.len(), 1);
    assert_eq!(bob[0].name, "BOB");
    assert!(bob[0].is_active);
}

#[tokio::test]
async fn bulk_update_matching_zero_rows_returns_zero() {
    let _g = live_lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let affected = LiveUser::objects()
        .eq("id", 999_i64)
        .update()
        .set("is_active", false)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(affected, 0);
    assert_eq!(count(&pool).await, 3);
}

#[tokio::test]
async fn bulk_delete_removes_matching_rows() {
    let _g = live_lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let affected = LiveUser::objects()
        .eq("is_active", false)
        .delete(&pool)
        .await
        .unwrap();
    assert_eq!(affected, 1);
    assert_eq!(count(&pool).await, 2);

    // Remaining rows are alice and carol.
    let mut remaining: Vec<i64> = LiveUser::objects()
        .fetch(&pool)
        .await
        .unwrap()
        .into_iter()
        .map(|u| u.id)
        .collect();
    remaining.sort_unstable();
    assert_eq!(remaining, vec![1, 3]);
}

#[tokio::test]
async fn bulk_delete_with_in_clause_removes_listed_rows() {
    let _g = live_lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let affected = LiveUser::objects()
        .filter(
            "id",
            Op::In,
            SqlValue::List(vec![SqlValue::I64(1), SqlValue::I64(3)]),
        )
        .delete(&pool)
        .await
        .unwrap();
    assert_eq!(affected, 2);

    let remaining: Vec<LiveUser> = LiveUser::objects().fetch(&pool).await.unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].id, 2);
}

#[tokio::test]
async fn instance_delete_targets_only_that_pk() {
    let _g = live_lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let alice = LiveUser {
        id: 1,
        name: "alice".into(),
        is_active: true,
    };
    let affected = alice.delete(&pool).await.unwrap();
    assert_eq!(affected, 1);
    assert_eq!(count(&pool).await, 2);

    // Re-deleting the same instance is idempotent (0 rows affected).
    let affected = alice.delete(&pool).await.unwrap();
    assert_eq!(affected, 0);
}

// ---------------- bounded model: max_length / min / max ----------------

#[derive(Model, Debug, Clone)]
#[rustango(table = "rustango_live_bounded")]
#[allow(dead_code)]
struct BoundedUser {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 8)]
    name: String,
    #[rustango(min = 0, max = 150)]
    age: i32,
}

async fn fresh_bounded_pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = sqlx::PgPool::connect(&url)
        .await
        .expect("connect to DATABASE_URL");

    sqlx::query("DROP TABLE IF EXISTS rustango_live_bounded")
        .execute(&pool)
        .await
        .unwrap();
    // The DB-level VARCHAR is wider than the rustango bound on purpose: this
    // way the rustango-side check fires *before* the DB-level check, so we
    // can prove validation runs without ever opening a query.
    sqlx::query(
        r"
        CREATE TABLE rustango_live_bounded (
            id BIGINT PRIMARY KEY,
            name TEXT NOT NULL,
            age INTEGER NOT NULL
        )
        ",
    )
    .execute(&pool)
    .await
    .unwrap();
    Some(pool)
}

#[tokio::test]
async fn insert_within_bounds_succeeds() {
    let _g = live_lock().lock().await;
    let Some(pool) = fresh_bounded_pool().await else {
        return;
    };
    BoundedUser {
        id: 1,
        name: "alice".into(),
        age: 30,
    }
    .insert(&pool)
    .await
    .unwrap();

    let rows: Vec<BoundedUser> = BoundedUser::objects().fetch(&pool).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "alice");
    assert_eq!(rows[0].age, 30);
}

#[tokio::test]
async fn insert_too_long_string_is_rejected_before_db() {
    let _g = live_lock().lock().await;
    let Some(pool) = fresh_bounded_pool().await else {
        return;
    };
    let bad = BoundedUser {
        id: 2,
        name: "this_is_way_too_long".into(), // > 8 chars
        age: 30,
    };
    let err = bad.insert(&pool).await.unwrap_err();
    assert!(matches!(
        err,
        ExecError::Query(QueryError::MaxLengthExceeded { max: 8, .. })
    ));
    // Confirm DB unchanged.
    let rows: Vec<BoundedUser> = BoundedUser::objects().fetch(&pool).await.unwrap();
    assert!(rows.is_empty());
}

#[tokio::test]
async fn insert_age_below_min_is_rejected() {
    let _g = live_lock().lock().await;
    let Some(pool) = fresh_bounded_pool().await else {
        return;
    };
    let err = BoundedUser {
        id: 3,
        name: "ok".into(),
        age: -1,
    }
    .insert(&pool)
    .await
    .unwrap_err();
    assert!(matches!(
        err,
        ExecError::Query(QueryError::OutOfRange { value: -1, .. })
    ));
}

#[tokio::test]
async fn insert_age_above_max_is_rejected() {
    let _g = live_lock().lock().await;
    let Some(pool) = fresh_bounded_pool().await else {
        return;
    };
    let err = BoundedUser {
        id: 4,
        name: "ok".into(),
        age: 9999,
    }
    .insert(&pool)
    .await
    .unwrap_err();
    assert!(matches!(
        err,
        ExecError::Query(QueryError::OutOfRange { value: 9999, .. })
    ));
}

#[tokio::test]
async fn insert_at_inclusive_bounds_is_accepted() {
    let _g = live_lock().lock().await;
    let Some(pool) = fresh_bounded_pool().await else {
        return;
    };
    BoundedUser {
        id: 5,
        name: "12345678".into(), // exactly 8 chars
        age: 0,
    }
    .insert(&pool)
    .await
    .unwrap();
    BoundedUser {
        id: 6,
        name: "ok".into(),
        age: 150,
    }
    .insert(&pool)
    .await
    .unwrap();

    let rows: Vec<BoundedUser> = BoundedUser::objects().fetch(&pool).await.unwrap();
    assert_eq!(rows.len(), 2);
}

#[tokio::test]
async fn update_too_long_string_is_rejected_before_db() {
    let _g = live_lock().lock().await;
    let Some(pool) = fresh_bounded_pool().await else {
        return;
    };
    BoundedUser {
        id: 7,
        name: "ok".into(),
        age: 25,
    }
    .insert(&pool)
    .await
    .unwrap();

    let err = BoundedUser::objects()
        .eq("id", 7_i64)
        .update()
        .set("name", "this_is_way_too_long")
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        ExecError::Query(QueryError::MaxLengthExceeded { max: 8, .. })
    ));

    // Row unchanged.
    let rows: Vec<BoundedUser> = BoundedUser::objects()
        .eq("id", 7_i64)
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(rows[0].name, "ok");
}

#[tokio::test]
async fn update_out_of_range_age_is_rejected() {
    let _g = live_lock().lock().await;
    let Some(pool) = fresh_bounded_pool().await else {
        return;
    };
    BoundedUser {
        id: 8,
        name: "ok".into(),
        age: 25,
    }
    .insert(&pool)
    .await
    .unwrap();

    let err = BoundedUser::objects()
        .update()
        .set("age", 200_i32)
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        ExecError::Query(QueryError::OutOfRange { value: 200, .. })
    ));
}

#[tokio::test]
async fn filter_with_long_string_still_works_at_runtime() {
    // Filters intentionally bypass max_length validation: a long filter
    // value just doesn't match any row, it doesn't error out.
    let _g = live_lock().lock().await;
    let Some(pool) = fresh_bounded_pool().await else {
        return;
    };
    BoundedUser {
        id: 9,
        name: "ok".into(),
        age: 25,
    }
    .insert(&pool)
    .await
    .unwrap();

    let rows: Vec<BoundedUser> = BoundedUser::objects()
        .eq("name", "definitely_longer_than_eight")
        .fetch(&pool)
        .await
        .unwrap();
    assert!(rows.is_empty());
}

#[tokio::test]
async fn full_crud_round_trip() {
    let _g = live_lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    // INSERT a fourth row.
    let dave = LiveUser {
        id: 4,
        name: "dave".into(),
        is_active: false,
    };
    dave.insert(&pool).await.unwrap();
    assert_eq!(count(&pool).await, 4);

    // UPDATE dave to active and rename.
    let affected = LiveUser::objects()
        .eq("id", 4_i64)
        .update()
        .set("is_active", true)
        .set("name", "DAVE")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(affected, 1);

    // FETCH the updated row.
    let dave_fetched: Vec<LiveUser> = LiveUser::objects()
        .eq("id", 4_i64)
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(dave_fetched.len(), 1);
    assert_eq!(dave_fetched[0].name, "DAVE");
    assert!(dave_fetched[0].is_active);

    // DELETE via QuerySet.
    let affected = LiveUser::objects()
        .eq("id", 4_i64)
        .delete(&pool)
        .await
        .unwrap();
    assert_eq!(affected, 1);
    assert_eq!(count(&pool).await, 3);

    // Confirm gone.
    let gone: Vec<LiveUser> = LiveUser::objects()
        .eq("id", 4_i64)
        .fetch(&pool)
        .await
        .unwrap();
    assert!(gone.is_empty());
}
