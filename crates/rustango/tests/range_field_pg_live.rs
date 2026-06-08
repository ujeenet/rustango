#![cfg(feature = "postgres")]
//! Live PostgreSQL round-trip for `Range<T>` columns — Django
//! `RangeField` family (#343). Proves the typed field wrapper writes a
//! native PG range on INSERT (via a range-literal bind) and decodes it
//! back into `Range<T>` on SELECT, and that the `@>` containment
//! operator filters on it.
//!
//! Skips silently when `DATABASE_URL` is unset (runs in CI's
//! `postgres_test` job).

use std::ops::Bound;
use std::sync::OnceLock;

use rustango::core::Column as _;
use rustango::sql::{sqlx, Auto, FetcherPool as _, Pool, Range};
use rustango::Model;
use tokio::sync::Mutex;

fn live_lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "rng_event")]
#[allow(dead_code)]
pub struct Event {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub name: String,
    pub seats: Range<i32>,
    pub valid_on: Range<chrono::NaiveDate>,
}

async fn pool() -> Option<Pool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pg = sqlx::PgPool::connect(&url).await.ok()?;
    Some(pg.into())
}

async fn fresh(pool: &Pool) {
    let pg = pool.as_postgres().expect("postgres pool");
    sqlx::query(r#"DROP TABLE IF EXISTS "rng_event" CASCADE"#)
        .execute(pg)
        .await
        .unwrap();
    sqlx::query(
        r#"CREATE TABLE "rng_event" (
            "id"       BIGSERIAL PRIMARY KEY,
            "name"     VARCHAR(80) NOT NULL,
            "seats"    int4range NOT NULL,
            "valid_on" daterange NOT NULL
        )"#,
    )
    .execute(pg)
    .await
    .unwrap();
}

fn date(y: i32, m: u32, d: u32) -> chrono::NaiveDate {
    chrono::NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

async fn insert(
    pool: &Pool,
    name: &str,
    seats: Range<i32>,
    valid_on: Range<chrono::NaiveDate>,
) -> i64 {
    let mut e = Event {
        id: Auto::default(),
        name: name.to_owned(),
        seats,
        valid_on,
    };
    e.save_pool(pool).await.unwrap();
    *e.id.get().unwrap()
}

#[tokio::test]
async fn range_columns_round_trip() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    let id = insert(
        &pool,
        "concert",
        Range::closed_open(1, 100),
        Range::closed_open(date(2025, 6, 1), date(2025, 6, 30)),
    )
    .await;

    let row = Event::objects()
        .filter("id", id)
        .first(&pool)
        .await
        .unwrap()
        .expect("row present");
    assert_eq!(row.name, "concert");
    // PG normalizes discrete ranges to the canonical `[lower, upper)` form.
    assert_eq!(row.seats.lower, Bound::Included(1));
    assert_eq!(row.seats.upper, Bound::Excluded(100));
    assert_eq!(row.valid_on.lower, Bound::Included(date(2025, 6, 1)));
    assert_eq!(row.valid_on.upper, Bound::Excluded(date(2025, 6, 30)));
}

#[tokio::test]
async fn unbounded_upper_round_trips() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    let id = insert(
        &pool,
        "open-ended",
        Range::at_least(5),
        Range::closed_open(date(2025, 1, 1), date(2026, 1, 1)),
    )
    .await;
    let row = Event::objects()
        .filter("id", id)
        .first(&pool)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.seats.lower, Bound::Included(5));
    assert_eq!(row.seats.upper, Bound::Unbounded);
}

#[tokio::test]
async fn range_contains_operator_filters() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    insert(
        &pool,
        "small",
        Range::closed_open(1, 10),
        Range::closed_open(date(2025, 1, 1), date(2025, 2, 1)),
    )
    .await;
    insert(
        &pool,
        "large",
        Range::closed_open(50, 200),
        Range::closed_open(date(2025, 1, 1), date(2025, 2, 1)),
    )
    .await;

    // `seats @> int4range(5,6)` — only "small" contains seat 5.
    let names: Vec<String> = Event::objects()
        .where_(Event::seats.range_contains("[5,6)"))
        .fetch_pool(&pool)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, vec!["small"]);
}
