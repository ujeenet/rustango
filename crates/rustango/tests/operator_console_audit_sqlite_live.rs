#![cfg(all(feature = "sqlite", feature = "tenancy"))]
//! Reading the registry audit trail from the operator console.
//!
//! Every console mutation already wrote here and nothing read it back,
//! so the record an operator wants during an incident existed and was
//! reachable only with a SQL client. These tests cover the page that
//! closed that, and the two things a history view has to get right:
//! the newest entry first, and a page past the end that is not a dead
//! end.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use rustango::sql::{sqlx, Auto};
use rustango::tenancy::operator_console::{router, SessionSecret};
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

    // `router()` deliberately — the audit page is read-only and must be
    // there even on a console that cannot change anything, which is
    // exactly the deployment where "what happened?" still needs
    // answering.
    let app = router(registry.clone(), SessionSecret::from_env_or_random());

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

    /// Write one entry straight to the trail, the way the console's own
    /// handlers do.
    async fn record(&self, table: &'static str, pk: &str, verb: &str, extra: (&str, &str)) {
        let mut changes = serde_json::Map::new();
        changes.insert(
            extra.0.to_owned(),
            serde_json::Value::String(extra.1.to_owned()),
        );
        let entry = rustango::audit::PendingEntry {
            entity_table: table,
            entity_pk: pk.to_owned(),
            operation: rustango::audit::AuditOp::Action,
            source: rustango::audit::AuditSource::Custom(format!("operator:1:{verb}")),
            changes: serde_json::Value::Object(changes),
        };
        rustango::audit::emit_one_pool(&self.registry, &entry)
            .await
            .expect("record an audit entry");
    }
}

#[tokio::test]
async fn the_page_shows_what_the_console_recorded() {
    let b = boot().await;
    b.record(
        "rustango_orgs",
        "acme",
        "host_add",
        ("hostname", "shop.acme.test"),
    )
    .await;

    let resp = b.get("/audit").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_of(resp).await;

    assert!(html.contains("host_add"), "the verb: {html}");
    assert!(html.contains("acme"), "the subject: {html}");
    assert!(
        html.contains("shop.acme.test"),
        "and the detail from `changes`: {html}"
    );
    // `operator:1:host_add` is split so the reader is not parsing the
    // same string on every row.
    assert!(html.contains("operator 1"), "the actor, readably: {html}");
}

#[tokio::test]
async fn the_newest_entry_is_first() {
    let b = boot().await;
    b.record("rustango_orgs", "first", "host_add", ("n", "1"))
        .await;
    b.record("rustango_orgs", "second", "host_remove", ("n", "2"))
        .await;

    let html = body_of(b.get("/audit").await).await;
    let first_at = html.find("first").expect("both rows present");
    let second_at = html.find("second").expect("both rows present");
    assert!(
        second_at < first_at,
        "newest first: `second` should appear above `first`"
    );
}

#[tokio::test]
async fn filtering_narrows_to_one_record() {
    let b = boot().await;
    b.record("rustango_orgs", "acme", "edit", ("k", "v")).await;
    b.record("rustango_operators", "7", "operator_create", ("k", "v"))
        .await;

    let orgs_only = body_of(b.get("/audit?entity_table=rustango_orgs").await).await;
    assert!(orgs_only.contains("edit"), "{orgs_only}");
    assert!(
        !orgs_only.contains("operator_create"),
        "the operator row should be filtered out: {orgs_only}"
    );

    let one_record = body_of(
        b.get("/audit?entity_table=rustango_operators&entity_pk=7")
            .await,
    )
    .await;
    assert!(one_record.contains("operator_create"), "{one_record}");
    assert!(
        !one_record.contains(">edit<"),
        "the org row should be filtered out: {one_record}"
    );
}

