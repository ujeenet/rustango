#![cfg(all(feature = "sqlite", feature = "tenancy"))]
//! Managing a tenant's extra hostnames from the operator console, end
//! to end through the real router.
//!
//! `org_hosts_sqlite_live` covers the engine — what `add_host` and
//! friends do to the registry. This covers the surface an operator
//! actually touches: that the routes are mounted at all, that they are
//! behind a session, that the page shows the base host without
//! controls, and that the writes land.
//!
//! The distinction earned its keep: the engine shipped complete and the
//! console had no page, no route and no link, so every one of the
//! engine's rules was unreachable from a browser.
//!
//! SQLite, because the console is tri-dialect and this needs no server.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use rustango::sql::{sqlx, Auto};
use rustango::tenancy::operator_console::{router, router_with_provisioning, SessionSecret};
use rustango::tenancy::provision::Provisioner;
use rustango::tenancy::{Org, TenantPools};
use tower::ServiceExt;

static UNIQ: AtomicU64 = AtomicU64::new(0);

/// Hyphens, not underscores: a slug becomes a hostname label.
fn unique(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::SeqCst)
    )
}

/// The resolver's host cache is process-global, so tests that mutate
/// hosts cannot interleave — see `org_hosts_sqlite_live`, which learned
/// this the hard way.
fn cache_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

struct Booted {
    app: axum::Router,
    cookie: String,
    registry: rustango::sql::Pool,
    slug: String,
    _tmp: tempfile::TempDir,
    _migrations: tempfile::TempDir,
}

async fn boot() -> Booted {
    let tmp = tempfile::tempdir().expect("tempdir");
    let url = format!("sqlite://{}?mode=rwc", tmp.path().join("reg.db").display());
    let pool = sqlx::SqlitePool::connect(&url).await.expect("connect");
    let pools = Arc::new(TenantPools::<sqlx::Sqlite>::new(pool));
    let migrations = tempfile::tempdir().expect("migrations dir");

    let mut buf: Vec<u8> = Vec::new();
    rustango::tenancy::manage::run_with_writer(
        pools.as_ref(),
        &url,
        migrations.path(),
        vec!["migrate-registry".to_owned()],
        &mut buf,
    )
    .await
    .expect("migrate-registry");

    let registry = pools.registry_pool();
    let username = unique("op");
    let password = "letmein";
    let mut op = rustango::tenancy::Operator {
        id: Auto::default(),
        username: username.clone(),
        password_hash: rustango::tenancy::password::hash(password).unwrap(),
        active: true,
        created_at: chrono::Utc::now(),
        password_changed_at: None,
    };
    op.insert_pool(&registry).await.expect("seed operator");

    // A tenant with a base host, which is the row the page must render
    // as undeletable.
    let slug = unique("acme");
    let mut org = Org {
        id: Auto::default(),
        slug: slug.clone(),
        display_name: slug.clone(),
        storage_mode: "schema".into(),
        backend_kind: "sqlite".into(),
        database_url: None,
        schema_name: Some(slug.clone()),
        host_pattern: Some(format!("{slug}.example.com")),
        port: None,
        path_prefix: None,
        active: true,
        created_at: chrono::Utc::now(),
        brand_name: None,
        brand_tagline: None,
        logo_path: None,
        favicon_path: None,
        primary_color: None,
        theme_mode: None,
    };
    org.insert_pool(&registry).await.expect("seed org");

    let provisioner = Provisioner::new(pools.clone(), url.clone(), migrations.path()).erased();
    let app = router_with_provisioning(
        registry.clone(),
        pools.clone(),
        provisioner,
        SessionSecret::from_env_or_random(),
    );

    let login = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "username={username}&password={password}"
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    let cookie = login
        .headers()
        .get("set-cookie")
        .expect("session cookie")
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();

    Booted {
        app,
        cookie,
        registry,
        slug,
        _tmp: tmp,
        _migrations: migrations,
    }
}

