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
    PathPrefixResolver, SubdomainResolver, TenantPools, User,
};
use tower::ServiceExt as _;

static SUITE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The `nonce` of the last begin, which the fake IdP echoes in its id_token
/// like a real one (tests hold `SUITE`).
static LAST_NONCE: Mutex<Option<String>> = Mutex::new(None);

/// Remember the `nonce` a begin redirect sends to the IdP.
fn note_nonce(to: &str) {
    *LAST_NONCE.lock().unwrap() = to
        .split(['?', '&'])
        .find_map(|kv| kv.strip_prefix("nonce="))
        .map(str::to_owned);
}

/// An ES256 key the fake IdP signs id_tokens with.
struct Signer(ring::signature::EcdsaKeyPair);

impl Signer {
    fn new() -> Self {
        use ring::signature::{EcdsaKeyPair, ECDSA_P256_SHA256_FIXED_SIGNING as ALG};
        let rng = ring::rand::SystemRandom::new();
        let doc = EcdsaKeyPair::generate_pkcs8(&ALG, &rng).unwrap();
        Self(EcdsaKeyPair::from_pkcs8(&ALG, doc.as_ref(), &rng).unwrap())
    }

    fn jwk(&self) -> serde_json::Value {
        use ring::signature::KeyPair as _;
        let p = self.0.public_key().as_ref();
        serde_json::json!({"kty": "EC", "crv": "P-256", "kid": "k1",
            "x": b64(&p[1..33]), "y": b64(&p[33..65])})
    }

    fn sign(&self, claims: &serde_json::Value) -> String {
        let head = b64(br#"{"alg":"ES256","kid":"k1"}"#);
        let input = format!("{head}.{}", b64(claims.to_string().as_bytes()));
        let sig = self
            .0
            .sign(&ring::rand::SystemRandom::new(), input.as_bytes())
            .unwrap();
        format!("{input}.{}", b64(sig.as_ref()))
    }
}

fn b64(v: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v)
}

/// The id_token the fake IdP returns: extra claims, and whether its own key signs it.
type IdTokenPlan = Option<(serde_json::Value, bool)>;

/// A fake OIDC IdP: discovery, token, a userinfo the test sets, and a JWKS.
#[derive(Clone)]
struct Idp {
    issuer: String,
    claims: Arc<Mutex<serde_json::Value>>,
    id_token: Arc<Mutex<IdTokenPlan>>,
}

impl Idp {
    async fn start() -> Self {
        // The fake IdP is on loopback, which outbound calls refuse unless
        // allowlisted. Every test holds `SUITE`, so setting env is safe.
        std::env::set_var("RUSTANGO_OUTBOUND_ALLOW", "127.0.0.1");
        // Tenant-owned provider rows read their own allowlist.
        std::env::set_var("RUSTANGO_TENANT_OUTBOUND_ALLOW", "127.0.0.1");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let claims = Arc::new(Mutex::new(serde_json::json!({})));
        let id_token: Arc<Mutex<IdTokenPlan>> = Arc::new(Mutex::new(None));
        let doc = serde_json::json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{issuer}/auth"),
            "token_endpoint": format!("{issuer}/token"),
            "userinfo_endpoint": format!("{issuer}/userinfo"),
            "jwks_uri": format!("{issuer}/jwks"),
        });
        let (key, rogue) = (Arc::new(Signer::new()), Arc::new(Signer::new()));
        let jwks = serde_json::json!({"keys": [key.jwk()]});
        let c = claims.clone();
        let (plan, user, iss) = (id_token.clone(), claims.clone(), issuer.clone());
        let app = Router::new()
            .route(
                "/.well-known/openid-configuration",
                get(move || async move { Json(doc) }),
            )
            .route("/jwks", get(move || async move { Json(jwks) }))
            .route(
                "/token",
                post(move || async move {
                    let mut body =
                        serde_json::json!({"access_token": "at", "token_type": "Bearer"});
                    if let Some((extra, genuine)) = plan.lock().unwrap().clone() {
                        let now = chrono::Utc::now().timestamp();
                        let mut c = serde_json::json!({"iss": iss, "aud": "cid", "exp": now + 300,
                            "sub": user.lock().unwrap()["sub"].clone(),
                            "nonce": LAST_NONCE.lock().unwrap().clone()});
                        for (k, v) in extra.as_object().unwrap() {
                            c[k] = v.clone();
                        }
                        let signer = if genuine { &key } else { &rogue };
                        body["id_token"] = signer.sign(&c).into();
                    }
                    Json(body)
                }),
            )
            .route(
                "/userinfo",
                get(move || async move { Json(c.lock().unwrap().clone()) }),
            );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            issuer,
            claims,
            id_token,
        }
    }

    /// Return an id_token with `extra` claims, signed by the IdP's key or another.
    fn id_token(&self, plan: IdTokenPlan) {
        *self.id_token.lock().unwrap() = plan;
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
        rustango::testkit::create_tables_for::<rustango::sso::policy::SsoInvite>(&pool)
            .await
            .unwrap();
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
        // A tenant's own host, else the apex with its path prefix.
        ChainResolver::new()
            .push(SubdomainResolver::new("app.test"))
            .push(PathPrefixResolver),
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
    handshake_from(app, host, login, slug, None).await
}

/// `req` from client address `ip`.
fn from_ip(mut req: Request<Body>, ip: Option<&str>) -> Request<Body> {
    if let Some(ip) = ip {
        let addr: std::net::SocketAddr = format!("{ip}:4000").parse().unwrap();
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(addr));
    }
    req
}

