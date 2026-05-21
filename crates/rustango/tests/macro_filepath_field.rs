//! Django-parity #338 — `FilePathField` equivalent via
//! `#[rustango(validators = "filepath")]`. Structural-only check:
//! non-empty, no NUL, no `..` segments.

use rustango::core::{validate_value, FieldSchema, Model, QueryError, SqlValue};
use rustango_macros::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "macro_filepath_doc")]
#[allow(dead_code)]
pub struct Doc {
    #[rustango(primary_key)]
    pub id: i64,

    #[rustango(max_length = 500, validators = "filepath")]
    pub path: String,

    /// Django-shape alias for verbatim translation.
    #[rustango(max_length = 500, validators = "filepath_field")]
    pub legacy_alias: String,
}

fn field<'a>(name: &str) -> &'a FieldSchema {
    Doc::SCHEMA
        .field(name)
        .unwrap_or_else(|| panic!("no field {name:?}"))
}

#[test]
fn accepts_relative_path() {
    validate_value(
        "Doc",
        field("path"),
        &SqlValue::String("docs/intro.md".into()),
    )
    .unwrap();
}

#[test]
fn accepts_absolute_unix_path() {
    validate_value(
        "Doc",
        field("path"),
        &SqlValue::String("/var/uploads/x.txt".into()),
    )
    .unwrap();
}

#[test]
fn accepts_absolute_windows_path() {
    validate_value(
        "Doc",
        field("path"),
        &SqlValue::String(r"C:\Users\me\f.txt".into()),
    )
    .unwrap();
}

#[test]
fn rejects_empty_string() {
    let err = validate_value("Doc", field("path"), &SqlValue::String(String::new())).unwrap_err();
    match err {
        QueryError::ValidatorFailed { validator, .. } => assert_eq!(validator, "filepath"),
        other => panic!("expected ValidatorFailed, got {other:?}"),
    }
}

#[test]
fn rejects_path_traversal_relative() {
    let err = validate_value(
        "Doc",
        field("path"),
        &SqlValue::String("docs/../etc/passwd".into()),
    )
    .unwrap_err();
    assert!(matches!(err, QueryError::ValidatorFailed { .. }));
}

#[test]
fn rejects_path_traversal_at_root() {
    let err =
        validate_value("Doc", field("path"), &SqlValue::String("../secret".into())).unwrap_err();
    assert!(matches!(err, QueryError::ValidatorFailed { .. }));
}

#[test]
fn rejects_windows_style_traversal() {
    let err =
        validate_value("Doc", field("path"), &SqlValue::String(r"a\..\b".into())).unwrap_err();
    assert!(matches!(err, QueryError::ValidatorFailed { .. }));
}

#[test]
fn rejects_nul_bytes() {
    let err = validate_value(
        "Doc",
        field("path"),
        &SqlValue::String("docs/intro\0.md".into()),
    )
    .unwrap_err();
    assert!(matches!(err, QueryError::ValidatorFailed { .. }));
}

#[test]
fn alias_works_same_as_filepath() {
    // Valid path through alias.
    validate_value(
        "Doc",
        field("legacy_alias"),
        &SqlValue::String("assets/logo.png".into()),
    )
    .unwrap();
    // Same rejection.
    let err = validate_value(
        "Doc",
        field("legacy_alias"),
        &SqlValue::String("a/../b".into()),
    )
    .unwrap_err();
    match err {
        QueryError::ValidatorFailed { validator, .. } => {
            assert_eq!(validator, "filepath_field");
        }
        other => panic!("expected ValidatorFailed, got {other:?}"),
    }
}
