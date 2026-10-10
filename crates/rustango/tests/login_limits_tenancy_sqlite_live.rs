//! Login limits and the hash queue on the tenancy logins (#1609, #1732):
//! operator console, tenant admin, JWT login, HTTP Basic and API keys.
//!
//! The gate, the lockout and the hash queue are process-global, so every
//! test holds [`SUITE`] and uses its own names and addresses.

#![cfg(all(feature = "sqlite", feature = "tenancy", feature = "admin"))]
#![allow(irrefutable_let_patterns)] // Pool enum is single-variant in sqlite-only builds.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, Request, StatusCode};
use axum::response::IntoResponse as _;
use axum::routing::get;
use axum::Router;
use rustango::core::Column as _;
use rustango::extractors::TenantContext;
use rustango::login_throttle::{configure_shared, LoginLimits, LoginThrottle};
use rustango::sql::{sqlx, Auto, FetcherPool as _, Pool};
use rustango::tenancy::auth_backends::{
    ensure_api_keys_table_pool, ApiKeyBackend, AuthBackend, AuthError, ModelBackend,
};
use rustango::tenancy::tenant_console::{
    encode, PasswordFingerprint, SessionSecret, TenantSessionPayload, COOKIE_NAME,
};
use rustango::tenancy::{
    admin::TenantAdminBuilder, routes::RouteConfig, ChainResolver, CurrentUser, Operator, Org,
    OrgResolver, RouterAuthExt, SubdomainResolver, TenancyError, TenantPools, User,
};
use tower::ServiceExt as _;

static SUITE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static UNIQ: AtomicU64 = AtomicU64::new(0);

const IP_LIMIT: u32 = 3;
const PASS: &str = "right-pass-123";

fn unique(prefix: &str) -> String {
    format!(
        "{prefix}{}x{}",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::SeqCst)
    )
}

/// A fresh address in a /16 no other test uses.
fn next_ip() -> String {
    let n = UNIQ.fetch_add(1, Ordering::SeqCst);
    format!("10.{}.{}.{}", 80 + (n >> 16) % 100, (n >> 8) & 255, n & 255)
}

fn limits() {
    let _ = configure_shared(LoginThrottle::new(LoginLimits {
        ip_limit: IP_LIMIT,
        ..LoginLimits::default()
    }));
    let _ = rustango::passwords::configure_hash_wait(Duration::from_millis(50));
}

#[derive(Clone)]
struct FixedResolver(Org);

#[async_trait::async_trait]
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
    /// Operator console.
    console: Router,
    /// Tenant admin (legacy routes).
    admin: Router,
    /// The same admin as `server::Builder` mounts it.
    served: Router,
    /// `/whoami` behind Basic + API key, and the JWT `/api/auth/*`.
    api: Router,
    registry: Pool,
    tenant: Pool,
    secret: SessionSecret,
    slug: String,
    host: String,
    _dir: tempfile::TempDir,
}

async fn whoami(CurrentUser(user): CurrentUser) -> axum::response::Response {
    match user {
        Some(u) => (StatusCode::OK, u.username).into_response(),
        None => (StatusCode::UNAUTHORIZED, "anonymous").into_response(),
    }
}

async fn boot() -> Env {
    boot_with(|_| {}).await
}

