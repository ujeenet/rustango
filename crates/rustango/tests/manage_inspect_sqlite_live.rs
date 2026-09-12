#![cfg(all(feature = "sqlite", feature = "tenancy"))]
//! `list-runs`, `show-run`, `audit-log` (#1344).
//!
//! The console renders all three. Nothing printed them, so "did that
//! provision finish?" and "who deactivated this tenant?" — the questions
//! asked during an incident — needed a browser pointed at production or a
//! SQL client on the registry.

use std::sync::Arc;

use rustango::audit::AuditLog;
use rustango::sql::{sqlx, Auto, Pool};
use rustango::tenancy::TenantPools;

struct Booted {
    pools: Arc<TenantPools<sqlx::Sqlite>>,
    registry: Pool,
    url: String,
    _tmp: tempfile::TempDir,
    migrations: tempfile::TempDir,
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
    Booted {
        pools,
        registry,
        url,
        _tmp: tmp,
        migrations,
    }
}

impl Booted {
    async fn run(&self, args: &[&str]) -> Result<String, String> {
        let mut buf: Vec<u8> = Vec::new();
        let argv: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        rustango::tenancy::manage::run_with_writer(
            self.pools.as_ref(),
            &self.url,
            self.migrations.path(),
            argv,
            &mut buf,
        )
        .await
        .map(|()| String::from_utf8_lossy(&buf).into_owned())
        .map_err(|e| e.to_string())
    }

    /// Provisioning a tenant is what opens a run, so this is the fixture.
    async fn tenant(&self, slug: &str) {
        let db = self._tmp.path().join(format!("{slug}.db"));
        self.run(&[
            "create-tenant",
            slug,
            "--mode",
            "database",
            "--backend",
            "sqlite",
            "--database-url",
            &format!("sqlite://{}?mode=rwc", db.display()),
            "--no-migrate",
        ])
        .await
        .unwrap_or_else(|e| panic!("create {slug}: {e}"));
    }

    async fn audit_row(&self, pk: &str, operation: &str, source: &str) {
        let mut row = AuditLog {
            id: Auto::default(),
            entity_table: "rustango_orgs".into(),
            entity_pk: pk.to_owned(),
            operation: operation.to_owned(),
            source: source.to_owned(),
            changes: serde_json::json!({}),
            occurred_at: chrono::Utc::now(),
        };
        row.insert_pool(&self.registry).await.expect("seed audit");
    }
}

#[tokio::test]
async fn provisioning_a_tenant_shows_up_in_list_runs() {
    let b = boot().await;
    b.tenant("acme").await;

    let out = b.run(&["list-runs"]).await.expect("list-runs");
    assert!(
        out.contains("acme"),
        "the run should name the tenant: {out}"
    );
    assert!(
        out.contains("show-run"),
        "should point at the detail verb: {out}"
    );
}

/// A tenant created from a shell used to leave no run at all, so the run
/// history described only what the console had done — and a CLI provision
/// that died halfway left nothing to find.
#[tokio::test]
async fn a_cli_provision_is_recorded_and_says_it_came_from_the_cli() {
    let b = boot().await;
    b.tenant("acme").await;

    let listed = b.run(&["list-runs"]).await.expect("list");
    let id = listed
        .lines()
        .find(|l| l.contains("acme"))
        .and_then(|l| l.split_whitespace().next())
        .expect("a run id")
        .to_owned();

    let out = b.run(&["show-run", &id]).await.expect("show-run");
    assert!(
        out.contains("requested:  cli"),
        "the run should say where it came from: {out}"
    );
    assert!(
        out.contains("succeeded") || out.contains("ok"),
        "and that it finished: {out}"
    );
}

#[tokio::test]
async fn an_empty_registry_reports_no_runs() {
    let b = boot().await;
    let out = b.run(&["list-runs"]).await.expect("list-runs");
    assert!(out.contains("(no runs)"), "{out}");
}

/// The detail view is the reason to have the list: it prints the steps a
/// run recorded, which is what says where a failure happened.
#[tokio::test]
async fn show_run_prints_the_recorded_steps() {
    let b = boot().await;
    b.tenant("acme").await;

    // The list is newest-first, so the run just opened is the first id.
    let listed = b.run(&["list-runs"]).await.expect("list");
    let id = listed
        .lines()
        .find(|l| l.contains("acme"))
        .and_then(|l| l.split_whitespace().next())
        .expect("a run id in the listing")
        .to_owned();

    let out = b.run(&["show-run", &id]).await.expect("show-run");
    assert!(out.contains(&format!("run {id}")), "{out}");
    assert!(out.contains("acme"), "{out}");
    assert!(out.contains("seq"), "should print the step table: {out}");
}

