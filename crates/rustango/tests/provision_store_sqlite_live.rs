#![cfg(all(feature = "sqlite", feature = "tenancy"))]
//! Durable provisioning runs (#1321).
//!
//! Four things have to hold, and each of them is a failure mode that
//! only shows up in production:
//!
//! * a run **replays** from persistence, so a console that connects
//!   after the POST still sees the opening steps;
//! * a run started on one pod is **readable from another**;
//! * a failed run leaves **nothing that resolves and serves**; and
//! * the stored row **never contains a password**.

use rustango::sql::{sqlx, FetcherPool as _};
use rustango::tenancy::provision::{self, MigrationsOutcome, ProvisionRequest};
use rustango::tenancy::provision_store::{self as store, RunState};
use rustango::tenancy::TenantPools;

async fn registry() -> (TenantPools<sqlx::Sqlite>, String, tempfile::TempDir) {
    let tmpdir = tempfile::tempdir().expect("tempdir");
    let db_path = tmpdir.path().join("registry.db");
    let url = format!("sqlite://{}?mode=rwc", db_path.display());
    let pool = sqlx::SqlitePool::connect(&url)
        .await
        .expect("sqlite connect");
    (TenantPools::<sqlx::Sqlite>::new(pool), url, tmpdir)
}

async fn migrate_registry(pools: &TenantPools<sqlx::Sqlite>, url: &str, dir: &std::path::Path) {
    let mut buf: Vec<u8> = Vec::new();
    rustango::tenancy::manage::run_with_writer(
        pools,
        url,
        dir,
        vec!["migrate-registry".to_owned()],
        &mut buf,
    )
    .await
    .expect("migrate-registry");
}

/// The tables arrive through the generated system-migration chain, the
/// same way `rustango_org_hosts` does — no hand-written DDL anywhere.
#[tokio::test]
async fn the_run_tables_come_from_the_registry_migration_chain() {
    let (pools, url, _tmp) = registry().await;
    let migrations = tempfile::tempdir().expect("migrations dir");
    migrate_registry(&pools, &url, migrations.path()).await;

    let pool = sqlx::SqlitePool::connect(&url).await.expect("connect");
    let names: Vec<(String,)> =
        sqlx::query_as("SELECT name FROM sqlite_master WHERE type = 'table'")
            .fetch_all(&pool)
            .await
            .expect("list tables");
    let names: Vec<&str> = names.iter().map(|(n,)| n.as_str()).collect();

    assert!(
        names.contains(&"rustango_provisioning_runs"),
        "runs table missing: {names:?}"
    );
    assert!(
        names.contains(&"rustango_provisioning_events"),
        "events table missing: {names:?}"
    );
}

/// A recorded run is replayable from the beginning, in order, by
/// something that was not watching while it happened. This is the whole
/// reason the events are in a table rather than a broadcast channel.
#[tokio::test]
async fn a_run_replays_in_order_for_a_subscriber_that_was_not_there() {
    let (pools, url, _tmp) = registry().await;
    let migrations = tempfile::tempdir().expect("migrations dir");
    migrate_registry(&pools, &url, migrations.path()).await;

    let tenant_db = _tmp.path().join("replay.db");
    let request = ProvisionRequest::database(
        "replay",
        format!("sqlite://{}?mode=rwc", tenant_db.display()),
    );

    // No observer at all — nobody watching live.
    let (run, outcome) = provision::provision_tenant_recorded(
        &pools,
        &url,
        migrations.path(),
        &request,
        None,
        Some("cli"),
        None,
    )
    .await
    .expect("provision");

    let run_id = run.id.get().copied().expect("run id");
    assert_eq!(RunState::parse(&run.state), RunState::Succeeded);
    assert_eq!(run.org_id, Some(outcome.org_id));

    // Replay the whole log.
    let registry_pool = pools.registry_pool();
    let events = store::events_since(&registry_pool, run_id, 0)
        .await
        .expect("replay");
    assert!(!events.is_empty(), "a run should leave a log");

    // `seq` is dense and ordered — `Last-Event-ID` counts on it.
    let seqs: Vec<i64> = events.iter().map(|e| e.seq).collect();
    let expected: Vec<i64> = (1..=seqs.len() as i64).collect();
    assert_eq!(seqs, expected, "seq must be dense and ordered: {seqs:?}");

    // The opening step is there, which a broadcast bus would have lost.
    let steps: Vec<&str> = events.iter().map(|e| e.step.as_str()).collect();
    assert_eq!(steps.first(), Some(&"validate"), "{steps:?}");
    assert!(steps.contains(&"activate"), "{steps:?}");

    // And a reconnect resumes from where it left off.
    let tail = store::events_since(&registry_pool, run_id, seqs[2])
        .await
        .expect("resume");
    assert_eq!(tail.len(), events.len() - 3);
    assert_eq!(tail.first().map(|e| e.seq), Some(seqs[3]));
}

