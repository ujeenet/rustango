#![cfg(feature = "postgres")]
//! Live PG end-to-end test for the new PG aggregates (issue #33).
//! Skips silently when `DATABASE_URL` is unset.

use std::sync::OnceLock;

use rustango::core::{AggregateExpr, AggregateQuery, SqlValue};
use rustango::sql::fetch_aggregate_on;
use rustango::sql::{sqlx, Auto};
use rustango::Model;
use tokio::sync::Mutex;

fn live_lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "pgal_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 50)]
    pub author: String,
    #[rustango(max_length = 50)]
    pub tag: String,
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    Some(
        sqlx::PgPool::connect(&url)
            .await
            .unwrap_or_else(|e| panic!("DATABASE_URL is set but unreachable ({url}): {e}")),
    )
}

async fn fresh(pool: &sqlx::PgPool) {
    sqlx::query(r#"DROP TABLE IF EXISTS "pgal_post" CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        r#"CREATE TABLE "pgal_post" (
            "id" BIGSERIAL PRIMARY KEY,
            "author" VARCHAR(50) NOT NULL,
            "tag" VARCHAR(50) NOT NULL
        )"#,
    )
    .execute(pool)
    .await
    .unwrap();
    // Three rows: alice has tags "rust" and "sql"; bob has "rust".
    for (author, tag) in [("alice", "rust"), ("alice", "sql"), ("bob", "rust")] {
        sqlx::query(r#"INSERT INTO "pgal_post" ("author", "tag") VALUES ($1, $2)"#)
            .bind(author)
            .bind(tag)
            .execute(pool)
            .await
            .unwrap();
    }
}

async fn cleanup(pool: &sqlx::PgPool) {
    sqlx::query(r#"DROP TABLE IF EXISTS "pgal_post" CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
}

fn agg_per_author(expr: AggregateExpr, alias: &'static str) -> AggregateQuery {
    let mut q = AggregateQuery::new(
        <Post as rustango::core::Model>::SCHEMA,
        vec![(alias.into(), expr)],
    );
    q.group_by = vec!["author"];
    q.order_by = vec![rustango::core::OrderClause {
        column: "author",
        desc: false,
    }
    .into()];
    q
}

#[tokio::test]
async fn array_agg_collects_tags_per_author() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    let q = agg_per_author(AggregateExpr::array_agg("tag"), "tags");
    let rows = fetch_aggregate_on(&q, &pool).await.unwrap();
    assert_eq!(rows.len(), 2, "two distinct authors");

    // Alice should have both rust + sql tags. The exact array
    // encoding depends on the SqlValue decoder — accept either a
    // List or a Json or a string-formatted array.
    let alice = rows
        .iter()
        .find(|r| matches!(r.get("author"), Some(SqlValue::String(s)) if s == "alice"))
        .expect("alice row");
    let tags = alice.get("tags").expect("tags col");
    let dbg = format!("{tags:?}");
    assert!(
        dbg.contains("rust") && dbg.contains("sql"),
        "alice's tags should include rust + sql: {dbg}"
    );

    cleanup(&pool).await;
}

#[tokio::test]
async fn string_agg_concatenates_tags_per_author() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    // Note: ordering inside string_agg is unspecified without an
    // ORDER BY inside the aggregate; sort the inputs by inserting
    // alphabetically before this call. For this test we accept
    // either "rust,sql" or "sql,rust".
    let q = agg_per_author(AggregateExpr::string_agg("tag", ","), "tag_list");
    let rows = fetch_aggregate_on(&q, &pool).await.unwrap();
    let alice = rows
        .iter()
        .find(|r| matches!(r.get("author"), Some(SqlValue::String(s)) if s == "alice"))
        .expect("alice row");
    let list = match alice.get("tag_list") {
        Some(SqlValue::String(s)) => s.clone(),
        other => panic!("expected String, got {other:?}"),
    };
    assert!(
        list == "rust,sql" || list == "sql,rust",
        "alice's tag_list should be the two tags joined: got {list}"
    );

    cleanup(&pool).await;
}

#[tokio::test]
async fn jsonb_agg_returns_json_array_per_author() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    let q = agg_per_author(AggregateExpr::jsonb_agg("tag"), "tag_json");
    let rows = fetch_aggregate_on(&q, &pool).await.unwrap();
    let alice = rows
        .iter()
        .find(|r| matches!(r.get("author"), Some(SqlValue::String(s)) if s == "alice"))
        .expect("alice row");
    let json = match alice.get("tag_json") {
        Some(SqlValue::Json(v)) => v.clone(),
        other => panic!("expected JSONB, got {other:?}"),
    };
    let arr = json.as_array().expect("array shape");
    assert_eq!(arr.len(), 2, "alice has 2 tags");
    let strs: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
    assert!(strs.contains(&"rust") && strs.contains(&"sql"), "{arr:?}");

    cleanup(&pool).await;
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "pgal_score")]
#[allow(dead_code)]
pub struct Score {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 50)]
    pub author: String,
    pub n32: i32,
    pub n64: i64,
}

/// `array_agg` of an INTEGER and a BIGINT column decodes as JSON arrays
/// of numbers (int[] and bigint[] cells), not NULL.
#[tokio::test]
async fn array_agg_decodes_int_and_bigint_array_cells() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    sqlx::query(r#"DROP TABLE IF EXISTS "pgal_score" CASCADE"#)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        r#"CREATE TABLE "pgal_score" (
            "id" BIGSERIAL PRIMARY KEY,
            "author" VARCHAR(50) NOT NULL,
            "n32" INTEGER NOT NULL,
            "n64" BIGINT NOT NULL
        )"#,
    )
    .execute(&pool)
    .await
    .unwrap();
    // Past i32 on the BIGINT side, so a narrowing decode would fail.
    for (n32, n64) in [(1_i32, 5_000_000_000_i64), (2, 7)] {
        Score {
            id: Auto::Unset,
            author: "alice".into(),
            n32,
            n64,
        }
        .insert_pool(&rustango::sql::Pool::Postgres(pool.clone()))
        .await
        .unwrap();
    }

    let mut q = AggregateQuery::new(
        <Score as rustango::core::Model>::SCHEMA,
        vec![
            ("n32s".into(), AggregateExpr::array_agg("n32")),
            ("n64s".into(), AggregateExpr::array_agg("n64")),
        ],
    );
    q.group_by = vec!["author"];
    let rows = fetch_aggregate_on(&q, &pool).await.unwrap();
    assert_eq!(rows.len(), 1);
    let sorted = |alias: &str| match rows[0].get(alias) {
        Some(SqlValue::Json(serde_json::Value::Array(a))) => {
            let mut v: Vec<i64> = a.iter().map(|n| n.as_i64().expect("number")).collect();
            v.sort_unstable();
            v
        }
        other => panic!("{alias}: expected a JSON array, got {other:?}"),
    };
    assert_eq!(sorted("n32s"), vec![1, 2]);
    assert_eq!(sorted("n64s"), vec![7, 5_000_000_000]);

    sqlx::query(r#"DROP TABLE IF EXISTS "pgal_score" CASCADE"#)
        .execute(&pool)
        .await
        .unwrap();
}
