//! SSO signs in by `(provider, sub)` link; email linking is opt-in and never
//! applies to privileged accounts. Drives the real callbacks against a fake OIDC IdP.

#![cfg(all(feature = "sqlite", feature = "tenancy", feature = "admin-sso"))]
#![allow(irrefutable_let_patterns)]

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use rustango::sql::{sqlx, Auto, FetcherPool as _, Pool};
use rustango::sso::{LinkSource, SsoProvider};
use rustango::tenancy::tenant_console::{
    decode, encode, PasswordFingerprint, SessionSecret, TenantSessionPayload, COOKIE_NAME,
};
use rustango::tenancy::{
    admin::TenantAdminBuilder, routes::RouteConfig, ChainResolver, Operator, Org,
    SubdomainResolver, TenantPools, User,
};
use tower::ServiceExt as _;

static SUITE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A fake OIDC IdP: discovery, token, and a userinfo the test sets.
#[derive(Clone)]
struct Idp {
    issuer: String,
    claims: Arc<Mutex<serde_json::Value>>,
}

impl Idp {
    async fn start() -> Self {
        // The fake IdP is on loopback, which outbound calls refuse unless
        // allowlisted. Every test holds `SUITE`, so setting env is safe.
        std::env::set_var("RUSTANGO_OUTBOUND_ALLOW", "127.0.0.1");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let claims = Arc::new(Mutex::new(serde_json::json!({})));
        let doc = serde_json::json!({
            "authorization_endpoint": format!("{issuer}/auth"),
            "token_endpoint": format!("{issuer}/token"),
            "userinfo_endpoint": format!("{issuer}/userinfo"),
        });
        let c = claims.clone();
        let app = Router::new()
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
                get(move || async move { Json(c.lock().unwrap().clone()) }),
            );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { issuer, claims }
    }

    fn assert(&self, sub: &str, email: &str) {
        *self.claims.lock().unwrap() =
            serde_json::json!({"sub": sub, "email": email, "email_verified": true});
    }
}

struct Tenant {
    slug: String,
    host: String,
    pool: Pool,
}

struct Env {
    admin: Router,
    /// An editable console (it has the tenant pools).
    console: Router,
    readonly_console: Router,
    /// Keeps the tenant pools alive for the test.
    _pools: Arc<TenantPools<sqlx::Sqlite>>,
    /// `acme` and `globex`, each with its own database.
    tenants: [Tenant; 2],
    secret: SessionSecret,
    idp: Idp,
    _dir: tempfile::TempDir,
}

