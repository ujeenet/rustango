#![cfg(all(feature = "sqlite", feature = "tenancy"))]
//! Running tenant migrations from the console.
//!
//! The work is spawned, so the only place a failure can land is the
//! run — these check that it does, rather than vanishing with the task.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use rustango::core::Column as _;
use rustango::sql::{sqlx, Auto, FetcherPool as _};
use rustango::tenancy::operator_console::{router_with_provisioning, SessionSecret};
use rustango::tenancy::provision::Provisioner;
use rustango::tenancy::provision_store::{self as store, ProvisioningRun, RunKind, RunState};
use rustango::tenancy::{Operator, TenantPools};
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

    /// The run a migrate POST redirected to.
    fn run_id(resp: &axum::response::Response) -> i64 {
        resp.headers()
            .get("location")
            .expect("a run to watch")
            .to_str()
            .unwrap()
            .rsplit('/')
            .next()
            .unwrap()
            .parse()
            .expect("numeric run id")
    }

    /// The work is spawned, so wait for the run to reach a terminal
    /// state rather than assuming it already has.
    async fn settled(&self, run_id: i64) -> ProvisioningRun {
        for _ in 0..200 {
            let run = store::run_by_id(&self.registry, run_id)
                .await
                .unwrap()
                .expect("the run exists");
            if RunState::parse(&run.state).is_terminal() {
                return run;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("run {run_id} never finished");
    }
}

/// A tenant to migrate, provisioned through the console so its storage
/// really exists.
async fn a_tenant(b: &Booted) -> String {
    let slug = unique("mig");
    let db = b._tmp.path().join(format!("{slug}.db"));
    let form = format!(
        "slug={slug}&storage_mode=database&backend_kind=sqlite&database_url=sqlite%3A%2F%2F{}%3Fmode%3Drwc",
        db.display().to_string().replace('/', "%2F")
    );
    let resp = b
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/orgs/new")
                .header("cookie", &b.cookie)
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

#[tokio::test]
async fn migrating_one_tenant_records_a_migrate_run() {
    let b = boot().await;
    let slug = a_tenant(&b).await;

    let resp = b.post(&format!("/orgs/{slug}/migrate")).await;
    assert!(resp.status().is_redirection(), "should point at a run");
    let run = b.settled(Booted::run_id(&resp)).await;

    assert_eq!(RunKind::parse(&run.kind), RunKind::Migrate);
    assert_eq!(run.slug, slug, "a single-tenant run names its tenant");
    assert_eq!(
        RunState::parse(&run.state),
        RunState::Succeeded,
        "error: {:?}",
        run.error
    );
}

/// A batch spans tenants that share no storage mode or backend, so it
/// stores no slug — the events name each tenant instead.
#[tokio::test]
async fn migrating_everything_records_one_run_naming_each_tenant() {
    let b = boot().await;
    let first = a_tenant(&b).await;
    let second = a_tenant(&b).await;

    let resp = b.post("/orgs/migrate").await;
    let run_id = Booted::run_id(&resp);
    let run = b.settled(run_id).await;

    assert_eq!(RunKind::parse(&run.kind), RunKind::Migrate);
    assert!(run.slug.is_empty(), "a batch names no single tenant");
    assert_eq!(
        RunState::parse(&run.state),
        RunState::Succeeded,
        "error: {:?}",
        run.error
    );

    let events = store::events_since(&b.registry, run_id, 0).await.unwrap();
    let narrative: String = events.iter().map(|e| e.message.clone()).collect();
    assert!(
        narrative.contains(&first),
        "should name {first}: {narrative}"
    );
    assert!(
        narrative.contains(&second),
        "should name {second}: {narrative}"
    );
    assert!(
        events.iter().any(|e| e.step == "plan"),
        "and open with a plan: {events:?}"
    );
}

/// The work is spawned. An error has nowhere to go but the run, so it
/// has to get there.
#[tokio::test]
async fn a_failure_lands_on_the_run_rather_than_vanishing() {
    let b = boot().await;
    let resp = b.post("/orgs/no-such-tenant/migrate").await;
    assert!(
        resp.status().is_redirection(),
        "even a doomed migrate gets a run to look at"
    );

    let run = b.settled(Booted::run_id(&resp)).await;
    assert_eq!(RunState::parse(&run.state), RunState::Failed);
    assert!(
        run.error
            .as_deref()
            .unwrap_or_default()
            .contains("no-such-tenant"),
        "the reason should name the tenant: {:?}",
        run.error
    );
}

#[tokio::test]
async fn the_run_view_says_migrating_rather_than_provisioning() {
    let b = boot().await;
    let slug = a_tenant(&b).await;
    let resp = b.post(&format!("/orgs/{slug}/migrate")).await;
    let run_id = Booted::run_id(&resp);
    b.settled(run_id).await;

    let html = body_of(b.get(&format!("/orgs/provision/{run_id}")).await).await;
    assert!(html.contains("Migrating"), "the heading: {html}");
    assert!(
        !html.contains("<h1>Provisioning"),
        "and not the provisioning one: {html}"
    );
}

#[tokio::test]
async fn the_run_index_marks_a_migrate_run() {
    let b = boot().await;
    b.settled(Booted::run_id(&b.post("/orgs/migrate").await))
        .await;

    let html = body_of(b.get("/orgs/provision").await).await;
    assert!(
        html.contains("all tenants"),
        "a batch reads as such: {html}"
    );
    assert!(
        html.contains("migrate"),
        "and is marked as a migrate run: {html}"
    );
}

#[tokio::test]
async fn the_console_links_to_both_migrate_actions() {
    let b = boot().await;
    let slug = a_tenant(&b).await;

    let orgs = body_of(b.get("/orgs").await).await;
    assert!(
        orgs.contains("/orgs/migrate"),
        "the org list should offer a batch migrate: {orgs}"
    );

    let edit = body_of(b.get(&format!("/orgs/{slug}/edit")).await).await;
    assert!(
        edit.contains(&format!("/orgs/{slug}/migrate")),
        "and a tenant's page should offer its own: {edit}"
    );
}

#[tokio::test]
async fn the_migrate_routes_require_a_session() {
    let b = boot().await;
    for uri in ["/orgs/migrate", "/orgs/acme/migrate"] {
        let resp = b
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::empty())
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

    let runs: Vec<ProvisioningRun> = ProvisioningRun::objects()
        .where_(ProvisioningRun::kind.eq(RunKind::Migrate.as_str().to_owned()))
        .fetch(&b.registry)
        .await
        .unwrap();
    assert!(
        runs.is_empty(),
        "an unauthenticated post must not start a migration: {runs:?}"
    );
}
