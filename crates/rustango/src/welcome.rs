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

/// Inline SVG mark used as the welcome page's logo. Self-contained
/// (no external file dependency, no `<img>` to fetch) so the page
/// renders correctly even when no static-file router is mounted.
/// Geometric "R" mark in two tones — rust-orange + tango-blue —
/// that scales cleanly from favicon size up to hero size.
const RUSTANGO_LOGO_SVG: &str = r##"<svg viewBox="0 0 96 96" xmlns="http://www.w3.org/2000/svg" aria-label="rustango">
  <defs>
    <linearGradient id="g" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0%" stop-color="#e07a3a"/>
      <stop offset="100%" stop-color="#3672e0"/>
    </linearGradient>
  </defs>
  <rect x="4" y="4" width="88" height="88" rx="20" fill="url(#g)"/>
  <path d="M28 22 h22 a18 18 0 0 1 18 18 v2 a18 18 0 0 1 -12 17 l14 17 h-12 l-13 -16 h-7 v16 h-10 z m10 10 v16 h12 a8 8 0 0 0 8 -8 v0 a8 8 0 0 0 -8 -8 z"
        fill="white"/>
</svg>"##;

fn welcome_html(version: &str) -> String {
    format!(
        r##"<!doctype html>
<html lang="en">
<head>
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
.hero svg {{
  width: 80px;
  height: 80px;
  flex-shrink: 0;
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
  {RUSTANGO_LOGO_SVG}
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
        // No cdn references, no external js, fonts use system stack.
        // Outbound links to docs.rs / github.com are fine — those
        // are user-clickable, not loaded resources.
        assert!(!html.contains("cdn."));
        assert!(!html.contains("googleapis"));
        assert!(!html.contains("<script"));
        // Inline SVG rather than <img src=...> means no asset
        // pipeline / static-file mount required.
        assert!(html.contains("<svg"));
        assert!(!html.contains("<img"));
    }

    /// v0.30.10 — the polished welcome page demonstrates the v0.30
    /// surface in its commands grid + features grid. Locks in that
    /// the page mentions the modern verbs (`make:viewset`, `migrate
    /// --squash`, `migrate-tenants`) and key feature areas a fresh
    /// developer would want to know exist.
    #[test]
    fn welcome_html_demonstrates_modern_v030_surface() {
        let html = welcome_html("x");
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
        let html = welcome_html("x");
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
        let html = welcome_html("x");
        assert!(
            html.contains(".with_welcome()"),
            "page must reference the Cli builder method users disable"
        );
    }
}
