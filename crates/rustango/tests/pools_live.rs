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

/// #2325: a legacy `Acme` schema stays `Acme`, not the `acme` another
/// tenant owns; a space in the name survives the startup option.
#[tokio::test]
async fn scoped_pool_quotes_the_schema_name() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();
    let schemas = ["Acme_sp_q", "acme_sp_q", "acme sp q"];
    for s in schemas {
        drop_schema(&pool, s).await;
        create_schema(&pool, s).await;
    }
    let pools = TenantPools::new(pool.clone());
    for (slug, schema) in [("acme_q_upper", "Acme_sp_q"), ("acme_q_space", "acme sp q")] {
        let org = seed_org(&pool, slug, StorageMode::Schema, Some(schema), None).await;
        let scoped = pools.scoped_pool(&org).await.expect("scoped pool");
        let mut conn = scoped.acquire().await.unwrap();
        assert_eq!(current_schema(&mut conn).await, schema);
        let mut conn = pools.acquire(&org).await.unwrap();
        assert_eq!(current_schema(&mut conn).await, schema);
    }
    for s in schemas {
        drop_schema(&pool, s).await;
    }
    migrate::drop_all(&pool).await.unwrap();
}

/// A schema change made by another process reaches this cache: the fresh
/// Org names another schema, so the cached pool is a miss (#1882).
#[tokio::test]
async fn scoped_pool_follows_a_schema_change_without_invalidate() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();
    for s in ["acme_src_old", "acme_src_new"] {
        drop_schema(&pool, s).await;
        create_schema(&pool, s).await;
    }
    let mut acme = seed_org(
        &pool,
        "acme_src",
        StorageMode::Schema,
        Some("acme_src_old"),
        None,
    )
    .await;
    let pools = TenantPools::new(pool.clone());
    let old = pools.scoped_pool(&acme).await.unwrap();
    assert_eq!(
        current_schema(&mut old.acquire().await.unwrap()).await,
        "acme_src_old"
    );

    // What `edit-tenant` in another process leaves: a new row, no evict here.
    acme.schema_name = Some("acme_src_new".into());
    let new = pools.scoped_pool(&acme).await.unwrap();
    assert_eq!(
        current_schema(&mut new.acquire().await.unwrap()).await,
        "acme_src_new",
        "served the pool cached for the old schema",
    );
    assert_eq!(pools.cached_scoped_pool_count().await, 1);

    for s in ["acme_src_old", "acme_src_new"] {
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
async fn scoped_pool_past_the_cap_still_serves_every_tenant() {
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

    // Cap of 1: the second tenant evicts the first. Exceeding the cap
    // must never error. The budget half is pinned by
    // `scoped_pool_over_cap_holds_the_connection_budget`.
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

/// Backends this database currently has open, excluding the counting
/// query's own connection.
async fn backend_count(pool: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM pg_stat_activity \
         WHERE datname = current_database() AND pid <> pg_backend_pid()",
    )
    .fetch_one(pool)
    .await
    .expect("count backends")
}

/// An over-cap schema-mode tenant must not open a fresh pool per
/// request (#1528). Asserts the connection count on the server — the
/// warning and the error type both pass against the broken code.
#[tokio::test]
async fn scoped_pool_over_cap_holds_the_connection_budget() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();
    for s in ["acme_sp_budget", "globex_sp_budget"] {
        drop_schema(&pool, s).await;
        create_schema(&pool, s).await;
    }
    let acme = seed_org(
        &pool,
        "acme_sp_budget",
        StorageMode::Schema,
        Some("acme_sp_budget"),
        None,
    )
    .await;
    let globex = seed_org(
        &pool,
        "globex_sp_budget",
        StorageMode::Schema,
        Some("globex_sp_budget"),
        None,
    )
    .await;

    let cfg = rustango::tenancy::TenantPoolsConfig {
        max_cached_scoped_pools: 1,
        ..Default::default()
    };
    let pools = TenantPools::new(pool.clone()).config(cfg);

    // Hold the handle so eviction does not close its connections
    // mid-measurement.
    let _warm = pools.scoped_pool(&acme).await.unwrap();
    let before = backend_count(&pool).await;

    const REQUESTS: usize = 12;
    let mut held = Vec::with_capacity(REQUESTS);
    for _ in 0..REQUESTS {
        held.push(pools.scoped_pool(&globex).await.unwrap());
    }
    let after = backend_count(&pool).await;
    let opened = after - before;

    // Budget is cap x scoped_pool_max_connections, plus slack for the
    // registry pool. The failure caught is one pool per request: +12.
    // Read off a fresh default rather than `pools.pool_config()`:
    // CodeQL taints anything reachable from `with_secrets` and flags
    // this count as a secret reaching a panic message.
    let budget =
        i64::from(rustango::tenancy::TenantPoolsConfig::default().scoped_pool_max_connections) + 2;
    assert!(
        opened <= budget,
        "{REQUESTS} requests for an over-cap tenant opened {opened} \
         connections (budget {budget}). A pool per request is the \
         #1528 outage.",
    );

    drop(held);
    drop(_warm);
    for s in ["acme_sp_budget", "globex_sp_budget"] {
        drop_schema(&pool, s).await;
    }
    migrate::drop_all(&pool).await.unwrap();
}

