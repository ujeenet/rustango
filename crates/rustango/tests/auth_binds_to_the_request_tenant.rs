//! A credential must not authenticate on another tenant's host.
//!
//! `require_auth` used to take a `Pool` captured when the router was
//! built, so every request authenticated against that one database
//! whatever host it arrived on.
//!
//! Two tenants, two SQLite databases, one router. The credential exists
//! in `alpha` only.

#![cfg(all(feature = "tenancy", feature = "sqlite"))]

use std::sync::Arc;

use axum::body::Body;
use axum::http::header::HOST;
use axum::http::{header, Request, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use rustango::extractors::DatabaseTenantContext;
use rustango::sql::sqlx;
use rustango::tenancy::auth_backends::{AuthBackend, ModelBackend};
use rustango::tenancy::{
    session::SessionSecret, BackendKind, ChainResolver, CurrentUser, DatabasePools, Org,
    OrgResolver, RouterAuthExt, TenancyError,
};
use tower::ServiceExt as _;

/// `Host: <slug>.localhost` → synthetic Org, database-mode SQLite.
#[derive(Clone)]
struct HostResolver;

#[async_trait::async_trait]
impl OrgResolver for HostResolver {
    async fn resolve(
        &self,
        parts: &axum::http::request::Parts,
        _registry: &rustango::sql::Pool,
    ) -> Result<Option<Org>, TenancyError> {
        let host = parts
            .headers
            .get(HOST)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        let slug = host.split('.').next().unwrap_or("");
        if slug.is_empty() || !host.contains('.') {
            return Ok(None);
        }
        Ok(Some(Org {
            id: rustango::sql::Auto::default(),
            slug: slug.to_owned(),
            display_name: slug.to_owned(),
            storage_mode: "database".into(),
            backend_kind: "sqlite".into(),
            database_url: None,
            ..rustango::testkit::org()
        }))
    }
}

async fn whoami(CurrentUser(user): CurrentUser) -> impl IntoResponse {
    match user {
        Some(u) => (StatusCode::OK, u.username).into_response(),
        None => (StatusCode::UNAUTHORIZED, "anonymous").into_response(),
    }
}

/// The tenant `require_auth` resolved, as seen by `Principal`.
async fn tenant_of(parts: axum::http::request::Parts) -> String {
    rustango::tenancy::Principal::from_parts(&parts)
        .and_then(|p| p.tenant)
        .unwrap_or_default()
}

/// Minimal base64 for the Basic header — no extra dependency.
fn b64(input: &str) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = input.as_bytes();
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Build a tenant database with the users table, optionally holding
/// one user. Returns the pool so the caller can keep the `cache=shared`
/// in-memory database alive for the duration of the test.
async fn tenant_db(url: &str, user: Option<(&str, &str)>) -> rustango::sql::Pool {
    let pool = rustango::sql::Pool::connect(url).await.expect("tenant db");
    rustango::testkit::create_tables_for::<rustango::tenancy::User>(&pool)
        .await
        .expect("users table");
    if let Some((username, password)) = user {
        let hash = rustango::tenancy::password::hash(password).expect("hash");
        let sq = pool.as_sqlite().expect("sqlite-only test");
        sqlx::query(
            "INSERT INTO rustango_users (username, password_hash, is_superuser, active, created_at) \
             VALUES (?, ?, 0, 1, datetime('now'))",
        )
        .bind(username)
        .bind(&hash)
        .execute(sq)
        .await
        .expect("seed user");
    }
    pool
}

fn build_app(ns: &str) -> (Router, String) {
    build_app_with(ns, None)
}

/// [`build_app`], with `require_perm(perm)` inside the auth layer.
fn build_app_with(ns: &str, perm: Option<&'static str>) -> (Router, String) {
    let template = format!("sqlite:file:xtenant_{ns}_{{slug}}?mode=memory&cache=shared");
    let pools = Arc::new(
        DatabasePools::<sqlx::Sqlite>::new(BackendKind::Sqlite).with_url_template(&template),
    );
    let ctx = Arc::new(DatabaseTenantContext {
        pools,
        resolver: ChainResolver::new().push(HostResolver),
        session_secret: SessionSecret::from_bytes(b"test_tenant_secret_____32bytes!!".to_vec()),
        operator_secret: SessionSecret::from_bytes(b"test_oper_secret_______32bytes!!".to_vec()),
        registry: rustango::sql::Pool::Sqlite(
            sqlx::SqlitePool::connect_lazy("sqlite::memory:").expect("lazy sqlite"),
        ),
    });

    let backends: Vec<Arc<dyn AuthBackend>> = vec![Arc::new(ModelBackend)];
    let mut app = Router::new()
        .route("/whoami", get(whoami))
        .route("/tenant", get(tenant_of));
    if let Some(perm) = perm {
        app = app.require_perm(perm);
    }
    let app = app.require_auth(backends).layer(axum::Extension(ctx));
    (app, template)
}

async fn call(app: &Router, host: &str, basic: &str) -> StatusCode {
    let req = Request::builder()
        .uri("/whoami")
        .header(HOST, host)
        .header(header::AUTHORIZATION, format!("Basic {}", b64(basic)))
        .body(Body::empty())
        .expect("request");
    app.clone().oneshot(req).await.expect("response").status()
}

/// The credential works on its own tenant — the control. Without this
/// the rejection below could be "authentication is broken everywhere",
/// which would pass the security assertion for the wrong reason.
#[tokio::test]
async fn the_credential_works_on_its_own_tenant() {
    let (app, template) = build_app("own");
    let _alpha = tenant_db(
        &template.replace("{slug}", "alpha"),
        Some(("ann", "s3cret")),
    )
    .await;

    assert_eq!(
        call(&app, "alpha.localhost", "ann:s3cret").await,
        StatusCode::OK,
        "ann exists in alpha and must authenticate there"
    );

    let req = Request::builder()
        .uri("/tenant")
        .header(HOST, "alpha.localhost")
        .header(
            header::AUTHORIZATION,
            format!("Basic {}", b64("ann:s3cret")),
        )
        .body(Body::empty())
        .expect("request");
    let resp = app.oneshot(req).await.expect("response");
    let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
    assert_eq!(&body[..], b"alpha", "require_auth records the tenant slug");
}

/// The same credential must be refused on another tenant's host.
///
/// This is the vulnerability. With the pool captured at mount time the
/// backend looked `ann` up in whichever database the router was built
/// with and admitted her to `beta` — where she has no row at all.
#[tokio::test]
async fn the_same_credential_is_refused_on_another_tenants_host() {
    let (app, template) = build_app("cross");
    let _alpha = tenant_db(
        &template.replace("{slug}", "alpha"),
        Some(("ann", "s3cret")),
    )
    .await;
    // `beta` exists and is reachable, but holds no `ann`. Creating its
    // table matters: a database with no table at all could be refused
    // for the wrong reason, which would pass this test while the bug
    // was fully present.
    let _beta = tenant_db(&template.replace("{slug}", "beta"), None).await;

    assert_eq!(
        call(&app, "beta.localhost", "ann:s3cret").await,
        StatusCode::UNAUTHORIZED,
        "ann has no row in beta, so her credential must not authenticate \
         there. A 200 means the middleware checked her against some other \
         tenant's database — the cross-tenant authentication bypass."
    );
}

/// `ListenerPort(8001)` → `alpha`, as `PortResolver` maps a listener to a tenant.
#[derive(Clone)]
struct PortSlugResolver;

#[async_trait::async_trait]
impl OrgResolver for PortSlugResolver {
    async fn resolve(
        &self,
        parts: &axum::http::request::Parts,
        _registry: &rustango::sql::Pool,
    ) -> Result<Option<Org>, TenancyError> {
        let Some(rustango::tenancy::ListenerPort(8001)) = parts.extensions.get().copied() else {
            return Ok(None);
        };
        Ok(Some(Org {
            id: rustango::sql::Auto::default(),
            slug: "alpha".to_owned(),
            display_name: "alpha".to_owned(),
            storage_mode: "database".into(),
            backend_kind: "sqlite".into(),
            database_url: None,
            ..rustango::testkit::org()
        }))
    }
}

fn port_app(ns: &str) -> (Router, String) {
    let template = format!("sqlite:file:xtenant_{ns}_{{slug}}?mode=memory&cache=shared");
    let pools = Arc::new(
        DatabasePools::<sqlx::Sqlite>::new(BackendKind::Sqlite).with_url_template(&template),
    );
    let ctx = Arc::new(DatabaseTenantContext {
        pools,
        resolver: ChainResolver::new().push(PortSlugResolver),
        session_secret: SessionSecret::from_bytes(b"test_tenant_secret_____32bytes!!".to_vec()),
        operator_secret: SessionSecret::from_bytes(b"test_oper_secret_______32bytes!!".to_vec()),
        registry: rustango::sql::Pool::Sqlite(
            sqlx::SqlitePool::connect_lazy("sqlite::memory:").expect("lazy sqlite"),
        ),
    });
    let backends: Vec<Arc<dyn AuthBackend>> = vec![Arc::new(ModelBackend)];
    let app = Router::new()
        .route("/whoami", get(whoami))
        .require_auth(backends)
        .layer(axum::Extension(ctx))
        .layer(axum::Extension(rustango::tenancy::ListenerPort(8001)));
    (app, template)
}

/// The tenant resolver sees the request's extensions, so a port-only chain resolves.
#[tokio::test]
async fn a_port_resolved_tenant_authenticates() {
    let (app, template) = port_app("port");
    let _alpha = tenant_db(
        &template.replace("{slug}", "alpha"),
        Some(("ann", "s3cret")),
    )
    .await;
    assert_eq!(
        call(&app, "api.localhost", "ann:s3cret").await,
        StatusCode::OK
    );
}

/// A tenant an outer layer bound must match the one auth resolves.
#[tokio::test]
async fn another_bound_tenant_is_refused() {
    let (app, template) = build_app("bound");
    let app = app.layer(axum::Extension(rustango::tenancy::TenantSlug(
        "beta".into(),
    )));
    let _alpha = tenant_db(
        &template.replace("{slug}", "alpha"),
        Some(("ann", "s3cret")),
    )
    .await;
    assert_eq!(
        call(&app, "alpha.localhost", "ann:s3cret").await,
        StatusCode::FORBIDDEN
    );
}

async fn call_full(
    app: &Router,
    host: &str,
    basic: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let mut req = Request::builder().uri("/whoami").header(HOST, host);
    if let Some(b) = basic {
        req = req.header(header::AUTHORIZATION, format!("Basic {}", b64(b)));
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::empty()).expect("request"))
        .await
        .expect("response");
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
    let json = serde_json::from_slice(&body).unwrap_or_else(|_| {
        panic!(
            "{status}: body is not JSON: {}",
            String::from_utf8_lossy(&body)
        )
    });
    (status, json)
}

