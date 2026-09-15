//! JSON path lookups on every backend — one body, three dialects
//! (#296, #1461).
//!
//! Replaces `json_path_mysql_live.rs` and `json_path_sqlite_live.rs`.
//! **PostgreSQL had no JSON-path suite at all**, which is the odd part:
//! it has the richest JSON support of the three — `jsonb`, `->`, `->>`,
//! `#>` — and the least coverage of it here. Not a decision, just the
//! cost of writing a third copy by hand.
//!
//! What is asserted is that the *emitted SQL executes*. Each dialect
//! spells the extraction differently —
//!
//! | | emission |
//! |---|---|
//! | PostgreSQL | `data ->> 'city'` |
//! | MySQL | `JSON_UNQUOTE(JSON_EXTRACT(data, '$.city'))` |
//! | SQLite | `json_extract(data, '$.city')` |
//!
//! — so a writer that produces something plausible but unparseable is
//! the failure this catches, and it can only be caught by handing the
//! statement to a real database. `EXPLAIN` does that without depending
//! on the row data matching.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::core::funcs::{json_path, json_path_indexed};
use rustango::core::{Expr, JsonPathStep, Op, SqlValue, WhereExpr, F};
use rustango::sql::{explain_pool, Auto, ExplainOptions, Pool};
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(table = "json_path_tri_demo")]
#[rustango(app = "json_path_tri")]
#[allow(dead_code)]
pub struct Demo {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    /// `JSONB` on PostgreSQL, `JSON` on MySQL, `TEXT` on SQLite — picked
    /// by the emitter. The three files this replaces each hand-wrote
    /// their own column type, which is why none of them could be run
    /// anywhere else.
    pub data: serde_json::Value,
}

async fn seeded(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Demo>(pool).await;

    let mut row = Demo {
        id: Auto::Unset,
        data: serde_json::json!({
            "address": { "city": "NYC" },
            "items": [ { "name": "x" } ],
        }),
    };
    row.insert_pool(pool).await.expect("seed JSON row");
}

/// Compile a predicate around `e` and hand it to the database.
///
/// `EXPLAIN` rather than `SELECT`: the claim is that the emission is
/// executable SQL for this dialect, and a plan proves the server
/// accepted it without the test also having to agree about what the
/// rows contain.
async fn assert_expr_executes(pool: &Pool, e: Expr) {
    use rustango::query::QuerySet;

    let qs = QuerySet::<Demo>::default().where_raw(WhereExpr::ExprCompare {
        lhs: e,
        op: Op::Eq,
        rhs: Expr::Literal(SqlValue::String("NYC".into())),
    });
    let q = qs.compile().expect("compile");
    let plan = explain_pool(pool, &q, ExplainOptions::default())
        .await
        .unwrap_or_else(|e| {
            panic!(
                "the emitted JSON-path SQL was rejected by {}: {e}",
                pool.dialect().name()
            )
        });
    assert!(!plan.is_empty(), "expected a non-empty plan");
}

async fn a_single_key_path_executes(pool: &Pool) {
    assert_expr_executes(pool, json_path(F("data"), &["city"], true)).await;
}

async fn a_nested_key_path_executes(pool: &Pool) {
    assert_expr_executes(pool, json_path(F("data"), &["address", "city"], true)).await;
}

/// Array indexing is where the three spellings diverge most — `->0`,
/// `$[0]`, `$.items[0]` — so it is the step most likely to emit
/// something one backend cannot parse.
async fn an_array_indexed_path_executes(pool: &Pool) {
    assert_expr_executes(
        pool,
        json_path_indexed(
            F("data"),
            [
                JsonPathStep::Key("items".into()),
                JsonPathStep::Index(0),
                JsonPathStep::Key("name".into()),
            ],
            true,
        ),
    )
    .await;
}

tri_dialect_test! {
    setup: seeded,
    scenarios: [
        a_single_key_path_executes,
        a_nested_key_path_executes,
        an_array_indexed_path_executes,
    ],
}
