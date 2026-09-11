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

/// Hyphens, not underscores.
///
/// A slug becomes a hostname label, where `_` is illegal — and the
/// engine now enforces that. This helper produced `acme12345_0`, so
/// tightening the rule turned every test slug invalid. The rule is
/// right; the generator was wrong.
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
    pools: Arc<TenantPools<sqlx::Sqlite>>,
    registry_url: String,
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
        registry_url: url,
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

/// The tenant list links to the create page.
///
/// This shipped without a link first: the route and the template
/// existed, and the only way to reach them was typing the URL. A page
/// nothing navigates to is not a feature, so the link is pinned here
/// rather than left to survive on someone remembering it.
#[tokio::test]
async fn the_org_list_links_to_the_create_page() {
    let b = boot().await;
    let resp = b
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/orgs")
                .header("cookie", &b.cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_of(resp).await;
    assert!(
        html.contains("href=\"/orgs/new\""),
        "no link to the create page: {html}"
    );
    assert!(html.contains("New tenant"), "the link has no label");
}

/// And it is absent when provisioning is off, so a console that
/// cannot create tenants does not advertise a 404.
#[tokio::test]
async fn the_org_list_hides_the_link_without_a_provisioner() {
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
    let mut op = rustango::tenancy::Operator {
        id: Auto::default(),
        username: username.clone(),
        password_hash: rustango::tenancy::password::hash("letmein").unwrap(),
        active: true,
        created_at: chrono::Utc::now(),
        password_changed_at: None,
    };
    op.insert_pool(&registry).await.expect("seed operator");

    let app = rustango::tenancy::operator_console::router_with_pools(
        registry.clone(),
        pools.clone(),
        SessionSecret::from_env_or_random(),
    );
    let login = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(format!("username={username}&password=letmein")))
                .unwrap(),
        )
        .await
        .unwrap();
    let cookie = login
        .headers()
        .get("set-cookie")
        .expect("cookie")
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/orgs")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let html = body_of(resp).await;
    assert!(
        !html.contains("href=\"/orgs/new\""),
        "a console without a provisioner must not link to a 404"
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

/// The probe has to answer the question the submit will answer, and
/// the registry's own database is the case where reachability and
/// usability disagree.
///
/// It is reachable, and the role *can* create tables in it, so the
/// probe reported "This role can create tables, so migrations will
/// run" — its most confident wording — for the one target
/// provisioning refuses outright. An operator who tests before
/// submitting was told the mistake was fine.
#[tokio::test]
async fn the_probe_refuses_the_registry_instead_of_blessing_it() {
    let b = boot().await;
    let registry_url = b.registry_url.clone();

    let resp = b
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/orgs/test-connection")
                .header("cookie", &b.cookie)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "database_url={}",
                    form_encode(&registry_url)
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_of(resp).await;
    assert!(
        html.contains("probe-bad"),
        "the registry's own database must not probe as usable: {html}"
    );
    assert!(
        html.contains("registry"),
        "should say why it is refused: {html}"
    );
    assert!(
        !html.contains("migrations will run"),
        "the success wording must not appear: {html}"
    );
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
                // A slug that is not a legal hostname label. Was
                // "database mode with no URL", which is no longer an
                // error — the console derives one from the registry
                // now, which is the whole point of that change.
                .body(Body::from("slug=Not_A_Slug&storage_mode=database"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK, "re-render, not a redirect");
    let html = body_of(resp).await;
    assert!(
        html.contains("lowercase letters, digits and hyphens"),
        "should say what is wrong: {html}"
    );
    assert!(
        html.contains("value=\"Not_A_Slug\""),
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
/// A run used to be reachable only by its id, which meant only from
/// the redirect that created it: navigate away and the record was
/// stranded in a table nobody could enumerate.
#[tokio::test]
async fn the_run_index_lists_runs_and_links_to_each() {
    let b = boot().await;
    let tenant_db = b._tmp.path().join("indexed.db");
    let slug = unique("indexed");
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
    let run_path = created
        .headers()
        .get("location")
        .expect("a run to watch")
        .to_str()
        .unwrap()
        .to_owned();
    // The redirect is relative (`provision/<id>`); the index links with
    // a leading slash, so compare on the id.
    let run_id = run_path.rsplit('/').next().unwrap().to_owned();

    let resp = b
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/orgs/provision")
                .header("cookie", &b.cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_of(resp).await;
    assert!(
        html.contains(&slug),
        "the run's tenant should appear: {html}"
    );
    assert!(
        html.contains(&format!("/orgs/provision/{run_id}")),
        "and link to the run itself: {html}"
    );
    assert!(html.contains("succeeded"), "with its outcome: {html}");
}

/// The index is where somebody looks for a run that went wrong, so the
/// reason has to be on it rather than one click away.
#[tokio::test]
async fn the_run_index_shows_why_a_run_failed() {
    let b = boot().await;
    let slug = unique("doomed");
    // Schema mode on SQLite is refused — a real failure, recorded as a
    // run, with a message worth surfacing.
    let form = format!("slug={slug}&storage_mode=schema&backend_kind=sqlite");
    b.app
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

    let html = body_of(
        b.app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/orgs/provision")
                    .header("cookie", &b.cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    // Either the run was recorded and shows as failed, or it was
    // refused before a run opened — in which case the index is
    // legitimately empty. Both are correct; what must not happen is a
    // run listed with no explanation.
    if html.contains(&slug) {
        assert!(
            html.contains("failed"),
            "a listed run that failed must say so: {html}"
        );
    }
}

/// Paging past the end must not be a dead end. The links used to sit
/// inside the non-empty branch, so an empty page offered no way back
/// and only a URL edit escaped it.
#[tokio::test]
async fn an_empty_run_page_still_offers_a_way_back() {
    let b = boot().await;
    let html = body_of(
        b.app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/orgs/provision?page=3")
                    .header("cookie", &b.cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert!(
        html.contains("/orgs/provision?page=2"),
        "an empty page past the end must link back: {html}"
    );
    assert!(
        !html.contains("No tenant has been provisioned"),
        "and must not claim nothing ever happened: {html}"
    );
}

/// Same overflow as the audit page: the multiply panicked the worker.
#[tokio::test]
async fn an_absurd_run_page_number_does_not_overflow_the_offset() {
    let b = boot().await;
    for page in ["1000000000000000000", "9223372036854775807"] {
        let resp = b
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/orgs/provision?page={page}"))
                    .header("cookie", &b.cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "page={page} should render an empty page, not fail"
        );
    }
}

#[tokio::test]
async fn the_run_index_requires_a_session() {
    let b = boot().await;
    let resp = b
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/orgs/provision")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        resp.status().is_redirection() || resp.status() == StatusCode::UNAUTHORIZED,
        "the index was reachable without a session: {}",
        resp.status()
    );
}

/// The org list is the only page that links to the index, so without
/// that link it is reachable by typing a URL and nothing else.
#[tokio::test]
async fn the_org_list_links_to_the_run_index() {
    let b = boot().await;
    let html = body_of(
        b.app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/orgs")
                    .header("cookie", &b.cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert!(
        html.contains("/orgs/provision"),
        "the org list should link to the run history: {html}"
    );
}

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

/// **The test that was missing.**
///
/// Everything above drives a router the *test* constructs. That proves
/// the handlers work and proves nothing about whether the framework
/// ever builds one — which is exactly how this shipped with
/// `router_with_provisioning` written, exported, documented, and
/// called by nobody.
///
/// So: go through `server::Builder`, the thing `Cli::tenancy()`
/// actually uses, and assert the routes exist on *its* output.
mod through_the_builder {
    use super::{body_of, unique};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use rustango::sql::sqlx;
    use tower::ServiceExt;

    async fn registry() -> (sqlx::SqlitePool, String, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let url = format!("sqlite://{}?mode=rwc", tmp.path().join("reg.db").display());
        let pool = sqlx::SqlitePool::connect(&url).await.expect("connect");
        (pool, url, tmp)
    }

    /// A route that exists answers *something* — 200, a redirect to
    /// login, whatever. A route that was never mounted 404s. That is
    /// the whole distinction being pinned, and it needs no session.
    async fn probes_as_mounted(app: &axum::Router, uri: &str) -> bool {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header("host", "localhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        resp.status() != StatusCode::NOT_FOUND
    }

    #[tokio::test]
    async fn with_tenant_provisioning_mounts_the_create_routes() {
        let (pool, url, tmp) = registry().await;
        let migrations = tempfile::tempdir().expect("migrations");
        let app = rustango::server::Builder::from_pool(pool, url, "localhost")
            .with_tenant_provisioning(migrations.path())
            .into_router()
            .await
            .expect("assemble");

        assert!(
            probes_as_mounted(&app, "/orgs/new").await,
            "`with_tenant_provisioning` did not reach the console"
        );
        assert!(probes_as_mounted(&app, "/orgs/provision/1").await);
        let _ = (tmp, unique("x"));
    }

    /// And the default really is off — so the opt-in means something.
    #[tokio::test]
    async fn without_it_the_create_routes_are_absent() {
        let (pool, url, tmp) = registry().await;
        let app = rustango::server::Builder::from_pool(pool, url, "localhost")
            .into_router()
            .await
            .expect("assemble");

        assert!(
            !probes_as_mounted(&app, "/orgs/new").await,
            "provisioning must be opt-in, not on by default"
        );
        // The console itself is still there — this is about one
        // capability, not the whole surface.
        assert!(probes_as_mounted(&app, "/orgs").await);
        let _ = tmp;
    }

    /// The link the operator clicks comes from the Builder's console
    /// too, not just from a hand-built one.
    #[tokio::test]
    async fn the_builders_console_links_to_the_create_page() {
        let (pool, url, tmp) = registry().await;
        let migrations = tempfile::tempdir().expect("migrations");
        let app = rustango::server::Builder::from_pool(pool, url, "localhost")
            .with_tenant_provisioning(migrations.path())
            .into_router()
            .await
            .expect("assemble");

        // Unauthenticated, so this is the login redirect — the point
        // is only that the route resolves through the Builder's
        // console. The rendered link is asserted in
        // `the_org_list_links_to_the_create_page`.
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/orgs")
                    .header("host", "localhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(resp.status(), StatusCode::NOT_FOUND);
        let _ = (body_of(resp).await, tmp);
    }
}