/// [`boot`] with the JWT router's config adjusted.
async fn boot_with(jwt_cfg: impl FnOnce(&mut rustango::tenancy::auth_routes::Config)) -> Env {
    limits();
    let dir = tempfile::tempdir().expect("tempdir");
    let reg_url = format!("sqlite://{}?mode=rwc", dir.path().join("reg.db").display());
    let tenant_url = format!("sqlite://{}?mode=rwc", dir.path().join("t.db").display());
    let pools = Arc::new(TenantPools::<sqlx::Sqlite>::new(
        sqlx::SqlitePool::connect(&reg_url).await.expect("registry"),
    ));
    let migrations = dir.path().join("migrations");
    std::fs::create_dir_all(&migrations).unwrap();
    let mut out = Vec::new();
    rustango::tenancy::manage::run_with_writer(
        pools.as_ref(),
        &reg_url,
        &migrations,
        vec!["migrate-registry".to_owned()],
        &mut out,
    )
    .await
    .expect("migrate-registry");
    let registry = pools.registry_pool();

    let slug = unique("t");
    let host = format!("{slug}.app.test");
    let org = Org {
        slug: slug.clone(),
        storage_mode: "database".into(),
        backend_kind: "sqlite".into(),
        database_url: Some(tenant_url.clone()),
        host_pattern: Some(host.clone()),
        ..rustango::testkit::org()
    };
    let mut row = org.clone();
    row.insert_pool(&registry).await.expect("seed org");

    let tenant = Pool::connect(&tenant_url).await.expect("tenant");
    rustango::testkit::create_tables_for::<User>(&tenant)
        .await
        .expect("users table");
    ensure_api_keys_table_pool(&tenant)
        .await
        .expect("api keys table");

    let secret = SessionSecret::from_bytes(b"tenant-admin-session-secret-32b!".to_vec());
    let admin = TenantAdminBuilder::new(
        pools.clone(),
        reg_url.clone(),
        ChainResolver::new().push(SubdomainResolver::new("app.test")),
    )
    .routes(RouteConfig::legacy())
    .with_session(secret.clone())
    .build();
    let served = rustango::server::Builder::<sqlx::Sqlite>::from_pool(
        sqlx::SqlitePool::connect(&reg_url).await.expect("registry"),
        reg_url.clone(),
        "app.test",
    )
    .routes(RouteConfig::legacy())
    .into_router()
    .await
    .expect("assemble");
    let console = rustango::tenancy::operator_console::router(
        registry.clone(),
        rustango::tenancy::operator_console::SessionSecret::from_bytes(vec![9u8; 32]),
    );

    let ctx = Arc::new(TenantContext::<sqlx::Sqlite> {
        pools: pools.clone(),
        resolver: ChainResolver::new().push(FixedResolver(org)),
        session_secret: secret.clone(),
        operator_secret: secret.clone(),
    });
    let mut cfg = rustango::tenancy::auth_routes::Config {
        session_secret: Some(b"login_limits_jwt_secret_32_bytes!!".to_vec()),
        ..rustango::tenancy::auth_routes::Config::default()
    };
    jwt_cfg(&mut cfg);
    let jwt = rustango::tenancy::auth_routes::JwtAuth::new(cfg);
    let backends: Vec<Arc<dyn AuthBackend>> = vec![Arc::new(ModelBackend), Arc::new(ApiKeyBackend)];
    let api = Router::new()
        .route("/whoami", get(whoami))
        .require_auth(backends)
        .merge(jwt.router_for::<sqlx::Sqlite>())
        .merge(
            Router::new()
                .route("/bearer", get(|| async { "ok" }))
                .route_layer(axum::middleware::from_fn_with_state(
                    jwt.clone(),
                    rustango::tenancy::auth_routes::require_bearer_for::<sqlx::Sqlite>,
                )),
        )
        .layer(axum::middleware::from_fn(
            move |mut req: Request<Body>, next: axum::middleware::Next| {
                let ctx = ctx.clone();
                async move {
                    req.extensions_mut().insert(ctx);
                    next.run(req).await
                }
            },
        ));

    Env {
        console,
        admin,
        served,
        api,
        registry,
        tenant,
        secret,
        slug,
        host,
        _dir: dir,
    }
}

impl Env {
    async fn user(&self, name: &str) -> i64 {
        let mut u = User {
            username: name.to_owned(),
            password_hash: rustango::tenancy::password::hash(PASS).unwrap(),
            // No permission tables here; a superuser skips the lookup.
            is_superuser: true,
            ..rustango::testkit::user()
        };
        u.insert_pool(&self.tenant).await.expect("seed user");
        u.id.get().copied().unwrap()
    }

    async fn operator(&self, name: &str) {
        let mut op = Operator {
            id: Auto::default(),
            username: name.to_owned(),
            password_hash: rustango::tenancy::password::hash(PASS).unwrap(),
            active: true,
            created_at: chrono::Utc::now(),
            password_changed_at: None,
            sessions_revoked_at: None,
        };
        op.insert_pool(&self.registry).await.expect("seed operator");
    }

    async fn console_login(&self, ip: &str, user: &str, pass: &str) -> axum::response::Response {
        let req = Request::builder()
            .method("POST")
            .uri("/login")
            .header(header::COOKIE, "rustango_csrf=t")
            .header("x-csrf-token", "t")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(format!("username={user}&password={pass}")))
            .unwrap();
        send(&self.console, ip, req).await
    }

    async fn admin_login(&self, ip: &str, user: &str, pass: &str) -> axum::response::Response {
        send(&self.admin, ip, self.admin_login_req(user, pass)).await
    }

    fn admin_login_req(&self, user: &str, pass: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/__login")
            .header(header::HOST, &self.host)
            .header(header::COOKIE, "rustango_csrf=t")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(format!(
                "_csrf=t&username={user}&password={pass}"
            )))
            .unwrap()
    }

    async fn jwt_login(&self, ip: &str, user: &str, pass: &str) -> axum::response::Response {
        let req = Request::builder()
            .method("POST")
            .uri("/api/auth/login")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({"username": user, "password": pass}).to_string(),
            ))
            .unwrap();
        send(&self.api, ip, req).await
    }

    async fn basic(&self, ip: &str, user: &str, pass: &str) -> axum::response::Response {
        let req = Request::builder()
            .uri("/whoami")
            .header(
                header::AUTHORIZATION,
                format!("Basic {}", b64(&format!("{user}:{pass}"))),
            )
            .body(Body::empty())
            .unwrap();
        send(&self.api, ip, req).await
    }

    async fn api_key(&self, ip: &str, token: &str) -> axum::response::Response {
        let req = Request::builder()
            .uri("/whoami")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        send(&self.api, ip, req).await
    }
}

