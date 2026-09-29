//! Tenant routers serve a `TenantContext<Sqlite>` through their `*_for::<DB>`
//! entry. With `postgres` also on, the default `Tenant` is Postgres and
//! these got 500 (#1741, #1787).

#![cfg(all(
    feature = "sqlite",
    feature = "tenancy",
    any(feature = "mcp", feature = "sso")
))]
#![allow(irrefutable_let_patterns)] // Pool enum is single-variant in sqlite-only builds.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{header, Request};
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
    use axum::http::StatusCode;
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

#[cfg(feature = "sso")]
#[tokio::test]
async fn member_sso_router_serves_a_non_default_backend() {
    use axum::routing::{get, post};
    use axum::Json;
    use rustango::sql::{Auto, FetcherPool as _};
    use rustango::sso::SsoProvider;
    use rustango::tenancy::member_auth::{member_sso_router_for, MemberAuthConfig};
    use rustango::tenancy::User;

    let _g = SUITE.lock().await;
    // A fake OIDC IdP on loopback, which outbound calls allow only when listed.
    std::env::set_var("RUSTANGO_OUTBOUND_ALLOW", "127.0.0.1");
    std::env::set_var("RUSTANGO_SECRET_KEY", "non-default-backend-key");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = format!("http://{}", listener.local_addr().unwrap());
    let doc = serde_json::json!({
        "authorization_endpoint": format!("{issuer}/auth"),
        "token_endpoint": format!("{issuer}/token"),
        "userinfo_endpoint": format!("{issuer}/userinfo"),
    });
    let idp = Router::new()
        .route(
            "/.well-known/openid-configuration",
            get(move || async move { Json(doc) }),
        )
        .route(
            "/token",
            post(|| async {
                Json(serde_json::json!({"access_token": "at", "token_type": "Bearer"}))
            }),
        )
        .route(
            "/userinfo",
            get(|| async {
                Json(serde_json::json!({
                    "sub": "sub-1", "email": "m@acme.test", "email_verified": true
                }))
            }),
        );
    tokio::spawn(async move { axum::serve(listener, idp).await.unwrap() });

    let env = boot().await;
    rustango::testkit::create_tables_for::<SsoProvider>(&env.tenant)
        .await
        .unwrap();
    rustango::sso::link::ensure_table(&env.tenant)
        .await
        .unwrap();
    let mut provider = SsoProvider {
        id: Auto::default(),
        slug: "idp".into(),
        label: "idp".into(),
        kind: "oidc".into(),
        issuer_url: Some(issuer.clone()),
        client_id: "cid".into(),
        client_secret: rustango::casts::Cast::new("csecret".into()),
        enabled: true,
        sort_order: 0,
        scopes: None,
        allow_email_link: false,
        created_at: Auto::default(),
        updated_at: Auto::default(),
    };
    provider.insert_pool(&env.tenant).await.unwrap();
    let app = env.mount(member_sso_router_for::<sqlx::Sqlite>(
        MemberAuthConfig::default(),
    ));

    let begin = send(
        &app,
        Request::get("/auth/sso/idp")
            .header(header::HOST, "acme.app.test")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(begin.status().is_redirection(), "begin: {}", begin.status());
    let to = begin.headers()[header::LOCATION]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(to.starts_with(&issuer), "begin redirects to the IdP: {to}");
    let state = to
        .split(['?', '&'])
        .find_map(|kv| kv.strip_prefix("state="))
        .unwrap()
        .to_owned();
    let flow: Vec<String> = begin
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_owned())
        .collect();

    let done = send(
        &app,
        Request::get(format!("/auth/sso/idp/callback?code=c&state={state}"))
            .header(header::HOST, "acme.app.test")
            .header(header::COOKIE, flow.join("; "))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(
        done.status().is_redirection(),
        "callback: {}",
        done.status()
    );
    let cookies: Vec<&str> = done
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap())
        .collect();
    assert!(
        cookies
            .iter()
            .any(|c| c.starts_with("rustango_member_session=") && !c.contains("Max-Age=0")),
        "member session minted: {cookies:?}"
    );
    let users = User::objects().fetch(&env.tenant).await.unwrap();
    assert_eq!(users.len(), 1, "member provisioned in the SQLite tenant");
}
