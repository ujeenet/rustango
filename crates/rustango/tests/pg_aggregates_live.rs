#![cfg(feature = "postgres")]
//! Live PG end-to-end test for the new PG aggregates (issue #33).
//! Skips silently when `DATABASE_URL` is unset.

use std::sync::OnceLock;

use rustango::core::{AggregateExpr, AggregateQuery, SqlValue, WhereExpr};
use rustango::sql::{fetch_aggregate, sqlx, Auto};
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
    sqlx::PgPool::connect(&url).await.ok()
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
    AggregateQuery {
        model: <Post as rustango::core::Model>::SCHEMA,
        where_clause: WhereExpr::And(vec![]),
        group_by: vec!["author"],
        aggregates: vec![(alias, expr)],
        having: None,
        order_by: vec![rustango::core::OrderClause {
            column: "author",
            desc: false,
        }
        .into()],
        limit: None,
        offset: None,
    }
}

#[tokio::test]
async fn array_agg_collects_tags_per_author() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    let q = agg_per_author(AggregateExpr::array_agg("tag"), "tags");
    let rows = fetch_aggregate(&q, &pool).await.unwrap();
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
    let rows = fetch_aggregate(&q, &pool).await.unwrap();
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
    let rows = fetch_aggregate(&q, &pool).await.unwrap();
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