async fn send(app: &Router, ip: &str, mut req: Request<Body>) -> axum::response::Response {
    let addr: SocketAddr = format!("{ip}:4000").parse().unwrap();
    req.extensions_mut().insert(ConnectInfo(addr));
    tokio::time::timeout(Duration::from_secs(30), app.clone().oneshot(req))
        .await
        .expect("request must not wait forever")
        .unwrap()
}

fn b64(input: &str) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = input.as_bytes();
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
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

fn is_429(r: &axum::response::Response) -> bool {
    r.status() == StatusCode::TOO_MANY_REQUESTS && r.headers().contains_key(header::RETRY_AFTER)
}

fn is_503(r: &axum::response::Response) -> bool {
    r.status() == StatusCode::SERVICE_UNAVAILABLE && r.headers().contains_key(header::RETRY_AFTER)
}

/// Five failures lock the operator; the sixth attempt, even with the
/// right password, gets 429.
#[tokio::test]
async fn operator_console_locks_with_429() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    let op = unique("op");
    env.operator(&op).await;
    for _ in 0..5 {
        let r = env.console_login(&next_ip(), &op, "wrong").await;
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
    }
    assert!(is_429(&env.console_login(&next_ip(), &op, PASS).await));
}

#[tokio::test]
async fn tenant_admin_login_locks_with_429() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    let name = unique("ta");
    env.user(&name).await;
    for _ in 0..5 {
        let r = env.admin_login(&next_ip(), &name, "wrong").await;
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
    }
    assert!(is_429(&env.admin_login(&next_ip(), &name, PASS).await));
}

/// Through `server::Builder`, failures from one IP get 429 once the
/// per-IP limit is reached: the client address must reach the gate.
fn change_password_req(uri: &str, cookie: &str, current: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::COOKIE, format!("rustango_csrf=t; {cookie}"))
        .header("x-csrf-token", "t")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(format!(
            "current_password={current}&new_password=another-pass-9&confirm_password=another-pass-9"
        )))
        .unwrap()
}

/// Wrong current passwords on change-password lock the account like
/// failed logins, so a stolen session cannot guess it (#1873).
#[tokio::test]
async fn change_password_misses_lock_the_account() {
    let _g = SUITE.lock().await;
    let env = boot().await;

    let op = unique("cpop");
    env.operator(&op).await;
    let r = env.console_login(&next_ip(), &op, PASS).await;
    let op_cookie = r.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    for _ in 0..5 {
        let req = change_password_req("/change-password", &op_cookie, "wrong");
        assert_eq!(
            send(&env.console, &next_ip(), req).await.status(),
            StatusCode::SEE_OTHER
        );
    }
    let req = change_password_req("/change-password", &op_cookie, PASS);
    assert!(
        is_429(&send(&env.console, &next_ip(), req).await),
        "operator"
    );
    assert!(is_429(&env.console_login(&next_ip(), &op, PASS).await));

    let name = unique("cpta");
    let uid = env.user(&name).await;
    let hash = User::objects()
        .where_(User::id.eq(uid))
        .fetch(&env.tenant)
        .await
        .unwrap()
        .remove(0)
        .password_hash;
    let payload = TenantSessionPayload::new(
        uid,
        &env.slug,
        3600,
        PasswordFingerprint::of(&env.secret, &hash),
    );
    let cookie = format!("{COOKIE_NAME}={}", encode(&env.secret, &payload));
    let tenant_req = |current: &str| {
        let mut req = change_password_req("/__change-password", &cookie, current);
        req.headers_mut()
            .insert(header::HOST, env.host.parse().unwrap());
        req
    };
    for _ in 0..5 {
        let r = send(&env.admin, &next_ip(), tenant_req("wrong")).await;
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
    }
    let r = send(&env.admin, &next_ip(), tenant_req(PASS)).await;
    assert!(is_429(&r), "tenant: {}", r.status());
    assert!(is_429(&env.admin_login(&next_ip(), &name, PASS).await));
}

