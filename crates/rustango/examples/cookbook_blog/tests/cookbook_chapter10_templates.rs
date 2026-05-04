//! Cookbook Chapter 10 — templates (Tera) + render helpers.
//!
//! No DB needed for the basic Tera setup recipes. The
//! `render_generic_fk_link` helper is exercised live in
//! cookbook_chapter02_models.rs::generic_fk_schema_and_content_type_lookup.
//!
//! Run: `cargo test --test cookbook_chapter10_templates`

// §10.119 — Tera template render with context.
#[test]
fn tera_template_renders_with_context() {
    let mut t = tera::Tera::default();
    t.add_raw_template(
        "post.html",
        r#"<h1>{{ title }}</h1><p>by {{ author }}</p>"#,
    ).unwrap();

    let mut ctx = tera::Context::new();
    ctx.insert("title", "Rust ORM");
    ctx.insert("author", "ada");

    let out = t.render("post.html", &ctx).unwrap();
    assert!(out.contains("<h1>Rust ORM</h1>"));
    assert!(out.contains("<p>by ada</p>"));
}

// §10.119 — Tera autoescape: HTML special chars in context get escaped.
#[test]
fn tera_template_autoescapes_html() {
    let mut t = tera::Tera::default();
    t.autoescape_on(vec!["html"]);
    t.add_raw_template("x.html", "{{ user_input }}").unwrap();

    let mut ctx = tera::Context::new();
    ctx.insert("user_input", "<script>alert('xss')</script>");

    let out = t.render("x.html", &ctx).unwrap();
    assert!(out.contains("&lt;script&gt;"), "auto-escaped output: {out}");
    assert!(!out.contains("<script>"), "raw script tag must not survive: {out}");
}

// §10.122 — Tera supports include + extends for layout reuse.
#[test]
fn tera_extends_inherits_blocks_from_base() {
    let mut t = tera::Tera::default();
    t.add_raw_templates(vec![
        ("base.html",  "<html><body>{% block content %}fallback{% endblock %}</body></html>"),
        ("post.html", "{% extends \"base.html\" %}{% block content %}<p>{{ body }}</p>{% endblock %}"),
    ]).unwrap();

    let mut ctx = tera::Context::new();
    ctx.insert("body", "hello world");
    let out = t.render("post.html", &ctx).unwrap();
    assert!(out.contains("<p>hello world</p>"));
    assert!(!out.contains("fallback"), "child block should override parent");
}
