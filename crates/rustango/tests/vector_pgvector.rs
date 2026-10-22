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

/// A PG-only value is refused when compiled for MySQL or SQLite, not left
/// to the bind step, which has no way to fail but a panic.
#[test]
fn pg_only_values_are_refused_at_compile_on_mysql_and_sqlite() {
    use rustango::core::{Assignment, DeleteQuery, Filter, Op, SqlValue, UpdateQuery, WhereExpr};
    use rustango::sql::SqlError;
    let values = [
        ("vector", SqlValue::Vector(vec![1.0, 2.0, 3.0])),
        ("array", SqlValue::Array(vec![SqlValue::I32(1)])),
        ("range", SqlValue::RangeLiteral("[1,2)".into())),
        ("hstore", SqlValue::HStore(vec![("k".into(), None)])),
        (
            "geometry",
            SqlValue::Geometry {
                x: 1.0,
                y: 2.0,
                srid: 4326,
            },
        ),
    ];
    let refused = |name: &str, kind: &str, res: Result<_, SqlError>| {
        let err = res.expect_err(&format!("{name} compiled a {kind}"));
        assert!(
            matches!(err, SqlError::ValueNotSupportedInDialect { value, dialect }
                if value == kind && dialect == name),
            "{name}: {err:?}"
        );
    };
    for (kind, value) in values {
        let q = UpdateQuery::new(
            Doc::SCHEMA,
            vec![Assignment::new("embedding", value.clone())],
            WhereExpr::Predicate(Filter::new("id", Op::Eq, 1_i64)),
        );
        assert!(Postgres.compile_update(&q).is_ok(), "{value:?}");
        refused("mysql", kind, MySql.compile_update(&q));
        refused("sqlite", kind, Sqlite.compile_update(&q));

        // Inside a list: `IN` binds each item, `=` binds the list whole.
        for op in [Op::In, Op::Eq] {
            let q = DeleteQuery::new(
                Doc::SCHEMA,
                WhereExpr::Predicate(Filter::new(
                    "id",
                    op,
                    SqlValue::List(vec![SqlValue::I64(1), value.clone()]),
                )),
            );
            refused("mysql", kind, MySql.compile_delete(&q));
            refused("sqlite", kind, Sqlite.compile_delete(&q));
        }
    }
}

/// Saving such a model on SQLite returns an error instead of panicking.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_insert_of_a_vector_errors() {
    let pool = rustango::sql::Pool::connect("sqlite::memory:")
        .await
        .unwrap();
    let doc = Doc {
        id: 1,
        title: "t".into(),
        embedding: Vector(vec![1.0, 2.0, 3.0]),
    };
    let err = doc
        .insert_pool(&pool)
        .await
        .expect_err("a vector on SQLite");
    assert!(err.to_string().contains("sqlite"), "{err}");
}
