//! Tenant routers serve a `TenantContext<Sqlite>` through their `*_for::<DB>`
//! entry. With `postgres` also on, the default `Tenant` is Postgres and
//! these got 500 (#1787).

#![cfg(all(feature = "sqlite", feature = "tenancy", feature = "mcp"))]
#![allow(irrefutable_let_patterns)] // Pool enum is single-variant in sqlite-only builds.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use rustango::extractors::TenantContext;
use rustango::sql::{sqlx, Pool};
use rustango::tenancy::session::SessionSecret;
use rustango::tenancy::{ChainResolver, Org, OrgResolver, TenancyError, TenantPools};
use tower::ServiceExt as _;

static SUITE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Clone)]
struct FixedResolver(Org);

#[async_trait]
impl OrgResolver for FixedResolver {
    async fn resolve(
        &self,
        _parts: &axum::http::request::Parts,
        _registry: &Pool,
    ) -> Result<Option<Org>, TenancyError> {
        Ok(Some(self.0.clone()))
    }
}

struct Env {
    ctx: Arc<TenantContext<sqlx::Sqlite>>,
    tenant: Pool,
    _dir: tempfile::TempDir,
}

/// A SQLite registry and one database-mode SQLite tenant.
async fn boot() -> Env {
    let dir = tempfile::tempdir().expect("tempdir");
    let reg_url = format!("sqlite://{}?mode=rwc", dir.path().join("reg.db").display());
    let tenant_url = format!("sqlite://{}?mode=rwc", dir.path().join("t.db").display());
    let pools = Arc::new(TenantPools::<sqlx::Sqlite>::new(
        sqlx::SqlitePool::connect(&reg_url).await.expect("registry"),
    ));
    let migrations = dir.path().join("migrations");
    std::fs::create_dir_all(&migrations).unwrap();
    rustango::tenancy::manage::run_with_writer(
        pools.as_ref(),
        &reg_url,
        &migrations,
        vec!["migrate-registry".to_owned()],
        &mut Vec::new(),
    )
    .await
    .expect("migrate-registry");
    let org = Org {
        slug: "acme".into(),
        storage_mode: "database".into(),
        backend_kind: "sqlite".into(),
        database_url: Some(tenant_url.clone()),
        host_pattern: Some("acme.app.test".into()),
        ..rustango::testkit::org()
    };
    org.clone()
        .insert_pool(&pools.registry_pool())
        .await
        .expect("seed org");
    let tenant = Pool::connect(&tenant_url).await.expect("tenant");
    rustango::testkit::migrate_framework(&tenant)
        .await
        .expect("migrate framework");
    let secret = SessionSecret::from_bytes(b"non-default-backend-secret-32b!!".to_vec());
    let ctx = Arc::new(TenantContext::<sqlx::Sqlite> {
        pools,
        resolver: ChainResolver::new().push(FixedResolver(org)),
        session_secret: secret.clone(),
        operator_secret: secret,
    });
    Env {
        ctx,
        tenant,
        _dir: dir,
    }
}

impl Env {
    /// `router` with the SQLite context injected, as `server::Builder` does.
    fn mount(&self, router: Router) -> Router {
        let ctx = self.ctx.clone();
        router.layer(axum::middleware::from_fn(
            move |mut req: Request<Body>, next: axum::middleware::Next| {
                let ctx = ctx.clone();
                async move {
                    req.extensions_mut().insert(ctx);
                    next.run(req).await
                }
            },
        ))
    }
}

async fn send(app: &Router, req: Request<Body>) -> axum::response::Response {
    app.clone().oneshot(req).await.unwrap()
}

#[cfg(feature = "mcp")]
#[tokio::test]
async fn mcp_tenant_router_serves_a_non_default_backend() {
    use base64::Engine as _;
    use rustango::tenancy::jwt_lifecycle::JwtLifecycle;

    async fn json_body(r: axum::response::Response) -> serde_json::Value {
        let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        serde_json::from_slice(&b).unwrap()
    }

    let _g = SUITE.lock().await;
    let env = boot().await;
    let issued = rustango::tenancy::create_agent_pool(&env.tenant, "bot")
        .await
        .expect("agent");
    let jwt = Arc::new(JwtLifecycle::new(
        b"non-default-backend-mcp-secret-32b!!".to_vec(),
    ));
    let app = env.mount(Router::new().nest(
        "/mcp",
        rustango::mcp::tenant_router_authed_for::<sqlx::Sqlite>(jwt),
    ));

    let r = send(
        &app,
        Request::post("/mcp/token")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({ "name": "bot", "secret": issued.token }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK, "/token");
    let access = json_body(r).await["access_token"]
        .as_str()
        .unwrap()
        .to_owned();

    let basic = base64::engine::general_purpose::STANDARD.encode(format!("bot:{}", issued.token));
    let r = send(
        &app,
        Request::post("/mcp/oauth/token")
            .header(header::AUTHORIZATION, format!("Basic {basic}"))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("grant_type=client_credentials"))
            .unwrap(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK, "/oauth/token");

    let r = send(
        &app,
        Request::post("/mcp")
            .header(header::AUTHORIZATION, format!("Bearer {access}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK, "JSON-RPC POST");
    assert!(json_body(r).await.get("result").is_some());

    let r = send(
        &app,
        Request::get("/mcp")
            .header(header::AUTHORIZATION, format!("Bearer {access}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK, "SSE GET");
}
