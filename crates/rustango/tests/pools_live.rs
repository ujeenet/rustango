#![cfg(all(feature = "tenancy", feature = "postgres"))]
//! Live tests for `TenantPools`. Bootstraps a registry + 2 schema-mode
//! tenants (acme + globex), exercises pool resolution and search_path
//! scoping. Database-mode tenants are tested separately because they
//! need a second Postgres database to point at.
//!
//! Reads `DATABASE_URL`. Skips silently when unset.

use rustango::migrate;
use rustango::sql::{sqlx, Auto};
use rustango::tenancy::{Org, StorageMode, TenantPool, TenantPools};

use tokio::sync::Mutex;

/// Suite-wide lock. Every test in this file resets the shared PG
/// schema; under cargo's default parallel harness two tests would race
/// on PG's `pg_type_typname_nsp_index` / `pg_class_relname_nsp_index`
/// system-catalog uniques when both try to CREATE/DROP the same table
/// at once.
fn live_lock() -> &'static Mutex<()> {
    use std::sync::OnceLock;
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    Some(
        sqlx::PgPool::connect(&url)
            .await
            .expect("connect to DATABASE_URL"),
    )
}

async fn seed_org(
    pool: &sqlx::PgPool,
    slug: &str,
    mode: StorageMode,
    schema_name: Option<&str>,
    database_url: Option<&str>,
) -> Org {
    let mut org = Org {
        id: Auto::default(),
        slug: slug.into(),
        display_name: slug.to_uppercase(),
        storage_mode: mode.as_str().into(),
        backend_kind: "postgres".to_owned(),
        database_url: database_url.map(str::to_owned),
        schema_name: schema_name.map(str::to_owned),
        host_pattern: Some(format!("{slug}.app.test")),
        port: None,
        path_prefix: None,
        ..rustango::testkit::org()
    };
    org.insert(pool).await.unwrap();
    org
}

