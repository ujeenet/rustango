#![cfg(all(feature = "sqlite", feature = "tenancy"))]
//! Creating a tenant from the operator console (#1322), end to end
//! through the real router.
//!
//! SQLite, because the console is tri-dialect and this needs no server.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use rustango::sql::{sqlx, Auto, FetcherPool as _};
use rustango::tenancy::operator_console::{router_with_provisioning, SessionSecret};
use rustango::tenancy::provision::Provisioner;
use rustango::tenancy::{Org, TenantPools};
use tower::ServiceExt;

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn unique(prefix: &str) -> String {
    format!(
        "{prefix}{}_{}",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::SeqCst)
    )
}

struct Booted {
    app: axum::Router,
    cookie: String,
    pools: Arc<TenantPools<sqlx::Sqlite>>,
    _tmp: tempfile::TempDir,
    _migrations: tempfile::TempDir,
}

async fn boot() -> Booted {
    let tmp = tempfile::tempdir().expect("tempdir");
    let url = format!("sqlite://{}?mode=rwc", tmp.path().join("reg.db").display());
    let pool = sqlx::SqlitePool::connect(&url).await.expect("connect");
    let pools = Arc::new(TenantPools::<sqlx::Sqlite>::new(pool));
    let migrations = tempfile::tempdir().expect("migrations dir");

    // Registry tables through the generated chain, as production does.
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
    assert!(
        login.status().is_redirection(),
        "login should redirect, got {}",
        login.status()
    );
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
        pools,
        _tmp: tmp,
        _migrations: migrations,
    }
}

/// Percent-encode a form value.
///
/// Local rather than `urlencoding::encode`: that crate is an optional
/// dependency not enabled by `sqlite,tenancy`, and widening a feature
/// set to spell one test fixture is the wrong trade.
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

async fn body_of(resp: axum::response::Response) -> String {
    String::from_utf8_lossy(&to_bytes(resp.into_body(), 4 * 1024 * 1024).await.unwrap())
        .into_owned()
}

