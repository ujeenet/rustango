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

#[cfg(feature = "jobs")]
mod job_source {
    use super::*;
    use rustango::audit::{self, with_tenant_source, AuditSource};
    use rustango::jobs::{InMemoryJobQueue, Job, JobError, JobQueue as _};
    use rustango::tenancy::with_tenant;
    use rustango::Model;
    use std::sync::OnceLock;

    #[derive(Model, Debug, Clone)]
    #[rustango(table = "sweep1229_note", app = "sweep1229", audit(track = "title"))]
    #[allow(dead_code)]
    pub struct Note {
        #[rustango(primary_key)]
        pub id: Auto<i64>,
        #[rustango(max_length = 32)]
        pub title: String,
    }

    static POOLS: OnceLock<TenantPools<sqlx::Sqlite>> = OnceLock::new();
    static DONE: Mutex<bool> = Mutex::new(false);

    /// Writes one audited row into every tenant.
    #[derive(serde::Serialize, serde::Deserialize)]
    struct TouchEveryTenant;

    #[async_trait::async_trait]
    impl Job for TouchEveryTenant {
        const NAME: &'static str = "sweep1229_touch";
        async fn run(&self) -> Result<(), JobError> {
            let sweep = for_each_tenant(POOLS.get().unwrap(), |org, pool| async move {
                let mut note = Note {
                    id: Auto::default(),
                    title: org.slug.clone(),
                };
                note.insert_pool(&pool).await
            })
            .await
            .map_err(|e| JobError::Fatal(e.to_string()))?;
            assert_eq!(sweep.failed(), 0);
            *DONE.lock().unwrap() = true;
            Ok(())
        }
    }

    /// #1229 — a job a tenant-A user dispatched writes `user:<id>` on A's
    /// rows only: in tenant B that id is someone else, so B gets `system`.
    #[tokio::test]
    async fn a_tenant_users_job_does_not_stamp_other_tenants() {
        let a = db_org("a_1229", &shared_mem("sweep1229_a"), true);
        let b = db_org("b_1229", &shared_mem("sweep1229_b"), true);
        let registry = registry_with(&[a.clone(), b.clone()]).await;
        let pools = POOLS.get_or_init(|| TenantPools::new(registry));
        for org in [&a, &b] {
            let pool = pools.scoped_pool_dyn(org).await.expect("tenant pool");
            rustango::testkit::matrix::fresh_table::<Note>(&pool).await;
            audit::ensure_table_pool(&pool).await.expect("audit table");
        }

        let q = InMemoryJobQueue::with_workers(1);
        q.register::<TouchEveryTenant>().await;
        q.start().await;
        with_tenant_source(
            AuditSource::User { id: "42".into() },
            "a_1229".into(),
            async {
                q.dispatch(&TouchEveryTenant).await.unwrap();
            },
        )
        .await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !*DONE.lock().unwrap() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        q.shutdown().await;
        assert!(*DONE.lock().unwrap(), "the job did not finish");

        let mut sources = Vec::new();
        for org in [&a, &b] {
            let pool = pools.scoped_pool_dyn(org).await.expect("tenant pool");
            let rows = audit::fetch_for_entity_pool(&pool, "sweep1229_note", "1")
                .await
                .expect("audit rows");
            sources.push(rows.first().map(|e| e.source.clone()));
        }
        assert_eq!(sources[0].as_deref(), Some("user:42"), "tenant A");
        assert_eq!(sources[1].as_deref(), Some("system"), "tenant B");
    }

    static POOLS_2123: OnceLock<(TenantPools<sqlx::Sqlite>, Org, Org)> = OnceLock::new();
    static DONE_2123: Mutex<bool> = Mutex::new(false);

    /// Writes to its own tenant through `with_tenant`, to the other one
    /// the same way, to its own through a bare `scoped_pool_dyn`, then to
    /// B's pool from inside A's scope.
    #[derive(serde::Serialize, serde::Deserialize)]
    struct WriteOwnTenant;

    #[async_trait::async_trait]
    impl Job for WriteOwnTenant {
        const NAME: &'static str = "sweep2123_own";
        async fn run(&self) -> Result<(), JobError> {
            let (pools, a, b) = POOLS_2123.get().unwrap();
            let insert = |title: &'static str| {
                move |pool: rustango::sql::Pool| async move {
                    let mut note = Note {
                        id: Auto::default(),
                        title: title.into(),
                    };
                    note.insert_pool(&pool).await.expect("insert");
                }
            };
            with_tenant(pools, a, insert("own")).await.expect("pool a");
            with_tenant(pools, b, insert("other"))
                .await
                .expect("pool b");
            insert("bare")(pools.scoped_pool_dyn(a).await.expect("pool a")).await;
            // Tenant B's pool, captured inside A's scope.
            let pool_b = pools.scoped_pool_dyn(b).await.expect("pool b");
            with_tenant(pools, a, |_| insert("cross")(pool_b))
                .await
                .expect("pool a");
            *DONE_2123.lock().unwrap() = true;
            Ok(())
        }
    }

    /// #2123 — a job writing to its own tenant through `with_tenant`
    /// records the tenant user who dispatched it.
    #[tokio::test]
    async fn a_job_writing_to_its_own_tenant_keeps_the_user() {
        let a = db_org("a_2123", &shared_mem("sweep2123_a"), true);
        let b = db_org("b_2123", &shared_mem("sweep2123_b"), true);
        let registry = registry_with(&[a.clone(), b.clone()]).await;
        let (pools, a, b) =
            POOLS_2123.get_or_init(|| (TenantPools::new(registry), a.clone(), b.clone()));
        for org in [a, b] {
            let pool = pools.scoped_pool_dyn(org).await.expect("tenant pool");
            rustango::testkit::matrix::fresh_table::<Note>(&pool).await;
            audit::ensure_table_pool(&pool).await.expect("audit table");
        }

        let q = InMemoryJobQueue::with_workers(1);
        q.register::<WriteOwnTenant>().await;
        q.start().await;
        let user = AuditSource::User { id: "42".into() };
        with_tenant_source(user, "a_2123".into(), async {
            q.dispatch(&WriteOwnTenant).await.unwrap();
        })
        .await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !*DONE_2123.lock().unwrap() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        q.shutdown().await;
        assert!(*DONE_2123.lock().unwrap(), "the job did not finish");

        let source = |org: &'static Org, pk: &'static str| async move {
            let pool = pools.scoped_pool_dyn(org).await.expect("tenant pool");
            let rows = audit::fetch_for_entity_pool(&pool, "sweep1229_note", pk)
                .await
                .expect("audit rows");
            rows.first().map(|e| e.source.clone())
        };
        assert_eq!(source(a, "1").await.as_deref(), Some("user:42"), "own");
        assert_eq!(source(b, "1").await.as_deref(), Some("system"), "other");
        assert_eq!(source(a, "2").await.as_deref(), Some("system"), "bare");
        assert_eq!(
            source(b, "2").await.as_deref(),
            Some("system"),
            "B inside A"
        );
    }
}