/// [`handshake`] from client address `ip`.
async fn handshake_from(
    app: &Router,
    host: &str,
    login: &str,
    slug: &str,
    ip: Option<&str>,
) -> axum::response::Response {
    let begin = send(
        app,
        from_ip(
            Request::builder()
                .uri(format!("{login}/sso/{slug}"))
                .header(header::HOST, host)
                .body(Body::empty())
                .unwrap(),
            ip,
        ),
    )
    .await;
    let to = location(&begin);
    note_nonce(&to);
    let state = to
        .split(['?', '&'])
        .find_map(|kv| kv.strip_prefix("state="))
        .unwrap_or_else(|| panic!("begin must redirect to the IdP, got {to:?}"))
        .to_owned();
    let flow = set_cookies(&begin).join("; ");
    send(
        app,
        from_ip(
            Request::builder()
                .uri(format!("{login}/sso/{slug}/callback?code=c&state={state}"))
                .header(header::HOST, host)
                .header(header::COOKIE, flow)
                .body(Body::empty())
                .unwrap(),
            ip,
        ),
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

async fn set_policy(pool: &Pool, slug: &str, column: &'static str, value: &str) {
    use rustango::sql::UpdaterPool as _;
    SsoProvider::objects()
        .filter("slug", slug.to_owned())
        .update()
        .set(column, value.to_owned())
        .execute_pool(pool)
        .await
        .unwrap();
}

/// Write one integer policy column of provider `slug`.
async fn set_number(pool: &Pool, slug: &str, column: &'static str, value: i64) {
    use rustango::sql::UpdaterPool as _;
    SsoProvider::objects()
        .filter("slug", slug.to_owned())
        .update()
        .set(column, value)
        .execute_pool(pool)
        .await
        .unwrap();
}

/// The rules of the `sso_refused` audit rows for `provider:subject`.
async fn refused_rules(pool: &Pool, provider: &str, subject: &str) -> Vec<String> {
    rustango::audit::fetch_for_entity_pool(
        pool,
        "rustango_sso_links",
        &format!("{provider}:{subject}"),
    )
    .await
    .unwrap()
    .into_iter()
    .filter(|e| e.changes["event"] == "sso_refused")
    .map(|e| e.changes["rule"].as_str().unwrap_or("").to_owned())
    .collect()
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
        allowed_email_domains: None,
        blocked_email_domains: None,
        allowed_idp_tenants: None,
        allowed_hosted_domains: None,
        required_claims: serde_json::json!({}),
        signup_mode: None,
        default_roles: None,
        role_map: serde_json::json!({}),
        role_claims: None,
        sync_roles_on_login: false,
        username_from: "email_local".into(),
        username_clash: "suffix".into(),
        sync_profile: false,
        allow_email_link_staff: false,
        sso_only: false,
        auto_deactivate_after_days: None,
        session_ttl_secs: None,
        superuser_session_ttl_secs: None,
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

    /// Write one policy column of tenant 0's provider `slug`.
    async fn set_policy(&self, slug: &str, column: &'static str, value: &str) {
        set_policy(self.pool(), slug, column, value).await;
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
        self.console_page("/sso-shared").await
    }

    /// A console page's HTML.
    async fn console_page(&self, path: &str) -> String {
        let cookies = self.console_cookies().await;
        let resp = send(
            &self.console,
            Request::builder()
                .uri(path)
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
        self.sso_at(t, &self.tenants[t].host, "/__login", slug, sub, email)
            .await
    }

    /// [`Self::sso_in`] on `host` under `login`.
    async fn sso_at(
        &self,
        t: usize,
        host: &str,
        login: &str,
        slug: &str,
        sub: &str,
        email: &str,
    ) -> Result<i64, String> {
        self.idp.assert(sub, email);
        let tenant = &self.tenants[t];
        let resp = handshake(&self.admin, host, login, slug).await;
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

/// A client hook reaches providers built from tenant rows, on the tenant's
/// host and on the apex under its path prefix.
#[tokio::test]
async fn the_client_hook_reaches_tenant_providers_on_host_and_prefix() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let _g = SUITE.lock().await;
    let env = boot().await;
    let ann = env.user("ann", "ann@example.com", false).await;
    env.tenant_provider("corp", true).await;
    let mut org = Org::objects()
        .filter("slug", "acme")
        .fetch(&env._pools.registry_pool())
        .await
        .unwrap()
        .remove(0);
    org.path_prefix = Some("/acme".into());
    org.save_pool(&env._pools.registry_pool()).await.unwrap();
    rustango::tenancy::invalidate_org_cache();

    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (c, s, issuer) = (calls.clone(), seen.clone(), env.idp.issuer.clone());
    rustango::sso::set_client_config(move |t| {
        if t.issuer_url != Some(issuer.as_str()) {
            return None;
        }
        s.lock().unwrap().push((t.source, t.provider.to_owned()));
        let c = c.clone();
        Some(Arc::new(move |b: reqwest::ClientBuilder| {
            c.fetch_add(1, Ordering::SeqCst);
            b
        }) as rustango::oauth2::ClientConfig)
    });

    assert_eq!(env.sso("corp", "sub-ann", "ann@example.com").await, Ok(ann));
    let on_host = calls.load(Ordering::SeqCst);
    assert!(on_host > 0, "the hook never built a client on the host");
    let via_prefix = env
        .sso_at(
            0,
            "app.test",
            "/acme/__login",
            "corp",
            "sub-ann",
            "ann@example.com",
        )
        .await;
    assert_eq!(via_prefix, Ok(ann));
    assert!(
        calls.load(Ordering::SeqCst) > on_host,
        "the hook never built a client under the prefix"
    );
    assert!(seen
        .lock()
        .unwrap()
        .iter()
        .all(|(src, p)| *src == LinkSource::Tenant && p == "oidc"));
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
    assert!(!out.contains("refuses every"), "{out}");
    // Email linking on a custom IdP is a warning of its own.
    assert!(
        out.contains("[sso] shared provider `team` is a custom OIDC IdP with email linking on"),
        "{out}"
    );
}

/// A provider that signs users up is not stranded; open sign-up with no
/// domain rule gets its own warning.
#[tokio::test]
async fn check_deploy_knows_sign_up() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    env.tenant_provider("corp", false).await;
    env.user("ann", "ann@example.com", false).await;
    env.set_policy("corp", "signup_mode", "invite_only").await;
    let out = env.check_deploy().await;
    assert!(!out.contains("[sso]"), "invite_only is not stranded: {out}");
    env.set_policy("corp", "signup_mode", "open").await;
    let out = env.check_deploy().await;
    assert!(
        out.contains("provider `corp` has signup_mode open and no allowed_email_domains"),
        "{out}"
    );
    env.set_policy("corp", "allowed_email_domains", "example.com")
        .await;
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
    assert!(
        !out.contains("refuses every"),
        "ann can link by email: {out}"
    );
    assert!(
        out.contains("[sso] tenant `acme`: provider `corp` is a custom OIDC IdP"),
        "{out}"
    );
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

/// Bad shared-provider input is a 400, never a stored row or a 500.
#[tokio::test]
async fn the_console_refuses_an_invalid_shared_provider() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    env.shared_provider("corp", false).await;
    for form in [
        "slug=x&label=x&kind=myspace&client_id=c&client_secret=s",
        "slug=x&label=x&kind=oidc&client_id=c&client_secret=s",
        "slug=a%2Fb&label=x&kind=google&client_id=c&client_secret=s",
        "slug=corp&label=x&kind=google&client_id=c&client_secret=s",
    ] {
        let status = env.console_post("/sso-shared", form.into()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{form}");
    }
    let html = env.shared_list().await;
    assert!(!html.contains("myspace") && !html.contains("a/b"), "{html}");
}

/// The page shows the redirect URI to register, per tenant host, and the
/// issuer row only for `oidc`.
#[tokio::test]
async fn the_shared_page_shows_callback_urls_and_the_issuer_row() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    let html = env.shared_list().await;
    assert!(html.contains("class=\"sso-issuer-row\">"), "{html}");
    env.shared_provider("corp", false).await;
    // Tera escapes `/` as `&#x2F;`.
    let html = env.shared_list().await.replace("&#x2F;", "/");
    assert!(
        html.contains("https://&lt;tenant host&gt;/login/sso/corp/callback"),
        "{html}"
    );
    assert!(
        html.contains("acme.app.test/login/sso/corp/callback"),
        "{html}"
    );
    let edit = env.email_link_path().await.replace("/email-link", "/edit");
    let html = env.console_page(&edit).await.replace("&#x2F;", "/");
    assert!(html.contains("class=\"sso-issuer-row\">"), "{html}");
    assert!(
        html.contains("acme.app.test/login/sso/corp/callback"),
        "{html}"
    );
    let form = "label=corp&kind=google&client_id=cid&enabled=on";
    assert_eq!(
        env.console_post(&edit, form.into()).await,
        StatusCode::SEE_OTHER
    );
    assert!(env
        .console_page(&edit)
        .await
        .contains("class=\"sso-issuer-row\" hidden"));
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
        (
            &path.replace("/email-link", "/edit"),
            "label=x&kind=google&client_id=c",
        ),
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
// The legacy helper, kept until it is removed.
#[allow(deprecated)]
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
                rustango::testkit::routed_args(
                    &args.iter().map(String::as_str).collect::<Vec<_>>(),
                ),
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

/// A pinned IdP tenant is read only from an id_token the IdP's JWKS
/// verifies. The browser sees one generic code; the audit log the rule.
#[tokio::test]
async fn idp_tenant_pinning_trusts_only_a_verified_id_token() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    rustango::audit::ensure_table_pool(env.pool())
        .await
        .unwrap();
    let ann = env.user("ann", "ann@example.com", false).await;
    env.tenant_provider("corp", true).await;
    env.set_policy("corp", "allowed_idp_tenants", "t1").await;
    let signed = |tid: &str, genuine| Some((serde_json::json!({ "tid": tid }), genuine));

    env.idp.id_token(signed("t1", true));
    assert_eq!(env.sso("corp", "sub-ann", "ann@example.com").await, Ok(ann));
    env.idp.id_token(signed("t2", true));
    assert_eq!(
        env.sso("corp", "sub-ann", "ann@example.com").await,
        Err("nouser".into())
    );
    // Signed by a key the JWKS lacks.
    env.idp.id_token(signed("t1", false));
    assert_eq!(
        env.sso("corp", "sub-ann", "ann@example.com").await,
        Err("nouser".into())
    );
    // No id_token: a `tid` in userinfo vouches for nothing.
    env.idp.id_token(None);
    env.idp.assert("sub-ann", "ann@example.com");
    env.idp.claims.lock().unwrap()["tid"] = "t1".into();
    let resp = handshake(&env.admin, &env.tenants[0].host, "/__login", "corp").await;
    assert_eq!(location(&resp), "/__login?sso_error=nouser");
    assert_eq!(
        refused_rules(env.pool(), "corp", "sub-ann").await,
        ["idp_tenant", "idp_tenant", "idp_tenant"]
    );
}

/// The tenant console refuses a linked user outside the allowed domains;
/// the login page shows a fixed message and the tenant's escaped note only.
#[tokio::test]
async fn tenant_domain_rule_refuses_a_linked_user() {
    use rustango::sso::policy::SsoTenantPolicy;
    let _g = SUITE.lock().await;
    let env = boot().await;
    rustango::audit::ensure_table_pool(env.pool())
        .await
        .unwrap();
    rustango::testkit::create_tables_for::<SsoTenantPolicy>(env.pool())
        .await
        .unwrap();
    SsoTenantPolicy {
        id: Auto::default(),
        name: "default".into(),
        access_denied_message: Some("<b>Ask IT</b>".into()),
        password_login: "on".into(),
        sso_only: false,
        default_provider: None,
        signups_per_hour: None,
    }
    .insert_pool(env.pool())
    .await
    .unwrap();
    let ann = env.user("ann", "ann@example.com", false).await;
    env.tenant_provider("corp", true).await;
    assert_eq!(env.sso("corp", "sub-ann", "ann@example.com").await, Ok(ann));
    env.set_policy("corp", "allowed_email_domains", "client.org")
        .await;
    assert_eq!(
        env.sso("corp", "sub-ann", "ann@example.com").await,
        Err("nouser".into())
    );
    assert_eq!(
        refused_rules(env.pool(), "corp", "sub-ann").await,
        ["domain_not_allowed"]
    );
    let page = send(
        &env.admin,
        Request::builder()
            .uri("/__login?sso_error=nouser")
            .header(header::HOST, &env.tenants[0].host)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let body = axum::body::to_bytes(page.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = String::from_utf8(body.to_vec()).unwrap();
    assert!(body.contains("Ask your administrator for access"), "{body}");
    assert!(
        body.contains("&lt;b&gt;Ask IT&lt;&#x2F;b&gt;")
            || body.contains("&lt;b&gt;Ask IT&lt;/b&gt;"),
        "{body}"
    );
    assert!(!body.contains("domain_not_allowed") && !body.contains("client.org"));
}

/// The member flow applies the rules before it provisions anyone.
#[cfg(not(feature = "postgres"))]
#[tokio::test]
async fn member_domain_rule_refuses_before_provisioning() {
    use rustango::extractors::TenantContext;
    use rustango::tenancy::{member_sso_router, MemberAuthConfig, MEMBER_COOKIE};
    let _g = SUITE.lock().await;
    let env = boot().await;
    rustango::audit::ensure_table_pool(env.pool())
        .await
        .unwrap();
    env.tenant_provider("corp", false).await;
    env.set_policy("corp", "allowed_email_domains", "client.org")
        .await;
    let ctx = Arc::new(TenantContext::<sqlx::Sqlite> {
        pools: env._pools.clone(),
        resolver: ChainResolver::new().push(SubdomainResolver::new("app.test")),
        session_secret: env.secret.clone(),
        operator_secret: env.secret.clone(),
    });
    let app = member_sso_router(MemberAuthConfig {
        auto_provision: true,
        ..MemberAuthConfig::default()
    })
    .layer(axum::Extension(ctx));
    let member = |sub: &'static str, email: &'static str| {
        let app = app.clone();
        let env = &env;
        async move {
            env.idp.assert(sub, email);
            let resp = handshake(&app, &env.tenants[0].host, "/auth", "corp").await;
            assert!(!location(&resp).contains("domain"), "{}", location(&resp));
            set_cookies(&resp)
                .iter()
                .any(|c| c.starts_with(&format!("{MEMBER_COOKIE}=")))
        }
    };
    assert!(
        !member("sub-eve", "eve@gmail.com").await,
        "outside the domains"
    );
    assert!(User::objects()
        .filter("email", "eve@gmail.com")
        .fetch(env.pool())
        .await
        .unwrap()
        .is_empty());
    assert!(member("sub-new", "new@client.org").await, "provisioned");
    assert_eq!(
        refused_rules(env.pool(), "corp", "sub-eve").await,
        ["domain_not_allowed"]
    );
}

/// A rule's refusal and an unknown account look the same to the browser.
#[tokio::test]
async fn tenant_refusals_look_like_no_account() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    env.user("ann", "ann@example.com", false).await;
    env.tenant_provider("corp", true).await;
    env.set_policy("corp", "allowed_email_domains", "example.com")
        .await;
    let callback = |sub: &'static str, email: &'static str| {
        let env = &env;
        async move {
            env.idp.assert(sub, email);
            let resp = handshake(&env.admin, &env.tenants[0].host, "/__login", "corp").await;
            (resp.status(), location(&resp))
        }
    };
    let unknown = callback("sub-x", "nobody@example.com").await;
    let refused = callback("sub-y", "eve@gmail.com").await;
    assert_eq!(refused, unknown);
}

/// Invite-only sign-up on the tenant console: the invitee lands with the
/// invited role; refused sign-ups look like an unknown account.
#[tokio::test]
async fn tenant_console_signs_up_an_invitee_only() {
    use rustango::sso::policy::SsoInvite;
    let _g = SUITE.lock().await;
    let env = boot().await;
    rustango::audit::ensure_table_pool(env.pool())
        .await
        .unwrap();
    env.tenant_provider("corp", false).await;
    let viewer = rustango::tenancy::permissions::get_or_create_role_pool("Viewer", "", env.pool())
        .await
        .unwrap();
    let callback = |sub: &'static str, email: &'static str| {
        let env = &env;
        async move {
            env.idp.assert(sub, email);
            let resp = handshake(&env.admin, &env.tenants[0].host, "/__login", "corp").await;
            (resp.status(), location(&resp))
        }
    };
    let closed = callback("sub-x", "x@client.org").await;
    env.set_policy("corp", "signup_mode", "invite_only").await;
    SsoInvite {
        id: Auto::default(),
        provider_slug: Some("corp".into()),
        target: "ann@client.org".into(),
        roles: Some("Viewer".into()),
        expires_at: None,
        max_uses: Some(1),
        uses: 0,
        used_at: None,
        used_by: None,
        invited_by: None,
        link_user_id: None,
        link_key: None,
        created_at: Auto::default(),
    }
    .insert_pool(env.pool())
    .await
    .unwrap();
    let uninvited = callback("sub-y", "y@client.org").await;
    assert_eq!(uninvited, closed);
    let uid = env.sso("corp", "sub-ann", "ann@client.org").await.unwrap();
    let ann = User::objects()
        .filter("id", uid)
        .fetch(env.pool())
        .await
        .unwrap()
        .remove(0);
    assert!(!ann.is_superuser);
    assert_eq!(
        rustango::tenancy::permissions::user_roles_pool(uid, env.pool())
            .await
            .unwrap(),
        [(viewer, "Viewer".to_owned())]
    );
    assert_eq!(
        refused_rules(env.pool(), "corp", "sub-y").await,
        ["not_invited"]
    );
    assert_eq!(
        refused_rules(env.pool(), "corp", "sub-x").await,
        ["signup_closed"]
    );
}

/// The member flow: a domain refusal and a missing account get
/// byte-identical error pages.
#[cfg(not(feature = "postgres"))]
#[tokio::test]
async fn member_refusals_look_like_no_account() {
    use rustango::extractors::TenantContext;
    use rustango::tenancy::{member_sso_router, MemberAuthConfig};
    let _g = SUITE.lock().await;
    let env = boot().await;
    env.tenant_provider("corp", false).await;
    env.set_policy("corp", "allowed_email_domains", "client.org")
        .await;
    let ctx = Arc::new(TenantContext::<sqlx::Sqlite> {
        pools: env._pools.clone(),
        resolver: ChainResolver::new().push(SubdomainResolver::new("app.test")),
        session_secret: env.secret.clone(),
        operator_secret: env.secret.clone(),
    });
    let app = member_sso_router(MemberAuthConfig::default()).layer(axum::Extension(ctx));
    let page = |sub: &'static str, email: &'static str| {
        let app = app.clone();
        let env = &env;
        async move {
            env.idp.assert(sub, email);
            let resp = handshake(&app, &env.tenants[0].host, "/auth", "corp").await;
            let status = resp.status();
            let headers: Vec<(String, String)> = resp
                .headers()
                .iter()
                .filter(|(k, _)| *k != header::DATE)
                .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_owned()))
                .collect();
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            (status, headers, body)
        }
    };
    let no_account = page("sub-x", "nobody@client.org").await;
    let refused = page("sub-y", "eve@gmail.com").await;
    assert_eq!(refused, no_account);
    assert!(!String::from_utf8_lossy(&refused.2).contains("domain"));
}

/// The bare admin refuses a linked admin outside the allowed domains.
#[tokio::test]
async fn bare_admin_domain_rule_refuses_a_linked_admin() {
    use rustango::admin::AdminUser;
    let _g = SUITE.lock().await;
    let (app, pool, idp) = bare_admin().await;
    let root = AdminUser::objects()
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
    idp.assert("sub-root", "root@example.com");
    assert!(signed_in(
        &handshake(&app, "admin.test", "/login", "corp").await
    ));
    set_policy(&pool, "corp", "allowed_email_domains", "client.org").await;
    let resp = handshake(&app, "admin.test", "/login", "corp").await;
    assert!(!signed_in(&resp));
    assert!(
        location(&resp).ends_with("/login?sso_error=nouser"),
        "{}",
        location(&resp)
    );
}

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

    set_number(&pool, "corp", "session_ttl_secs", 120).await;
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
    let done = submit(code, pending).await;
    assert!(signed_in(&done), "right code signs in");
    // The provider's lifetime survives the code step.
    let left = admin_session_left(&done);
    assert!((110..=120).contains(&left), "{left}");
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
// The legacy helper, kept until it is removed.
#[allow(deprecated)]
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
    // #2560: the default refuses an unknown email.
    let strict = member_sso_router(MemberAuthConfig::default()).layer(axum::Extension(ctx.clone()));
    env.idp.assert("sub-stranger", "stranger@example.com");
    let resp = handshake(&strict, &env.tenants[0].host, "/auth", "corp").await;
    assert!(
        !set_cookies(&resp)
            .iter()
            .any(|c| c.starts_with(&format!("{MEMBER_COOKIE}="))),
        "default config provisioned a stranger"
    );
    let app = member_sso_router(MemberAuthConfig {
        auto_provision: true,
        ..MemberAuthConfig::default()
    })
    .layer(axum::Extension(ctx));
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
    let app = member_sso_router_for::<sqlx::Sqlite>(MemberAuthConfig {
        auto_provision: true,
        ..MemberAuthConfig::default()
    })
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

/// #2559 — every SSO surface fires `user_logged_in` and `user_login_failed`,
/// naming the tenant (`-` for none).
#[tokio::test]
async fn sso_sign_ins_fire_the_auth_signals() {
    use rustango::extractors::TenantContext;
    use rustango::signals::auth as sig;
    use rustango::tenancy::member_auth::{member_sso_router_for, MemberAuthConfig};
    let _g = SUITE.lock().await;
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let (s1, s2) = (seen.clone(), seen.clone());
    let in_id = sig::connect_user_logged_in(move |c| {
        s1.lock().unwrap().push(format!(
            "in:{}:{}:{}",
            c.source,
            c.username,
            c.tenant.as_deref().unwrap_or("-")
        ));
        async {}
    });
    let fail_id = sig::connect_user_login_failed(move |c| {
        s2.lock().unwrap().push(format!(
            "failed:{}:{:?}:{}:{}",
            c.source,
            c.reason,
            c.attempted_username.unwrap_or_default(),
            c.tenant.as_deref().unwrap_or("-")
        ));
        async {}
    });
    let take = || std::mem::take(&mut *seen.lock().unwrap());

    // Bare admin: refused without a link, signed in with one.
    let (app, pool, idp) = bare_admin().await;
    idp.assert("sub-root", "root@example.com");
    handshake(&app, "admin.test", "/login", "corp").await;
    assert_eq!(
        take(),
        ["failed:admin_sso:InvalidCredentials:root@example.com:-"]
    );
    let key = rustango::sso::resolve_by_slug(&pool, "corp", String::new())
        .await
        .unwrap()
        .unwrap()
        .key(LinkSource::Admin);
    let root = rustango::admin::AdminUser::objects()
        .filter("username", "root")
        .fetch(&pool)
        .await
        .unwrap()
        .remove(0);
    rustango::sso::link::create_link(&pool, &key, "sub-root", root.id.get().copied().unwrap())
        .await
        .unwrap();
    handshake(&app, "admin.test", "/login", "corp").await;
    assert_eq!(take(), ["in:admin_sso:root:-"]);

    // Tenant console.
    let env = boot().await;
    let ann = env.user("ann", "ann@example.com", false).await;
    env.tenant_provider("corp", false).await;
    assert!(env.sso("corp", "sub-ann", "ann@example.com").await.is_err());
    assert_eq!(
        take(),
        ["failed:tenant_admin_sso:InvalidCredentials:ann@example.com:acme"]
    );
    env.tenant_provider("open", true).await;
    assert!(env.sso("open", "sub-ann", "ann@example.com").await.is_ok());
    assert_eq!(take(), ["in:tenant_admin_sso:ann:acme"]);
    // An unverified email is never named; the subject is.
    *env.idp.claims.lock().unwrap() =
        serde_json::json!({"sub": "sub-x", "email": "x@example.com", "email_verified": false});
    handshake(&env.admin, &env.tenants[0].host, "/__login", "open").await;
    assert_eq!(
        take(),
        ["failed:tenant_admin_sso:InvalidCredentials:sub-x:acme"]
    );
    env.deactivate(ann).await;
    assert!(env.sso("open", "sub-ann", "ann@example.com").await.is_err());
    assert_eq!(
        take(),
        ["failed:tenant_admin_sso:Inactive:ann@example.com:acme"]
    );

    // Member SSO: the default refuses a stranger; provisioning signs in.
    let ctx = Arc::new(TenantContext::<sqlx::Sqlite> {
        pools: env._pools.clone(),
        resolver: ChainResolver::new().push(SubdomainResolver::new("app.test")),
        session_secret: env.secret.clone(),
        operator_secret: env.secret.clone(),
    });
    let host = env.tenants[0].host.clone();
    let strict = member_sso_router_for::<sqlx::Sqlite>(MemberAuthConfig::default())
        .layer(axum::Extension(ctx.clone()));
    env.idp.assert("sub-new", "new@example.com");
    handshake(&strict, &host, "/auth", "corp").await;
    assert_eq!(
        take(),
        ["failed:member_sso:InvalidCredentials:new@example.com:acme"]
    );
    let open = member_sso_router_for::<sqlx::Sqlite>(MemberAuthConfig {
        auto_provision: true,
        ..MemberAuthConfig::default()
    })
    .layer(axum::Extension(ctx));
    handshake(&open, &host, "/auth", "corp").await;
    assert_eq!(take(), ["in:member_sso:new:acme"]);

    sig::disconnect_user_logged_in(in_id);
    sig::disconnect_user_login_failed(fail_id);
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
    // The strip above also took the tenant override's column of that name.
    rustango::sql::raw_execute_pool(
        &pool,
        "ALTER TABLE rustango_sso_shared_overrides DROP COLUMN allow_email_link",
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
                    .contains("\"allow_email_link\"")
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

/// A failed member sign-in says so in its status and is never cached.
#[tokio::test]
async fn member_sso_failures_have_an_error_status_and_no_store() {
    use rustango::extractors::TenantContext;
    use rustango::tenancy::member_auth::{member_sso_router_for, MemberAuthConfig};
    let _g = SUITE.lock().await;
    let env = boot().await;
    env.tenant_provider("corp", false).await;
    let ctx = Arc::new(TenantContext::<sqlx::Sqlite> {
        pools: env._pools.clone(),
        resolver: ChainResolver::new().push(SubdomainResolver::new("app.test")),
        session_secret: env.secret.clone(),
        operator_secret: env.secret.clone(),
    });
    let app = member_sso_router_for::<sqlx::Sqlite>(MemberAuthConfig::default())
        .layer(axum::Extension(ctx));
    let host = env.tenants[0].host.clone();
    let get = |uri: &str| {
        Request::builder()
            .uri(uri)
            .header(header::HOST, &host)
            .body(Body::empty())
            .unwrap()
    };
    env.idp.assert("sub-stranger", "stranger@example.com");
    let refused = handshake(&app, &host, "/auth", "corp").await;
    for (what, resp, want) in [
        (
            "unknown provider",
            send(&app, get("/auth/sso/nope")).await,
            404,
        ),
        (
            "no flow cookie",
            send(&app, get("/auth/sso/corp/callback?code=c&state=s")).await,
            400,
        ),
        (
            "IdP error",
            send(&app, get("/auth/sso/corp/callback?error=access_denied")).await,
            403,
        ),
        ("not linked", refused, 403),
    ] {
        assert_eq!(resp.status().as_u16(), want, "{what}");
        assert_eq!(resp.headers()[header::CACHE_CONTROL], "no-store", "{what}");
    }
}

/// A tenant settings row in tenant 0.
async fn tenant_settings(env: &Env, password_login: &str, default_provider: Option<&str>) {
    use rustango::sso::policy::SsoTenantPolicy;
    rustango::testkit::create_tables_for::<SsoTenantPolicy>(env.pool())
        .await
        .unwrap();
    SsoTenantPolicy {
        id: Auto::default(),
        name: "default".into(),
        access_denied_message: None,
        password_login: password_login.into(),
        sso_only: false,
        default_provider: default_provider.map(str::to_owned),
        signups_per_hour: None,
    }
    .insert_pool(env.pool())
    .await
    .unwrap();
}

impl Env {
    /// A tenant 0 user with a real password.
    async fn password_user(&self, name: &str, superuser: bool) -> i64 {
        let mut u = User {
            username: name.into(),
            password_hash: rustango::tenancy::password::hash("pw-123456789").unwrap(),
            email: Some(format!("{name}@example.com")),
            is_superuser: superuser,
            ..rustango::testkit::user()
        };
        u.insert_pool(self.pool()).await.unwrap();
        u.id.get().copied().unwrap()
    }

    /// GET a tenant 0 page.
    async fn get(&self, uri: &str) -> axum::response::Response {
        send(
            &self.admin,
            Request::builder()
                .uri(uri)
                .header(header::HOST, &self.tenants[0].host)
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    /// Whether `name` signs in with the tenant 0 password form.
    async fn password_login(&self, name: &str) -> bool {
        let resp = send(
            &self.admin,
            Request::builder()
                .method("POST")
                .uri("/__login")
                .header(header::HOST, &self.tenants[0].host)
                .header(header::COOKIE, "rustango_csrf=t")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "_csrf=t&username={name}&password=pw-123456789"
                )))
                .unwrap(),
        )
        .await;
        set_cookies(&resp)
            .iter()
            .any(|c| c.starts_with(&format!("{COOKIE_NAME}=")))
    }
}

async fn body_of(resp: axum::response::Response) -> String {
    let b = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(b.to_vec()).unwrap()
}

/// `superusers_only` hides the password form unless `?local=1`, and only a
/// superuser signs in with it: the break-glass way in.
#[tokio::test]
async fn superusers_only_is_a_break_glass_password_form() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    env.password_user("boss", true).await;
    env.password_user("ann", false).await;
    assert!(env.password_login("ann").await, "password_login on");
    tenant_settings(&env, "superusers_only", None).await;
    let page = body_of(env.get("/__login").await).await;
    assert!(!page.contains("name=\"password\""), "{page}");
    let local = body_of(env.get("/__login?local=1").await).await;
    assert!(local.contains("name=\"password\""), "{local}");
    assert!(env.password_login("boss").await, "break-glass");
    assert!(!env.password_login("ann").await);
}

/// The login page goes straight to the default provider, but not with
/// `?local=1` nor after a refusal: no redirect loop.
#[tokio::test]
async fn the_login_page_redirects_to_the_default_provider_once() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    env.tenant_provider("corp", false).await;
    tenant_settings(&env, "on", Some("corp")).await;
    let resp = env.get("/__login").await;
    assert!(resp.status().is_redirection());
    assert_eq!(location(&resp), "/__login/sso/corp");
    // A refused sign-in lands on the page, which stays put.
    env.idp.assert("sub-stranger", "stranger@example.com");
    let refused = handshake(&env.admin, &env.tenants[0].host, "/__login", "corp").await;
    let back = location(&refused);
    assert_eq!(back, "/__login?sso_error=nouser");
    assert_eq!(env.get(&back).await.status(), StatusCode::OK);
    assert_eq!(env.get("/__login?local=1").await.status(), StatusCode::OK);
}

/// A disabled tenant provider shadows the shared one with its slug: no
/// button, and its sign-in is refused instead of falling through.
#[tokio::test]
async fn a_disabled_tenant_provider_never_falls_through_to_the_shared_one() {
    use rustango::sql::UpdaterPool as _;
    let _g = SUITE.lock().await;
    let env = boot().await;
    let ann = env.user("ann", "ann@example.com", false).await;
    env.shared_provider("corp", true).await;
    assert_eq!(env.sso("corp", "sub-ann", "ann@example.com").await, Ok(ann));
    env.tenant_provider("corp", true).await;
    SsoProvider::objects()
        .filter("slug", "corp")
        .update()
        .set("enabled", false)
        .execute_pool(env.pool())
        .await
        .unwrap();
    // Refused at the start: never sent to the shared provider's IdP.
    assert_eq!(
        location(&env.get("/__login/sso/corp").await),
        "/__login?sso_error=disabled"
    );
    let page = body_of(env.get("/__login").await).await;
    assert!(!page.contains("/__login/sso/corp"), "{page}");
}

/// Refused SSO sign-ins spend their own per-IP bucket: past it the SSO
/// start answers 429, and the password form from that IP still works. A
/// subject's refusals are audited a few times, not every time.
#[tokio::test]
async fn sso_refusals_are_throttled_apart_from_passwords() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    rustango::audit::ensure_table_pool(env.pool())
        .await
        .unwrap();
    env.password_user("ann", false).await;
    env.tenant_provider("corp", false).await;
    env.idp.assert("sub-eve", "eve@example.com");
    let ip = "10.250.7.9";
    let limit = rustango::login_throttle::shared().limits().ip_limit;
    for _ in 0..limit {
        let r = handshake_from(
            &env.admin,
            &env.tenants[0].host,
            "/__login",
            "corp",
            Some(ip),
        )
        .await;
        assert_eq!(location(&r), "/__login?sso_error=nouser");
    }
    let begin = send(
        &env.admin,
        from_ip(
            Request::builder()
                .uri("/__login/sso/corp")
                .header(header::HOST, &env.tenants[0].host)
                .body(Body::empty())
                .unwrap(),
            Some(ip),
        ),
    )
    .await;
    assert_eq!(begin.status(), StatusCode::TOO_MANY_REQUESTS);
    let login = send(
        &env.admin,
        from_ip(
            Request::builder()
                .method("POST")
                .uri("/__login")
                .header(header::HOST, &env.tenants[0].host)
                .header(header::COOKIE, "rustango_csrf=t")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from("_csrf=t&username=ann&password=pw-123456789"))
                .unwrap(),
            Some(ip),
        ),
    )
    .await;
    assert!(
        set_cookies(&login)
            .iter()
            .any(|c| c.starts_with(&format!("{COOKIE_NAME}="))),
        "the password bucket is untouched"
    );
    let rows = refused_rules(env.pool(), "corp", "sub-eve").await;
    assert_eq!(rows.len(), 5, "{rows:?}");
}

