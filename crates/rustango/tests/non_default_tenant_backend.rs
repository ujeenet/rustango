//! Tenant routers serve a `TenantContext<Sqlite>` through their `*_for::<DB>`
//! entry. With `postgres` also on, the default `Tenant` is Postgres and
//! these got 500 (#1741, #1787). The pure `DatabaseTenantContext` stack got
//! 500 too (#1802).

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
use rustango::extractors::{DatabaseTenantContext, TenantContext};
use rustango::sql::{sqlx, Pool};
use rustango::tenancy::session::SessionSecret;
use rustango::tenancy::{
    BackendKind, ChainResolver, DatabasePools, Org, OrgResolver, TenancyError, TenantPools,
};
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
    /// The same registry and tenant, as the pure SQLite stack mounts it.
    db_ctx: Arc<DatabaseTenantContext<sqlx::Sqlite>>,
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
    let db_ctx = Arc::new(DatabaseTenantContext::<sqlx::Sqlite> {
        pools: Arc::new(DatabasePools::new(BackendKind::Sqlite)),
        resolver: ChainResolver::new().push(FixedResolver(org.clone())),
        session_secret: secret.clone(),
        operator_secret: secret.clone(),
        registry: pools.registry_pool(),
    });
    let ctx = Arc::new(TenantContext::<sqlx::Sqlite> {
        pools,
        resolver: ChainResolver::new().push(FixedResolver(org)),
        session_secret: secret.clone(),
        operator_secret: secret,
    });
    Env {
        ctx,
        db_ctx,
        tenant,
        _dir: dir,
    }
}

impl Env {
    /// `router` with the SQLite context injected, as `server::Builder` does.
    fn mount(&self, router: Router) -> Router {
        inject(router, self.ctx.clone())
    }

    /// `router` with the pure-stack `DatabaseTenantContext` injected.
    fn mount_db(&self, router: Router) -> Router {
        inject(router, self.db_ctx.clone())
    }
}

fn inject<T: Clone + Send + Sync + 'static>(router: Router, ctx: T) -> Router {
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

async fn send(app: &Router, req: Request<Body>) -> axum::response::Response {
    app.clone().oneshot(req).await.unwrap()
}

/// `Tenant<Sqlite>` resolves on the pure stack; `t.pool()` and the
/// deferred `pool_conn()` both reach the tenant's database (#1802).
#[tokio::test]
async fn tenant_extractor_reads_the_database_tenant_context() {
    use rustango::extractors::Tenant;
    use rustango::sql::CounterPool as _;
    use rustango::tenancy::User;

    let _g = SUITE.lock().await;
    let env = boot().await;
    // A tenant table every feature set has; `rustango_agents` needs `mcp` (#2362).
    rustango::testkit::user()
        .insert_pool(&env.tenant)
        .await
        .expect("user");
    let app = env.mount_db(Router::new().route(
        "/",
        axum::routing::get(|mut t: Tenant<sqlx::Sqlite>| async move {
            let via_pool = User::objects().count(t.pool()).await.expect("t.pool()");
            let conn = t.pool_conn().await.expect("deferred conn");
            let via_conn: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rustango_users")
                .fetch_one(&mut **conn)
                .await
                .expect("pool_conn()");
            format!("{}:{via_pool}:{via_conn}", t.org.slug)
        }),
    ));
    let r = send(&app, Request::get("/").body(Body::empty()).unwrap()).await;
    assert_eq!(r.status(), axum::http::StatusCode::OK);
    let b = axum::body::to_bytes(r.into_body(), 1 << 16).await.unwrap();
    assert_eq!(std::str::from_utf8(&b).unwrap(), "acme:1:1");
}

#[cfg(feature = "mcp")]
#[tokio::test]
async fn mcp_tenant_router_serves_a_non_default_backend() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    mcp_flow(&env, Env::mount).await;
}

#[cfg(feature = "mcp")]
#[tokio::test]
async fn mcp_tenant_router_serves_the_database_tenant_stack() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    mcp_flow(&env, Env::mount_db).await;
}

