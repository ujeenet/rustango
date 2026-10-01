#![cfg(all(feature = "sqlite", feature = "tenancy", feature = "webhook"))]
//! Inbound provisioning webhook (#1323).
//!
//! The acceptance list, each one a way this goes wrong in production:
//! a forged or unsigned delivery is refused; the signature is checked
//! over the **raw bytes**; a replayed delivery outside the window is
//! refused; a retry returns the first run and creates **no second
//! tenant** — tested concurrently, because that is the real case; and
//! the run is visible exactly like a console-initiated one.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use rustango::sql::{sqlx, FetcherPool as _};
use rustango::tenancy::provision::Provisioner;
use rustango::tenancy::provision_store as store;
use rustango::tenancy::provision_webhook::{router, UrlPolicy, WebhookConfig, WebhookState};
use rustango::tenancy::{Org, TenantPools};
use rustango::webhook::{sign, SignatureFormat};
use tower::ServiceExt;

const SECRET: &[u8] = b"shared-secret-at-least-32-bytes-long";
static UNIQ: AtomicU64 = AtomicU64::new(0);

fn unique(prefix: &str) -> String {
    format!(
        "{prefix}{}-{}",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::SeqCst)
    )
}

struct Booted {
    app: axum::Router,
    pools: Arc<TenantPools<sqlx::Sqlite>>,
    tmp: tempfile::TempDir,
    _migrations: tempfile::TempDir,
}

async fn boot_with(policy: UrlPolicy) -> Booted {
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

    let provisioner = Provisioner::new(pools.clone(), url.clone(), migrations.path()).erased();
    let config = WebhookConfig {
        url_policy: policy,
        ..WebhookConfig::new(SECRET.to_vec(), "")
    };
    let app = router(
        "/hooks/provision",
        WebhookState {
            config,
            provisioner,
        },
    );

    Booted {
        app,
        pools,
        tmp,
        _migrations: migrations,
    }
}

/// Template policy pointing at sqlite files under `dir`.
async fn boot(dir_hint: &tempfile::TempDir) -> Booted {
    let template = format!(
        "sqlite://{}/t_{{slug}}.db?mode=rwc",
        dir_hint.path().display()
    );
    boot_with(UrlPolicy::Template(template)).await
}

fn body_for(slug: &str, event_id: &str, ts: i64) -> String {
    serde_json::json!({
        "event_id": event_id,
        "timestamp": ts,
        "slug": slug,
    })
    .to_string()
}

fn signed(body: &str) -> String {
    sign(
        SignatureFormat::HexSha256WithPrefix,
        SECRET,
        body.as_bytes(),
    )
}

fn post(body: String, signature: Option<&str>) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri("/hooks/provision")
        .header("content-type", "application/json");
    if let Some(sig) = signature {
        b = b.header("x-signature-256", sig);
    }
    b.body(Body::from(body)).unwrap()
}

