//! ViewSet and template views answer FK / NOT NULL / CHECK refusals with
//! a 409 or 422, never a 500; a bulk create reads back in one select.

#![cfg(all(
    any(feature = "postgres", feature = "mysql", feature = "sqlite"),
    feature = "template_views",
    any(feature = "admin", feature = "tenancy")
))]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use rustango::core::Model as _;
use rustango::sql::{Auto, CounterPool as _, Pool};
use rustango::template_views::{CreateView, DeleteView, ListView, UpdateView};
use rustango::{tri_dialect_test, Model};
use tera::Tera;
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone)]
#[rustango(table = "crf_author", app = "crf")]
pub struct Author {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub name: String,
}

/// A string PK, so the clustered (MySQL) or index (SQLite) order is not the submit order.
#[derive(Model, Debug, Clone)]
#[rustango(table = "crf_tag", app = "crf")]
pub struct Tag {
    #[rustango(primary_key, max_length = 8)]
    pub code: String,
    #[rustango(max_length = 32)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "crf_post", app = "crf")]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(fk = "crf_author", on = "id")]
    pub author_id: i64,
    #[rustango(max_length = 32)]
    pub title: String,
}

const CSRF: &str = "crfcrfc-csrf-token-crfcrfc-csrf-token-crfcr";

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::drop_table(pool, Post::SCHEMA.table).await;
    rustango::testkit::matrix::fresh_table::<Author>(pool).await;
    rustango::testkit::matrix::fresh_table::<Post>(pool).await;
    rustango::testkit::matrix::fresh_table::<Tag>(pool).await;
}

fn app(pool: &Pool) -> axum::Router {
    use rustango::viewset::ViewSet;
    let mut t = Tera::default();
    t.add_raw_templates(vec![
        ("form.html", "form {{ form.errors | json_encode() | safe }}"),
        ("list.html", "rows={{ object_list | length }}"),
        ("detail.html", "author"),
    ])
    .unwrap();
    let t = Arc::new(t);
    ViewSet::for_model(Author::SCHEMA, &["id", "name"])
        .router_pool("/api/authors", pool.clone())
        .merge(ViewSet::for_model(Tag::SCHEMA, &["code", "name"]).router_pool("/api/tags", pool.clone()))
        .merge(
            DeleteView::for_model(Author::SCHEMA)
                .template("detail.html")
                .success_url("/authors")
                .router("/authors", t.clone(), pool.clone()),
        )
        .merge(
            ListView::for_model(Author::SCHEMA)
                .template("list.html")
                .bulk_actions(true)
                .router("/authors", t.clone(), pool.clone()),
        )
        .merge(
            CreateView::for_model(Post::SCHEMA, &["author_id", "title"])
                .template("form.html")
                .success_url("/posts")
                .router("/posts", t.clone(), pool.clone()),
        )
        .merge(
            UpdateView::for_model(Post::SCHEMA, &["author_id", "title"])
                .template("form.html")
                .success_url("/posts")
                .router("/posts", t, pool.clone()),
        )
}

async fn send(pool: &Pool, method: Method, uri: &str, body: &str) -> (StatusCode, String) {
    let resp = app(pool)
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, format!("rustango_csrf={CSRF}"))
                .body(Body::from(format!("_csrf={CSRF}&{body}")))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn seed(pool: &Pool) -> (i64, i64) {
    let mut a = Author {
        id: Auto::default(),
        name: "a".into(),
    };
    a.insert_pool(pool).await.expect("author");
    let a = *a.id.get().expect("pk");
    let mut p = Post {
        id: Auto::default(),
        author_id: a,
        title: "t".into(),
    };
    p.insert_pool(pool).await.expect("post");
    (a, *p.id.get().expect("pk"))
}

fn assert_no_driver_text(body: &str) {
    for raw in ["violates", "constraint", "FOREIGN KEY", "crf_post"] {
        assert!(!body.contains(raw), "driver text `{raw}` leaked: {body}");
    }
}

