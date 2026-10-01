#![cfg(all(feature = "sqlite", feature = "tenancy", feature = "admin"))]
//! Tenant admin sessions end on a password change (#1338), for tenant
//! users and for operator impersonation (#1735). SQLite, no service.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use rustango::sql::{sqlx, Auto, FetcherPool as _};
use rustango::tenancy::tenant_console::{
    encode, PasswordFingerprint, SessionSecret, TenantSessionPayload, COOKIE_NAME,
};
use rustango::tenancy::{
    admin::TenantAdminBuilder, routes::RouteConfig, ChainResolver, Operator, Org,
    SubdomainResolver, TenantPools, User,
};
use tower::ServiceExt;

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn unique(prefix: &str) -> String {
    format!(
        "{prefix}{}x{}",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::SeqCst)
    )
}

struct Env {
    admin: axum::Router,
    pools: Arc<TenantPools<sqlx::Sqlite>>,
    registry: rustango::sql::Pool,
    tenant: rustango::sql::Pool,
    secret: SessionSecret,
    slug: String,
    host: String,
    _dir: tempfile::TempDir,
}

/// A migrated registry, one database-mode tenant with a users table,
/// and the tenant admin on legacy routes.
async fn boot() -> Env {
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
    let mut org = Org {
        slug: slug.clone(),
        storage_mode: "database".into(),
        backend_kind: "sqlite".into(),
        database_url: Some(tenant_url.clone()),
        host_pattern: Some(host.clone()),
        ..rustango::testkit::org()
    };
    org.insert_pool(&registry).await.expect("seed org");

    let tenant = rustango::sql::Pool::connect(&tenant_url)
        .await
        .expect("tenant");
    rustango::testkit::create_tables_for::<User>(&tenant)
        .await
        .expect("users table");

    let secret = SessionSecret::from_bytes(b"tenant-admin-session-secret-32b!".to_vec());
    let admin = TenantAdminBuilder::new(
        pools.clone(),
        reg_url,
        ChainResolver::new().push(SubdomainResolver::new("app.test")),
    )
    .routes(RouteConfig::legacy())
    .with_session(secret.clone())
    .build();
    Env {
        admin,
        pools,
        registry,
        tenant,
        secret,
        slug,
        host,
        _dir: dir,
    }
}

impl Env {
    async fn get(&self, uri: &str, cookie: &str) -> axum::response::Response {
        self.admin
            .clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header(header::HOST, &self.host)
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }
}

/// A password change stamped in the login second still ends a tenant
/// admin session. The old `iat < password_changed_at` check let it through.
#[tokio::test]
async fn a_password_change_in_the_login_second_ends_a_tenant_admin_session() {
    let env = boot().await;
    let mut user = User {
        is_superuser: true,
        password_hash: rustango::tenancy::password::hash("first-password").unwrap(),
        ..rustango::testkit::user()
    };
    user.insert_pool(&env.tenant).await.expect("seed user");
    let uid = user.id.get().copied().unwrap();

    let login = TenantSessionPayload::new(
        uid,
        &env.slug,
        3600,
        PasswordFingerprint::of(&env.secret, &user.password_hash),
    );
    let cookie = format!("{COOKIE_NAME}={}", encode(&env.secret, &login));
    assert_eq!(
        env.get("/__change-password", &cookie).await.status(),
        StatusCode::OK,
        "the session works before the change"
    );

    user.password_hash = rustango::tenancy::password::hash("second-password").unwrap();
    user.password_changed_at = chrono::DateTime::from_timestamp(login.iat, 999_000_000);
    user.save_pool(&env.tenant).await.expect("change password");

    let res = env.get("/__change-password", &cookie).await;
    assert_eq!(
        res.status(),
        StatusCode::SEE_OTHER,
        "a session from the change's own second must be refused"
    );
}

