#![cfg(feature = "postgres")]
//! Live test for the derive's `save()` method (v0.7 slice 1).
//!
//! `save()` is generated only for models whose primary key is
//! `Auto<T>`. Unset PK → INSERT (with RETURNING populating the PK).
//! Set PK → UPDATE every non-PK column WHERE pk = …. No-op match
//! returns `Ok(())` silently.
//!
//! Reads `DATABASE_URL`. If unset, every test returns silently.
//! Tests share one table, so they're serialized via a tokio mutex.

use std::sync::OnceLock;

use rustango::core::Op;
use rustango::sql::{sqlx, Auto};
use rustango::Model;
use tokio::sync::Mutex;

#[derive(Model, Debug, Clone)]
#[rustango(table = "rustango_save_thing")]
pub struct Thing {
    #[rustango(primary_key)]
    pub id: rustango::sql::Auto<i64>,
    #[rustango(max_length = 64)]
    pub label: String,
    pub count: i32,
}

fn save_lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn fresh_pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = sqlx::PgPool::connect(&url)
        .await
        .expect("connect to DATABASE_URL");
    sqlx::query("DROP TABLE IF EXISTS rustango_save_thing")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        r#"
        CREATE TABLE rustango_save_thing (
            id BIGSERIAL PRIMARY KEY,
            label TEXT NOT NULL,
            count INTEGER NOT NULL
        )
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();
    Some(pool)
}

#[tokio::test]
async fn save_inserts_when_pk_unset_and_populates_it() {
    let _g = save_lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let mut t = Thing {
        id: Auto::default(),
        label: "alpha".into(),
        count: 1,
    };
    assert!(matches!(t.id, Auto::Unset));

    t.save(&pool).await.unwrap();

    assert!(matches!(t.id, Auto::Set(_)));
    assert_eq!(Thing::objects().count_on(&pool).await.unwrap(), 1);
}

#[tokio::test]
async fn save_updates_when_pk_set_without_changing_pk() {
    let _g = save_lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let mut t = Thing {
        id: Auto::default(),
        label: "alpha".into(),
        count: 1,
    };
    t.save(&pool).await.unwrap();
    let original_id = match t.id {
        Auto::Set(v) => v,
        Auto::Unset => panic!("save() should have populated id"),
    };

    t.label = "beta".into();
    t.count = 42;
    t.save(&pool).await.unwrap();

    let after_id = match t.id {
        Auto::Set(v) => v,
        Auto::Unset => panic!("id should still be Set after update"),
    };
    assert_eq!(original_id, after_id);

    let fetched: Vec<Thing> = Thing::objects()
        .filter_op("id", Op::Eq, after_id)
        .fetch_on(&pool)
        .await
        .unwrap();
    assert_eq!(fetched.len(), 1);
    assert_eq!(fetched[0].label, "beta");
    assert_eq!(fetched[0].count, 42);
    assert_eq!(Thing::objects().count_on(&pool).await.unwrap(), 1);
}

#[tokio::test]
async fn save_with_set_pk_matching_no_row_is_silent_ok() {
    let _g = save_lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let mut t = Thing {
        id: Auto::Set(9999),
        label: "ghost".into(),
        count: 0,
    };
    t.save(&pool).await.unwrap();

    assert_eq!(Thing::objects().count_on(&pool).await.unwrap(), 0);
}
