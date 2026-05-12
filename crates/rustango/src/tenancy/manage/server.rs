//! `run-server` verb — boot the operator console + tenant admin
//! using the project's existing models. Thin wrapper around
//! [`crate::tenancy::server::run`].
//!
//! v0.38 — fully tri-dialect. `tenancy::server::run<DB>` boots the
//! operator console + tenant admin on whichever backend
//! `TenantPools<DB>` was built with. Schema-mode tenants remain
//! PG-only by language (`SET search_path`); database-mode tenants
//! work on any backend.

use std::io::Write;

use sqlx::Database;

use crate::tenancy::error::TenancyError;
use crate::tenancy::manage::args::next_value;
use crate::tenancy::pools::TenantPools;

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
    // v0.38 — `tenancy::server::run<DB>` is generic; the downcast to
    // `TenantPools<sqlx::Postgres>` that used to live here is gone.
    // Build a fresh Arc wrapping a re-borrow of `pools`' registry pool:
    // the caller hands us `&TenantPools<DB>` (no Arc) and the server's
    // closures need an `Arc<TenantPools<DB>>` they can clone per
    // request. Re-using the registry pool is cheap (sqlx::Pool is
    // Arc-shaped under the hood); the database-mode tenant cache on
    // `pools` is independent and remains usable elsewhere.
    let arc_pools = std::sync::Arc::new(TenantPools::<DB>::new(pools.registry_inner().clone()));
    crate::tenancy::server::run::<DB, _>(arc_pools, registry_url.to_owned(), cfg, w).await
}
