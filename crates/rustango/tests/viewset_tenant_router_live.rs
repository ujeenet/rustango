#![cfg(all(feature = "tenancy", feature = "postgres"))]
//! Live integration test for `ViewSet::tenant_router(prefix)` (#80, v0.30).
//!
//! Boots a single tenant in database storage mode (pointing at the same
//! DB the test connects to — degenerate but sufficient to exercise the
//! resolver-chain + per-request-conn-acquire flow), wraps a
//! `tenant_router` with the same `Extension(Arc<TenantContext>)` layer
//! `Server::Builder` mounts in production, and round-trips every CRUD
//! verb via `tower::ServiceExt::oneshot`.
//!
//! The test asserts the v0.30 unification — every builder knob that
//! works for the static `router(prefix, pool)` path also works for
//! `tenant_router(prefix)` — by configuring `filter_fields`,
//! `search_fields`, `ordering`, and `page_size` and exercising each.
//!
//! Reads `DATABASE_URL`. Skips silently when unset.
//!
//! Run: `DATABASE_URL=... cargo test --test viewset_tenant_router_live -- --test-threads=1`

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{header, Method, Request, StatusCode};
use axum::Extension;
use rustango::core::Model as _;
use rustango::extractors::TenantContext;
use rustango::sql::{sqlx, Auto};
use rustango::tenancy::{
    session::SessionSecret, ChainResolver, HeaderResolver, Org, StorageMode, TenantPools,
};
use rustango::viewset::ViewSet;
use rustango::{migrate as rmig, Model};
use tower::ServiceExt;

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn unique(prefix: &str) -> String {
    let n = UNIQ.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    format!("{prefix}_{pid}_{n}")
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    Some(sqlx::PgPool::connect(&url).await.unwrap())
}

fn now() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}

/// The test model — single string field so list/retrieve/create/update/
/// delete + filter/search are all exercisable. `ten_vs_widget` table
/// name avoids any collision with other live-test fixtures.
#[derive(Model, Debug, Clone)]
#[rustango(table = "ten_vs_widget", display = "label")]
#[allow(dead_code)]
pub struct Widget {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub label: String,
}