/// The tenant session lasts the shortest of the route, the provider and,
/// for a superuser, the superuser lifetime.
#[tokio::test]
async fn the_tenant_session_ttl_is_the_shortest() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    let ann = env.user("ann", "ann@example.com", false).await;
    env.tenant_provider("corp", true).await;
    set_number(env.pool(), "corp", "session_ttl_secs", 600).await;
    set_number(env.pool(), "corp", "superuser_session_ttl_secs", 300).await;
    env.idp.assert("sub-ann", "ann@example.com");
    let resp = handshake(&env.admin, &env.tenants[0].host, "/__login", "corp").await;
    let cookie = resp
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|c| c.starts_with(&format!("{COOKIE_NAME}=")))
        .unwrap_or_else(|| panic!("no session: {}", location(&resp)))
        .to_owned();
    assert!(cookie.contains("Max-Age=600"), "{cookie}");
    let value = cookie
        .split(';')
        .next()
        .unwrap()
        .trim_start_matches(&format!("{COOKIE_NAME}="));
    let p = decode(&env.secret, &env.tenants[0].slug, value).unwrap();
    assert_eq!(p.uid, ann);
    let left = p.exp - chrono::Utc::now().timestamp();
    assert!((590..=600).contains(&left), "{left}");
}

/// The member session is capped by the provider's lifetime too.
#[cfg(not(feature = "postgres"))]
#[tokio::test]
async fn the_member_session_ttl_is_the_shortest() {
    use rustango::extractors::TenantContext;
    use rustango::tenancy::{member_sso_router, MemberAuthConfig, MEMBER_COOKIE};
    let _g = SUITE.lock().await;
    let env = boot().await;
    env.tenant_provider("corp", false).await;
    set_number(env.pool(), "corp", "session_ttl_secs", 900).await;
    let ctx = Arc::new(TenantContext::<sqlx::Sqlite> {
        pools: env._pools.clone(),
        resolver: ChainResolver::new().push(SubdomainResolver::new("app.test")),
        session_secret: env.secret.clone(),
        operator_secret: env.secret.clone(),
    });
    let app = member_sso_router(MemberAuthConfig {
        auto_provision: true,
        ..MemberAuthConfig::default()
    })
    .layer(axum::Extension(ctx));
    env.idp.assert("sub-new", "new@example.com");
    let resp = handshake(&app, &env.tenants[0].host, "/auth", "corp").await;
    let cookie = resp
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|c| c.starts_with(&format!("{MEMBER_COOKIE}=")))
        .expect("member cookie")
        .to_owned();
    assert!(cookie.contains("Max-Age=900"), "{cookie}");
}

