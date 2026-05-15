//! Tri-dialect emission tests for PG aggregate functions (issue #33):
//! `array_agg`, `string_agg`, `jsonb_agg`. PG-only — non-PG backends
//! must emit `SqlError::AggregateNotSupportedInDialect`.

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
        aggregates: vec![(alias, expr)],
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
fn string_agg_rejected_on_non_pg() {
    let q = aggregate_query(AggregateExpr::string_agg("tag", ", "), "tags");
    assert!(matches!(
        MySql.compile_aggregate(&q),
        Err(SqlError::AggregateNotSupportedInDialect {
            aggregate: "string_agg",
            ..
        })
    ));
    assert!(matches!(
        Sqlite.compile_aggregate(&q),
        Err(SqlError::AggregateNotSupportedInDialect {
            aggregate: "string_agg",
            ..
        })
    ));
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
        aggregates: vec![("tags", AggregateExpr::array_agg("tag"))],
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
