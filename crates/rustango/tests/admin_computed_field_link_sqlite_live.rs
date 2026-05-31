//! Django-parity #349 — `register_admin_computed!` with `link = …`
//! callable advertises a per-row click target. The admin list view
//! wraps the rendered cell in `<a href="{url}">…</a>` when the
//! callable returns `Some(url)`, and leaves it alone otherwise.
//!
//! Sibling of `admin_list_display_links_sqlite_live.rs`: that test
//! covers the container-level `list_display_links` whitelist, this
//! one covers the per-callable override. The two interact in a
//! predictable way — the callable's URL wins over the detail-href
//! wrap when both are eligible for the same cell (the callable
//! knows where THIS specific cell should jump; the container fallback
//! only knows the row's detail page).

#![cfg(all(feature = "sqlite", feature = "admin", feature = "tenancy"))]

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use http_body_util::BodyExt;
use rustango::sql::Pool;
use rustango::Model;
use tower::ServiceExt;

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "cfl_post",
    display = "title",
    admin(list_display = "title, author_link, plain_summary", ordering = "-id",)
)]
pub struct CflPost {
    #[rustango(primary_key)]
    id: rustango::Auto<i64>,
    #[rustango(max_length = 200)]
    title: String,
    author_id: i64,
}

// Computed field WITH link callable — should auto-link to author detail.
rustango::register_admin_computed!(
    "cfl_post",
    "author_link",
    "Author",
    |row| {
        let id = row
            .get("author_id")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        format!("user-{id}")
    },
    link = |row| {
        row.get("author_id")
            .and_then(serde_json::Value::as_i64)
            .map(|id| format!("/auth_user/{id}"))
    },
);

// Computed field WITHOUT link callable — should render plain text,
// no wrapping, no detail-href (table isn't in list_display_links).
rustango::register_admin_computed!("cfl_post", "plain_summary", "Summary", |row| {
    let t = row
        .get("title")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    format!("re: {t}")
});

fn build_app(pool: Pool) -> axum::Router {
    rustango::admin::Builder::new(pool).admin_prefix("").build()
}

async fn pool_with_post() -> (Pool, String) {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    rustango::sql::raw_execute_pool(
        &pool,
        r#"CREATE TABLE IF NOT EXISTS "cfl_post" (
            "id"        INTEGER PRIMARY KEY AUTOINCREMENT,
            "title"     TEXT NOT NULL,
            "author_id" INTEGER NOT NULL
        )"#,
        Vec::new(),
    )
    .await
    .expect("create");
    let app = build_app(pool.clone());
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/cfl_post")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("title=Hello&author_id=7"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        resp.status() == StatusCode::SEE_OTHER || resp.status() == StatusCode::OK,
        "seed POST failed: {}",
        resp.status()
    );
    (pool, "/cfl_post".into())
}

async fn fetch_body(pool: Pool, uri: &str) -> String {
    let app = build_app(pool);
    let resp = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn computed_with_link_wraps_in_anchor_to_callable_url() {
    let (pool, uri) = pool_with_post().await;
    let body = fetch_body(pool, &uri).await;
    // The author_link computed field's link callable returned
    // `/auth_user/7` (escaped — the slash stays, the `/` doesn't
    // need encoding inside an href but it's pumped through
    // `render::escape` which only HTML-encodes & < > " ' / and `/`
    // round-trips).
    assert!(
        body.contains(r#"<a href="/auth_user/7">user-7</a>"#),
        "author_link cell should wrap inner in <a> using callable URL, got: {body}"
    );
}

#[tokio::test]
async fn computed_without_link_renders_plain() {
    let (pool, uri) = pool_with_post().await;
    let body = fetch_body(pool, &uri).await;
    // plain_summary has no link callable AND no entry in
    // list_display_links — must appear bare, never wrapped.
    assert!(
        body.contains("re: Hello"),
        "plain_summary cell should be present, got: {body}"
    );
    // Make sure the unsupported pattern doesn't appear — the inner
    // string never gets an `<a>` around it. Capture a small window
    // by searching for an anchor that contains "re: Hello".
    assert!(
        !body.contains(r#">re: Hello</a>"#),
        "plain_summary must not be wrapped in <a>, got: {body}"
    );
}
