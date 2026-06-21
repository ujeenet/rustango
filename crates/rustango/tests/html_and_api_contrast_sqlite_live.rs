//! Backing test for `docs/html-views.md` and the API-vs-HTML contrast section
//! of `docs/viewsets.md`. It serves the **same `Post` model two ways at once**:
//!
//!   * `/api/posts`  → a JSON REST API  (`viewset::ViewSet`)       — for clients
//!   * `/posts`      → server-rendered HTML (`template_views::*`)   — for browsers
//!
//! That side-by-side mount is the whole point: one model, one pool, two front
//! doors. In-memory SQLite, no external services.
//!
//! Run: `cargo test -p rustango --features sqlite --test html_and_api_contrast_sqlite_live`

#![cfg(all(feature = "template_views", feature = "sqlite"))]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use rustango::core::Model as _; // brings `Post::SCHEMA` into scope
use rustango::sql::{Auto, Pool};
use rustango::template_views::{CreateView, DetailView, ListView};
use rustango::viewset::ViewSet;
use rustango::Model;
use tera::Tera;
use tower::ServiceExt;

/// The blog post from the docs, the same shape on the API and HTML sides.
#[derive(Model, Debug, Clone)]
#[rustango(table = "posts", display = "title")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    pub body: String,
    #[rustango(max_length = 20, default = "'draft'")]
    pub status: String,
    pub author_id: i64,
    #[rustango(auto_now_add)]
    pub published_at: Auto<DateTime<Utc>>,
}

/// In-memory SQLite with the `posts` table + two rows.
async fn seeded_pool() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    rustango::sql::raw_execute_pool(
        &pool,
        r#"CREATE TABLE posts (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            title        TEXT NOT NULL,
            body         TEXT NOT NULL,
            status       TEXT NOT NULL DEFAULT 'draft',
            author_id    INTEGER NOT NULL,
            published_at TEXT NOT NULL DEFAULT (datetime('now'))
        )"#,
        Vec::new(),
    )
    .await
    .expect("create table");
    rustango::sql::raw_execute_pool(
        &pool,
        "INSERT INTO posts (id, title, body, status, author_id, published_at) VALUES \
         (1, 'Hello Rustango', 'First post body.', 'published', 1, '2026-01-01T00:00:00Z'), \
         (2, 'Second Post',    'More words.',      'published', 1, '2026-01-02T00:00:00Z')",
        Vec::new(),
    )
    .await
    .expect("seed");
    pool
}

/// Templates a browser-facing app needs. The `posts_*` names are the framework
/// defaults for table `posts`.
fn tera() -> Arc<Tera> {
    let mut t = Tera::default();
    t.add_raw_template(
        "posts_list.html",
        // `object_list` + the pagination vars every ListView stamps.
        r#"<h1>Posts ({{ total }})</h1>
        {% for post in object_list %}<article><h2>{{ post.title }}</h2></article>{% endfor %}
        page={{ page }}/{{ total_pages }} has_next={{ has_next }}"#,
    )
    .unwrap();
    t.add_raw_template(
        "posts_detail.html",
        "<h1>{{ object.title }}</h1><p>{{ object.body }}</p>",
    )
    .unwrap();
    t.add_raw_template(
        "posts_form.html",
        // CreateView/UpdateView render this; `is_update` + `errors` are stamped.
        r#"<form method="post">{% if is_update %}edit{% else %}new{% endif %}</form>"#,
    )
    .unwrap();
    Arc::new(t)
}

/// Mount the JSON API and the HTML views over the SAME pool, in one app.
async fn app() -> axum::Router {
    let pool = seeded_pool().await;
    let tera = tera();
    axum::Router::new()
        // The API view — JSON in, JSON out.
        .merge(ViewSet::for_model(Post::SCHEMA).router_pool("/api/posts", pool.clone()))
        // The HTML views — server-rendered pages for a browser.
        .merge(
            ListView::for_model(Post::SCHEMA)
                .order_by("published_at", true)
                .router("/posts", tera.clone(), pool.clone()),
        )
        .merge(DetailView::for_model(Post::SCHEMA).router("/posts", tera.clone(), pool.clone()))
        .merge(
            CreateView::for_model(Post::SCHEMA)
                .success_url("/posts")
                .router("/posts", tera, pool),
        )
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, String, Option<String>) {
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let ctype = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|v| v.to_str().unwrap().to_owned());
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap(), ctype)
}

#[tokio::test]
async fn api_view_returns_json() {
    let (status, body, ctype) = get(&app().await, "/api/posts").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        ctype.unwrap().contains("application/json"),
        "API speaks JSON"
    );
    // Paginated JSON envelope — data, not markup.
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["count"], 2);
    assert!(v["results"].is_array());
}

#[tokio::test]
async fn html_list_view_renders_a_page() {
    let (status, body, ctype) = get(&app().await, "/posts").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        ctype.unwrap().contains("text/html"),
        "HTML view speaks HTML"
    );
    // Rendered markup, not JSON — and the pagination context is present.
    assert!(body.contains("<h1>Posts (2)</h1>"));
    assert!(body.contains("<h2>Hello Rustango</h2>"));
    assert!(body.contains("page=1/1 has_next=false"));
}

#[tokio::test]
async fn html_detail_view_renders_one_row() {
    let app = app().await;
    let (status, body, _) = get(&app, "/posts/1").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("<h1>Hello Rustango</h1>"));
    assert!(body.contains("<p>First post body.</p>"));

    // A missing row is a 404 (browser-shaped not-found).
    let (status, _, _) = get(&app, "/posts/999").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn html_create_view_renders_form_then_redirects_on_post() {
    let app = app().await;

    // GET renders the empty form (is_update = false).
    let (status, body, _) = get(&app, "/posts/new").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(">new<"), "fresh form, got: {body}");

    // POST a urlencoded form → row is inserted, then a 303 redirect to the
    // success_url. (An API client would instead POST JSON and get 201 + the
    // object — that contrast is the API-vs-HTML point.)
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/posts/new")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(
                    "title=Posted+from+a+form&body=hi&status=published&author_id=1",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER); // 303, the PRG pattern
    assert_eq!(resp.headers().get(header::LOCATION).unwrap(), "/posts");

    // The new row shows up in the list (now 3).
    let (_, body, _) = get(&app, "/posts").await;
    assert!(
        body.contains("<h1>Posts (3)</h1>"),
        "list grew, got: {body}"
    );
}
