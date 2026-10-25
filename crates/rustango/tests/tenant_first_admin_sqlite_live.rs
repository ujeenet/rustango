#![cfg(all(feature = "sqlite", feature = "tenancy", feature = "admin"))]
//! A new tenant's first admin: made while provisioning, handed over by a
//! single-use set-password link, from the console, the CLI and the webhook.
//! The link reaches the operator once and is never stored, audited or logged.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{header, Request, StatusCode};
use rustango::core::Column as _;
use rustango::sql::{sqlx, Auto, FetcherPool as _, UpdaterPool as _};
use rustango::tenancy::operator_console::router_with_provisioning;
use rustango::tenancy::provision::Provisioner;
use rustango::tenancy::provision_store as store;
use rustango::tenancy::tenant_console::{SessionSecret, COOKIE_NAME};
use rustango::tenancy::{
    admin::TenantAdminBuilder, ChainResolver, FirstAdminHandover, Org, SubdomainResolver,
    TenantPools, User,
};
use tower::ServiceExt;

/// Log capture is thread-local, and the CLI reads the shared dev key file.
static SUITE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static UNIQ: AtomicU64 = AtomicU64::new(0);

const NEW_PASSWORD: &str = "Corr3ct-Horse-Battery";

fn unique(prefix: &str) -> String {
    format!(
        "{prefix}{}x{}",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::SeqCst)
    )
}

struct Env {
    pools: Arc<TenantPools<sqlx::Sqlite>>,
    url: String,
    migrations: std::path::PathBuf,
    tmp: tempfile::TempDir,
}

impl Env {
    async fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let url = format!("sqlite://{}?mode=rwc", tmp.path().join("reg.db").display());
        let pools = Arc::new(TenantPools::<sqlx::Sqlite>::new(
            sqlx::SqlitePool::connect(&url).await.expect("registry"),
        ));
        let migrations = tmp.path().join("migrations");
        std::fs::create_dir_all(&migrations).unwrap();
        let env = Self {
            pools,
            url,
            migrations,
            tmp,
        };
        env.run(&["migrate-registry"])
            .await
            .expect("migrate-registry");
        env
    }

    fn registry(&self) -> rustango::sql::Pool {
        self.pools.registry_pool()
    }

    fn tenant_url(&self, slug: &str) -> String {
        format!(
            "sqlite://{}?mode=rwc",
            self.tmp.path().join(format!("{slug}.db")).display()
        )
    }

    async fn run(&self, args: &[&str]) -> Result<String, String> {
        let mut out = Vec::new();
        rustango::tenancy::manage::run_with_writer(
            self.pools.as_ref(),
            &self.url,
            &self.migrations,
            args.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
            &mut out,
        )
        .await
        .map(|()| String::from_utf8_lossy(&out).into_owned())
        .map_err(|e| e.to_string())
    }

    /// The tenant admin on default routes, for `<slug>.app.test`.
    fn tenant_admin(&self, secret: &SessionSecret) -> axum::Router {
        TenantAdminBuilder::new(
            self.pools.clone(),
            self.url.clone(),
            ChainResolver::new().push(SubdomainResolver::new("app.test")),
        )
        .with_session(secret.clone())
        .build()
    }

    async fn org(&self, slug: &str) -> Option<Org> {
        Org::objects()
            .where_(Org::slug.eq(slug.to_owned()))
            .fetch(&self.registry())
            .await
            .unwrap()
            .into_iter()
            .next()
    }

    async fn user(&self, slug: &str, username: &str) -> Option<User> {
        let org = self.org(slug).await?;
        let pool = self.pools.scoped_pool_dyn(&org).await.expect("tenant pool");
        User::objects()
            .where_(User::username.eq(username.to_owned()))
            .fetch(&pool)
            .await
            .unwrap()
            .into_iter()
            .next()
    }

    /// Every stored run error and event, and every audit change.
    async fn trail(&self) -> String {
        let registry = self.registry();
        let mut out = String::new();
        for run in store::recent_runs(&registry, 100, 0).await.unwrap() {
            out.push_str(&format!("{:?}\n", run.error));
            let id = run.id.get().copied().unwrap_or_default();
            for e in store::events_since(&registry, id, 0).await.unwrap() {
                out.push_str(&format!("{} {} {}\n", e.step, e.status, e.message));
            }
        }
        for e in rustango::audit::list(&registry, &Default::default(), 500, 0)
            .await
            .unwrap()
        {
            out.push_str(&format!(
                "{} {} {} {}\n",
                e.entity_table, e.entity_pk, e.source, e.changes
            ));
        }
        out
    }
}

