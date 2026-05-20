#![cfg(feature = "postgres")]
//! Live test of `rustango::migrate::apply_all` against a real Postgres.
//!
//! Reads `DATABASE_URL`. If unset, every test returns silently — same
//! convention as other live tests. The tests in this file run in a
//! separate binary, so the inventory registry contains *only* the
//! models defined here. That makes `apply_all` deterministic.

use rustango::core::Column as _;
use rustango::migrate;
use rustango::sql::sqlx;
use rustango::{Auto, Model};
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

#[derive(Model, Debug, Clone)]
#[rustango(table = "mig_auto_user")]
pub struct AutoUser {
    #[rustango(primary_key)]
    id: rustango::Auto<i64>,
    #[rustango(max_length = 32)]
    name: String,
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

    let users: Vec<MigUser> = MigUser::objects().fetch_on(&pool).await.unwrap();
    assert_eq!(users.len(), 1);
    assert_eq!(users[0].name, "alice");

    let posts: Vec<MigPost> = MigPost::objects().fetch_on(&pool).await.unwrap();
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
async fn auto_pk_insert_populates_id_from_sequence() {
    // `Auto::Unset` on insert should drop the column from the INSERT
    // (so BIGSERIAL fires) and read the assigned value back via
    // RETURNING. The `&mut self` insert mutates the field in place.
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };

    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();

    let mut alice = AutoUser {
        id: Auto::default(),
        name: "alice".into(),
    };
    assert!(alice.id.is_unset());
    alice.insert(&pool).await.unwrap();
    assert!(alice.id.is_set(), "id must be populated after insert");
    let alice_id = *alice.id.get().unwrap();
    assert!(
        alice_id > 0,
        "BIGSERIAL should assign a positive id, got {alice_id}"
    );

    let mut bob = AutoUser {
        id: Auto::Unset,
        name: "bob".into(),
    };
    bob.insert(&pool).await.unwrap();
    let bob_id = *bob.id.get().unwrap();
    assert!(
        bob_id > alice_id,
        "second insert should get a strictly-greater id; got alice={alice_id}, bob={bob_id}"
    );

    let users: Vec<AutoUser> = AutoUser::objects().fetch_on(&pool).await.unwrap();
    assert_eq!(users.len(), 2);

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn auto_pk_set_value_is_honored() {
    // `Auto::Set(N)` should bypass the sequence and use the supplied
    // value verbatim — useful for fixtures, replication, idempotent
    // re-inserts.
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };

    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();

    let mut explicit = AutoUser {
        id: Auto::Set(9999),
        name: "explicit".into(),
    };
    explicit.insert(&pool).await.unwrap();
    assert_eq!(*explicit.id.get().unwrap(), 9999);

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn bulk_insert_non_auto_model_writes_n_rows_one_round_trip() {
    // Non-Auto model — fields written verbatim, no RETURNING needed.
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };

    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();

    let rows: Vec<MigUser> = (1..=10_i64)
        .map(|i| MigUser {
            id: i,
            name: format!("user_{i}"),
            age: 20 + i as i32,
            is_active: i % 2 == 0,
        })
        .collect();

    MigUser::bulk_insert(&rows, &pool).await.unwrap();

    let fetched: Vec<MigUser> = MigUser::objects().fetch_on(&pool).await.unwrap();
    assert_eq!(fetched.len(), 10, "all 10 rows should be present");

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn bulk_insert_auto_model_unset_path_populates_each_pk() {
    // Auto model, all rows Auto::Unset → sequence assigns each PK,
    // RETURNING populates `id` on every row in input order.
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };

    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();

    let mut rows: Vec<AutoUser> = (1..=5)
        .map(|i| AutoUser {
            id: Auto::Unset,
            name: format!("bulk_{i}"),
        })
        .collect();

    AutoUser::bulk_insert(&mut rows, &pool).await.unwrap();

    // Every row's id is now Set, in strictly-ascending order.
    let mut last: i64 = 0;
    for r in &rows {
        let v = *r.id.get().expect("Auto::Set after bulk_insert");
        assert!(v > last, "ids must be ascending; got {v} after {last}");
        last = v;
    }

    let fetched: Vec<AutoUser> = AutoUser::objects().fetch_on(&pool).await.unwrap();
    assert_eq!(fetched.len(), 5);

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn bulk_insert_auto_model_all_set_path_honors_supplied_ids() {
    // Auto model, all rows Auto::Set(N) → sequence is bypassed, the
    // user-supplied ids are stored verbatim.
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };

    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();

    let mut rows: Vec<AutoUser> = (100..=102)
        .map(|i: i64| AutoUser {
            id: Auto::Set(i),
            name: format!("explicit_{i}"),
        })
        .collect();

    AutoUser::bulk_insert(&mut rows, &pool).await.unwrap();

    for (i, r) in rows.iter().enumerate() {
        assert_eq!(*r.id.get().unwrap(), 100 + i as i64);
    }

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn bulk_insert_auto_model_mixed_set_unset_is_rejected() {
    // Mixing Set and Unset within one bulk_insert is rejected before
    // any DB work — the column list can't differ across rows.
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };

    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();

    let mut rows = vec![
        AutoUser {
            id: Auto::Unset,
            name: "first".into(),
        },
        AutoUser {
            id: Auto::Set(42),
            name: "second".into(),
        },
    ];

    let err = AutoUser::bulk_insert(&mut rows, &pool).await.unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("mixed Set/Unset") || msg.contains("Auto"),
        "got: {msg}"
    );

    // No rows should have been inserted.
    let fetched: Vec<AutoUser> = AutoUser::objects().fetch_on(&pool).await.unwrap();
    assert_eq!(fetched.len(), 0);

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn bulk_insert_empty_slice_is_noop() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };

    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();

    let mut empty: Vec<AutoUser> = Vec::new();
    AutoUser::bulk_insert(&mut empty, &pool).await.unwrap();
    let empty_non_auto: Vec<MigUser> = Vec::new();
    MigUser::bulk_insert(&empty_non_auto, &pool).await.unwrap();

    let fetched: Vec<AutoUser> = AutoUser::objects().fetch_on(&pool).await.unwrap();
    assert_eq!(fetched.len(), 0);

    migrate::drop_all(&pool).await.unwrap();
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
        .fetch_on(&pool)
        .await
        .unwrap()
        .len();
    assert_eq!(count, 1);

    migrate::drop_all(&pool).await.unwrap();
}
