//! Django-parity #383 — `register_template_filter!` /
//! `register_template_function!` register custom Tera filters +
//! functions in an inventory-collected registry. The framework's
//! `template_extensions::apply_to_tera` walks the registry and
//! attaches everything to a Tera instance at construction time.

#![cfg(feature = "sqlite")]

use std::collections::HashMap;

use serde_json::Value;
use tera::Tera;

// Inventory-collected filter — uppercases the string it receives
// and appends an exclamation mark.
fn shout(value: &Value, _args: &HashMap<String, Value>) -> tera::Result<Value> {
    let raw = value.as_str().unwrap_or("");
    Ok(Value::String(format!("{}!", raw.to_uppercase())))
}
rustango::register_template_filter!("shout", shout);

// Inventory-collected function — returns a constant version string.
fn build_version(_args: &HashMap<String, Value>) -> tera::Result<Value> {
    Ok(Value::String("test-build".to_string()))
}
rustango::register_template_function!("build_version", build_version);

// Filter that takes an argument — verifies the args map threads
// through.
fn add_suffix(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let raw = value.as_str().unwrap_or("");
    let suffix = args.get("suffix").and_then(Value::as_str).unwrap_or("");
    Ok(Value::String(format!("{raw}{suffix}")))
}
rustango::register_template_filter!("add_suffix", add_suffix);

fn fresh_tera_with_template(name: &str, body: &str) -> Tera {
    let mut t = Tera::default();
    t.add_raw_template(name, body).unwrap();
    rustango::template_extensions::apply_to_tera(&mut t);
    t
}

#[test]
fn registered_filter_runs_in_a_template() {
    let tera = fresh_tera_with_template("t.html", r#"{{ "hello" | shout | safe }}"#);
    let out = tera.render("t.html", &tera::Context::new()).unwrap();
    assert_eq!(out, "HELLO!");
}

#[test]
fn registered_function_runs_in_a_template() {
    let tera = fresh_tera_with_template("t.html", "{{ build_version() | safe }}");
    let out = tera.render("t.html", &tera::Context::new()).unwrap();
    assert_eq!(out, "test-build");
}

#[test]
fn filter_with_named_argument_threads_through() {
    let tera = fresh_tera_with_template(
        "t.html",
        r#"{{ "hi" | add_suffix(suffix=" there") | safe }}"#,
    );
    let out = tera.render("t.html", &tera::Context::new()).unwrap();
    assert_eq!(out, "hi there");
}

#[test]
fn apply_to_tera_is_idempotent() {
    let mut t = Tera::default();
    t.add_raw_template("t.html", r#"{{ "x" | shout | safe }}"#)
        .unwrap();
    rustango::template_extensions::apply_to_tera(&mut t);
    // Second call — Tera::register_filter overwrites, so a re-apply
    // must not corrupt the existing registration.
    rustango::template_extensions::apply_to_tera(&mut t);
    let out = t.render("t.html", &tera::Context::new()).unwrap();
    assert_eq!(out, "X!");
}

#[test]
fn unregistered_filter_still_errors() {
    // Sanity: only the names we registered should resolve. A bogus
    // filter name still produces a Tera template error.
    let mut t = Tera::default();
    t.add_raw_template("t.html", r#"{{ "x" | nonexistent_filter }}"#)
        .unwrap_or_else(|_| {
            // Tera may surface unknown filters at parse OR render
            // time depending on the version; this branch covers
            // the parse-time variant.
        });
    rustango::template_extensions::apply_to_tera(&mut t);
    let result = t.render("t.html", &tera::Context::new());
    assert!(
        result.is_err(),
        "rendering with an unregistered filter must error"
    );
}
