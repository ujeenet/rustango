//! `#[rustango(validators = "...")]` model-side validators
//! (Django-parity #447).
//!
//! Covers:
//! - macro threads comma-separated names onto `FieldSchema::validators`
//! - `validate_value` accepts in-spec values
//! - `validate_value` rejects off-spec values with
//!   `QueryError::ValidatorFailed` carrying the validator name + reason
//! - unknown validator names surface `QueryError::UnknownValidator`
//! - multiple validators chain — first failure wins
//! - `validators` is presentation/runtime-only — no DDL impact

use rustango::core::{validate_value, FieldSchema, Model, QueryError, SqlValue};
use rustango::migrate::ddl::create_table_sql_with_dialect;
use rustango::sql::Postgres;
use rustango_macros::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "macro_vchain_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: i64,

    /// Single validator.
    #[rustango(max_length = 200, validators = "email")]
    pub author_email: String,

    /// Two validators — both must pass.
    #[rustango(max_length = 200, validators = "url, no_null")]
    pub homepage: String,

    /// No validators declared.
    #[rustango(max_length = 64)]
    pub title: String,

    /// Bad name — caught at runtime when validate_value fires.
    #[rustango(max_length = 64, validators = "this_does_not_exist")]
    pub bad: String,
}

fn field<'a>(name: &str) -> &'a FieldSchema {
    Post::SCHEMA
        .field(name)
        .unwrap_or_else(|| panic!("no field {name:?}"))
}

#[test]
fn schema_threads_comma_separated_validator_names() {
    assert_eq!(field("author_email").validators, &["email"]);
    assert_eq!(field("homepage").validators, &["url", "no_null"]);
    assert!(field("title").validators.is_empty());
    assert!(field("id").validators.is_empty());
}

#[test]
fn validate_value_accepts_in_spec_value() {
    validate_value(
        "Post",
        field("author_email"),
        &SqlValue::String("alice@example.com".into()),
    )
    .unwrap();
}

#[test]
fn validate_value_rejects_off_spec_email() {
    let err = validate_value(
        "Post",
        field("author_email"),
        &SqlValue::String("not-an-email".into()),
    )
    .unwrap_err();
    match err {
        QueryError::ValidatorFailed {
            field: name,
            validator,
            ..
        } => {
            assert_eq!(name, "author_email");
            assert_eq!(validator, "email");
        }
        other => panic!("expected ValidatorFailed, got {other:?}"),
    }
}

#[test]
fn validate_value_chains_multiple_validators_first_failure_wins() {
    // `homepage` = url + no_null. Non-URL value → url validator fails first.
    let err =
        validate_value("Post", field("homepage"), &SqlValue::String("nope".into())).unwrap_err();
    match err {
        QueryError::ValidatorFailed { validator, .. } => assert_eq!(validator, "url"),
        other => panic!("expected ValidatorFailed, got {other:?}"),
    }
}

#[test]
fn validate_value_passes_when_all_validators_pass() {
    validate_value(
        "Post",
        field("homepage"),
        &SqlValue::String("https://example.com/page".into()),
    )
    .unwrap();
}

#[test]
fn unknown_validator_name_surfaces_clear_error() {
    let err = validate_value("Post", field("bad"), &SqlValue::String("x".into())).unwrap_err();
    match err {
        QueryError::UnknownValidator {
            field: name,
            validator,
            ..
        } => {
            assert_eq!(name, "bad");
            assert_eq!(validator, "this_does_not_exist");
        }
        other => panic!("expected UnknownValidator, got {other:?}"),
    }
}

#[test]
fn validators_do_not_affect_ddl() {
    let sql = create_table_sql_with_dialect(&Postgres, Post::SCHEMA);
    assert!(
        !sql.contains("validators"),
        "validators leaked into DDL: {sql}"
    );
    assert!(
        !sql.to_lowercase().contains("validator"),
        "validator string leaked into DDL: {sql}"
    );
}