/// The bare admin session is capped by the provider's lifetime.
#[tokio::test]
async fn the_bare_admin_session_ttl_is_the_shortest() {
    let _g = SUITE.lock().await;
    let (app, pool, idp) = bare_admin().await;
    set_number(&pool, "corp", "session_ttl_secs", 120).await;
    let key = rustango::sso::resolve_by_slug(&pool, "corp", String::new())
        .await
        .unwrap()
        .unwrap()
        .key(LinkSource::Admin);
    let root = rustango::admin::AdminUser::objects()
        .filter("username", "root")
        .fetch(&pool)
        .await
        .unwrap()
        .remove(0);
    rustango::sso::link::create_link(&pool, &key, "sub-root", root.id.get().copied().unwrap())
        .await
        .unwrap();
    idp.assert("sub-root", "root@example.com");
    let resp = handshake(&app, "admin.test", "/login", "corp").await;
    assert!(signed_in(&resp), "{}", location(&resp));
    let left = admin_session_left(&resp);
    assert!((110..=120).contains(&left), "{left}");
}

/// Seconds the bare admin session set on `resp` has left.
fn admin_session_left(resp: &axum::response::Response) -> i64 {
    use base64::Engine as _;
    let value = set_cookies(resp)
        .into_iter()
        .find_map(|c| c.strip_prefix("rustango_admin_session=").map(str::to_owned))
        .expect("admin session");
    let body = value.split('.').next().unwrap();
    let json: serde_json::Value = serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(body)
            .unwrap(),
    )
    .unwrap();
    json["exp"].as_i64().unwrap() - chrono::Utc::now().timestamp()
}