async fn body_of(resp: axum::response::Response) -> String {
    String::from_utf8_lossy(&to_bytes(resp.into_body(), 4 * 1024 * 1024).await.unwrap())
        .into_owned()
}

fn form_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Undo Tera's HTML escaping of an attribute value.
fn unescape(s: &str) -> String {
    s.replace("&#x2F;", "/")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// The `/change-password?…` link in `text`, unescaped.
fn link_in(text: &str) -> String {
    let start = text.find("http").expect("a link");
    let rest = &text[start..];
    let end = rest
        .find(|c: char| c == '"' || c.is_whitespace())
        .unwrap_or(rest.len());
    let link = unescape(&rest[..end]);
    assert!(link.contains("/change-password?"), "{link}");
    link
}

/// The link's path and query: what a browser sends to the tenant host.
fn path_and_query(link: &str) -> String {
    let after_scheme = link.split_once("://").expect("absolute").1;
    after_scheme[after_scheme.find('/').expect("a path")..].to_owned()
}

/// The link's signature, the one part that must appear nowhere else.
fn signature_of(link: &str) -> String {
    link.split(['?', '&'])
        .find_map(|p| p.strip_prefix("signature="))
        .expect("signed")
        .to_owned()
}

/// The `Origin` a browser sends on a form POST from `page`: `null` under
/// `Referrer-Policy: no-referrer`, the page's own origin otherwise.
fn browser_origin(page: &axum::response::Response, host: &str) -> String {
    match page.headers().get(header::REFERRER_POLICY) {
        Some(v) if v == "no-referrer" => "null".to_owned(),
        _ => format!("http://{host}"),
    }
}

async fn tenant_req(
    admin: &axum::Router,
    host: &str,
    method: &str,
    uri: &str,
    body: &str,
) -> axum::response::Response {
    admin
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header(header::HOST, host)
                .header(header::COOKIE, "rustango_csrf=t")
                .header("x-csrf-token", "t")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body.to_owned()))
                .unwrap(),
        )
        .await
        .unwrap()
}

/// Set a password through `link`, check it works once, then sign in with it.
async fn redeem_and_sign_in(admin: &axum::Router, host: &str, link: &str, username: &str) {
    let uri = path_and_query(link);
    let form = tenant_req(admin, host, "GET", &uri, "").await;
    assert_eq!(form.status(), StatusCode::OK);
    assert_eq!(form.headers()[header::CACHE_CONTROL], "no-store");
    let html = body_of(form).await;
    assert!(html.contains("Set your password"), "{html}");

    let set = format!("new_password={NEW_PASSWORD}&confirm_password={NEW_PASSWORD}");
    let done = body_of(tenant_req(admin, host, "POST", &uri, &set).await).await;
    assert!(done.contains("Password set"), "{done}");
    let again = body_of(tenant_req(admin, host, "POST", &uri, &set).await).await;
    assert!(
        again.contains("invalid, used or expired"),
        "the link worked twice: {again}"
    );

    let login = tenant_req(
        admin,
        host,
        "POST",
        "/login",
        // Some feature sets read the login's token from the form, not the header.
        &format!(
            "username={}&password={NEW_PASSWORD}&_csrf=t",
            form_encode(username)
        ),
    )
    .await;
    if !login.status().is_redirection() {
        let status = login.status();
        panic!("login failed: {status}: {}", body_of(login).await);
    }
    assert!(
        login
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .any(|v| v.to_str().unwrap().starts_with(COOKIE_NAME)),
        "no tenant session cookie"
    );
}

