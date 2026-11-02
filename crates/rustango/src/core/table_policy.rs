//! Which generic surfaces may serve a framework table. The admin, the
//! ViewSet and every template view ask here, so they cannot disagree.

use super::ModelSchema;

/// Who may serve a table, for every operation: list, detail, create,
/// update and delete alike.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TableAccess {
    /// Any generic surface.
    Open,
    /// Only the admin: SSO configuration, credentials, agent grants, operator and admin users.
    AdminOnly,
    /// No generic surface, the admin included: only their own flows write them.
    Never,
}

/// Tables only the admin (or the operator console's) or their own flows write:
/// credentials, SSO configuration, agent grants, operator and admin users.
const ADMIN_ONLY_TABLES: &[&str] = &[
    #[cfg(feature = "admin")]
    <crate::admin::user::AdminUser as super::Model>::SCHEMA.table,
    #[cfg(feature = "tenancy")]
    <crate::tenancy::Operator as super::Model>::SCHEMA.table,
    #[cfg(feature = "tenancy")]
    <crate::tenancy::auth_backends::ApiKey as super::Model>::SCHEMA.table,
    #[cfg(all(feature = "tenancy", feature = "mcp"))]
    <crate::tenancy::Agent as super::Model>::SCHEMA.table,
    #[cfg(all(feature = "tenancy", feature = "mcp"))]
    <crate::tenancy::AgentGrant as super::Model>::SCHEMA.table,
    #[cfg(feature = "sso")]
    <crate::sso::SsoProvider as super::Model>::SCHEMA.table,
    #[cfg(feature = "sso")]
    <crate::sso::SsoLink as super::Model>::SCHEMA.table,
    #[cfg(feature = "sso")]
    <crate::sso::policy::SsoInvite as super::Model>::SCHEMA.table,
    #[cfg(feature = "sso")]
    <crate::sso::policy::SsoSharedOverride as super::Model>::SCHEMA.table,
    #[cfg(feature = "sso")]
    <crate::sso::policy::SsoTenantPolicy as super::Model>::SCHEMA.table,
    #[cfg(all(feature = "tenancy", feature = "admin-sso"))]
    <crate::tenancy::sso::SharedSsoProvider as super::Model>::SCHEMA.table,
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

#[cfg(all(test, feature = "tenancy"))]
mod tests {
    use super::{table_access, TableAccess};

    /// A generic view over these would mint a password hash or raise an agent's rights.
    #[test]
    fn identity_and_grant_tables_are_admin_only() {
        for t in [
            "rustango_operators",
            "rustango_admin_users",
            "rustango_api_keys",
            #[cfg(feature = "mcp")]
            "rustango_agent_grants",
            #[cfg(feature = "mcp")]
            "rustango_agents",
        ] {
            assert_eq!(table_access(t), TableAccess::AdminOnly, "{t}");
        }
    }
}