/// Logout lands on `?local=1`, so a live IdP session does not sign the
/// user straight back in; and a second visit within a minute of an
/// automatic redirect shows the page, so a session that does not stick
/// cannot loop.
#[tokio::test]
async fn logout_and_a_recent_redirect_do_not_bounce_to_the_idp() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    env.tenant_provider("corp", false).await;
    tenant_settings(&env, "on", Some("corp")).await;
    let out = send(
        &env.admin,
        Request::builder()
            .method("POST")
            .uri("/__logout")
            .header(header::HOST, &env.tenants[0].host)
            .header(header::COOKIE, "rustango_csrf=t")
            .header("x-csrf-token", "t")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from("_csrf=t"))
            .unwrap(),
    )
    .await;
    let to = location(&out);
    assert_eq!(to, "/__login?local=1");
    assert_eq!(env.get(&to).await.status(), StatusCode::OK);
    let first = env.get("/__login").await;
    assert_eq!(location(&first), "/__login/sso/corp");
    let marker = set_cookies(&first)
        .into_iter()
        .find(|c| c.starts_with("rustango_sso_auto="))
        .expect("marker cookie");
    let again = send(
        &env.admin,
        Request::builder()
            .uri("/__login")
            .header(header::HOST, &env.tenants[0].host)
            .header(header::COOKIE, marker)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(again.status(), StatusCode::OK, "{}", location(&again));
}

