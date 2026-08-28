//! Live regressions for the per-tenant sweep fan-out (#1226).
//!
//! The framework's "run this from the scheduler" helpers all take one
//! pool, which under tenancy means one tenant. `tenancy::for_each_tenant`
//! is the loop that closes that gap; these tests pin the two properties
//! that make it usable for a nightly sweep:
//!
//! - it visits **every active** tenant, each against its **own** pool
//!   (so a sweep can't silently clean one tenant and report success), and
//! - one broken tenant does **not** stop the others (so a rotated
//!   credential doesn't starve every tenant later in the list).
//!
//! Database-mode SQLite throughout — each tenant is its own file, which
//! is the cleanest way to prove per-tenant isolation without Postgres.

#![cfg(all(feature = "sqlite", feature = "tenancy"))]

use std::sync::{Arc, Mutex};

use rustango::sql::{sqlx, Auto};
use rustango::tenancy::{for_each_tenant, Org, SweepError, TenantPools};

fn db_org(slug: &str, url: &str, active: bool) -> Org {
    Org {
        id: Auto::default(),
        slug: slug.to_owned(),
        display_name: slug.to_owned(),
        storage_mode: "database".into(),
        backend_kind: "sqlite".into(),
        database_url: Some(url.to_owned()),
        active,
        ..rustango::testkit::org()
    }
}

fn shared_mem(name: &str) -> String {
    format!("sqlite:file:{name}?mode=memory&cache=shared")
}

/// Registry with `rustango_orgs` materialized and the given orgs seeded.
async fn registry_with(orgs: &[Org]) -> sqlx::SqlitePool {
    let pool: sqlx::SqlitePool = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("registry pool");
    let erased = rustango::sql::Pool::Sqlite(pool.clone());
    rustango::testkit::migrate_framework(&erased)
        .await
        .expect("framework tables");
    for org in orgs {
        let mut o = org.clone();
        o.insert_pool(&erased).await.expect("seed org");
    }
    pool
}

#[tokio::test]
async fn for_each_tenant_visits_every_active_tenant_with_its_own_pool() {
    let acme = db_org("acme_sweep", &shared_mem("sweep_acme"), true);
    let globex = db_org("globex_sweep", &shared_mem("sweep_globex"), true);
    let dormant = db_org("dormant_sweep", &shared_mem("sweep_dormant"), false);
    let registry = registry_with(&[acme, globex, dormant]).await;
    let pools: TenantPools<sqlx::Sqlite> = TenantPools::new(registry);

    // Each tenant writes its own slug into its own database, then reads
    // back what that database holds. If the fan-out leaked one pool
    // across tenants, a tenant would see more than its own row.
    let sweep = for_each_tenant(&pools, |org, pool| async move {
        rustango::sql::raw_execute_pool(
            &pool,
            "CREATE TABLE IF NOT EXISTS seen (slug TEXT)",
            vec![],
        )
        .await?;
        rustango::sql::raw_execute_pool(
            &pool,
            "INSERT INTO seen (slug) VALUES (?1)",
            vec![rustango::core::SqlValue::String(org.slug.clone())],
        )
        .await?;
        let rows: Vec<(String,)> =
            rustango::sql::raw_query_pool("SELECT slug FROM seen", vec![], &pool).await?;
        Ok::<_, rustango::sql::ExecError>(rows.into_iter().map(|(s,)| s).collect::<Vec<_>>())
    })
    .await
    .expect("sweep runs");

    assert_eq!(sweep.failed(), 0, "no tenant should fail");
    assert_eq!(
        sweep.succeeded(),
        2,
        "only the two active tenants are visited — the inactive one is skipped"
    );

    let mut visited: Vec<&str> = sweep.values().map(|(slug, _)| slug).collect();
    visited.sort_unstable();
    assert_eq!(visited, vec!["acme_sweep", "globex_sweep"]);

    // Every tenant's database contains exactly its own slug.
    for (slug, seen) in sweep.values() {
        assert_eq!(
            seen,
            &vec![slug.to_owned()],
            "tenant {slug} saw rows from another tenant's database"
        );
    }
}

#[tokio::test]
async fn one_broken_tenant_does_not_stop_the_sweep() {
    // `broken` names a directory that cannot be opened as a database, so
    // resolving its pool fails while its neighbours are fine.
    let good_a = db_org("good_a_sweep", &shared_mem("sweep_good_a"), true);
    let broken = db_org("broken_sweep", "sqlite:/nonexistent-dir/nope.db", true);
    let good_b = db_org("good_b_sweep", &shared_mem("sweep_good_b"), true);
    let registry = registry_with(&[good_a, broken, good_b]).await;
    let pools: TenantPools<sqlx::Sqlite> = TenantPools::new(registry);

    let ran = Arc::new(Mutex::new(Vec::new()));
    let ran_c = Arc::clone(&ran);
    let sweep = for_each_tenant(&pools, move |org, pool| {
        let ran = Arc::clone(&ran_c);
        async move {
            rustango::sql::raw_query_pool::<(i64,)>("SELECT 1", vec![], &pool).await?;
            ran.lock().unwrap().push(org.slug.clone());
            Ok::<_, rustango::sql::ExecError>(())
        }
    })
    .await
    .expect("sweep runs even though a tenant is broken");

    assert_eq!(sweep.succeeded(), 2, "both healthy tenants ran");
    assert_eq!(
        sweep.failed(),
        1,
        "the broken tenant is recorded, not fatal"
    );

    let failed: Vec<&str> = sweep.errors().map(|(slug, _)| slug).collect();
    assert_eq!(failed, vec!["broken_sweep"]);
    assert!(
        matches!(
            sweep.errors().next().map(|(_, e)| e),
            Some(SweepError::Pool(_))
        ),
        "an unopenable database should surface as a pool-resolution failure"
    );

    // Crucially, the tenant *after* the broken one still ran.
    let mut ran = ran.lock().unwrap().clone();
    ran.sort_unstable();
    assert_eq!(ran, vec!["good_a_sweep", "good_b_sweep"]);
}
