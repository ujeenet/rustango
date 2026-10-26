//! Backing test for `docs/html-views.md` — the "Error pages" section: the
//! router example, the `Message` / `PageError` handlers with `?`, the two
//! example override templates and the `[errors]` settings.
//!
//! Run: `cargo test -p rustango --test html_views_errors_doc`

#![cfg(feature = "template_views")]

use std::sync::Arc;

use axum::body::Body;
use axum::extract::Path;
use axum::http::{header, Request, StatusCode};
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::Router;
use rustango::errors::{ErrorPagesLayer, ErrorPagesRouterExt as _, Message, PageError};
use tower::ServiceExt as _;

struct Project {
    name: String,
}

async fn find_project(slug: &str) -> Result<Option<Project>, std::io::Error> {
    match slug {
        "down" => Err(std::io::Error::other("db: password=hunter2")),
        "acme" => Ok(Some(Project {
            name: "Acme".into(),
        })),
        _ => Ok(None),
    }
}

fn render_project(p: &Project) -> String {
    format!("<h1>{}</h1>", p.name)
}

// The page's handler, verbatim.
async fn project(Path(slug): Path<String>) -> Result<Html<String>, PageError> {
    let project = find_project(&slug)
        .await
        .map_err(PageError::server)? // logged; the page says something generic
        .ok_or_else(|| PageError::not_found("No project with that name."))?;
    Ok(Html(render_project(&project)))
}

async fn archived() -> impl IntoResponse {
    (StatusCode::GONE, Message::new("This project was archived."))
}

/// The two example templates from the page, plus a `base.html` they extend.
fn tera() -> Arc<tera::Tera> {
    let mut tera = rustango::template_extensions::html_tera();
    tera.add_raw_templates([
        (
            "base.html",
            "<html><body>{% block content %}{% endblock content %}</body></html>",
        ),
        (
            "errors/404.html",
            r#"{% extends "base.html" %}
{% block content %}
<section class="error">
  <h1>{{ code }} · {{ title }}</h1>
  <p>{{ message }}</p>
  <p>Nothing at <code>{{ path }}</code>.</p>
  {% if user %}<p>Signed in as {{ user.username }}.</p>{% endif %}
  <a href="/">Back to the start</a>
</section>
{% endblock content %}"#,
        ),
        (
            "errors/5xx.html",
            r#"{% extends "base.html" %}
{% block content %}
<section class="error">
  <h1>{{ title }}</h1>
  <p>{{ message }}</p>
  <form method="post" action="/feedback">{{ csrf_input | safe }}
    <button>Tell us what you were doing</button>
  </form>
</section>
{% endblock content %}"#,
        ),
    ])
    .unwrap();
    Arc::new(tera)
}

fn app() -> Router {
    let tera = tera();
    Router::new()
        .route("/p/{slug}", get(project))
        .route("/archived", get(archived))
        .fallback(rustango::errors::not_found)
        .error_pages(ErrorPagesLayer::new().templates(tera.clone()))
}

async fn browse(path: &str) -> (StatusCode, String) {
    let req = Request::get(path)
        .header(header::ACCEPT, "text/html")
        .body(Body::empty())
        .unwrap();
    let res = app().oneshot(req).await.unwrap();
    let status = res.status();
    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

#[tokio::test]
async fn the_page_examples_render_as_documented() {
    let (status, body) = browse("/p/acme").await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, "<h1>Acme</h1>"));

    let (status, body) = browse("/p/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("404 · Not found") && body.contains("No project with that name."));

    let (status, body) = browse("/nowhere").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body.contains("Nothing at <code>&#x2F;nowhere</code>"),
        "{body}"
    );

    let (status, body) = browse("/p/down").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(body.contains("Something went wrong"), "{body}");
    assert!(
        body.contains(r#"<input type="hidden" name="_csrf""#),
        "{body}"
    );
    assert!(!body.contains("hunter2"));

    // 410 has no override: `errors/4xx.html` and `default.html` are absent.
    let (status, body) = browse("/archived").await;
    assert_eq!(status, StatusCode::GONE);
    assert!(body.contains("<!doctype html>") && body.contains("This project was archived."));
}

#[cfg(feature = "config")]
#[test]
fn the_errors_settings_section_parses_as_documented() {
    let s: rustango::config::Settings = toml::from_str(
        r##"
[errors]
enabled = true            # default false
templates = "templates"   # template root holding errors/404.html etc.; default "templates"
htmx_target = "#flash"    # HX-Retarget for htmx errors; default "#flash", "" sends none
"##,
    )
    .unwrap();
    assert_eq!(s.errors.enabled, Some(true));
    assert_eq!(s.errors.templates.as_deref(), Some("templates"));
    assert!(ErrorPagesLayer::from_settings(&s).is_some());
    assert!(ErrorPagesLayer::from_settings(&rustango::config::Settings::default()).is_none());
}

/// The `Builder` example compiles.
#[cfg(all(feature = "tenancy", feature = "postgres"))]
#[allow(dead_code)]
async fn builder_example() -> Result<(), Box<dyn std::error::Error>> {
    let tera = Arc::new(rustango::template_extensions::html_tera_from_glob(
        "templates/**/*.html",
    )?);
    let _app = rustango::server::Builder::from_env()
        .await?
        .api(Router::new())
        .error_pages(ErrorPagesLayer::new().templates(tera));
    Ok(())
}