async fn operator_cookie(console: &axum::Router, registry: &rustango::sql::Pool) -> String {
    let username = unique("op");
    let mut op = rustango::tenancy::Operator {
        id: Auto::default(),
        username: username.clone(),
        password_hash: rustango::tenancy::password::hash("letmein").unwrap(),
        active: true,
        created_at: chrono::Utc::now(),
        password_changed_at: None,
        sessions_revoked_at: None,
    };
    op.insert_pool(registry).await.expect("seed operator");
    let login = console
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header("cookie", "rustango_csrf=t")
                .header("x-csrf-token", "t")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(format!("username={username}&password=letmein")))
                .unwrap(),
        )
        .await
        .unwrap();
    login.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}

fn capture() -> (
    rustango::testkit::CaptureWriter,
    tracing::subscriber::DefaultGuard,
) {
    let buf = rustango::testkit::CaptureWriter::default();
    let writer = buf.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .with_max_level(tracing::Level::TRACE)
        .finish();
    (buf, tracing::subscriber::set_default(subscriber))
}

/// The console creates a superuser and shows its link once; the admin
/// sets a password with it and signs in. The link is in no record or log.
#[tokio::test]
async fn the_console_creates_a_first_admin_who_signs_in_through_the_link() {
    let _suite = SUITE.lock().await;
    let env = Env::new().await;
    let secret = SessionSecret::from_bytes(b"first-admin-tenant-secret-32-b!!".to_vec());
    let provisioner = Provisioner::new(env.pools.clone(), env.url.clone(), env.migrations.clone())
        .with_first_admin_handover(FirstAdminHandover::new(&secret))
        .erased();
    let console = router_with_provisioning(
        env.registry(),
        env.pools.clone(),
        provisioner,
        SessionSecret::from_env_or_random(),
    );
    let cookie = operator_cookie(&console, &env.registry()).await;

    let form_page = console
        .clone()
        .oneshot(
            Request::builder()
                .uri("/orgs/new")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(body_of(form_page).await.contains("name=\"admin_email\""));

    let slug = unique("acme");
    let host = format!("{slug}.app.test");
    let email = "ann@acme.test";
    let (logs, guard) = capture();
    let resp = console
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/orgs/new")
                .header("cookie", "rustango_csrf=t")
                .header("x-csrf-token", "t")
                .header("cookie", &cookie)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "slug={slug}&storage_mode=database&backend_kind=sqlite&database_source=url\
                     &database_url={}&host_pattern={host}&admin_email={}",
                    form_encode(&env.tenant_url(&slug)),
                    form_encode(email),
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    drop(guard);
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "the run renders with the link"
    );
    assert_eq!(resp.headers()[header::CACHE_CONTROL], "no-store");
    let run_policy = resp.headers()[header::REFERRER_POLICY].clone();
    let run_origin = browser_origin(&resp, "console.test");
    let page = body_of(resp).await;
    assert!(page.contains("create_admin"), "{page}");
    let link = link_in(&page[page.find("first-admin").expect("handover block")..]);
    let sig = signature_of(&link);

    let user = env
        .user(&slug, email)
        .await
        .expect("the first admin exists");
    assert!(user.is_superuser && user.active);
    assert!(env.org(&slug).await.unwrap().active);

    let trail = env.trail().await;
    assert!(
        trail.contains("provision:first_admin") && trail.contains(email),
        "{trail}"
    );
    assert!(!trail.contains(&sig), "the link reached a record: {trail}");
    assert!(!logs.contents().contains(&sig), "the link reached a log");

    // Shown once: the run page does not show it again.
    let run_id = store::recent_runs(&env.registry(), 1, 0).await.unwrap()[0]
        .id
        .get()
        .copied()
        .unwrap();
    let again = console
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/orgs/provision/{run_id}"))
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(!body_of(again).await.contains(&sig));

    let admin = env.tenant_admin(&secret);
    redeem_and_sign_in(&admin, &host, &link, email).await;

    // The run page's own form (sign out) still passes the CSRF Origin check.
    let logout = console
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/logout")
                .header(header::HOST, "console.test")
                .header(header::ORIGIN, &run_origin)
                .header("cookie", "rustango_csrf=t")
                .header("x-csrf-token", "t")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(logout.status(), StatusCode::FORBIDDEN, "{run_origin}");
    assert_eq!(run_policy, "same-origin");
}

