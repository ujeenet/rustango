//! Tri-dialect emission tests for PG aggregate functions (issue #33):
//! `array_agg`, `string_agg`, `jsonb_agg`. `array_agg` / `jsonb_agg`
//! stay PG-only (non-PG emits `SqlError::AggregateNotSupportedInDialect`);
//! `string_agg` is database-agnostic as of Django 6.0 (#1024) — it
//! lowers to GROUP_CONCAT (MySQL) / group_concat (SQLite).

use rustango::core::{AggregateExpr, AggregateQuery, SqlValue, WhereExpr};
use rustango::sql::{Dialect, MySql, Postgres, SqlError, Sqlite};
use rustango::Model;

#[derive(Model)]
#[rustango(table = "pga_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 50)]
    tag: String,
    #[rustango(max_length = 50)]
    author: String,
}

fn aggregate_query(expr: AggregateExpr, alias: &'static str) -> AggregateQuery {
    AggregateQuery {
        model: <Post as rustango::core::Model>::SCHEMA,
        where_clause: WhereExpr::And(vec![]),
        group_by: Vec::new(),
        aggregates: vec![(alias.into(), expr)],
        aliases: vec![],
        having: None,
        order_by: Vec::new(),
        limit: None,
        offset: None,
    }
}

// ---------- array_agg ----------

