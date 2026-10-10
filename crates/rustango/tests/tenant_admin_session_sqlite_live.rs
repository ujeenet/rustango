#![cfg(all(feature = "sqlite", feature = "tenancy", feature = "admin"))]
//! Tenant admin sessions end on a password change (#1338), for tenant
//! users and for operator impersonation (#1735). SQLite, no service.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use rustango::admin::AdminSession;
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

/// Tracing's callsite interest is process-global: a sibling logging on another
/// thread can hide the handoff line from the capture test's scoped subscriber.
static SUITE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// What a user detail page saw: the audit source token and the admin session.
static SEEN: std::sync::Mutex<Vec<(String, Option<AdminSession>)>> =
    std::sync::Mutex::new(Vec::new());

fn record_actor(parts: &axum::http::request::Parts, _: Option<&serde_json::Value>) -> bool {
    let source = rustango::audit::current_source().as_token();
    let session = parts.extensions.get::<AdminSession>().cloned();
    SEEN.lock().unwrap().push((source, session));
    true
}
rustango::register_admin_object_permission!("rustango_users", "view", record_actor);

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
    _suite: tokio::sync::MutexGuard<'static, ()>,
}

/// A migrated registry, one database-mode tenant with a users table,
/// and the tenant admin on legacy routes.
async fn boot() -> Env {
    let suite = SUITE.lock().await;
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
        _suite: suite,
    }
}