/// A resolver that returns one org without touching the registry.
struct FixedOrg(Org);

#[async_trait::async_trait]
impl rustango::tenancy::OrgResolver for FixedOrg {
    async fn resolve(
        &self,
        _: &axum::http::request::Parts,
        _: &rustango::sql::Pool,
    ) -> Result<Option<Org>, rustango::tenancy::TenancyError> {
        Ok(Some(self.0.clone()))
    }
}

/// Request parts carrying a `TenantContext` that resolves `org`.
fn tenant_parts(pools: std::sync::Arc<TenantPools>, org: Org) -> axum::http::request::Parts {
    use rustango::extractors::TenantContext;
    use rustango::session::SessionSecret;
    let ctx = TenantContext::<sqlx::Postgres> {
        pools,
        resolver: rustango::tenancy::ChainResolver::new().push(FixedOrg(org)),
        session_secret: SessionSecret::from_bytes(vec![7; 32]),
        operator_secret: SessionSecret::from_bytes(vec![8; 32]),
    };
    let mut parts = axum::http::Request::new(()).into_parts().0;
    parts.extensions.insert(std::sync::Arc::new(ctx));
    parts
}

fn schema_org(schema: &str) -> Org {
    Org {
        slug: schema.into(),
        storage_mode: StorageMode::Schema.as_str().into(),
        backend_kind: "postgres".to_owned(),
        schema_name: Some(schema.to_owned()),
        ..rustango::testkit::org()
    }
}

/// Requests that each hold a schema-mode `Tenant` and also query through
/// `t.pool()` must not starve the scoped pool, even at its max size.
#[tokio::test]
async fn schema_mode_tenants_using_pool_do_not_starve_it() {
    use axum::extract::FromRequestParts as _;
    use std::sync::Arc;
    let _g = live_lock().lock().await;
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let admin = pool().await.unwrap();
    create_schema(&admin, "acme_tp_many").await;
    let registry = sqlx::postgres::PgPoolOptions::new()
        .max_connections(16)
        .connect(&url)
        .await
        .unwrap();
    let cfg = rustango::tenancy::TenantPoolsConfig {
        database_pool_acquire_timeout: std::time::Duration::from_secs(3),
        ..Default::default()
    };
    let n = cfg.scoped_pool_max_connections as usize;
    let pools = Arc::new(TenantPools::new(registry).config(cfg));
    // Every request holds its `Tenant` before any of them queries the pool.
    let barrier = Arc::new(tokio::sync::Barrier::new(n));
    let tasks: Vec<_> = (0..n)
        .map(|_| {
            let mut parts = tenant_parts(pools.clone(), schema_org("acme_tp_many"));
            let barrier = barrier.clone();
            tokio::spawn(async move {
                let mut t = rustango::extractors::Tenant::<sqlx::Postgres>::from_request_parts(
                    &mut parts,
                    &(),
                )
                .await
                .map_err(|_| "extractor rejected".to_owned())?;
                barrier.wait().await;
                let pg = t.pool().as_postgres().expect("a Postgres pool").clone();
                let row: (String,) = sqlx::query_as("SELECT current_schema()::text")
                    .fetch_one(&pg)
                    .await
                    .map_err(|e| e.to_string())?;
                assert_eq!(current_schema(t.conn()).await, "acme_tp_many");
                Ok::<_, String>(row.0)
            })
        })
        .collect();
    for task in tasks {
        let got = tokio::time::timeout(std::time::Duration::from_secs(30), task)
            .await
            .expect("request hung")
            .unwrap();
        assert_eq!(got.expect("t.pool() query"), "acme_tp_many");
    }
    drop_schema(&admin, "acme_tp_many").await;
}

