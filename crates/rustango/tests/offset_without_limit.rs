//! Tri-dialect emission tests for `.offset(N)` without a paired
//! `.limit(M)`. Issue #560.
//!
//! MySQL's grammar requires a `LIMIT` whenever `OFFSET` appears —
//! `SELECT … OFFSET 10` alone raises `ERROR 1064`. The fix is to
//! emit `LIMIT 18446744073709551615` (the documented max-`u64`
//! placeholder) ahead of the OFFSET on MySQL. PG + SQLite accept
//! the bare OFFSET and the writer leaves the LIMIT off.

use rustango::sql::{Dialect, MySql, Postgres, Sqlite};
use rustango::Model;

#[derive(Model)]
#[rustango(table = "owl_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 200)]
    title: String,
}

#[test]
fn postgres_offset_without_limit_emits_bare_offset() {
    let stmt = Postgres
        .compile_select(&Post::objects().offset(10).compile().unwrap())
        .unwrap();
    assert!(
        stmt.sql.contains("OFFSET 10"),
        "missing OFFSET 10: {}",
        stmt.sql
    );
    assert!(
        !stmt.sql.contains("LIMIT "),
        "PG should NOT add a placeholder LIMIT: {}",
        stmt.sql
    );
}

#[test]
fn sqlite_offset_without_limit_emits_bare_offset() {
    let stmt = Sqlite
        .compile_select(&Post::objects().offset(7).compile().unwrap())
        .unwrap();
    assert!(stmt.sql.contains("OFFSET 7"), "got: {}", stmt.sql);
    assert!(
        !stmt.sql.contains("LIMIT "),
        "SQLite should NOT add a placeholder LIMIT: {}",
        stmt.sql
    );
}

#[test]
fn mysql_offset_without_limit_emits_placeholder_limit() {
    let stmt = MySql
        .compile_select(&Post::objects().offset(5).compile().unwrap())
        .unwrap();
    // The placeholder LIMIT precedes the OFFSET — together MySQL
    // parses them as a valid pagination clause.
    assert!(
        stmt.sql.contains("LIMIT 18446744073709551615 OFFSET 5"),
        "MySQL must emit placeholder LIMIT for bare OFFSET; got: {}",
        stmt.sql
    );
}

#[test]
fn mysql_offset_with_explicit_limit_still_works_normally() {
    // Make sure the fix doesn't double-emit when LIMIT is also set.
    let stmt = MySql
        .compile_select(&Post::objects().limit(20).offset(5).compile().unwrap())
        .unwrap();
    assert!(
        stmt.sql.contains("LIMIT 20 OFFSET 5"),
        "explicit LIMIT must be respected; got: {}",
        stmt.sql
    );
    assert!(
        !stmt.sql.contains("18446744073709551615"),
        "no placeholder when LIMIT is explicit; got: {}",
        stmt.sql
    );
}

#[test]
fn limit_only_unaffected_on_every_dialect() {
    for (name, sql) in [
        (
            "pg",
            Postgres
                .compile_select(&Post::objects().limit(10).compile().unwrap())
                .unwrap()
                .sql,
        ),
        (
            "sqlite",
            Sqlite
                .compile_select(&Post::objects().limit(10).compile().unwrap())
                .unwrap()
                .sql,
        ),
        (
            "mysql",
            MySql
                .compile_select(&Post::objects().limit(10).compile().unwrap())
                .unwrap()
                .sql,
        ),
    ] {
        assert!(sql.contains("LIMIT 10"), "[{name}] LIMIT 10 missing: {sql}");
        assert!(
            !sql.contains("OFFSET "),
            "[{name}] OFFSET shouldn't appear when only LIMIT is set: {sql}"
        );
    }
}
