//! Template error page for development, in the style of Django's
//! DEBUG page.
//!
//! In production a failed Tera render returns a plain 500 and the
//! operator reads the details from the log. That is no help locally,
//! where the developer wants to see the failure in the browser.
//!
//! [`enabled`] says whether this process should show the page.
//! [`error_page_html`] builds it from a Tera error.
//!
//! **The page shows internal details, so never turn it on in
//! production.** It prints everything [`tera::Error`] carries: the
//! message, the debug form and the full source chain. The CSS is
//! inline, so the page needs no static asset that might fail for the
//! same reason the template did.
//!
//! ## Wiring
//!
//! Anywhere a caller invokes `Tera::render` and turns the result into
//! an HTTP response, replace
//!
//! ```ignore
//! match tera.render(name, ctx) {
//!     Ok(html) => axum::response::Html(html).into_response(),
//!     Err(e) => /* plain-text 500 */,
//! }
//! ```
//!
//! with:
//!
//! ```ignore
//! match tera.render(name, ctx) {
//!     Ok(html) => axum::response::Html(html).into_response(),
//!     Err(e) if rustango::template_debug::enabled() => (
//!         axum::http::StatusCode::INTERNAL_SERVER_ERROR,
//!         axum::response::Html(rustango::template_debug::error_page_html(&e, name)),
//!     ).into_response(),
//!     Err(_) => /* plain-text 500 */,
//! }
//! ```
//!
//! `template_views::render` already does this. It also checks
//! `error::disclose_server_errors()`, so a locked-down process keeps
//! the plain 500.
//!
//! [`enabled`]: crate::template_debug::enabled
//! [`error_page_html`]: crate::template_debug::error_page_html

/// `true` when this process should show the debug page instead of a
/// plain 500. Two inputs, in order:
///
/// 1. `RUSTANGO_TEMPLATE_DEBUG`. A truthy value (`1`, `true`, `yes`,
///    `on`) forces it on; a falsy one (`0`, `false`, `no`, `off`)
///    forces it off. Case does not matter.
/// 2. Otherwise `RUSTANGO_ENV`: `prod` or `production` means off,
///    anything else (or nothing) means on.
///
/// Reading the env each call is cheap and avoids a load-order problem
/// with config. For a compile-time switch, guard the call site with
/// `#[cfg(debug_assertions)]`.
///
/// The answer comes from `error.rs`, because every 5xx path needs the
/// same one and this module is gated on `_tera`.
#[must_use]
pub fn enabled() -> bool {
    crate::error::debug_details_enabled()
}

