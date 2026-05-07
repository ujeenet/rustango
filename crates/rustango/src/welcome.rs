//! First-run welcome page — confidence signal that rustango is wired up.
//!
//! Mount under `/` while you're getting started; replace with your own
//! root handler when you have content to serve.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::welcome::welcome_router;
//! use axum::Router;
//!
//! let app = Router::new()
//!     .merge(welcome_router())     // serves "/"
//!     .nest("/api", api_routes());
//! ```

use axum::http::header;
use axum::response::Html;
use axum::routing::get;
use axum::Router;

/// Build a router that serves a welcome page at `/`.
#[must_use]
pub fn welcome_router() -> Router {
    Router::new().route("/", get(welcome_page))
}

async fn welcome_page() -> ([(axum::http::HeaderName, &'static str); 1], Html<String>) {
    let version = env!("CARGO_PKG_VERSION");
    let html = welcome_html(version);
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        Html(html),
    )
}

fn welcome_html(version: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>rustango — it works!</title>
<style>
:root {{ color-scheme: light dark; }}
body {{ font-family: ui-sans-serif, system-ui, -apple-system, sans-serif;
       max-width: 720px; margin: 4rem auto; padding: 0 1.5rem; line-height: 1.6; }}
h1 {{ margin: 0 0 .25rem; font-weight: 700; letter-spacing: -.02em; }}
.tag {{ color: #888; font-size: .9rem; margin-bottom: 2rem; }}
ul {{ padding-left: 1.25rem; }}
code {{ font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
       background: rgba(127,127,127,.15); padding: .15em .35em; border-radius: 4px;
       font-size: .92em; }}
.next {{ background: rgba(127,127,127,.08); padding: 1rem 1.5rem;
         border-radius: 8px; margin: 1rem 0; }}
a {{ color: #3672e0; text-decoration: none; }}
a:hover {{ text-decoration: underline; }}
.foot {{ color: #999; font-size: .85rem; margin-top: 3rem; text-align: center; }}
</style>
</head>
<body>
<h1>rustango is running ✓</h1>
<p class="tag">Framework v{version} — ready to build something.</p>

<div class="next">
<h3 style="margin-top:0">Next steps</h3>
<ul>
  <li>Scaffold an app: <code>cargo run -- startapp blog</code></li>
  <li>Define a model in <code>src/blog/models.rs</code> with <code>#[derive(Model)]</code></li>
  <li>Generate + apply migration: <code>manage makemigrations &amp;&amp; manage migrate</code></li>
  <li>Mount your routes in <code>src/urls.rs</code> and remove <code>welcome_router()</code> from your app</li>
</ul>
</div>

<h3>Useful commands</h3>
<ul>
  <li><code>manage about</code> — env summary, version, configured backends</li>
  <li><code>manage check --deploy</code> — production readiness audit</li>
  <li><code>manage showmigrations</code> — applied/pending migration list</li>
  <li><code>manage docs</code> — open API docs</li>
</ul>

<h3>Defaults that just shipped</h3>
<ul>
  <li>ORM with auto-migrations, M2M, audit trail, soft-delete</li>
  <li>Multi-tenant resolver chain (subdomain / path / header)</li>
  <li>Auth: model-backed, API keys, JWT (access + refresh + custom claims), TOTP / 2FA</li>
  <li>Cache (in-memory + Redis), email, file storage, scheduled tasks, signals</li>
  <li>Security headers, rate limiting, CORS, CSRF, request ID, IP filter</li>
</ul>

<p class="foot">Replace this page when you mount your own <code>/</code> route.</p>
</body>
</html>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn welcome_html_contains_version() {
        let html = welcome_html("0.20.30");
        assert!(html.contains("0.20.30"));
    }

    #[test]
    fn welcome_html_includes_next_steps() {
        let html = welcome_html("x");
        assert!(html.contains("startapp"));
        assert!(html.contains("makemigrations"));
        assert!(html.contains("migrate"));
    }

    #[test]
    fn welcome_html_is_self_contained_no_external_deps() {
        let html = welcome_html("x");
        // No cdn references, no external js, fonts use system stack
        assert!(!html.contains("cdn."));
        assert!(!html.contains("googleapis"));
        assert!(!html.contains("<script"));
    }
}
