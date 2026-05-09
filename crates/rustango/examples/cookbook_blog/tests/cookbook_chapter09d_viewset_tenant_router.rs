//! Cookbook Chapter 9d — `ViewSet::tenant_router` for tenancy projects (#80, v0.30).
//!
//! `ViewSet::router(prefix, pool)` bakes a single `PgPool` at mount
//! time — fine for single-tenant projects, broken for multi-tenant
//! ones. Schema-mode tenants share the registry pool but rely on a
//! per-checkout `SET search_path`; database-mode tenants live in
//! entirely separate Postgres databases. Mounting a normal ViewSet
//! against `&pool` from inside a tenant project hits the wrong
//! schema/database on every request.
//!
//! `ViewSet::tenant_router(prefix)` resolves the connection per
//! request via the [`Tenant`] extractor instead. Same builder chain
//! (`filter_fields` / `search_fields` / `ordering` / `page_size` /
//! `permissions_for_model`) as the static `router` path, but no
//! pool argument.
//!
//! Live in-process tests via `tower::ServiceExt::oneshot`. Boots a
//! tenant in `database` storage mode (pointing at the same DB the
//! test connects to — degenerate but sufficient to exercise the
//! resolver-chain + per-request-conn-acquire flow), wraps the
//! tenant_router in the same `Extension(Arc<TenantContext>)` layer
//! `Server::Builder` mounts in production.
//!
//! Run: `DATABASE_URL=... cargo test --test cookbook_chapter09d_viewset_tenant_router -- --test-threads=1`

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::Extension;
use cookbook_blog::apps::blog::models::Author;
use http_body_util::BodyExt;
use rustango::core::Model as _;
use rustango::extractors::TenantContext;
use rustango::migrate as rmig;
use rustango::sql::sqlx;
use rustango::tenancy::{
    self, operator_console::SessionSecret, ChainResolver, HeaderResolver, TenantPools,
};
use rustango::viewset::ViewSet;
use tower::ServiceExt;

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn unique(prefix: &str) -> String {
    let n = UNIQ.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    format!("{prefix}_{pid}_{n}")
}

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