/// A blank filter box posts `?entity_table=`, which must mean "no
/// filter" rather than "match the empty string" — otherwise submitting
/// the form with nothing typed returns nothing.
#[tokio::test]
async fn blank_filters_are_ignored_rather_than_matched() {
    let b = boot().await;
    b.record("rustango_orgs", "acme", "edit", ("k", "v")).await;

    let html = body_of(
        b.get("/audit?entity_table=&entity_pk=%20%20&operation=")
            .await,
    )
    .await;
    assert!(
        html.contains("edit"),
        "blank boxes should not filter everything out: {html}"
    );
}

/// `Paginator::get_page` clamps an out-of-range page to the last real
/// one, so there is no empty page to be stranded on.
#[tokio::test]
async fn a_page_past_the_end_clamps_to_real_data() {
    let b = boot().await;
    b.record("rustango_orgs", "acme", "edit", ("k", "v")).await;

    let html = body_of(b.get("/audit?page=4").await).await;
    assert!(
        html.contains("edit"),
        "should clamp to the page that has the data: {html}"
    );
    assert!(
        !html.contains("Nothing recorded yet"),
        "and must not claim the log is empty: {html}"
    );
}

#[tokio::test]
async fn paging_keeps_the_active_filter() {
    let b = boot().await;
    b.record("rustango_orgs", "acme", "edit", ("k", "v")).await;

    let html = body_of(b.get("/audit?page=2&entity_table=rustango_orgs").await).await;
    assert!(
        html.contains("entity_table=rustango_orgs"),
        "moving pages must not silently drop the filter: {html}"
    );
}

#[tokio::test]
async fn an_empty_log_says_so_plainly() {
    let b = boot().await;
    let html = body_of(b.get("/audit").await).await;
    assert!(
        html.contains("Nothing recorded yet"),
        "a fresh registry should say the log is empty: {html}"
    );
}

/// `changes` carries operator-supplied text — a tenant's
/// `display_name` is free-form — so it reaches the page as data.
#[tokio::test]
async fn audited_content_is_escaped() {
    let b = boot().await;
    b.record(
        "rustango_orgs",
        "acme",
        "edit",
        ("display_name", "<img src=x onerror=alert(1)>"),
    )
    .await;

    let html = body_of(b.get("/audit").await).await;
    assert!(
        !html.contains("<img src=x onerror="),
        "must not render as markup: {html}"
    );
    assert!(
        html.contains("&lt;img src=x onerror="),
        "should be escaped and visible: {html}"
    );
}

/// `?page=1000000000000000000` parses into an `i64` and then overflowed
/// `(page - 1) * PAGE_SIZE`, panicking the worker in debug and wrapping
/// to a negative offset in release. Clamped, it is just an empty page.
#[tokio::test]
async fn an_absurd_page_number_does_not_overflow_the_offset() {
    let b = boot().await;
    for page in [
        "1000000000000000000",
        "9223372036854775807", // i64::MAX
        "4611686018427387904",
    ] {
        let resp = b.get(&format!("/audit?page={page}")).await;
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "page={page} should render an empty page, not fail"
        );
    }
}

#[tokio::test]
async fn a_page_number_that_is_not_a_number_is_refused_cleanly() {
    let b = boot().await;
    for bad in ["abc", "99999999999999999999", "1%20OR%201=1"] {
        let resp = b.get(&format!("/audit?page={bad}")).await;
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "page={bad} should be a 400"
        );
    }
}

#[tokio::test]
async fn the_page_requires_a_session() {
    let b = boot().await;
    let resp = b
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/audit")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        resp.status().is_redirection() || resp.status() == StatusCode::UNAUTHORIZED,
        "the audit log was readable without a session: {}",
        resp.status()
    );
}

/// The nav is the only route to this page.
#[tokio::test]
async fn the_console_links_to_the_audit_log() {
    let b = boot().await;
    let html = body_of(b.get("/orgs").await).await;
    assert!(
        html.contains("href=\"/audit\""),
        "the layout nav should link to the audit log: {html}"
    );
}