#[tokio::test]
async fn served_tenant_admin_limits_per_ip() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    let ip = next_ip();
    for n in 0..IP_LIMIT {
        let r = send(&env.served, &ip, env.admin_login_req(&unique("ghost"), "x")).await;
        assert_eq!(r.status(), StatusCode::SEE_OTHER, "attempt {n}");
    }
    let r = send(&env.served, &ip, env.admin_login_req(&unique("ghost"), "x")).await;
    assert!(is_429(&r), "got {}", r.status());
}

#[tokio::test]
async fn jwt_login_locks_with_429() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    let name = unique("jw");
    env.user(&name).await;
    let ok = env.jwt_login(&next_ip(), &name, PASS).await;
    assert_eq!(ok.status(), StatusCode::OK, "control: the login works");
    for _ in 0..5 {
        let r = env.jwt_login(&next_ip(), &name, "wrong").await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }
    assert!(is_429(&env.jwt_login(&next_ip(), &name, PASS).await));
}

/// #1778 — refresh, me, logout and `require_bearer_for` serve a SQLite
/// tenant, also in a build where `postgres` is the default backend.
#[tokio::test]
async fn jwt_routes_serve_a_non_default_backend() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    let name = unique("jr");
    env.user(&name).await;
    let r = env.jwt_login(&next_ip(), &name, PASS).await;
    assert_eq!(r.status(), StatusCode::OK);
    let tokens = json_body(r).await;
    let refresh = tokens["refresh"].as_str().unwrap().to_owned();
    let call = |method: &str, uri: &str, bearer: Option<&str>, body: serde_json::Value| {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(b) = bearer {
            req = req.header(header::AUTHORIZATION, format!("Bearer {b}"));
        }
        req.body(Body::from(body.to_string())).unwrap()
    };

    let r = send(
        &env.api,
        &next_ip(),
        call(
            "POST",
            "/api/auth/refresh",
            None,
            serde_json::json!({ "refresh": refresh }),
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK, "refresh");
    let rotated = json_body(r).await;
    let access = rotated["access"].as_str().unwrap().to_owned();
    let refresh = rotated["refresh"].as_str().unwrap().to_owned();

    let r = send(
        &env.api,
        &next_ip(),
        call("GET", "/api/auth/me", Some(&access), serde_json::json!({})),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK, "me");
    let r = send(
        &env.api,
        &next_ip(),
        call("GET", "/bearer", Some(&access), serde_json::json!({})),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK, "require_bearer_for");

    let r = send(
        &env.api,
        &next_ip(),
        call(
            "POST",
            "/api/auth/logout",
            Some(&access),
            serde_json::json!({ "refresh": refresh }),
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT, "logout");
    let r = send(
        &env.api,
        &next_ip(),
        call("GET", "/bearer", Some(&access), serde_json::json!({})),
    )
    .await;
    assert_eq!(
        r.status(),
        StatusCode::UNAUTHORIZED,
        "logout revoked the bearer"
    );
    let r = send(
        &env.api,
        &next_ip(),
        call(
            "POST",
            "/api/auth/refresh",
            None,
            serde_json::json!({ "refresh": refresh }),
        ),
    )
    .await;
    assert_eq!(
        r.status(),
        StatusCode::UNAUTHORIZED,
        "logout revoked the refresh"
    );
}

/// A logout elsewhere stamps `sessions_revoked_at`: the access token
/// stops working at once, not at its expiry (#2086).
#[tokio::test]
async fn a_logout_elsewhere_ends_the_access_token() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    let name = unique("rv");
    let id = env.user(&name).await;
    let r = env.jwt_login(&next_ip(), &name, PASS).await;
    assert_eq!(r.status(), StatusCode::OK);
    let access = json_body(r).await["access"].as_str().unwrap().to_owned();
    let get = |uri: &str| {
        Request::builder()
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {access}"))
            .body(Body::empty())
            .unwrap()
    };
    let r = send(&env.api, &next_ip(), get("/bearer")).await;
    assert_eq!(r.status(), StatusCode::OK, "control");

    let mut user = User::objects()
        .where_(User::id.eq(id))
        .fetch(&env.tenant)
        .await
        .unwrap()
        .remove(0);
    user.sessions_revoked_at = Some(chrono::Utc::now());
    user.save_pool(&env.tenant)
        .await
        .expect("log out everywhere");

    let r = send(&env.api, &next_ip(), get("/bearer")).await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "require_bearer_for");
    let r = send(&env.api, &next_ip(), get("/api/auth/me")).await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "me");
}

impl Env {
    /// Log `name` in through `/api/auth/login`; the access token.
    async fn access(&self, name: &str) -> String {
        let r = self.jwt_login(&next_ip(), name, PASS).await;
        assert_eq!(r.status(), StatusCode::OK, "login {name}");
        json_body(r).await["access"].as_str().unwrap().to_owned()
    }

