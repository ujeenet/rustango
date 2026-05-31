//! Django-shape DEBUG template-error overlay — issue #386.
//!
//! When Tera fails to render a template at request time, the default
//! `template_views::render` fallback emits a 500 with a plain-text
//! "template render error: {err}" body — fine for production where
//! the operator pulls the full error from the tracing log, but
//! hostile to local dev where the developer wants to *see* what
//! broke without leaving the browser.
//!
//! This module provides the inverse path. [`enabled`] decides whether
//! the current process should serve debug overlays based on the
//! tiered-settings convention ([`RUSTANGO_ENV`] != `"prod"`, or the
//! explicit `RUSTANGO_TEMPLATE_DEBUG=1` override). [`error_page_html`]
//! renders a styled HTML page from a Tera error.
//!
//! The page intentionally pulls every field [`tera::Error`] exposes
//! — `Display`, kind discriminator, source chain — so the dev sees
//! the same diagnostic the tracing log gets. The styling is
//! inline-CSS so it works in any vanilla browser without a static
//! asset round-trip (which might fail for the same reason the
//! template did).
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
//! The framework's own `template_views::render` is updated for this
//! in the same slice as the helper — see the call site for the
//! reference wiring.

/// `true` when the current process should serve debug overlays on
/// template render errors. Resolution order:
///
/// 1. `RUSTANGO_TEMPLATE_DEBUG` env var, when set to a truthy value
///    (`1` / `true` / `yes` / `on` — case-insensitive) — forces ON.
///    Setting it to a falsy value (`0` / `false` / `no` / `off`)
///    forces OFF.
/// 2. Otherwise, `RUSTANGO_ENV` — `prod` (or `production`) →  OFF,
///    anything else (including absent) → ON.
///
/// Reading the env on every call is cheap (single `std::env::var`)
/// and avoids a startup-time vs. config-load-time ordering hazard.
/// Callers that want compile-time control can wrap their call site
/// in `#[cfg(debug_assertions)]`.
#[must_use]
pub fn enabled() -> bool {
    if let Ok(raw) = std::env::var("RUSTANGO_TEMPLATE_DEBUG") {
        return match raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            // Any other value — ignore and fall through to env-tier.
            _ => env_tier_is_dev(),
        };
    }
    env_tier_is_dev()
}

fn env_tier_is_dev() -> bool {
    let env = std::env::var("RUSTANGO_ENV").unwrap_or_default();
    !matches!(
        env.trim().to_ascii_lowercase().as_str(),
        "prod" | "production"
    )
}

/// Render a styled HTML page describing a template render failure.
/// Layout: red header banner, monospace error body, template-name
/// + error-kind discriminator, full source-chain walk. Inline CSS
/// so it works without an external stylesheet (the same template
/// system that failed isn't trustworthy for re-rendering its own
/// error page).
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

    // `tera::Error::kind` is a private field; `Debug` on the error
    // value itself surfaces the kind variant + any wrapped payload
    // for the same diagnostic value.
    let _ = write!(
        buf,
        "<section><h2>Debug</h2><pre>{}</pre></section>\n",
        escape_html(&format!("{err:?}")),
    );

    // Walk the source chain to pick up parse-error line numbers and
    // any underlying I/O failure that Tera wraps.
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

/// Minimal HTML-escape — enough for showing raw error strings on
/// the debug page without ever crossing back into a Tera render
/// (which is what failed in the first place).
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

    /// Guard against parallel-test env mutation — env is process-global.
    /// All `enabled()` tests acquire this mutex so they can mutate the
    /// two env vars deterministically.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    /// Helper for env-mutating tests. Edition 2021 still permits
    /// bare `set_var`/`remove_var`; the workspace `unsafe_code =
    /// "forbid"` lint blocks the edition-2024 unsafe form, so this
    /// keeps the calls bare.
    fn with_env<F: FnOnce()>(key: &str, val: Option<&str>, f: F) {
        let prev = std::env::var(key).ok();
        match val {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        f();
        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }

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
        // prod tier + explicit "1" → forced ON.
        with_env("RUSTANGO_ENV", Some("prod"), || {
            with_env("RUSTANGO_TEMPLATE_DEBUG", Some("1"), || {
                assert!(enabled(), "explicit `1` forces debug on in prod");
            });
            with_env("RUSTANGO_TEMPLATE_DEBUG", Some("true"), || {
                assert!(enabled());
            });
        });
        // dev tier + explicit "0" → forced OFF.
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
        // Inline CSS so the page works without a static-asset hop.
        assert!(page.contains("<style>"));
        // No raw < / > leak through from the error message.
        assert!(
            !page.contains("<%"),
            "any `<` in error text must be escaped"
        );
    }

    #[test]
    fn error_page_escapes_html_in_template_name() {
        // Defensive — a malicious or accidental template name with
        // angle brackets shouldn't break out of the page.
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
