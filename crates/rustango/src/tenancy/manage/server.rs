//! `run-server` verb — boot the operator console + tenant admin
//! using the project's existing models. Thin wrapper around
//! [`crate::tenancy::server::run`].
//!
//! v0.38 — PG-only because `tenancy::server::Builder` glues together
//! TenantPools<Postgres>, schema-mode dispatch, operator console
//! (PG-typed cookies), and the per-tenant admin builder. Sqlite/MySQL
//! tenancy projects mount their own axum routes against
//! `DatabaseTenant<DB>` + the ORM `_pool` helpers until the
//! Builder<DB> generic lift lands (queued for v0.39).

use std::io::Write;

use sqlx::Database;

use crate::tenancy::error::TenancyError;
#[cfg(feature = "postgres")]
use crate::tenancy::manage::args::next_value;
use crate::tenancy::pools::TenantPools;

#[cfg(feature = "postgres")]
pub(super) async fn run_server_cmd<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let mut cfg = crate::tenancy::server::ServerConfig::from_env();
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--bind" => cfg.bind = next_value(&mut iter, "--bind")?,
            "--apex" | "--apex-domain" => {
                cfg.apex_domain = next_value(&mut iter, "--apex")?;
            }
            "--help" | "-h" => {
                return Err(TenancyError::Validation(
                    "run-server [--bind <addr>] [--apex <domain>]\n  \
                     Boots the operator console (apex) + tenant admin\n  \
                     (subdomains) with sensible defaults. Reads RUSTANGO_BIND,\n  \
                     RUSTANGO_APEX_DOMAIN, RUSTANGO_SESSION_SECRET from env.\n  \
                     Ctrl-C to stop."
                        .into(),
                ));
            }
            other => {
                return Err(TenancyError::Validation(format!(
                    "run-server: unknown argument `{other}`"
                )));
            }
        }
    }
    // Pools is borrowed; the server takes an `Arc<TenantPools>` so it
    // can clone into per-request closures. Build a fresh Arc carrying
    // a clone of the registry pool — the existing pools' database-mode
    // cache stays distinct, but for `run-server` the freshly-built
    // registry uses the same connection-pool handle so the existing
    // cache isn't lost.
    // The PG-only `tenancy::server::Builder` consumes the concrete
    // `TenantPools<sqlx::Postgres>`. Downcast through Any — this
    // function is cfg(postgres)-gated so the cast either succeeds
    // (DB = Postgres at the type level) or we're being called from
    // dead code under a feature combination that shouldn't compile.
    let pg_pools = (pools as &dyn std::any::Any)
        .downcast_ref::<TenantPools<sqlx::Postgres>>()
        .ok_or_else(|| {
            TenancyError::Validation(
                "run-server requires TenantPools<sqlx::Postgres> — schema-mode dispatch \
                 + operator console are PG-only by language. Mount your own axum routes \
                 against DatabaseTenant<DB> for sqlite/mysql tenancy projects."
                    .into(),
            )
        })?;
    let arc_pools = std::sync::Arc::new(crate::tenancy::TenantPools::new(
        pg_pools.registry().clone(),
    ));
    crate::tenancy::server::run(arc_pools, registry_url.to_owned(), cfg, w).await
}

#[cfg(not(feature = "postgres"))]
pub(super) async fn run_server_cmd<W: Write + Send, DB: Database>(
    _pools: &TenantPools<DB>,
    _registry_url: &str,
    _args: &[String],
    _w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    Err(TenancyError::Validation(
        "run-server is PG-only today — the bundled multi-tenant server (TenantPools + \
         schema-mode dispatch + operator console) is wired through Postgres types. For \
         sqlite/mysql tenancy projects, mount your own axum routes against \
         DatabaseTenant<DB> + the ORM _pool helpers. The Builder<DB> generic lift is \
         queued for v0.39."
            .into(),
    ))
}