/// A run written by one connection is readable from another — the
/// stand-in for a second pod, which is the case in-process state gets
/// wrong.
#[tokio::test]
async fn a_run_is_readable_from_a_separate_connection() {
    let (pools, url, _tmp) = registry().await;
    let migrations = tempfile::tempdir().expect("migrations dir");
    migrate_registry(&pools, &url, migrations.path()).await;

    let tenant_db = _tmp.path().join("otherpod.db");
    let request = ProvisionRequest::database(
        "otherpod",
        format!("sqlite://{}?mode=rwc", tenant_db.display()),
    );
    let (run, _) = provision::provision_tenant_recorded(
        &pools,
        &url,
        migrations.path(),
        &request,
        None,
        None,
        None,
    )
    .await
    .expect("provision");
    let run_id = run.id.get().copied().expect("run id");

    // A completely separate pool — as far as this process is
    // concerned, another pod.
    let other =
        rustango::sql::Pool::Sqlite(sqlx::SqlitePool::connect(&url).await.expect("second pool"));
    let seen = store::run_by_id(&other, run_id)
        .await
        .expect("read")
        .expect("the run should be visible");
    assert_eq!(seen.slug, "otherpod");
    assert_eq!(RunState::parse(&seen.state), RunState::Succeeded);
    assert!(!store::events_since(&other, run_id, 0)
        .await
        .expect("events")
        .is_empty());
}

/// The stored URL must never carry a password. The single most likely
/// place for a credential to escape into a log or an HTTP response.
#[tokio::test]
async fn the_stored_url_never_contains_the_password() {
    let (pools, url, _tmp) = registry().await;
    let migrations = tempfile::tempdir().expect("migrations dir");
    migrate_registry(&pools, &url, migrations.path()).await;
    let registry_pool = pools.registry_pool();

    // Straight at the store, with a URL shaped like a real one. (The
    // engine would refuse to connect to this, which is beside the
    // point — redaction happens on the way in.)
    let run = store::open_run(
        &registry_pool,
        "secretive",
        "database",
        "postgres",
        Some("postgres://app:hunter2@db.internal:5432/acme"),
        Some("operator@example.com"),
        None,
    )
    .await
    .expect("open run");

    assert_eq!(
        run.database_url.as_deref(),
        Some("postgres://app:***@db.internal:5432/acme"),
        "the password must be gone before it is stored"
    );

    // And nothing reading the row back can recover it.
    let run_id = run.id.get().copied().expect("id");
    let read = store::run_by_id(&registry_pool, run_id)
        .await
        .expect("read")
        .expect("row");
    assert!(
        !read.database_url.unwrap_or_default().contains("hunter2"),
        "password survived a round-trip"
    );
}

/// **The disposition decision.** A run that fails at the migrate step
/// leaves an `Org` row — an operator needs to see what was half-made —
/// but it is inactive, so nothing resolves to it.
#[tokio::test]
async fn a_failed_migration_leaves_an_inactive_tenant_and_a_failed_run() {
    let (pools, url, _tmp) = registry().await;
    let migrations = tempfile::tempdir().expect("migrations dir");
    migrate_registry(&pools, &url, migrations.path()).await;

    // A migration directory with a broken tenant-scoped migration:
    // two files creating the same table, so the second collides.
    let broken = tempfile::tempdir().expect("broken dir");
    for name in ["0001_make", "0002_again"] {
        let mig = rustango::migrate::Migration {
            name: name.to_owned(),
            created_at: "2026-09-10T00:00:00Z".into(),
            prev: None,
            atomic: true,
            scope: rustango::migrate::MigrationScope::Tenant,
            replaces: Vec::new(),
            snapshot: serde_json::from_value(serde_json::json!({
                "tables": [{
                    "name": "collide", "model": "T",
                    "fields": [{"name": "id", "column": "id", "ty": "i64",
                                "nullable": false, "primary_key": true}]
                }]
            }))
            .unwrap(),
            forward: vec![rustango::migrate::Operation::Schema(
                rustango::migrate::SchemaChange::CreateTable("collide".into()),
            )],
        };
        rustango::migrate::file::write(&broken.path().join(format!("{name}.json")), &mig).unwrap();
    }

    let tenant_db = _tmp.path().join("broken.db");
    let request = ProvisionRequest::database(
        "broken",
        format!("sqlite://{}?mode=rwc", tenant_db.display()),
    );
    let (run, outcome) = provision::provision_tenant_recorded(
        &pools,
        &url,
        broken.path(),
        &request,
        None,
        None,
        None,
    )
    .await
    .expect("provisioning itself succeeds — the migration is what fails");

    assert!(
        matches!(outcome.migrations, MigrationsOutcome::Failed(_)),
        "{:?}",
        outcome.migrations
    );
    assert_eq!(RunState::parse(&run.state), RunState::Failed);
    assert!(run.error.is_some(), "a failed run must say why");
    assert!(run.finished_at.is_some(), "a failed run is still finished");

    // The row exists — and does not serve.
    let registry_pool = pools.registry_pool();
    let orgs = rustango::tenancy::Org::objects()
        .fetch(&registry_pool)
        .await
        .expect("fetch orgs");
    assert_eq!(
        orgs.len(),
        1,
        "the half-made tenant is kept for the operator"
    );
    assert!(
        !orgs[0].active,
        "a tenant whose migrations failed must not resolve"
    );

    // And the log names the step that failed.
    let run_id = run.id.get().copied().expect("id");
    let events = store::events_since(&registry_pool, run_id, 0)
        .await
        .expect("events");
    assert!(
        events
            .iter()
            .any(|e| e.step == "migrate" && e.status == "failed"),
        "{:?}",
        events
            .iter()
            .map(|e| (&e.step, &e.status))
            .collect::<Vec<_>>()
    );
    assert!(
        events
            .iter()
            .any(|e| e.step == "activate" && e.status == "skipped"),
        "activation must be skipped, not attempted"
    );
}