/// `permissions`: create the tenant permission tables (else every
/// permission read fails).
async fn boot_with(permissions: bool) -> Env {
    std::env::set_var("RUSTANGO_SECRET_KEY", "sso-link-test-key");
    // Process-global resolver state from an earlier test in this binary.
    rustango::testkit::reset_org_cache();
    rustango::testkit::reset_host_generation();
    rustango::testkit::reset_registry_breaker();
    let dir = tempfile::tempdir().unwrap();
    let reg_url = format!("sqlite://{}?mode=rwc", dir.path().join("reg.db").display());
    let pools = Arc::new(TenantPools::<sqlx::Sqlite>::new(
        sqlx::SqlitePool::connect(&reg_url).await.unwrap(),
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
    let registry = pools.registry_pool();

    let mut tenants = Vec::new();
    for slug in ["acme", "globex"] {
        let url = format!("sqlite://{}?mode=rwc", dir.path().join(slug).display());
        let host = format!("{slug}.app.test");
        let mut org = Org {
            slug: slug.into(),
            storage_mode: "database".into(),
            backend_kind: "sqlite".into(),
            database_url: Some(url.clone()),
            host_pattern: Some(host.clone()),
            ..rustango::testkit::org()
        };
        org.insert_pool(&registry).await.unwrap();
        let pool = Pool::connect(&url).await.unwrap();
        rustango::testkit::create_tables_for::<User>(&pool)
            .await
            .unwrap();
        rustango::testkit::create_tables_for::<SsoProvider>(&pool)
            .await
            .unwrap();
        rustango::sso::link::ensure_table(&pool).await.unwrap();
        if permissions {
            rustango::tenancy::permissions::ensure_tables_pool(&pool)
                .await
                .unwrap();
        }
        tenants.push(Tenant {
            slug: slug.into(),
            host,
            pool,
        });
    }

    let secret = SessionSecret::from_bytes(b"tenant-admin-session-secret-32b!".to_vec());
    let admin = TenantAdminBuilder::new(
        pools.clone(),
        reg_url,
        ChainResolver::new().push(SubdomainResolver::new("app.test")),
    )
    .routes(RouteConfig::legacy())
    .with_session(secret.clone())
    .build();
    let op_secret =
        || rustango::tenancy::operator_console::SessionSecret::from_bytes(vec![9u8; 32]);
    let console = rustango::tenancy::operator_console::router_with_pools(
        registry.clone(),
        pools.clone(),
        op_secret(),
    );
    let readonly_console =
        rustango::tenancy::operator_console::router(registry.clone(), op_secret());
    let mut op = Operator {
        id: Auto::default(),
        username: "op".into(),
        password_hash: rustango::tenancy::password::hash("op-pass-123").unwrap(),
        active: true,
        created_at: chrono::Utc::now(),
        password_changed_at: None,
        sessions_revoked_at: None,
    };
    op.insert_pool(&registry).await.unwrap();

    let [a, b]: [Tenant; 2] = tenants.try_into().ok().unwrap();
    Env {
        admin,
        console,
        readonly_console,
        _pools: pools,
        tenants: [a, b],
        secret,
        idp: Idp::start().await,
        _dir: dir,
    }
}

async fn boot() -> Env {
    boot_with(true).await
}

fn set_cookies(resp: &axum::response::Response) -> Vec<String> {
    resp.headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or("").to_owned())
        .collect()
}

fn location(resp: &axum::response::Response) -> String {
    resp.headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned()
}

async fn send(app: &Router, req: Request<Body>) -> axum::response::Response {
    app.clone().oneshot(req).await.unwrap()
}

/// Run begin + callback on `app` under `login`; return the callback response.
async fn handshake(app: &Router, host: &str, login: &str, slug: &str) -> axum::response::Response {
    let begin = send(
        app,
        Request::builder()
            .uri(format!("{login}/sso/{slug}"))
            .header(header::HOST, host)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let to = location(&begin);
    let state = to
        .split(['?', '&'])
        .find_map(|kv| kv.strip_prefix("state="))
        .unwrap_or_else(|| panic!("begin must redirect to the IdP, got {to:?}"))
        .to_owned();
    let flow = set_cookies(&begin).join("; ");
    send(
        app,
        Request::builder()
            .uri(format!("{login}/sso/{slug}/callback?code=c&state={state}"))
            .header(header::HOST, host)
            .header(header::COOKIE, flow)
            .body(Body::empty())
            .unwrap(),
    )
    .await
}

/// The check ran to the end: the unset deploy env may fail it, nothing else.
fn assert_check_ran<E: std::fmt::Display>(res: Result<(), E>, out: &str) {
    if let Err(e) = res {
        assert!(e.to_string().contains("system check(s) failed"), "{e}");
    }
    assert!(
        out.contains("running rustango system check (deploy mode)"),
        "{out}"
    );
}

fn provider_row(issuer: &str, slug: &str, allow_email_link: bool) -> SsoProvider {
    SsoProvider {
        id: Auto::default(),
        slug: slug.into(),
        label: slug.into(),
        kind: "oidc".into(),
        issuer_url: Some(issuer.to_owned()),
        client_id: "cid".into(),
        client_secret: rustango::casts::Cast::new("csecret".into()),
        enabled: true,
        sort_order: 0,
        scopes: None,
        allow_email_link,
        created_at: Auto::default(),
        updated_at: Auto::default(),
    }
}

impl Env {
    fn pool(&self) -> &Pool {
        &self.tenants[0].pool
    }

    async fn user_in(&self, t: usize, name: &str, email: &str, superuser: bool) -> i64 {
        let mut u = User {
            username: name.into(),
            password_hash: "x".into(),
            email: Some(email.into()),
            is_superuser: superuser,
            ..rustango::testkit::user()
        };
        u.insert_pool(&self.tenants[t].pool).await.unwrap();
        u.id.get().copied().unwrap()
    }

    async fn user(&self, name: &str, email: &str, superuser: bool) -> i64 {
        self.user_in(0, name, email, superuser).await
    }

    async fn deactivate(&self, id: i64) {
        let mut u = User::objects()
            .filter("id", id)
            .fetch(self.pool())
            .await
            .unwrap()
            .remove(0);
        u.active = false;
        u.save_pool(self.pool()).await.unwrap();
    }

    async fn provider_in(&self, t: usize, slug: &str, allow_email_link: bool) {
        provider_row(&self.idp.issuer, slug, allow_email_link)
            .insert_pool(&self.tenants[t].pool)
            .await
            .unwrap();
    }

    async fn tenant_provider(&self, slug: &str, allow_email_link: bool) {
        self.provider_in(0, slug, allow_email_link).await;
    }

    /// Operator console session cookies (plus the CSRF pair).
    async fn console_cookies(&self) -> String {
        self.cookies_for(&self.console).await
    }

    async fn cookies_for(&self, console: &Router) -> String {
        let login = send(
            console,
            Request::builder()
                .method("POST")
                .uri("/login")
                .header(header::COOKIE, "rustango_csrf=t")
                .header("x-csrf-token", "t")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from("username=op&password=op-pass-123"))
                .unwrap(),
        )
        .await;
        assert!(login.status().is_redirection(), "operator login");
        let mut cookies: Vec<String> = set_cookies(&login)
            .into_iter()
            .filter(|c| !c.starts_with("rustango_csrf="))
            .collect();
        cookies.push("rustango_csrf=t".into());
        cookies.join("; ")
    }

    async fn console_post(&self, path: &str, form: String) -> StatusCode {
        let cookies = self.console_cookies().await;
        send(
            &self.console,
            Request::builder()
                .method("POST")
                .uri(path)
                .header(header::COOKIE, cookies)
                .header("x-csrf-token", "t")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(form))
                .unwrap(),
        )
        .await
        .status()
    }

    /// Create a shared provider through the operator console form.
    async fn shared_provider(&self, slug: &str, allow_email_link: bool) {
        let mut form = format!(
            "slug={slug}&label={slug}&kind=oidc&issuer_url={}&client_id=cid&client_secret=cs&enabled=on",
            urlencoding::encode(&self.idp.issuer)
        );
        if allow_email_link {
            form.push_str("&allow_email_link=on");
        }
        assert_eq!(
            self.console_post("/sso-shared", form).await,
            StatusCode::SEE_OTHER
        );
    }

    /// The operator console's shared provider list page.
    async fn shared_list(&self) -> String {
        let cookies = self.console_cookies().await;
        let resp = send(
            &self.console,
            Request::builder()
                .uri("/sso-shared")
                .header(header::COOKIE, cookies)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(body.to_vec()).unwrap()
    }

    /// The only shared provider's email-link form action, from the console list.
    async fn email_link_path(&self) -> String {
        let html = self.shared_list().await;
        html.split("action=\"")
            .filter_map(|s| s.split('"').next())
            .find(|p| p.ends_with("/email-link"))
            .expect("email-link form")
            .to_owned()
    }

    /// Set the only shared provider's email linking.
    async fn set_shared_email_link(&self, on: bool) {
        let path = self.email_link_path().await;
        let form = format!("allow_email_link={}", if on { "on" } else { "off" });
        assert_eq!(self.console_post(&path, form).await, StatusCode::SEE_OTHER);
    }

    /// SSO into tenant `t`'s admin as `(sub, email)`: the user id, or the `sso_error`.
    async fn sso_in(&self, t: usize, slug: &str, sub: &str, email: &str) -> Result<i64, String> {
        self.idp.assert(sub, email);
        let tenant = &self.tenants[t];
        let resp = handshake(&self.admin, &tenant.host, "/__login", slug).await;
        let to = location(&resp);
        if let Some((_, code)) = to.split_once("sso_error=") {
            return Err(code.to_owned());
        }
        let session = set_cookies(&resp)
            .into_iter()
            .find_map(|c| {
                c.strip_prefix(&format!("{COOKIE_NAME}="))
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| panic!("no session cookie; location {to:?}"));
        Ok(decode(&self.secret, &tenant.slug, &session).unwrap().uid)
    }

    async fn sso(&self, slug: &str, sub: &str, email: &str) -> Result<i64, String> {
        self.sso_in(0, slug, sub, email).await
    }

    /// `check --deploy` output, through the tenancy dispatcher.
    async fn check_deploy(&self) -> String {
        let dir = self._dir.path();
        let reg_url = format!("sqlite://{}?mode=rwc", dir.join("reg.db").display());
        let mut out = Vec::new();
        let res = rustango::tenancy::manage::run_with_writer(
            self._pools.as_ref(),
            &reg_url,
            &dir.join("migrations"),
            vec!["check".to_owned(), "--deploy".to_owned()],
            &mut out,
        )
        .await;
        let out = String::from_utf8(out).unwrap();
        assert_check_ran(res, &out);
        out
    }

    /// POST a tenant admin form as user `uid`; the response status.
    async fn admin_post(&self, uid: i64, path: &str, form: &str) -> StatusCode {
        let payload = TenantSessionPayload::new(
            uid,
            &self.tenants[0].slug,
            3600,
            PasswordFingerprint::of(&self.secret, "x"),
        );
        let cookie = format!(
            "{COOKIE_NAME}={}; rustango_csrf=t",
            encode(&self.secret, &payload)
        );
        send(
            &self.admin,
            Request::builder()
                .method("POST")
                .uri(path)
                .header(header::HOST, &self.tenants[0].host)
                .header(header::COOKIE, cookie)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(format!("_csrf=t&{form}")))
                .unwrap(),
        )
        .await
        .status()
    }
}

#[tokio::test]
async fn email_match_without_opt_in_is_refused() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    env.user("ann", "ann@example.com", false).await;
    env.tenant_provider("corp", false).await;
    assert_eq!(
        env.sso("corp", "sub-ann", "ann@example.com").await,
        Err("nouser".into())
    );
}

#[tokio::test]
async fn opt_in_links_then_sub_wins_over_a_changed_email() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    let ann = env.user("ann", "ann@example.com", false).await;
    let bob = env.user("bob", "bob@example.com", false).await;
    env.tenant_provider("corp", true).await;
    assert_eq!(env.sso("corp", "sub-ann", "ann@example.com").await, Ok(ann));
    // The IdP now reports another email, even Bob's: the link decides.
    assert_eq!(env.sso("corp", "sub-ann", "ann@new.example").await, Ok(ann));
    assert_eq!(env.sso("corp", "sub-ann", "bob@example.com").await, Ok(ann));
    assert_ne!(ann, bob);
}