/// `create-tenant --admin-email` prints the link once; a link for another
/// tenant does not open this one.
#[tokio::test]
async fn the_cli_creates_a_first_admin_and_prints_the_link() {
    let _suite = SUITE.lock().await;
    let env = Env::new().await;
    let slug = unique("cli");
    let host = format!("{slug}.app.test");
    // The CLI signs with the server's tenant key: the env var, or the file
    // a server writes on first boot. It only loads it.
    let secret =
        SessionSecret::from_env_or_disk(std::path::Path::new("./var/.rustango_tenant_session.key"));
    let out = env
        .run(&[
            "create-tenant",
            &slug,
            "--mode",
            "database",
            "--backend",
            "sqlite",
            "--database-url",
            &env.tenant_url(&slug),
            "--host-pattern",
            &host,
            "--admin-email",
            "bob@acme.test",
            "--admin-username",
            "bob",
        ])
        .await
        .expect("create-tenant");
    assert!(out.contains("created the first admin"), "{out}");
    let link = link_in(&out[out.find("set-password link").expect("the link")..]);
    let user = env
        .user(&slug, "bob")
        .await
        .expect("the first admin exists");
    assert!(user.is_superuser);
    assert!(!env.trail().await.contains(&signature_of(&link)));
    let admin = env.tenant_admin(&secret);

    // The same path and query on another tenant's host is refused.
    let other = unique("oth");
    env.run(&[
        "create-tenant",
        &other,
        "--mode",
        "database",
        "--backend",
        "sqlite",
        "--database-url",
        &env.tenant_url(&other),
        "--host-pattern",
        &format!("{other}.app.test"),
    ])
    .await
    .expect("second tenant");
    let foreign = body_of(
        tenant_req(
            &admin,
            &format!("{other}.app.test"),
            "GET",
            &path_and_query(&link),
            "",
        )
        .await,
    )
    .await;
    assert!(foreign.contains("invalid, used or expired"), "{foreign}");

    redeem_and_sign_in(&admin, &host, &link, "bob").await;

    // A fresh link for the same user, for when the first one expired.
    let fresh = env
        .run(&["set-password-link", &slug, "bob"])
        .await
        .expect("set-password-link");
    let fresh = &fresh[fresh.find("set-password link").expect("the link")..];
    assert_ne!(signature_of(&link_in(fresh)), signature_of(&link));
}

/// A tenant with a first admin `ann`, and a link for her from `handover`.
async fn tenant_with_admin(env: &Env, handover: &FirstAdminHandover) -> (String, String) {
    let slug = unique("http");
    let request = rustango::tenancy::provision::ProvisionRequest {
        host_pattern: Some(format!("{slug}.app.test")),
        first_admin: Some(
            rustango::tenancy::FirstAdmin::new("ann@acme.test", Some("ann")).unwrap(),
        ),
        ..rustango::tenancy::provision::ProvisionRequest::database(&slug, env.tenant_url(&slug))
    };
    let outcome = rustango::tenancy::provision::provision_tenant(
        env.pools.as_ref(),
        &env.url,
        &env.migrations,
        &request,
        None,
    )
    .await
    .expect("provision");
    assert!(outcome.first_admin.is_some());
    let org = env.org(&slug).await.unwrap();
    let handed =
        rustango::tenancy::first_admin::hand_over_user(env.pools.as_ref(), &org, "ann", handover)
            .await
            .unwrap();
    let link = handed
        .link()
        .expect("no mailer: a link")
        .expose()
        .to_owned();
    (slug, link)
}

/// The cookie value `name` set by `resp`.
fn set_cookie(resp: &axum::response::Response, name: &str) -> Option<String> {
    resp.headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|v| v.strip_prefix(&format!("{name}=")))
        .map(|v| v.split(';').next().unwrap().to_owned())
}

