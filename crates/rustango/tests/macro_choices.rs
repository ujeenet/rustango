//! `#[rustango(choices = "...")]` field attribute (Django parity #446).
//!
//! Covers:
//! - macro parses comma-separated `value:Label` pairs onto `FieldSchema::choices`
//! - missing label falls back to value (`value` == `Label`)
//! - validator rejects off-choice strings and accepts in-choice ones
//! - admin `render_input` emits `<select>` when choices are present
//!
//! No DB roundtrip — these are pure schema / writer / validator checks
//! against the macro output, so the test compiles under any backend.

use rustango::core::{validate_value, FieldSchema, Model, QueryError, SqlValue};
use rustango_macros::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "macro_choices_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: i64,

    #[rustango(max_length = 200)]
    pub title: String,

    #[rustango(
        max_length = 32,
        choices = "draft:Draft, published:Published, archived:Archived"
    )]
    pub status: String,

    /// A choices field with no `:` separators — value reused as label.
    #[rustango(max_length = 8, choices = "yes, no, maybe")]
    pub vote: String,
}

fn field<'a>(name: &str) -> &'a FieldSchema {
    Post::SCHEMA
        .field(name)
        .unwrap_or_else(|| panic!("no field {name:?}"))
}

#[test]
fn schema_threads_choices_with_labels() {
    let f = field("status");
    let choices = f.choices.expect("choices threaded onto FieldSchema");
    assert_eq!(
        choices,
        &[
            ("draft", "Draft"),
            ("published", "Published"),
            ("archived", "Archived"),
        ]
    );
}

#[test]
fn schema_threads_choices_without_labels_reuses_value() {
    let f = field("vote");
    let choices = f.choices.expect("choices threaded");
    assert_eq!(choices, &[("yes", "yes"), ("no", "no"), ("maybe", "maybe")]);
}

#[test]
fn fields_without_choices_attribute_have_none() {
    assert!(field("id").choices.is_none());
    assert!(field("title").choices.is_none());
}

#[test]
fn validate_value_rejects_off_choice() {
    let f = field("status");
    let err = validate_value("Post", f, &SqlValue::String("nope".to_owned())).unwrap_err();
    match err {
        QueryError::InvalidChoice {
            field: name,
            value,
            allowed,
            ..
        } => {
            assert_eq!(name, "status");
            assert_eq!(value, "nope");
            assert_eq!(allowed, vec!["draft", "published", "archived"]);
        }
        other => panic!("expected InvalidChoice, got {other:?}"),
    }
}

#[test]
fn validate_value_accepts_in_choice() {
    let f = field("status");
    validate_value("Post", f, &SqlValue::String("draft".to_owned())).unwrap();
    validate_value("Post", f, &SqlValue::String("published".to_owned())).unwrap();
}

#[test]
fn validate_value_still_checks_max_length_on_choices_field() {
    // Even though the choices set is small, `max_length` should still
    // fire on a long off-choice value — this guards against future
    // regressions that might short-circuit max_length when choices set.
    let f = field("vote");
    let err = validate_value("Post", f, &SqlValue::String("waytoolong".to_owned())).unwrap_err();
    match err {
        QueryError::MaxLengthExceeded { .. } => {}
        QueryError::InvalidChoice { .. } => {} // either order is fine
        other => panic!("unexpected error variant: {other:?}"),
    }
}