/// A tenant SSO login gets `iat` = the logout cut-off + 1 (#1855).
#[tokio::test]
async fn a_tenant_sso_login_lands_after_the_logout_cutoff() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    let ann = env.user("ann", "ann@example.com", false).await;
    env.tenant_provider("corp", true).await;
    let cut = chrono::DateTime::from_timestamp(chrono::Utc::now().timestamp() + 60, 0).unwrap();
    let mut row = User::objects()
        .filter("id", ann)
        .fetch(env.pool())
        .await
        .unwrap()
        .remove(0);
    row.sessions_revoked_at = Some(cut);
    row.save_pool(env.pool()).await.unwrap();

    env.idp.assert("sub-ann", "ann@example.com");
    let resp = handshake(&env.admin, &env.tenants[0].host, "/__login", "corp").await;
    let session = set_cookies(&resp)
        .into_iter()
        .find_map(|c| {
            c.strip_prefix(&format!("{COOKIE_NAME}="))
                .map(str::to_owned)
        })
        .unwrap_or_else(|| panic!("no session cookie; location {:?}", location(&resp)));
    assert_eq!(
        decode(&env.secret, &env.tenants[0].slug, &session)
            .unwrap()
            .iat,
        cut.timestamp() + 1
    );
}

#[tokio::test]
async fn opt_in_never_links_a_superuser_or_staff() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    env.user("root", "root@example.com", true).await;
    let staff = env.user("staff", "staff@example.com", false).await;
    rustango::tenancy::permissions::set_user_perm_pool(staff, "post.change", true, env.pool())
        .await
        .unwrap();
    env.tenant_provider("corp", true).await;
    for (sub, email) in [
        ("sub-root", "root@example.com"),
        ("sub-staff", "staff@example.com"),
    ] {
        assert_eq!(env.sso("corp", sub, email).await, Err("nouser".into()));
    }
}

#[tokio::test]
async fn an_unreadable_permission_set_counts_as_privileged() {
    let _g = SUITE.lock().await;
    let env = boot_with(false).await;
    env.user("ann", "ann@example.com", false).await;
    env.tenant_provider("corp", true).await;
    assert_eq!(
        env.sso("corp", "sub-ann", "ann@example.com").await,
        Err("nouser".into())
    );
}

#[tokio::test]
async fn an_inactive_linked_user_is_refused() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    let ann = env.user("ann", "ann@example.com", false).await;
    env.tenant_provider("corp", true).await;
    assert_eq!(env.sso("corp", "sub-ann", "ann@example.com").await, Ok(ann));
    env.deactivate(ann).await;
    assert!(env.sso("corp", "sub-ann", "ann@example.com").await.is_err());
}

#[tokio::test]
async fn a_failing_user_read_keeps_the_link() {
    use rustango::sql::raw_execute_pool;
    let _g = SUITE.lock().await;
    let env = boot().await;
    let ann = env.user("ann", "ann@example.com", false).await;
    env.tenant_provider("corp", true).await;
    assert_eq!(env.sso("corp", "sub-ann", "ann@example.com").await, Ok(ann));
    // From here only the link can sign Ann in.
    let mut p = SsoProvider::objects()
        .fetch(env.pool())
        .await
        .unwrap()
        .remove(0);
    p.allow_email_link = false;
    p.save_pool(env.pool()).await.unwrap();
    // Test-only DDL: the user table goes away, like a broken read.
    let rename = |from: &str, to: &str| format!("ALTER TABLE {from} RENAME TO {to}");
    raw_execute_pool(
        env.pool(),
        &rename("rustango_users", "users_away"),
        Vec::new(),
    )
    .await
    .unwrap();
    assert!(env.sso("corp", "sub-ann", "ann@example.com").await.is_err());
    raw_execute_pool(
        env.pool(),
        &rename("users_away", "rustango_users"),
        Vec::new(),
    )
    .await
    .unwrap();
    assert_eq!(env.sso("corp", "sub-ann", "ann@example.com").await, Ok(ann));
}

#[tokio::test]
async fn a_link_in_one_tenant_does_not_sign_into_another() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    let ann = env.user_in(0, "ann", "ann@example.com", false).await;
    env.user_in(1, "ann", "ann@example.com", false).await;
    env.provider_in(0, "corp", true).await;
    // Same slug, same row id, same issuer in the other tenant; email linking off there.
    env.provider_in(1, "corp", false).await;
    assert_eq!(
        env.sso_in(0, "corp", "sub-ann", "ann@example.com").await,
        Ok(ann)
    );
    assert_eq!(
        env.sso_in(1, "corp", "sub-ann", "ann@example.com").await,
        Err("nouser".into())
    );
}

/// #1992 — a flow begun for one tenant's provider is refused at another
/// provider's or tenant's callback.
#[tokio::test]
async fn a_flow_completes_only_where_it_began() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    for t in 0..2 {
        env.user_in(t, "ann", "ann@example.com", false).await;
        env.provider_in(t, "corp", true).await;
        env.provider_in(t, "other", true).await;
    }
    env.idp.assert("sub-ann", "ann@example.com");
    let begin = send(
        &env.admin,
        Request::builder()
            .uri("/__login/sso/corp")
            .header(header::HOST, &env.tenants[0].host)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let to = location(&begin);
    let state = to
        .split(['?', '&'])
        .find_map(|kv| kv.strip_prefix("state="))
        .unwrap()
        .to_owned();
    let flow = set_cookies(&begin).join("; ");
    for (host, slug) in [
        (&env.tenants[0].host, "other"),
        (&env.tenants[1].host, "corp"),
    ] {
        let resp = send(
            &env.admin,
            Request::builder()
                .uri(format!("/__login/sso/{slug}/callback?code=c&state={state}"))
                .header(header::HOST, host)
                .header(header::COOKIE, &flow)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert!(
            location(&resp).ends_with("sso_error=expired"),
            "{host} {slug}: {}",
            location(&resp)
        );
    }
}

#[tokio::test]
async fn shared_and_tenant_providers_with_one_slug_do_not_share_links() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    let ann = env.user("ann", "ann@example.com", false).await;
    env.shared_provider("corp", true).await;
    assert_eq!(env.sso("corp", "sub-ann", "ann@example.com").await, Ok(ann));
    // A tenant provider with the same slug now wins; the shared link is not its link.
    env.tenant_provider("corp", false).await;
    assert_eq!(
        env.sso("corp", "sub-ann", "ann@example.com").await,
        Err("nouser".into())
    );
}

/// A tenant without its own provider table signs in with a shared one (#2366).
#[tokio::test]
async fn shared_provider_serves_a_tenant_without_a_provider_table() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    let ann = env.user("ann", "ann@example.com", false).await;
    rustango::testkit::matrix::drop_table(env.pool(), "rustango_sso_providers").await;
    env.shared_provider("corp", true).await;
    assert_eq!(env.sso("corp", "sub-ann", "ann@example.com").await, Ok(ann));
}