async fn body_of(resp: axum::response::Response) -> String {
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

impl Booted {
    async fn get(&self, uri: &str) -> axum::response::Response {
        self.app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header("cookie", &self.cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn post(&self, uri: &str, body: &str) -> axum::response::Response {
        self.app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("cookie", &self.cookie)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(body.to_owned()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    /// Where a write redirected to, which is where the outcome lives.
    fn location(resp: &axum::response::Response) -> String {
        resp.headers()
            .get("location")
            .expect("a write should redirect")
            .to_str()
            .unwrap()
            .to_owned()
    }
}

#[tokio::test]
async fn the_page_lists_the_base_host_and_marks_it_undeletable() {
    let _g = cache_lock().lock().await;
    let b = boot().await;

    let resp = b.get(&format!("/orgs/{}/hosts", b.slug)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_of(resp).await;

    assert!(
        html.contains(&format!("{}.example.com", b.slug)),
        "the base host should be listed: {html}"
    );
    assert!(html.contains("base"), "and flagged as the base: {html}");
    // The base has no row in `rustango_org_hosts`, so it must not offer
    // controls that would 500 or silently do nothing.
    assert!(
        !html.contains("hosts/remove"),
        "a tenant with only a base host should offer no remove form: {html}"
    );
}

#[tokio::test]
async fn a_host_added_through_the_form_is_listed_and_bound() {
    let _g = cache_lock().lock().await;
    let b = boot().await;
    let host = format!("shop.{}.example.com", b.slug);

    let resp = b
        .post(
            &format!("/orgs/{}/hosts/add", b.slug),
            &format!("hostname={host}"),
        )
        .await;
    assert!(
        resp.status().is_redirection(),
        "a write should Post/Redirect/Get, got {}",
        resp.status()
    );
    assert!(
        Booted::location(&resp).contains("notice="),
        "the outcome should survive the redirect"
    );

    let html = body_of(b.get(&format!("/orgs/{}/hosts", b.slug)).await).await;
    assert!(
        html.contains(&host),
        "the new host should be listed: {html}"
    );
    assert!(
        html.contains("hosts/remove"),
        "an extra host should offer a remove form: {html}"
    );

    let bound = rustango::tenancy::list_for_org(&b.registry, &b.slug)
        .await
        .unwrap();
    assert!(
        bound.iter().any(|h| h.hostname == host && !h.is_base),
        "the row should exist in the registry: {bound:?}"
    );
}

/// Uppercase is normalized, not refused — the client-side `pattern`
/// allows it for exactly this reason, so the browser is never stricter
/// than the server it is previewing.
#[tokio::test]
async fn an_uppercase_hostname_is_lowercased_rather_than_refused() {
    let _g = cache_lock().lock().await;
    let b = boot().await;

    let resp = b
        .post(
            &format!("/orgs/{}/hosts/add", b.slug),
            &format!("hostname=SHOP.{}.EXAMPLE.COM", b.slug.to_uppercase()),
        )
        .await;
    assert!(
        Booted::location(&resp).contains("notice="),
        "should have been accepted: {}",
        Booted::location(&resp)
    );

    let bound = rustango::tenancy::list_for_org(&b.registry, &b.slug)
        .await
        .unwrap();
    assert!(
        bound
            .iter()
            .any(|h| h.hostname == h.hostname.to_lowercase() && !h.is_base),
        "the stored hostname should be lowercase: {bound:?}"
    );
}

#[tokio::test]
async fn a_host_can_be_parked_and_served_again_without_unbinding_it() {
    let _g = cache_lock().lock().await;
    let b = boot().await;
    let host = format!("park.{}.example.com", b.slug);
    b.post(
        &format!("/orgs/{}/hosts/add", b.slug),
        &format!("hostname={host}"),
    )
    .await;

    // Park: the toggle posts no `enabled` field when disabling, which
    // is the shape an unchecked checkbox sends.
    b.post(
        &format!("/orgs/{}/hosts/toggle", b.slug),
        &format!("hostname={host}"),
    )
    .await;
    let parked = rustango::tenancy::list_for_org(&b.registry, &b.slug)
        .await
        .unwrap();
    assert!(
        parked.iter().any(|h| h.hostname == host && !h.enabled),
        "should be parked but still bound: {parked:?}"
    );

    // Serve again.
    b.post(
        &format!("/orgs/{}/hosts/toggle", b.slug),
        &format!("hostname={host}&enabled=on"),
    )
    .await;
    let serving = rustango::tenancy::list_for_org(&b.registry, &b.slug)
        .await
        .unwrap();
    assert!(
        serving.iter().any(|h| h.hostname == host && h.enabled),
        "should be serving again: {serving:?}"
    );
}

#[tokio::test]
async fn a_host_removed_through_the_form_is_unbound() {
    let _g = cache_lock().lock().await;
    let b = boot().await;
    let host = format!("gone.{}.example.com", b.slug);
    b.post(
        &format!("/orgs/{}/hosts/add", b.slug),
        &format!("hostname={host}"),
    )
    .await;

    b.post(
        &format!("/orgs/{}/hosts/remove", b.slug),
        &format!("hostname={host}"),
    )
    .await;

    let bound = rustango::tenancy::list_for_org(&b.registry, &b.slug)
        .await
        .unwrap();
    assert!(
        !bound.iter().any(|h| h.hostname == host),
        "should be gone: {bound:?}"
    );
}

/// The base host is protected structurally — it has no row here — and
/// the page hides its controls. An operator who posts the form anyway
/// must get the same answer, not a 500 or a silent success.
#[tokio::test]
async fn the_base_host_survives_a_hand_posted_remove_and_toggle() {
    let _g = cache_lock().lock().await;
    let b = boot().await;
    let base = format!("{}.example.com", b.slug);

    for verb in ["remove", "toggle"] {
        let resp = b
            .post(
                &format!("/orgs/{}/hosts/{verb}", b.slug),
                &format!("hostname={base}"),
            )
            .await;
        let location = Booted::location(&resp);
        assert!(
            location.contains("error="),
            "{verb} of the base host should be refused, got {location}"
        );
    }

    let hosts = rustango::tenancy::list_for_org(&b.registry, &b.slug)
        .await
        .unwrap();
    assert!(
        hosts.iter().any(|h| h.hostname == base && h.is_base),
        "the base host must still be there: {hosts:?}"
    );
}

/// A hostname belongs to one tenant, so a second claim is refused —
/// and the refusal has to reach the page, not a 500.
#[tokio::test]
async fn a_hostname_already_claimed_is_refused_with_a_reason() {
    let _g = cache_lock().lock().await;
    let b = boot().await;
    let host = format!("dup.{}.example.com", b.slug);
    b.post(
        &format!("/orgs/{}/hosts/add", b.slug),
        &format!("hostname={host}"),
    )
    .await;

    let resp = b
        .post(
            &format!("/orgs/{}/hosts/add", b.slug),
            &format!("hostname={host}"),
        )
        .await;
    let location = Booted::location(&resp);
    assert!(
        location.contains("error="),
        "a duplicate should be refused: {location}"
    );
    assert!(
        location.contains("already"),
        "and should say why: {location}"
    );
}

#[tokio::test]
async fn a_value_that_is_not_a_bare_hostname_is_refused() {
    let _g = cache_lock().lock().await;
    let b = boot().await;

    for bad in ["https%3A%2F%2Fx.com", "x.com%3A8080", "x.com%2Fadmin", ""] {
        let resp = b
            .post(
                &format!("/orgs/{}/hosts/add", b.slug),
                &format!("hostname={bad}"),
            )
            .await;
        let location = Booted::location(&resp);
        assert!(
            location.contains("error="),
            "`{bad}` should be refused, got {location}"
        );
    }
}

/// The engine scopes every mutation to the owning org. Posting another
/// tenant's hostname at this tenant's URL must not touch it.
#[tokio::test]
async fn a_host_belonging_to_another_tenant_cannot_be_touched() {
    let _g = cache_lock().lock().await;
    let b = boot().await;

    let other = unique("other");
    let mut org = Org {
        id: Auto::default(),
        slug: other.clone(),
        display_name: other.clone(),
        storage_mode: "schema".into(),
        backend_kind: "sqlite".into(),
        database_url: None,
        schema_name: Some(other.clone()),
        host_pattern: Some(format!("{other}.example.com")),
        port: None,
        path_prefix: None,
        active: true,
        created_at: chrono::Utc::now(),
        brand_name: None,
        brand_tagline: None,
        logo_path: None,
        favicon_path: None,
        primary_color: None,
        theme_mode: None,
    };
    org.insert_pool(&b.registry).await.expect("seed other org");
    let victim = format!("owned.{other}.example.com");
    rustango::tenancy::add_host(&b.registry, &other, &victim)
        .await
        .expect("bind to the other tenant");

    for verb in ["remove", "toggle"] {
        let resp = b
            .post(
                &format!("/orgs/{}/hosts/{verb}", b.slug),
                &format!("hostname={victim}"),
            )
            .await;
        assert!(
            Booted::location(&resp).contains("error="),
            "{verb} across tenants should be refused"
        );
    }

    let still = rustango::tenancy::list_for_org(&b.registry, &other)
        .await
        .unwrap();
    assert!(
        still.iter().any(|h| h.hostname == victim && h.enabled),
        "the other tenant's host must be untouched: {still:?}"
    );
}

#[tokio::test]
async fn an_unknown_tenant_is_a_404_not_an_empty_list() {
    let _g = cache_lock().lock().await;
    let b = boot().await;
    let resp = b.get("/orgs/no-such-tenant/hosts").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_routes_require_a_session() {
    let _g = cache_lock().lock().await;
    let b = boot().await;

    for (method, uri) in [
        ("GET", format!("/orgs/{}/hosts", b.slug)),
        ("POST", format!("/orgs/{}/hosts/add", b.slug)),
        ("POST", format!("/orgs/{}/hosts/remove", b.slug)),
        ("POST", format!("/orgs/{}/hosts/toggle", b.slug)),
    ] {
        let resp = b
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(&uri)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from("hostname=evil.example.com"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            resp.status().is_redirection() || resp.status() == StatusCode::UNAUTHORIZED,
            "{method} {uri} was reachable without a session: {}",
            resp.status()
        );
    }

    let hosts = rustango::tenancy::list_for_org(&b.registry, &b.slug)
        .await
        .unwrap();
    assert!(
        !hosts.iter().any(|h| h.hostname == "evil.example.com"),
        "an unauthenticated post must not bind a host: {hosts:?}"
    );
}

/// The page is only reachable if something links to it. A working
/// handler nobody can navigate to is the bug this whole feature
/// started as.
#[tokio::test]
async fn the_edit_page_links_to_the_hosts_page() {
    let _g = cache_lock().lock().await;
    let b = boot().await;
    let html = body_of(b.get(&format!("/orgs/{}/edit", b.slug)).await).await;
    assert!(
        html.contains(&format!("/orgs/{}/hosts", b.slug)),
        "the edit page should link to the hostnames page: {html}"
    );
}

/// Read-only consoles do not get write routes. `router()` supplies no
/// pool invalidator, which is how a deployment says "read-only".
#[tokio::test]
async fn a_read_only_console_does_not_mount_the_routes() {
    let _g = cache_lock().lock().await;
    let b = boot().await;
    let read_only = router(b.registry.clone(), SessionSecret::from_env_or_random());

    let resp = read_only
        .oneshot(
            Request::builder()
                .uri(format!("/orgs/{}/hosts", b.slug))
                .header("cookie", &b.cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "a read-only console must not expose host management"
    );
}
