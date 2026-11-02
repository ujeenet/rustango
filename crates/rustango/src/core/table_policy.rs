//! Which generic surfaces may serve a framework table. The admin, the
//! ViewSet and every template view ask here, so they cannot disagree.

use super::ModelSchema;

/// Who may serve a table, for every operation: list, detail, create,
/// update and delete alike.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TableAccess {
    /// Any generic surface.
    Open,
    /// Only the admin: SSO configuration, and credentials minted once with their secret.
    AdminOnly,
    /// No generic surface, the admin included: only their own flows write them.
    Never,
}

/// Tables only the admin or their own flows write.
const ADMIN_ONLY_TABLES: &[&str] = &[
    "rustango_api_keys",
    "rustango_agents",
    "rustango_sso_providers",
    "rustango_shared_sso_providers",
    "rustango_sso_links",
    "rustango_sso_invites",
    "rustango_sso_shared_overrides",
    "rustango_sso_tenant_policy",
];

/// The policy for `table`.
pub(crate) fn table_access(table: &str) -> TableAccess {
    if is_never_served(table) {
        TableAccess::Never
    } else if ADMIN_ONLY_TABLES.contains(&table) {
        TableAccess::AdminOnly
    } else {
        TableAccess::Open
    }
}

/// Panic unless a non-admin generic surface (`who`) may serve `schema`.
pub(crate) fn assert_generic(schema: &ModelSchema, who: &str) {
    assert!(
        table_access(schema.table) == TableAccess::Open,
        "{who}: `{}` is admin only (or written by its own flow); a generic view cannot serve it",
        schema.table
    );
}

/// The TOTP stores hold raw secrets, a passkey row signs in as its
/// `user_id`, a database server row holds the sealed admin password, and a
/// purge drops what a tenant database row names. Enrollment has its own pages.
fn is_never_served(table: &str) -> bool {
    #[cfg(not(any(feature = "totp", feature = "passkey", feature = "tenancy")))]
    let _ = table;
    #[cfg(feature = "tenancy")]
    if table == <crate::tenancy::DbServer as super::Model>::SCHEMA.table
        || table == <crate::tenancy::TenantDatabase as super::Model>::SCHEMA.table
    {
        return true;
    }
    #[cfg(all(feature = "admin", feature = "totp"))]
    if table == <crate::admin::totp_store::AdminTotp as super::Model>::SCHEMA.table {
        return true;
    }
    #[cfg(all(feature = "tenancy", feature = "totp"))]
    if table == <crate::tenancy::OperatorTotp as super::Model>::SCHEMA.table {
        return true;
    }
    #[cfg(feature = "passkey")]
    if table == <crate::passkey::WebauthnCredential as super::Model>::SCHEMA.table {
        return true;
    }
    false
}
