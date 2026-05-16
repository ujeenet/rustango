//! Emission tests for Postgres full-text search `__search`
//! (issue #28). PG emits `to_tsvector(<col>) @@ plainto_tsquery(<p>)`;
//! MySQL and SQLite reject with `OpNotSupportedInDialect` at compile
//! time (their FTS shapes are schema-bound and don't translate
//! cleanly from a bare column).

use rustango::core::Column as _;
#[cfg(feature = "mysql")]
use rustango::sql::MySql;
#[cfg(feature = "sqlite")]
use rustango::sql::Sqlite;
use rustango::sql::{Dialect, Postgres};
use rustango::Model;

#[derive(Model)]
#[rustango(table = "fts_doc")]
#[allow(dead_code)]
pub struct Doc {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 200)]
    title: String,
}

// ---------- PG: native tsvector match ----------

#[test]
fn search_emits_tsvector_match_on_pg() {
    let q = Doc::objects()
        .where_(Doc::title.search("rust orm"))
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql
            .contains(r#"to_tsvector("title") @@ plainto_tsquery($1)"#),
        "PG search: {}",
        stmt.sql
    );
}

#[test]
fn search_binds_query_as_single_string_param() {
    let q = Doc::objects()
        .where_(Doc::title.search("rust orm"))
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert_eq!(stmt.params.len(), 1, "one param");
    match &stmt.params[0] {
        rustango::core::SqlValue::String(s) => assert_eq!(s, "rust orm"),
        other => panic!("expected SqlValue::String, got {other:?}"),
    }
}

// ---------- MySQL: rejected with clean error ----------

#[cfg(feature = "mysql")]
#[test]
fn search_rejects_on_mysql_with_op_not_supported() {
    use rustango::sql::SqlError;
    let q = Doc::objects()
        .where_(Doc::title.search("rust orm"))
        .compile()
        .unwrap();
    let err = MySql.compile_select(&q).unwrap_err();
    match err {
        SqlError::OpNotSupportedInDialect { op, dialect } => {
            assert!(op.contains("search"), "op label: {op}");
            assert_eq!(dialect, "mysql");
        }
        other => panic!("expected OpNotSupportedInDialect, got: {other:?}"),
    }
}

// ---------- SQLite: rejected with clean error ----------

#[cfg(feature = "sqlite")]
#[test]
fn search_rejects_on_sqlite_with_op_not_supported() {
    use rustango::sql::SqlError;
    let q = Doc::objects()
        .where_(Doc::title.search("rust orm"))
        .compile()
        .unwrap();
    let err = Sqlite.compile_select(&q).unwrap_err();
    match err {
        SqlError::OpNotSupportedInDialect { op, dialect } => {
            assert!(op.contains("search"), "op label: {op}");
            assert_eq!(dialect, "sqlite");
        }
        other => panic!("expected OpNotSupportedInDialect, got: {other:?}"),
    }
}

// ---------- Django-shape lookup parser ----------

#[test]
fn search_lookup_via_filter_string_parser() {
    let q = Doc::objects()
        .filter("title__search", "rust")
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql
            .contains(r#"to_tsvector("title") @@ plainto_tsquery($1)"#),
        "filter(\"title__search\", ...) routes to Op::Search: {}",
        stmt.sql
    );
}

#[test]
fn search_lookup_with_non_string_value_rejects_at_compile() {
    use rustango::core::SqlValue;
    let r = Doc::objects()
        .filter("title__search", SqlValue::I64(42))
        .compile();
    assert!(
        matches!(
            r,
            Err(rustango::core::QueryError::InvalidLookupValue { ref suffix, .. })
                if suffix == "search"
        ),
        "non-string value to __search surfaces InvalidLookupValue: {r:?}",
    );
}

#[test]
fn unknown_lookup_error_mentions_search_suffix() {
    let r = Doc::objects().filter("title__nope_typo", "value").compile();
    let err = r.unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("search"),
        "supported-lookups list should advertise `search`: {msg}"
    );
}