async fn json_of(resp: axum::response::Response) -> serde_json::Value {
    let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

/// Wait for the spawned provisioning task to finish, so assertions
/// about the tenant are not racing it.
async fn await_run(b: &Booted, run_id: i64) -> store::ProvisioningRun {
    let registry = b.pools.registry_pool();
    for _ in 0..200 {
        if let Ok(Some(run)) = store::run_by_id(&registry, run_id).await {
            if store::RunState::parse(&run.state).is_terminal() {
                return run;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("run {run_id} never reached a terminal state");
}

#[tokio::test]
async fn a_signed_delivery_creates_a_tenant() {
    let holder = tempfile::tempdir().expect("tenants dir");
    let b = boot(&holder).await;
    let slug = unique("acme");
    let body = body_for(&slug, "evt-create", chrono::Utc::now().timestamp());

    let resp = b
        .app
        .clone()
        .oneshot(post(body.clone(), Some(&signed(&body))))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "should accept and queue"
    );
    let json = json_of(resp).await;
    assert_eq!(json["duplicate"], serde_json::json!(false));
    let run_id = json["run_id"].as_i64().expect("a run id to poll");

    let run = await_run(&b, run_id).await;
    assert_eq!(
        store::RunState::parse(&run.state),
        store::RunState::Succeeded,
        "run failed: {:?}",
        run.error
    );

    let orgs: Vec<Org> = Org::objects()
        .fetch(&b.pools.registry_pool())
        .await
        .unwrap();
    assert_eq!(orgs.len(), 1);
    assert_eq!(orgs[0].slug, slug);
    assert!(orgs[0].active, "a successful run ends with the tenant live");
    // The template derived the URL — the caller never named a host.
    assert!(
        orgs[0]
            .database_url
            .as_deref()
            .is_some_and(|u| u.contains(&format!("t_{slug}.db"))),
        "url should come from the template: {:?}",
        orgs[0].database_url
    );
}

#[tokio::test]
async fn an_unsigned_or_forged_delivery_is_refused_and_creates_nothing() {
    let holder = tempfile::tempdir().expect("tenants dir");
    let b = boot(&holder).await;
    let body = body_for(
        &unique("forged"),
        "evt-forged",
        chrono::Utc::now().timestamp(),
    );

    for signature in [None, Some("sha256=deadbeef"), Some("garbage")] {
        let resp = b
            .app
            .clone()
            .oneshot(post(body.clone(), signature))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "signature {signature:?} should have been refused"
        );
    }

    let orgs: Vec<Org> = Org::objects()
        .fetch(&b.pools.registry_pool())
        .await
        .unwrap();
    assert!(orgs.is_empty(), "a refused delivery must create nothing");
    let runs: Vec<store::ProvisioningRun> = store::ProvisioningRun::objects()
        .fetch(&b.pools.registry_pool())
        .await
        .unwrap();
    assert!(runs.is_empty(), "a refused delivery must not open a run");
}

/// The signature covers the **exact bytes**. A body altered after
/// signing must fail even when it parses to something plausible — the
/// forgery hole a parse-then-re-serialize handler leaves open.
#[tokio::test]
async fn the_signature_is_checked_over_the_raw_bytes() {
    let holder = tempfile::tempdir().expect("tenants dir");
    let b = boot(&holder).await;
    let original = body_for("victim", "evt-tamper", chrono::Utc::now().timestamp());
    let signature = signed(&original);

    // Same JSON *document* in a different byte encoding (whitespace).
    let reformatted = serde_json::to_string_pretty(
        &serde_json::from_str::<serde_json::Value>(&original).unwrap(),
    )
    .unwrap();
    assert_ne!(original, reformatted, "the bytes must actually differ");

    let resp = b
        .app
        .clone()
        .oneshot(post(reformatted, Some(&signature)))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "a re-encoded body must not verify"
    );

    // And a genuinely swapped slug, obviously.
    let swapped = original.replace("victim", "attacker");
    let resp = b
        .app
        .clone()
        .oneshot(post(swapped, Some(&signature)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// A signature alone is replayable forever. The timestamp window is
/// what stops a captured delivery being re-sent next month.
#[tokio::test]
async fn a_stale_or_future_delivery_is_refused() {
    let holder = tempfile::tempdir().expect("tenants dir");
    let b = boot(&holder).await;
    let now = chrono::Utc::now().timestamp();

    for (label, ts) in [("stale", now - 3600), ("future", now + 3600)] {
        let body = body_for(&unique("replay"), &unique("evt"), ts);
        let resp = b
            .app
            .clone()
            .oneshot(post(body.clone(), Some(&signed(&body))))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "a {label} timestamp should be refused"
        );
    }
}

/// A retried delivery returns the first run and creates no second
/// tenant — the sequential case.
#[tokio::test]
async fn a_retried_delivery_returns_the_first_run() {
    let holder = tempfile::tempdir().expect("tenants dir");
    let b = boot(&holder).await;
    let slug = unique("retry");
    let body = body_for(&slug, "evt-retry", chrono::Utc::now().timestamp());
    let sig = signed(&body);

    let first = b
        .app
        .clone()
        .oneshot(post(body.clone(), Some(&sig)))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::ACCEPTED);
    let first_json = json_of(first).await;
    let run_id = first_json["run_id"].as_i64().unwrap();
    await_run(&b, run_id).await;

    let second = b.app.clone().oneshot(post(body, Some(&sig))).await.unwrap();
    assert_eq!(
        second.status(),
        StatusCode::OK,
        "a duplicate is not a fresh acceptance"
    );
    let second_json = json_of(second).await;
    assert_eq!(second_json["duplicate"], serde_json::json!(true));
    assert_eq!(
        second_json["run_id"], first_json["run_id"],
        "a retry must point at the original run"
    );

    let orgs: Vec<Org> = Org::objects()
        .fetch(&b.pools.registry_pool())
        .await
        .unwrap();
    assert_eq!(orgs.len(), 1, "a retry must not create a second tenant");
}

/// **The real case.** Two deliveries of one event arriving at once —
/// the check-then-insert window. The unique constraint on
/// `idempotency_key`, not the handler, is what has to win.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_simultaneous_deliveries_create_one_tenant() {
    let holder = tempfile::tempdir().expect("tenants dir");
    let b = boot(&holder).await;
    let slug = unique("racy");
    let body = body_for(&slug, "evt-race", chrono::Utc::now().timestamp());
    let sig = signed(&body);

    let handles: Vec<_> = (0..8)
        .map(|_| tokio::spawn(b.app.clone().oneshot(post(body.clone(), Some(&sig)))))
        .collect();
    let mut resps = Vec::new();
    for h in handles {
        resps.push(h.await.unwrap().unwrap());
    }

    // One accepted, the rest de-duplicated; the loser of the unique-key
    // race is a duplicate too, never a 500 (#1933).
    let statuses: Vec<_> = resps.iter().map(|r| r.status()).collect();
    assert_eq!(
        statuses
            .iter()
            .filter(|s| **s == StatusCode::ACCEPTED)
            .count(),
        1,
        "{statuses:?}"
    );
    assert!(
        statuses
            .iter()
            .all(|s| *s == StatusCode::ACCEPTED || *s == StatusCode::OK),
        "a racing delivery failed: {statuses:?}"
    );

    // Let whichever run started finish.
    for resp in resps {
        if let Some(id) = json_of(resp).await["run_id"].as_i64() {
            let _ =
                tokio::time::timeout(std::time::Duration::from_secs(10), await_run(&b, id)).await;
        }
    }

    let orgs: Vec<Org> = Org::objects()
        .fetch(&b.pools.registry_pool())
        .await
        .unwrap();
    assert!(
        orgs.len() <= 1,
        "two simultaneous deliveries created {} tenants",
        orgs.len()
    );
    let runs: Vec<store::ProvisioningRun> = store::ProvisioningRun::objects()
        .fetch(&b.pools.registry_pool())
        .await
        .unwrap();
    assert_eq!(runs.len(), 1, "one event, one run: {runs:?}");
}

/// The security decision, end to end: a caller-supplied `database_url`
/// is **refused**, not silently ignored.
#[tokio::test]
async fn a_caller_supplied_url_is_refused_under_the_default_policy() {
    let holder = tempfile::tempdir().expect("tenants dir");
    let b = boot(&holder).await;
    let body = serde_json::json!({
        "event_id": "evt-policy",
        "timestamp": chrono::Utc::now().timestamp(),
        "slug": "sneaky",
        "database_url": "postgres://attacker:pw@evil.example.com:5432/exfil",
    })
    .to_string();

    let resp = b
        .app
        .clone()
        .oneshot(post(body.clone(), Some(&signed(&body))))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "a signed-but-disallowed field must be refused, not dropped"
    );
    let json = json_of(resp).await;
    assert!(
        json["message"]
            .as_str()
            .is_some_and(|e| e.contains("derives")),
        "the refusal should say why: {json}"
    );

    let orgs: Vec<Org> = Org::objects()
        .fetch(&b.pools.registry_pool())
        .await
        .unwrap();
    assert!(orgs.is_empty());
}

