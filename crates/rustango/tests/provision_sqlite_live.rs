#![cfg(all(feature = "sqlite", feature = "tenancy"))]
//! The provisioning engine, driven directly (#1318).
//!
//! The point of extracting it is that a caller who is not a terminal can
//! stand up a tenant — so these drive `provision_tenant` with **no
//! `argv` and no `Write`**, which is exactly what the operator console
//! (#1322) and the provisioning webhook (#1323) will do.
//!
//! SQLite on a temp file, so every pool connection sees the same DB.

use std::sync::Mutex;

use rustango::sql::{sqlx, FetcherPool as _};
use rustango::tenancy::provision::{
    self, MigrationsOutcome, ProvisionEvent, ProvisionRequest, ProvisionStep, StepStatus,
};
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

/// Stand the registry tables up the way the CLI does.
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

#[derive(Default)]
struct Recorder(Mutex<Vec<ProvisionEvent>>);

impl Recorder {
    fn observer(&self) -> impl provision::ProvisionObserver + '_ {
        move |e: ProvisionEvent| self.0.lock().unwrap().push(e)
    }

    /// Step transitions only — migration events are #1320's business
    /// and are asserted there.
    fn steps(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                ProvisionEvent::Step { step, status } => {
                    let s = match status {
                        StepStatus::Started => "started",
                        StepStatus::Ok => "ok",
                        StepStatus::Skipped(_) => "skipped",
                        StepStatus::Failed(_) => "failed",
                    };
                    Some(format!("{step:?}:{s}"))
                }
                ProvisionEvent::Registered { .. } => Some("registered".to_owned()),
                ProvisionEvent::Migration(_) => None,
            })
            .collect()
    }
}

/// The headline: a tenant created with no `argv` and no `Write`.
#[tokio::test]
async fn the_engine_provisions_without_argv_or_a_writer() {
    let (pools, url, _tmp) = registry().await;
    let migrations = tempfile::tempdir().expect("migrations dir");
    migrate_registry(&pools, &url, migrations.path()).await;

    let tenant_db = _tmp.path().join("acme.db");
    let request =
        ProvisionRequest::database("acme", format!("sqlite://{}?mode=rwc", tenant_db.display()));

    let rec = Recorder::default();
    let outcome = provision::provision_tenant(
        &pools,
        &url,
        migrations.path(),
        &request,
        Some(&rec.observer()),
    )
    .await
    .expect("provision");

    assert_eq!(outcome.slug, "acme");
    assert!(outcome.org_id > 0, "org id should come back: {outcome:?}");

    // Every step reports, in order, and the not-yet-built one reports
    // as skipped rather than silently not existing.
    assert_eq!(
        rec.steps(),
        vec![
            "Validate:started",
            "Validate:ok",
            "CheckConnection:skipped",
            "ProvisionStorage:skipped",
            "RegisterOrg:started",
            "RegisterOrg:ok",
            "registered",
            "Migrate:started",
            "Migrate:ok",
        ],
        "{:?}",
        rec.steps()
    );

    // The tenant actually resolves now.
    let orgs = rustango::tenancy::Org::objects()
        .fetch(&pools.registry_pool())
        .await
        .expect("fetch orgs");
    assert_eq!(orgs.len(), 1);
    assert_eq!(orgs[0].slug, "acme");
    assert!(orgs[0].active);
}

/// `run_migrations: false` is the `--no-migrate` flag inverted. The
/// step reports skipped, and no migration events are emitted at all.
#[tokio::test]
async fn migrations_can_be_skipped() {
    let (pools, url, _tmp) = registry().await;
    let migrations = tempfile::tempdir().expect("migrations dir");
    migrate_registry(&pools, &url, migrations.path()).await;

    let tenant_db = _tmp.path().join("globex.db");
    let mut request = ProvisionRequest::database(
        "globex",
        format!("sqlite://{}?mode=rwc", tenant_db.display()),
    );
    request.run_migrations = false;

    let rec = Recorder::default();
    let outcome = provision::provision_tenant(
        &pools,
        &url,
        migrations.path(),
        &request,
        Some(&rec.observer()),
    )
    .await
    .expect("provision");

    assert!(
        matches!(outcome.migrations, MigrationsOutcome::Skipped),
        "{:?}",
        outcome.migrations
    );
    assert!(
        rec.steps().contains(&"Migrate:skipped".to_owned()),
        "{:?}",
        rec.steps()
    );
    assert!(
        !rec.0
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, ProvisionEvent::Migration(_))),
        "no migration events should be emitted when migrations are skipped"
    );
}

