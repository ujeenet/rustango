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
use rustango::sso::SsoProvider;
use rustango::tenancy::tenant_console::{decode, SessionSecret, COOKIE_NAME};
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

struct Env {
    admin: Router,
    console: Router,
    tenant: Pool,
    secret: SessionSecret,
    slug: String,
    host: String,
    idp: Idp,
    _dir: tempfile::TempDir,
}

async fn boot() -> Env {
    std::env::set_var("RUSTANGO_SECRET_KEY", "sso-link-test-key");
    let dir = tempfile::tempdir().unwrap();
    let reg_url = format!("sqlite://{}?mode=rwc", dir.path().join("reg.db").display());
    let tenant_url = format!("sqlite://{}?mode=rwc", dir.path().join("t.db").display());
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

    let slug = "acme".to_owned();
    let host = format!("{slug}.app.test");
    let mut org = Org {
        slug: slug.clone(),
        storage_mode: "database".into(),
        backend_kind: "sqlite".into(),
        database_url: Some(tenant_url.clone()),
        host_pattern: Some(host.clone()),
        ..rustango::testkit::org()
    };
    org.insert_pool(&registry).await.unwrap();

    let tenant = Pool::connect(&tenant_url).await.unwrap();
    rustango::testkit::create_tables_for::<User>(&tenant)
        .await
        .unwrap();
    rustango::testkit::create_tables_for::<SsoProvider>(&tenant)
        .await
        .unwrap();
    rustango::tenancy::permissions::ensure_tables_pool(&tenant)
        .await
        .unwrap();

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

    Env {
        admin,
        console,
        tenant,
        secret,
        slug,
        host,
        idp: Idp::start().await,
        _dir: dir,
    }
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

/// Run begin + callback on `app` under `login`; return the callback response.
async fn handshake(app: &Router, host: &str, login: &str, slug: &str) -> axum::response::Response {
    let begin = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("{login}/sso/{slug}"))
                .header(header::HOST, host)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let to = location(&begin);
    let state = to
        .split(['?', '&'])
        .find_map(|kv| kv.strip_prefix("state="))
        .unwrap_or_else(|| panic!("begin must redirect to the IdP, got {to:?}"))
        .to_owned();
    let flow = set_cookies(&begin).join("; ");
    app.clone()
        .oneshot(
            Request::builder()
                .uri(format!("{login}/sso/{slug}/callback?code=c&state={state}"))
                .header(header::HOST, host)
                .header(header::COOKIE, flow)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

impl Env {
    async fn user(&self, name: &str, email: &str, superuser: bool) -> i64 {
        let mut u = User {
            username: name.into(),
            password_hash: "x".into(),
            email: Some(email.into()),
            is_superuser: superuser,
            ..rustango::testkit::user()
        };
        u.insert_pool(&self.tenant).await.unwrap();
        u.id.get().copied().unwrap()
    }

    async fn tenant_provider(&self, slug: &str, allow_email_link: bool) {
        let mut p = SsoProvider {
            id: Auto::default(),
            slug: slug.into(),
            label: slug.into(),
            kind: "oidc".into(),
            issuer_url: Some(self.idp.issuer.clone()),
            client_id: "cid".into(),
            client_secret: rustango::casts::Cast::new("csecret".into()),
            enabled: true,
            sort_order: 0,
            scopes: None,
            allow_email_link,
            created_at: Auto::default(),
            updated_at: Auto::default(),
        };
        p.insert_pool(&self.tenant).await.unwrap();
    }

    /// Create a shared provider through the operator console form.
    async fn shared_provider(&self, slug: &str, allow_email_link: bool) {
        let login = self
            .console
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/login")
                    .header(header::COOKIE, "rustango_csrf=t")
                    .header("x-csrf-token", "t")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from("username=op&password=op-pass-123"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(login.status().is_redirection(), "operator login");
        let mut cookies: Vec<String> = set_cookies(&login)
            .into_iter()
            .filter(|c| !c.starts_with("rustango_csrf="))
            .collect();
        cookies.push("rustango_csrf=t".into());
        let mut form = format!(
            "slug={slug}&label={slug}&kind=oidc&issuer_url={}&client_id=cid&client_secret=cs&enabled=on",
            urlencoding::encode(&self.idp.issuer)
        );
        if allow_email_link {
            form.push_str("&allow_email_link=on");
        }
        let resp = self
            .console
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/sso-shared")
                    .header(header::COOKIE, cookies.join("; "))
                    .header("x-csrf-token", "t")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(form))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::SEE_OTHER,
            "shared provider create"
        );
    }

    /// SSO into the tenant admin as `(sub, email)`: the session's user id, or the `sso_error`.
    async fn sso(&self, slug: &str, sub: &str, email: &str) -> Result<i64, String> {
        self.idp.assert(sub, email);
        let resp = handshake(&self.admin, &self.host, "/__login", slug).await;
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
        Ok(decode(&self.secret, &self.slug, &session).unwrap().uid)
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
    rustango::tenancy::permissions::set_user_perm_pool(staff, "post.change", true, &env.tenant)
        .await
        .unwrap();
    env.tenant_provider("corp", true).await;
    assert_eq!(
        env.sso("corp", "sub-root", "root@example.com").await,
        Err("nouser".into())
    );
    assert_eq!(
        env.sso("corp", "sub-staff", "staff@example.com").await,
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
    let idp = Idp::start().await;
    let mut p = SsoProvider {
        id: Auto::default(),
        slug: "corp".into(),
        label: "Corp".into(),
        kind: "oidc".into(),
        issuer_url: Some(idp.issuer.clone()),
        client_id: "cid".into(),
        client_secret: rustango::casts::Cast::new("cs".into()),
        enabled: true,
        sort_order: 0,
        scopes: None,
        allow_email_link: true,
        created_at: Auto::default(),
        updated_at: Auto::default(),
    };
    p.insert_pool(&pool).await.unwrap();
    let mut root = AdminUser::new_with_password("root", "pw-123456789", true).unwrap();
    root.email = Some("root@example.com".into());
    root.insert_pool(&pool).await.unwrap();
    let app = rustango::admin::Builder::new(pool.clone())
        .with_session_auth(rustango::session::SessionSecret::from_bytes(vec![7u8; 32]))
        .build();
    (app, pool, idp)
}

#[tokio::test]
async fn bare_admin_never_links_by_email_and_signs_in_by_link() {
    let _g = SUITE.lock().await;
    let (app, pool, idp) = bare_admin().await;
    idp.assert("sub-root", "root@example.com");
    let resp = handshake(&app, "admin.test", "/login", "corp").await;
    assert!(
        location(&resp).contains("sso_error=nouser"),
        "{}",
        location(&resp)
    );

    // An admin adds the link row; then the same identity signs in.
    let root = rustango::admin::AdminUser::objects()
        .filter("username", "root")
        .fetch(&pool)
        .await
        .unwrap()
        .remove(0);
    let provider_id = SsoProvider::objects().fetch(&pool).await.unwrap()[0]
        .id
        .get()
        .copied()
        .unwrap();
    let mut link = rustango::sso::SsoLink {
        id: Auto::default(),
        provider_source: "admin".into(),
        provider_id,
        issuer: format!("oidc|{}", idp.issuer),
        subject: "sub-root".into(),
        user_id: root.id.get().copied().unwrap(),
        created_at: Auto::default(),
    };
    link.insert_pool(&pool).await.unwrap();
    let resp = handshake(&app, "admin.test", "/login", "corp").await;
    assert!(
        !location(&resp).contains("sso_error"),
        "{}",
        location(&resp)
    );
    assert!(set_cookies(&resp)
        .iter()
        .any(|c| c.starts_with("rustango_admin_session=")));
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
async fn member_flow_provisions_with_a_link_and_refuses_existing_without_opt_in() {
    use rustango::sso::ProviderKey;
    use rustango::tenancy::member_auth::find_or_provision_member;
    let _g = SUITE.lock().await;
    let env = boot().await;
    let key = ProviderKey::app("https://accounts.google.com");
    let root = env.user("root", "root@example.com", true).await;
    let ann = env.user("ann", "ann@example.com", false).await;

    // Existing accounts: refused without opt-in, and a superuser even with it.
    let p = profile("g-ann", "ann@example.com");
    assert_eq!(
        find_or_provision_member(&env.tenant, &key, false, &p, true).await,
        Ok(None)
    );
    let p = profile("g-root", "root@example.com");
    assert_eq!(
        find_or_provision_member(&env.tenant, &key, true, &p, true).await,
        Ok(None)
    );
    let p = profile("g-ann", "ann@example.com");
    assert_eq!(
        find_or_provision_member(&env.tenant, &key, true, &p, true).await,
        Ok(Some(ann))
    );

    // A new email is provisioned and linked: later its sub alone signs in.
    let p = profile("g-new", "new@example.com");
    let new = find_or_provision_member(&env.tenant, &key, false, &p, true)
        .await
        .unwrap()
        .unwrap();
    let p = profile("g-new", "root@example.com");
    assert_eq!(
        find_or_provision_member(&env.tenant, &key, false, &p, false).await,
        Ok(Some(new))
    );
    assert_ne!(new, root);
}
