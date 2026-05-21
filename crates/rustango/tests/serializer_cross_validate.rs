//! Django-parity #436 — DRF `validate(self, data)` cross-field hook
//! via `#[serializer(validate = "fn_name")]` at the container level.
//!
//! Verifies the macro:
//! - Emits a `validate()` method even when no per-field validators are declared
//! - Calls the named cross-field method after per-field validators
//! - Merges cross-field `FormErrors` with per-field errors via the new
//!   `FormErrors::merge` API

#![cfg(feature = "serializer")]

use rustango::core::Model;
use rustango::forms::FormErrors;
use rustango_macros::{Model, Serializer};

#[derive(Model, Debug, Clone)]
#[rustango(table = "ser_cv_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 200)]
    pub title: String,
    #[rustango(max_length = 1000)]
    pub body: String,
}

// Container-level cross-field validator only — no per-field validators.
#[derive(Serializer, serde::Deserialize, Debug, Clone, Default)]
#[serializer(model = Post, validate = "cross_validate")]
pub struct PostSerializer {
    pub id: i64,
    pub title: String,
    pub body: String,
}

impl PostSerializer {
    fn cross_validate(&self) -> Result<(), FormErrors> {
        let mut errs = FormErrors::default();
        // Cross-field rule: title must not duplicate the first line of body.
        if !self.title.is_empty() && self.body.starts_with(&self.title) {
            errs.add_non_field("title must not match the body's first line");
        }
        if errs.is_empty() {
            Ok(())
        } else {
            Err(errs)
        }
    }
}

// Mixed model — per-field validator + container-level cross-field.
#[derive(Serializer, serde::Deserialize, Debug, Clone, Default)]
#[serializer(model = Post, validate = "matched_pair")]
pub struct MixedPostSerializer {
    pub id: i64,
    #[serializer(validate = "non_empty")]
    pub title: String,
    #[serializer(validate = "non_empty")]
    pub body: String,
}

impl MixedPostSerializer {
    fn non_empty(value: &String) -> Result<(), String> {
        if value.trim().is_empty() {
            Err("must not be empty".into())
        } else {
            Ok(())
        }
    }
    fn matched_pair(&self) -> Result<(), FormErrors> {
        let mut errs = FormErrors::default();
        if self.title.len() < self.body.len() / 10 {
            errs.add("title", "title must be at least 1/10 the length of body");
        }
        if errs.is_empty() {
            Ok(())
        } else {
            Err(errs)
        }
    }
}

#[test]
fn cross_validate_ok_when_rule_passes() {
    let s = PostSerializer {
        id: 1,
        title: "Hello".into(),
        body: "World wide post".into(),
    };
    s.validate().unwrap();
}

#[test]
fn cross_validate_surfaces_non_field_error() {
    let s = PostSerializer {
        id: 1,
        title: "Hello".into(),
        body: "Hello world".into(),
    };
    let err = s.validate().unwrap_err();
    assert!(
        !err.non_field().is_empty(),
        "expected non_field error, got: {err:?}"
    );
    assert!(err
        .non_field()
        .iter()
        .any(|m| m.contains("title must not match")));
}

#[test]
fn mixed_aggregates_per_field_then_cross_field() {
    let s = MixedPostSerializer {
        id: 1,
        // Triggers per-field 'non_empty' validator AND the cross-field
        // ratio check (since body.len() > 10 * 0).
        title: "".into(),
        body: "a very long body string that exceeds the cross-field ratio".into(),
    };
    let err = s.validate().unwrap_err();
    // Per-field error on title:
    let title_errs = err.fields().get("title").cloned().unwrap_or_default();
    assert!(
        title_errs.iter().any(|m| m.contains("must not be empty")),
        "per-field error missing: {title_errs:?}"
    );
    // Cross-field error also on title key (the cross_validate added it):
    assert!(
        title_errs
            .iter()
            .any(|m| m.contains("at least 1/10 the length of body")),
        "cross-field error missing: {title_errs:?}"
    );
}

#[test]
fn mixed_ok_when_both_layers_pass() {
    let s = MixedPostSerializer {
        id: 1,
        title: "Hello, this is a long enough title".into(),
        body: "Short body".into(),
    };
    s.validate().unwrap();
}

#[test]
fn form_errors_merge_combines_both_buckets() {
    let mut a = FormErrors::default();
    a.add("title", "too short");
    a.add_non_field("non-field A");

    let mut b = FormErrors::default();
    b.add("title", "also too long");
    b.add("body", "missing");
    b.add_non_field("non-field B");

    a.merge(b);
    assert_eq!(a.fields().get("title").unwrap().len(), 2);
    assert!(a.fields().contains_key("body"));
    assert_eq!(a.non_field().len(), 2);
}

/// Required by the Model derive — Post is referenced in the serializer
/// header but never actually constructed here; the test surface is
/// purely the serializer's validate(). Keeps clippy happy.
#[test]
fn post_schema_is_addressable() {
    let _ = Post::SCHEMA.name;
}