/// `check --deploy` names the providers that refuse every existing user (#2359).
#[tokio::test]
async fn check_deploy_names_providers_that_refuse_every_user() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    env.tenant_provider("corp", false).await;
    env.shared_provider("team", false).await;
    let out = env.check_deploy().await;
    assert!(!out.contains("[sso]"), "no users, nobody refused: {out}");

    let ann = env.user("ann", "ann@example.com", false).await;
    let out = env.check_deploy().await;
    assert!(
        out.contains("[warning] [sso] tenant `acme`: provider `corp` has allow_email_link off"),
        "{out}"
    );
    assert!(
        out.contains("[sso] shared provider `team` (tenant(s) acme) has allow_email_link off"),
        "{out}"
    );
    assert!(!out.contains("`globex`"), "globex has no users: {out}");

    // Linked once, the provider signs that user in: no warning.
    let corp = rustango::sso::resolve_by_slug(env.pool(), "corp", String::new())
        .await
        .unwrap()
        .unwrap();
    rustango::sso::link::create_link(env.pool(), &corp.key(LinkSource::Tenant), "sub-ann", ann)
        .await
        .unwrap();
    env.set_shared_email_link(true).await;
    let out = env.check_deploy().await;
    assert!(!out.contains("[sso]"), "{out}");
}

/// A shared provider names every active tenant it refuses; a tenant without
/// the links table has no links (#2359).
#[tokio::test]
async fn check_deploy_lists_every_refused_active_tenant() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    env.shared_provider("team", false).await;
    env.user("ann", "ann@example.com", false).await;
    env.user_in(1, "bob", "bob@example.com", false).await;
    rustango::testkit::matrix::drop_table(&env.tenants[1].pool, "rustango_sso_links").await;
    let out = env.check_deploy().await;
    assert!(
        out.contains("[sso] shared provider `team` (tenant(s) acme, globex) has"),
        "{out}"
    );

    let registry = env._pools.registry_pool();
    let mut globex = Org::objects()
        .filter("slug", "globex")
        .fetch(&registry)
        .await
        .unwrap()
        .remove(0);
    globex.active = false;
    globex.save_pool(&registry).await.unwrap();
    let out = env.check_deploy().await;
    assert!(
        out.contains("[sso] shared provider `team` (tenant(s) acme) has"),
        "inactive globex is skipped: {out}"
    );
}

/// A registry that can't list tenants is a warning; the rest of the check runs (#2359).
#[tokio::test]
async fn check_deploy_runs_when_tenants_cannot_be_listed() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}?mode=rwc", dir.path().join("reg.db").display());
    let pools = TenantPools::<sqlx::Sqlite>::new(sqlx::SqlitePool::connect(&url).await.unwrap());
    let mut out = Vec::new();
    let _ = rustango::tenancy::manage::run_with_writer(
        &pools,
        &url,
        dir.path(),
        vec!["check".to_owned(), "--deploy".to_owned()],
        &mut out,
    )
    .await;
    let out = String::from_utf8(out).unwrap();
    assert!(out.contains("running rustango system check"), "{out}");
    assert!(
        out.contains("[warning] [sso] could not list tenants"),
        "{out}"
    );
}

/// A tenant pool that won't open is reported without its database URL (#2359).
#[tokio::test]
async fn check_deploy_reports_an_unopenable_tenant_without_its_url() {
    let _g = SUITE.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}?mode=rwc", dir.path().join("reg.db").display());
    let pools = TenantPools::<sqlx::Sqlite>::with_secrets(
        sqlx::SqlitePool::connect(&url).await.unwrap(),
        rustango::tenancy::EnvSecretsResolver,
    );
    let run = |verb: &str| {
        let mut out = Vec::new();
        let args: Vec<String> = verb.split(' ').map(str::to_owned).collect();
        let pools = &pools;
        let url = &url;
        let dir = dir.path();
        async move {
            let res =
                rustango::tenancy::manage::run_with_writer(pools, url, dir, args, &mut out).await;
            (res, String::from_utf8(out).unwrap())
        }
    };
    run("migrate-registry").await.0.expect("migrate-registry");
    // `EnvSecretsResolver` wants `env://`; its error quotes the literal URL.
    let mut org = Org {
        slug: "acme".into(),
        backend_kind: "sqlite".into(),
        database_url: Some("sqlite:///tmp/acme-hunter2.db".into()),
        ..rustango::testkit::org()
    };
    org.insert_pool(&pools.registry_pool()).await.unwrap();
    let (_, out) = run("check --deploy").await;
    assert!(
        out.contains(
            "[sso] tenant `acme`: could not check SSO providers: could not open the tenant pool"
        ),
        "{out}"
    );
    assert!(!out.contains("hunter2"), "{out}");
}

/// A table probe that fails is an error, not "no tables" (#2359).
#[tokio::test]
async fn check_reports_a_failed_table_probe() {
    let pool = Pool::connect("sqlite::memory:").await.unwrap();
    pool.close().await;
    assert!(rustango::testkit::sso_check::tenant_providers(&pool)
        .await
        .is_err());
}

/// Email linking never signs in a privileged account, so a provider whose
/// only users are privileged still refuses everyone (#2359).
#[tokio::test]
async fn check_deploy_names_an_email_linking_provider_with_only_privileged_users() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    env.tenant_provider("corp", true).await;
    env.user("root", "root@example.com", true).await;
    // No email, so nothing to link by.
    env.user("noemail", "", false).await;
    let staff = env.user("staff", "staff@example.com", false).await;
    rustango::tenancy::permissions::set_user_perm_pool(staff, "post.change", true, env.pool())
        .await
        .unwrap();
    let gone = env.user("gone", "gone@example.com", false).await;
    env.deactivate(gone).await;
    let out = env.check_deploy().await;
    assert!(
        out.contains(
            "[warning] [sso] tenant `acme`: provider `corp` has no SsoLink rows and every active user is privileged"
        ),
        "{out}"
    );
    env.user("ann", "ann@example.com", false).await;
    let out = env.check_deploy().await;
    assert!(!out.contains("[sso]"), "ann can link by email: {out}");
}

#[tokio::test]
async fn toggling_shared_email_linking_keeps_the_provider_and_its_links() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    let ann = env.user("ann", "ann@example.com", false).await;
    env.shared_provider("corp", false).await;
    assert!(env.sso("corp", "sub-ann", "ann@example.com").await.is_err());
    env.set_shared_email_link(true).await;
    // The wanted value, not a flip: sending it twice keeps it on.
    env.set_shared_email_link(true).await;
    assert!(env.shared_list().await.contains("Stop email linking"));
    assert_eq!(env.sso("corp", "sub-ann", "ann@example.com").await, Ok(ann));
    env.set_shared_email_link(false).await;
    assert!(env.shared_list().await.contains("Allow email linking"));
    assert_eq!(env.sso("corp", "sub-ann", "ann@example.com").await, Ok(ann));
}

#[tokio::test]
async fn a_read_only_console_cannot_change_shared_providers() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    env.shared_provider("corp", false).await;
    let path = env.email_link_path().await;
    let cookies = env.cookies_for(&env.readonly_console).await;
    for (uri, form) in [
        (path.as_str(), "allow_email_link=on"),
        (
            "/sso-shared",
            "slug=x&label=x&kind=google&client_id=c&client_secret=s",
        ),
        (&path.replace("/email-link", "/delete"), ""),
    ] {
        let resp = send(
            &env.readonly_console,
            Request::builder()
                .method("POST")
                .uri(uri)
                .header(header::COOKIE, cookies.clone())
                .header("x-csrf-token", "t")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(form.to_owned()))
                .unwrap(),
        )
        .await;
        assert!(resp.status().is_client_error(), "{uri}: {}", resp.status());
    }
    assert!(env.shared_list().await.contains("Allow email linking"));
}

