#![cfg(all(feature = "sqlite", feature = "tenancy"))]
//! `audit-cleanup` retention, including the registry's own log.
//!
//! The verb walked tenants only, so the log the operator console writes
//! to — every tenant edit, hostname change, operator action, purge —
//! grew with console use and nothing trimmed it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use rustango::audit::AuditLog;
use rustango::core::Column as _;
use rustango::sql::{sqlx, Auto, FetcherPool as _, Pool};
use rustango::tenancy::TenantPools;

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn unique(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::SeqCst)
    )
}

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

    /// One registry audit row, `age_days` old.
    async fn seed(&self, pk: &str, age_days: i64) {
        let mut row = AuditLog {
            id: Auto::default(),
            entity_table: "rustango_orgs".into(),
            entity_pk: pk.to_owned(),
            operation: "action".into(),
            source: "operator:1:test".into(),
            changes: serde_json::json!({}),
            occurred_at: chrono::Utc::now() - chrono::Duration::days(age_days),
        };
        row.insert_pool(&self.registry)
            .await
            .expect("seed audit row");
    }

    async fn registry_entries(&self) -> Vec<String> {
        let rows: Vec<AuditLog> = AuditLog::objects().fetch(&self.registry).await.unwrap();
        rows.into_iter().map(|r| r.entity_pk).collect()
    }
}

/// The gap: the registry's own log was never swept.
#[tokio::test]
async fn the_registry_log_is_trimmed_by_days() {
    let b = boot().await;
    b.seed("ancient", 120).await;
    b.seed("recent", 1).await;

    let out = b.run(&["audit-cleanup", "--days", "30"]).await.expect("ok");
    assert!(
        out.contains("registry"),
        "the output should account for the registry: {out}"
    );

    let left = b.registry_entries().await;
    assert!(
        left.contains(&"recent".to_owned()),
        "recent entries stay: {left:?}"
    );
    assert!(
        !left.contains(&"ancient".to_owned()),
        "old entries go: {left:?}"
    );
}

#[tokio::test]
async fn keep_last_applies_to_the_registry_too() {
    let b = boot().await;
    // Same (entity_table, entity_pk), so they compete for the same slot.
    for age in [5, 4, 3, 2, 1] {
        b.seed("acme", age).await;
    }
    assert_eq!(b.registry_entries().await.len(), 5);

    b.run(&["audit-cleanup", "--keep-last", "2"])
        .await
        .expect("ok");
    assert_eq!(
        b.registry_entries().await.len(),
        2,
        "should keep the 2 most recent for that record"
    );
}

#[tokio::test]
async fn registry_only_does_not_need_a_tenant() {
    let b = boot().await;
    b.seed("ancient", 120).await;

    let out = b
        .run(&["audit-cleanup", "--registry", "--days", "30"])
        .await
        .expect("ok");
    assert!(out.contains("registry"), "{out}");
    assert!(
        b.registry_entries().await.is_empty(),
        "the old entry should be gone"
    );
}

impl Booted {
    /// A tenant whose storage exists but has no schema — `--no-migrate`,
    /// so it has no `rustango_audit_log` to sweep.
    async fn unmigrated_tenant(&self) -> String {
        let slug = unique("acme");
        let db = self._tmp.path().join(format!("{slug}.db"));
        self.run(&[
            "create-tenant",
            &slug,
            "--mode",
            "database",
            "--backend",
            "sqlite",
            "--database-url",
            &format!("sqlite://{}?mode=rwc", db.display()),
            "--no-migrate",
        ])
        .await
        .expect("create the tenant");
        slug
    }
}

/// Naming one tenant asks for that tenant, so the registry is left
/// alone — otherwise `--tenant acme` would quietly trim a log the
/// caller never mentioned.
#[tokio::test]
async fn naming_a_tenant_leaves_the_registry_alone() {
    let b = boot().await;
    b.seed("ancient", 120).await;
    let slug = b.unmigrated_tenant().await;

    // The tenant has no audit table, so this fails — the point is what
    // it did *not* touch.
    let _ = b
        .run(&["audit-cleanup", "--tenant", &slug, "--days", "30"])
        .await;

    assert!(
        b.registry_entries().await.contains(&"ancient".to_owned()),
        "the registry's log must be untouched when a tenant was named"
    );
}

/// A sweep that aborts on the first bad tenant leaves every later one
/// untrimmed — which defeats retention exactly when it matters. The
/// loop collects outcomes instead.
#[tokio::test]
async fn one_broken_tenant_does_not_stop_the_sweep() {
    let b = boot().await;
    b.seed("ancient", 120).await;
    let broken = b.unmigrated_tenant().await;

    let out = b
        .run(&["audit-cleanup", "--days", "30"])
        .await
        .expect("the sweep should finish despite a broken tenant");

    assert!(
        out.contains(&format!("tenant={broken} FAILED")),
        "the broken tenant should be reported, not swallowed: {out}"
    );
    assert!(out.contains("failed=1"), "and counted: {out}");
    assert!(
        !b.registry_entries().await.contains(&"ancient".to_owned()),
        "while the registry was still trimmed: {out}"
    );
}

#[tokio::test]
async fn registry_and_tenant_together_are_refused() {
    let b = boot().await;
    let err = b
        .run(&[
            "audit-cleanup",
            "--registry",
            "--tenant",
            "acme",
            "--days",
            "30",
        ])
        .await
        .expect_err("contradictory scopes");
    assert!(
        err.contains("--registry") && err.contains("--tenant"),
        "{err}"
    );
}

#[tokio::test]
async fn a_retention_mode_is_required() {
    let b = boot().await;
    let err = b.run(&["audit-cleanup"]).await.expect_err("no mode");
    assert!(
        err.contains("--days") && err.contains("--keep-last"),
        "{err}"
    );

    let err = b
        .run(&["audit-cleanup", "--days", "1", "--keep-last", "1"])
        .await
        .expect_err("both modes");
    assert!(err.contains("mutually exclusive"), "{err}");
}
