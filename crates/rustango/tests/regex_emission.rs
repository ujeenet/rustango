//! Tri-dialect emission tests for `__regex` / `__iregex` lookups
//! (issue #26). Django parity for `Q(name__regex='^foo.*')` /
//! `Q(name__iregex=...)` plus the negated forms. PG uses native
//! `~` / `~*` / `!~` / `!~*` POSIX operators; MySQL uses `REGEXP`
//! (with LOWER fallback for case-insensitive); SQLite uses
//! `REGEXP` (delegating to the loaded `regexp` user-function),
//! same LOWER fallback for case-insensitive.

use rustango::core::Column as _;
#[cfg(feature = "mysql")]
use rustango::sql::MySql;
#[cfg(feature = "sqlite")]
use rustango::sql::Sqlite;
use rustango::sql::{Dialect, Postgres};
use rustango::Model;

#[derive(Model)]
#[rustango(table = "rx_user")]
#[allow(dead_code)]
pub struct User {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 64)]
    name: String,
}

// ---------- PG: native POSIX operators ----------

#[test]
fn regex_emits_tilde_on_pg() {
    let q = User::objects()
        .where_(User::name.regex("^al.*"))
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(r#""name" ~ $1"#),
        "PG regex: {}",
        stmt.sql
    );
    assert!(!stmt.sql.contains("~*"), "case-sensitive: {}", stmt.sql);
}

#[test]
fn not_regex_emits_bang_tilde_on_pg() {
    let q = User::objects()
        .where_(User::name.not_regex("^bad"))
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(stmt.sql.contains(r#""name" !~ $1"#), "PG !~: {}", stmt.sql);
}

#[test]
fn iregex_emits_tilde_star_on_pg() {
    let q = User::objects()
        .where_(User::name.iregex("^[A-Z][a-z]+"))
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(r#""name" ~* $1"#),
        "PG iregex: {}",
        stmt.sql
    );
}

#[test]
fn not_iregex_emits_bang_tilde_star_on_pg() {
    let q = User::objects()
        .where_(User::name.not_iregex("admin"))
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(r#""name" !~* $1"#),
        "PG !~*: {}",
        stmt.sql
    );
}

// ---------- MySQL: REGEXP keyword + LOWER fallback ----------

#[cfg(feature = "mysql")]
#[test]
fn regex_emits_regexp_keyword_on_mysql() {
    let q = User::objects()
        .where_(User::name.regex("^al.*"))
        .compile()
        .unwrap();
    let stmt = MySql.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains("`name` REGEXP ?"),
        "MySQL REGEXP: {}",
        stmt.sql
    );
    assert!(
        !stmt.sql.contains("LOWER"),
        "case-sensitive on MySQL — no LOWER wrap: {}",
        stmt.sql
    );
}

#[cfg(feature = "mysql")]
#[test]
fn not_regex_emits_not_regexp_keyword_on_mysql() {
    let q = User::objects()
        .where_(User::name.not_regex("^x"))
        .compile()
        .unwrap();
    let stmt = MySql.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains("`name` NOT REGEXP ?"),
        "MySQL NOT REGEXP: {}",
        stmt.sql
    );
}

#[cfg(feature = "mysql")]
#[test]
fn iregex_wraps_lower_on_mysql() {
    let q = User::objects()
        .where_(User::name.iregex("admin"))
        .compile()
        .unwrap();
    let stmt = MySql.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains("LOWER(`name`) REGEXP LOWER(?)"),
        "MySQL iregex LOWER fallback: {}",
        stmt.sql
    );
}

#[cfg(feature = "mysql")]
#[test]
fn not_iregex_wraps_lower_on_mysql() {
    let q = User::objects()
        .where_(User::name.not_iregex("admin"))
        .compile()
        .unwrap();
    let stmt = MySql.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains("LOWER(`name`) NOT REGEXP LOWER(?)"),
        "MySQL NOT iregex LOWER fallback: {}",
        stmt.sql
    );
}

// ---------- SQLite: REGEXP user-function + LOWER fallback ----------

#[cfg(feature = "sqlite")]
#[test]
fn regex_emits_regexp_keyword_on_sqlite() {
    let q = User::objects()
        .where_(User::name.regex("^al.*"))
        .compile()
        .unwrap();
    let stmt = Sqlite.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(r#""name" REGEXP ?"#),
        "SQLite REGEXP: {}",
        stmt.sql
    );
}

#[cfg(feature = "sqlite")]
#[test]
fn iregex_wraps_lower_on_sqlite() {
    let q = User::objects()
        .where_(User::name.iregex("admin"))
        .compile()
        .unwrap();
    let stmt = Sqlite.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"LOWER("name") REGEXP LOWER(?)"#),
        "SQLite iregex LOWER fallback: {}",
        stmt.sql
    );
}

// ---------- Django-shape lookup-suffix parser ----------

#[test]
fn regex_lookup_via_filter_string_parser() {
    // `.filter("name__regex", pattern)` — Django's string-keyed form
    // (issue #71 parser). Should route to Op::Regex.
    let q = User::objects()
        .filter("name__regex", "^foo.*")
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(r#""name" ~ $1"#),
        "filter(\"name__regex\", pattern) routes to Op::Regex: {}",
        stmt.sql
    );
}

#[test]
fn iregex_lookup_via_filter_string_parser() {
    let q = User::objects()
        .filter("name__iregex", "ADMIN")
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(r#""name" ~* $1"#),
        "filter(\"name__iregex\", pattern) routes to Op::IRegex: {}",
        stmt.sql
    );
}

#[test]
fn regex_with_non_string_value_rejects_at_compile() {
    use rustango::core::SqlValue;
    let r = User::objects()
        .filter("name__regex", SqlValue::I64(42))
        .compile();
    assert!(
        matches!(
            r,
            Err(rustango::core::QueryError::InvalidLookupValue { ref suffix, .. })
                if suffix == "regex"
        ),
        "non-string value to __regex surfaces InvalidLookupValue: {r:?}",
    );
}

// ---------- Param binding sanity ----------

#[test]
fn regex_binds_pattern_as_single_string_param() {
    let q = User::objects()
        .where_(User::name.regex("^test$"))
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert_eq!(stmt.params.len(), 1, "one param");
    match &stmt.params[0] {
        rustango::core::SqlValue::String(s) => assert_eq!(s, "^test$"),
        other => panic!("expected SqlValue::String, got {other:?}"),
    }
}