#[tokio::test]
async fn the_create_form_renders_for_an_authenticated_operator() {
    let b = boot().await;
    let resp = b
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/orgs/new")
                .header("cookie", &b.cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_of(resp).await;
    assert!(html.contains("name=\"slug\""), "no slug field: {html}");
    assert!(html.contains("Test connection"), "no probe button");
    // Schema mode is Postgres-only; a SQLite registry must not offer
    // a choice that can only fail.
    assert!(
        !html.contains("value=\"schema\""),
        "schema mode offered on a sqlite registry"
    );
}

/// Unauthenticated requests never reach the form.
#[tokio::test]
async fn the_create_routes_require_a_session() {
    let b = boot().await;
    for (method, uri) in [("GET", "/orgs/new"), ("POST", "/orgs/new")] {
        let resp = b
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::empty())
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
}

/// The probe reports the taxonomy, writes nothing, and does not leak
/// the password back into the page.
#[tokio::test]
async fn test_connection_reports_why_and_creates_nothing() {
    let b = boot().await;
    let resp = b
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/orgs/test-connection")
                .header("cookie", &b.cookie)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(
                    "database_url=postgres%3A%2F%2Fu%3Ahunter2%40127.0.0.1%3A59417%2Fnope",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_of(resp).await;
    assert!(
        html.contains("probe-bad"),
        "an unusable URL should report as bad: {html}"
    );
    // The endpoint is echoed so the operator can see *what* was tried,
    // and the password is not.
    assert!(html.contains("127.0.0.1:59417"), "should name the endpoint");
    assert!(!html.contains("hunter2"), "password leaked into the page");
    // The fault taxonomy itself (unreachable / auth / no-such-database
    // / wrong-server) is covered per dialect in `preflight_live`. This
    // build has no `postgres` feature, so a `postgres://` URL is
    // refused at the feature check rather than at connect — asserting
    // a specific fault here would be asserting the build's feature
    // set, not the console's behaviour.

    let orgs: Vec<Org> = Org::objects()
        .fetch(&b.pools.registry_pool())
        .await
        .unwrap();
    assert!(orgs.is_empty(), "a probe must not create a tenant");
}

/// The whole path: submit the form, get redirected to the run, and
/// find the tenant live.
#[tokio::test]
async fn submitting_the_form_provisions_a_tenant_and_redirects_to_its_run() {
    let b = boot().await;
    let tenant_db = b._tmp.path().join("acme.db");
    let slug = unique("acme");
    let form = format!(
        "slug={slug}&storage_mode=database&backend_kind=sqlite&database_url={}",
        form_encode(&format!("sqlite://{}?mode=rwc", tenant_db.display()))
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
        "expected a redirect to the run, got {} — body: {}",
        resp.status(),
        body_of(resp).await
    );
    let location = resp
        .headers()
        .get("location")
        .expect("redirect target")
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        location.starts_with("provision/"),
        "should land on the run: {location}"
    );

    // The tenant exists and is live — activation is the last step.
    let orgs: Vec<Org> = Org::objects()
        .fetch(&b.pools.registry_pool())
        .await
        .unwrap();
    assert_eq!(orgs.len(), 1);
    assert_eq!(orgs[0].slug, slug);
    assert!(orgs[0].active, "a successful run ends with the tenant live");

    // The run view renders its step log.
    let run_uri = format!("/orgs/{location}");
    let view = b
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri(&run_uri)
                .header("cookie", &b.cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(view.status(), StatusCode::OK);
    let html = body_of(view).await;
    for step in ["validate", "check_connection", "register_org", "activate"] {
        assert!(html.contains(step), "run view missing `{step}`: {html}");
    }
    assert!(html.contains("succeeded"), "run should read as succeeded");
}

/// A bad submission comes back as the form with the reason, not a 500
/// and not a half-made tenant.
#[tokio::test]
async fn a_bad_submission_re_renders_the_form_with_the_reason() {
    let b = boot().await;
    let resp = b
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/orgs/new")
                .header("cookie", &b.cookie)
                .header("content-type", "application/x-www-form-urlencoded")
                // Database mode with no URL.
                .body(Body::from("slug=nourl&storage_mode=database"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK, "re-render, not a redirect");
    let html = body_of(resp).await;
    assert!(html.contains("--database-url"), "should say what is wrong");
    assert!(
        html.contains("value=\"nourl\""),
        "the operator's input should survive the round-trip"
    );

    let orgs: Vec<Org> = Org::objects()
        .fetch(&b.pools.registry_pool())
        .await
        .unwrap();
    assert!(orgs.is_empty(), "nothing should have been created");
}

/// The stream replays a finished run and **terminates**. A stream that
/// never closes is a connection leak and leaves the browser unable to
/// tell it is watching something already over.
#[tokio::test]
async fn the_stream_replays_a_finished_run_and_then_ends() {
    let b = boot().await;
    let tenant_db = b._tmp.path().join("streamed.db");
    let slug = unique("streamed");
    let form = format!(
        "slug={slug}&storage_mode=database&backend_kind=sqlite&database_url={}",
        form_encode(&format!("sqlite://{}?mode=rwc", tenant_db.display()))
    );
    let created = b
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
    let location = created
        .headers()
        .get("location")
        .expect("redirect")
        .to_str()
        .unwrap()
        .to_owned();

    let resp = b
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/orgs/{location}/stream"))
                .header("cookie", &b.cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );

    // Collecting the body to completion only returns if the stream
    // actually ends — which is the assertion.
    let body = tokio::time::timeout(std::time::Duration::from_secs(20), body_of(resp))
        .await
        .expect("the stream must terminate on a finished run");

    assert!(body.contains("event: step"), "no step events: {body}");
    assert!(body.contains("event: done"), "stream did not signal done");
    // Every event carries its `seq` as the SSE id — that is what
    // `Last-Event-ID` resumes from.
    assert!(body.contains("id: 1"), "events must be addressable: {body}");
    assert!(body.contains("activate"), "the last step should be there");
}

/// `?after=` skips what the page already rendered, so a reload does
/// not duplicate the log.
#[tokio::test]
async fn the_stream_resumes_from_a_given_seq() {
    let b = boot().await;
    let tenant_db = b._tmp.path().join("resume.db");
    let slug = unique("resume");
    let form = format!(
        "slug={slug}&storage_mode=database&backend_kind=sqlite&database_url={}",
        form_encode(&format!("sqlite://{}?mode=rwc", tenant_db.display()))
    );
    let created = b
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
    let location = created
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();

    let resp = b
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/orgs/{location}/stream?after=3"))
                .header("cookie", &b.cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = tokio::time::timeout(std::time::Duration::from_secs(20), body_of(resp))
        .await
        .expect("stream terminates");

    // Parse the ids rather than substring-matching them: `id: 1` is a
    // prefix of `id: 14`, so a `contains` check passes on a stream
    // that did replay the beginning. (It did here, and the assertion
    // said the opposite of what it meant.)
    let ids: Vec<i64> = body
        .lines()
        .filter_map(|l| l.strip_prefix("id: "))
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    assert!(!ids.is_empty(), "no events at all: {body}");
    assert_eq!(
        ids.iter().min().copied(),
        Some(4),
        "resume must start after the given seq, got {ids:?}"
    );
    assert!(
        ids.windows(2).all(|w| w[0] < w[1]),
        "events must arrive in order: {ids:?}"
    );
}

/// Without a provisioner the routes are simply absent — the console
/// built the old way cannot create tenants at all.
#[tokio::test]
async fn the_routes_do_not_exist_without_a_provisioner() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let url = format!("sqlite://{}?mode=rwc", tmp.path().join("reg.db").display());
    let pool = sqlx::SqlitePool::connect(&url).await.expect("connect");
    let pools = Arc::new(TenantPools::<sqlx::Sqlite>::new(pool));
    let app = rustango::tenancy::operator_console::router_with_pools(
        pools.registry_pool(),
        pools.clone(),
        SessionSecret::from_env_or_random(),
    );

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/orgs/new")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "provisioning must be opt-in"
    );
}
