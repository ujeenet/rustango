//! Cookbook Chapter 7 — forms + serializer.
//!
//! ModelFormFor<T> parses + validates request payloads against the
//! model's schema. Both form-encoded (HashMap<String, String>) and
//! JSON request bodies are supported via the same per-field parser.
//!
//! No DB needed — pure parsing + validation. Run:
//! `cargo test --test cookbook_chapter07_forms`.

use cookbook_blog::apps::blog::models::{Author, Rating};
use rustango::forms::ModelFormFor;
use std::collections::HashMap;

// §7.95 — ModelFormFor<T> parses a form-encoded payload into typed values.
#[test]
fn modelform_parses_form_encoded_into_typed_values() {
    let mut payload = HashMap::new();
    payload.insert("name".into(), "ada".into());
    payload.insert("email".into(), "ada@example.com".into());
    // bio is Option<String>; missing key means the field is absent.
    let mf: ModelFormFor<Author> = ModelFormFor::parse(&payload).expect("parse");
    let cols = mf.columns();
    let vals = mf.values();
    assert_eq!(cols.len(), vals.len(), "every column has a value");
    // Auto<i64> id is skipped on create. joined_at is Auto<DateTime> + auto_now_add — also skipped.
    assert!(cols.contains(&"name"));
    assert!(cols.contains(&"email"));
}

// §7.96 — ModelFormFor::from_json accepts a JSON object.
#[test]
fn modelform_from_json_parses_object() {
    let body = serde_json::json!({
        "name": "json-author",
        "email": "json@example.com",
        "bio": "wrote a thing",
    });
    let mf: ModelFormFor<Author> = ModelFormFor::from_json(&body).expect("from_json");
    let bio_col = mf.columns().iter().position(|c| *c == "bio")
        .expect("bio should land in columns when value provided");
    match &mf.values()[bio_col] {
        rustango::core::SqlValue::String(s) => assert_eq!(s, "wrote a thing"),
        other => panic!("bio should be String, got {other:?}"),
    }
}

// §7.95 — Missing required fields surface FormErrors per field.
#[test]
fn modelform_missing_required_fields_aggregate_errors() {
    let payload = HashMap::new(); // empty — every required field will trip
    let err = ModelFormFor::<Author>::parse(&payload).expect_err("required fields missing");
    // FormErrors carries one issue per missing/invalid field — name and email both required.
    let s = format!("{err:?}").to_lowercase();
    assert!(s.contains("name"), "expected `name` in error report: {s}");
    assert!(s.contains("email"), "expected `email` in error report: {s}");
}

// §7.98 — bounds validation runs at parse time. Rating.score has min=1 max=5;
// posting score=99 triggers an OutOfRange error in FormErrors.
#[test]
fn modelform_bound_violation_lands_in_form_errors() {
    let mut payload = HashMap::new();
    payload.insert("score".into(), "99".into());
    let err = ModelFormFor::<Rating>::parse(&payload)
        .expect_err("score=99 violates max=5");
    let s = format!("{err:?}").to_lowercase();
    assert!(
        s.contains("score") && (s.contains("range") || s.contains("max")),
        "expected score+range/max in error report: {s}"
    );
}

// §7.96 — JSON null on Option<String> field writes SqlValue::Null
// explicitly. Caller said "set this column to NULL"; ModelFormFor
// preserves the intent (vs omitting and relying on DB default).
#[test]
fn modelform_from_json_null_writes_explicit_null() {
    let body = serde_json::json!({
        "name": "n",
        "email": "n@example.com",
        "bio": null,
    });
    let mf: ModelFormFor<Author> = ModelFormFor::from_json(&body).expect("from_json");
    let bio_idx = mf.columns().iter().position(|c| *c == "bio")
        .expect("bio kept on the column list");
    assert!(matches!(mf.values()[bio_idx], rustango::core::SqlValue::Null),
        "bio = null should land as SqlValue::Null, got {:?}", mf.values()[bio_idx]);
}

// §7.99 — into_insert_query emits a query against the right table.
#[test]
fn modelform_into_insert_query_targets_model_table() {
    let mut payload = HashMap::new();
    payload.insert("name".into(), "fred".into());
    payload.insert("email".into(), "fred@example.com".into());
    let mf: ModelFormFor<Author> = ModelFormFor::parse(&payload).unwrap();
    let q = mf.into_insert_query();
    assert_eq!(q.model.table, Author::SCHEMA.table);
}

use rustango::core::Model as _;