impl Env {
    async fn post(&self, uri: &str, cookie: &str, body: &str) -> axum::response::Response {
        self.admin
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header(header::HOST, &self.host)
                    .header(header::COOKIE, format!("rustango_csrf=t; {cookie}"))
                    .header("x-csrf-token", "t")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(body.to_owned()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

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

/// Log an operator into the console: the operator, the console and its cookie.
async fn console_session(env: &Env) -> (Operator, axum::Router, String) {
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
    (op, console, op_cookie)
}

/// Log an operator into the console and start impersonating `env`'s
/// tenant. Returns the operator and the handoff redirect.
async fn start_impersonation(env: &Env) -> (Operator, String) {
    let (op, console, op_cookie) = console_session(env).await;
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

/// #2425 — with Secure cookies and no `RUSTANGO_TENANT_SCHEME`, the
/// handoff (a one-time login) goes to https.
#[tokio::test]
async fn the_handoff_uses_https_when_cookies_are_secure() {
    let env = boot().await;
    // Process-wide and set once; skip if an earlier test fixed it off.
    let ours = rustango::session::set_secure_cookies(true);
    if (!ours && !rustango::session::secure_cookies())
        || std::env::var("RUSTANGO_TENANT_SCHEME").is_ok_and(|s| !s.is_empty())
    {
        return;
    }
    let (_, location) = start_impersonation(&env).await;
    assert!(
        location.starts_with(&format!("https://{}", env.host)),
        "got {location}"
    );
}

/// The handoff token is a live login: the info log names the URL, not it (#2107).
#[tokio::test]
async fn the_handoff_token_is_not_logged() {
    let env = boot().await;
    let out = rustango::testkit::CaptureWriter::default();
    let writer = out.clone();
    let sub = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .with_max_level(tracing::Level::INFO)
        .finish();
    let _guard = tracing::subscriber::set_default(sub);
    let (_, location) = start_impersonation(&env).await;
    let token = location.split("token=").nth(1).expect("token in redirect");
    let logged = out.contents();
    assert!(
        logged.contains("minted impersonation handoff token"),
        "{logged}"
    );
    assert!(!logged.contains(token), "{logged}");
}

/// A path-prefix tenant's admin and handoff live under its prefix (#2059).
#[tokio::test]
async fn a_path_prefix_tenant_admin_is_served_under_its_prefix() {
    let env = boot().await;
    let mut org = Org::objects()
        .filter("slug", env.slug.as_str())
        .fetch(&env.registry)
        .await
        .unwrap()
        .remove(0);
    org.path_prefix = Some("/acme".into());
    org.save_pool(&env.registry).await.expect("set prefix");
    rustango::tenancy::invalidate_org_cache();

    let (_, location) = start_impersonation(&env).await;
    assert!(
        location.contains(&format!("{}/acme/__impersonation_handoff?", env.host)),
        "got {location}"
    );
    let handoff = &location[location.find("/acme/").expect("prefixed handoff")..];
    let redeemed = env.get(handoff, "").await;
    let set_cookie = redeemed
        .headers()
        .get("set-cookie")
        .unwrap_or_else(|| panic!("handoff should set a cookie, got {}", redeemed.status()))
        .to_str()
        .unwrap()
        .to_owned();
    // Scoped to the prefix, so a second prefix tenant on this host keeps its own (#2098).
    assert!(set_cookie.contains("Path=/acme;"), "{set_cookie}");
    let cookie = set_cookie.split(';').next().unwrap().to_owned();
    assert_eq!(
        redeemed
            .headers()
            .get("location")
            .unwrap()
            .to_str()
            .unwrap(),
        "/acme/__admin/"
    );
    let page = env.get("/acme/__admin/", &cookie).await;
    assert_eq!(page.status(), StatusCode::OK);
    let html = axum::body::to_bytes(page.into_body(), usize::MAX)
        .await
        .unwrap();
    let html = String::from_utf8_lossy(&html);
    // The sidebar link is the full route, not `{admin_prefix}{route}` (#2102).
    assert!(
        html.contains(r#"href="&#x2F;acme&#x2F;__change-password""#),
        "sidebar change-password link: {html}"
    );

    // Anonymous: the login redirect keeps the prefix.
    let anon = env.get("/acme/__admin/", "").await;
    assert!(
        anon.headers()
            .get("location")
            .is_some_and(|l| l.to_str().unwrap().starts_with("/acme/__login")),
        "{:?}",
        anon.headers().get("location")
    );

    // #2098 — end-impersonation clears the prefix cookie and a legacy `Path=/` one.
    let ended = env
        .post("/acme/__admin/__end-impersonation", &cookie, "")
        .await;
    let paths = session_cookie_paths(&ended);
    assert_eq!(paths, ["/acme", "/"], "end-impersonation clears");

    // Login and logout use the prefix path too.
    let mut user = User {
        password_hash: rustango::tenancy::password::hash("first-password").unwrap(),
        ..rustango::testkit::user()
    };
    user.insert_pool(&env.tenant).await.expect("seed user");
    let login = env
        .post(
            "/acme/__login",
            "",
            "username=alice&password=first-password&_csrf=t",
        )
        .await;
    assert_eq!(login.status(), StatusCode::SEE_OTHER);
    assert_eq!(session_cookie_paths(&login), ["/acme"], "login sets");
    let session = login.headers()["set-cookie"].to_str().unwrap();
    let session = session.split(';').next().unwrap().to_owned();
    let logout = env.post("/acme/__logout", &session, "").await;
    assert_eq!(session_cookie_paths(&logout), ["/acme"], "logout clears");
}

/// The `Path` of every tenant session `Set-Cookie` on `resp`.
fn session_cookie_paths(resp: &axum::response::Response) -> Vec<String> {
    resp.headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap())
        .filter(|v| v.starts_with(&format!("{COOKIE_NAME}=")))
        .map(|v| {
            v.split("; ")
                .find_map(|a| a.strip_prefix("Path="))
                .unwrap_or("")
                .to_owned()
        })
        .collect()
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
    let index = env.get("/__admin/", &imp_cookie).await;
    assert_eq!(
        index.status(),
        StatusCode::OK,
        "the impersonation session works before the change"
    );
    // The sidebar names the operator by id, not by a name (#2110).
    let html = axum::body::to_bytes(index.into_body(), usize::MAX)
        .await
        .unwrap();
    let html = String::from_utf8_lossy(&html);
    let op_id = op.id.get().copied().unwrap();
    assert!(
        html.contains(&format!(
            "Impersonating as operator <strong>#{op_id}</strong>"
        )),
        "the sidebar should name the operator"
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

/// Ending an impersonation, by its button or by logout, revokes the
/// cookie server-side: a copy stops working (#2038).
#[tokio::test]
async fn ending_an_impersonation_revokes_a_copy_of_its_cookie() {
    let env = boot().await;
    for end in ["/__admin/__end-impersonation", "/__logout"] {
        let (op, location) = start_impersonation(&env).await;
        let handoff = &location[location
            .find("/__impersonation_handoff")
            .expect("handoff url")..];
        let redeemed = env.get(handoff, "").await;
        let cookie = redeemed.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        assert_eq!(env.get("/__admin/", &cookie).await.status(), StatusCode::OK);

        let ended = env.post(end, &cookie, "").await;
        assert!(ended.status().is_redirection(), "{end}: {}", ended.status());
        assert_eq!(
            env.get("/__admin/", &cookie).await.status(),
            StatusCode::SEE_OTHER,
            "{end}: a kept copy must be refused"
        );
        let op = Operator::objects()
            .filter("id", op.id.get().copied().unwrap())
            .fetch(&env.registry)
            .await
            .unwrap()
            .remove(0);
        assert_eq!(
            op.sessions_revoked_at, None,
            "{end}: the console session stays"
        );
    }

    // A cookie with no session id could never be revoked, so it is refused.
    let (op, _) = start_impersonation(&env).await;
    let mut legacy = TenantSessionPayload::impersonation(
        op.id.get().copied().unwrap(),
        &env.slug,
        3600,
        PasswordFingerprint::of(&env.secret, &op.password_hash),
        "unused",
    );
    legacy.sid = None;
    let legacy = format!("{COOKIE_NAME}={}", encode(&env.secret, &legacy));
    assert_eq!(
        env.get("/__admin/", &legacy).await.status(),
        StatusCode::SEE_OTHER
    );
}

/// The tenant admin with its own handoff/revoke store, set before or
/// after `with_session`.
fn admin_with_jti(
    env: &Env,
    store: Arc<dyn rustango::jti_store::JtiStore>,
    store_first: bool,
) -> axum::Router {
    let reg_url = format!(
        "sqlite://{}?mode=rwc",
        env._dir.path().join("reg.db").display()
    );
    let b = TenantAdminBuilder::new(
        env.pools.clone(),
        reg_url,
        ChainResolver::new().push(SubdomainResolver::new("app.test")),
    )
    .routes(RouteConfig::legacy());
    let b = if store_first {
        b.impersonation_jti_store(store)
            .with_session(env.secret.clone())
    } else {
        b.with_session(env.secret.clone())
            .impersonation_jti_store(store)
    };
    b.build()
}

/// A plugged store holds the used handoff and the ended impersonation,
/// whatever the builder call order, and only that store is read (#2176).
#[tokio::test]
async fn a_plugged_jti_store_backs_the_impersonation_revoke() {
    use rustango::jti_store::{InMemoryJtiStore, JtiStore as _};
    let mut env = boot().await;
    for store_first in [false, true] {
        let store = Arc::new(InMemoryJtiStore::new());
        env.admin = admin_with_jti(&env, store.clone(), store_first);

        let (_, location) = start_impersonation(&env).await;
        let handoff = &location[location
            .find("/__impersonation_handoff")
            .expect("handoff url")..];
        let redeemed = env.get(handoff, "").await;
        let cookie = redeemed.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        assert_eq!(
            store.approx_size().await,
            Some(1),
            "{store_first}: the handoff jti"
        );

        let ended = env.post("/__admin/__end-impersonation", &cookie, "").await;
        assert!(ended.status().is_redirection(), "{}", ended.status());
        assert_eq!(
            store.approx_size().await,
            Some(2),
            "{store_first}: the ended session"
        );
        assert_eq!(
            env.get("/__admin/", &cookie).await.status(),
            StatusCode::SEE_OTHER
        );

        // A replica with another store never saw the revoke.
        env.admin = admin_with_jti(&env, Arc::new(InMemoryJtiStore::new()), store_first);
        assert_eq!(env.get("/__admin/", &cookie).await.status(), StatusCode::OK);
    }
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

/// #1703 — the tenant login, admin pages, change-password page and the
/// logged-in operator console all run under a strict CSP.
#[tokio::test]
async fn tenant_pages_pass_a_strict_csp() {
    use rustango::testkit::{assert_strict_csp_page, with_strict_csp};
    let env = boot().await;
    let app = with_strict_csp(env.admin.clone());
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
    let get = |uri: String, cookie: String| {
        let app = app.clone();
        let host = env.host.clone();
        async move {
            let req = Request::builder()
                .uri(uri)
                .header(header::HOST, host)
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap();
            app.oneshot(req).await.unwrap()
        }
    };
    let pages = [
        ("/__login".to_owned(), String::new()),
        ("/__admin/".to_owned(), cookie.clone()),
        ("/__admin/rustango_users".to_owned(), cookie.clone()),
        ("/__admin/rustango_users/new".to_owned(), cookie.clone()),
        (format!("/__admin/rustango_users/{uid}"), cookie.clone()),
        ("/__admin/__audit".to_owned(), cookie.clone()),
        ("/__change-password".to_owned(), cookie.clone()),
    ];
    for (uri, cookie) in pages {
        assert_strict_csp_page(get(uri.clone(), cookie).await, &uri).await;
    }

    // The console, signed in: every page an operator reads.
    let (_op, console, op_cookie) = console_session(&env).await;
    let console = with_strict_csp(console);
    let mut uris = vec![
        "/".to_owned(),
        "/orgs".to_owned(),
        "/operators".to_owned(),
        "/audit".to_owned(),
        "/change-password".to_owned(),
        format!("/orgs/{}/edit", env.slug),
        format!("/orgs/{}/hosts", env.slug),
    ];
    if cfg!(feature = "admin-sso") {
        uris.push("/sso-shared".to_owned());
    }
    for uri in uris {
        let req = Request::builder()
            .uri(&uri)
            .header(header::COOKIE, &op_cookie)
            .body(Body::empty())
            .unwrap();
        let resp = console.clone().oneshot(req).await.unwrap();
        assert_strict_csp_page(resp, &format!("console {uri}")).await;
    }
}

/// #2097 — `api::create_tenant` refuses a bad host and a host, prefix or
/// port another tenant routes on.
#[tokio::test]
async fn api_create_tenant_checks_the_host() {
    use rustango::tenancy::manage::api::{create_tenant, CreateTenantOpts};
    use rustango::tenancy::{BackendKind, StorageMode, TenancyError};
    let env = boot().await;
    let mut org = Org::objects()
        .filter("slug", env.slug.as_str())
        .fetch(&env.registry)
        .await
        .unwrap()
        .remove(0);
    org.path_prefix = Some("/taken".into());
    org.port = Some(8443);
    org.save_pool(&env.registry)
        .await
        .expect("claim prefix and port");
    let dir = tempfile::tempdir().unwrap();
    let opts = |host: &str| CreateTenantOpts {
        mode: StorageMode::Database,
        backend: BackendKind::Sqlite,
        database_url: Some(format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("new.db").display()
        )),
        host_pattern: Some(host.to_owned()),
        no_migrate: true,
        ..CreateTenantOpts::default()
    };
    let reg = "sqlite::memory:";
    let cases = [
        (opts(&env.host), "already used by another tenant"),
        (opts("bad host!"), "host"),
        (
            CreateTenantOpts {
                path_prefix: Some("/taken".into()),
                ..opts("p.app.test")
            },
            "path prefix `/taken` is already used",
        ),
        (
            CreateTenantOpts {
                port: Some(8443),
                ..opts("q.app.test")
            },
            "port 8443 is already used",
        ),
    ];
    for (o, want) in cases {
        match create_tenant(env.pools.as_ref(), reg, dir.path(), &unique("n"), o).await {
            Err(TenancyError::Validation(msg)) => assert!(msg.contains(want), "{msg}"),
            other => panic!("want Validation({want}), got {other:?}"),
        }
    }
    let ok = create_tenant(
        env.pools.as_ref(),
        reg,
        dir.path(),
        &unique("n"),
        opts("fresh.app.test"),
    )
    .await;
    assert!(ok.is_ok(), "{ok:?}");
}

/// An impersonating operator acts as `operator:<id>:impersonating`, never
/// as `user:0` or a username a tenant user could also take (#2110).
#[tokio::test]
async fn an_impersonation_is_attributed_to_the_operator_id() {
    let env = boot().await;
    let (op, location) = start_impersonation(&env).await;
    let op_id = op.id.get().copied().unwrap();
    let mut user = User {
        username: format!("operator:{}", op.username),
        ..rustango::testkit::user()
    };
    user.insert_pool(&env.tenant).await.expect("seed user");
    let uid = user.id.get().copied().unwrap();

    let handoff = &location[location
        .find("/__impersonation_handoff")
        .expect("handoff url")..];
    let redeemed = env.get(handoff, "").await;
    let cookie = redeemed.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    SEEN.lock().unwrap().clear();
    let res = env
        .get(&format!("/__admin/rustango_users/{uid}"), &cookie)
        .await;
    assert_eq!(res.status(), StatusCode::OK);

    let (source, session) = SEEN.lock().unwrap().pop().expect("the action ran");
    let token = format!("operator:{op_id}:impersonating");
    assert_eq!(source, token, "audited writes");
    let session = session.expect("an admin session");
    assert_eq!(session.impersonated_by, Some(op_id));
    assert_eq!(session.actor().as_token(), token, "updated_by");
    assert!(session.username.is_empty(), "{}", session.username);
}
