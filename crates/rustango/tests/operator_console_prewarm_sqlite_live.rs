#![cfg(all(feature = "sqlite", feature = "tenancy"))]
//! Pre-warming tenant pools from the operator console (#1341).
//!
//! `prewarm-pools` was command-line only, so the one thing worth doing
//! right after a deploy, a registry restart, or a credential rotation
//! needed shell access to the production host.
//!
//! End to end through the real router, because the failure this guards
//! against is not the engine — `TenantPools::prewarm_database_tenants`
//! shipped in v0.27.7 — it is the surface: no route, no button, no link.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use rustango::sql::{sqlx, Auto};
use rustango::tenancy::operator_console::{router, router_with_pools, SessionSecret};
use rustango::tenancy::{Org, TenantPools};
use tower::ServiceExt;

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn unique(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::SeqCst)
    )
}

struct Booted {
    app: axum::Router,
    cookie: String,
    _tmp: tempfile::TempDir,
    _migrations: tempfile::TempDir,
}

/// `edit` decides whether the console gets pool-holding routes at all.
async fn boot(edit: bool) -> Booted {
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

    // Two database-mode tenants with reachable files — pre-warm only
    // walks database-mode, and these are what it should open.
    for i in 0..2 {
        let slug = unique(&format!("t{i}"));
        let db = tmp.path().join(format!("{slug}.db"));
        let mut org = Org {
            id: Auto::default(),
            slug: slug.clone(),
            display_name: slug.clone(),
            storage_mode: "database".into(),
            backend_kind: "sqlite".into(),
            database_url: Some(format!("sqlite://{}?mode=rwc", db.display())),
            schema_name: None,
            host_pattern: None,
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
    }

    let app = if edit {
        router_with_pools(
            registry.clone(),
            pools.clone(),
            SessionSecret::from_env_or_random(),
        )
    } else {
        router(registry.clone(), SessionSecret::from_env_or_random())
    };

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

    async fn post(&self, uri: &str) -> axum::response::Response {
        self.app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("cookie", &self.cookie)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }
}

/// The gap was a missing surface, not a missing engine — so the button
/// being on the page is the assertion that matters.
#[tokio::test]
async fn the_tenant_list_offers_a_prewarm_button() {
    let b = boot(true).await;
    let html = body_of(b.get("/orgs").await).await;
    assert!(
        html.contains(r#"action="/orgs/prewarm""#),
        "the orgs page should post to the prewarm route: {html}"
    );
    assert!(html.contains("Pre-warm pools"), "{html}");
}

#[tokio::test]
async fn prewarming_reports_how_many_it_opened() {
    let b = boot(true).await;
    let resp = b.post("/orgs/prewarm").await;
    assert_eq!(
        resp.status(),
        StatusCode::SEE_OTHER,
        "should redirect, so a reload does not re-open every pool"
    );
    let location = resp
        .headers()
        .get("location")
        .expect("a redirect")
        .to_str()
        .unwrap()
        .to_owned();
    assert!(location.starts_with("/orgs?notice="), "{location}");
    assert!(
        location.contains("warmed") && location.contains('2'),
        "both tenants should have been warmed: {location}"
    );
}

/// A read-only console holds no pools, so it must not offer the button
/// or accept the POST.
#[tokio::test]
async fn a_read_only_console_neither_shows_nor_accepts_it() {
    let b = boot(false).await;

    let html = body_of(b.get("/orgs").await).await;
    assert!(
        !html.contains("/orgs/prewarm"),
        "a read-only console should not offer it: {html}"
    );

    let resp = b.post("/orgs/prewarm").await;
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "the route should not be mounted at all"
    );
}

/// The write routes are behind a session like everything else.
#[tokio::test]
async fn prewarm_requires_a_session() {
    let b = boot(true).await;
    let resp = b
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/orgs/prewarm")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // The session middleware also answers with a 303, so the status alone
    // proves nothing — where it points is the difference between "sign in"
    // and "pre-warmed every pool for you".
    let location = resp
        .headers()
        .get("location")
        .map(|v| v.to_str().unwrap().to_owned())
        .unwrap_or_default();
    assert!(
        location.starts_with("/login"),
        "an unauthenticated POST should bounce to login, got `{location}` \
         with status {}",
        resp.status()
    );
}

/// A stray GET bounces back instead of 405ing — the same treatment the
/// other POST-only console routes get after a session-expiry replay.
#[tokio::test]
async fn a_get_bounces_back_to_the_list() {
    let b = boot(true).await;
    let resp = b.get("/orgs/prewarm").await;
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        resp.headers().get("location").unwrap().to_str().unwrap(),
        "/orgs"
    );
}
