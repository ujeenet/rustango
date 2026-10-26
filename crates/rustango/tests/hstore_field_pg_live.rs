#![cfg(feature = "postgres")]
//! Live PostgreSQL round-trip for `HStore` columns
//! (#342). Proves the typed field wrapper writes a native
//! `hstore` (no text-literal escaping) on INSERT and decodes it back
//! into `HStore` on SELECT.
//!
//! Skips locally when `DATABASE_URL` is unset or `hstore` can't be
//! created; fails in CI, where `postgres_test` provides both.

use std::sync::OnceLock;

use rustango::sql::{sqlx, Auto, HStore, Pool};
use rustango::Model;
use tokio::sync::Mutex;

fn live_lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "hs_product")]
#[allow(dead_code)]
pub struct Product {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub name: String,
    pub attrs: HStore,
}

/// Connect + ensure the `hstore` extension exists, or skip (fails in CI).
async fn pool() -> Option<Pool> {
    let url = rustango::testkit::matrix::required_env("DATABASE_URL")?;
    let pg = sqlx::PgPool::connect(&url)
        .await
        .unwrap_or_else(|e| panic!("DATABASE_URL is set but unreachable ({url}): {e}"));
    let pool: Pool = pg.into();
    rustango::testkit::matrix::pg_extension(&pool, "hstore")
        .await
        .then_some(pool)
}

async fn fresh(pool: &Pool) {
    let pg = pool.as_postgres().expect("postgres pool");
    sqlx::query(r#"DROP TABLE IF EXISTS "hs_product" CASCADE"#)
        .execute(pg)
        .await
        .unwrap();
    sqlx::query(
        r#"CREATE TABLE "hs_product" (
            "id"    BIGSERIAL PRIMARY KEY,
            "name"  VARCHAR(80) NOT NULL,
            "attrs" hstore NOT NULL DEFAULT ''
        )"#,
    )
    .execute(pg)
    .await
    .unwrap();
}

async fn insert(pool: &Pool, name: &str, attrs: HStore) -> i64 {
    let mut p = Product {
        id: Auto::default(),
        name: name.to_owned(),
        attrs,
    };
    p.save_pool(pool).await.unwrap();
    *p.id.get().unwrap()
}

#[tokio::test]
async fn hstore_round_trips() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    let id = insert(
        &pool,
        "widget",
        HStore::from_iter([("color", "red"), ("size", "L")]),
    )
    .await;

    let row = Product::objects()
        .filter("id", id)
        .first(&pool)
        .await
        .unwrap()
        .expect("row present");
    assert_eq!(row.name, "widget");
    assert_eq!(row.attrs.get("color"), Some(&Some("red".to_owned())));
    assert_eq!(row.attrs.get("size"), Some(&Some("L".to_owned())));
    assert_eq!(row.attrs.len(), 2);
}

#[tokio::test]
async fn hstore_null_value_and_special_chars_round_trip() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    let mut attrs = HStore::new();
    attrs.insert("note".to_owned(), Some("a => b, \"quoted\"".to_owned()));
    attrs.insert("missing".to_owned(), None);
    let id = insert(&pool, "tricky", attrs).await;

    let row = Product::objects()
        .filter("id", id)
        .first(&pool)
        .await
        .unwrap()
        .unwrap();
    // Native bind/decode handles `=>`, commas, and quotes in values
    // without any text-literal escaping on our side.
    assert_eq!(
        row.attrs.get("note"),
        Some(&Some("a => b, \"quoted\"".to_owned()))
    );
    assert_eq!(row.attrs.get("missing"), Some(&None));
}

#[tokio::test]
async fn empty_hstore_round_trips() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    let id = insert(&pool, "blank", HStore::new()).await;
    let row = Product::objects()
        .filter("id", id)
        .first(&pool)
        .await
        .unwrap()
        .unwrap();
    assert!(row.attrs.is_empty());
}