/// Build an HTML page describing a template render failure: the
/// template name, the error, and the full source chain. The CSS is
/// inline, since the template system that just failed cannot be
/// trusted to render its own error page.
///
/// The page exposes internal details, so only serve it when
/// [`enabled`] is true.
#[must_use]
pub fn error_page_html(err: &tera::Error, template_name: &str) -> String {
    use std::error::Error as _;
    use std::fmt::Write as _;

    let mut buf = String::with_capacity(2_048);
    let _ = write!(
        buf,
        r#"<!DOCTYPE html>
<html lang="en"><head>
<meta charset="utf-8">
<title>Template error: {name}</title>
<style>
body {{
    font: 14px/1.5 -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
    margin: 0; padding: 0; background: #fafafa; color: #1a1a1a;
}}
.banner {{
    background: #b00020; color: #fff; padding: 1.2rem 2rem;
}}
.banner h1 {{ margin: 0; font-size: 1.5rem; }}
.banner small {{ display: block; opacity: 0.8; margin-top: 0.2rem; }}
main {{ padding: 1.5rem 2rem; max-width: 64rem; }}
section {{ margin-bottom: 1.5rem; }}
h2 {{ font-size: 1rem; text-transform: uppercase;
       letter-spacing: 0.05em; color: #777; margin: 0 0 0.4rem 0; }}
pre, code {{ font-family: "SF Mono", "Menlo", "Consolas", monospace;
              font-size: 13px; }}
pre {{ background: #fff; border: 1px solid #ddd;
       border-radius: 4px; padding: 0.8rem 1rem; overflow-x: auto;
       white-space: pre-wrap; word-break: break-word; }}
.footer {{ color: #888; font-size: 12px; padding: 1rem 2rem; }}
</style>
</head><body>
<div class="banner">
    <h1>Template error</h1>
    <small>Rendering <code>{name}</code></small>
</div>
<main>
"#,
        name = escape_html(template_name),
    );

    let _ = write!(
        buf,
        "<section><h2>Error</h2><pre>{}</pre></section>\n",
        escape_html(&err.to_string()),
    );

    // `tera::Error::kind` is private, but `Debug` on the error shows the
    // kind and any wrapped payload.
    let _ = write!(
        buf,
        "<section><h2>Debug</h2><pre>{}</pre></section>\n",
        escape_html(&format!("{err:?}")),
    );

    // Walk the source chain for parse line numbers and any I/O error
    // Tera wrapped.
    let mut chain: Vec<String> = Vec::new();
    let mut current: Option<&dyn std::error::Error> = err.source();
    while let Some(e) = current {
        chain.push(e.to_string());
        current = e.source();
    }
    if !chain.is_empty() {
        let _ = write!(buf, "<section><h2>Caused by</h2><pre>");
        for (i, line) in chain.iter().enumerate() {
            let _ = write!(buf, "{i}. {}\n", escape_html(line));
        }
        let _ = write!(buf, "</pre></section>\n");
    }

    buf.push_str(
        r#"<section><h2>Why am I seeing this?</h2>
<p>This page is rendered when <code>rustango::template_debug::enabled()</code>
is true. Set <code>RUSTANGO_ENV=prod</code> or
<code>RUSTANGO_TEMPLATE_DEBUG=0</code> to switch back to the
plain-text 500 response.</p>
</section>
"#,
    );
    buf.push_str("</main>\n<div class=\"footer\">rustango template debug</div>\n</body></html>");
    buf
}

/// Small HTML escape, enough to print raw error text safely without
/// going back through Tera, which is what just failed.
fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // `error::server_error_body` reads the same env vars, so share its
    // test lock instead of adding a second one.
    use crate::error::test_env::{lock as env_lock, with as with_env};

    #[test]
    fn enabled_defaults_to_true_when_no_env_set() {
        let _g = env_lock();
        with_env("RUSTANGO_TEMPLATE_DEBUG", None, || {
            with_env("RUSTANGO_ENV", None, || {
                assert!(enabled(), "missing both env vars → dev tier → debug on");
            });
        });
    }

    #[test]
    fn enabled_is_false_in_prod_tier() {
        let _g = env_lock();
        with_env("RUSTANGO_TEMPLATE_DEBUG", None, || {
            with_env("RUSTANGO_ENV", Some("prod"), || {
                assert!(!enabled(), "prod tier → debug off");
            });
            with_env("RUSTANGO_ENV", Some("production"), || {
                assert!(!enabled(), "`production` long form → debug off");
            });
        });
    }

    #[test]
    fn enabled_is_true_in_staging_and_dev_tiers() {
        let _g = env_lock();
        with_env("RUSTANGO_TEMPLATE_DEBUG", None, || {
            with_env("RUSTANGO_ENV", Some("dev"), || {
                assert!(enabled());
            });
            with_env("RUSTANGO_ENV", Some("staging"), || {
                assert!(enabled());
            });
        });
    }

    #[test]
    fn explicit_template_debug_override_wins_over_env_tier() {
        let _g = env_lock();
        // prod tier plus explicit "1" forces it on.
        with_env("RUSTANGO_ENV", Some("prod"), || {
            with_env("RUSTANGO_TEMPLATE_DEBUG", Some("1"), || {
                assert!(enabled(), "explicit `1` forces debug on in prod");
            });
            with_env("RUSTANGO_TEMPLATE_DEBUG", Some("true"), || {
                assert!(enabled());
            });
        });
        // dev tier plus explicit "0" forces it off.
        with_env("RUSTANGO_ENV", Some("dev"), || {
            with_env("RUSTANGO_TEMPLATE_DEBUG", Some("0"), || {
                assert!(!enabled(), "explicit `0` forces debug off in dev");
            });
            with_env("RUSTANGO_TEMPLATE_DEBUG", Some("off"), || {
                assert!(!enabled());
            });
        });
    }

    #[test]
    fn error_page_html_contains_template_name_and_error() {
        // Build a real `tera::Error` by triggering a parse failure.
        let mut tera = tera::Tera::default();
        let err = tera
            .add_raw_template("broken.html", "{% if %}{% endif %}")
            .expect_err("intentionally bad template");

        let page = error_page_html(&err, "broken.html");
        assert!(page.contains("Template error"), "needs banner header");
        assert!(page.contains("broken.html"), "must echo template name");
        // The error's Display includes the parser diagnostic.
        assert!(
            page.contains("Failed to parse") || page.contains("parse"),
            "should surface the parse failure text, got: {page}"
        );
        // Inline CSS, so the page needs no static asset.
        assert!(page.contains("<style>"));
        // No raw angle brackets leak from the error message.
        assert!(
            !page.contains("<%"),
            "any `<` in error text must be escaped"
        );
    }

    #[test]
    fn error_page_escapes_html_in_template_name() {
        // A template name with angle brackets must not break the page.
        let mut tera = tera::Tera::default();
        let err = tera
            .add_raw_template("x", "{% if %}{% endif %}")
            .expect_err("intentionally bad template");
        let page = error_page_html(&err, "<script>alert(1)</script>");
        assert!(!page.contains("<script>alert(1)</script>"));
        assert!(page.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    }

    #[test]
    fn escape_html_handles_all_five_entities() {
        assert_eq!(escape_html("&"), "&amp;");
        assert_eq!(escape_html("<"), "&lt;");
        assert_eq!(escape_html(">"), "&gt;");
        assert_eq!(escape_html("\""), "&quot;");
        assert_eq!(escape_html("'"), "&#39;");
        assert_eq!(escape_html("abc"), "abc");
    }
}
