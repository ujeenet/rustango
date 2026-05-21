//! `#[rustango(blank)]` field attribute (Django parity #445).
//!
//! Covers:
//! - macro threads the value through to `FieldSchema::blank`
//! - flag form (`#[rustango(blank)]`) parses as `true`
//! - explicit form (`#[rustango(blank = true / false)]`) parses verbatim
//! - default is `false` for un-attributed fields
//! - DDL stays unchanged (form-layer flag, not schema-layer)

use rustango::core::{FieldSchema, Model};
use rustango::migrate::ddl::create_table_sql_with_dialect;
use rustango::sql::Postgres;
use rustango_macros::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "macro_empty_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: i64,

    /// Default — no `blank` attribute → form treats as required.
    #[rustango(max_length = 200)]
    pub title: String,

    /// Flag form sets `blank = true`.
    #[rustango(blank)]
    pub subtitle: String,

    /// Explicit-true form.
    #[rustango(blank = true)]
    pub summary: String,

    /// Explicit-false: same as default.
    #[rustango(blank = false)]
    pub author: String,
}

fn field<'a>(name: &str) -> &'a FieldSchema {
    Post::SCHEMA
        .field(name)
        .unwrap_or_else(|| panic!("no field {name:?}"))
}

#[test]
fn schema_threads_blank_default_false() {
    assert!(!field("id").blank, "PK defaults to blank=false");
    assert!(!field("title").blank, "no attr → blank=false");
    assert!(!field("author").blank, "explicit false → blank=false");
}

#[test]
fn schema_threads_blank_flag_form() {
    assert!(field("subtitle").blank, "flag form should set blank=true");
}

#[test]
fn schema_threads_blank_explicit_true() {
    assert!(
        field("summary").blank,
        "explicit true should set blank=true"
    );
}

/// `blank` is a form-layer flag — must never affect the DDL. Column
/// stays in CREATE TABLE with the same NOT NULL constraint regardless
/// of the attribute.
#[test]
fn blank_does_not_change_ddl() {
    let sql = create_table_sql_with_dialect(&Postgres, Post::SCHEMA);
    // subtitle is NOT NULL despite `blank = true` (blank is form-only)
    assert!(
        sql.contains(r#""subtitle" TEXT NOT NULL"#),
        "subtitle should still be NOT NULL in DDL, got: {sql}"
    );
    assert!(
        sql.contains(r#""summary" TEXT NOT NULL"#),
        "summary should still be NOT NULL in DDL, got: {sql}"
    );
    // No "blank" keyword leaks anywhere.
    assert!(
        !sql.to_lowercase().contains("blank"),
        "blank keyword leaked into DDL: {sql}"
    );
}