/// Token, OAuth, JSON-RPC and SSE all answer through `mount`'s context.
#[cfg(feature = "mcp")]
async fn mcp_flow(env: &Env, mount: fn(&Env, Router) -> Router) {
    use axum::http::StatusCode;
    use base64::Engine as _;
    use rustango::tenancy::jwt_lifecycle::JwtLifecycle;

    async fn json_body(r: axum::response::Response) -> serde_json::Value {
        let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        serde_json::from_slice(&b).unwrap()
    }

    let issued = rustango::tenancy::create_agent_pool(&env.tenant, "bot")
        .await
        .expect("agent");
    let jwt = Arc::new(JwtLifecycle::new(
        b"non-default-backend-mcp-secret-32b!!".to_vec(),
    ));
    let app = mount(
        env,
        Router::new().nest(
            "/mcp",
            rustango::mcp::tenant_router_authed_for::<sqlx::Sqlite>(jwt),
        ),
    );

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

    // No bearer: both mounts refuse the JSON-RPC and SSE endpoints.
    let secure = mount(
        env,
        Router::new().nest(
            "/mcp",
            rustango::mcp::secure_tenant_router_for::<sqlx::Sqlite>(),
        ),
    );
    for app in [&app, &secure] {
        let r = send(
            app,
            Request::post("/mcp")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }).to_string(),
                ))
                .unwrap(),
        )
        .await;
        assert_unauthorized_json(r, "POST without bearer").await;
        let r = send(app, Request::get("/mcp").body(Body::empty()).unwrap()).await;
        assert_unauthorized_json(r, "GET without bearer").await;
    }

    // A rotated secret revokes both the minted JWT and the raw key (#2259).
    rustango::tenancy::rotate_agent_secret_pool(&env.tenant, "bot")
        .await
        .expect("rotate");
    for (bearer, what) in [(&access, "revoked JWT"), (&issued.token, "revoked raw key")] {
        let r = send(
            &app,
            Request::post("/mcp")
                .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }).to_string(),
                ))
                .unwrap(),
        )
        .await;
        assert_unauthorized_json(r, what).await;
    }
}

/// A 401 with the OAuth challenge and a JSON-RPC error body (#2259).
#[cfg(feature = "mcp")]
async fn assert_unauthorized_json(r: axum::response::Response, what: &str) {
    assert_eq!(r.status(), axum::http::StatusCode::UNAUTHORIZED, "{what}");
    assert!(
        r.headers().contains_key(header::WWW_AUTHENTICATE),
        "{what}: challenge"
    );
    assert_eq!(
        r.headers()[header::CONTENT_TYPE],
        "application/json",
        "{what}: content-type"
    );
    let b = axum::body::to_bytes(r.into_body(), 1 << 16).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&b).expect("JSON body");
    assert_eq!(
        v,
        serde_json::json!({
            "jsonrpc": "2.0",
            "error": { "code": -32001, "message": "missing or invalid agent token" },
            "id": null
        }),
        "{what}"
    );
}

#[cfg(feature = "sso")]
#[tokio::test]
async fn member_sso_router_serves_a_non_default_backend() {
    use axum::routing::{get, post};
    use axum::Json;
    use rustango::sql::{Auto, FetcherPool as _};
    use rustango::sso::SsoProvider;
    use rustango::tenancy::member_auth::{member_sso_router_for, CurrentMember, MemberAuthConfig};
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
    let app = env.mount(
        member_sso_router_for::<sqlx::Sqlite>(MemberAuthConfig::default()).route(
            "/whoami",
            get(|m: CurrentMember| async move { m.0.map(|u| u.username).unwrap_or_default() }),
        ),
    );

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
    let session = cookies
        .iter()
        .find(|c| c.starts_with("rustango_member_session=") && !c.contains("Max-Age=0"))
        .unwrap_or_else(|| panic!("member session minted: {cookies:?}"))
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let users = User::objects().fetch(&env.tenant).await.unwrap();
    assert_eq!(users.len(), 1, "member provisioned in the SQLite tenant");
    assert!(!users[0].username.is_empty());

    // The minted cookie resolves as `CurrentMember` on the SQLite context.
    let who = send(
        &app,
        Request::get("/whoami")
            .header(header::HOST, "acme.app.test")
            .header(header::COOKIE, session)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let body = axum::body::to_bytes(who.into_body(), 1 << 16)
        .await
        .unwrap();
    assert_eq!(
        std::str::from_utf8(&body).unwrap(),
        users[0].username,
        "cookie resolves the member"
    );
}