/// The same in database mode: each held `Tenant` used to pin a slot of the
/// pool `t.pool()` queries, so at its max size every query timed out.
#[tokio::test]
async fn database_mode_tenants_using_pool_do_not_starve_it() {
    use axum::extract::FromRequestParts as _;
    use std::sync::Arc;
    let _g = live_lock().lock().await;
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let registry = pool().await.unwrap();
    let cfg = rustango::tenancy::TenantPoolsConfig {
        database_pool_max_connections: 2,
        database_pool_acquire_timeout: std::time::Duration::from_secs(3),
        ..Default::default()
    };
    let n = cfg.database_pool_max_connections as usize;
    let pools = Arc::new(TenantPools::new(registry).config(cfg));
    let org = Org {
        slug: "dbm_tp_many".into(),
        storage_mode: StorageMode::Database.as_str().into(),
        backend_kind: "postgres".to_owned(),
        database_url: Some(url),
        ..rustango::testkit::org()
    };
    let barrier = Arc::new(tokio::sync::Barrier::new(n));
    let tasks: Vec<_> = (0..n)
        .map(|_| {
            let mut parts = tenant_parts(pools.clone(), org.clone());
            let barrier = barrier.clone();
            tokio::spawn(async move {
                let mut t = rustango::extractors::Tenant::<sqlx::Postgres>::from_request_parts(
                    &mut parts,
                    &(),
                )
                .await
                .map_err(|_| "extractor rejected".to_owned())?;
                barrier.wait().await;
                let pg = t.pool().as_postgres().expect("a Postgres pool").clone();
                let row: (i32,) = sqlx::query_as("SELECT 1")
                    .fetch_one(&pg)
                    .await
                    .map_err(|e| e.to_string())?;
                let _ = current_schema(t.conn()).await;
                Ok::<_, String>(row.0)
            })
        })
        .collect();
    for task in tasks {
        let got = tokio::time::timeout(std::time::Duration::from_secs(30), task)
            .await
            .expect("request hung")
            .unwrap();
        assert_eq!(got.expect("t.pool() query"), 1);
    }
}

/// `TenantScope` holds no connection, so many of them fit a one-slot pool.
#[tokio::test]
async fn tenant_scope_holds_no_connection() {
    use axum::extract::FromRequestParts as _;
    use rustango::extractors::TenantScope;
    let _g = live_lock().lock().await;
    let Some(admin) = pool().await else {
        return;
    };
    create_schema(&admin, "acme_tp_scope").await;
    let cfg = rustango::tenancy::TenantPoolsConfig {
        scoped_pool_max_connections: 1,
        database_pool_acquire_timeout: std::time::Duration::from_secs(2),
        ..Default::default()
    };
    let pools = TenantPools::new(admin.clone()).config(cfg);
    let mut parts = tenant_parts(std::sync::Arc::new(pools), schema_org("acme_tp_scope"));
    let first = TenantScope::from_request_parts(&mut parts, &())
        .await
        .unwrap();
    let second = TenantScope::from_request_parts(&mut parts, &())
        .await
        .unwrap();
    assert_eq!(first.org.slug, "acme_tp_scope");
    let pg = second.pool().as_postgres().expect("a Postgres pool");
    let mut conn = pg.acquire().await.expect("the one slot is free");
    assert_eq!(current_schema(&mut conn).await, "acme_tp_scope");
    drop(conn);
    drop((first, second));
    drop_schema(&admin, "acme_tp_scope").await;
}

