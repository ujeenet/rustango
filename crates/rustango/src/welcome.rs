//! First-run welcome page. It shows that rustango is wired up.
//!
//! Mount it at `/` while you start out, then replace it with your own
//! root handler.
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

/// Brand image built into the binary. The welcome page loads it from
/// a sibling route, so the page works in a project with no
/// static-file setup at all.
const RUSTANGO_ICON_PNG: &[u8] = include_bytes!("tenancy/static/icon.png");

/// Router serving the welcome page at `/` and its icon at
/// `/welcome_icon.png`. Both are built in, so the page needs no
/// static-file setup.
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
    // This router can sit at `/` or be nested under any prefix.
    // `Router::nest` strips the prefix before the handler runs, so
    // `req.uri().path()` says "/" either way, which builds the wrong
    // icon URL. `OriginalUri` keeps the path the client asked for.
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
    <p class="tag">Batteries-included Rust web framework — ready to build something.</p>
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
      <li>Auto-admin + theming</li>
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
        // No CDN, no external JS, system fonts. Links the user can
        // click are fine; the page must not load anything remote.
        assert!(!html.contains("cdn."));
        assert!(!html.contains("googleapis"));
        assert!(!html.contains("<script"));
        // The icon is a sibling route in the same binary, so the
        // page needs no static-file pipeline.
        assert!(
            html.contains(r#"<img src="/welcome_icon.png""#),
            "expected hero <img> referencing the sibling PNG route"
        );
        assert!(
            html.contains(r#"<link rel="icon" type="image/png" href="/welcome_icon.png""#),
            "expected favicon <link> pointing at the same PNG"
        );
    }

    /// The icon URL follows the request path, so the page works
    /// mounted at `/` or nested under any prefix. This checks the
    /// URL shape without booting axum.
    #[test]
    fn welcome_html_icon_url_is_pluggable_for_nested_mounts() {
        // Mounted at /
        let html = welcome_html("x", "/welcome_icon.png");
        assert!(html.contains(r#"href="/welcome_icon.png""#));

        // Nested at /welcome
        let html = welcome_html("x", "/welcome/welcome_icon.png");
        assert!(html.contains(r#"href="/welcome/welcome_icon.png""#));
        assert!(html.contains(r#"src="/welcome/welcome_icon.png""#));

        // Nested more than one level deep
        let html = welcome_html("x", "/admin/intro/welcome_icon.png");
        assert!(html.contains(r#"src="/admin/intro/welcome_icon.png""#));
    }

    /// The page's command and feature grids must name the current
    /// verbs, so a new developer sees what the framework offers.
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

    /// The outbound links are present and spelled right.
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

    /// The page must say how to remove it. Otherwise a new project
    /// keeps it mounted and cannot find the switch.
    #[test]
    fn welcome_html_explains_how_to_disable_itself() {
        let html = welcome_html("x", "/welcome_icon.png");
        assert!(
            html.contains(".with_welcome()"),
            "page must reference the Cli builder method users disable"
        );
    }
}
