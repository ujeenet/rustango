//! Unit-style tests for `#[derive(Form)]`.
//!
//! No DB required — tests macro codegen + multi-error `FormErrors` collection.

use std::collections::HashMap;

use rustango::forms::Form;
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
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

#[test]
fn parses_minimal_payload() {
    let form = payload(&[("name", "alice"), ("age", "30")]);
    let parsed = CreateItemForm::parse(&form).unwrap();
    assert_eq!(parsed, CreateItemForm {
        name: "alice".into(),
        age: 30,
        active: false,
        email: None,
    });
}

#[test]
fn parses_full_payload_with_checkbox_and_optional() {
    let form = payload(&[("name", "bob"), ("age", "42"), ("active", "on"), ("email", "bob@example.com")]);
    let parsed = CreateItemForm::parse(&form).unwrap();
    assert!(parsed.active);
    assert_eq!(parsed.email.as_deref(), Some("bob@example.com"));
}

#[test]
fn empty_optional_string_becomes_none() {
    let form = payload(&[("name", "carol"), ("age", "25"), ("email", "")]);
    let parsed = CreateItemForm::parse(&form).unwrap();
    assert_eq!(parsed.email, None);
}

#[test]
fn missing_required_field_collects_error() {
    let form = payload(&[("age", "10")]);
    let errors = CreateItemForm::parse(&form).unwrap_err();
    let msgs = errors.get("name");
    assert!(!msgs.is_empty(), "expected error for 'name', got none");
    assert!(msgs[0].contains("required"), "unexpected message: {}", msgs[0]);
}

#[test]
fn unparseable_int_collects_error() {
    let form = payload(&[("name", "dave"), ("age", "twelve")]);
    let errors = CreateItemForm::parse(&form).unwrap_err();
    let msgs = errors.get("age");
    assert!(!msgs.is_empty(), "expected error for 'age'");
    assert!(msgs[0].to_lowercase().contains("i32") || msgs[0].to_lowercase().contains("valid"),
        "unexpected message: {}", msgs[0]);
}

#[test]
fn multiple_field_errors_collected() {
    // Both name (missing) and age (bad type) fail — both should be in errors.
    let form = payload(&[("age", "not-a-number")]);
    let errors = CreateItemForm::parse(&form).unwrap_err();
    assert!(!errors.get("name").is_empty(), "expected name error");
    assert!(!errors.get("age").is_empty(), "expected age error");
}

#[test]
fn max_length_validator_fires() {
    let long = "x".repeat(100);
    let form = payload(&[("name", long.as_str()), ("age", "10")]);
    let errors = CreateItemForm::parse(&form).unwrap_err();
    let msgs = errors.get("name");
    assert!(!msgs.is_empty(), "expected name error");
    assert!(msgs[0].contains("100") || msgs[0].to_lowercase().contains("most"),
        "unexpected message: {}", msgs[0]);
}

#[test]
fn max_int_validator_fires() {
    let form = payload(&[("name", "x"), ("age", "999")]);
    let errors = CreateItemForm::parse(&form).unwrap_err();
    let msgs = errors.get("age");
    assert!(!msgs.is_empty(), "expected age error");
    assert!(msgs[0].to_lowercase().contains("150") || msgs[0].to_lowercase().contains("less"),
        "unexpected message: {}", msgs[0]);
}

#[test]
fn checkbox_falsy_aliases_recognized() {
    for falsy in ["false", "0", "off", "no", "FALSE"] {
        let form = payload(&[("name", "x"), ("age", "1"), ("active", falsy)]);
        let parsed = CreateItemForm::parse(&form).unwrap();
        assert!(!parsed.active, "expected `{falsy}` to parse as false");
    }
}