    /// `GET uri` with `token`; the status.
    async fn bearer_status(&self, uri: &str, token: &str) -> StatusCode {
        let req = Request::builder()
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        send(&self.api, &next_ip(), req).await.status()
    }

    async fn user_row(&self, id: i64) -> User {
        User::objects()
            .where_(User::id.eq(id))
            .fetch(&self.tenant)
            .await
            .unwrap()
            .remove(0)
    }
}

/// A password change ends the access token at once (#2086).
#[tokio::test]
async fn a_password_change_ends_the_access_token() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    let name = unique("pw");
    let id = env.user(&name).await;
    let access = env.access(&name).await;
    assert_eq!(env.bearer_status("/bearer", &access).await, StatusCode::OK);

    let mut user = env.user_row(id).await;
    user.password_hash = rustango::tenancy::password::hash("another-password").unwrap();
    user.password_changed_at = Some(chrono::Utc::now());
    user.save_pool(&env.tenant).await.expect("change password");

    assert_eq!(
        env.bearer_status("/bearer", &access).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        env.bearer_status("/api/auth/me", &access).await,
        StatusCode::UNAUTHORIZED
    );
}

/// A tenant-pinned token not minted by `/login` has no session to check,
/// so it is refused (#2086).
#[tokio::test]
async fn a_token_without_a_session_is_refused() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    let id = env.user(&unique("ns")).await;
    let jwt =
        rustango::tenancy::auth_routes::JwtAuth::new(rustango::tenancy::auth_routes::Config {
            session_secret: Some(b"login_limits_jwt_secret_32_bytes!!".to_vec()),
            ..rustango::tenancy::auth_routes::Config::default()
        });
    let mut custom = serde_json::Map::new();
    custom.insert("tenant".into(), env.slug.clone().into());
    let token = jwt.lifecycle().issue_access_with(id, custom).unwrap();
    assert_eq!(
        env.bearer_status("/bearer", &token).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        env.bearer_status("/api/auth/me", &token).await,
        StatusCode::UNAUTHORIZED
    );
}

/// Same user id, same password hash, other tenant: only the `tenant`
/// claim refuses the token (#2086 review).
#[tokio::test]
async fn a_token_from_another_tenant_is_refused_with_the_same_hash() {
    let _g = SUITE.lock().await;
    let acme = boot().await;
    let globex = boot().await;
    let name = unique("xt");
    let acme_id = acme.user(&name).await;
    let globex_id = globex.user(&name).await;
    assert_eq!(acme_id, globex_id, "both tenant databases start at id 1");
    let token = globex.access(&name).await;

    let mut row = acme.user_row(acme_id).await;
    row.password_hash = globex.user_row(globex_id).await.password_hash;
    row.save_pool(&acme.tenant).await.expect("same hash");

    assert_eq!(
        globex.bearer_status("/bearer", &token).await,
        StatusCode::OK
    );
    assert_eq!(
        acme.bearer_status("/bearer", &token).await,
        StatusCode::UNAUTHORIZED
    );
}

impl Env {
    /// `POST /api/auth/refresh` with `token`; the response.
    async fn refresh(&self, token: &str) -> axum::response::Response {
        let req = Request::builder()
            .method("POST")
            .uri("/api/auth/refresh")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({ "refresh": token }).to_string(),
            ))
            .unwrap();
        send(&self.api, &next_ip(), req).await
    }

    /// Both endpoints that take a bearer give `want` for `token`.
    async fn assert_bearer(&self, token: &str, want: StatusCode, what: &str) {
        for uri in ["/bearer", "/api/auth/me"] {
            assert_eq!(self.bearer_status(uri, token).await, want, "{what}: {uri}");
        }
    }
}

/// A replayed refresh token revokes its family, and with it every access
/// token of that chain, at once (#2119).
#[tokio::test]
async fn a_revoked_refresh_family_ends_its_access_tokens() {
    let _g = SUITE.lock().await;
    // No grace: the first replay counts as theft.
    let env = boot_with(|c| c.refresh_reuse_grace_secs = 0).await;
    let name = unique("fam");
    env.user(&name).await;
    let login = json_body(env.jwt_login(&next_ip(), &name, PASS).await).await;
    let first = login["access"].as_str().unwrap().to_owned();
    let refresh = login["refresh"].as_str().unwrap().to_owned();
    let r = env.refresh(&refresh).await;
    assert_eq!(r.status(), StatusCode::OK, "rotate");
    let rotated = json_body(r).await["access"].as_str().unwrap().to_owned();
    env.assert_bearer(&first, StatusCode::OK, "control").await;
    env.assert_bearer(&rotated, StatusCode::OK, "control").await;

    let r = env.refresh(&refresh).await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "replay");

    env.assert_bearer(&first, StatusCode::UNAUTHORIZED, "login access")
        .await;
    env.assert_bearer(&rotated, StatusCode::UNAUTHORIZED, "rotated access")
        .await;
}