async fn fresh_widget_table(pool: &sqlx::PgPool) {
    sqlx::query(r#"DROP TABLE IF EXISTS ten_vs_widget CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        r#"CREATE TABLE ten_vs_widget (
            id    BIGSERIAL PRIMARY KEY,
            label VARCHAR(64) NOT NULL
        )"#,
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn body_string(resp: axum::response::Response) -> String {
    let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let s = body_string(resp).await;
    serde_json::from_str(&s).unwrap_or_else(|_| panic!("non-JSON response: {s}"))
}

/// Build the test fixture: registry tables + Org row + Widget table +
/// TenantContext + tenant_router-mounted axum::Router. Returns the slug
/// to put in the `x-org` header and the pool for direct seeding.
async fn fixture(pool: sqlx::PgPool) -> (String, sqlx::PgPool, axum::Router) {
    let url = std::env::var("DATABASE_URL").unwrap();

    // Registry tables (rustango_orgs etc).
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();
    fresh_widget_table(&pool).await;

    // Single tenant in database mode pointing at this same DB. This
    // avoids the schema-mode `SET search_path` setup the test would
    // otherwise need to manage manually — and the per-request conn
    // path is exercised the same way for either mode.
    let slug = unique("vstest");
    let mut org = Org {
        id: Auto::default(),
        slug: slug.clone(),
        display_name: "VS Test".into(),
        storage_mode: StorageMode::Database.as_str().into(),
        backend_kind: "postgres".to_owned(),
        database_url: Some(url.clone()),
        schema_name: None,
        host_pattern: Some(format!("{slug}.app.test")),
        port: None,
        path_prefix: None,
        active: true,
        created_at: now(),
        brand_name: None,
        brand_tagline: None,
        logo_path: None,
        favicon_path: None,
        primary_color: None,
        theme_mode: None,
    };
    org.insert(&pool).await.unwrap();

    // Header-based resolver so the test doesn't need DNS — same shape
    // tenant_admin_live.rs uses.
    let pools = Arc::new(TenantPools::new(pool.clone()));
    let resolver = ChainResolver::new().push(HeaderResolver::default());
    let ctx = Arc::new(TenantContext {
        pools,
        resolver,
        session_secret: SessionSecret::from_bytes(b"a-test-secret-thirty-two-bytes-x".to_vec()),
        operator_secret: SessionSecret::from_bytes(b"a-test-secret-thirty-two-bytes-y".to_vec()),
    });
    let _ = &pool; // silence unused-warning if the test no longer needs the raw pool

    // The full v0.30 builder chain — unsupported in the v1
    // tenant_router. If any of these silently no-op, the assertions
    // below would catch it.
    let vs_router = ViewSet::for_model(Widget::SCHEMA)
        .filter_fields(&["label"])
        .search_fields(&["label"])
        .ordering(&[("id", false)])
        .page_size(2)
        .tenant_router("/api/widgets");

    let app = axum::Router::new().merge(vs_router).layer(Extension(ctx));
    (slug, pool, app)
}

fn req_get(uri: &str, slug: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header("x-org", slug)
        .body(Body::empty())
        .unwrap()
}

fn req_json(method: Method, uri: &str, slug: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("x-org", slug)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn list_returns_paginated_payload_against_tenant_conn() {
    let Some(pool) = pool().await else { return };
    let (slug, pool, app) = fixture(pool).await;

    for label in ["alpha", "beta", "gamma"] {
        sqlx::query("INSERT INTO ten_vs_widget (label) VALUES ($1)")
            .bind(label)
            .execute(&pool)
            .await
            .unwrap();
    }

    let resp = app
        .clone()
        .oneshot(req_get("/api/widgets", &slug))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    // page_size = 2 → first 2 rows of 3.
    assert_eq!(v["count"], serde_json::json!(3));
    assert_eq!(v["page"], serde_json::json!(1));
    assert_eq!(v["page_size"], serde_json::json!(2));
    assert_eq!(v["last_page"], serde_json::json!(2));
    let results = v["results"].as_array().expect("results array");
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["label"], serde_json::json!("alpha"));
    assert_eq!(results[1]["label"], serde_json::json!("beta"));
}

#[tokio::test]
async fn search_param_narrows_via_tenant_router() {
    let Some(pool) = pool().await else { return };
    let (slug, pool, app) = fixture(pool).await;

    for label in ["alpha", "beta", "gamma"] {
        sqlx::query("INSERT INTO ten_vs_widget (label) VALUES ($1)")
            .bind(label)
            .execute(&pool)
            .await
            .unwrap();
    }

    let resp = app
        .oneshot(req_get("/api/widgets?search=bet", &slug))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["count"], serde_json::json!(1));
    let results = v["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["label"], serde_json::json!("beta"));
}

#[tokio::test]
async fn filter_param_exact_match_via_tenant_router() {
    let Some(pool) = pool().await else { return };
    let (slug, pool, app) = fixture(pool).await;

    for label in ["alpha", "beta"] {
        sqlx::query("INSERT INTO ten_vs_widget (label) VALUES ($1)")
            .bind(label)
            .execute(&pool)
            .await
            .unwrap();
    }

    let resp = app
        .oneshot(req_get("/api/widgets?label=alpha", &slug))
        .await
        .unwrap();
    let v = body_json(resp).await;
    let results = v["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["label"], serde_json::json!("alpha"));
}

#[tokio::test]
async fn retrieve_by_pk_via_tenant_router() {
    let Some(pool) = pool().await else { return };
    let (slug, pool, app) = fixture(pool).await;

    let id: i64 = sqlx::query_scalar("INSERT INTO ten_vs_widget (label) VALUES ($1) RETURNING id")
        .bind("solo")
        .fetch_one(&pool)
        .await
        .unwrap();

    let resp = app
        .oneshot(req_get(&format!("/api/widgets/{id}"), &slug))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["id"], serde_json::json!(id));
    assert_eq!(v["label"], serde_json::json!("solo"));
}

#[tokio::test]
async fn create_then_retrieve_via_tenant_router() {
    let Some(pool) = pool().await else { return };
    let (slug, pool, app) = fixture(pool).await;

    let resp = app
        .clone()
        .oneshot(req_json(
            Method::POST,
            "/api/widgets",
            &slug,
            serde_json::json!({ "label": "fresh" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let v = body_json(resp).await;
    let new_id = v["id"].as_i64().expect("returned id");
    assert_eq!(v["label"], serde_json::json!("fresh"));

    // Verify the row landed via direct SQL.
    let label: String = sqlx::query_scalar("SELECT label FROM ten_vs_widget WHERE id = $1")
        .bind(new_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(label, "fresh");
}

#[tokio::test]
async fn update_then_destroy_via_tenant_router() {
    let Some(pool) = pool().await else { return };
    let (slug, pool, app) = fixture(pool).await;

    let id: i64 = sqlx::query_scalar("INSERT INTO ten_vs_widget (label) VALUES ($1) RETURNING id")
        .bind("before")
        .fetch_one(&pool)
        .await
        .unwrap();

    // PUT → full update.
    let resp = app
        .clone()
        .oneshot(req_json(
            Method::PUT,
            &format!("/api/widgets/{id}"),
            &slug,
            serde_json::json!({ "label": "after" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["label"], serde_json::json!("after"));

    // DELETE.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri(format!("/api/widgets/{id}"))
                .header("x-org", &slug)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Confirm gone.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ten_vs_widget WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

/// Tenant resolver miss → the extractor returns 404 before any handler
/// runs. Confirms the `acquire(...)` path surfaces TenantRejection
/// cleanly rather than leaking a 500 from the inner SQL layer.
#[tokio::test]
async fn missing_tenant_header_yields_404() {
    let Some(pool) = pool().await else { return };
    let (_slug, _pool, app) = fixture(pool).await;

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/widgets")
                // intentionally NO x-org header
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