/// A run that fails *before* the row lands leaves no tenant at all, and
/// still closes rather than sitting in `running` forever.
#[tokio::test]
async fn a_run_that_fails_early_still_closes() {
    let (pools, url, _tmp) = registry().await;
    let migrations = tempfile::tempdir().expect("migrations dir");
    migrate_registry(&pools, &url, migrations.path()).await;

    let mut request = ProvisionRequest::database("nowhere", "unused");
    request.database_url = None; // rejected at Validate

    let err = provision::provision_tenant_recorded(
        &pools,
        &url,
        migrations.path(),
        &request,
        None,
        None,
        None,
    )
    .await
    .expect_err("must be rejected");
    assert!(err.to_string().contains("--database-url"), "got: {err}");

    let registry_pool = pools.registry_pool();
    let runs: Vec<store::ProvisioningRun> = store::ProvisioningRun::objects()
        .fetch(&registry_pool)
        .await
        .expect("fetch runs");
    assert_eq!(runs.len(), 1, "the attempt is still recorded");
    assert_eq!(RunState::parse(&runs[0].state), RunState::Failed);
    assert!(
        runs[0].finished_at.is_some(),
        "a run must never be left in `running`"
    );
    assert_eq!(runs[0].org_id, None, "no tenant was created");
}

/// The idempotency key is unique, so two deliveries of one event cannot
/// both open a run — the constraint, not the handler, is what wins.
#[tokio::test]
async fn an_idempotency_key_cannot_open_two_runs() {
    let (pools, url, _tmp) = registry().await;
    let migrations = tempfile::tempdir().expect("migrations dir");
    migrate_registry(&pools, &url, migrations.path()).await;
    let registry_pool = pools.registry_pool();

    store::open_run(
        &registry_pool,
        "once",
        "database",
        "sqlite",
        None,
        None,
        Some("evt-1"),
    )
    .await
    .expect("first");
    let second = store::open_run(
        &registry_pool,
        "twice",
        "database",
        "sqlite",
        None,
        None,
        Some("evt-1"),
    )
    .await;
    assert!(second.is_err(), "the unique constraint must refuse this");

    let found = store::run_by_idempotency_key(&registry_pool, "evt-1")
        .await
        .expect("lookup")
        .expect("the first run");
    assert_eq!(found.slug, "once");
}

/// Pruning removes finished runs and their events, and leaves
/// unfinished ones alone however old — one stuck in `running` is
/// evidence of a pod that died, which is what someone comes looking for.
#[tokio::test]
async fn pruning_removes_finished_runs_and_keeps_unfinished_ones() {
    let (pools, url, _tmp) = registry().await;
    let migrations = tempfile::tempdir().expect("migrations dir");
    migrate_registry(&pools, &url, migrations.path()).await;
    let registry_pool = pools.registry_pool();

    let done = store::open_run(
        &registry_pool,
        "done",
        "database",
        "sqlite",
        None,
        None,
        None,
    )
    .await
    .expect("open");
    let done_id = done.id.get().copied().expect("id");
    store::append_event(&registry_pool, done_id, 1, "validate", "ok", "")
        .await
        .expect("event");
    store::finish_run(&registry_pool, done_id, RunState::Succeeded, None)
        .await
        .expect("finish");

    let stuck = store::open_run(
        &registry_pool,
        "stuck",
        "database",
        "sqlite",
        None,
        None,
        None,
    )
    .await
    .expect("open");
    let stuck_id = stuck.id.get().copied().expect("id");

    let cutoff = chrono::Utc::now() + chrono::Duration::minutes(1);
    let removed = store::prune_runs(&registry_pool, cutoff)
        .await
        .expect("prune");
    assert_eq!(removed, 1, "only the finished run should go");

    assert!(store::run_by_id(&registry_pool, done_id)
        .await
        .expect("read")
        .is_none());
    assert!(store::events_since(&registry_pool, done_id, 0)
        .await
        .expect("events")
        .is_empty());
    assert!(
        store::run_by_id(&registry_pool, stuck_id)
            .await
            .expect("read")
            .is_some(),
        "an unfinished run must survive pruning at any age"
    );
}