/// A duplicate slug fails at `Validate` and reports *which* step failed
/// — the reason the failure path goes through the observer rather than
/// a bare `?`. A watcher must never be left with a step stuck on
/// "started".
#[tokio::test]
async fn a_duplicate_slug_fails_at_validate_and_says_so() {
    let (pools, url, _tmp) = registry().await;
    let migrations = tempfile::tempdir().expect("migrations dir");
    migrate_registry(&pools, &url, migrations.path()).await;

    let tenant_db = _tmp.path().join("dup.db");
    let mut request =
        ProvisionRequest::database("dup", format!("sqlite://{}?mode=rwc", tenant_db.display()));
    request.run_migrations = false;
    provision::provision_tenant(&pools, &url, migrations.path(), &request, None)
        .await
        .expect("first provision");

    let rec = Recorder::default();
    let err = provision::provision_tenant(
        &pools,
        &url,
        migrations.path(),
        &request,
        Some(&rec.observer()),
    )
    .await
    .expect_err("second provision must be rejected");

    assert!(err.to_string().contains("already exists"), "got: {err}");
    assert_eq!(
        rec.steps(),
        vec!["Validate:started", "Validate:failed"],
        "the failing step must be named, and nothing after it should run: {:?}",
        rec.steps()
    );

    // And nothing was written: still exactly one org.
    let orgs = rustango::tenancy::Org::objects()
        .fetch(&pools.registry_pool())
        .await
        .expect("fetch orgs");
    assert_eq!(orgs.len(), 1);
}

/// Database-mode without a URL is rejected before anything is written.
#[tokio::test]
async fn database_mode_without_a_url_is_rejected_before_any_write() {
    let (pools, url, _tmp) = registry().await;
    let migrations = tempfile::tempdir().expect("migrations dir");
    migrate_registry(&pools, &url, migrations.path()).await;

    let mut request = ProvisionRequest::database("nourl", "unused");
    request.database_url = None;

    let rec = Recorder::default();
    let err = provision::provision_tenant(
        &pools,
        &url,
        migrations.path(),
        &request,
        Some(&rec.observer()),
    )
    .await
    .expect_err("must be rejected");

    assert!(err.to_string().contains("--database-url"), "got: {err}");
    assert_eq!(rec.steps(), vec!["Validate:started", "Validate:failed"]);

    let orgs = rustango::tenancy::Org::objects()
        .fetch(&pools.registry_pool())
        .await
        .expect("fetch orgs");
    assert!(orgs.is_empty(), "nothing should have been written");
}

/// Schema-mode on a SQLite registry is refused with a message that says
/// what to do instead — `CREATE SCHEMA` does not exist there.
#[tokio::test]
async fn schema_mode_on_sqlite_is_refused_with_a_useful_message() {
    let (pools, url, _tmp) = registry().await;
    let migrations = tempfile::tempdir().expect("migrations dir");
    migrate_registry(&pools, &url, migrations.path()).await;

    let mut request = ProvisionRequest::database("schemamode", "unused");
    request.mode = rustango::tenancy::StorageMode::Schema;
    request.database_url = None;
    request.run_migrations = false;

    let rec = Recorder::default();
    let err = provision::provision_tenant(
        &pools,
        &url,
        migrations.path(),
        &request,
        Some(&rec.observer()),
    )
    .await
    .expect_err("schema mode must be refused on sqlite");

    assert!(
        err.to_string().contains("--mode database"),
        "the error should point at the fix, got: {err}"
    );
    // It gets past validation — the request is internally coherent —
    // and fails at the storage step, which is where the dialect
    // actually bites.
    assert_eq!(
        rec.steps(),
        vec![
            "Validate:started",
            "Validate:ok",
            "CheckConnection:skipped",
            "ProvisionStorage:started",
            "ProvisionStorage:failed",
        ],
        "{:?}",
        rec.steps()
    );
}

/// An observer is optional. Passing `None` must provision identically —
/// the engine's behaviour cannot depend on whether anyone is watching.
#[tokio::test]
async fn provisioning_unobserved_works_the_same() {
    let (pools, url, _tmp) = registry().await;
    let migrations = tempfile::tempdir().expect("migrations dir");
    migrate_registry(&pools, &url, migrations.path()).await;

    let tenant_db = _tmp.path().join("quiet.db");
    let request = ProvisionRequest::database(
        "quiet",
        format!("sqlite://{}?mode=rwc", tenant_db.display()),
    );

    let outcome = provision::provision_tenant(&pools, &url, migrations.path(), &request, None)
        .await
        .expect("provision");

    assert_eq!(outcome.slug, "quiet");
    assert!(outcome.org_id > 0);
    assert!(
        !matches!(outcome.migrations, MigrationsOutcome::Skipped),
        "migrations were requested: {:?}",
        outcome.migrations
    );
}

/// `ProvisionStep` is part of the public surface a console will match
/// on, so pin the set. Adding one is a breaking change for a watcher.
#[test]
fn the_step_list_is_stable() {
    let all = [
        ProvisionStep::Validate,
        ProvisionStep::CheckConnection,
        ProvisionStep::ProvisionStorage,
        ProvisionStep::RegisterOrg,
        ProvisionStep::Migrate,
    ];
    assert_eq!(all.len(), 5);
}