/// Build the test fixture: registry tables + Org row + Author table +
/// `TenantContext` + `tenant_router`-mounted axum::Router. Returns the
/// slug to put in the `x-org` header and the pool for direct seeding.
async fn fixture() -> Option<(String, sqlx::PgPool, axum::Router)> {
    let pool = pool().await?;
    let registry_url = url()?;

    // The cookbook example registers a sprawling set of models via
    // inventory; `rmig::apply_all` would try to wire FKs across all
    // of them and hits ordering issues. Instead, init just the
    // tenancy bootstrap migrations into a tempdir + run them
    // through `migrate_registry` — same shape Chapter 5 uses to set
    // up the registry tables without dragging in cookbook models.
    //
    // Drop the registry tables AND the migration ledger before
    // each fixture build so: (a) state left from sibling tests
    // doesn't conflict with bootstrap CREATE TABLE statements, and
    // (b) the ledger and the actual schema stay in sync —
    // `migrate_registry` is idempotent against the ledger, so a
    // dropped table + populated ledger would silently skip the
    // re-create.
    let _ = rmig::drop_all(&pool).await;
    let _ = sqlx::query(&format!(
        r#"DROP TABLE IF EXISTS "{}" CASCADE"#,
        rmig::LEDGER_TABLE
    ))
    .execute(&pool)
    .await;
    let pools_for_init = TenantPools::new(pool.clone());
    let dir = std::env::temp_dir().join(format!(
        "cookbook_ch9d_{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    tenancy::init_tenancy(&dir).expect("init bootstrap");
    tenancy::migrate_registry(&pools_for_init, &dir)
        .await
        .expect("migrate registry");

    fresh_author_table(&pool).await;

    let slug = unique("acme");
    // Use the public `create_tenant_if_missing` API so we exercise
    // the same path operators run when provisioning. `database`
    // mode points at the same DB the test connects to — degenerate
    // but sufficient for the per-request-conn-acquire flow.
    let opts = tenancy::manage::api::CreateTenantOpts {
        host_pattern: Some(format!("{slug}.app.test")),
        mode: tenancy::StorageMode::Database,
        database_url: Some(registry_url.clone()),
        // Skip the tenant-migration pass: the Author table is set up
        // manually via fresh_author_table since the test only needs
        // that one table on the tenant DB.
        no_migrate: true,
        ..Default::default()
    };
    tenancy::manage::api::create_tenant_if_missing(
        &pools_for_init,
        &registry_url,
        &dir,
        &slug,
        opts,
    )
    .await
    .expect("create_tenant_if_missing");

    let pools = Arc::new(TenantPools::new(pool.clone()));
    let resolver = ChainResolver::new().push(HeaderResolver::default());
    let ctx = Arc::new(TenantContext {
        pools,
        resolver,
        session_secret: SessionSecret::from_bytes(b"a-test-secret-thirty-two-bytes-x".to_vec()),
        operator_secret: SessionSecret::from_bytes(b"a-test-secret-thirty-two-bytes-y".to_vec()),
        registry: pool.clone(),
    });

    // §9.116 / §9.116b — same builder chain that works for the static
    // `router(prefix, pool)` path, minus the pool. v0.30 unification:
    // filter_fields / search_fields / ordering / page_size all carry
    // over to the per-tenant path.
    let vs_router = ViewSet::for_model(Author::SCHEMA)
        .filter_fields(&["name"])
        .search_fields(&["name", "email", "bio"])
        .ordering(&[("id", false)])
        .page_size(2)
        .tenant_router("/api/authors");

    let app = axum::Router::new()
        .merge(vs_router)
        .layer(Extension(ctx));
    Some((slug, pool, app))
}

async fn json_response(
    app: axum::Router,
    method: Method,
    uri: &str,
    slug: &str,
    body: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("x-org", slug);
    let body = match body {
        Some(s) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            Body::from(s.to_owned())
        }
        None => Body::empty(),
    };
    let resp = app.oneshot(req.body(body).unwrap()).await.expect("response");
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value =
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// §9.116 — `tenant_router` mounts the same shape of paginated list
/// endpoint as the static `router(prefix, pool)` path. The
/// `TenantContext` extension drives the per-request connection
/// acquire; the `x-org` header dispatches to the right tenant via
/// the `HeaderResolver`.
#[tokio::test]
async fn tenant_router_lists_paginated_payload() {
    let Some((slug, pool, app)) = fixture().await else {
        return;
    };
    for (n, e) in [
        ("Alice", "alice@x.com"),
        ("Bob", "bob@x.com"),
        ("Carol", "carol@x.com"),
    ] {
        sqlx::query("INSERT INTO cookbook_author (name, email) VALUES ($1, $2)")
            .bind(n)
            .bind(e)
            .execute(&pool)
            .await
            .unwrap();
    }

    let (status, body) = json_response(app, Method::GET, "/api/authors", &slug, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"], serde_json::json!(3));
    assert_eq!(body["page_size"], serde_json::json!(2));
    assert_eq!(body["last_page"], serde_json::json!(2));
    let results = body["results"].as_array().expect("results array");
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["name"], serde_json::json!("Alice"));
    assert_eq!(results[1]["name"], serde_json::json!("Bob"));
}

/// §9.116 — `?search=...` ILIKE narrows. Confirms the v0.30.1
/// CountQuery.search bug fix flows through tenant_router too — the
/// `count` returned matches the visible row count rather than the
/// table total.
#[tokio::test]
async fn tenant_router_search_param_narrows_count_and_results() {
    let Some((slug, pool, app)) = fixture().await else {
        return;
    };
    for (n, e) in [
        ("Alice", "alice@x.com"),
        ("Bob", "bob@x.com"),
        ("Carol", "carol@x.com"),
    ] {
        sqlx::query("INSERT INTO cookbook_author (name, email) VALUES ($1, $2)")
            .bind(n)
            .bind(e)
            .execute(&pool)
            .await
            .unwrap();
    }

    let (status, body) =
        json_response(app, Method::GET, "/api/authors?search=Bob", &slug, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"], serde_json::json!(1));
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["name"], serde_json::json!("Bob"));
}

/// §9.116 — `?{field}=...` exact filter via `filter_fields`. Same
/// Django-style lookups (`__gt`, `__icontains`, `__in`, `__isnull`)
/// the static `router` path supports.
#[tokio::test]
async fn tenant_router_filter_param_exact_match() {
    let Some((slug, pool, app)) = fixture().await else {
        return;
    };
    for (n, e) in [("Alice", "alice@x.com"), ("Bob", "bob@x.com")] {
        sqlx::query("INSERT INTO cookbook_author (name, email) VALUES ($1, $2)")
            .bind(n)
            .bind(e)
            .execute(&pool)
            .await
            .unwrap();
    }

    let (status, body) =
        json_response(app, Method::GET, "/api/authors?name=Alice", &slug, None).await;
    assert_eq!(status, StatusCode::OK);
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["name"], serde_json::json!("Alice"));
}

/// §9.116 — full CRUD round-trip on the per-tenant connection.
/// `POST` creates a row, `GET` retrieves it, `PUT` updates it,
/// `DELETE` destroys it. Each handler resolves its own connection
/// via the `Tenant` extractor; the `x-org` header pins the right
/// tenant for every request.
#[tokio::test]
async fn tenant_router_full_crud_round_trip() {
    let Some((slug, _pool, app)) = fixture().await else {
        return;
    };

    // POST /api/authors — create.
    let payload = r#"{"name": "ada", "email": "ada@example.com", "bio": "first"}"#;
    let (status, body) = json_response(
        app.clone(),
        Method::POST,
        "/api/authors",
        &slug,
        Some(payload),
    )
    .await;
    assert!(
        status == StatusCode::CREATED || status == StatusCode::OK,
        "expected 201 or 200, got {status}"
    );
    let new_id = body["id"].as_i64().expect("returned id");
    assert_eq!(body["name"], serde_json::json!("ada"));

    // GET /api/authors/{id} — retrieve.
    let (status, body) = json_response(
        app.clone(),
        Method::GET,
        &format!("/api/authors/{new_id}"),
        &slug,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], serde_json::json!("ada"));
    assert_eq!(body["bio"], serde_json::json!("first"));

    // PUT /api/authors/{id} — full update.
    let updated = r#"{"name": "ada lovelace", "email": "ada@example.com", "bio": "updated"}"#;
    let (status, body) = json_response(
        app.clone(),
        Method::PUT,
        &format!("/api/authors/{new_id}"),
        &slug,
        Some(updated),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], serde_json::json!("ada lovelace"));
    assert_eq!(body["bio"], serde_json::json!("updated"));

    // DELETE /api/authors/{id} — destroy.
    let (status, _) = json_response(
        app.clone(),
        Method::DELETE,
        &format!("/api/authors/{new_id}"),
        &slug,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Confirm gone via GET → 404.
    let (status, _) = json_response(
        app,
        Method::GET,
        &format!("/api/authors/{new_id}"),
        &slug,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// §9.116 — missing tenant header → the `Tenant` extractor surfaces
/// `TenantRejection::NotFound` as a clean 404 *before* the SQL
/// layer ever runs. Confirms the per-request `acquire` path
/// short-circuits cleanly rather than leaking a 500.
#[tokio::test]
async fn tenant_router_missing_header_yields_404_not_500() {
    let Some((_slug, _pool, app)) = fixture().await else {
        return;
    };
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/authors")
                // intentionally NO x-org header
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