#[tokio::test]
async fn show_run_refuses_a_non_id_and_names_a_missing_one() {
    let b = boot().await;

    let err = b.run(&["show-run", "banana"]).await.expect_err("not an id");
    assert!(err.contains("banana"), "{err}");

    let err = b.run(&["show-run", "4242"]).await.expect_err("no such run");
    assert!(err.contains("4242"), "{err}");

    let err = b.run(&["show-run"]).await.expect_err("no id at all");
    assert!(err.contains("run id"), "{err}");
}

#[tokio::test]
async fn list_runs_filters_by_kind_and_validates_its_limit() {
    let b = boot().await;
    b.tenant("acme").await;

    let out = b
        .run(&["list-runs", "--kind", "provision"])
        .await
        .expect("by kind");
    assert!(out.contains("acme"), "{out}");

    // No migration runs exist, so filtering to them is empty, not wrong.
    let out = b
        .run(&["list-runs", "--kind", "migrate"])
        .await
        .expect("by kind");
    assert!(out.contains("(no runs)"), "{out}");

    let err = b
        .run(&["list-runs", "--limit", "0"])
        .await
        .expect_err("zero");
    assert!(err.contains("--limit"), "{err}");
}

/// The filter runs in the query, not over the page. Filtering the newest N
/// reports "none" whenever the newest N happen to be a different kind —
/// which is exactly the wrong answer while hunting the run that failed.
#[tokio::test]
async fn a_kind_filter_reaches_past_a_page_of_the_other_kind() {
    let b = boot().await;
    // One migration run, then enough provisions to bury it.
    rustango::tenancy::provision_store::open_migrate_run(&b.registry, Some("acme"), Some("cli"))
        .await
        .expect("open a migrate run");
    for i in 0..4 {
        b.tenant(&format!("t{i}")).await;
    }

    // A window smaller than the number of provisions above it.
    let out = b
        .run(&["list-runs", "--kind", "migrate", "--limit", "2"])
        .await
        .expect("by kind");
    assert!(
        out.contains("acme"),
        "the migration run is older than the window but still the match: {out}"
    );
}

#[tokio::test]
async fn audit_log_prints_the_registry_trail() {
    let b = boot().await;
    b.audit_row("acme", "action", "operator:1:tenant_deactivate")
        .await;
    b.audit_row("globex", "update", "operator:2:tenant_edit")
        .await;

    let out = b.run(&["audit-log"]).await.expect("audit-log");
    assert!(out.contains("acme") && out.contains("globex"), "{out}");
    assert!(out.contains("tenant_deactivate"), "source column: {out}");
}

#[tokio::test]
async fn audit_log_filters() {
    let b = boot().await;
    b.audit_row("acme", "action", "operator:1:tenant_deactivate")
        .await;
    b.audit_row("globex", "update", "operator:2:tenant_edit")
        .await;

    let out = b.run(&["audit-log", "--pk", "acme"]).await.expect("by pk");
    assert!(out.contains("acme"), "{out}");
    assert!(
        !out.contains("globex"),
        "the filter should exclude it: {out}"
    );

    let out = b
        .run(&["audit-log", "--operation", "update"])
        .await
        .expect("by operation");
    assert!(out.contains("globex") && !out.contains("acme"), "{out}");
}

#[tokio::test]
async fn audit_log_says_when_it_truncated() {
    let b = boot().await;
    for i in 0..5 {
        b.audit_row(&format!("t{i}"), "action", "operator:1:x")
            .await;
    }
    let out = b
        .run(&["audit-log", "--limit", "2"])
        .await
        .expect("limited");
    assert!(
        out.contains("showing 2 of 5"),
        "a truncated view must say so: {out}"
    );
}

#[tokio::test]
async fn an_empty_trail_reports_rather_than_printing_a_bare_header() {
    let b = boot().await;
    let out = b.run(&["audit-log"]).await.expect("audit-log");
    assert!(out.contains("(no audit entries)"), "{out}");
}

#[tokio::test]
async fn unknown_flags_are_named_with_what_is_accepted() {
    let b = boot().await;
    let err = b
        .run(&["audit-log", "--tenant", "acme"])
        .await
        .expect_err("not a flag here");
    assert!(err.contains("--tenant") && err.contains("--table"), "{err}");
}