/// Log an operator into the console and start impersonating `env`'s
/// tenant. Returns the operator and the handoff redirect.
async fn start_impersonation(env: &Env) -> (Operator, String) {
    let password = "operator-password-1";
    let mut op = Operator {
        id: Auto::default(),
        username: unique("op"),
        password_hash: rustango::tenancy::password::hash(password).unwrap(),
        active: true,
        created_at: chrono::Utc::now(),
        password_changed_at: None,
        sessions_revoked_at: None,
    };
    op.insert_pool(&env.registry).await.expect("seed operator");

    let console = rustango::tenancy::operator_console::router_with_impersonation(
        env.registry.clone(),
        env.pools.clone(),
        SessionSecret::from_bytes(b"operator-console-secret-32bytes!".to_vec()),
        Arc::new(rustango::storage::LocalStorage::new(
            env._dir.path().join("brand"),
        )),
        env.secret.clone(),
        RouteConfig::legacy().impersonation_handoff_url,
    );
    let post = |uri: String, body: String, cookie: String| {
        let console = console.clone();
        async move {
            console
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(uri)
                        .header(header::COOKIE, format!("rustango_csrf=t; {cookie}"))
                        .header("x-csrf-token", "t")
                        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap()
        }
    };
    let first = |resp: &axum::response::Response, name| {
        resp.headers()
            .get(name)
            .unwrap_or_else(|| panic!("response has {name}"))
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned()
    };

    let login = post(
        "/login".into(),
        format!("username={}&password={password}", op.username),
        String::new(),
    )
    .await;
    let op_cookie = first(&login, "set-cookie");
    let start = post(
        format!("/orgs/{}/impersonate", env.slug),
        String::new(),
        op_cookie,
    )
    .await;
    let location = first(&start, "location");
    (op, location)
}

/// A port-routed org's handoff lands on its own port (#1933).
#[tokio::test]
async fn impersonating_a_port_routed_org_lands_on_its_port() {
    let env = boot().await;
    let mut org = Org::objects()
        .filter("slug", env.slug.as_str())
        .fetch(&env.registry)
        .await
        .unwrap()
        .remove(0);
    org.port = Some(8443);
    org.save_pool(&env.registry).await.expect("set port");
    let (_, location) = start_impersonation(&env).await;
    assert!(
        location.contains(&format!("{}:8443/", env.host)),
        "got {location}"
    );
}

/// An operator password change ends their open impersonation session.
/// The cookie is minted through the real console and handoff.
#[tokio::test]
async fn an_operator_password_change_ends_their_impersonation_session() {
    let env = boot().await;
    let (mut op, location) = start_impersonation(&env).await;
    let first = |resp: &axum::response::Response, name| {
        resp.headers()
            .get(name)
            .unwrap_or_else(|| panic!("response has {name}"))
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned()
    };
    let handoff = &location[location
        .find("/__impersonation_handoff")
        .expect("handoff url")..];

    let redeemed = env.get(handoff, "").await;
    let imp_cookie = first(&redeemed, "set-cookie");
    assert!(imp_cookie.starts_with(COOKIE_NAME), "got {imp_cookie}");
    assert_eq!(
        env.get("/__admin/", &imp_cookie).await.status(),
        StatusCode::OK,
        "the impersonation session works before the change"
    );

    // A reset: new hash, stamped now.
    op.password_hash = rustango::tenancy::password::hash("operator-password-2").unwrap();
    op.password_changed_at = Some(chrono::Utc::now());
    op.save_pool(&env.registry).await.expect("reset password");

    assert_eq!(
        env.get("/__admin/", &imp_cookie).await.status(),
        StatusCode::SEE_OTHER,
        "the impersonation session must end with the operator's password"
    );
}

/// The tenant change-password form applies the shared 8-character rule (#1874).
#[tokio::test]
async fn a_short_new_password_is_refused_by_the_tenant_admin() {
    let env = boot().await;
    let mut user = User {
        is_superuser: true,
        password_hash: rustango::tenancy::password::hash("first-password").unwrap(),
        ..rustango::testkit::user()
    };
    user.insert_pool(&env.tenant).await.expect("seed user");
    let uid = user.id.get().copied().unwrap();
    let login = TenantSessionPayload::new(
        uid,
        &env.slug,
        3600,
        PasswordFingerprint::of(&env.secret, &user.password_hash),
    );
    let cookie = format!("{COOKIE_NAME}={}", encode(&env.secret, &login));
    let res = env
        .admin
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/__change-password")
                .header(header::HOST, &env.host)
                .header(header::COOKIE, format!("rustango_csrf=t; {cookie}"))
                .header("x-csrf-token", "t")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(
                    "current_password=first-password&new_password=abc&confirm_password=abc",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let location = res
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        location.contains("error=") && location.contains("8"),
        "got {location}"
    );
    let stored: Vec<User> = User::objects().fetch(&env.tenant).await.unwrap();
    assert!(
        rustango::tenancy::password::verify("first-password", &stored[0].password_hash).unwrap(),
        "the short password must not be stored"
    );
}