#[tokio::test]
async fn only_a_superuser_writes_links_and_providers() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    let root = env.user("root", "root@example.com", true).await;
    let ann = env.user("ann", "ann@example.com", false).await;
    let staff = env.user("staff", "staff@example.com", false).await;
    for table in [
        "rustango_sso_links",
        "rustango_sso_providers",
        "rustango_roles",
    ] {
        for action in ["add", "change", "delete", "view"] {
            let codename = format!("{table}.{action}");
            rustango::tenancy::permissions::set_user_perm_pool(staff, &codename, true, env.pool())
                .await
                .unwrap();
        }
    }
    env.tenant_provider("corp", true).await;
    assert_eq!(env.sso("corp", "sub-ann", "ann@example.com").await, Ok(ann));
    let link_id = rustango::sso::SsoLink::objects()
        .fetch(env.pool())
        .await
        .unwrap()[0]
        .id
        .get()
        .copied()
        .unwrap();

    // Control: the same staff grant does write an ordinary table.
    let resp = env
        .admin_post(
            staff,
            "/__admin/rustango_roles",
            "name=editors&description=d",
        )
        .await;
    assert_ne!(resp, StatusCode::FORBIDDEN, "staff can add a role");

    let new_link =
        format!("provider_source=tenant&provider_id=1&issuer=x&subject=s&user_id={root}");
    let repoint = format!(
        "provider_source=tenant&provider_id=1&issuer=oidc|{}&subject=sub-ann&user_id={root}",
        env.idp.issuer
    );
    let provider = "slug=evil&label=e&kind=oidc&client_id=c&client_secret=s&enabled=on&sort_order=0&allow_email_link=on";
    for (path, form) in [
        ("/__admin/rustango_sso_links".to_owned(), new_link.clone()),
        (
            format!("/__admin/rustango_sso_links/{link_id}"),
            repoint.clone(),
        ),
        (
            format!("/__admin/rustango_sso_links/{link_id}/delete"),
            String::new(),
        ),
        (
            "/__admin/rustango_sso_providers".to_owned(),
            provider.to_owned(),
        ),
        (
            "/__admin/rustango_sso_providers/1".to_owned(),
            provider.to_owned(),
        ),
        (
            "/__admin/rustango_sso_providers/1/delete".to_owned(),
            String::new(),
        ),
    ] {
        assert_eq!(
            env.admin_post(staff, &path, &form).await,
            StatusCode::FORBIDDEN,
            "staff {path}"
        );
    }
    // Still Ann's link.
    assert_eq!(env.sso("corp", "sub-ann", "x@example.com").await, Ok(ann));

    // A superuser gets through the same routes.
    let resp = env
        .admin_post(root, "/__admin/rustango_sso_links", &new_link)
        .await;
    assert_ne!(resp, StatusCode::FORBIDDEN);
    assert!(rustango::sso::SsoLink::objects()
        .fetch(env.pool())
        .await
        .unwrap()
        .iter()
        .any(|l| l.user_id == root));
}

#[tokio::test]
async fn a_failed_link_insert_leaves_no_member_behind() {
    use rustango::sso::ProviderKey;
    use rustango::tenancy::member_auth::find_or_provision_member;
    let _g = SUITE.lock().await;
    let env = boot().await;
    // Test-only DDL: make every link insert fail.
    rustango::sql::raw_execute_pool(
        env.pool(),
        "CREATE TRIGGER no_links BEFORE INSERT ON rustango_sso_links \
         BEGIN SELECT RAISE(ABORT, 'no links'); END",
        Vec::new(),
    )
    .await
    .unwrap();
    let key = ProviderKey::app("https://accounts.google.com").unwrap();
    let got = find_or_provision_member(
        env.pool(),
        &key,
        false,
        &profile("g-half", "half@example.com"),
        true,
    )
    .await;
    assert!(got.is_err(), "{got:?}");
    let users = User::objects()
        .filter("email", "half@example.com")
        .fetch(env.pool())
        .await
        .unwrap();
    assert!(users.is_empty(), "no half-provisioned member");
}

