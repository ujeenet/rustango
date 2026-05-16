//! PG `ArrayField` operators (`@>`, `<@`, `&&`) — issue #30 ops slice.
//!
//! Ships the Op variants + Column trait helpers + Django parser
//! routes. A `FieldType::Array(elem)` declaration shape follows in a
//! separate slice — for v1 the column is declared via raw migration
//! SQL (`tags TEXT[]`) and referenced by the typed-IR `Column::array_*`
//! methods, mirroring how trigram lookups land before the
//! `TrigramSimilarity` annotation.

use rustango::core::Column as _;
#[cfg(feature = "mysql")]
use rustango::sql::MySql;
#[cfg(feature = "sqlite")]
use rustango::sql::Sqlite;
use rustango::sql::{Dialect, Postgres};
use rustango::Model;

#[derive(Model)]
#[rustango(table = "arr_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 64)]
    title: String,
}

// ---------- PG: native @> / <@ / && ----------

#[test]
fn pg_emits_array_contains_operator() {
    let q = Post::objects()
        .where_(Post::title.array_contains(["rust", "orm"]))
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(r#""title" @> $1"#),
        "PG array_contains: {}",
        stmt.sql
    );
}

#[test]
fn pg_emits_array_contained_by_operator() {
    let q = Post::objects()
        .where_(Post::id.array_contained_by([1_i64, 2, 3]))
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(r#""id" <@ $1"#),
        "PG array_contained_by: {}",
        stmt.sql
    );
}

#[test]
fn pg_emits_array_overlap_operator() {
    let q = Post::objects()
        .where_(Post::title.array_overlap(["rust", "go"]))
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(r#""title" && $1"#),
        "PG array_overlap: {}",
        stmt.sql
    );
}

#[test]
fn pg_binds_array_as_single_param() {
    let q = Post::objects()
        .where_(Post::title.array_contains(["a", "b", "c"]))
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    // Single parameter — three elements bundled into one Array value,
    // not three placeholders.
    assert_eq!(stmt.params.len(), 1, "single PG array parameter");
    use rustango::core::SqlValue;
    let SqlValue::Array(elems) = &stmt.params[0] else {
        panic!("expected SqlValue::Array, got {:?}", stmt.params[0]);
    };
    assert_eq!(elems.len(), 3);
}

// ---------- MySQL / SQLite reject ----------

#[cfg(feature = "mysql")]
#[test]
fn mysql_rejects_array_contains_with_clean_error() {
    use rustango::sql::SqlError;
    let q = Post::objects()
        .where_(Post::title.array_contains(["x"]))
        .compile()
        .unwrap();
    let err = MySql.compile_select(&q).unwrap_err();
    match err {
        SqlError::OpNotSupportedInDialect { op, dialect } => {
            assert!(op.contains("array operators"), "op label: {op}");
            assert_eq!(dialect, "mysql");
        }
        other => panic!("expected OpNotSupportedInDialect, got: {other:?}"),
    }
}

#[cfg(feature = "sqlite")]
#[test]
fn sqlite_rejects_array_overlap_with_clean_error() {
    use rustango::sql::SqlError;
    let q = Post::objects()
        .where_(Post::id.array_overlap([1_i64]))
        .compile()
        .unwrap();
    let err = Sqlite.compile_select(&q).unwrap_err();
    assert!(matches!(
        err,
        SqlError::OpNotSupportedInDialect { op, .. } if op.contains("array operators")
    ));
}

// ---------- Django-shape parser routes ----------

#[test]
fn parser_routes_array_contains_lookup() {
    use rustango::core::SqlValue;
    let q = Post::objects()
        .filter(
            "title__array_contains",
            SqlValue::List(vec![
                SqlValue::String("rust".into()),
                SqlValue::String("orm".into()),
            ]),
        )
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(r#""title" @> $1"#),
        "parser route to ArrayContains: {}",
        stmt.sql
    );
}

#[test]
fn parser_routes_array_overlap_lookup() {
    use rustango::core::SqlValue;
    let q = Post::objects()
        .filter(
            "id__array_overlap",
            SqlValue::List(vec![SqlValue::I64(1), SqlValue::I64(2)]),
        )
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(r#""id" && $1"#),
        "parser route to ArrayOverlap: {}",
        stmt.sql
    );
}

#[test]
fn parser_rejects_non_list_value_for_array_lookup() {
    use rustango::core::{QueryError, SqlValue};
    let r = Post::objects()
        .filter(
            "title__array_contains",
            SqlValue::String("not a list".into()),
        )
        .compile();
    assert!(
        matches!(r, Err(QueryError::InvalidLookupValue { ref suffix, .. }) if suffix == "array_contains"),
        "non-list value rejected: {r:?}",
    );
}

#[test]
fn unknown_lookup_error_advertises_array_suffixes() {
    let r = Post::objects().filter("title__nope", "value").compile();
    let err = r.unwrap_err();
    let msg = format!("{err}");
    for suffix in ["array_contains", "array_contained_by", "array_overlap"] {
        assert!(
            msg.contains(suffix),
            "supported-lookups list should advertise `{suffix}`: {msg}"
        );
    }
}

// ---------- Writer-level: shape check ----------
//
// `Column::array_contains` always produces a `SqlValue::Array`, so the
// `ArrayOpRequiresArray` writer-level guard isn't reachable through
// the typed API. It's covered defensively by `forms::collect_values`
// (which uses `SqlValue::List`) and the `__array_*` parser route
// (which promotes `List` → `Array` automatically). Verify that
// promotion happens by inspecting the post-parser query:
#[test]
fn parser_promotes_list_to_array_so_writer_accepts_it() {
    use rustango::core::SqlValue;
    let q = Post::objects()
        .filter(
            "title__array_contains",
            SqlValue::List(vec![SqlValue::String("rust".into())]),
        )
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(stmt.sql.contains("@>"));
    assert_eq!(stmt.params.len(), 1);
    assert!(
        matches!(&stmt.params[0], SqlValue::Array(_)),
        "parser must promote List → Array for writer; got {:?}",
        stmt.params[0]
    );
}