/// A password change re-mints the session with its remaining lifetime,
/// not a fresh full one.
#[tokio::test]
async fn a_password_change_keeps_the_session_expiry() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    let uid = env.password_user("ann", false).await;
    let hash = User::objects()
        .filter("id", uid)
        .fetch(env.pool())
        .await
        .unwrap()
        .remove(0)
        .password_hash;
    let payload = TenantSessionPayload::new(
        uid,
        &env.tenants[0].slug,
        600,
        PasswordFingerprint::of(&env.secret, &hash),
    );
    let resp = send(
        &env.admin,
        Request::builder()
            .method("POST")
            .uri("/__change-password")
            .header(header::HOST, &env.tenants[0].host)
            .header(
                header::COOKIE,
                format!("{COOKIE_NAME}={}; rustango_csrf=t", encode(&env.secret, &payload)),
            )
            .header("x-csrf-token", "t")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(
                "_csrf=t&current_password=pw-123456789&new_password=Another-strong-pass-9!&confirm_password=Another-strong-pass-9!",
            ))
            .unwrap(),
    )
    .await;
    let value = set_cookies(&resp)
        .into_iter()
        .find_map(|c| {
            c.strip_prefix(&format!("{COOKIE_NAME}="))
                .map(str::to_owned)
        })
        .unwrap_or_else(|| panic!("no new session: {}", location(&resp)));
    let p = decode(&env.secret, &env.tenants[0].slug, &value).unwrap();
    let left = p.exp - chrono::Utc::now().timestamp();
    assert!((1..=600).contains(&left), "{left}");
}

/// A bare-admin user linked through an `sso_only` provider is refused a
/// password; a superuser is not.
#[tokio::test]
async fn bare_admin_password_login_honours_sso_only() {
    use rustango::admin::AdminUser;
    let _g = SUITE.lock().await;
    let (app, pool, _idp) = bare_admin().await;
    let mut ed = AdminUser::new_with_password("ed", "pw-123456789", false).unwrap();
    ed.insert_pool(&pool).await.unwrap();
    let key = rustango::sso::resolve_by_slug(&pool, "corp", String::new())
        .await
        .unwrap()
        .unwrap()
        .key(LinkSource::Admin);
    let root = AdminUser::objects()
        .filter("username", "root")
        .fetch(&pool)
        .await
        .unwrap()
        .remove(0);
    for (sub, u) in [("sub-ed", &ed), ("sub-root", &root)] {
        rustango::sso::link::create_link(&pool, &key, sub, u.id.get().copied().unwrap())
            .await
            .unwrap();
    }
    let login = |name: &'static str| {
        let app = app.clone();
        async move {
            let resp = send(
                &app,
                Request::builder()
                    .method("POST")
                    .uri("/login")
                    .header(header::HOST, "admin.test")
                    .header(header::COOKIE, "rustango_csrf=t")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "_csrf=t&username={name}&password=pw-123456789"
                    )))
                    .unwrap(),
            )
            .await;
            set_cookies(&resp)
                .iter()
                .any(|c| c.starts_with("rustango_admin_session="))
        }
    };
    assert!(login("ed").await, "before sso_only");
    {
        use rustango::sql::UpdaterPool as _;
        SsoProvider::objects()
            .filter("slug", "corp")
            .update()
            .set("sso_only", true)
            .execute_pool(&pool)
            .await
            .unwrap();
    }
    assert!(!login("ed").await, "sso_only refuses the password");
    assert!(login("root").await, "a superuser is never locked out");
}

/// Full `Set-Cookie` values (attributes kept).
fn raw_cookies(resp: &axum::response::Response) -> Vec<String> {
    resp.headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(str::to_owned)
        .collect()
}

/// The `Set-Cookie` value for cookie `name`.
fn cookie_named(resp: &axum::response::Response, name: &str) -> String {
    raw_cookies(resp)
        .into_iter()
        .find(|c| c.starts_with(&format!("{name}=")) && !c.starts_with(&format!("{name}=;")))
        .unwrap_or_else(|| panic!("no {name} cookie in {:?}", raw_cookies(resp)))
}

