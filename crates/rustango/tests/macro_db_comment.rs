//! `#[rustango(db_comment = "...")]` field attribute (Django parity #450).
//!
//! Covers:
//! - macro threads the value through to `FieldSchema::db_comment`
//! - DDL writer emits inline `COMMENT '...'` on MySQL
//! - DDL writer emits post-table `COMMENT ON COLUMN ... IS '...'` on Postgres
//! - DDL writer emits nothing on SQLite (no native column comments)
//! - single quotes in the comment value are escaped per dialect
//!
//! No DB roundtrip — these are pure schema / writer checks against the
//! macro output, so the test compiles under any backend.

use rustango::core::{FieldSchema, Model};
use rustango::migrate::ddl::{
    column_comment_statements_with_dialect, create_table_sql_with_dialect,
};
#[cfg(feature = "mysql")]
use rustango::sql::MySql;
use rustango::sql::Postgres;
#[cfg(feature = "sqlite")]
use rustango::sql::Sqlite;
use rustango_macros::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "macro_dbnote_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: i64,

    #[rustango(max_length = 200, db_comment = "Display title, plain text")]
    pub title: String,

    /// Comment with an embedded apostrophe — round-trips per-dialect escape.
    #[rustango(db_comment = "Author's name (full)")]
    pub author: String,

    pub untagged: i32,
}

fn field<'a>(name: &str) -> &'a FieldSchema {
    Post::SCHEMA
        .field(name)
        .unwrap_or_else(|| panic!("no field {name:?}"))
}

#[test]
fn schema_threads_db_comment() {
    assert_eq!(field("title").db_comment, Some("Display title, plain text"));
    assert_eq!(field("author").db_comment, Some("Author's name (full)"));
    assert!(field("id").db_comment.is_none());
    assert!(field("untagged").db_comment.is_none());
}

#[cfg(feature = "mysql")]
#[test]
fn mysql_inlines_comment_in_create_table() {
    let sql = create_table_sql_with_dialect(&MySql, Post::SCHEMA);
    // Inline on the title column
    assert!(
        sql.contains("COMMENT 'Display title, plain text'"),
        "expected inline COMMENT on title, got:\n{sql}"
    );
    // Escaped single quote on the author column
    assert!(
        sql.contains("COMMENT 'Author''s name (full)'"),
        "expected escaped apostrophe on author, got:\n{sql}"
    );
    // No spurious comment on untagged
    let no_comment_count = sql.matches("COMMENT '").count();
    assert_eq!(
        no_comment_count, 2,
        "expected exactly 2 inline COMMENTs, got {no_comment_count} in:\n{sql}"
    );
    // MySQL doesn't get any post-table COMMENT ON statements
    assert!(column_comment_statements_with_dialect(&MySql, Post::SCHEMA).is_empty());
}

#[test]
fn postgres_emits_post_table_comment_on_column() {
    let sql = create_table_sql_with_dialect(&Postgres, Post::SCHEMA);
    // No inline COMMENT on PG — they come as separate statements.
    assert!(
        !sql.contains("COMMENT '"),
        "PG CREATE TABLE should not inline comments, got:\n{sql}"
    );

    let stmts = column_comment_statements_with_dialect(&Postgres, Post::SCHEMA);
    assert_eq!(
        stmts.len(),
        2,
        "expected 2 COMMENT ON statements, got {stmts:?}"
    );
    let joined = stmts.join("\n");
    assert!(joined.contains(
        r#"COMMENT ON COLUMN "macro_dbnote_post"."title" IS 'Display title, plain text'"#
    ));
    // PG also doubles the embedded apostrophe
    assert!(joined
        .contains(r#"COMMENT ON COLUMN "macro_dbnote_post"."author" IS 'Author''s name (full)'"#));
}

#[cfg(feature = "sqlite")]
#[test]
fn sqlite_silently_drops_db_comment() {
    let sql = create_table_sql_with_dialect(&Sqlite, Post::SCHEMA);
    // SQLite emits no comment text — DDL stays clean.
    assert!(
        !sql.to_uppercase().contains("COMMENT"),
        "SQLite should not emit COMMENT, got:\n{sql}"
    );
    assert!(column_comment_statements_with_dialect(&Sqlite, Post::SCHEMA).is_empty());
}
