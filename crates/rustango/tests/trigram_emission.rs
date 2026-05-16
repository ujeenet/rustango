//! Emission tests for `__trigram_similar` / `__trigram_word_similar`
//! (issue #29). PG emits `%` / `%>` pg_trgm operators; MySQL and
//! SQLite reject with `OpNotSupportedInDialect` at compile time.

use rustango::core::Column as _;
#[cfg(feature = "mysql")]
use rustango::sql::MySql;
#[cfg(feature = "sqlite")]
use rustango::sql::Sqlite;
use rustango::sql::{Dialect, Postgres};
use rustango::Model;

#[derive(Model)]
#[rustango(table = "trg_user")]
#[allow(dead_code)]
pub struct User {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 64)]
    name: String,
}

// ---------- PG: native pg_trgm operators ----------

#[test]
fn trigram_similar_emits_percent_on_pg() {
    let q = User::objects()
        .where_(User::name.trigram_similar("Rusty"))
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(r#""name" % $1"#),
        "PG trigram_similar: {}",
        stmt.sql
    );
}

#[test]
fn trigram_word_similar_emits_percent_gt_on_pg() {
    let q = User::objects()
        .where_(User::name.trigram_word_similar("Rust"))
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(r#""name" %> $1"#),
        "PG trigram_word_similar: {}",
        stmt.sql
    );
}

// ---------- MySQL: rejected with clean error ----------

#[cfg(feature = "mysql")]
#[test]
fn trigram_similar_rejects_on_mysql_with_op_not_supported() {
    use rustango::sql::SqlError;
    let q = User::objects()
        .where_(User::name.trigram_similar("Rusty"))
        .compile()
        .unwrap();
    let err = MySql.compile_select(&q).unwrap_err();
    match err {
        SqlError::OpNotSupportedInDialect { op, dialect } => {
            assert!(op.contains("trigram"), "op label: {op}");
            assert_eq!(dialect, "mysql");
        }
        other => panic!("expected OpNotSupportedInDialect, got: {other:?}"),
    }
}

#[cfg(feature = "mysql")]
#[test]
fn trigram_word_similar_rejects_on_mysql() {
    use rustango::sql::SqlError;
    let q = User::objects()
        .where_(User::name.trigram_word_similar("Rust"))
        .compile()
        .unwrap();
    let err = MySql.compile_select(&q).unwrap_err();
    assert!(matches!(err, SqlError::OpNotSupportedInDialect { .. }));
}

// ---------- SQLite: rejected with clean error ----------

#[cfg(feature = "sqlite")]
#[test]
fn trigram_similar_rejects_on_sqlite_with_op_not_supported() {
    use rustango::sql::SqlError;
    let q = User::objects()
        .where_(User::name.trigram_similar("Rusty"))
        .compile()
        .unwrap();
    let err = Sqlite.compile_select(&q).unwrap_err();
    match err {
        SqlError::OpNotSupportedInDialect { op, dialect } => {
            assert!(op.contains("trigram"), "op label: {op}");
            assert_eq!(dialect, "sqlite");
        }
        other => panic!("expected OpNotSupportedInDialect, got: {other:?}"),
    }
}

// ---------- Django-shape lookup parser ----------

#[test]
fn trigram_similar_lookup_via_filter_string_parser() {
    let q = User::objects()
        .filter("name__trigram_similar", "Rusty")
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(r#""name" % $1"#),
        "filter(\"name__trigram_similar\", ...) routes to Op::TrigramSimilar: {}",
        stmt.sql
    );
}

#[test]
fn trigram_word_similar_lookup_via_filter_string_parser() {
    let q = User::objects()
        .filter("name__trigram_word_similar", "Rust")
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(r#""name" %> $1"#),
        "filter(\"name__trigram_word_similar\", ...) routes to Op::TrigramWordSimilar: {}",
        stmt.sql
    );
}

#[test]
fn trigram_lookup_with_non_string_value_rejects_at_compile() {
    use rustango::core::SqlValue;
    let r = User::objects()
        .filter("name__trigram_similar", SqlValue::I64(42))
        .compile();
    assert!(
        matches!(
            r,
            Err(rustango::core::QueryError::InvalidLookupValue { ref suffix, .. })
                if suffix == "trigram_similar"
        ),
        "non-string value to __trigram_similar surfaces InvalidLookupValue: {r:?}",
    );
}

#[test]
fn unknown_lookup_error_mentions_trigram_suffixes() {
    let r = User::objects().filter("name__nope_typo", "value").compile();
    let err = r.unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("trigram_similar"),
        "supported-lookups list should advertise trigram_similar: {msg}"
    );
    assert!(
        msg.contains("trigram_word_similar"),
        "supported-lookups list should advertise trigram_word_similar: {msg}"
    );
}

#[test]
fn trigram_binds_pattern_as_single_string_param() {
    let q = User::objects()
        .where_(User::name.trigram_similar("Rust"))
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert_eq!(stmt.params.len(), 1, "one param");
    match &stmt.params[0] {
        rustango::core::SqlValue::String(s) => assert_eq!(s, "Rust"),
        other => panic!("expected SqlValue::String, got {other:?}"),
    }
}
