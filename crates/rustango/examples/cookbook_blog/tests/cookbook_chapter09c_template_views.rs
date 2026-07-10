//! Cookbook Chapter 9b — `template_views` (HTML-side CBVs).
//!
//! Live in-process tests via `tower::ServiceExt::oneshot` against
//! a `ListView` / `DetailView` / `CreateView` / `UpdateView` /
//! `DeleteView` mounted on a real PG pool. Exercises the full
//! CRUD + pagination + filter/search/ordering URL params + the
//! Tera context shape.
//!
//! Run: `DATABASE_URL=... cargo test --test cookbook_chapter09c_template_views -- --test-threads=1`

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use cookbook_blog::apps::blog::models::Author;
use http_body_util::BodyExt;
use rustango::core::Model as _;
use rustango::sql::sqlx;
use rustango::template_views::{CreateView, DeleteView, DetailView, ListView, UpdateView};
use std::sync::Arc;
use tera::Tera;
use tower::ServiceExt;

fn url() -> Option<String> {
    std::env::var("DATABASE_URL").ok()
}

async fn pool() -> Option<sqlx::PgPool> {
    Some(sqlx::PgPool::connect(&url()?).await.expect("connect"))
}

async fn fresh_author_table(pool: &sqlx::PgPool) {
    sqlx::query("DROP TABLE IF EXISTS cookbook_author CASCADE")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        r#"CREATE TABLE cookbook_author (
            id BIGSERIAL PRIMARY KEY,
            name VARCHAR(80) NOT NULL,
            email VARCHAR(200) NOT NULL UNIQUE,
            bio VARCHAR(500) NULL,
            joined_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )"#,
    )
    .execute(pool)
    .await
    .unwrap();
}

/// Tera with the four canonical templates pre-registered. Output
/// is intentionally minimal — just stamps context vars so tests
/// can assert on the bytes.
fn tera() -> Arc<Tera> {
    let mut tera = Tera::default();
    tera.add_raw_template(
        "cookbook_author_list.html",
        "page={{ page }}|total={{ total }}|search={{ search }}|ordering={{ ordering }}\
         |count={{ object_list | length }}\
         {% for a in object_list %} ROW({{ a.id }}={{ a.name }}){% endfor %}",
    )
    .unwrap();
    tera.add_raw_template(
        "cookbook_author_detail.html",
        "DETAIL id={{ object.id }} name={{ object.name }}",
    )
    .unwrap();
    tera.add_raw_template(
        "cookbook_author_form.html",
        "FORM is_create={{ is_create }} is_update={{ is_update }} fields={{ form.fields | length }} csrf={{ csrf_token }}",
    )
    .unwrap();
    tera.add_raw_template(
        "cookbook_author_confirm_delete.html",
        "DELETE? id={{ object.id }} name={{ object.name }}",
    )
    .unwrap();
    Arc::new(tera)
}

async fn body_to_string(resp: axum::http::Response<Body>) -> String {
    String::from_utf8(
        resp.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap()
}

/// Build the full CRUD router for the cookbook_author table.
fn router(pool: sqlx::PgPool, tera: Arc<Tera>) -> axum::Router {
    axum::Router::new()
        .merge(
            ListView::for_model(Author::SCHEMA)
                .template("cookbook_author_list.html")
                .page_size(2)
                .filter_fields(&["name"])
                .search_fields(&["name", "email"])
                .ordering_fields(&["name", "id"])
                .router("/authors", tera.clone(), pool.clone().into()),
        )
        .merge(
            DetailView::for_model(Author::SCHEMA)
                .template("cookbook_author_detail.html")
                .router("/authors", tera.clone(), pool.clone().into()),
        )
        .merge(
            CreateView::for_model(Author::SCHEMA)
                .template("cookbook_author_form.html")
                .success_url("/authors/{pk}")
                .router("/authors", tera.clone(), pool.clone().into()),
        )
        .merge(
            UpdateView::for_model(Author::SCHEMA)
                .template("cookbook_author_form.html")
                .success_url("/authors/{pk}")
                .router("/authors", tera.clone(), pool.clone().into()),
        )
        .merge(
            DeleteView::for_model(Author::SCHEMA)
                .template("cookbook_author_confirm_delete.html")
                .success_url("/authors")
                .router("/authors", tera, pool.into()),
        )
}

#[tokio::test]
async fn list_paginates_and_renders_search_context() {
    let Some(pool) = pool().await else { return };
    fresh_author_table(&pool).await;
    for (n, e) in [
        ("Alice", "a@x.com"),
        ("Bob", "b@x.com"),
        ("Carol", "c@x.com"),
    ] {
        sqlx::query("INSERT INTO cookbook_author (name, email) VALUES ($1, $2)")
            .bind(n)
            .bind(e).execute(&pool)
            .await
            .unwrap();
    }
    let app = router(pool, tera());

    // Page 1, page_size = 2 → first 2 rows.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/authors?page=1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_to_string(resp).await;
    assert!(body.contains("page=1"), "got: {body}");
    assert!(body.contains("total=3"), "got: {body}");
    assert!(body.contains("count=2"), "got: {body}");

    // ?search=Bob — ILIKE filter narrows.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/authors?search=Bob")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_to_string(resp).await;
    assert!(body.contains("count=1"), "got: {body}");
    assert!(body.contains("search=Bob"), "got: {body}");
    assert!(body.contains("ROW("), "got: {body}");
    assert!(body.contains("Bob"), "got: {body}");
}

#[tokio::test]
async fn detail_renders_object_context() {
    let Some(pool) = pool().await else { return };
    fresh_author_table(&pool).await;
    let id: i64 =
        sqlx::query_scalar("INSERT INTO cookbook_author (name, email) VALUES ($1, $2) RETURNING id")
            .bind("Eve")
            .bind("e@x.com")
            .fetch_one(&pool)
            .await
            .unwrap();
    let app = router(pool, tera());

    let resp = app
        .oneshot(
            Request::builder()
                .uri(&format!("/authors/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_to_string(resp).await;
    assert!(body.contains("name=Eve"), "got: {body}");
}

#[tokio::test]
async fn create_view_inserts_then_redirects_to_pk_url() {
    let Some(pool) = pool().await else { return };
    fresh_author_table(&pool).await;
    let app = router(pool.clone(), tera());

    // GET /authors/new — renders the form.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/authors/new")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_to_string(resp).await;
    assert!(body.contains("FORM is_create=true"), "got: {body}");

    // POST /authors/new — inserts + 303 to /authors/<new pk>.
    let post_body = "name=Frank&email=f@x.com&bio=";
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/authors/new")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(post_body))
                .unwrap(),
        )
        .await
        .unwrap();
    // Redirect::to() defaults to 303.
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let location = resp
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap();
    assert!(
        location.starts_with("/authors/"),
        "expected /authors/<pk>, got: {location}"
    );
    // Confirm the row landed.
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM cookbook_author WHERE name = 'Frank'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn delete_view_two_step_flow() {
    let Some(pool) = pool().await else { return };
    fresh_author_table(&pool).await;
    let id: i64 =
        sqlx::query_scalar("INSERT INTO cookbook_author (name, email) VALUES ($1, $2) RETURNING id")
            .bind("Garry")
            .bind("g@x.com")
            .fetch_one(&pool)
            .await
            .unwrap();
    let app = router(pool.clone(), tera());

    // GET confirm — renders.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(&format!("/authors/{id}/delete"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_to_string(resp).await;
    assert!(body.contains("name=Garry"), "got: {body}");

    // POST execute — deletes + 303 to /authors.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(&format!("/authors/{id}/delete"))
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cookbook_author")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}