#[test]
fn array_agg_emits_pg_native_call() {
    let q = aggregate_query(AggregateExpr::array_agg("tag"), "tags");
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"array_agg("tag")"#),
        "expected array_agg(\"tag\") in: {}",
        stmt.sql
    );
    assert!(stmt.sql.contains(r#"AS "tags""#), "got: {}", stmt.sql);
}

#[test]
fn array_agg_distinct_emits_distinct() {
    let q = aggregate_query(AggregateExpr::array_agg_distinct("tag"), "tags");
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"array_agg(DISTINCT "tag")"#),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn array_agg_rejected_on_mysql() {
    let q = aggregate_query(AggregateExpr::array_agg("tag"), "tags");
    let r = MySql.compile_aggregate(&q);
    match r {
        Err(SqlError::AggregateNotSupportedInDialect { aggregate, dialect }) => {
            assert_eq!(aggregate, "array_agg");
            assert_eq!(dialect, "mysql");
        }
        other => panic!("expected AggregateNotSupportedInDialect, got {other:?}"),
    }
}

#[test]
fn array_agg_rejected_on_sqlite() {
    let q = aggregate_query(AggregateExpr::array_agg("tag"), "tags");
    let r = Sqlite.compile_aggregate(&q);
    assert!(matches!(
        r,
        Err(SqlError::AggregateNotSupportedInDialect {
            aggregate: "array_agg",
            dialect: "sqlite"
        })
    ));
}

// ---------- string_agg ----------

#[test]
fn string_agg_emits_pg_call_with_bound_delimiter() {
    let q = aggregate_query(AggregateExpr::string_agg("tag", ", "), "tag_list");
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    // The delimiter binds as $N — the SQL contains `string_agg("tag", $1)`
    assert!(
        stmt.sql.contains(r#"string_agg("tag", $1)"#),
        "expected bound delimiter: {}",
        stmt.sql
    );
    assert_eq!(stmt.params, vec![SqlValue::String(", ".into())]);
}

#[test]
fn string_agg_distinct_emits_distinct() {
    let q = aggregate_query(AggregateExpr::string_agg_distinct("tag", "; "), "tag_list");
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"string_agg(DISTINCT "tag", $1)"#),
        "got: {}",
        stmt.sql
    );
    assert_eq!(stmt.params, vec![SqlValue::String("; ".into())]);
}

#[test]
fn string_agg_lowers_on_mysql_and_sqlite() {
    // #1024 — Django 6.0 made StringAgg database-agnostic. MySQL maps to
    // GROUP_CONCAT (delimiter inlined into SEPARATOR), SQLite to
    // group_concat (delimiter bound as a parameter).
    let q = aggregate_query(AggregateExpr::string_agg("tag", ", "), "tags");
    let my = MySql.compile_aggregate(&q).unwrap();
    assert!(
        my.sql.contains("GROUP_CONCAT(`tag` SEPARATOR ', ')"),
        "MySQL GROUP_CONCAT: {}",
        my.sql
    );
    let sq = Sqlite.compile_aggregate(&q).unwrap();
    assert!(
        sq.sql.contains(r#"group_concat("tag", "#),
        "SQLite group_concat: {}",
        sq.sql
    );
    assert_eq!(sq.params, vec![SqlValue::String(", ".into())]);
}

// ---------- any_value (#1025) ----------

#[test]
fn any_value_emits_per_dialect() {
    // Django 6.0 AnyValue: PG `any_value()`, MySQL `ANY_VALUE()`, SQLite
    // has neither so it falls back to `min()` (deterministic).
    let q = aggregate_query(AggregateExpr::AnyValue("tag"), "any_tag");
    assert!(
        Postgres
            .compile_aggregate(&q)
            .unwrap()
            .sql
            .contains(r#"any_value("tag")"#),
        "PG any_value"
    );
    assert!(
        MySql
            .compile_aggregate(&q)
            .unwrap()
            .sql
            .contains("ANY_VALUE(`tag`)"),
        "MySQL ANY_VALUE"
    );
    assert!(
        Sqlite
            .compile_aggregate(&q)
            .unwrap()
            .sql
            .contains(r#"min("tag")"#),
        "SQLite min() fallback"
    );
}

// ---------- string_agg ORDER BY (#1026) ----------

#[test]
fn string_agg_ordered_emits_per_dialect() {
    let q = aggregate_query(
        AggregateExpr::string_agg_ordered("tag", ", ", &[("tag", true)]),
        "tag_list",
    );
    assert!(
        Postgres
            .compile_aggregate(&q)
            .unwrap()
            .sql
            .contains(r#"string_agg("tag", $1 ORDER BY "tag" DESC)"#),
        "PG ordered"
    );
    assert!(
        MySql
            .compile_aggregate(&q)
            .unwrap()
            .sql
            .contains("GROUP_CONCAT(`tag` ORDER BY `tag` DESC SEPARATOR ', ')"),
        "MySQL ordered (ORDER BY before SEPARATOR)"
    );
    // SQLite — ORDER BY inside group_concat (3.44+); ascending here.
    let asc = aggregate_query(
        AggregateExpr::string_agg_ordered("tag", ", ", &[("tag", false)]),
        "tag_list",
    );
    assert!(
        Sqlite
            .compile_aggregate(&asc)
            .unwrap()
            .sql
            .contains(r#"ORDER BY "tag")"#),
        "SQLite ordered"
    );
}

#[test]
fn string_agg_distinct_ordered_by_non_agg_column_is_rejected() {
    // DISTINCT aggregates may only ORDER BY the aggregated column.
    let q = aggregate_query(
        AggregateExpr::string_agg_distinct_ordered("tag", ",", &[("author", true)]),
        "tag_list",
    );
    assert!(matches!(
        Postgres.compile_aggregate(&q),
        Err(SqlError::AggregateNotSupportedInDialect { .. })
    ));
    // Ordering by the agg column itself is fine.
    let ok = aggregate_query(
        AggregateExpr::string_agg_distinct_ordered("tag", ",", &[("tag", false)]),
        "tag_list",
    );
    assert!(Postgres.compile_aggregate(&ok).is_ok());
}

// ---------- jsonb_agg ----------

#[test]
fn jsonb_agg_emits_pg_native_call() {
    let q = aggregate_query(AggregateExpr::jsonb_agg("tag"), "tag_json");
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"jsonb_agg("tag")"#),
        "got: {}",
        stmt.sql
    );
    assert!(
        stmt.sql.contains(r#"AS "tag_json""#),
        "alias should be quoted: {}",
        stmt.sql
    );
}

#[test]
fn jsonb_agg_rejected_on_non_pg() {
    let q = aggregate_query(AggregateExpr::jsonb_agg("tag"), "j");
    assert!(matches!(
        MySql.compile_aggregate(&q),
        Err(SqlError::AggregateNotSupportedInDialect {
            aggregate: "jsonb_agg",
            ..
        })
    ));
    assert!(matches!(
        Sqlite.compile_aggregate(&q),
        Err(SqlError::AggregateNotSupportedInDialect {
            aggregate: "jsonb_agg",
            ..
        })
    ));
}

// ---------- mixed with GROUP BY ----------

#[test]
fn array_agg_composes_with_group_by() {
    let q = AggregateQuery {
        model: <Post as rustango::core::Model>::SCHEMA,
        where_clause: WhereExpr::And(vec![]),
        group_by: vec!["author"],
        aggregates: vec![("tags".into(), AggregateExpr::array_agg("tag"))],
        aliases: vec![],
        having: None,
        order_by: Vec::new(),
        limit: None,
        offset: None,
    };
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    // SELECT "author", array_agg("tag") AS "tags" FROM "pga_post" GROUP BY "author"
    assert!(stmt.sql.contains(r#"SELECT "author""#));
    assert!(stmt.sql.contains(r#"array_agg("tag")"#));
    assert!(stmt.sql.contains(r#"GROUP BY "author""#));
}