/// Tampered, foreign, expired, used and weak: each refused over HTTP, and
/// the POST needs a real CSRF token.
#[tokio::test]
async fn a_set_password_link_holds_up_over_http() {
    let _suite = SUITE.lock().await;
    let env = Env::new().await;
    let secret = SessionSecret::from_bytes(b"first-admin-http-secret-32-byte!".to_vec());
    let (slug, link) = tenant_with_admin(&env, &FirstAdminHandover::new(&secret)).await;
    let host = format!("{slug}.app.test");
    let admin = env.tenant_admin(&secret);
    let uri = path_and_query(&link);
    let hash_before = env.user(&slug, "ann").await.unwrap().password_hash;
    let set = format!("new_password={NEW_PASSWORD}&confirm_password={NEW_PASSWORD}");
    let dead = |html: &str| html.contains("invalid, used or expired");

    // Another user id under the same signature.
    let id = env
        .user(&slug, "ann")
        .await
        .unwrap()
        .id
        .get()
        .copied()
        .unwrap();
    let tampered = uri.replace(&format!("user_id={id}"), &format!("user_id={}", id + 1));
    assert_ne!(tampered, uri);
    assert!(dead(
        &body_of(tenant_req(&admin, &host, "GET", &tampered, "").await).await
    ));
    assert!(dead(
        &body_of(tenant_req(&admin, &host, "POST", &tampered, &set).await).await
    ));

    // The same link POSTed to another tenant.
    let (other, _) = tenant_with_admin(&env, &FirstAdminHandover::new(&secret)).await;
    let foreign = format!("{other}.app.test");
    assert!(dead(
        &body_of(tenant_req(&admin, &foreign, "POST", &uri, &set).await).await
    ));

    // Expired.
    let org = env.org(&slug).await.unwrap();
    let short = FirstAdminHandover::new(&secret).ttl(std::time::Duration::from_secs(1));
    let expired =
        rustango::tenancy::first_admin::hand_over_user(env.pools.as_ref(), &org, "ann", &short)
            .await
            .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
    let expired_uri = path_and_query(expired.link().unwrap().expose());
    assert!(dead(
        &body_of(tenant_req(&admin, &host, "GET", &expired_uri, "").await).await
    ));
    assert!(dead(
        &body_of(tenant_req(&admin, &host, "POST", &expired_uri, &set).await).await
    ));

    // The form's own rule: too short, and the link survives it.
    let weak = body_of(
        tenant_req(
            &admin,
            &host,
            "POST",
            &uri,
            "new_password=Ab1!&confirm_password=Ab1!",
        )
        .await,
    )
    .await;
    assert!(!dead(&weak) && !weak.contains("Password set"), "{weak}");
    assert_eq!(
        env.user(&slug, "ann").await.unwrap().password_hash,
        hash_before
    );

    // No CSRF token: refused, nothing written.
    let bare = admin
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&uri)
                .header(header::HOST, &host)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(set.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bare.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        env.user(&slug, "ann").await.unwrap().password_hash,
        hash_before
    );

    // The token the GET hands out, sent back as a browser would.
    let page = admin
        .clone()
        .oneshot(
            Request::builder()
                .uri(&uri)
                .header(header::HOST, &host)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let policy = page.headers()[header::REFERRER_POLICY].clone();
    let origin = browser_origin(&page, &host);
    let cookie = set_cookie(&page, "rustango_csrf").expect("a CSRF cookie");
    let html = body_of(page).await;
    let token = html
        .split("name=\"_csrf\" value=\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .expect("a CSRF field")
        .to_owned();
    let done = admin
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&uri)
                .header(header::HOST, &host)
                .header(header::ORIGIN, &origin)
                .header(header::COOKIE, format!("rustango_csrf={cookie}"))
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(format!("_csrf={}&{set}", form_encode(&token))))
                .unwrap(),
        )
        .await
        .unwrap();
    let done = body_of(done).await;
    assert!(done.contains("Password set"), "{done}");
    // Keeps the link out of cross-origin `Referer` without nulling `Origin`.
    assert_eq!(policy, "same-origin");

    // Used: the form no longer shows.
    assert!(dead(
        &body_of(tenant_req(&admin, &host, "GET", &uri, "").await).await
    ));
}

