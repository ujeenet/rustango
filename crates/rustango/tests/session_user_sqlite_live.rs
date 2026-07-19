//! End-to-end live test for the `SessionUser` extractor on SQLite
//! (rustango#317).
//!
//! Before the fix, `crate::extractors::session_user` was gated
//! behind `#[cfg(feature = "postgres")]` because its inner
//! User-fetch ran through `pools.acquire(&org)` + `fetch_on(&mut **conn)`,
//! a PG-specific path. The tri-dialect lift swapped that for
//! `scoped_pool_dyn` + `fetch`, matching `SessionOperator`'s
//! shape and unblocking sqlite/mysql tenancy builds.
//!
//! This test proves the new path compiles + runs end-to-end on
//! SQLite by:
//!
//! 1. Building a `TenantContext<sqlx::Sqlite>` with an in-memory
//!    registry pool and a fixed resolver.
//! 2. Mounting an axum router with a handler that takes `SessionUser`.
//! 3. Sending a request with NO cookie → expects `SessionUser(None)`.
//!
//! Anonymous-path validation is sufficient for the cfg-lift contract.
//! The cookie-decode + user-fetch path goes through
//! `tenant_console::decode` (dialect-agnostic HMAC) +
//! `Model::objects().fetch(&pool)` (the same primitive
//! `SessionOperator` already uses successfully on sqlite). PG-specific
//! regression of the cookie path is covered by the existing PG
//! integration suite (`auth_live.rs`).

#![cfg(all(feature = "tenancy", feature = "sqlite"))]

use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use rustango::extractors::{SessionUser, TenantContext};
use rustango::sql::sqlx;
use rustango::tenancy::{ChainResolver, Org, OrgResolver, TenantPools};
use tower::ServiceExt;

#[derive(Clone)]
struct FixedResolver(Org);

#[async_trait::async_trait]
impl OrgResolver for FixedResolver {
    async fn resolve(
        &self,
        _parts: &axum::http::request::Parts,
        _registry: &rustango::sql::Pool,
    ) -> Result<Option<Org>, rustango::tenancy::TenancyError> {
        Ok(Some(self.0.clone()))
    }
}

fn fake_sqlite_org() -> Org {
    Org {
        id: rustango::sql::Auto::default(),
        slug: "acme".into(),
        display_name: "Acme".into(),
        storage_mode: "database".into(),
        backend_kind: "sqlite".into(),
        database_url: Some("sqlite::memory:".into()),
        ..rustango::testkit::org()
    }
}

/// Handler that returns "anon" when no SessionUser is present.
/// The extractor is `Infallible`, so the handler is reached even
/// for anonymous requests.
async fn whoami(SessionUser(user): SessionUser) -> impl IntoResponse {
    match user {
        Some(u) => format!("user:{}", u.username),
        None => "anon".to_owned(),
    }
}

#[tokio::test]
async fn session_user_anon_path_returns_none_on_sqlite() {
    let registry = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("registry pool");
    let pools: Arc<TenantPools<sqlx::Sqlite>> = Arc::new(TenantPools::new(registry));

    let resolver = ChainResolver::new().push(FixedResolver(fake_sqlite_org()));

    let ctx = Arc::new(TenantContext {
        pools,
        resolver,
        session_secret: rustango::tenancy::session::SessionSecret::from_bytes(
            b"test_tenant_session_secret_32by!".to_vec(),
        ),
        operator_secret: rustango::tenancy::session::SessionSecret::from_bytes(
            b"test_oper_session_secret____32b!".to_vec(),
        ),
    });

    let app: Router = Router::new()
        .route("/whoami", get(whoami))
        .layer(axum::middleware::from_fn(
            move |mut req: Request, next: axum::middleware::Next| {
                let ctx = ctx.clone();
                async move {
                    req.extensions_mut().insert(ctx);
                    next.run(req).await
                }
            },
        ));

    let req = Request::builder()
        .uri("/whoami")
        .body(Body::empty())
        .expect("build req");
    let resp = app.oneshot(req).await.expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    let text = std::str::from_utf8(&body).expect("utf8");
    assert_eq!(
        text, "anon",
        "SessionUser should return None for anonymous requests",
    );
}

#[tokio::test]
async fn session_user_malformed_cookie_returns_none_on_sqlite() {
    let registry = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("registry pool");
    let pools: Arc<TenantPools<sqlx::Sqlite>> = Arc::new(TenantPools::new(registry));

    let resolver = ChainResolver::new().push(FixedResolver(fake_sqlite_org()));

    let ctx = Arc::new(TenantContext {
        pools,
        resolver,
        session_secret: rustango::tenancy::session::SessionSecret::from_bytes(
            b"test_tenant_session_secret_32by!".to_vec(),
        ),
        operator_secret: rustango::tenancy::session::SessionSecret::from_bytes(
            b"test_oper_session_secret____32b!".to_vec(),
        ),
    });

    let app: Router = Router::new()
        .route("/whoami", get(whoami))
        .layer(axum::middleware::from_fn(
            move |mut req: Request, next: axum::middleware::Next| {
                let ctx = ctx.clone();
                async move {
                    req.extensions_mut().insert(ctx);
                    next.run(req).await
                }
            },
        ));

    // Send a request with a malformed `rustango_tenant_session`
    // cookie — the extractor must short-circuit to None rather than
    // panicking or rejecting (Infallible contract).
    let req = Request::builder()
        .uri("/whoami")
        .header("Cookie", "rustango_tenant_session=not.a.valid.cookie")
        .body(Body::empty())
        .expect("build req");
    let resp = app.oneshot(req).await.expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    let text = std::str::from_utf8(&body).expect("utf8");
    assert_eq!(
        text, "anon",
        "SessionUser should swallow malformed cookies and return None",
    );
}
