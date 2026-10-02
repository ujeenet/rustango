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

/// #1338: a password change in the same second as the login still ends
/// that session, and a login made after the change works.
#[tokio::test]
async fn a_password_change_in_the_login_second_ends_that_session() {
    use rustango::tenancy::tenant_console::{
        encode, PasswordFingerprint, TenantSessionPayload, COOKIE_NAME,
    };

    let url = "sqlite:file:session_user_1338?mode=memory&cache=shared";
    let tenant = rustango::sql::Pool::connect(url).await.expect("tenant db");
    rustango::testkit::create_tables_for::<rustango::tenancy::User>(&tenant)
        .await
        .expect("users table");
    let mut user = rustango::tenancy::User {
        password_hash: rustango::tenancy::password::hash("first-password").unwrap(),
        ..rustango::testkit::user()
    };
    user.insert_pool(&tenant).await.expect("seed user");
    let uid = user.id.get().copied().unwrap();

    let secret = rustango::tenancy::session::SessionSecret::from_bytes(
        b"test_tenant_session_secret_32by!".to_vec(),
    );
    let registry = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("registry pool");
    let org = Org {
        database_url: Some(url.into()),
        ..fake_sqlite_org()
    };
    let ctx = Arc::new(TenantContext {
        pools: Arc::new(TenantPools::<sqlx::Sqlite>::new(registry)),
        resolver: ChainResolver::new().push(FixedResolver(org)),
        session_secret: secret.clone(),
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
    let whoami_with = |cookie: String| {
        let app = app.clone();
        async move {
            let req = Request::builder()
                .uri("/whoami")
                .header("Cookie", format!("{COOKIE_NAME}={cookie}"))
                .body(Body::empty())
                .unwrap();
            let resp = app.oneshot(req).await.unwrap();
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            String::from_utf8(body.to_vec()).unwrap()
        }
    };

    let login = TenantSessionPayload::new(
        uid,
        "acme",
        3600,
        PasswordFingerprint::of(&secret, &user.password_hash),
    );
    let old_cookie = encode(&secret, &login);
    assert_eq!(whoami_with(old_cookie.clone()).await, "user:alice");

    // Change the password, stamped in the same second the session was issued.
    user.password_hash = rustango::tenancy::password::hash("second-password").unwrap();
    user.password_changed_at = chrono::DateTime::from_timestamp(login.iat, 999_000_000);
    user.save_pool(&tenant).await.expect("change password");

    assert_eq!(
        whoami_with(old_cookie).await,
        "anon",
        "a session from before the change must not survive it"
    );

    let relogin = TenantSessionPayload::new(
        uid,
        "acme",
        3600,
        PasswordFingerprint::of(&secret, &user.password_hash),
    );
    assert_eq!(
        whoami_with(encode(&secret, &relogin)).await,
        "user:alice",
        "a login after the change must work"
    );
}

/// Mount `handler` behind a `TenantContext` on `registry`, resolving `org`.
fn app_with<H, T>(
    handler: H,
    registry: sqlx::SqlitePool,
    org: Org,
    session_secret: rustango::tenancy::session::SessionSecret,
    operator_secret: rustango::tenancy::session::SessionSecret,
) -> Router
where
    H: axum::handler::Handler<T, ()>,
    T: 'static,
{
    let ctx = Arc::new(TenantContext {
        pools: Arc::new(TenantPools::<sqlx::Sqlite>::new(registry)),
        resolver: ChainResolver::new().push(FixedResolver(org)),
        session_secret,
        operator_secret,
    });
    Router::new()
        .route("/whoami", get(handler))
        .layer(axum::middleware::from_fn(
            move |mut req: Request, next: axum::middleware::Next| {
                let ctx = ctx.clone();
                async move {
                    req.extensions_mut().insert(ctx);
                    next.run(req).await
                }
            },
        ))
}

async fn whoami_with(app: &Router, cookie: &str) -> String {
    let req = Request::builder()
        .uri("/whoami")
        .header("Cookie", cookie)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(body.to_vec()).unwrap()
}

/// `SessionOperator` authenticates a real cookie and drops it after a
/// password change in the login second.
#[tokio::test]
async fn session_operator_ends_on_a_same_second_password_change() {
    use rustango::extractors::SessionOperator;
    use rustango::tenancy::session::{encode, PasswordFingerprint, SessionPayload, COOKIE_NAME};

    async fn whoop(SessionOperator(op): SessionOperator) -> String {
        op.map_or_else(|| "anon".to_owned(), |o| format!("op:{}", o.username))
    }

    let registry =
        sqlx::SqlitePool::connect("sqlite:file:session_op_1338?mode=memory&cache=shared")
            .await
            .expect("registry pool");
    let reg = rustango::sql::Pool::from(registry.clone());
    rustango::testkit::create_tables_for::<rustango::tenancy::Operator>(&reg)
        .await
        .expect("operators table");
    let mut op = rustango::tenancy::Operator {
        id: rustango::sql::Auto::default(),
        username: "root".into(),
        password_hash: rustango::tenancy::password::hash("first-password").unwrap(),
        active: true,
        created_at: chrono::Utc::now(),
        password_changed_at: None,
        sessions_revoked_at: None,
    };
    op.insert_pool(&reg).await.expect("seed operator");

    let op_secret = rustango::tenancy::session::SessionSecret::from_bytes(
        b"test_oper_session_secret____32b!".to_vec(),
    );
    let app = app_with(
        whoop,
        registry,
        fake_sqlite_org(),
        rustango::tenancy::session::SessionSecret::from_bytes(
            b"test_tenant_session_secret_32by!".to_vec(),
        ),
        op_secret.clone(),
    );
    let login = SessionPayload::new(
        op.id.get().copied().unwrap(),
        3600,
        PasswordFingerprint::of(&op_secret, &op.password_hash),
    );
    let cookie = format!("{COOKIE_NAME}={}", encode(&op_secret, &login));
    assert_eq!(whoami_with(&app, &cookie).await, "op:root");

    op.password_hash = rustango::tenancy::password::hash("second-password").unwrap();
    op.password_changed_at = chrono::DateTime::from_timestamp(login.iat, 999_000_000);
    op.save_pool(&reg).await.expect("change password");
    assert_eq!(
        whoami_with(&app, &cookie).await,
        "anon",
        "an operator session must not survive a password change"
    );
}

/// A member cookie from `mint_cookie` (what the SSO callback sets)
/// authenticates, and a password change in the login second ends it.
#[cfg(feature = "sso")]
#[tokio::test]
async fn current_member_ends_on_a_same_second_password_change() {
    use rustango::tenancy::member_auth::{decode, mint_cookie, CurrentMember};

    async fn member(CurrentMember(user): CurrentMember) -> String {
        user.map_or_else(|| "anon".to_owned(), |u| format!("member:{}", u.username))
    }

    let url = "sqlite:file:current_member_1338?mode=memory&cache=shared";
    let tenant = rustango::sql::Pool::connect(url).await.expect("tenant db");
    rustango::testkit::create_tables_for::<rustango::tenancy::User>(&tenant)
        .await
        .expect("users table");
    let mut user = rustango::tenancy::User {
        password_hash: rustango::tenancy::password::hash("first-password").unwrap(),
        ..rustango::testkit::user()
    };
    user.insert_pool(&tenant).await.expect("seed user");

    let secret = rustango::tenancy::session::SessionSecret::from_bytes(
        b"test_tenant_session_secret_32by!".to_vec(),
    );
    let app = app_with(
        member,
        sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap(),
        Org {
            database_url: Some(url.into()),
            ..fake_sqlite_org()
        },
        secret.clone(),
        rustango::tenancy::session::SessionSecret::from_bytes(
            b"test_oper_session_secret____32b!".to_vec(),
        ),
    );
    let set_cookie = mint_cookie(&secret, &user, "acme", 3600);
    let cookie = set_cookie.split(';').next().unwrap().to_owned();
    assert_eq!(whoami_with(&app, &cookie).await, "member:alice");

    let iat = decode(&secret, "acme", cookie.split_once('=').unwrap().1)
        .unwrap()
        .iat;
    user.password_hash = rustango::tenancy::password::hash("second-password").unwrap();
    user.password_changed_at = chrono::DateTime::from_timestamp(iat, 999_000_000);
    user.save_pool(&tenant).await.expect("change password");
    assert_eq!(
        whoami_with(&app, &cookie).await,
        "anon",
        "a member session must not survive a password change"
    );

    let relogin = mint_cookie(&secret, &user, "acme", 3600);
    assert_eq!(
        whoami_with(&app, relogin.split(';').next().unwrap()).await,
        "member:alice",
        "a login after the change must work"
    );
}

/// `member_auth::logout` ends the member and tenant cookies from before
/// it, and a login in the same second still works (#1855).
#[cfg(feature = "sso")]
#[tokio::test]
async fn a_member_logout_ends_member_and_tenant_sessions() {
    use rustango::sql::FetcherPool as _;
    use rustango::tenancy::member_auth::{logout, mint_cookie, CurrentMember};
    use rustango::tenancy::tenant_console::{
        encode, PasswordFingerprint, TenantSessionPayload, COOKIE_NAME,
    };

    async fn both(SessionUser(u): SessionUser, CurrentMember(m): CurrentMember) -> String {
        format!("{}|{}", u.is_some(), m.is_some())
    }

    let url = "sqlite:file:member_logout_1855?mode=memory&cache=shared";
    let tenant = rustango::sql::Pool::connect(url).await.expect("tenant db");
    rustango::testkit::create_tables_for::<rustango::tenancy::User>(&tenant)
        .await
        .expect("users table");
    let mut user = rustango::testkit::user();
    user.insert_pool(&tenant).await.expect("seed user");
    let uid = user.id.get().copied().unwrap();

    let secret = rustango::tenancy::session::SessionSecret::from_bytes(
        b"test_tenant_session_secret_32by!".to_vec(),
    );
    let app = app_with(
        both,
        sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap(),
        Org {
            database_url: Some(url.into()),
            ..fake_sqlite_org()
        },
        secret.clone(),
        rustango::tenancy::session::SessionSecret::from_bytes(
            b"test_oper_session_secret____32b!".to_vec(),
        ),
    );
    let tenant_session = TenantSessionPayload::new(
        uid,
        "acme",
        3600,
        PasswordFingerprint::of(&secret, &user.password_hash),
    );
    let member = mint_cookie(&secret, &user, "acme", 3600);
    let cookies = format!(
        "{COOKIE_NAME}={}; {}",
        encode(&secret, &tenant_session),
        member.split(';').next().unwrap()
    );
    assert_eq!(whoami_with(&app, &cookies).await, "true|true");

    logout(&tenant, &user).await.expect("logout");
    assert_eq!(
        whoami_with(&app, &cookies).await,
        "false|false",
        "cookies from before logout must be refused"
    );

    let user = rustango::tenancy::User::objects()
        .filter("id", uid)
        .fetch(&tenant)
        .await
        .unwrap()
        .remove(0);
    let relogin = mint_cookie(&secret, &user, "acme", 3600);
    assert_eq!(
        whoami_with(&app, relogin.split(';').next().unwrap()).await,
        "false|true",
        "a member login right after logout must work"
    );
}

/// `SessionOperator` refuses a session issued at or before the logout cut-off (#1855).
#[tokio::test]
async fn session_operator_ends_at_the_logout_cutoff() {
    use rustango::extractors::SessionOperator;
    use rustango::tenancy::session::{encode, PasswordFingerprint, SessionPayload, COOKIE_NAME};

    async fn whoop(SessionOperator(op): SessionOperator) -> String {
        op.map_or_else(|| "anon".to_owned(), |o| format!("op:{}", o.username))
    }

    let registry =
        sqlx::SqlitePool::connect("sqlite:file:session_op_1855?mode=memory&cache=shared")
            .await
            .expect("registry pool");
    let reg = rustango::sql::Pool::from(registry.clone());
    rustango::testkit::create_tables_for::<rustango::tenancy::Operator>(&reg)
        .await
        .expect("operators table");
    let mut op = rustango::tenancy::Operator {
        id: rustango::sql::Auto::default(),
        username: "root".into(),
        password_hash: rustango::tenancy::password::hash("first-password").unwrap(),
        active: true,
        created_at: chrono::Utc::now(),
        password_changed_at: None,
        sessions_revoked_at: None,
    };
    op.insert_pool(&reg).await.expect("seed operator");
    let op_secret = rustango::tenancy::session::SessionSecret::from_bytes(
        b"test_oper_session_secret____32b!".to_vec(),
    );
    let app = app_with(
        whoop,
        registry,
        fake_sqlite_org(),
        rustango::tenancy::session::SessionSecret::from_bytes(
            b"test_tenant_session_secret_32by!".to_vec(),
        ),
        op_secret.clone(),
    );
    let login = SessionPayload::new(
        op.id.get().copied().unwrap(),
        3600,
        PasswordFingerprint::of(&op_secret, &op.password_hash),
    );
    let cookie = format!("{COOKIE_NAME}={}", encode(&op_secret, &login));
    assert_eq!(whoami_with(&app, &cookie).await, "op:root");

    op.sessions_revoked_at = chrono::DateTime::from_timestamp(login.iat, 0);
    op.save_pool(&reg).await.expect("log out");
    assert_eq!(
        whoami_with(&app, &cookie).await,
        "anon",
        "a session from the logout's own second must be refused"
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