/// `set-password-link` refuses an inactive user.
#[tokio::test]
async fn no_link_for_an_inactive_user() {
    let _suite = SUITE.lock().await;
    let env = Env::new().await;
    let secret = SessionSecret::from_bytes(b"first-admin-http-secret-32-byte!".to_vec());
    let handover = FirstAdminHandover::new(&secret);
    let (slug, _) = tenant_with_admin(&env, &handover).await;
    let org = env.org(&slug).await.unwrap();
    let pool = env.pools.scoped_pool_dyn(&org).await.unwrap();
    User::objects()
        .where_(User::username.eq("ann".to_owned()))
        .update()
        .set("active", false)
        .execute_pool(&pool)
        .await
        .unwrap();
    let err =
        rustango::tenancy::first_admin::hand_over_user(env.pools.as_ref(), &org, "ann", &handover)
            .await
            .expect_err("refused");
    assert!(err.to_string().contains("inactive"), "{err}");
}

/// A resumed run reuses the admin an earlier run made, and never adds a
/// second superuser for somebody else.
#[cfg(feature = "testkit")]
#[tokio::test]
async fn a_resumed_run_never_adds_a_second_superuser() {
    use rustango::tenancy::provision::{provision_tenant_in_run, ProvisionRequest};
    let _suite = SUITE.lock().await;
    let env = Env::new().await;
    let slug = unique("resume");
    let request = |email: &str, username: &str| ProvisionRequest {
        first_admin: Some(rustango::tenancy::FirstAdmin::new(email, Some(username)).unwrap()),
        ..ProvisionRequest::database(&slug, env.tenant_url(&slug))
    };
    let run = |request: ProvisionRequest| {
        let env = &env;
        async move {
            let opened = store::open_run(
                &env.registry(),
                &request.slug,
                "database",
                "sqlite",
                request.database_url(),
                Some("test"),
                None,
            )
            .await
            .unwrap();
            let id = opened.id.get().copied().unwrap();
            provision_tenant_in_run(
                env.pools.as_ref(),
                &env.url,
                &env.migrations,
                &request,
                None,
                id,
            )
            .await
        }
    };
    std::env::set_var("RUSTANGO_PROVISION_FAIL_AT", "activate");
    run(request("ann@acme.test", "ann"))
        .await
        .expect_err("activate fails");
    std::env::remove_var("RUSTANGO_PROVISION_FAIL_AT");
    assert!(env.user(&slug, "ann").await.unwrap().is_superuser);

    let err = run(request("eve@evil.test", "eve"))
        .await
        .expect_err("another admin");
    assert!(err.to_string().contains("superuser"), "{err}");
    assert!(env.user(&slug, "eve").await.is_none());
    assert!(!env.org(&slug).await.unwrap().active);

    let outcome = run(request("ann@acme.test", "ann"))
        .await
        .expect("the same admin resumes");
    let ann = env.user(&slug, "ann").await.unwrap();
    assert_eq!(
        outcome.first_admin.unwrap().user_id,
        ann.id.get().copied().unwrap()
    );
    assert!(env.org(&slug).await.unwrap().active);
    let org = env.org(&slug).await.unwrap();
    let pool = env.pools.scoped_pool_dyn(&org).await.unwrap();
    let supers = User::objects()
        .where_(User::is_superuser.eq(true))
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(supers.len(), 1);
}

/// A first admin needs migrations: refused before anything is made.
#[tokio::test]
async fn a_first_admin_without_migrations_is_refused_up_front() {
    let _suite = SUITE.lock().await;
    let env = Env::new().await;
    let slug = unique("nomig");
    let err = env
        .run(&[
            "create-tenant",
            &slug,
            "--mode",
            "database",
            "--backend",
            "sqlite",
            "--database-url",
            &env.tenant_url(&slug),
            "--no-migrate",
            "--admin-email",
            "c@acme.test",
        ])
        .await
        .expect_err("refused");
    assert!(err.contains("migrations"), "{err}");
    assert!(env.org(&slug).await.is_none());
}