#[tokio::test]
async fn real_tenant_migrations_create_the_link_table() {
    let _g = SUITE.lock().await;
    std::env::set_var("RUSTANGO_SECRET_KEY", "sso-link-test-key");
    let dir = tempfile::tempdir().unwrap();
    let reg_url = format!("sqlite://{}?mode=rwc", dir.path().join("reg.db").display());
    let pools = Arc::new(TenantPools::<sqlx::Sqlite>::new(
        sqlx::SqlitePool::connect(&reg_url).await.unwrap(),
    ));
    let migrations = dir.path().join("migrations");
    std::fs::create_dir_all(&migrations).unwrap();
    let run = |args: Vec<String>| {
        let (pools, reg_url, migrations) = (pools.clone(), reg_url.clone(), migrations.clone());
        async move {
            rustango::tenancy::manage::run_with_writer(
                pools.as_ref(),
                &reg_url,
                &migrations,
                args,
                &mut Vec::new(),
            )
            .await
            .unwrap();
        }
    };
    run(vec!["migrate-registry".into()]).await;
    let db = dir.path().join("acme.db");
    let url = format!("sqlite://{}?mode=rwc", db.display());
    run([
        "create-tenant",
        "acme",
        "--mode",
        "database",
        "--backend",
        "sqlite",
        "--database-url",
        &url,
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect())
    .await;
    let tenant = Pool::connect(&url).await.unwrap();
    // No `ensure_table`: the migrations made it, with its unique key.
    let key = rustango::sso::ProviderKey::app("https://idp.example").unwrap();
    rustango::sso::link::create_link(&tenant, &key, "sub", 1)
        .await
        .unwrap();
    assert!(rustango::sso::link::create_link(&tenant, &key, "sub", 2)
        .await
        .is_err());
}

// ---- bare admin ---------------------------------------------------------

async fn bare_admin() -> (Router, Pool, Idp) {
    use rustango::admin::AdminUser;
    std::env::set_var("RUSTANGO_SECRET_KEY", "sso-link-test-key");
    let pool = Pool::connect("sqlite::memory:").await.unwrap();
    rustango::testkit::create_tables_for::<AdminUser>(&pool)
        .await
        .unwrap();
    rustango::testkit::create_tables_for::<SsoProvider>(&pool)
        .await
        .unwrap();
    rustango::sso::link::ensure_table(&pool).await.unwrap();
    // The sign-in reads the TOTP device table when `totp` is on (#2249).
    #[cfg(feature = "totp")]
    rustango::admin::totp_store::ensure_table(&pool)
        .await
        .unwrap();
    let idp = Idp::start().await;
    provider_row(&idp.issuer, "corp", true)
        .insert_pool(&pool)
        .await
        .unwrap();
    let mut root = AdminUser::new_with_password("root", "pw-123456789", true).unwrap();
    root.email = Some("root@example.com".into());
    root.insert_pool(&pool).await.unwrap();
    let app = rustango::admin::Builder::new(pool.clone())
        .with_session_auth(rustango::session::SessionSecret::from_bytes(vec![7u8; 32]))
        .build();
    (app, pool, idp)
}

fn signed_in(resp: &axum::response::Response) -> bool {
    !location(resp).contains("sso_error")
        && set_cookies(resp)
            .iter()
            .any(|c| c.starts_with("rustango_admin_session="))
}

#[tokio::test]
async fn bare_admin_never_links_by_email_and_signs_in_by_link() {
    use rustango::admin::AdminUser;
    let _g = SUITE.lock().await;
    let (app, pool, idp) = bare_admin().await;
    idp.assert("sub-root", "root@example.com");
    let resp = handshake(&app, "admin.test", "/login", "corp").await;
    assert!(!signed_in(&resp), "{}", location(&resp));

    // An admin adds the link; then the same identity signs in.
    let mut root = AdminUser::objects()
        .filter("username", "root")
        .fetch(&pool)
        .await
        .unwrap()
        .remove(0);
    let key = rustango::sso::resolve_by_slug(&pool, "corp", String::new())
        .await
        .unwrap()
        .unwrap()
        .key(LinkSource::Admin);
    assert_eq!(key.issuer(), format!("oidc|{}", idp.issuer));
    rustango::sso::link::create_link(&pool, &key, "sub-root", root.id.get().copied().unwrap())
        .await
        .unwrap();
    let resp = handshake(&app, "admin.test", "/login", "corp").await;
    assert!(signed_in(&resp), "{}", location(&resp));

    // Deactivated: the link no longer signs in.
    root.active = false;
    root.save_pool(&pool).await.unwrap();
    let resp = handshake(&app, "admin.test", "/login", "corp").await;
    assert!(!signed_in(&resp), "{}", location(&resp));
}

/// `check --deploy` names an admin provider with no links (#2359).
#[tokio::test]
async fn check_deploy_names_an_unlinked_admin_provider() {
    let _g = SUITE.lock().await;
    let (_app, pool, _idp) = bare_admin().await;
    let check = || async {
        let mut out = Vec::new();
        let res = rustango::migrate::manage::run_with_writer(
            &pool,
            std::path::Path::new("/nonexistent"),
            vec!["check".to_owned(), "--deploy".to_owned()],
            &mut out,
        )
        .await;
        let out = String::from_utf8(out).unwrap();
        assert_check_ran(res, &out);
        out
    };
    // `corp` allows email linking, which the admin ignores.
    let out = check().await;
    assert!(
        out.contains("[warning] [sso] admin provider `corp` has no SsoLink rows"),
        "{out}"
    );
    let key = rustango::sso::resolve_by_slug(&pool, "corp", String::new())
        .await
        .unwrap()
        .unwrap()
        .key(LinkSource::Admin);
    rustango::sso::link::create_link(&pool, &key, "sub-root", 1)
        .await
        .unwrap();
    assert!(!check().await.contains("[sso]"));
}

/// A bare-admin SSO login gets `iat` = the logout cut-off + 1 (#1855).
#[tokio::test]
async fn a_bare_admin_sso_login_lands_after_the_logout_cutoff() {
    use base64::Engine as _;
    use rustango::admin::AdminUser;
    let _g = SUITE.lock().await;
    let (app, pool, idp) = bare_admin().await;
    let mut root = AdminUser::objects()
        .filter("username", "root")
        .fetch(&pool)
        .await
        .unwrap()
        .remove(0);
    let key = rustango::sso::resolve_by_slug(&pool, "corp", String::new())
        .await
        .unwrap()
        .unwrap()
        .key(LinkSource::Admin);
    rustango::sso::link::create_link(&pool, &key, "sub-root", root.id.get().copied().unwrap())
        .await
        .unwrap();
    let cut = chrono::DateTime::from_timestamp(chrono::Utc::now().timestamp() + 60, 0).unwrap();
    root.sessions_revoked_at = Some(cut);
    root.save_pool(&pool).await.unwrap();

    idp.assert("sub-root", "root@example.com");
    let resp = handshake(&app, "admin.test", "/login", "corp").await;
    let cookie = set_cookies(&resp)
        .into_iter()
        .find_map(|c| c.strip_prefix("rustango_admin_session=").map(str::to_owned))
        .unwrap_or_else(|| panic!("not signed in: {}", location(&resp)));
    let body = cookie.split_once('.').unwrap().0;
    let json = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(body)
        .unwrap();
    let iat = serde_json::from_slice::<serde_json::Value>(&json).unwrap()["iat"].as_i64();
    assert_eq!(iat, Some(cut.timestamp() + 1));
    let home = send(
        &app,
        Request::builder()
            .uri("/")
            .header(header::HOST, "admin.test")
            .header(header::COOKIE, format!("rustango_admin_session={cookie}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(home.status(), StatusCode::OK, "the SSO session must work");
}

/// #2249 — SSO for a user with a confirmed TOTP device asks for the code first.
#[cfg(feature = "totp")]
#[tokio::test]
async fn a_bare_admin_sso_login_owes_the_totp_code() {
    use rustango::admin::{totp_store, AdminUser};
    let _g = SUITE.lock().await;
    let (app, pool, idp) = bare_admin().await;
    let root = AdminUser::objects()
        .filter("username", "root")
        .fetch(&pool)
        .await
        .unwrap()
        .remove(0);
    let uid = root.id.get().copied().unwrap();
    let key = rustango::sso::resolve_by_slug(&pool, "corp", String::new())
        .await
        .unwrap()
        .unwrap()
        .key(LinkSource::Admin);
    rustango::sso::link::create_link(&pool, &key, "sub-root", uid)
        .await
        .unwrap();
    let device = rustango::totp::TotpSecret::generate();
    totp_store::start_enrollment(&pool, uid, &device)
        .await
        .unwrap();
    totp_store::confirm(&pool, uid).await.unwrap();

    idp.assert("sub-root", "root@example.com");
    let resp = handshake(&app, "admin.test", "/login", "corp").await;
    assert!(!signed_in(&resp), "SSO alone must not mint the session");
    let pending = set_cookies(&resp)
        .into_iter()
        .find(|c| c.starts_with("rustango_admin_sso_totp="))
        .expect("pending cookie");

    let submit = |code: String, pending: String| {
        let app = app.clone();
        async move {
            send(
                &app,
                Request::builder()
                    .method("POST")
                    .uri("/login/sso-totp")
                    .header(header::HOST, "admin.test")
                    .header(header::COOKIE, format!("rustango_csrf=t; {pending}"))
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!("_csrf=t&totp_code={code}")))
                    .unwrap(),
            )
            .await
        }
    };
    assert!(
        !signed_in(&submit("000000".into(), pending.clone()).await),
        "wrong code"
    );
    assert!(
        !signed_in(&submit(String::new(), "rustango_admin_sso_totp=forged.x".into()).await),
        "forged pending cookie"
    );
    let code = rustango::totp::generate(&device, 30, 6);
    assert!(
        signed_in(&submit(code, pending).await),
        "right code signs in"
    );
}

#[tokio::test]
async fn real_admin_migrations_create_the_link_table() {
    let _g = SUITE.lock().await;
    std::env::set_var("RUSTANGO_SECRET_KEY", "sso-link-test-key");
    let dir = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("admin.db").display()
    );
    let pool = Pool::connect(&url).await.unwrap();
    let migrations = dir.path().join("migrations");
    std::fs::create_dir_all(&migrations).unwrap();
    // The bare admin's `manage migrate`: applies the framework's system chain.
    rustango::migrate::manage::run_with_writer(
        &pool,
        &migrations,
        vec!["migrate".to_owned()],
        &mut Vec::new(),
    )
    .await
    .unwrap();
    let key = rustango::sso::ProviderKey::app("https://idp.example").unwrap();
    rustango::sso::link::create_link(&pool, &key, "sub", 1)
        .await
        .unwrap();
    assert!(rustango::sso::link::create_link(&pool, &key, "sub", 2)
        .await
        .is_err());
}

// ---- member flow --------------------------------------------------------

fn profile(sub: &str, email: &str) -> rustango::oauth2::NormalizedUser {
    rustango::oauth2::NormalizedUser {
        provider: "google".into(),
        provider_user_id: sub.into(),
        email: Some(email.into()),
        email_verified: true,
        name: None,
        avatar_url: None,
        raw: serde_json::json!({}),
    }
}

#[tokio::test]
async fn member_flow_provisions_with_a_link_and_tells_not_linked_apart() {
    use rustango::sso::ProviderKey;
    use rustango::tenancy::member_auth::{find_or_provision_member, MemberSignIn};
    let _g = SUITE.lock().await;
    let env = boot().await;
    let pool = env.pool();
    let key = ProviderKey::app("https://accounts.google.com").unwrap();
    let root = env.user("root", "root@example.com", true).await;
    let ann = env.user("ann", "ann@example.com", false).await;

    // Existing accounts: not linked without opt-in, and a superuser never.
    let p = profile("g-ann", "ann@example.com");
    assert_eq!(
        find_or_provision_member(pool, &key, false, &p, true).await,
        Ok(MemberSignIn::NotLinked)
    );
    let p = profile("g-root", "root@example.com");
    assert_eq!(
        find_or_provision_member(pool, &key, true, &p, true).await,
        Ok(MemberSignIn::NotLinked)
    );
    let p = profile("g-ann", "ann@example.com");
    assert_eq!(
        find_or_provision_member(pool, &key, true, &p, true).await,
        Ok(MemberSignIn::Member(ann))
    );
    let p = profile("g-nobody", "nobody@example.com");
    assert_eq!(
        find_or_provision_member(pool, &key, false, &p, false).await,
        Ok(MemberSignIn::NoAccount)
    );

    // A new email is provisioned and linked: later its sub alone signs in.
    let p = profile("g-new", "new@example.com");
    let MemberSignIn::Member(new) = find_or_provision_member(pool, &key, false, &p, true)
        .await
        .unwrap()
    else {
        panic!("provisioned");
    };
    let p = profile("g-new", "root@example.com");
    assert_eq!(
        find_or_provision_member(pool, &key, false, &p, false).await,
        Ok(MemberSignIn::Member(new))
    );
    assert_ne!(new, root);

    // An unverified email never links or provisions.
    let mut p = profile("g-unverified", "ann@example.com");
    p.email_verified = false;
    assert_eq!(
        find_or_provision_member(pool, &key, true, &p, true).await,
        Ok(MemberSignIn::Unverified)
    );
}

/// The member HTTP callback. `Tenant` defaults to Postgres when that feature
/// is on, so this runs in the SQLite-only build.
#[cfg(not(feature = "postgres"))]
#[tokio::test]
async fn member_callback_signs_in_by_link_only() {
    use rustango::extractors::TenantContext;
    use rustango::tenancy::{member_sso_router, MemberAuthConfig, MEMBER_COOKIE};
    let _g = SUITE.lock().await;
    let env = boot().await;
    let ann = env.user("ann", "ann@example.com", false).await;
    env.tenant_provider("corp", false).await;
    let ctx = Arc::new(TenantContext::<sqlx::Sqlite> {
        pools: env._pools.clone(),
        resolver: ChainResolver::new().push(SubdomainResolver::new("app.test")),
        session_secret: env.secret.clone(),
        operator_secret: env.secret.clone(),
    });
    let app = member_sso_router(MemberAuthConfig::default()).layer(axum::Extension(ctx));
    let member = |sub: &'static str, email: &'static str| {
        let app = app.clone();
        let env = &env;
        async move {
            env.idp.assert(sub, email);
            let resp = handshake(&app, &env.tenants[0].host, "/auth", "corp").await;
            set_cookies(&resp)
                .iter()
                .any(|c| c.starts_with(&format!("{MEMBER_COOKIE}=")))
        }
    };
    assert!(
        !member("sub-ann", "ann@example.com").await,
        "existing, not linked"
    );
    assert!(member("sub-new", "new@example.com").await, "provisioned");
    assert!(member("sub-new", "changed@example.com").await, "by link");
    let new = User::objects()
        .filter("email", "new@example.com")
        .fetch(env.pool())
        .await
        .unwrap()[0]
        .id
        .get()
        .copied()
        .unwrap();
    assert_ne!(new, ann);
    env.deactivate(new).await;
    assert!(!member("sub-new", "new@example.com").await, "inactive");
}

/// #2145 — a path-prefix tenant's member SSO runs under its prefix, and
/// the IdP is sent back there.
#[tokio::test]
async fn member_sso_under_a_path_prefix_keeps_the_prefix() {
    use rustango::extractors::TenantContext;
    use rustango::tenancy::member_auth::{member_sso_router_for, MemberAuthConfig};
    use rustango::tenancy::{PathPrefixResolver, MEMBER_COOKIE};
    let _g = SUITE.lock().await;
    let env = boot().await;
    env.tenant_provider("corp", false).await;
    let registry = env._pools.registry_pool();
    let mut org = Org::objects()
        .filter("slug", "acme")
        .fetch(&registry)
        .await
        .unwrap()
        .remove(0);
    org.path_prefix = Some("/acme".into());
    org.save_pool(&registry).await.unwrap();
    let ctx = Arc::new(TenantContext::<sqlx::Sqlite> {
        pools: env._pools.clone(),
        resolver: ChainResolver::new().push(PathPrefixResolver),
        session_secret: env.secret.clone(),
        operator_secret: env.secret.clone(),
    });
    let app = member_sso_router_for::<sqlx::Sqlite>(MemberAuthConfig::default())
        .layer(axum::Extension(ctx));
    let begin = send(
        &app,
        Request::builder()
            .uri("/acme/auth/sso/corp")
            .header(header::HOST, "app.test")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let to = location(&begin);
    assert!(
        to.contains("redirect_uri=https%3A%2F%2Fapp.test%2Facme%2Fauth%2Fsso%2Fcorp%2Fcallback"),
        "callback must keep the prefix: {to}"
    );
    env.idp.assert("sub-p", "p@example.com");
    let resp = handshake(&app, "app.test", "/acme/auth", "corp").await;
    assert!(
        set_cookies(&resp)
            .iter()
            .any(|c| c.starts_with(&format!("{MEMBER_COOKIE}="))),
        "signed in under the prefix: {:?}",
        location(&resp)
    );
    assert_eq!(location(&resp), "/acme/", "lands under the prefix");
    let denied = send(
        &app,
        Request::builder()
            .uri("/acme/auth/sso/corp/callback?error=denied")
            .header(header::HOST, "app.test")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let body = axum::body::to_bytes(denied.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        body.contains("href=\"/acme/auth\""),
        "back link keeps the prefix: {body}"
    );
}

// ---- upgrade: a framework column lives only in the system chain ----------

/// Strip every `allow_email_link` entry from a migration file, as the
/// framework before it would have written the file.
fn without_email_link(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Array(items) => {
            items.retain(|i| {
                !["name", "column"]
                    .iter()
                    .any(|k| i.get(k).and_then(|n| n.as_str()) == Some("allow_email_link"))
            });
            items.iter_mut().for_each(without_email_link);
        }
        serde_json::Value::Object(map) => map.values_mut().for_each(without_email_link),
        _ => {}
    }
}

#[tokio::test]
async fn upgrading_adds_the_framework_column_once() {
    let _g = SUITE.lock().await;
    std::env::set_var("RUSTANGO_SECRET_KEY", "sso-link-test-key");
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}?mode=rwc", dir.path().join("app.db").display());
    let pool = Pool::connect(&url).await.unwrap();
    let migrations = dir.path().join("migrations");
    std::fs::create_dir_all(&migrations).unwrap();
    let manage = |verb: &str| {
        let (pool, migrations, verb) = (pool.clone(), migrations.clone(), verb.to_owned());
        async move {
            let mut out = Vec::new();
            rustango::migrate::manage::run_with_writer(&pool, &migrations, vec![verb], &mut out)
                .await
                .map_err(|e| format!("{e}\n{}", String::from_utf8_lossy(&out)))
        }
    };
    manage("makemigrations").await.unwrap();
    manage("migrate").await.unwrap();

    // Rewind to a framework without the column: files and database.
    for sub in ["migrations", "system/migrations"] {
        for entry in std::fs::read_dir(dir.path().join(sub)).unwrap() {
            let path = entry.unwrap().path();
            let mut v: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            without_email_link(&mut v);
            std::fs::write(&path, serde_json::to_string_pretty(&v).unwrap()).unwrap();
        }
    }
    // Older projects' app snapshots carry the framework tables too.
    let latest = |sub: &str| {
        let mut files: Vec<_> = std::fs::read_dir(dir.path().join(sub))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        files.sort();
        files.pop().unwrap()
    };
    let read = |p: &std::path::Path| -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
    };
    let old_table = read(&latest("system/migrations"))["snapshot"]["tables"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "rustango_sso_providers")
        .cloned()
        .expect("framework table in the system snapshot");
    // An app migration (no ops) whose snapshot has the old framework table.
    let mut app = read(&latest("system/migrations"));
    app["name"] = "0001_app".into();
    app["forward"] = serde_json::json!([]);
    app["snapshot"]["tables"] = serde_json::json!([old_table]);
    if let Some(m) = app.as_object_mut() {
        m.remove("prev");
    }
    std::fs::write(
        dir.path().join("migrations/0001_app.json"),
        serde_json::to_string_pretty(&app).unwrap(),
    )
    .unwrap();
    // Test-only DDL: the old table had no such column.
    rustango::sql::raw_execute_pool(
        &pool,
        "ALTER TABLE rustango_sso_providers DROP COLUMN allow_email_link",
        Vec::new(),
    )
    .await
    .unwrap();

    manage("makemigrations").await.unwrap();
    let mentions = |sub: &str| {
        std::fs::read_dir(dir.path().join(sub))
            .unwrap()
            .filter(|e| {
                std::fs::read_to_string(e.as_ref().unwrap().path())
                    .unwrap()
                    .contains("allow_email_link")
            })
            .count()
    };
    assert_eq!(mentions("migrations"), 0, "the app chain never adds it");
    assert!(mentions("system/migrations") > 0, "the system chain does");
    manage("migrate").await.expect("the column is added once");
    let p = provider_row("https://idp.example", "corp", true);
    let mut p = p;
    p.insert_pool(&pool).await.unwrap();
    assert!(
        rustango::sso::resolve_by_slug(&pool, "corp", String::new())
            .await
            .unwrap()
            .unwrap()
            .allow_email_link
    );
}

