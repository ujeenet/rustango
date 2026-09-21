//! Tera template registry, baked into the binary.
//!
//! Every admin template is pulled in with `include_str!`, so the admin
//! needs no files at runtime. The registry is built on first render
//! and held in a `OnceLock`.

use std::sync::OnceLock;

/// The bundled templates, built on first use.
fn templates() -> &'static tera::Tera {
    static T: OnceLock<tera::Tera> = OnceLock::new();
    T.get_or_init(|| {
        let mut tera = tera::Tera::default();
        tera.add_raw_templates([
            (
                "_theme_tokens.html",
                include_str!("../styles/theme_tokens.html"),
            ),
            (
                "_admin_styles.html",
                include_str!("templates/_admin_styles.html"),
            ),
            (
                "_theme_toggle.html",
                include_str!("templates/_theme_toggle.html"),
            ),
            ("base.html", include_str!("templates/base.html")),
            ("_sidebar.html", include_str!("templates/_sidebar.html")),
            ("index.html", include_str!("templates/index.html")),
            ("list.html", include_str!("templates/list.html")),
            ("detail.html", include_str!("templates/detail.html")),
            (
                "_inline_panels.html",
                include_str!("templates/_inline_panels.html"),
            ),
            ("form.html", include_str!("templates/form.html")),
            ("login.html", include_str!("templates/login.html")),
            (
                "change_password.html",
                include_str!("templates/change_password.html"),
            ),
            ("audit_log.html", include_str!("templates/audit_log.html")),
            ("docs.html", include_str!("templates/docs.html")),
            (
                "totp_enroll.html",
                include_str!("templates/totp_enroll.html"),
            ),
        ])
        .expect("admin templates compile");
        tera
    })
}

/// Render a bundled template with a serializable context.
///
/// # Panics
/// If the template is missing or the context does not serialize. Both
/// are bugs the tests catch.
pub(crate) fn render_template(template: &str, ctx: &serde_json::Value) -> String {
    let tera_ctx = tera::Context::from_serialize(ctx).expect("admin context serializes");
    templates()
        .render(template, &tera_ctx)
        .expect("admin template renders")
}

/// Render with extra chrome variables merged into `ctx` just before
/// the render. Views use it to add the sidebar and chrome data without
/// having to know the keys. An existing key in `ctx` wins.
pub(crate) fn render_with_chrome(
    template: &str,
    ctx: &mut serde_json::Value,
    chrome: serde_json::Value,
) -> String {
    if let (Some(map), Some(extra)) = (ctx.as_object_mut(), chrome.as_object()) {
        for (k, v) in extra {
            map.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
    render_template(template, ctx)
}
