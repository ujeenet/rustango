//! `sso-deactivate-idle`: switch off idle SSO-made users in every active tenant.

use std::io::Write;

use sqlx::Database;

use crate::tenancy::error::TenancyError;
use crate::tenancy::pools::TenantPools;

pub(super) async fn sso_deactivate_idle_cmd<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<crate::sql::sqlx::Pool<DB>>,
{
    let registry = pools.registry_pool();
    let sweep = crate::tenancy::sweep::for_each_tenant(pools, |_org, pool| {
        let registry = registry.clone();
        async move { crate::sso::lifecycle::deactivate_idle(&pool, Some(&registry)).await }
    })
    .await?;
    let mut failed = 0;
    for outcome in &sweep.outcomes {
        match &outcome.result {
            Ok(ids) => writeln!(w, "{}: {} deactivated", outcome.slug, ids.len())?,
            Err(e) => {
                failed += 1;
                writeln!(w, "{}: failed: {e}", outcome.slug)?;
            }
        }
    }
    if failed > 0 {
        return Err(TenancyError::Validation(format!(
            "sso-deactivate-idle failed for {failed} tenant(s)"
        )));
    }
    Ok(())
}
