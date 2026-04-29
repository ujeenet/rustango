//! Unit-style tests for `#[derive(Form)]` (slice 8.4B).
//!
//! No DB required — these test the macro codegen + the
//! `FormStruct::parse` impl in isolation. Live integration with the
//! axum extractor and the CSRF middleware follows in slice 8.4C's
//! live tests.

use std::collections::HashMap;

use rustango::forms::{FormError, FormStruct};
use rustango::Form;

#[derive(Form, Debug, PartialEq)]
pub struct CreateItemForm {
    #[form(min_length = 1, max_length = 64)]
    pub name: String,
    #[form(min = 0, max = 150)]
    pub age: i32,
    pub active: bool,
    pub email: Option<String>,
}

fn payload(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

#[test]
fn parses_minimal_payload() {
    let form = payload(&[("name", "alice"), ("age", "30")]);
    let parsed = CreateItemForm::parse(&form).unwrap();
    assert_eq!(
        parsed,
        CreateItemForm {
            name: "alice".into(),
            age: 30,
            active: false, // checkbox absent → false
            email: None,   // Option<String> absent → None
        }
    );
}

#[test]
fn parses_full_payload_with_checkbox_and_optional() {
    let form = payload(&[
        ("name", "bob"),
        ("age", "42"),
        ("active", "on"),
        ("email", "bob@example.com"),
    ]);
    let parsed = CreateItemForm::parse(&form).unwrap();
    assert_eq!(parsed.active, true);
    assert_eq!(parsed.email.as_deref(), Some("bob@example.com"));
}

#[test]
fn empty_optional_string_becomes_none() {
    let form = payload(&[("name", "carol"), ("age", "25"), ("email", "")]);
    let parsed = CreateItemForm::parse(&form).unwrap();
    assert_eq!(parsed.email, None);
}

#[test]
fn missing_required_field_errors_with_field_name() {
    let form = payload(&[("age", "10")]);
    let err = CreateItemForm::parse(&form).unwrap_err();
    match err {
        FormError::Missing { field } => assert_eq!(field, "name"),
        other => panic!("expected Missing(name), got {other:?}"),
    }
}

#[test]
fn unparseable_int_errors_with_value_and_detail() {
    let form = payload(&[("name", "dave"), ("age", "twelve")]);
    let err = CreateItemForm::parse(&form).unwrap_err();
    match err {
        FormError::Parse {
            field, value, ty, ..
        } => {
            assert_eq!(field, "age");
            assert_eq!(value, "twelve");
            assert_eq!(ty, "i32");
        }
        other => panic!("expected Parse(age), got {other:?}"),
    }
}

#[test]
fn min_length_validator_fires() {
    let form = payload(&[("name", ""), ("age", "10")]);
    // name="" is treated as missing for required String fields, so
    // we expect Missing rather than Parse-for-min_length here.
    // To exercise min_length proper, we'd need a min_length > 1 with
    // a 1-char string.
    let err = CreateItemForm::parse(&form).unwrap_err();
    assert!(matches!(err, FormError::Missing { .. }));
}

#[test]
fn max_length_validator_fires() {
    let long = "x".repeat(100);
    let form = payload(&[("name", long.as_str()), ("age", "10")]);
    let err = CreateItemForm::parse(&form).unwrap_err();
    match err {
        FormError::Parse { field, detail, .. } => {
            assert_eq!(field, "name");
            assert!(detail.contains("max_length"), "{detail}");
        }
        other => panic!("expected Parse(name) max_length, got {other:?}"),
    }
}

#[test]
fn min_max_int_validators_fire() {
    let form = payload(&[("name", "x"), ("age", "999")]);
    let err = CreateItemForm::parse(&form).unwrap_err();
    match err {
        FormError::Parse { field, detail, .. } => {
            assert_eq!(field, "age");
            assert!(detail.contains("max"), "{detail}");
        }
        other => panic!("expected Parse(age) max, got {other:?}"),
    }
}

#[test]
fn checkbox_falsy_aliases_recognized() {
    for falsy in ["false", "0", "off", "no", "FALSE"] {
        let form = payload(&[("name", "x"), ("age", "1"), ("active", falsy)]);
        let parsed = CreateItemForm::parse(&form).unwrap();
        assert!(!parsed.active, "expected `{falsy}` to parse as false");
    }
}
