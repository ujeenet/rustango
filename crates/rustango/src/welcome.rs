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

use axum::extract::OriginalUri;
use axum::http::header;
use axum::response::{Html, Response};
use axum::routing::get;
use axum::Router;

/// Embedded brand image (square `icon.png`). Welcome page
/// references this via a sibling route so the page works
/// standalone even in single-tenant projects with no static-file
/// infrastructure. v0.30.19. Replaces the inline SVG mark used
/// in v0.29.12 → v0.30.18; the .ico variant tested in interim
/// rendered poorly because the embedded inner image was non-
/// square.
const RUSTANGO_ICON_PNG: &[u8] = include_bytes!("tenancy/static/icon.png");

/// Build a router that serves a welcome page at `/` and the
/// embedded favicon at `/welcome_icon.png` so the welcome page
/// can render the real rustango brand mark without depending on
/// the tenancy static-file infrastructure.
#[must_use]
pub fn welcome_router() -> Router {
    Router::new()
        .route("/", get(welcome_page))
        .route("/welcome_icon.png", get(welcome_icon))
}

async fn welcome_page(
    OriginalUri(uri): OriginalUri,
) -> ([(axum::http::HeaderName, &'static str); 1], Html<String>) {
    let version = env!("CARGO_PKG_VERSION");
    // The welcome router can be mounted at `/` (via Cli::with_welcome())
    // OR nested at any prefix (e.g. `/welcome` in tango — see
    // `urls.rs::api()`). axum's `Router::nest` strips the prefix
    // from the inner request path before our handler runs, so
    // `req.uri().path()` returns "/" in both cases — wrong for
    // building an absolute icon URL when nested.
    //
    // `OriginalUri` preserves the pre-nest path: "/" when mounted
    // via merge, "/welcome" when nested at "/welcome", etc. We
    // strip the trailing "/" and append "/welcome_icon.png".
    let req_path = uri.path();
    let prefix = req_path.trim_end_matches('/');
    let icon_url = format!("{prefix}/welcome_icon.png");
    let html = welcome_html(version, &icon_url);
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        Html(html),
    )
}

async fn welcome_icon() -> Response {
    Response::builder()
        .status(axum::http::StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/png")
        .header(header::CACHE_CONTROL, "public, max-age=86400")
        .body(axum::body::Body::from(RUSTANGO_ICON_PNG))
        .expect("response builds")
}

fn welcome_html(version: &str, icon_url: &str) -> String {
    format!(
        r##"<!doctype html>
<html lang="en">
<head>
<link rel="icon" type="image/png" href="{icon_url}">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>rustango — it works!</title>
<style>
:root {{
  color-scheme: light dark;
  --accent: #3672e0;
  --rust: #e07a3a;
  --bg-card: rgba(127,127,127,.08);
  --border: rgba(127,127,127,.2);
}}
* {{ box-sizing: border-box; }}
body {{
  font-family: ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto, sans-serif;
  max-width: 880px;
  margin: 3rem auto;
  padding: 0 1.5rem;
  line-height: 1.55;
}}
.hero {{
  display: flex;
  align-items: center;
  gap: 1.5rem;
  margin-bottom: 2rem;
}}
.hero img {{
  width: 80px;
  height: 80px;
  flex-shrink: 0;
  border-radius: 16px;
}}
h1 {{
  margin: 0 0 .15rem;
  font-weight: 700;
  letter-spacing: -.02em;
  font-size: 2rem;
}}
.tag {{
  color: #888;
  margin: 0;
}}
.pill {{
  display: inline-block;
  font-size: .75rem;
  font-weight: 600;
  background: var(--bg-card);
  color: inherit;
  padding: .15em .55em;
  border-radius: 999px;
  margin-left: .5rem;
  vertical-align: middle;
  border: 1px solid var(--border);
}}
h2 {{
  margin: 2.25rem 0 .75rem;
  font-size: 1rem;
  text-transform: uppercase;
  letter-spacing: .08em;
  color: #888;
}}
ul {{ padding-left: 1.25rem; margin: 0; }}
ul li + li {{ margin-top: .35rem; }}
code {{
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  background: var(--bg-card);
  padding: .12em .4em;
  border-radius: 4px;
  font-size: .9em;
}}
.cards {{
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(240px, 1fr));
  gap: 1rem;
  margin: .5rem 0 0;
}}
.card {{
  background: var(--bg-card);
  border: 1px solid var(--border);
  padding: 1rem 1.25rem;
  border-radius: 10px;
}}
.card h3 {{
  margin: 0 0 .5rem;
  font-size: .95rem;
  font-weight: 600;
}}
.card ul {{ padding-left: 1.1rem; font-size: .92rem; }}
a {{ color: var(--accent); text-decoration: none; }}
a:hover {{ text-decoration: underline; }}
.foot {{
  color: #999;
  font-size: .85rem;
  margin-top: 3rem;
  padding-top: 1.5rem;
  border-top: 1px solid var(--border);
  text-align: center;
}}
.links {{
  display: flex;
  flex-wrap: wrap;
  gap: 1rem;
  margin: .5rem 0 0;
  font-size: .92rem;
}}
</style>
</head>
<body>
<header class="hero">
  <img src="{icon_url}" alt="rustango">
  <div>
    <h1>rustango is running<span class="pill">v{version}</span></h1>
    <p class="tag">Django-shape Rust web framework — ready to build something.</p>
  </div>
</header>

<h2>Next steps</h2>
<ol>
  <li>Scaffold an app: <code>cargo run -- startapp blog</code></li>
  <li>Define a model in <code>src/blog/models.rs</code> with <code>#[derive(Model)]</code></li>
  <li>Generate + apply migrations: <code>cargo run -- makemigrations &amp;&amp; cargo run -- migrate</code></li>
  <li>Mount your routes in <code>src/urls.rs</code>, then drop <code>.with_welcome()</code> from <code>main.rs</code></li>
</ol>

<h2>Useful commands</h2>
<div class="cards">
  <div class="card">
    <h3>Project</h3>
    <ul>
      <li><code>manage startapp &lt;name&gt;</code></li>
      <li><code>manage make:viewset &lt;Name&gt;</code></li>
      <li><code>manage make:api_routes &lt;app&gt;</code></li>
      <li><code>manage check --deploy</code></li>
    </ul>
  </div>
  <div class="card">
    <h3>Migrations</h3>
    <ul>
      <li><code>manage makemigrations</code></li>
      <li><code>manage migrate</code></li>
      <li><code>manage migrate --squash</code></li>
      <li><code>manage showmigrations</code></li>
    </ul>
  </div>
  <div class="card">
    <h3>Tenancy</h3>
    <ul>
      <li><code>manage init-tenancy</code></li>
      <li><code>manage create-tenant &lt;slug&gt;</code></li>
      <li><code>manage create-operator &lt;email&gt;</code></li>
      <li><code>manage migrate-tenants</code></li>
    </ul>
  </div>
</div>

<h2>Batteries included</h2>
<div class="cards">
  <div class="card">
    <h3>Data</h3>
    <ul>
      <li>ORM with auto-migrations + M2M</li>
      <li>Audit trail + soft-delete</li>
      <li>Multi-tenant: subdomain / path / header / port</li>
      <li>Schema-mode + database-mode tenants</li>
    </ul>
  </div>
  <div class="card">
    <h3>HTTP + UI</h3>
    <ul>
      <li>Auto-admin (Django-shape) + theming</li>
      <li>Class-based views (List/Detail/Create/Update/Delete)</li>
      <li>ViewSets + OpenAPI auto-derive</li>
      <li>Tera templates + CSRF + bulk actions</li>
    </ul>
  </div>
  <div class="card">
    <h3>Auth + ops</h3>
    <ul>
      <li>Sessions, API keys, JWT (access + refresh)</li>
      <li>TOTP / 2FA, password reset, impersonation</li>
      <li>Cache, email, scheduler, signals</li>
      <li>Security headers, rate limiting, CORS</li>
    </ul>
  </div>
</div>

<h2>Where to next</h2>
<div class="links">
  <a href="https://docs.rs/rustango">docs.rs/rustango</a>
  <a href="https://github.com/ujeenet/rustango">GitHub</a>
  <a href="https://github.com/ujeenet/rustango/tree/main/crates/rustango/examples">Examples</a>
  <a href="https://github.com/ujeenet/rustango/blob/main/CHANGELOG.md">Changelog</a>
</div>

<p class="foot">
  Replace this page once you mount your own <code>/</code> route — drop
  <code>.with_welcome()</code> from the <code>Cli::new()</code> chain in <code>src/main.rs</code>.
</p>
</body>
</html>"##
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn welcome_html_contains_version() {
        let html = welcome_html("0.20.30", "/welcome_icon.png");
        assert!(html.contains("0.20.30"));
    }

    #[test]
    fn welcome_html_includes_next_steps() {
        let html = welcome_html("x", "/welcome_icon.png");
        assert!(html.contains("startapp"));
        assert!(html.contains("makemigrations"));
        assert!(html.contains("migrate"));
    }

    #[test]
    fn welcome_html_is_self_contained_no_external_deps() {
        let html = welcome_html("x", "/welcome_icon.png");
        // No cdn references, no external js, fonts use system stack.
        // Outbound links to docs.rs / github.com are fine — those
        // are user-clickable, not loaded resources.
        assert!(!html.contains("cdn."));
        assert!(!html.contains("googleapis"));
        assert!(!html.contains("<script"));
        // v0.30.19 — the icon comes from a sibling route inside
        // welcome_router (also embedded into the binary), so no
        // external static-file pipeline is required, even though
        // the page now uses <img> rather than inline SVG.
        assert!(
            html.contains(r#"<img src="/welcome_icon.png""#),
            "expected hero <img> referencing the sibling PNG route"
        );
        assert!(
            html.contains(r#"<link rel="icon" type="image/png" href="/welcome_icon.png""#),
            "expected favicon <link> pointing at the same PNG"
        );
    }

    /// v0.30.19 — the welcome handler picks the icon URL based on
    /// the actual request path so the page works whether mounted
    /// at `/` (via `Cli::with_welcome()`) or nested at any prefix
    /// (e.g. `Router::nest("/welcome", welcome_router())`). Tests
    /// the URL-shape contract rather than booting axum end-to-end.
    #[test]
    fn welcome_html_icon_url_is_pluggable_for_nested_mounts() {
        // Mounted at /
        let html = welcome_html("x", "/welcome_icon.png");
        assert!(html.contains(r#"href="/welcome_icon.png""#));

        // Nested at /welcome
        let html = welcome_html("x", "/welcome/welcome_icon.png");
        assert!(html.contains(r#"href="/welcome/welcome_icon.png""#));
        assert!(html.contains(r#"src="/welcome/welcome_icon.png""#));

        // Deeply nested (operator stash, etc.)
        let html = welcome_html("x", "/admin/intro/welcome_icon.png");
        assert!(html.contains(r#"src="/admin/intro/welcome_icon.png""#));
    }

    /// v0.30.10 — the polished welcome page demonstrates the v0.30
    /// surface in its commands grid + features grid. Locks in that
    /// the page mentions the modern verbs (`make:viewset`, `migrate
    /// --squash`, `migrate-tenants`) and key feature areas a fresh
    /// developer would want to know exist.
    #[test]
    fn welcome_html_demonstrates_modern_v030_surface() {
        let html = welcome_html("x", "/welcome_icon.png");
        for verb in [
            "make:viewset",
            "make:api_routes",
            "migrate --squash",
            "create-tenant",
            "init-tenancy",
            "check --deploy",
        ] {
            assert!(html.contains(verb), "missing `{verb}` in welcome page");
        }
        for area in [
            "Class-based views",
            "ViewSets",
            "JWT",
            "TOTP / 2FA",
            "soft-delete",
            "Multi-tenant",
        ] {
            assert!(
                html.contains(area),
                "missing feature mention `{area}` in welcome page"
            );
        }
    }

    /// Outbound links are present + look plausibly valid — caught
    /// trailing-slash / typo regressions.
    #[test]
    fn welcome_html_has_outbound_doc_links() {
        let html = welcome_html("x", "/welcome_icon.png");
        for url in [
            "https://docs.rs/rustango",
            "https://github.com/ujeenet/rustango",
        ] {
            assert!(html.contains(url), "missing link `{url}` in welcome page");
        }
    }

    /// The page tells the user how to remove it. Without this, fresh
    /// projects keep the welcome page mounted forever and can't find
    /// the toggle. Regression guard for v0.30.10.
    #[test]
    fn welcome_html_explains_how_to_disable_itself() {
        let html = welcome_html("x", "/welcome_icon.png");
        assert!(
            html.contains(".with_welcome()"),
            "page must reference the Cli builder method users disable"
        );
    }
}