/// Past the absolute session cap the access token is refused too (#2119).
#[tokio::test]
async fn the_session_cap_ends_the_access_token() {
    let _g = SUITE.lock().await;
    let env = boot_with(|c| c.refresh_absolute_ttl_secs = 2).await;
    let name = unique("cap");
    env.user(&name).await;
    let access = env.access(&name).await;
    env.assert_bearer(&access, StatusCode::OK, "control").await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    env.assert_bearer(&access, StatusCode::UNAUTHORIZED, "capped")
        .await;
}

/// Logout ends every access token of that login, not only the bearer sent.
#[tokio::test]
async fn logout_ends_every_access_token_of_the_login() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    let name = unique("lo");
    env.user(&name).await;
    let login = json_body(env.jwt_login(&next_ip(), &name, PASS).await).await;
    let first = login["access"].as_str().unwrap().to_owned();
    let r = env.refresh(login["refresh"].as_str().unwrap()).await;
    assert_eq!(r.status(), StatusCode::OK, "rotate");
    let rotated = json_body(r).await["access"].as_str().unwrap().to_owned();
    env.assert_bearer(&first, StatusCode::OK, "control").await;

    let logout = Request::builder()
        .method("POST")
        .uri("/api/auth/logout")
        .header(header::AUTHORIZATION, format!("Bearer {rotated}"))
        .body(Body::empty())
        .unwrap();
    let r = send(&env.api, &next_ip(), logout).await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT, "logout");
    env.assert_bearer(&first, StatusCode::UNAUTHORIZED, "earlier access")
        .await;
}

/// #2419 — logout with a dead access token and a rotated refresh still
/// ends the refresh family, so a thief's rotated token stops refreshing.
#[tokio::test]
async fn logout_with_a_rotated_refresh_ends_the_family() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    let name = unique("fam");
    env.user(&name).await;
    let login = json_body(env.jwt_login(&next_ip(), &name, PASS).await).await;
    let stolen = login["refresh"].as_str().unwrap().to_owned();
    // The thief rotates the stolen token first.
    let r = env.refresh(&stolen).await;
    assert_eq!(r.status(), StatusCode::OK, "thief rotates");
    let thief = json_body(r).await["refresh"].as_str().unwrap().to_owned();

    let logout = Request::builder()
        .method("POST")
        .uri("/api/auth/logout")
        .header(header::AUTHORIZATION, "Bearer expired.access.token")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::json!({ "refresh": stolen }).to_string(),
        ))
        .unwrap();
    let r = send(&env.api, &next_ip(), logout).await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT, "logout");
    let r = env.refresh(&thief).await;
    assert_eq!(
        r.status(),
        StatusCode::UNAUTHORIZED,
        "thief's token still refreshes"
    );
}

/// #2419 — logout with only an expired, signed access token ends its
/// family: the thief's rotated refresh stops refreshing.
#[tokio::test]
async fn logout_with_an_expired_access_token_ends_the_family() {
    let _g = SUITE.lock().await;
    let env = boot_with(|c| c.access_ttl_secs = 1).await;
    let name = unique("exp");
    env.user(&name).await;
    let login = json_body(env.jwt_login(&next_ip(), &name, PASS).await).await;
    let access = login["access"].as_str().unwrap().to_owned();
    let r = env.refresh(login["refresh"].as_str().unwrap()).await;
    assert_eq!(r.status(), StatusCode::OK, "thief rotates");
    let thief = json_body(r).await["refresh"].as_str().unwrap().to_owned();
    tokio::time::sleep(Duration::from_secs(2)).await;
    env.assert_bearer(&access, StatusCode::UNAUTHORIZED, "expired")
        .await;

    let logout = Request::builder()
        .method("POST")
        .uri("/api/auth/logout")
        .header(header::AUTHORIZATION, format!("Bearer {access}"))
        .body(Body::empty())
        .unwrap();
    let r = send(&env.api, &next_ip(), logout).await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT, "logout");
    let r = env.refresh(&thief).await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "family still alive");
}