impl Env {
    /// Tenant 0 (`acme`) reachable on its own host and as `app.test/acme`,
    /// with a tenant admin that resolves both.
    async fn both_ways(&self) -> Router {
        use rustango::tenancy::PathPrefixResolver;
        let registry = self._pools.registry_pool();
        let mut org = Org::objects()
            .filter("slug", "acme")
            .fetch(&registry)
            .await
            .unwrap()
            .remove(0);
        org.path_prefix = Some("/acme".into());
        org.save_pool(&registry).await.unwrap();
        rustango::testkit::reset_org_cache();
        let reg_url = format!(
            "sqlite://{}?mode=rwc",
            self._dir.path().join("reg.db").display()
        );
        TenantAdminBuilder::new(
            self._pools.clone(),
            reg_url,
            ChainResolver::new()
                .push(SubdomainResolver::new("app.test"))
                .push(PathPrefixResolver),
        )
        .routes(RouteConfig::legacy())
        .with_session(self.secret.clone())
        .build()
    }
}

/// One tenant on its own host and under its prefix on the apex: each
/// handshake keeps its own callback URL and cookie path, and both sign in
/// the same user through the same link.
#[tokio::test]
async fn tenant_console_sso_works_on_its_host_and_under_its_prefix() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    let ann = env.user("ann", "ann@example.com", false).await;
    env.tenant_provider("corp", true).await;
    let app = env.both_ways().await;
    env.idp.assert("sub-ann", "ann@example.com");
    for (host, login, path, callback) in [
        ("acme.app.test", "/__login", "/", "acme.app.test%2F__login"),
        (
            "app.test",
            "/acme/__login",
            "/acme",
            "app.test%2Facme%2F__login",
        ),
    ] {
        let begin = send(
            &app,
            Request::builder()
                .uri(format!("{login}/sso/corp"))
                .header(header::HOST, host)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let to = location(&begin);
        assert!(
            to.contains(&format!(
                "redirect_uri=https%3A%2F%2F{callback}%2Fsso%2Fcorp%2Fcallback"
            )),
            "{host}: {to}"
        );
        let flow = cookie_named(&begin, "rustango_admin_sso_flow");
        assert!(flow.contains(&format!("; Path={path};")), "{flow}");
        let resp = handshake(&app, host, login, "corp").await;
        let session = cookie_named(&resp, COOKIE_NAME);
        assert!(session.contains(&format!("; Path={path};")), "{session}");
        let value = session
            .split(';')
            .next()
            .unwrap()
            .split_once('=')
            .unwrap()
            .1
            .to_owned();
        assert_eq!(
            decode(&env.secret, "acme", &value).unwrap().uid,
            ann,
            "{host}"
        );
    }
    let links = rustango::sso::SsoLink::objects()
        .filter("subject", "sub-ann")
        .fetch(env.pool())
        .await
        .unwrap();
    assert_eq!(links.len(), 1, "one link for both ways in");
}

/// Under a prefix the automatic redirect, its marker cookie, the logout
/// landing and `?local=1` all keep the prefix.
#[tokio::test]
async fn sso_login_page_pieces_keep_the_prefix() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    env.tenant_provider("corp", false).await;
    env.password_user("boss", true).await;
    tenant_settings(&env, "superusers_only", Some("corp")).await;
    let app = env.both_ways().await;
    let get = |uri: &str| {
        send(
            &app,
            Request::builder()
                .uri(uri)
                .header(header::HOST, "app.test")
                .body(Body::empty())
                .unwrap(),
        )
    };
    let first = get("/acme/__login").await;
    assert_eq!(location(&first), "/acme/__login/sso/corp");
    let marker = cookie_named(&first, "rustango_sso_auto");
    assert!(marker.contains("Path=/acme;"), "{marker}");
    let local = body_of(get("/acme/__login?local=1").await).await;
    assert!(
        local.contains("name=\"password\""),
        "break-glass form: {local}"
    );
    let out = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/acme/__logout")
            .header(header::HOST, "app.test")
            .header(header::COOKIE, "rustango_csrf=t")
            .header("x-csrf-token", "t")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from("_csrf=t"))
            .unwrap(),
    )
    .await;
    assert_eq!(location(&out), "/acme/__login?local=1");
}

