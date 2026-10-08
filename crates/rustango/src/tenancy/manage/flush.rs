//! `flush` in a tenancy project: one tenant's tables, never the registry (#2284).

use std::io::Write;

use sqlx::Database;

use crate::core::ModelScope;
use crate::tenancy::error::TenancyError;
use crate::tenancy::pools::TenantPools;

use super::args::next_value;

const USAGE: &str = "flush --tenant <slug> [--yes] [--app <label>] [--model <name>]";

pub(super) async fn flush_cmd<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let mut slug: Option<String> = None;
    let mut rest: Vec<String> = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--tenant" => slug = Some(next_value(&mut iter, "--tenant")?),
            "--help" | "-h" => {
                writeln!(w, "{USAGE}")?;
                writeln!(w)?;
                writeln!(
                    w,
                    "  Wipe one tenant's tables. The registry is never flushed."
                )?;
                return Ok(());
            }
            _ => rest.push(arg.clone()),
        }
    }
    // A plain flush ran on the registry pool and wiped orgs, operators and hosts.
    let slug = slug.ok_or_else(|| {
        TenancyError::Validation(format!(
            "in a tenancy project `flush` needs a tenant: {USAGE} \
             (the registry is never flushed)"
        ))
    })?;
    let org = super::api::find_org(pools, &slug)
        .await?
        .ok_or_else(|| TenancyError::Validation(format!("tenant `{slug}` not found")))?;
    let scoped = pools.scoped_pool_dyn(&org).await?;
    crate::migrate::manage::flush_cmd(&scoped, &rest, Some(ModelScope::Tenant), w)
        .await
        .map_err(TenancyError::Migrate)
}
