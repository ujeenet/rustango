#![cfg(all(feature = "sqlite", feature = "tenancy"))]
//! Taking a tenant out of service from the console.
//!
//! Purging is unrecoverable, so most of this is about what must *not*
//! happen: a mistyped confirmation, a database-mode purge without the
//! explicit flag, an unauthenticated post.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use rustango::core::Column as _;
use rustango::sql::{sqlx, Auto, FetcherPool as _};
use rustango::tenancy::operator_console::{router_with_provisioning, SessionSecret};
use rustango::tenancy::provision::Provisioner;
use rustango::tenancy::{Operator, Org, TenantPools};
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
    registry: rustango::sql::Pool,
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
    let password = "letmein-please";
    let mut op = Operator {
        id: Auto::default(),
        username: username.clone(),
        password_hash: rustango::tenancy::password::hash(password).unwrap(),
        active: true,
        created_at: chrono::Utc::now(),
        password_changed_at: None,
    };
    op.insert_pool(&registry).await.expect("seed operator");

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
        _tmp: tmp,
        _migrations: migrations,
    }
}

async fn body_of(resp: axum::response::Response) -> String {
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

impl Booted {
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

    async fn org(&self, slug: &str) -> Option<Org> {
        let rows: Vec<Org> = Org::objects()
            .where_(Org::slug.eq(slug.to_owned()))
            .fetch(&self.registry)
            .await
            .unwrap();
        rows.into_iter().next()
    }

    /// A real tenant with its own SQLite file, provisioned through the
    /// console so its storage genuinely exists.
    async fn tenant(&self) -> String {
        let slug = unique("doomed");
        let db = self._tmp.path().join(format!("{slug}.db"));
        let form = format!(
            "slug={slug}&storage_mode=database&backend_kind=sqlite&database_url=sqlite%3A%2F%2F{}%3Fmode%3Drwc",
            db.display().to_string().replace('/', "%2F")
        );
        let resp = self
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/orgs/new")
                    .header("cookie", &self.cookie)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(form))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            resp.status().is_redirection(),
            "the tenant should provision: {}",
            body_of(resp).await
        );
        slug
    }
}

#[tokio::test]
async fn deactivating_stops_service_without_destroying_anything() {
    let b = boot().await;
    let slug = b.tenant().await;
    assert!(b.org(&slug).await.unwrap().active);

    let resp = b.post(&format!("/orgs/{slug}/deactivate"), "").await;
    assert!(resp.status().is_redirection());

    let org = b.org(&slug).await.expect("the row survives");
    assert!(!org.active, "should be inactive");
    assert!(
        org.database_url.is_some(),
        "and still point at its storage: {org:?}"
    );
}

/// A purge removes the tenant, so there is no tenant page to report
/// back on — the org list is where the outcome has to appear, and it
/// had no place to render one.
#[tokio::test]
async fn the_org_list_renders_the_outcome_it_is_redirected_with() {
    let b = boot().await;
    let html = body_of(b.get("/orgs?notice=purged%20%60ghost%60").await).await;
    assert!(
        html.contains("purged") && html.contains("ghost"),
        "the notice should reach the page: {html}"
    );

    let html = body_of(b.get("/orgs?error=nope").await).await;
    assert!(html.contains("nope"), "and so should an error: {html}");
}

#[tokio::test]
async fn deactivating_twice_says_it_changed_nothing() {
    let b = boot().await;
    let slug = b.tenant().await;
    b.post(&format!("/orgs/{slug}/deactivate"), "").await;

    let resp = b.post(&format!("/orgs/{slug}/deactivate"), "").await;
    let location = resp
        .headers()
        .get("location")
        .expect("redirect")
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        location.contains("already"),
        "a no-op should say so: {location}"
    );
}

/// The typed slug is the pause between intent and an unrecoverable act.
#[tokio::test]
async fn a_mistyped_confirmation_destroys_nothing() {
    let b = boot().await;
    let slug = b.tenant().await;

    for body in [
        "confirm=&purge_database=on",
        "confirm=wrong&purge_database=on",
        &format!("confirm={}X&purge_database=on", slug),
    ] {
        let resp = b.post(&format!("/orgs/{slug}/purge"), body).await;
        let location = resp
            .headers()
            .get("location")
            .expect("redirect")
            .to_str()
            .unwrap()
            .to_owned();
        assert!(
            location.contains("error="),
            "`{body}` should be refused: {location}"
        );
        assert!(
            b.org(&slug).await.is_some(),
            "and the tenant must survive `{body}`"
        );
    }
}

/// Dropping a whole database is a bigger act than dropping a schema, so
/// it takes a second, explicit yes.
#[tokio::test]
async fn purging_a_database_mode_tenant_needs_the_explicit_flag() {
    let b = boot().await;
    let slug = b.tenant().await;

    let resp = b
        .post(&format!("/orgs/{slug}/purge"), &format!("confirm={slug}"))
        .await;
    let location = resp
        .headers()
        .get("location")
        .expect("redirect")
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        location.contains("error="),
        "should be refused without the flag: {location}"
    );
    assert!(
        location.contains("unrecoverable") || location.contains("database-mode"),
        "and say why: {location}"
    );
    assert!(b.org(&slug).await.is_some(), "the tenant must survive");
}

#[tokio::test]
async fn purging_removes_the_tenant() {
    let b = boot().await;
    let slug = b.tenant().await;

    let resp = b
        .post(
            &format!("/orgs/{slug}/purge"),
            &format!("confirm={slug}&purge_database=on"),
        )
        .await;
    let location = resp
        .headers()
        .get("location")
        .expect("redirect")
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        location.contains("notice="),
        "should report what it did: {location}"
    );
    assert!(
        b.org(&slug).await.is_none(),
        "the registry row should be gone"
    );
}

#[tokio::test]
async fn purging_a_tenant_that_does_not_exist_says_so() {
    let b = boot().await;
    let resp = b
        .post(
            "/orgs/no-such-tenant/purge",
            "confirm=no-such-tenant&purge_database=on",
        )
        .await;
    let location = resp
        .headers()
        .get("location")
        .expect("redirect")
        .to_str()
        .unwrap()
        .to_owned();
    assert!(location.contains("error="), "{location}");
    assert!(
        location.contains("no%20tenant") || location.contains("no+tenant"),
        "{location}"
    );
}

#[tokio::test]
async fn the_tenant_page_offers_both_and_explains_the_difference() {
    let b = boot().await;
    let slug = b.tenant().await;
    let html = body_of(b.get(&format!("/orgs/{slug}/edit")).await).await;

    assert!(
        html.contains(&format!("/orgs/{slug}/deactivate")),
        "should offer deactivate: {html}"
    );
    assert!(
        html.contains(&format!("/orgs/{slug}/purge")),
        "and purge: {html}"
    );
    assert!(
        html.contains("no undo"),
        "and say that purging cannot be undone: {html}"
    );
}

#[tokio::test]
async fn the_routes_require_a_session() {
    let b = boot().await;
    let slug = b.tenant().await;

    for (uri, body) in [
        (format!("/orgs/{slug}/deactivate"), String::new()),
        (
            format!("/orgs/{slug}/purge"),
            format!("confirm={slug}&purge_database=on"),
        ),
    ] {
        let resp = b
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(&uri)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            resp.status().is_redirection() || resp.status() == StatusCode::UNAUTHORIZED,
            "POST {uri} was reachable without a session: {}",
            resp.status()
        );
    }

    let org = b.org(&slug).await.expect("the tenant must survive");
    assert!(org.active, "and must not have been deactivated");
}