#[cfg(all(feature = "webhook", feature = "email"))]
mod webhook {
    use super::*;
    use rustango::email::InMemoryMailer;
    use rustango::tenancy::provision_webhook::{router, UrlPolicy, WebhookConfig, WebhookState};
    use rustango::webhook::{sign, SignatureFormat};

    const HOOK_SECRET: &[u8] = b"shared-secret-at-least-32-bytes-long";

    fn app(env: &Env, handover: Option<FirstAdminHandover>) -> axum::Router {
        let mut provisioner =
            Provisioner::new(env.pools.clone(), env.url.clone(), env.migrations.clone());
        if let Some(h) = handover {
            provisioner = provisioner.with_first_admin_handover(h);
        }
        let template = format!("sqlite://{}/{{slug}}.db?mode=rwc", env.tmp.path().display());
        router(
            "/hooks/provision",
            WebhookState {
                config: WebhookConfig {
                    url_policy: UrlPolicy::Template(template),
                    ..WebhookConfig::new(HOOK_SECRET.to_vec(), "")
                },
                provisioner: provisioner.erased(),
            },
        )
    }

    async fn deliver(app: &axum::Router, slug: &str) -> axum::response::Response {
        let body = serde_json::json!({
            "event_id": unique("evt"),
            "timestamp": chrono::Utc::now().timestamp(),
            "slug": slug,
            "host_pattern": format!("{slug}.app.test"),
            "admin_email": "dee@acme.test",
        })
        .to_string();
        let sig = sign(
            SignatureFormat::HexSha256WithPrefix,
            HOOK_SECRET,
            body.as_bytes(),
        )
        .unwrap();
        app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/hooks/provision")
                    .header("content-type", "application/json")
                    .header("x-signature-256", sig)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    /// The webhook mails the first admin a link bound to their tenant.
    #[tokio::test]
    async fn the_webhook_mails_a_tenant_bound_link() {
        let _suite = SUITE.lock().await;
        let env = Env::new().await;
        let secret = SessionSecret::from_bytes(b"first-admin-webhook-secret-32-b!".to_vec());
        let mailer = Arc::new(InMemoryMailer::new());
        let handover = FirstAdminHandover::new(&secret).mailer(mailer.clone(), "noreply@ops.test");
        let app = app(&env, Some(handover));
        let slug = unique("hook");
        let resp = deliver(&app, &slug).await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);

        let mut sent = Vec::new();
        for _ in 0..400 {
            sent = mailer.sent();
            if !sent.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert_eq!(sent.len(), 1, "one mail");
        assert_eq!(sent[0].to, vec!["dee@acme.test".to_owned()]);
        let link = link_in(&sent[0].body);
        let org = env.org(&slug).await.expect("tenant");
        let id = org.id.get().copied().unwrap();
        assert!(
            link.contains(&format!("scope=tenant%3A{slug}%3A{id}")),
            "{link}"
        );
        assert!(env.user(&slug, "dee@acme.test").await.unwrap().is_superuser);
        assert!(!env.trail().await.contains(&signature_of(&link)));

        let admin = env.tenant_admin(&secret);
        redeem_and_sign_in(&admin, &format!("{slug}.app.test"), &link, "dee@acme.test").await;
    }

    /// Without a mailer nobody could pass the link on: refused, no run.
    #[tokio::test]
    async fn the_webhook_refuses_a_first_admin_it_cannot_mail() {
        let _suite = SUITE.lock().await;
        let env = Env::new().await;
        let secret = SessionSecret::from_bytes(b"first-admin-webhook-secret-32-b!".to_vec());
        for handover in [Some(FirstAdminHandover::new(&secret)), None] {
            let app = app(&env, handover);
            let slug = unique("nomail");
            let resp = deliver(&app, &slug).await;
            assert_eq!(resp.status(), StatusCode::FORBIDDEN);
            assert!(store::recent_runs(&env.registry(), 10, 0)
                .await
                .unwrap()
                .is_empty());
            assert!(env.org(&slug).await.is_none());
        }
    }
}