/// One busy schema-mode tenant must not block the others or the registry:
/// the extractor's held connections come from that tenant's own pool.
#[tokio::test]
async fn a_busy_schema_tenant_does_not_block_other_tenants() {
    use axum::extract::FromRequestParts as _;
    use std::sync::Arc;
    use std::time::Duration;
    let _g = live_lock().lock().await;
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let admin = pool().await.unwrap();
    for s in ["acme_tp_busy", "globex_tp_busy"] {
        create_schema(&admin, s).await;
    }
    let registry = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(2))
        .connect(&url)
        .await
        .unwrap();
    let cfg = rustango::tenancy::TenantPoolsConfig {
        scoped_pool_max_connections: 2,
        database_pool_acquire_timeout: Duration::from_secs(2),
        ..Default::default()
    };
    let pools = Arc::new(TenantPools::new(registry.clone()).config(cfg));
    let tenant = |schema: &str| {
        let mut parts = tenant_parts(pools.clone(), schema_org(schema));
        async move {
            rustango::extractors::Tenant::<sqlx::Postgres>::from_request_parts(&mut parts, &())
                .await
                .map_err(|_| "extractor rejected")
        }
    };
    // As many in-flight acme requests as the registry pool has slots.
    let mut busy = Vec::new();
    for _ in 0..2 {
        busy.push(tenant("acme_tp_busy").await.expect("acme request"));
    }
    let mut other = tokio::time::timeout(Duration::from_secs(5), tenant("globex_tp_busy"))
        .await
        .expect("globex waited for acme's connections")
        .expect("globex request");
    assert_eq!(current_schema(other.conn()).await, "globex_tp_busy");
    for t in &mut busy {
        assert_eq!(current_schema(t.conn()).await, "acme_tp_busy");
        // `t.pool()` stays usable while every held slot is taken.
        let pg = t.pool().as_postgres().unwrap().clone();
        let mut c = pg.acquire().await.expect("t.pool() starved");
        assert_eq!(current_schema(&mut c).await, "acme_tp_busy");
    }
    let mut reg = tokio::time::timeout(Duration::from_secs(5), registry.acquire())
        .await
        .expect("the registry pool is exhausted")
        .unwrap();
    assert_eq!(current_schema(&mut reg).await, "public");
    drop((busy, other, reg));
    for s in ["acme_tp_busy", "globex_tp_busy"] {
        drop_schema(&admin, s).await;
    }
}

/// With the scoped-pool cache off, a held connection comes from the
/// registry pool, not from a pool built per request.
#[tokio::test]
async fn uncached_schema_tenant_holds_a_registry_connection() {
    use std::time::Duration;
    let _g = live_lock().lock().await;
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let admin = pool().await.unwrap();
    create_schema(&admin, "acme_tp_nocache").await;
    let registry = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(2))
        .connect(&url)
        .await
        .unwrap();
    let cfg = rustango::tenancy::TenantPoolsConfig {
        max_cached_scoped_pools: 0,
        ..Default::default()
    };
    let pools = TenantPools::new(registry.clone()).config(cfg);
    let org = schema_org("acme_tp_nocache");
    {
        let mut conn = pools.acquire(&org).await.unwrap();
        assert_eq!(current_schema(&mut conn).await, "acme_tp_nocache");
        assert!(
            registry.try_acquire().is_none(),
            "built a pool for one request"
        );
    }
    let mut plain = tokio::time::timeout(Duration::from_secs(5), registry.acquire())
        .await
        .expect("the connection never came back")
        .unwrap();
    assert_eq!(
        current_schema(&mut plain).await,
        "public",
        "search_path leaked"
    );
    drop(plain);
    drop_schema(&admin, "acme_tp_nocache").await;
}