/// A bad slug never reaches the provisioning engine.
#[tokio::test]
async fn a_dangerous_slug_is_refused_at_the_door() {
    let holder = tempfile::tempdir().expect("tenants dir");
    let b = boot(&holder).await;
    for bad in ["Acme", "ac me", "acme;DROP TABLE x", "../../etc/passwd"] {
        let body = body_for(bad, &unique("evt"), chrono::Utc::now().timestamp());
        let resp = b
            .app
            .clone()
            .oneshot(post(body.clone(), Some(&signed(&body))))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "slug `{bad}` should have been refused"
        );
    }
    let orgs: Vec<Org> = Org::objects()
        .fetch(&b.pools.registry_pool())
        .await
        .unwrap();
    assert!(orgs.is_empty());
}

/// A webhook-initiated run is an ordinary run: the same events, the
/// same shape the console renders.
#[tokio::test]
async fn the_run_is_visible_exactly_like_a_console_run() {
    let holder = tempfile::tempdir().expect("tenants dir");
    let b = boot(&holder).await;
    let body = body_for(
        &unique("visible"),
        "evt-visible",
        chrono::Utc::now().timestamp(),
    );
    let resp = b
        .app
        .clone()
        .oneshot(post(body.clone(), Some(&signed(&body))))
        .await
        .unwrap();
    let run_id = json_of(resp).await["run_id"].as_i64().unwrap();
    await_run(&b, run_id).await;

    let events = store::events_since(&b.pools.registry_pool(), run_id, 0)
        .await
        .expect("events");
    let steps: Vec<&str> = events.iter().map(|e| e.step.as_str()).collect();
    for expected in ["validate", "register_org", "migrate", "activate"] {
        assert!(steps.contains(&expected), "missing `{expected}`: {steps:?}");
    }
    // Dense and ordered, so the console's `Last-Event-ID` resume works
    // for a webhook run too.
    let seqs: Vec<i64> = events.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, (1..=seqs.len() as i64).collect::<Vec<_>>());

    // `requested_by` distinguishes it from an operator's own run.
    let run = store::run_by_id(&b.pools.registry_pool(), run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(run.requested_by.as_deref(), Some("webhook"));
    let _ = &b.tmp;
}

