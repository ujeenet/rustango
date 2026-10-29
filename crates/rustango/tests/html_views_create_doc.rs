//! Backing test for `docs/html-views.md` — the CreateView example and its
//! `posts_form.html`, read from every locale: the documented field list
//! can insert the docs' `Post`, and a write the database refuses is shown.
//!
//! Run: `cargo test -p rustango --features sqlite --test html_views_create_doc`

#![cfg(all(feature = "template_views", feature = "sqlite"))]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use rustango::core::Model as _;
use rustango::sql::{Auto, Pool};
use rustango::template_views::CreateView;
use rustango::Model;
use tower::ServiceExt as _;

/// The getting-started `Post`: `author_id` is NOT NULL with no default.
#[derive(Model, Debug, Clone)]
#[rustango(table = "posts")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    pub body: String,
    pub author_id: i64,
}

const CSRF: &str = "html-create-doc-csrf-token-html-create-doc";

fn page(locale: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs")
        .join(locale)
        .join("html-views.md");
    std::fs::read_to_string(path).unwrap()
}

/// The field list of the first `CreateView::for_model(Post::SCHEMA, &[...])`.
fn create_fields(md: &str) -> Vec<&str> {
    let start = md
        .find("CreateView::for_model(Post::SCHEMA, &[")
        .expect("a CreateView example")
        + "CreateView::for_model(Post::SCHEMA, &[".len();
    let list = &md[start..start + md[start..].find(']').unwrap()];
    list.split(',')
        .map(|f| f.trim().trim_matches('"'))
        .filter(|f| !f.is_empty())
        .collect()
}

/// The ```html block that renders `form.fields`: the documented form template.
fn form_template(md: &str) -> String {
    md.split("```html\n")
        .skip(1)
        .map(|b| &b[..b.find("```").unwrap()])
        .find(|b| b.contains("form.fields"))
        .expect("the form template")
        .to_owned()
}

/// `author_id > 0`, so the database refuses a write the form accepts.
async fn pool() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.unwrap();
    rustango::sql::raw_execute_pool(
        &pool,
        "CREATE TABLE posts (id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT NOT NULL, \
         body TEXT NOT NULL, author_id INTEGER NOT NULL CHECK (author_id > 0))",
        Vec::new(),
    )
    .await
    .unwrap();
    pool
}

async fn post(app: &axum::Router, form: &str) -> (StatusCode, String) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/posts/new")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, format!("rustango_csrf={CSRF}"))
                .body(Body::from(format!("_csrf={CSRF}&{form}")))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
async fn the_documented_create_view_inserts_and_shows_a_refusal() {
    for locale in ["", "de", "es", "fr"] {
        let md = page(locale);
        let fields = create_fields(&md);
        assert!(
            fields.contains(&"author_id"),
            "{locale}: the docs' Post needs author_id: {fields:?}"
        );
        let mut tera = rustango::template_extensions::html_tera();
        tera.add_raw_template("posts_form.html", &form_template(&md))
            .unwrap();
        let app = CreateView::for_model(Post::SCHEMA, &fields)
            .success_url("/posts")
            .router("/posts", Arc::new(tera), pool().await);

        let (status, body) = post(&app, "title=Hello&body=b&author_id=1").await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{locale}: {body}");

        let (status, body) = post(&app, "title=Hello&body=b&author_id=0").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{locale}: {body}");
        assert!(
            body.contains("class=\"error\""),
            "{locale}: the refusal is not shown: {body}"
        );
    }
}
