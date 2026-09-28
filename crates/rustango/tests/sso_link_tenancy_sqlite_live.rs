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
    console: Router,
    pools: Arc<TenantPools<sqlx::Sqlite>>,
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
    let console = rustango::tenancy::operator_console::router(
        registry.clone(),
        rustango::tenancy::operator_console::SessionSecret::from_bytes(vec![9u8; 32]),
    );
    let mut op = Operator {
        id: Auto::default(),
        username: "op".into(),
        password_hash: rustango::tenancy::password::hash("op-pass-123").unwrap(),
        active: true,
        created_at: chrono::Utc::now(),
        password_changed_at: None,
    };
    op.insert_pool(&registry).await.unwrap();

    let [a, b]: [Tenant; 2] = tenants.try_into().ok().unwrap();
    Env {
        admin,
        console,
        pools,
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
        let login = send(
            &self.console,
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

    /// Flip the only shared provider's email linking from the console list.
    async fn toggle_shared_email_link(&self) {
        let html = self.shared_list().await;
        let path = html
            .split("action=\"")
            .filter_map(|s| s.split('"').next())
            .find(|p| p.ends_with("/email-link"))
            .expect("toggle form")
            .to_owned();
        assert_eq!(
            self.console_post(&path, String::new()).await,
            StatusCode::SEE_OTHER
        );
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

#[tokio::test]
async fn toggling_shared_email_linking_keeps_the_provider_and_its_links() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    let ann = env.user("ann", "ann@example.com", false).await;
    env.shared_provider("corp", false).await;
    assert!(env.sso("corp", "sub-ann", "ann@example.com").await.is_err());
    env.toggle_shared_email_link().await;
    assert!(env.shared_list().await.contains("Stop email linking"));
    assert_eq!(env.sso("corp", "sub-ann", "ann@example.com").await, Ok(ann));
    env.toggle_shared_email_link().await;
    assert_eq!(env.sso("corp", "sub-ann", "ann@example.com").await, Ok(ann));
}

#[tokio::test]
async fn only_a_superuser_adds_or_changes_links_and_providers() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    let root = env.user("root", "root@example.com", true).await;
    let staff = env.user("staff", "staff@example.com", false).await;
    for table in ["rustango_sso_links", "rustango_sso_providers"] {
        for action in ["add", "change", "view"] {
            rustango::tenancy::permissions::set_user_perm_pool(
                staff,
                &format!("{table}.{action}"),
                true,
                env.pool(),
            )
            .await
            .unwrap();
        }
    }
    env.tenant_provider("corp", false).await;
    let link = format!(
        "provider_source=tenant&provider_id=1&issuer=x&subject=s&subject_sha256={}&user_id={root}",
        rustango::sso::link::subject_sha256("s")
    );
    let provider = "slug=evil&label=e&kind=oidc&client_id=c&client_secret=s&enabled=on&sort_order=0&allow_email_link=on";
    for (path, form) in [
        ("/__admin/rustango_sso_links", link.as_str()),
        ("/__admin/rustango_sso_providers", provider),
        ("/__admin/rustango_sso_providers/1", provider),
    ] {
        assert_eq!(
            env.admin_post(staff, path, form).await,
            StatusCode::FORBIDDEN,
            "staff {path}"
        );
    }
    // A superuser gets through the same routes.
    let resp = env
        .admin_post(root, "/__admin/rustango_sso_links", &link)
        .await;
    assert_ne!(resp, StatusCode::FORBIDDEN);
    assert!(rustango::sso::SsoLink::objects()
        .fetch(env.pool())
        .await
        .unwrap()
        .iter()
        .any(|l| l.user_id == root));
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
        Ok(MemberSignIn::Refused)
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
        pools: env.pools.clone(),
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