#[tokio::test]
async fn admin_written_links_get_the_key_and_stay_unique() {
    use rustango::sso::SsoLink;
    let _g = SUITE.lock().await;
    let env = boot().await;
    let root = env.user("root", "root@example.com", true).await;
    let ann = env.user("ann", "ann@example.com", false).await;
    env.tenant_provider("corp", false).await;
    let issuer = format!("oidc|{}", env.idp.issuer);
    let form = |subject: &str| {
        format!(
            "provider_source=tenant&provider_id=1&issuer={}&subject={subject}&user_id={ann}",
            urlencoding::encode(&issuer)
        )
    };
    let links = || async { SsoLink::objects().fetch(env.pool()).await.unwrap() };
    env.admin_post(root, "/__admin/rustango_sso_links", &form("sub-a"))
        .await;
    env.admin_post(root, "/__admin/rustango_sso_links", &form("sub-a"))
        .await;
    let rows = links().await;
    assert_eq!(rows.len(), 1, "the second row for one identity is refused");
    assert_eq!(
        rows[0].key_sha256,
        rustango::sso::link::key_sha256(&issuer, "sub-a")
    );
    assert_eq!(env.sso("corp", "sub-a", "x@example.com").await, Ok(ann));

    // Editing the subject rewrites the key; the new identity signs in.
    let id = rows[0].id.get().copied().unwrap();
    env.admin_post(
        root,
        &format!("/__admin/rustango_sso_links/{id}"),
        &form("sub-b"),
    )
    .await;
    let rows = links().await;
    assert_eq!(
        rows[0].key_sha256,
        rustango::sso::link::key_sha256(&issuer, "sub-b")
    );
    assert_eq!(env.sso("corp", "sub-b", "x@example.com").await, Ok(ann));
    assert!(env.sso("corp", "sub-a", "x@example.com").await.is_err());
}

