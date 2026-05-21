//! `#[rustango(editable = false)]` field attribute (Django parity #449).
//!
//! Covers:
//! - macro threads the value through to `FieldSchema::editable`
//! - default is `true` for fields without the attribute
//! - `editable = true` explicit-form parses
//! - DDL is unaffected (presentation-only attribute)
//!
//! The admin-form skip behavior (the actual visible() filter at
//! `admin/helpers.rs`) is exercised by the broader admin test suite —
//! this file scopes to the macro + schema layer so it compiles under
//! any backend.

use rustango::core::{FieldSchema, Model};
use rustango::migrate::ddl::create_table_sql_with_dialect;
use rustango::sql::Postgres;
use rustango_macros::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "macro_edit_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: i64,

    /// Default — no `editable` attribute → field is editable.
    #[rustango(max_length = 200)]
    pub title: String,

    /// Explicit `editable = false` — admin change-form skips this.
    #[rustango(editable = false)]
    pub computed_score: i32,

    /// Explicit `editable = true` — same as the default; parser accepts.
    #[rustango(editable = true)]
    pub author: String,
}

fn field<'a>(name: &str) -> &'a FieldSchema {
    Post::SCHEMA
        .field(name)
        .unwrap_or_else(|| panic!("no field {name:?}"))
}

#[test]
fn schema_threads_editable_default_true() {
    assert!(field("id").editable, "PK defaults to editable");
    assert!(field("title").editable, "no attr → editable");
    assert!(field("author").editable, "explicit true → editable");
}

#[test]
fn schema_threads_editable_false() {
    assert!(
        !field("computed_score").editable,
        "explicit `editable = false` should set the field to false"
    );
}

/// `editable` is presentation-only and must never affect the DDL —
/// no column rename, no rogue text. Belt-and-braces regression test
/// against future changes that conflate the two.
#[test]
fn editable_does_not_change_ddl() {
    let sql = create_table_sql_with_dialect(&Postgres, Post::SCHEMA);
    // The non-editable column is still in CREATE TABLE — `editable`
    // is a form-layer flag, not a schema-layer one.
    assert!(
        sql.contains(r#""computed_score""#),
        "computed_score should still appear in DDL, got: {sql}"
    );
    // No "editable" keyword leaks anywhere.
    assert!(
        !sql.to_lowercase().contains("editable"),
        "editable keyword leaked into DDL: {sql}"
    );
}
