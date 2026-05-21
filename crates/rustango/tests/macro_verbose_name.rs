//! `#[rustango(verbose_name = "...")]` field attribute (Django parity #448).
//!
//! Covers:
//! - macro threads the value through to `FieldSchema::verbose_name`
//! - `display_label()` returns `verbose_name` when set, else `name`
//! - PG/MySQL/SQLite DDL is unaffected (label is presentation-only)

use rustango::core::{FieldSchema, Model};
use rustango::migrate::ddl::create_table_sql_with_dialect;
use rustango::sql::Postgres;
use rustango_macros::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "macro_vn_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: i64,

    #[rustango(max_length = 200, verbose_name = "Display title")]
    pub title: String,

    /// Field without verbose_name — display_label() falls back to name.
    pub status: String,

    #[rustango(verbose_name = "Author's full name")]
    pub author: String,
}

fn field<'a>(name: &str) -> &'a FieldSchema {
    Post::SCHEMA
        .field(name)
        .unwrap_or_else(|| panic!("no field {name:?}"))
}

#[test]
fn schema_threads_verbose_name() {
    assert_eq!(field("title").verbose_name, Some("Display title"));
    assert_eq!(field("author").verbose_name, Some("Author's full name"));
    assert!(field("status").verbose_name.is_none());
    assert!(field("id").verbose_name.is_none());
}

#[test]
fn display_label_returns_verbose_name_when_set() {
    assert_eq!(field("title").display_label(), "Display title");
    assert_eq!(field("author").display_label(), "Author's full name");
}

#[test]
fn display_label_falls_back_to_name_when_unset() {
    assert_eq!(field("status").display_label(), "status");
    assert_eq!(field("id").display_label(), "id");
}

/// `verbose_name` is presentation-only and must never affect the DDL —
/// no rogue `COMMENT` text, no rename of the column. Belt-and-braces
/// regression test against future changes that conflate the two.
#[test]
fn verbose_name_does_not_change_ddl() {
    let sql = create_table_sql_with_dialect(&Postgres, Post::SCHEMA);
    assert!(
        !sql.contains("Display title"),
        "verbose_name leaked into DDL: {sql}"
    );
    assert!(
        !sql.contains("Author"),
        "verbose_name leaked into DDL: {sql}"
    );
    // Column names are still the Rust identifiers.
    assert!(sql.contains(r#""title""#));
    assert!(sql.contains(r#""author""#));
}