/// Audit sources recorded on the registry for `(table, pk)`.
async fn audit_sources(env: &Env, table: &str, pk: &str) -> Vec<String> {
    rustango::audit::fetch_for_entity_pool(&env._pools.registry_pool(), table, pk)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.source)
        .collect()
}

/// #2424 — shared provider create, email-link and delete each write an audit row.
#[tokio::test]
async fn shared_provider_changes_are_audited() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    env.shared_provider("corp", false).await;
    let path = env.email_link_path().await;
    let id = path.split('/').rev().nth(1).unwrap().to_owned();
    env.set_shared_email_link(true).await;
    assert_eq!(
        env.console_post(&path.replace("/email-link", "/delete"), String::new())
            .await,
        StatusCode::SEE_OTHER
    );
    let sources = audit_sources(&env, "rustango_shared_sso_providers", &id).await;
    for verb in [
        "sso_shared_create",
        "sso_shared_email_link",
        "sso_shared_delete",
    ] {
        assert!(
            sources.iter().any(|s| s.ends_with(&format!(":{verb}"))),
            "{verb} not audited: {sources:?}"
        );
    }
}

/// #2424 — an operator's own password change writes an audit row.
#[tokio::test]
async fn an_operator_password_change_is_audited() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    let form = "current_password=op-pass-123&new_password=Another-strong-pass-9!&confirm_password=Another-strong-pass-9!";
    assert_eq!(
        env.console_post("/change-password", form.into()).await,
        StatusCode::SEE_OTHER
    );
    let op = Operator::objects()
        .filter("username", "op")
        .fetch(&env._pools.registry_pool())
        .await
        .unwrap()
        .remove(0);
    let id = op.id.get().unwrap().to_string();
    let sources = audit_sources(&env, "rustango_operators", &id).await;
    assert!(
        sources.iter().any(|s| s.ends_with(":change_password")),
        "not audited: {sources:?}"
    );
}
