//! Django parity — `Meta.db_table_comment` (Django 4.2+). Verifies the
//! macro parses the attribute, threads it onto `ModelSchema`, and the
//! migration DDL writer emits the right shape per dialect.

#![cfg(feature = "sqlite")]

use rustango::core::Model as _;
use rustango::migrate::ddl::{
    create_table_sql_with_dialect, table_comment_statements_with_dialect,
};
use rustango::sql::{Dialect as _, MySql, Postgres, Sqlite};
use rustango::Model;

#[derive(Model)]
#[rustango(
    table = "dtc_post",
    db_table_comment = "Blog posts — public CMS content."
)]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    id: i64,
    title: String,
}

#[derive(Model)]
#[rustango(table = "dtc_plain")]
#[allow(dead_code)]
pub struct Plain {
    #[rustango(primary_key)]
    id: i64,
}

#[test]
fn macro_threads_comment_onto_model_schema() {
    let schema = <Post as rustango::core::Model>::SCHEMA;
    assert_eq!(
        schema.db_table_comment,
        Some("Blog posts — public CMS content.")
    );
    let plain = <Plain as rustango::core::Model>::SCHEMA;
    assert_eq!(plain.db_table_comment, None);
}

#[test]
fn postgres_emits_post_hoc_comment_on_table() {
    let schema = <Post as rustango::core::Model>::SCHEMA;
    // CREATE TABLE on PG has no inline comment — the trailer is empty.
    let create = create_table_sql_with_dialect(&Postgres, schema);
    assert!(
        !create.contains("COMMENT"),
        "PG CREATE TABLE must not inline the comment; got: {create}"
    );
    // The post-hoc COMMENT ON TABLE statement is the single entry.
    let stmts = table_comment_statements_with_dialect(&Postgres, schema);
    assert_eq!(stmts.len(), 1, "PG should emit one COMMENT ON TABLE");
    assert_eq!(
        stmts[0],
        "COMMENT ON TABLE \"dtc_post\" IS 'Blog posts — public CMS content.'"
    );
}

#[test]
fn mysql_emits_inline_table_comment_trailer() {
    let schema = <Post as rustango::core::Model>::SCHEMA;
    let create = create_table_sql_with_dialect(&MySql, schema);
    assert!(
        create.ends_with(" COMMENT='Blog posts — public CMS content.'"),
        "MySQL must inline COMMENT=… trailer; got: {create}"
    );
    // No post-hoc statement on MySQL.
    let stmts = table_comment_statements_with_dialect(&MySql, schema);
    assert!(stmts.is_empty(), "MySQL inlines, post-hoc is empty");
}

#[test]
fn sqlite_silently_drops_table_comment() {
    let schema = <Post as rustango::core::Model>::SCHEMA;
    // SQLite has no native table comments — CREATE TABLE stays clean.
    let create = create_table_sql_with_dialect(&Sqlite, schema);
    assert!(
        !create.contains("COMMENT"),
        "SQLite CREATE TABLE must not include COMMENT; got: {create}"
    );
    let stmts = table_comment_statements_with_dialect(&Sqlite, schema);
    assert!(stmts.is_empty(), "SQLite emits no post-hoc statement");
}

#[test]
fn models_without_db_table_comment_emit_nothing_anywhere() {
    let plain = <Plain as rustango::core::Model>::SCHEMA;
    for dialect in [&Postgres as &dyn rustango::sql::Dialect, &MySql, &Sqlite] {
        let create = create_table_sql_with_dialect(dialect, plain);
        assert!(
            !create.contains("COMMENT"),
            "[{}] no-comment model must not emit COMMENT: {create}",
            dialect.name()
        );
        let stmts = table_comment_statements_with_dialect(dialect, plain);
        assert!(stmts.is_empty(), "[{}] no post-hoc", dialect.name());
    }
}

#[test]
fn pg_escapes_single_quotes_in_comment() {
    // Synthesize a model with a quote-containing comment through the
    // dialect APIs directly — the macro layer doesn't need its own
    // escape since the SQL emitters do the work.
    let stmt = Postgres
        .table_comment_statement("t", "it's tricky")
        .expect("PG emits a stmt");
    assert!(
        stmt.contains("'it''s tricky'"),
        "single quotes must be doubled: {stmt}"
    );
    let stmt = MySql
        .write_inline_table_comment("it's tricky")
        .expect("MySQL emits inline");
    assert_eq!(stmt, " COMMENT='it''s tricky'");
}
