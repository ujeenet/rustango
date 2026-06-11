//! Tri-dialect emission + DDL tests for the pgvector vector column and
//! similarity-search operators — issue #824.
//!
//! pgvector is **PG-only by language semantics**: `vector(N)` DDL +
//! `<->` / `<=>` / `<#>` distance operators emit on Postgres; MySQL /
//! SQLite degrade the column to `TEXT` and the distance operators raise
//! `OpNotSupportedInDialect` at emit time. These tests pin both.

use rustango::core::{FieldType, Model as _, VectorMetric};
use rustango::sql::Vector;
use rustango::sql::{Dialect, MySql, Postgres, Sqlite};
use rustango::Model;

#[derive(Model)]
#[rustango(table = "vec_doc")]
#[allow(dead_code)]
pub struct Doc {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 80)]
    title: String,
    #[rustango(vector(dims = 3))]
    embedding: Vector,
}

#[test]
fn schema_field_is_vector_with_declared_dims() {
    let f = Doc::SCHEMA
        .fields
        .iter()
        .find(|f| f.column == "embedding")
        .expect("embedding field");
    assert_eq!(f.ty, FieldType::Vector(3));
}

#[test]
fn ddl_column_type_is_vector_n_on_pg_text_elsewhere() {
    assert_eq!(
        Postgres.column_type(FieldType::Vector(3), None),
        "vector(3)"
    );
    // 0 dims = unconstrained `vector`.
    assert_eq!(Postgres.column_type(FieldType::Vector(0), None), "vector");
    assert_eq!(MySql.column_type(FieldType::Vector(3), None), "TEXT");
    assert_eq!(Sqlite.column_type(FieldType::Vector(3), None), "TEXT");
}

fn distance_sql<D: Dialect>(
    d: &D,
    metric: VectorMetric,
) -> Result<String, rustango::sql::SqlError> {
    let q = Doc::objects()
        .order_by_distance("embedding", vec![1.0, 2.0, 3.0], metric)
        .compile()
        .expect("compile order_by_distance");
    d.compile_select(&q).map(|s| s.sql)
}

#[test]
fn pg_order_by_distance_emits_l2_operator_and_binds_vector() {
    let sql = distance_sql(&Postgres, VectorMetric::L2).expect("emit");
    assert!(
        sql.contains(r#"ORDER BY ("embedding" <-> $1)"#),
        "L2 order-by should use <->: {sql}"
    );
}

#[test]
fn pg_each_metric_emits_its_operator() {
    assert!(distance_sql(&Postgres, VectorMetric::L2)
        .unwrap()
        .contains("<->"));
    assert!(distance_sql(&Postgres, VectorMetric::Cosine)
        .unwrap()
        .contains("<=>"));
    assert!(distance_sql(&Postgres, VectorMetric::InnerProduct)
        .unwrap()
        .contains("<#>"));
}

#[test]
fn k_nearest_adds_limit() {
    let q = Doc::objects()
        .k_nearest("embedding", vec![1.0, 2.0, 3.0], 5, VectorMetric::Cosine)
        .compile()
        .expect("compile");
    let sql = Postgres.compile_select(&q).expect("emit").sql;
    assert!(sql.contains("<=>"), "cosine op: {sql}");
    assert!(sql.contains("LIMIT 5"), "k limit: {sql}");
}

#[test]
fn distance_operators_error_cleanly_on_mysql_and_sqlite() {
    for (name, res) in [
        ("mysql", distance_sql(&MySql, VectorMetric::L2)),
        ("sqlite", distance_sql(&Sqlite, VectorMetric::L2)),
    ] {
        let err = res.expect_err(&format!("{name} should reject the pgvector operator"));
        let msg = format!("{err}");
        assert!(
            msg.contains("pgvector") || msg.to_lowercase().contains("not supported"),
            "{name} error should name the unsupported pgvector op: {msg}"
        );
    }
}