/// #2419 — another tenant's refresh token posted to this tenant's logout
/// does not end its family, even when both share one JTI store.
#[tokio::test]
async fn logout_does_not_end_another_tenants_family() {
    let _g = SUITE.lock().await;
    let store: Arc<dyn rustango::jti_store::JtiStore> =
        Arc::new(rustango::jti_store::InMemoryJtiStore::new());
    let acme = boot_with(|c| c.jti_store = Some(store.clone())).await;
    let globex = boot_with(|c| c.jti_store = Some(store.clone())).await;
    let name = unique("xf");
    globex.user(&name).await;
    let login = json_body(globex.jwt_login(&next_ip(), &name, PASS).await).await;
    let refresh = login["refresh"].as_str().unwrap().to_owned();

    let logout = Request::builder()
        .method("POST")
        .uri("/api/auth/logout")
        .header(header::AUTHORIZATION, "Bearer junk")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::json!({ "refresh": refresh }).to_string(),
        ))
        .unwrap();
    let _ = send(&acme.api, &next_ip(), logout).await;
    let r = globex.refresh(&refresh).await;
    assert_eq!(
        r.status(),
        StatusCode::OK,
        "acme's logout ended globex's family"
    );
}

async fn json_body(r: axum::response::Response) -> serde_json::Value {
    let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    serde_json::from_slice(&b).unwrap()
}

/// Basic failures from one IP get 429 through the middleware; normal
/// Basic traffic from one IP is never limited.
#[tokio::test]
async fn basic_counts_only_failures_per_ip() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    let name = unique("svc");
    env.user(&name).await;
    let ip = next_ip();
    for n in 0..(IP_LIMIT * 4) {
        let r = env.basic(&ip, &name, PASS).await;
        assert_eq!(r.status(), StatusCode::OK, "request {n}");
    }
    let bad = next_ip();
    for n in 0..IP_LIMIT {
        let r = env.basic(&bad, &format!("ghost{n}"), "x").await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }
    assert!(is_429(&env.basic(&bad, &name, PASS).await));
}

/// Failed form logins do not lock the same user's HTTP Basic access.
#[tokio::test]
async fn form_failures_do_not_lock_basic() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    let name = unique("svc");
    env.user(&name).await;
    for _ in 0..5 {
        let _ = env.admin_login(&next_ip(), &name, "wrong").await;
    }
    assert!(is_429(&env.admin_login(&next_ip(), &name, PASS).await));
    let r = env.basic(&next_ip(), &name, PASS).await;
    assert_eq!(r.status(), StatusCode::OK, "Basic shares the form lock");
}

/// Bad API keys from one IP get 429; a good key is not counted.
#[tokio::test]
async fn api_key_failures_are_limited_per_ip() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    let uid = env.user(&unique("key")).await;
    let good = rustango::tenancy::auth_backends::create_api_key(uid, "t", None, &env.tenant)
        .await
        .unwrap();
    let ip = next_ip();
    for _ in 0..(IP_LIMIT * 2) {
        assert_eq!(env.api_key(&ip, &good).await.status(), StatusCode::OK);
    }
    for _ in 0..IP_LIMIT {
        let r = env.api_key(&ip, "abcdef12.0000000000000000").await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }
    assert!(is_429(&env.api_key(&ip, &good).await));
}

/// #2250 — keys whose random prefixes collide each still authenticate.
#[tokio::test]
async fn api_keys_sharing_a_prefix_each_authenticate() {
    use rustango::tenancy::auth_backends::{create_api_key, ApiKey};
    let _g = SUITE.lock().await;
    let env = boot().await;
    let uid = env.user(&unique("pfx")).await;
    let first = create_api_key(uid, "a", None, &env.tenant).await.unwrap();
    let second = create_api_key(uid, "b", None, &env.tenant).await.unwrap();
    let (prefix, _) = first.split_once('.').unwrap();
    let (old, secret) = second.split_once('.').unwrap();
    let mut row = ApiKey::objects()
        .where_(ApiKey::key_prefix.eq(old.to_owned()))
        .fetch(&env.tenant)
        .await
        .unwrap()
        .remove(0);
    row.key_prefix = prefix.to_owned();
    row.save_pool(&env.tenant).await.unwrap();

    let second = format!("{prefix}.{secret}");
    assert_eq!(
        env.api_key(&next_ip(), &first).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        env.api_key(&next_ip(), &second).await.status(),
        StatusCode::OK
    );
}

/// #1729 — an expired key is verified before it is refused, so a full
/// hash queue answers it 503, the same as an unknown prefix.
#[cfg(feature = "testkit")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_expired_api_key_costs_a_verify_like_an_unknown_one() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    let uid = env.user(&unique("exp")).await;
    let past = chrono::Utc::now() - chrono::Duration::hours(1);
    let expired =
        rustango::tenancy::auth_backends::create_api_key(uid, "t", Some(past), &env.tenant)
            .await
            .unwrap();
    let r = env.api_key(&next_ip(), &expired).await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);

    let held = rustango::passwords::hold_all_hash_slots().await;
    let unknown = env.api_key(&next_ip(), "abcdef12.0000000000000000").await;
    let exp = env.api_key(&next_ip(), &expired).await;
    drop(held);
    assert!(is_503(&unknown), "unknown prefix: {}", unknown.status());
    assert!(
        is_503(&exp),
        "expired key skipped the verify: {}",
        exp.status()
    );
}