/// Backdate a run so it looks like its pod died two hours ago.
async fn age_run(b: &Booted, run_id: i64) {
    use rustango::core::Column as _;
    use rustango::sql::UpdaterPool as _;
    store::ProvisioningRun::objects()
        .where_(store::ProvisioningRun::id.eq(run_id))
        .update()
        .set(
            "started_at",
            chrono::Utc::now() - chrono::Duration::hours(2),
        )
        .execute_pool(&b.pools.registry_pool())
        .await
        .unwrap();
}

/// A run left `running` by a dead pod no longer answers every retry with
/// `duplicate: running` forever; the retry runs again (#1883).
#[tokio::test]
async fn a_retry_after_an_interrupted_run_provisions_again() {
    let holder = tempfile::tempdir().expect("tenants dir");
    let b = boot(&holder).await;
    let slug = unique("stuck");
    let registry = b.pools.registry_pool();
    let stuck = store::open_run(
        &registry,
        &slug,
        "database",
        "sqlite",
        None,
        Some("webhook"),
        Some("evt-stuck"),
    )
    .await
    .unwrap();
    let stuck_id = stuck.id.get().copied().unwrap();
    age_run(&b, stuck_id).await;

    let body = body_for(&slug, "evt-stuck", chrono::Utc::now().timestamp());
    let sig = signed(&body);
    let resp = b.app.clone().oneshot(post(body, Some(&sig))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let json = json_of(resp).await;
    assert_eq!(json["duplicate"], serde_json::json!(false));
    let run_id = json["run_id"].as_i64().unwrap();
    assert_ne!(run_id, stuck_id);
    assert_eq!(
        store::RunState::parse(&await_run(&b, run_id).await.state),
        store::RunState::Succeeded
    );
    let old = store::run_by_id(&registry, stuck_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(store::RunState::parse(&old.state), store::RunState::Failed);
}

/// A run still making progress is not taken over by a retry.
#[tokio::test]
async fn a_retry_during_a_live_run_is_still_a_duplicate() {
    let holder = tempfile::tempdir().expect("tenants dir");
    let b = boot(&holder).await;
    let slug = unique("live");
    let registry = b.pools.registry_pool();
    let live = store::open_run(
        &registry,
        &slug,
        "database",
        "sqlite",
        None,
        Some("webhook"),
        Some("evt-live"),
    )
    .await
    .unwrap();
    let body = body_for(&slug, "evt-live", chrono::Utc::now().timestamp());
    let sig = signed(&body);
    let resp = b.app.clone().oneshot(post(body, Some(&sig))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = json_of(resp).await;
    assert_eq!(json["duplicate"], serde_json::json!(true));
    assert_eq!(json["run_id"].as_i64(), live.id.get().copied());
}

/// Insert the inactive Org a run left behind, then fail that run.
async fn half_made(
    b: &Booted,
    holder: &tempfile::TempDir,
    slug: &str,
    key: &str,
    own: bool,
) -> i64 {
    let registry = b.pools.registry_pool();
    let run = store::open_run(&registry, slug, "database", "sqlite", None, None, Some(key))
        .await
        .unwrap();
    let run_id = run.id.get().copied().unwrap();
    let mut org = Org {
        slug: slug.to_owned(),
        display_name: slug.to_owned(),
        backend_kind: "sqlite".into(),
        database_url: Some(format!(
            "sqlite://{}/t_{slug}.db?mode=rwc",
            holder.path().display()
        )),
        active: false,
        ..rustango::testkit::org()
    };
    org.save_pool(&registry).await.unwrap();
    if own {
        store::attach_org(&registry, run_id, org.id.get().copied().unwrap())
            .await
            .unwrap();
    }
    store::finish_run(&registry, run_id, store::RunState::Failed, Some("pod died"))
        .await
        .unwrap();
    org.id.get().copied().unwrap()
}

/// A retry picks up the tenant its failed run left inactive, instead of
/// failing on "slug already exists" (#1883).
#[tokio::test]
async fn a_retry_resumes_the_tenant_a_failed_run_left_behind() {
    let holder = tempfile::tempdir().expect("tenants dir");
    let b = boot(&holder).await;
    let slug = unique("resume");
    let _ = half_made(&b, &holder, &slug, "evt-resume", true).await;

    let body = body_for(&slug, "evt-resume", chrono::Utc::now().timestamp());
    let sig = signed(&body);
    let resp = b.app.clone().oneshot(post(body, Some(&sig))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let run_id = json_of(resp).await["run_id"].as_i64().unwrap();
    let run = await_run(&b, run_id).await;
    assert_eq!(
        store::RunState::parse(&run.state),
        store::RunState::Succeeded,
        "{:?}",
        run.error
    );
    let orgs: Vec<Org> = Org::objects()
        .fetch(&b.pools.registry_pool())
        .await
        .unwrap();
    assert_eq!(orgs.len(), 1);
    assert!(orgs[0].active, "the resumed tenant was not activated");
}

/// An inactive Org no failed run made is not taken over.
#[tokio::test]
async fn a_retry_does_not_resume_a_tenant_it_did_not_make() {
    let holder = tempfile::tempdir().expect("tenants dir");
    let b = boot(&holder).await;
    let slug = unique("foreign");
    let _ = half_made(&b, &holder, &slug, "evt-foreign", false).await;

    let body = body_for(&slug, "evt-foreign", chrono::Utc::now().timestamp());
    let sig = signed(&body);
    let resp = b.app.clone().oneshot(post(body, Some(&sig))).await.unwrap();
    let run_id = json_of(resp).await["run_id"].as_i64().unwrap();
    let run = await_run(&b, run_id).await;
    assert_eq!(store::RunState::parse(&run.state), store::RunState::Failed);
    let orgs: Vec<Org> = Org::objects()
        .fetch(&b.pools.registry_pool())
        .await
        .unwrap();
    assert!(!orgs[0].active);
}

/// Purge forgets which run made the Org, so a new tenant that reuses the
/// id (SQLite does) is not taken for the old half-made one.
#[tokio::test]
async fn a_purged_tenant_is_not_resumed_through_a_reused_id() {
    let holder = tempfile::tempdir().expect("tenants dir");
    let b = boot(&holder).await;
    let slug = unique("reused");
    let old_id = half_made(&b, &holder, &slug, "evt-reused-1", true).await;
    rustango::tenancy::decommission::decommission(
        b.pools.as_ref(),
        &slug,
        rustango::tenancy::decommission::Action::Purge {
            purge_database: true,
        },
    )
    .await
    .expect("purge");

    let registry = b.pools.registry_pool();
    // Reused on purpose: a table without AUTOINCREMENT would do it itself.
    let mut org = Org {
        id: rustango::sql::Auto::Set(old_id),
        slug: slug.clone(),
        display_name: slug.clone(),
        backend_kind: "sqlite".into(),
        database_url: Some(format!(
            "sqlite://{}/t_{slug}.db?mode=rwc",
            holder.path().display()
        )),
        active: false,
        ..rustango::testkit::org()
    };
    org.insert_pool(&registry).await.unwrap();

    let body = body_for(&slug, "evt-reused-2", chrono::Utc::now().timestamp());
    let sig = signed(&body);
    let resp = b.app.clone().oneshot(post(body, Some(&sig))).await.unwrap();
    let run_id = json_of(resp).await["run_id"].as_i64().unwrap();
    let run = await_run(&b, run_id).await;
    assert_eq!(store::RunState::parse(&run.state), store::RunState::Failed);
    let orgs: Vec<Org> = Org::objects().fetch(&registry).await.unwrap();
    assert!(!orgs[0].active, "a purged tenant's run resumed a new one");
}
