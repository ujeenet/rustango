//! The tenant registry handle: what resolvers look tenants up in, and
//! what the unresolved-tenant router gets in place of a tenant.

use crate::sql::{ExecError, ExistsPool as _, Pool};

use super::org::Org;

/// The tenant registry (orgs, operators). Passed to
/// [`OrgResolver::resolve_in`](super::OrgResolver::resolve_in), and to
/// the router set by [`crate::server::Builder::unresolved_tenant`] as an
/// `Extension`. It holds no tenant.
#[derive(Clone)]
#[non_exhaustive]
pub struct TenantRegistry {
    pool: Pool,
}

impl TenantRegistry {
    /// Wrap the registry pool, e.g. to call a resolver in a test.
    #[must_use]
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    /// The registry pool (orgs, operators), never a tenant's.
    #[must_use]
    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    /// Whether the registry holds any tenant, inactive ones included,
    /// so a suspended tenant does not reopen a first-run wizard.
    ///
    /// # Errors
    /// The registry query failing.
    pub async fn has_tenants(&self) -> Result<bool, ExecError> {
        Org::objects().exists(&self.pool).await
    }
}
