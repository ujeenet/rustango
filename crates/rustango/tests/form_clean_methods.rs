//! Django-parity #372 + #373 — Form `clean_<field>` (per-field) and
//! `clean()` (cross-field) validation hooks.
//!
//! Issue #372: `#[form(clean = "fn")]` on a field declares a
//! per-field cleaning function called after the typed parse +
//! length/range checks.
//!
//! Issue #373: `#[form(validate = "fn")]` on the container declares
//! a cross-field validation function called after every field
//! parses successfully.

use std::collections::HashMap;

use rustango::forms::{Form as _, FormErrors};
use rustango_macros::Form;

// ---------- #372 — per-field clean ----------

#[derive(Form, Default, Debug)]
pub struct PerFieldCleanForm {
    #[form(clean = "lowercase_email")]
    pub email: String,
    pub name: String,
}

impl PerFieldCleanForm {
    fn lowercase_email(value: &str) -> Result<String, String> {
        if value.ends_with("@spam.com") {
            return Err("no @spam.com addresses allowed".into());
        }
        Ok(value.to_lowercase())
    }
}

#[test]
fn per_field_clean_lowers_input() {
    let mut data = HashMap::new();
    data.insert("email".into(), "FOO@Example.com".into());
    data.insert("name".into(), "Foo".into());
    let parsed = PerFieldCleanForm::parse(&data).expect("parse");
    assert_eq!(parsed.email, "foo@example.com");
    assert_eq!(parsed.name, "Foo");
}

#[test]
fn per_field_clean_rejects_with_field_keyed_error() {
    let mut data = HashMap::new();
    data.insert("email".into(), "bot@spam.com".into());
    data.insert("name".into(), "Bot".into());
    let err = PerFieldCleanForm::parse(&data).unwrap_err();
    let field_errs = err.fields().get("email").cloned().unwrap_or_default();
    assert!(
        field_errs.iter().any(|m| m.contains("@spam.com")),
        "expected field-keyed clean error: {field_errs:?}"
    );
    assert!(
        err.non_field().is_empty(),
        "clean error must be field-keyed"
    );
}

// ---------- #373 — cross-field validate ----------

#[derive(Form, Default, Debug)]
#[form(validate = "check_pair")]
pub struct PairForm {
    pub start: i64,
    pub end: i64,
}

impl PairForm {
    fn check_pair(&self) -> Result<(), FormErrors> {
        let mut errs = FormErrors::default();
        if self.start > self.end {
            errs.add_non_field("start must be ≤ end");
        }
        if errs.is_empty() {
            Ok(())
        } else {
            Err(errs)
        }
    }
}

#[test]
fn cross_field_validator_ok_when_rule_passes() {
    let mut data = HashMap::new();
    data.insert("start".into(), "1".into());
    data.insert("end".into(), "10".into());
    let parsed = PairForm::parse(&data).expect("parse");
    assert_eq!(parsed.start, 1);
    assert_eq!(parsed.end, 10);
}

#[test]
fn cross_field_validator_surfaces_non_field_error() {
    let mut data = HashMap::new();
    data.insert("start".into(), "100".into());
    data.insert("end".into(), "5".into());
    let err = PairForm::parse(&data).unwrap_err();
    assert!(
        err.non_field().iter().any(|m| m.contains("≤ end")),
        "expected non-field error, got: {err:?}"
    );
}

// ---------- Composition: per-field + cross-field ----------

#[derive(Form, Default, Debug)]
#[form(validate = "check_combined")]
pub struct CombinedForm {
    #[form(clean = "trim_name")]
    pub name: String,
    pub age: i64,
}

impl CombinedForm {
    fn trim_name(v: &str) -> Result<String, String> {
        Ok(v.trim().to_owned())
    }
    fn check_combined(&self) -> Result<(), FormErrors> {
        let mut errs = FormErrors::default();
        if self.name.is_empty() && self.age < 18 {
            errs.add_non_field("name required when age < 18");
        }
        if errs.is_empty() {
            Ok(())
        } else {
            Err(errs)
        }
    }
}

#[test]
fn combined_per_field_runs_before_cross_field() {
    let mut data = HashMap::new();
    data.insert("name".into(), "  ".into());
    data.insert("age".into(), "12".into());
    let err = CombinedForm::parse(&data).unwrap_err();
    // The trim runs first, leaving name empty; the cross-field
    // validator then rejects.
    assert!(err.non_field().iter().any(|m| m.contains("name required")));
}

#[test]
fn combined_per_field_ok_skips_cross_field_when_no_failure() {
    let mut data = HashMap::new();
    data.insert("name".into(), "  Alice  ".into());
    data.insert("age".into(), "10".into());
    let parsed = CombinedForm::parse(&data).expect("parse");
    assert_eq!(parsed.name, "Alice");
    assert_eq!(parsed.age, 10);
}
