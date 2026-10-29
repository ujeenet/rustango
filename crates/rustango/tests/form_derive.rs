//! Unit-style tests for `#[derive(Form)]`.
//!
//! No DB required — tests macro codegen + multi-error `FormErrors` collection.

#![cfg(feature = "forms")]

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
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
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
            active: false,
            email: None,
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
    assert!(
        msgs[0].contains("required"),
        "unexpected message: {}",
        msgs[0]
    );
}

#[test]
fn unparseable_int_collects_error() {
    let form = payload(&[("name", "dave"), ("age", "twelve")]);
    let errors = CreateItemForm::parse(&form).unwrap_err();
    let msgs = errors.get("age");
    assert!(!msgs.is_empty(), "expected error for 'age'");
    assert!(
        msgs[0].to_lowercase().contains("i32") || msgs[0].to_lowercase().contains("valid"),
        "unexpected message: {}",
        msgs[0]
    );
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
    assert!(
        msgs[0].contains("100") || msgs[0].to_lowercase().contains("most"),
        "unexpected message: {}",
        msgs[0]
    );
}

/// Lengths count chars, as the model and serializer do: 64 `é` is 128 bytes (#1937).
#[test]
fn max_length_counts_chars_not_bytes() {
    let name = "é".repeat(64);
    let form = payload(&[("name", name.as_str()), ("age", "10")]);
    assert_eq!(CreateItemForm::parse(&form).unwrap().name, name);
}

#[test]
fn max_int_validator_fires() {
    let form = payload(&[("name", "x"), ("age", "999")]);
    let errors = CreateItemForm::parse(&form).unwrap_err();
    let msgs = errors.get("age");
    assert!(!msgs.is_empty(), "expected age error");
    assert!(
        msgs[0].to_lowercase().contains("150") || msgs[0].to_lowercase().contains("less"),
        "unexpected message: {}",
        msgs[0]
    );
}

#[test]
fn checkbox_falsy_aliases_recognized() {
    for falsy in ["false", "0", "off", "no", "FALSE"] {
        let form = payload(&[("name", "x"), ("age", "1"), ("active", falsy)]);
        let parsed = CreateItemForm::parse(&form).unwrap();
        assert!(!parsed.active, "expected `{falsy}` to parse as false");
    }
}

#[derive(Form, Debug, PartialEq)]
pub struct FlagForm {
    pub on: bool,
    pub maybe: Option<bool>,
}

/// Only the documented spellings read as a bool; anything else is an error.
#[test]
fn unknown_bool_spellings_are_refused() {
    for bad in ["nope", "2", "fals", "y"] {
        let errors = FlagForm::parse(&payload(&[("on", bad)])).unwrap_err();
        assert!(!errors.get("on").is_empty(), "on accepted `{bad}`");
        let errors = FlagForm::parse(&payload(&[("maybe", bad)])).unwrap_err();
        assert!(!errors.get("maybe").is_empty(), "maybe accepted `{bad}`");
    }
    for (raw, want) in [("TRUE", true), ("1", true), ("on", true), ("yes", true)] {
        let ok = FlagForm::parse(&payload(&[("on", raw), ("maybe", raw)])).unwrap();
        assert_eq!((ok.on, ok.maybe), (want, Some(want)), "`{raw}`");
    }
    let ok = FlagForm::parse(&payload(&[("on", ""), ("maybe", "")])).unwrap();
    assert_eq!((ok.on, ok.maybe), (false, None));
    let ok = FlagForm::parse(&payload(&[("maybe", "no")])).unwrap();
    assert_eq!((ok.on, ok.maybe), (false, Some(false)));
}

#[derive(Form, Debug, PartialEq)]
pub struct RatioForm {
    #[form(min = 0, max = 1)]
    pub ratio: f64,
    pub weight: Option<f32>,
}

/// NaN and inf pass every bound and serialize as null.
#[test]
fn non_finite_floats_are_refused() {
    for bad in ["NaN", "inf", "-inf", "infinity"] {
        let errors = RatioForm::parse(&payload(&[("ratio", bad)])).unwrap_err();
        assert!(!errors.get("ratio").is_empty(), "ratio accepted `{bad}`");
        let errors = RatioForm::parse(&payload(&[("ratio", "0.5"), ("weight", bad)])).unwrap_err();
        assert!(!errors.get("weight").is_empty(), "weight accepted `{bad}`");
    }
    let ok = RatioForm::parse(&payload(&[("ratio", "0.5"), ("weight", "2.5")])).unwrap();
    assert_eq!(ok.weight, Some(2.5));
}

/// Fields named like the generated locals must not shadow them.
#[derive(Form, Debug, PartialEq)]
#[form(validate = "check")]
pub struct ShadowForm {
    pub data: String,
    #[form(clean = "upper")]
    pub __errors: String,
    #[form(clean = "upper")]
    pub __cleaned: String,
    pub after: i32,
}

impl ShadowForm {
    fn upper(v: &String) -> Result<String, String> {
        Ok(v.to_uppercase())
    }

    fn check(&self) -> Result<(), rustango::forms::FormErrors> {
        Ok(())
    }
}

#[test]
fn fields_named_like_generated_locals_parse() {
    let form = payload(&[
        ("data", "d"),
        ("__errors", "e"),
        ("__cleaned", "c"),
        ("after", "7"),
    ]);
    assert_eq!(
        ShadowForm::parse(&form).unwrap(),
        ShadowForm {
            data: "d".into(),
            __errors: "E".into(),
            __cleaned: "C".into(),
            after: 7,
        }
    );
    let errors = ShadowForm::parse(&payload(&[("data", "d")])).unwrap_err();
    assert!(!errors.get("after").is_empty());
}