/// A registry connection switched between schemas drops its prepared
/// statements: reused, one would keep the other table's result type (0A000).
#[tokio::test]
async fn uncached_schema_tenants_do_not_share_prepared_statements() {
    use std::time::Duration;
    let _g = live_lock().lock().await;
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let admin = pool().await.unwrap();
    for (schema, ty) in [("acme_tp_stmt", "BIGINT"), ("globex_tp_stmt", "TEXT")] {
        drop_schema(&admin, schema).await;
        create_schema(&admin, schema).await;
        let sql = format!(r#"CREATE TABLE "{schema}".tp_stmt (v {ty})"#);
        sqlx::query(&sql).execute(&admin).await.unwrap();
    }
    let registry = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&url)
        .await
        .unwrap();
    let cfg = rustango::tenancy::TenantPoolsConfig {
        max_cached_scoped_pools: 0,
        ..Default::default()
    };
    let pools = TenantPools::new(registry.clone()).config(cfg);
    for schema in ["acme_tp_stmt", "globex_tp_stmt", "acme_tp_stmt"] {
        let mut conn = pools.acquire(&schema_org(schema)).await.unwrap();
        sqlx::query("SELECT v FROM tp_stmt")
            .fetch_all(&mut **conn)
            .await
            .unwrap_or_else(|e| panic!("{schema}: {e}"));
    }
    drop(pools);
    registry.close().await;
    for schema in ["acme_tp_stmt", "globex_tp_stmt"] {
        drop_schema(&admin, schema).await;
    }
}

/// A schema-mode tenant's migrate takes a lock of its own: another run
/// holding the registry's does not make it wait.
#[tokio::test]
async fn schema_tenant_migrate_lock_is_its_own() {
    use rustango::migrate::{migrate_pool_with_ledger, with_lock_timeout, MigrateError};
    use std::time::Duration;
    let _g = live_lock().lock().await;
    let Some(admin) = pool().await else {
        return;
    };
    let schema = "acme_tp_lock";
    drop_schema(&admin, schema).await;
    create_schema(&admin, schema).await;
    let dir = tempfile::tempdir().unwrap();
    let snapshot: migrate::SchemaSnapshot = serde_json::from_value(serde_json::json!({
        "tables": [{"name": "tp_lock_t", "model": "tp_lock_t", "fields": [
            {"name": "id", "column": "id", "ty": "i64", "nullable": false,
             "primary_key": true}]}]
    }))
    .unwrap();
    migrate::make_migrations_from(dir.path(), &snapshot, None)
        .unwrap()
        .unwrap();
    // Another run holds the registry's (`public`) lock.
    let mut holder = admin.acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock(5932739705669502804)")
        .execute(&mut *holder)
        .await
        .unwrap();
    let pools = TenantPools::new(admin.clone());
    let tenant: rustango::sql::Pool = pools.scoped_pool(&schema_org(schema)).await.unwrap().into();
    let limit = Duration::from_secs(2);
    let ran = with_lock_timeout(
        limit,
        migrate_pool_with_ledger(&tenant, dir.path(), "tp_lock_ledger"),
    )
    .await;
    let registry: rustango::sql::Pool = admin.clone().into();
    let blocked = with_lock_timeout(
        limit,
        migrate_pool_with_ledger(&registry, dir.path(), "tp_lock_ledger"),
    )
    .await;
    sqlx::query("SELECT pg_advisory_unlock(5932739705669502804)")
        .execute(&mut *holder)
        .await
        .unwrap();
    drop(holder);
    drop_schema(&admin, schema).await;
    ran.expect("the tenant's migrate waited on the registry's lock");
    assert!(
        matches!(blocked, Err(MigrateError::LockTimeout(_))),
        "the registry's migrate still takes its lock: {blocked:?}"
    );
}