/// Deleting a referenced row is a 409 from every delete path; the row stays.
async fn deleting_a_referenced_row_is_a_409(pool: &Pool) {
    let (a, _) = seed(pool).await;
    for (method, uri, form) in [
        (Method::DELETE, format!("/api/authors/{a}"), String::new()),
        (Method::POST, format!("/authors/{a}/delete"), String::new()),
        (
            Method::POST,
            "/authors".to_owned(),
            format!("action=delete_selected&_selected_action={a}"),
        ),
    ] {
        let (status, body) = send(pool, method, &uri, &form).await;
        assert_eq!(status, StatusCode::CONFLICT, "{uri}: {body}");
        assert_no_driver_text(&body);
        assert_eq!(Author::objects().count(pool).await.expect("count"), 1);
    }
}

/// A missing FK target on create or update is a 422 form error.
async fn a_bad_fk_is_a_form_error(pool: &Pool) {
    let (_, p) = seed(pool).await;
    for uri in ["/posts/new".to_owned(), format!("/posts/{p}/edit")] {
        let (status, body) = send(pool, Method::POST, &uri, "author_id=9999&title=x").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{uri}: {body}");
        assert!(body.contains("__all__"), "{uri}: {body}");
        assert_no_driver_text(&body);
    }
}

/// Queries and response of one JSON bulk create.
async fn bulk_create(pool: &Pool, names: &[&str]) -> (StatusCode, String, usize) {
    let rows: Vec<_> = names
        .iter()
        .map(|n| serde_json::json!({ "name": n }))
        .collect();
    bulk_post(pool, "/api/authors", &rows).await
}

async fn bulk_post(
    pool: &Pool,
    uri: &str,
    rows: &[serde_json::Value],
) -> (StatusCode, String, usize) {
    use rustango::test_assertions::QueryCounter;
    let body = serde_json::to_string(rows).unwrap();
    QueryCounter::scope(async {
        let resp = app(pool)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(uri)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        (status, text, QueryCounter::current())
    })
    .await
}

/// A bulk create reads its rows back in one select, in order.
async fn bulk_create_reads_back_in_one_select(pool: &Pool) {
    let (s1, _, one) = bulk_create(pool, &["a"]).await;
    let (s5, body, five) = bulk_create(pool, &["b", "c", "d", "e", "f"]).await;
    assert_eq!(
        (s1, s5),
        (StatusCode::CREATED, StatusCode::CREATED),
        "{body}"
    );
    let names: Vec<String> = serde_json::from_str::<Vec<serde_json::Value>>(&body)
        .expect("JSON array")
        .iter()
        .map(|r| r["name"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(names, ["b", "c", "d", "e", "f"]);
    // Only the four extra INSERTs may add queries.
    assert_eq!(five - one, 4, "read-back scales with the rows");

    // String PKs submitted out of their sort order come back as submitted.
    let codes = ["m", "c", "x", "a"];
    let rows: Vec<_> = codes
        .iter()
        .map(|c| serde_json::json!({ "code": c, "name": format!("n-{c}") }))
        .collect();
    let (status, body, _) = bulk_post(pool, "/api/tags", &rows).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let back: Vec<serde_json::Value> = serde_json::from_str(&body).expect("JSON array");
    let got: Vec<(&str, &str)> = back
        .iter()
        .map(|r| {
            (
                r["code"].as_str().unwrap_or("?"),
                r["name"].as_str().unwrap_or("?"),
            )
        })
        .collect();
    assert_eq!(
        got,
        [("m", "n-m"), ("c", "n-c"), ("x", "n-x"), ("a", "n-a")],
        "{body}"
    );
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        deleting_a_referenced_row_is_a_409,
        a_bad_fk_is_a_form_error,
        bulk_create_reads_back_in_one_select,
    ],
}