/// A callback completes once, on either way in, and a signed id_token must
/// carry this sign-in's nonce.
#[tokio::test]
async fn a_flow_completes_once_and_needs_its_nonce() {
    let _g = SUITE.lock().await;
    let env = boot().await;
    rustango::audit::ensure_table_pool(env.pool())
        .await
        .unwrap();
    let ann = env.user("ann", "ann@example.com", false).await;
    env.tenant_provider("corp", true).await;
    let app = env.both_ways().await;
    env.idp.assert("sub-ann", "ann@example.com");
    env.idp.id_token(Some((serde_json::json!({}), true)));
    for (host, login) in [("acme.app.test", "/__login"), ("app.test", "/acme/__login")] {
        let begin = send(
            &app,
            Request::builder()
                .uri(format!("{login}/sso/corp"))
                .header(header::HOST, host)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let to = location(&begin);
        note_nonce(&to);
        let state = to
            .split(['?', '&'])
            .find_map(|kv| kv.strip_prefix("state="))
            .unwrap()
            .to_owned();
        let flow = set_cookies(&begin).join("; ");
        let callback = || {
            send(
                &app,
                Request::builder()
                    .uri(format!("{login}/sso/corp/callback?code=c&state={state}"))
                    .header(header::HOST, host)
                    .header(header::COOKIE, &flow)
                    .body(Body::empty())
                    .unwrap(),
            )
        };
        let first = callback().await;
        assert!(
            set_cookies(&first)
                .iter()
                .any(|c| c.starts_with(&format!("{COOKIE_NAME}="))),
            "{host}: {}",
            location(&first)
        );
        let again = callback().await;
        assert!(
            location(&again).ends_with("sso_error=expired"),
            "{host} replay: {}",
            location(&again)
        );
    }
    // Another sign-in's nonce in a genuine token refuses.
    env.idp
        .id_token(Some((serde_json::json!({"nonce": "not-this-one"}), true)));
    assert_eq!(
        env.sso("corp", "sub-ann", "ann@example.com").await,
        Err("nouser".into())
    );
    assert_eq!(
        refused_rules(env.pool(), "corp", "sub-ann").await,
        ["id_token_replayed"]
    );
    env.idp.id_token(Some((serde_json::json!({}), true)));
    assert_eq!(env.sso("corp", "sub-ann", "ann@example.com").await, Ok(ann));
}

/// The member landing comes from the `?next` sealed at begin; a `?next`
/// on the callback is ignored.
#[tokio::test]
async fn the_member_landing_is_sealed_at_begin() {
    use rustango::extractors::TenantContext;
    use rustango::tenancy::member_auth::{member_sso_router_for, MemberAuthConfig};
    let _g = SUITE.lock().await;
    let env = boot().await;
    env.tenant_provider("corp", false).await;
    let ctx = Arc::new(TenantContext::<sqlx::Sqlite> {
        pools: env._pools.clone(),
        resolver: ChainResolver::new().push(SubdomainResolver::new("app.test")),
        session_secret: env.secret.clone(),
        operator_secret: env.secret.clone(),
    });
    let app = member_sso_router_for::<sqlx::Sqlite>(MemberAuthConfig {
        auto_provision: true,
        ..MemberAuthConfig::default()
    })
    .layer(axum::Extension(ctx));
    env.idp.assert("sub-m", "m@example.com");
    let host = &env.tenants[0].host;
    let run = |begin_next: &'static str, cb_next: &'static str| {
        let app = app.clone();
        let host = host.clone();
        async move {
            let begin = send(
                &app,
                Request::builder()
                    .uri(format!("/auth/sso/corp{begin_next}"))
                    .header(header::HOST, &host)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            let to = location(&begin);
            note_nonce(&to);
            let state = to
                .split(['?', '&'])
                .find_map(|kv| kv.strip_prefix("state="))
                .unwrap()
                .to_owned();
            let resp = send(
                &app,
                Request::builder()
                    .uri(format!(
                        "/auth/sso/corp/callback?code=c&state={state}{cb_next}"
                    ))
                    .header(header::HOST, &host)
                    .header(header::COOKIE, set_cookies(&begin).join("; "))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            location(&resp)
        }
    };
    assert_eq!(run("?next=/dash", "&next=/elsewhere").await, "/dash");
    assert_eq!(run("", "&next=/elsewhere").await, "/");
    assert_eq!(run("?next=//evil.example", "").await, "/");
}

/// Begin once, then send the same callback twice: both responses.
async fn replayed(
    app: &Router,
    host: &str,
    login: &str,
    slug: &str,
) -> (axum::response::Response, axum::response::Response) {
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
    note_nonce(&to);
    let state = to
        .split(['?', '&'])
        .find_map(|kv| kv.strip_prefix("state="))
        .unwrap_or_else(|| panic!("{host}{login}: begin got {} {to:?}", begin.status()))
        .to_owned();
    let flow = set_cookies(&begin).join("; ");
    let callback = || {
        send(
            app,
            Request::builder()
                .uri(format!("{login}/sso/{slug}/callback?code=c&state={state}"))
                .header(header::HOST, host)
                .header(header::COOKIE, &flow)
                .body(Body::empty())
                .unwrap(),
        )
    };
    let first = callback().await;
    (first, callback().await)
}

/// The member and bare-admin callbacks also complete a flow only once.
#[tokio::test]
async fn a_replayed_callback_is_refused_on_member_and_bare_admin() {
    use rustango::extractors::TenantContext;
    use rustango::tenancy::member_auth::member_sso_router_for;
    use rustango::tenancy::{MemberAuthConfig, MEMBER_COOKIE};
    let _g = SUITE.lock().await;
    let env = boot().await;
    env.tenant_provider("corp", false).await;
    let ctx = Arc::new(TenantContext::<sqlx::Sqlite> {
        pools: env._pools.clone(),
        resolver: ChainResolver::new().push(SubdomainResolver::new("app.test")),
        session_secret: env.secret.clone(),
        operator_secret: env.secret.clone(),
    });
    let members = member_sso_router_for::<sqlx::Sqlite>(MemberAuthConfig {
        auto_provision: true,
        ..MemberAuthConfig::default()
    })
    .layer(axum::Extension(ctx));
    env.idp.assert("sub-m", "m@example.com");
    let member_session = |r: &axum::response::Response| {
        set_cookies(r)
            .iter()
            .any(|c| c.starts_with(&format!("{MEMBER_COOKIE}=")))
    };
    let (first, again) = replayed(&members, &env.tenants[0].host, "/auth", "corp").await;
    assert!(member_session(&first), "member: {}", location(&first));
    assert!(!member_session(&again), "member replay signed in");

    let (admin, pool, idp) = bare_admin().await;
    let key = rustango::sso::resolve_by_slug(&pool, "corp", String::new())
        .await
        .unwrap()
        .unwrap()
        .key(LinkSource::Admin);
    let root = rustango::admin::AdminUser::objects()
        .filter("username", "root")
        .fetch(&pool)
        .await
        .unwrap()
        .remove(0);
    rustango::sso::link::create_link(&pool, &key, "sub-root", root.id.get().copied().unwrap())
        .await
        .unwrap();
    idp.assert("sub-root", "root@example.com");
    let (first, again) = replayed(&admin, "admin.test", "/login", "corp").await;
    assert!(signed_in(&first), "bare admin: {}", location(&first));
    assert!(!signed_in(&again), "bare admin replay signed in");
    assert!(
        location(&again).ends_with("sso_error=expired"),
        "{}",
        location(&again)
    );
}

/// Sets an env var for one test and puts the old value back (tests hold `SUITE`).
struct EnvVar(&'static str, Option<String>);

impl EnvVar {
    fn set(name: &'static str, value: &str) -> Self {
        let old = std::env::var(name).ok();
        std::env::set_var(name, value);
        Self(name, old)
    }
}

impl Drop for EnvVar {
    fn drop(&mut self) {
        match &self.1 {
            Some(v) => std::env::set_var(self.0, v),
            None => std::env::remove_var(self.0),
        }
    }
}

/// With Secure cookies the tenant flow cookie is `__Host-` on the tenant's
/// own host and keeps its plain name under the prefix; each signs in, and
/// the own host reads only the `__Host-` name.
#[tokio::test]
async fn secure_flow_cookies_take_the_host_prefix_only_on_the_own_host() {
    let _g = SUITE.lock().await;
    // Secure cookies come from the prod tier when nothing set them at boot.
    let _prod = EnvVar::set("RUSTANGO_ENV", "prod");
    assert!(rustango::session::secure_cookies(), "secure cookies are on");
    let env = boot().await;
    let ann = env.user("ann", "ann@example.com", false).await;
    env.tenant_provider("corp", true).await;
    let app = env.both_ways().await;
    env.idp.assert("sub-ann", "ann@example.com");
    for (host, login, name) in [
        (
            "acme.app.test",
            "/__login",
            "__Host-rustango_admin_sso_flow",
        ),
        ("app.test", "/acme/__login", "rustango_admin_sso_flow"),
    ] {
        let begin = send(
            &app,
            Request::builder()
                .uri(format!("{login}/sso/corp"))
                .header(header::HOST, host)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let flow = cookie_named(&begin, name);
        assert!(flow.contains("; Secure"), "{flow}");
        assert_eq!(
            set_cookies(&begin)
                .iter()
                .filter(|c| c.contains("sso_flow="))
                .count(),
            1,
            "{host}: one flow cookie"
        );
        let resp = handshake(&app, host, login, "corp").await;
        let session = cookie_named(&resp, COOKIE_NAME);
        let value = session
            .split(';')
            .next()
            .unwrap()
            .split_once('=')
            .unwrap()
            .1
            .to_owned();
        assert_eq!(
            decode(&env.secret, "acme", &value).unwrap().uid,
            ann,
            "{host}"
        );
    }
    // On the own host a flow under the plain name is not read.
    let begin = send(
        &app,
        Request::builder()
            .uri("/__login/sso/corp")
            .header(header::HOST, "acme.app.test")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let to = location(&begin);
    note_nonce(&to);
    let state = to
        .split(['?', '&'])
        .find_map(|kv| kv.strip_prefix("state="))
        .unwrap()
        .to_owned();
    let planted = set_cookies(&begin).join("; ").replace(
        "__Host-rustango_admin_sso_flow=",
        "rustango_admin_sso_flow=",
    );
    let resp = send(
        &app,
        Request::builder()
            .uri(format!("/__login/sso/corp/callback?code=c&state={state}"))
            .header(header::HOST, "acme.app.test")
            .header(header::COOKIE, planted)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(
        location(&resp).ends_with("sso_error=expired"),
        "{}",
        location(&resp)
    );
}

/// A shared provider signs in on the tenant console; the tenant's override
/// row tightens it there and nowhere else.
#[tokio::test]
async fn a_shared_override_row_tightens_the_tenant_console() {
    use rustango::sso::policy::SsoSharedOverride;
    let _g = SUITE.lock().await;
    let env = boot().await;
    rustango::audit::ensure_table_pool(env.pool())
        .await
        .unwrap();
    let ann = env.user("ann", "ann@example.com", false).await;
    let bea = env.user_in(1, "bea", "bea@example.com", false).await;
    env.shared_provider("corp", true).await;
    assert_eq!(env.sso("corp", "sub-ann", "ann@example.com").await, Ok(ann));
    rustango::testkit::create_tables_for::<SsoSharedOverride>(env.pool())
        .await
        .unwrap();
    SsoSharedOverride {
        id: Auto::default(),
        shared_slug: "corp".into(),
        shared_provider_id: None,
        shared_issuer: None,
        allowed_email_domains: None,
        blocked_email_domains: Some("example.com".into()),
        allowed_idp_tenants: None,
        allowed_hosted_domains: None,
        required_claims: serde_json::json!({}),
        signup_mode: None,
        allow_email_link: None,
        allow_email_link_staff: None,
        sso_only: None,
        session_ttl_secs: None,
        superuser_session_ttl_secs: None,
        allowed_roles: None,
    }
    .insert_pool(env.pool())
    .await
    .unwrap();
    assert_eq!(
        env.sso("corp", "sub-ann", "ann@example.com").await,
        Err("nouser".into()),
        "the override blocks the domain"
    );
    assert_eq!(
        refused_rules(env.pool(), "corp", "sub-ann").await,
        ["domain_blocked"]
    );
    // The other tenant has no override.
    assert_eq!(
        env.sso_in(1, "corp", "sub-bea", "bea@example.com").await,
        Ok(bea)
    );
}
