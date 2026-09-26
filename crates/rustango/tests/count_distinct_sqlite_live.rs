//! v0.45 — live SQLite coverage for `AggregateExpr::CountDistinct`.
//!
//! Proves the new variant emits `COUNT(DISTINCT col)` and decodes
//! to an i64. Same dialect writer arms for PG / MySQL 8+ / SQLite —
//! the SQLite test is enough to cover the IR round-trip; the
//! dialect emission is a single straight-line write that doesn't
//! vary across backends (only the identifier quoting differs and
//! that's exercised in other tests).

#![cfg(feature = "sqlite")]

use rustango::core::{AggregateExpr, AggregateQuery, Model as _, SqlValue, WhereExpr};
use rustango::sql::{fetch_aggregate_pool, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "v045_cd_tag")]
pub struct CdTag {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub category: String,
}

async fn pool_with_tags() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite");
    rustango::sql::raw_execute_pool(
        &pool,
        "CREATE TABLE v045_cd_tag (id INTEGER PRIMARY KEY AUTOINCREMENT, category TEXT NOT NULL)",
        vec![],
    )
    .await
    .unwrap();
    // 4 rows across 2 categories — duplicates intentional.
    for cat in ["rust", "rust", "elixir", "elixir", "rust", "go"] {
        rustango::sql::raw_execute_pool(
            &pool,
            "INSERT INTO v045_cd_tag(category) VALUES (?)",
            vec![SqlValue::String(cat.to_owned())],
        )
        .await
        .unwrap();
    }
    pool
}

#[tokio::test]
async fn count_distinct_returns_unique_count_not_row_count() {
    let pool = pool_with_tags().await;

    // Total rows = 6
    let total_q = AggregateQuery::new(
        CdTag::SCHEMA,
        vec![("total".into(), AggregateExpr::Count(None))],
    );
    let totals: Vec<(i64,)> = fetch_aggregate_pool(&pool, &total_q).await.expect("total");
    assert_eq!(totals[0].0, 6, "row count");

    // Distinct categories = 3 (rust, elixir, go)
    let distinct_q = AggregateQuery::new(
        CdTag::SCHEMA,
        vec![("uniq".into(), AggregateExpr::CountDistinct("category"))],
    );
    let distincts: Vec<(i64,)> = fetch_aggregate_pool(&pool, &distinct_q)
        .await
        .expect("count distinct");
    assert_eq!(distincts[0].0, 3, "distinct count");
}

#[tokio::test]
async fn count_distinct_respects_where_clause() {
    use rustango::core::{Filter, Op};
    let pool = pool_with_tags().await;
    // WHERE category = 'rust' → 3 matching rows, 1 distinct value.
    let mut q = AggregateQuery::new(
        CdTag::SCHEMA,
        vec![("uniq".into(), AggregateExpr::CountDistinct("category"))],
    );
    q.where_clause = WhereExpr::Predicate(Filter::new(
        "category",
        Op::Eq,
        SqlValue::String("rust".to_owned()),
    ));
    let rows: Vec<(i64,)> = fetch_aggregate_pool(&pool, &q)
        .await
        .expect("count distinct");
    assert_eq!(rows[0].0, 1);
}
