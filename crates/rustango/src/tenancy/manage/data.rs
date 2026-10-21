//! `dumpdata` / `loaddata` in a tenancy project: registry models on the
//! registry, tenant models on the `--tenant` pool.

use std::io::Write;

use sqlx::Database;

use crate::migrate::manage::DataScope;
use crate::tenancy::error::TenancyError;
use crate::tenancy::pools::TenantPools;

use super::args::next_value;

/// Which of the two verbs to run.
#[derive(Clone, Copy)]
pub(super) enum Verb {
    Dump,
    Load,
}

pub(super) async fn data_cmd<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    verb: Verb,
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
            _ => rest.push(arg.clone()),
        }
    }
    let org = match slug {
        Some(slug) => Some(
            super::api::find_org(pools, &slug)
                .await?
                .ok_or_else(|| TenancyError::Validation(format!("tenant `{slug}` not found")))?,
        ),
        None => None,
    };
    let (pool, scope) = match &org {
        Some(org) => (
            pools.scoped_pool_dyn(org).await?,
            DataScope::Tenant {
                schema: org.own_schema(),
            },
        ),
        None => (pools.registry_pool(), DataScope::Registry),
    };
    let run = match verb {
        Verb::Dump => crate::migrate::manage::dumpdata_cmd(&pool, &rest, scope, w).await,
        Verb::Load => crate::migrate::manage::loaddata_cmd(&pool, &rest, scope, w).await,
    };
    if rest.iter().any(|a| a == "--help" || a == "-h") {
        writeln!(
            w,
            "  --tenant <slug>  That tenant's models; without it, the registry's."
        )?;
    }
    run.map_err(TenancyError::Migrate)
}