/// A failing lookup is a server error, not a 401 carrying the driver text.
#[tokio::test]
async fn a_database_error_is_a_503_without_driver_text() {
    let (app, template) = build_app("dberr");
    // The tenant database exists but has no users table, so the lookup fails.
    let _gamma = rustango::sql::Pool::connect(&template.replace("{slug}", "gamma"))
        .await
        .expect("tenant db");
    let (status, body) = call_full(&app, "gamma.localhost", Some("ann:s3cret")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"], "service_unavailable");
    let text = body.to_string();
    assert!(
        !text.contains("rustango_users") && !text.contains("no such table"),
        "{text}"
    );
}

/// A failing permission read is a server error, not a 403.
#[tokio::test]
async fn a_permission_read_error_is_a_503_without_driver_text() {
    let (app, template) = build_app_with("permerr", Some("post.add"));
    // Users only: the permission tables are missing, so the read fails.
    let _alpha = tenant_db(
        &template.replace("{slug}", "alpha"),
        Some(("ann", "s3cret")),
    )
    .await;
    let (status, body) = call_full(&app, "alpha.localhost", Some("ann:s3cret")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"], "service_unavailable");
    assert!(!body.to_string().contains("no such table"), "{body}");
}

/// The 401 and 403 from the auth layer use the API error JSON shape.
#[tokio::test]
async fn auth_layer_refusals_are_json() {
    let (app, template) = build_app("json");
    let _alpha = tenant_db(
        &template.replace("{slug}", "alpha"),
        Some(("ann", "s3cret")),
    )
    .await;
    let (status, body) = call_full(&app, "alpha.localhost", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "unauthorized");

    let bound = app.layer(axum::Extension(rustango::tenancy::TenantSlug(
        "beta".into(),
    )));
    let (status, body) = call_full(&bound, "alpha.localhost", Some("ann:s3cret")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "forbidden");
}

/// Users only, no permission tables: every permission read fails.
async fn alpha_without_perm_tables(ns: &str) -> (Router, String, rustango::sql::Pool) {
    let (app, template) = build_app(ns);
    let alpha = tenant_db(
        &template.replace("{slug}", "alpha"),
        Some(("ann", "s3cret")),
    )
    .await;
    (app, template, alpha)
}

/// A ViewSet's permission read error is a logged 503, not a 403.
#[tokio::test]
async fn a_viewset_permission_read_error_is_a_503() {
    let (_, template, _alpha) = alpha_without_perm_tables("vsperm").await;
    let pools = Arc::new(
        DatabasePools::<sqlx::Sqlite>::new(BackendKind::Sqlite).with_url_template(&template),
    );
    let ctx = Arc::new(DatabaseTenantContext {
        pools,
        resolver: ChainResolver::new().push(HostResolver),
        session_secret: SessionSecret::from_bytes(b"test_tenant_secret_____32bytes!!".to_vec()),
        operator_secret: SessionSecret::from_bytes(b"test_oper_secret_______32bytes!!".to_vec()),
        registry: rustango::sql::Pool::Sqlite(
            sqlx::SqlitePool::connect_lazy("sqlite::memory:").expect("lazy sqlite"),
        ),
    });
    let backends: Vec<Arc<dyn AuthBackend>> = vec![Arc::new(ModelBackend)];
    use rustango::core::Model as _;
    let app = rustango::viewset::ViewSet::for_model(rustango::tenancy::User::SCHEMA, &["id"])
        .permissions_for_model::<rustango::tenancy::User>()
        .tenant_router("/users")
        .require_auth(backends)
        .layer(axum::Extension(ctx));
    let req = Request::builder()
        .uri("/users")
        .header(HOST, "alpha.localhost")
        .header(
            header::AUTHORIZATION,
            format!("Basic {}", b64("ann:s3cret")),
        )
        .body(Body::empty())
        .expect("request");
    let resp = app.oneshot(req).await.expect("response");
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
    assert!(
        !String::from_utf8_lossy(&body).contains("no such table"),
        "{body:?}"
    );
}

/// `permission_required(_or_403)` answer a permission read error with 503.
/// They read `Tenant<DefaultTenantDb>`, a Postgres one when that feature is on.
#[cfg(not(feature = "postgres"))]
#[tokio::test]
async fn a_decorator_permission_read_error_is_a_503() {
    use rustango::tenancy::tenant_console::{
        encode, PasswordFingerprint, TenantSessionPayload, COOKIE_NAME,
    };
    let (_, template, alpha) = alpha_without_perm_tables("decperm").await;
    let ann: Vec<rustango::tenancy::User> = {
        use rustango::sql::FetcherPool as _;
        rustango::tenancy::User::objects()
            .filter("username", "ann")
            .fetch(&alpha)
            .await
            .expect("ann")
    };
    let secret = SessionSecret::from_bytes(b"test_tenant_secret_____32bytes!!".to_vec());
    let payload = TenantSessionPayload::new(
        *ann[0].id.get().expect("saved"),
        "alpha",
        3600,
        PasswordFingerprint::of(&secret, &ann[0].password_hash),
    );
    let cookie = format!("{COOKIE_NAME}={}", encode(&secret, &payload));
    let pools = Arc::new(
        DatabasePools::<sqlx::Sqlite>::new(BackendKind::Sqlite).with_url_template(&template),
    );
    let ctx = Arc::new(DatabaseTenantContext {
        pools,
        resolver: ChainResolver::new().push(HostResolver),
        session_secret: secret,
        operator_secret: SessionSecret::from_bytes(b"test_oper_secret_______32bytes!!".to_vec()),
        registry: rustango::sql::Pool::Sqlite(
            sqlx::SqlitePool::connect_lazy("sqlite::memory:").expect("lazy sqlite"),
        ),
    });
    use rustango::auth_decorators::{permission_required, permission_required_or_403};
    let app = Router::new()
        .route("/a", get(|| async { "ok" }))
        .layer(permission_required("/login", "post.add"))
        .merge(
            Router::new()
                .route("/b", get(|| async { "ok" }))
                .layer(permission_required_or_403("post.add")),
        )
        .layer(axum::Extension(ctx));
    for path in ["/a", "/b"] {
        let req = Request::builder()
            .uri(path)
            .header(HOST, "alpha.localhost")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .expect("request");
        let status = app.clone().oneshot(req).await.expect("response").status();
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{path}");
    }
}