/// A backend called without a tenant refuses instead of sharing one
/// lock scope across tenants.
#[tokio::test]
async fn basic_without_a_tenant_is_refused() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    let name = unique("nos");
    env.user(&name).await;
    let (parts, ()) = Request::builder()
        .header(
            header::AUTHORIZATION,
            format!("Basic {}", b64(&format!("{name}:{PASS}"))),
        )
        .body(())
        .unwrap()
        .into_parts();
    let r = ModelBackend.authenticate(&parts, &env.tenant).await;
    assert!(matches!(r, Err(AuthError::InvalidToken)), "{r:?}");
}

/// With every hashing slot taken, each login answers 503 + Retry-After
/// instead of queueing, and so do the password changes.
#[cfg(feature = "testkit")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_full_hash_queue_answers_503_everywhere() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    let name = unique("busy");
    let uid = env.user(&name).await;
    let op = unique("bop");
    env.operator(&op).await;

    // Sessions first, while hashing is free.
    let r = env.console_login(&next_ip(), &op, PASS).await;
    let op_cookie = r.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let hash = User::objects()
        .where_(User::id.eq(uid))
        .fetch(&env.tenant)
        .await
        .unwrap()
        .remove(0)
        .password_hash;
    let payload = TenantSessionPayload::new(
        uid,
        &env.slug,
        3600,
        PasswordFingerprint::of(&env.secret, &hash),
    );
    let tenant_cookie = format!("{COOKIE_NAME}={}", encode(&env.secret, &payload));
    #[cfg(feature = "mcp")]
    let agent = {
        rustango::testkit::create_tables_for::<rustango::tenancy::Agent>(&env.tenant)
            .await
            .unwrap();
        rustango::tenancy::create_agent_pool(&env.tenant, "busy-bot")
            .await
            .unwrap()
    };

    // Every slot held, so no request gets one within the wait (#1786).
    let held = rustango::passwords::hold_all_hash_slots().await;

    let change = "current_password=right-pass-123&new_password=another-pass-9&confirm_password=another-pass-9";
    let console_change = env
        .console
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/change-password")
                .header(header::COOKIE, format!("rustango_csrf=t; {op_cookie}"))
                .header("x-csrf-token", "t")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(change))
                .unwrap(),
        )
        .await
        .unwrap();
    let admin_change = env
        .admin
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/__change-password")
                .header(header::HOST, &env.host)
                .header(header::COOKIE, format!("rustango_csrf=t; {tenant_cookie}"))
                .header("x-csrf-token", "t")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(change))
                .unwrap(),
        )
        .await
        .unwrap();
    let answers = [
        (
            "operator login",
            env.console_login(&next_ip(), &op, PASS).await,
        ),
        (
            "tenant admin login",
            env.admin_login(&next_ip(), &name, PASS).await,
        ),
        ("basic", env.basic(&next_ip(), &name, PASS).await),
        ("operator change-password", console_change),
        ("tenant change-password", admin_change),
        ("jwt login", env.jwt_login(&next_ip(), &name, PASS).await),
    ];
    #[cfg(feature = "mcp")]
    let agent_auth =
        rustango::tenancy::authenticate_agent_pool(&env.tenant, "busy-bot", &agent.token).await;
    #[cfg(feature = "mcp")]
    let raw_auth =
        rustango::mcp::verify_raw_agent_credential(&env.tenant, &env.slug, &agent.token).await;
    drop(held);
    for (what, r) in answers {
        let status = r.status();
        let retry = r.headers().contains_key(header::RETRY_AFTER);
        let body = axum::body::to_bytes(r.into_body(), 4096).await.unwrap();
        assert!(
            status == StatusCode::SERVICE_UNAVAILABLE && retry,
            "{what}: {status} {}",
            String::from_utf8_lossy(&body)
        );
    }
    #[cfg(feature = "mcp")]
    assert!(
        matches!(
            agent_auth,
            Err(rustango::tenancy::AgentError::Tenancy(TenancyError::Busy))
        ),
        "agent secret check must report busy, not a bad secret: {agent_auth:?}"
    );
    #[cfg(feature = "mcp")]
    assert!(
        matches!(
            raw_auth,
            Err(rustango::tenancy::AgentError::Tenancy(TenancyError::Busy))
        ),
        "raw MCP key must report busy (503), not a refused key (401): {raw_auth:?}"
    );

    // Once the queue drains, the user logs in again.
    assert_eq!(
        env.basic(&next_ip(), &name, PASS).await.status(),
        StatusCode::OK
    );
}
