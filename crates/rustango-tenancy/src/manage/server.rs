//! `run-server` verb — boot the operator console + tenant admin
//! using the project's existing models. Thin wrapper around
//! [`crate::server::run`].

use std::io::Write;

use crate::error::TenancyError;
use crate::manage::args::next_value;
use crate::pools::TenantPools;

pub(super) async fn run_server_cmd<W: Write + Send>(
    pools: &TenantPools,
    registry_url: &str,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError> {
    let mut cfg = crate::server::ServerConfig::from_env();
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
    let arc_pools = std::sync::Arc::new(crate::TenantPools::new(pools.registry().clone()));
    crate::server::run(arc_pools, registry_url.to_owned(), cfg, w).await
}
