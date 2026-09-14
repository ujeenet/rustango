//! `TenantPoolsConfig` must be reachable from the builders apps
//! actually use (#1456).
//!
//! The type was public, documented in `docs/manage.md`, and
//! unreachable: every route to a running server built
//! `TenantPools::new(pool)`, which takes `TenantPoolsConfig::default()`,
//! and `tenancy/pools.rs` reads no environment variables — the
//! `RUSTANGO_DB_*` knobs reach `sql::Pool` only. So tenant pool sizing
//! could not be changed at all.
//!
//! That is not a cosmetic gap. Connections multiply by tenant *and* by
//! process: twenty database-mode tenants at the default 16, across a web
//! and a worker, is 640 against a stock PostgreSQL limit of 100. The
//! only lever was the database server's own `max_connections`.
//!
//! The assertion below is on the pool config a builder **actually
//! carries**, not on the setter returning `Self` — a setter that stores
//! a value nothing reads is exactly the bug being fixed.

#![cfg(all(feature = "tenancy", feature = "postgres"))]

use rustango::tenancy::{TenantPools, TenantPoolsConfig};

fn non_default() -> TenantPoolsConfig {
    TenantPoolsConfig {
        database_pool_max_connections: 3,
        max_cached_database_pools: 512,
        ..Default::default()
    }
}

/// Baseline: the defaults really are what the bug pinned everyone to.
/// If these ever change, the numbers quoted in #1456 and in
/// `docker/soak/README.md` need revisiting.
#[test]
fn the_defaults_are_the_ones_the_issue_describes() {
    let d = TenantPoolsConfig::default();
    assert_eq!(d.database_pool_max_connections, 16);
    assert_eq!(d.max_cached_database_pools, 64);
}

// `connect_lazy` still needs a Tokio context to build its internals,
// even though it contacts no server.
#[tokio::test]
async fn tenant_pools_carries_a_supplied_config() {
    let pool =
        rustango::sql::sqlx::PgPool::connect_lazy("postgres://user:pw@127.0.0.1:1/never_connected")
            .expect("lazy pool needs no server");

    let pools = TenantPools::new(pool).config(non_default());
    assert_eq!(pools.pool_config().database_pool_max_connections, 3);
    assert_eq!(pools.pool_config().max_cached_database_pools, 512);
}

/// `server::Builder::tenant_pools` is the setter the tenancy
/// `runserver` path routes through.
#[tokio::test]
async fn server_builder_applies_the_config() {
    let pool =
        rustango::sql::sqlx::PgPool::connect_lazy("postgres://user:pw@127.0.0.1:1/never_connected")
            .expect("lazy pool needs no server");

    let builder = rustango::server::Builder::<sqlx::Postgres>::from_pool(
        pool,
        "postgres://user:pw@127.0.0.1:1/never_connected",
        "example.test",
    );
    // Default before, supplied after — the pair is the point. Asserting
    // only the second would pass even if the setter were a no-op and 3
    // happened to be the default.
    assert_eq!(
        builder.pools().pool_config().database_pool_max_connections,
        16,
        "from_pool should start at the default"
    );
    let builder = builder.tenant_pools(non_default());
    assert_eq!(
        builder.pools().pool_config().database_pool_max_connections,
        3,
        "Builder::tenant_pools must reach the pools the server will use (#1456)"
    );
}

/// Every path that builds tenant pools must consult the override.
///
/// Recomputed from source: a new serve path that forgets it would
/// reintroduce exactly this bug, silently, for whoever uses that path.
#[test]
fn no_path_builds_tenant_pools_while_ignoring_the_override() {
    let src = include_str!("../src/manage.rs");

    // The management-verb dispatch path.
    let dispatch = src
        .find("let pools = match self.tenant_pools.clone()")
        .is_some();
    assert!(
        dispatch,
        "the management-verb dispatch path builds TenantPools without consulting \
         `self.tenant_pools`, so `Cli::with_tenant_pools` is a no-op there (#1456)"
    );

    // Both tenancy serve arms, which go through `Builder::from_pool`.
    let applied = src.matches("builder.tenant_pools(cfg)").count();
    assert_eq!(
        applied, 2,
        "expected both `runserver_tenancy` arms (postgres and non-postgres) to \
         apply the tenant pool override; found {applied}"
    );
}
