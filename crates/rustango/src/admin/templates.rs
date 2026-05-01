//! Tera template registry baked into the binary at compile time.
//!
//! Five small templates (`base`, `index`, `list`, `detail`, `form`) are
//! pulled in via `include_str!` so the admin has no runtime filesystem
//! dependency. Lazily-initialized on first render via `OnceLock`.

use std::sync::OnceLock;

/// Lazily-initialized Tera registry holding the bundled templates.
fn templates() -> &'static tera::Tera {
    static T: OnceLock<tera::Tera> = OnceLock::new();
    T.get_or_init(|| {
        let mut tera = tera::Tera::default();
        tera.add_raw_templates([
            ("base.html", include_str!("templates/base.html")),
            ("_sidebar.html", include_str!("templates/_sidebar.html")),
            ("index.html", include_str!("templates/index.html")),
            ("list.html", include_str!("templates/list.html")),
            ("detail.html", include_str!("templates/detail.html")),
            ("form.html", include_str!("templates/form.html")),
        ])
        .expect("admin templates compile");
        tera
    })
}

/// Render a bundled template with a serde-serializable context. Panics
/// if the template is missing or the context fails to serialize — both
/// are programmer bugs caught in tests.
pub(crate) fn render_template(template: &str, ctx: &serde_json::Value) -> String {
    let tera_ctx = tera::Context::from_serialize(ctx).expect("admin context serializes");
    templates()
        .render(template, &tera_ctx)
        .expect("admin template renders")
}

/// Render with a render-time supplement context — the supplement is
/// merged into the base context just before render. Used by view code
/// to layer sidebar / chrome data onto a per-view context without each
/// caller having to remember the keys.
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