async fn create_schema(pool: &sqlx::PgPool, name: &str) {
    let sql = format!(r#"CREATE SCHEMA IF NOT EXISTS "{name}""#);
    sqlx::query(&sql).execute(pool).await.unwrap();
}

async fn drop_schema(pool: &sqlx::PgPool, name: &str) {
    let sql = format!(r#"DROP SCHEMA IF EXISTS "{name}" CASCADE"#);
    sqlx::query(&sql).execute(pool).await.unwrap();
}

async fn current_schema(conn: &mut sqlx::PgConnection) -> String {
    let row: (String,) = sqlx::query_as("SELECT current_schema()::text")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    row.0
}

#[tokio::test]
async fn schema_mode_pool_for_org_returns_schema_variant() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();
    drop_schema(&pool, "acme_tp_t1").await;

    let acme = seed_org(
        &pool,
        "acme_tp_t1",
        StorageMode::Schema,
        Some("acme_tp_t1"),
        None,
    )
    .await;

    let pools = TenantPools::new(pool.clone());
    let tp = pools.pool_for_org(&acme).await.unwrap();
    assert!(tp.is_schema(), "schema mode org → TenantPool::Schema");
    match tp {
        TenantPool::Schema { schema, .. } => assert_eq!(schema, "acme_tp_t1"),
        TenantPool::Database { .. } => panic!("expected Schema variant"),
    }

    drop_schema(&pool, "acme_tp_t1").await;
    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn schema_mode_acquire_sets_search_path_to_tenant_schema() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();
    drop_schema(&pool, "acme_tp_t2").await;
    create_schema(&pool, "acme_tp_t2").await;

    let acme = seed_org(
        &pool,
        "acme_tp_t2",
        StorageMode::Schema,
        Some("acme_tp_t2"),
        None,
    )
    .await;

    let pools = TenantPools::new(pool.clone());
    let mut conn = pools.acquire(&acme).await.unwrap();
    let schema = current_schema(&mut conn).await;
    assert_eq!(
        schema, "acme_tp_t2",
        "current_schema should be the tenant's"
    );
    assert_eq!(conn.schema(), Some("acme_tp_t2"));

    drop_schema(&pool, "acme_tp_t2").await;
    migrate::drop_all(&pool).await.unwrap();
}

/// Schema-mode `acquire` issues a session-level `SET search_path` on a
/// connection borrowed from the **shared registry pool**. Dropping the
/// `TenantConn` must leave that connection scoped back at `public`,
/// because the very next borrower may be a registry query, a
/// `Tenant::pool()` handler, or a long-lived background worker — none of
/// which issue a `SET` of their own. sqlx does not reset session state
/// on release (it only pings), so the reset is the framework's job.
///
/// Pinned to `max_connections(1)` so the second checkout is guaranteed to
/// be the same physical connection the tenant just used.
#[tokio::test]
async fn schema_mode_acquire_resets_search_path_on_release() {
    let _g = live_lock().lock().await;
    let Some(url) = std::env::var("DATABASE_URL").ok() else {
        return;
    };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("connect to DATABASE_URL");

    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();
    drop_schema(&pool, "acme_tp_leak").await;
    create_schema(&pool, "acme_tp_leak").await;

    let acme = seed_org(
        &pool,
        "acme_tp_leak",
        StorageMode::Schema,
        Some("acme_tp_leak"),
        None,
    )
    .await;

    // Baseline BEFORE any tenant touches the pool. Asserting against
    // this rather than a hardcoded "public" keeps the test honest under
    // a `DATABASE_URL` carrying `options=-csearch_path=…`, or a
    // role-level default — `RESET search_path` restores the session's
    // startup value, which is not always `public`.
    let baseline = {
        let mut conn = pool.acquire().await.unwrap();
        current_schema(&mut conn).await
    };

    let pools = TenantPools::new(pool.clone());
    {
        let mut conn = pools.acquire(&acme).await.unwrap();
        assert_eq!(current_schema(&mut conn).await, "acme_tp_leak");
    } // TenantConn dropped — connection goes back to the registry pool.

    // A plain registry checkout — what a background worker or a
    // `Tenant::pool()` query gets. It must NOT inherit the tenant's
    // search_path.
    let mut plain = pool.acquire().await.unwrap();
    let after = current_schema(&mut plain).await;
    assert_ne!(
        after, "acme_tp_leak",
        "registry connection inherited the tenant's search_path after release"
    );
    assert_eq!(
        after, baseline,
        "release must restore the session's startup search_path, not just move off the tenant's"
    );
    drop(plain);

    drop_schema(&pool, "acme_tp_leak").await;
    migrate::drop_all(&pool).await.unwrap();
}

/// The same leak stated as data rather than as a setting: a table name
/// that exists in **both** the tenant schema and `public` must resolve to
/// `public` for a plain registry checkout. This is the shape a background
/// worker or a `Tenant::pool()` handler actually hits — it reads rows, not
/// `current_schema()`.
#[tokio::test]
async fn released_registry_connection_does_not_read_tenant_rows() {
    let _g = live_lock().lock().await;
    let Some(url) = std::env::var("DATABASE_URL").ok() else {
        return;
    };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("connect to DATABASE_URL");

    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();
    drop_schema(&pool, "acme_tp_rows").await;
    create_schema(&pool, "acme_tp_rows").await;

    // Same table name in both namespaces: one tenant row, zero public rows.
    for stmt in [
        "DROP TABLE IF EXISTS public.leak_probe",
        "CREATE TABLE public.leak_probe (id int)",
        r#"CREATE TABLE "acme_tp_rows".leak_probe (id int)"#,
        r#"INSERT INTO "acme_tp_rows".leak_probe (id) VALUES (1)"#,
    ] {
        sqlx::query(stmt).execute(&pool).await.unwrap();
    }

    let acme = seed_org(
        &pool,
        "acme_tp_rows",
        StorageMode::Schema,
        Some("acme_tp_rows"),
        None,
    )
    .await;

    let pools = TenantPools::new(pool.clone());
    {
        let mut conn = pools.acquire(&acme).await.unwrap();
        let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM leak_probe")
            .fetch_one(&mut **conn)
            .await
            .unwrap();
        assert_eq!(n, 1, "the tenant's own connection should see its row");
    }

    let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM leak_probe")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        n, 0,
        "a released registry connection read the tenant's rows — search_path leaked"
    );

    sqlx::query("DROP TABLE IF EXISTS public.leak_probe")
        .execute(&pool)
        .await
        .unwrap();
    drop_schema(&pool, "acme_tp_rows").await;
    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn schema_mode_pool_uses_slug_when_schema_name_is_none() {
    // `schema_name = None` means "use the slug as the schema name".
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();
    drop_schema(&pool, "fallback_tp_t3").await;
    create_schema(&pool, "fallback_tp_t3").await;

    let fallback = seed_org(&pool, "fallback_tp_t3", StorageMode::Schema, None, None).await;

    let pools = TenantPools::new(pool.clone());
    let mut conn = pools.acquire(&fallback).await.unwrap();
    assert_eq!(current_schema(&mut conn).await, "fallback_tp_t3");

    drop_schema(&pool, "fallback_tp_t3").await;
    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn database_mode_without_database_url_errors() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();

    let bad = seed_org(&pool, "bad_tp_t4", StorageMode::Database, None, None).await;
    let pools = TenantPools::new(pool.clone());
    let err = pools.pool_for_org(&bad).await.unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("database_url"), "got: {msg}");

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn database_mode_pool_caches_per_slug() {
    // Database-mode pool building is cached. We can't easily spin
    // up a second Postgres for the test, so we point the database_url
    // at the registry itself — that's a degenerate but valid
    // configuration for proving the cache shape.
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();

    let dbm = seed_org(&pool, "dbm_tp_t5", StorageMode::Database, None, Some(&url)).await;

    let pools = TenantPools::new(pool.clone());
    assert_eq!(pools.cached_database_pool_count().await, 0);

    // First lookup builds the pool and caches it.
    let _tp1 = pools.pool_for_org(&dbm).await.unwrap();
    assert_eq!(pools.cached_database_pool_count().await, 1);

    // Second lookup hits the cache; count stays at 1.
    let _tp2 = pools.pool_for_org(&dbm).await.unwrap();
    assert_eq!(pools.cached_database_pool_count().await, 1);

    // Invalidate drops the cached pool.
    pools.invalidate(&dbm.slug).await;
    assert_eq!(pools.cached_database_pool_count().await, 0);

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn database_mode_resolves_secret_reference_via_resolver() {
    // EnvSecretsResolver: store an env:// reference in the Org row,
    // resolver picks it up, builds the pool from the resolved URL.
    // We still point at the registry DB (only Postgres available
    // in the test harness); the test demonstrates the indirection
    // works.
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let url_var = "DATABASE_URL";
    if std::env::var(url_var).is_err() {
        return;
    }
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();

    let dbm = seed_org(
        &pool,
        "secret_tp_t6",
        StorageMode::Database,
        None,
        Some(&format!("env://{url_var}")),
    )
    .await;

    let pools = TenantPools::with_secrets(
        pool.clone(),
        rustango::tenancy::ChainSecretsResolver::standard(),
    );
    let tp = pools.pool_for_org(&dbm).await.unwrap();
    assert!(!tp.is_schema(), "database-mode tenant");

    migrate::drop_all(&pool).await.unwrap();
}

// ---------------------------------------------------------------- #1235
// Schema-mode scoped pools are cached per tenant. They used to be
// rebuilt on every call, and the `Tenant` extractor calls
// `scoped_pool` once per request — so a schema-mode app paid a full
// connection establishment per request, in the mode that exists to
// reduce per-tenant connection overhead.
//
// These lean on `PgPool::size()` (connections the pool currently
// owns) as the observable: a cached pool carries its predecessor's
// connections, a freshly built one does not.

#[tokio::test]
async fn scoped_pool_is_cached_per_tenant() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();
    drop_schema(&pool, "acme_sp_cache").await;
    create_schema(&pool, "acme_sp_cache").await;

    let acme = seed_org(
        &pool,
        "acme_sp_cache",
        StorageMode::Schema,
        Some("acme_sp_cache"),
        None,
    )
    .await;
    let pools = TenantPools::new(pool.clone());

    // Saturate the first pool: default `scoped_pool_max_connections`
    // is 2, so holding two checkouts puts its size at 2.
    let first = pools.scoped_pool(&acme).await.unwrap();
    let _c1 = first.acquire().await.unwrap();
    let _c2 = first.acquire().await.unwrap();
    assert_eq!(first.size(), 2, "expected both connections established");

    // A cached pool IS the first one, so it reports the same size.
    // Pre-#1235 this was a brand-new pool and reported 1 — the single
    // connection `connect_with` opens eagerly.
    let second = pools.scoped_pool(&acme).await.unwrap();
    assert_eq!(
        second.size(),
        2,
        "second scoped_pool must reuse the cached pool, not build a new one",
    );

    drop(_c1);
    drop(_c2);
    drop_schema(&pool, "acme_sp_cache").await;
    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn scoped_pool_cache_is_keyed_per_tenant_and_stays_scoped() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();
    for s in ["acme_sp_key", "globex_sp_key"] {
        drop_schema(&pool, s).await;
        create_schema(&pool, s).await;
    }

    let acme = seed_org(
        &pool,
        "acme_sp_key",
        StorageMode::Schema,
        Some("acme_sp_key"),
        None,
    )
    .await;
    let globex = seed_org(
        &pool,
        "globex_sp_key",
        StorageMode::Schema,
        Some("globex_sp_key"),
        None,
    )
    .await;
    let pools = TenantPools::new(pool.clone());

    // Caching must not collapse two tenants onto one pool — each keeps
    // its own baked-in search_path. Interleaved on purpose, so a
    // single shared entry would show up.
    let a1 = pools.scoped_pool(&acme).await.unwrap();
    let g1 = pools.scoped_pool(&globex).await.unwrap();
    let a2 = pools.scoped_pool(&acme).await.unwrap();
    let g2 = pools.scoped_pool(&globex).await.unwrap();

    for (p, want) in [
        (&a1, "acme_sp_key"),
        (&a2, "acme_sp_key"),
        (&g1, "globex_sp_key"),
        (&g2, "globex_sp_key"),
    ] {
        let mut conn = p.acquire().await.unwrap();
        assert_eq!(current_schema(&mut conn).await, want);
    }

    for s in ["acme_sp_key", "globex_sp_key"] {
        drop_schema(&pool, s).await;
    }
    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn invalidate_drops_the_scoped_pool_too() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();
    drop_schema(&pool, "acme_sp_inval").await;
    create_schema(&pool, "acme_sp_inval").await;

    let acme = seed_org(
        &pool,
        "acme_sp_inval",
        StorageMode::Schema,
        Some("acme_sp_inval"),
        None,
    )
    .await;
    let pools = TenantPools::new(pool.clone());

    let first = pools.scoped_pool(&acme).await.unwrap();
    let c1 = first.acquire().await.unwrap();
    let c2 = first.acquire().await.unwrap();
    assert_eq!(first.size(), 2);
    drop(c1);
    drop(c2);

    // Without this eviction a tenant whose `schema_name` changed would
    // keep being handed a pool with the OLD schema baked into its
    // connect options — a cross-tenant read.
    pools.invalidate(&acme.slug).await;

    let rebuilt = pools.scoped_pool(&acme).await.unwrap();
    assert!(
        rebuilt.size() < 2,
        "invalidate must drop the scoped pool; got a pool with {} connections \
         (i.e. the cached one)",
        rebuilt.size(),
    );

    drop_schema(&pool, "acme_sp_inval").await;
    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn scoped_pool_past_the_cap_falls_back_instead_of_failing() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();
    for s in ["acme_sp_cap", "globex_sp_cap"] {
        drop_schema(&pool, s).await;
        create_schema(&pool, s).await;
    }

    let acme = seed_org(
        &pool,
        "acme_sp_cap",
        StorageMode::Schema,
        Some("acme_sp_cap"),
        None,
    )
    .await;
    let globex = seed_org(
        &pool,
        "globex_sp_cap",
        StorageMode::Schema,
        Some("globex_sp_cap"),
        None,
    )
    .await;

    // Cap of 1: the second tenant cannot be cached. Schema mode is
    // sold for high tenant counts, so exceeding the cap must degrade
    // to the old per-call build — never error.
    let cfg = rustango::tenancy::TenantPoolsConfig {
        max_cached_scoped_pools: 1,
        ..Default::default()
    };
    let pools = TenantPools::new(pool.clone()).config(cfg);

    let a = pools.scoped_pool(&acme).await.unwrap();
    let g = pools.scoped_pool(&globex).await.unwrap();

    // Both still work, and both are still correctly scoped — the
    // uncached one is just rebuilt each time.
    let mut ca = a.acquire().await.unwrap();
    assert_eq!(current_schema(&mut ca).await, "acme_sp_cap");
    let mut cg = g.acquire().await.unwrap();
    assert_eq!(current_schema(&mut cg).await, "globex_sp_cap");

    let g_again = pools.scoped_pool(&globex).await.unwrap();
    let mut cg2 = g_again.acquire().await.unwrap();
    assert_eq!(current_schema(&mut cg2).await, "globex_sp_cap");

    for s in ["acme_sp_cap", "globex_sp_cap"] {
        drop_schema(&pool, s).await;
    }
    migrate::drop_all(&pool).await.unwrap();
}
