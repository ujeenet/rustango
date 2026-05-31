//! Django-parity #384 — `register_template_context_processor!`
//! registers a callable that gets merged into every Tera
//! template's context via `apply_to_context`. Verifies the macro
//! shape end-to-end + the handler-key-wins override semantics.

#![cfg(feature = "sqlite")]

use std::collections::HashMap;

use axum::http::Request;
use serde_json::{json, Value};
use tera::{Context, Tera};

// Register two processors at static-init time. Both fire on every
// `apply_to_context` call, in registration order.
rustango::register_template_context_processor!(|_parts| {
    let mut out: HashMap<String, Value> = HashMap::new();
    out.insert("build_version".into(), json!("test-build"));
    out.insert("winner".into(), json!("processor")); // collides with handler key
    out
});

rustango::register_template_context_processor!(|parts| {
    let mut out: HashMap<String, Value> = HashMap::new();
    out.insert("request_path".into(), json!(parts.uri.path().to_string()));
    out
});

fn parts(path: &str) -> axum::http::request::Parts {
    let req: Request<()> = Request::builder().uri(path).body(()).unwrap();
    let (p, ()) = req.into_parts();
    p
}

#[test]
fn registered_processors_inject_their_keys() {
    let parts = parts("/some/path");
    let ctx = rustango::template_context_processors::context_from_processors(&parts);
    let json = ctx.into_json();
    let obj = json.as_object().unwrap();
    assert_eq!(obj.get("build_version").unwrap(), &json!("test-build"));
    assert_eq!(obj.get("request_path").unwrap(), &json!("/some/path"));
    assert_eq!(obj.get("winner").unwrap(), &json!("processor"));
}

#[test]
fn handler_supplied_keys_win_over_processor_keys() {
    let parts = parts("/handler-wins-path");
    let mut ctx = Context::new();
    ctx.insert("winner", &"caller");
    rustango::template_context_processors::apply_to_context(&mut ctx, &parts);
    let json = ctx.into_json();
    let obj = json.as_object().unwrap();
    // Handler's `winner` survives; processor's was skipped.
    assert_eq!(obj.get("winner").unwrap(), &json!("caller"));
    // Processor-only keys still land.
    assert_eq!(obj.get("build_version").unwrap(), &json!("test-build"));
    assert_eq!(
        obj.get("request_path").unwrap(),
        &json!("/handler-wins-path")
    );
}

#[test]
fn processors_render_through_an_actual_tera_template() {
    let parts = parts("/render-test");
    let ctx = rustango::template_context_processors::context_from_processors(&parts);

    let mut tera = Tera::default();
    // Tera's default HTML auto-escape policy fires on `.html`
    // templates and turns `/` into `&#x2F;`. Use `safe` so the
    // assertion stays readable.
    tera.add_raw_template(
        "page.html",
        "v={{ build_version | safe }} path={{ request_path | safe }}",
    )
    .unwrap();
    let rendered = tera.render("page.html", &ctx).unwrap();
    assert_eq!(rendered, "v=test-build path=/render-test");
}

#[test]
fn parts_value_threads_through_to_the_processor() {
    // Different paths produce different `request_path` injections.
    let p1 = parts("/a");
    let p2 = parts("/b");
    let c1 = rustango::template_context_processors::context_from_processors(&p1);
    let c2 = rustango::template_context_processors::context_from_processors(&p2);
    assert_eq!(c1.into_json().get("request_path").unwrap(), &json!("/a"));
    assert_eq!(c2.into_json().get("request_path").unwrap(), &json!("/b"));
}
