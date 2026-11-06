//! Which generic surfaces may serve a table. The admin, the ViewSet and every
//! template view ask here, so they cannot disagree. Each model declares its
//! level with `#[rustango(access = "...")]`; nothing here names a model.

use std::collections::HashMap;
use std::sync::OnceLock;

use super::{ModelEntry, ModelSchema, TableAccess};

/// The strictest level the models registered on `table` declare, so a proxy
/// cannot open what its base closes. `None` when no model registers it:
/// there are no rows to serve, only the admin's custom views use such names.
pub(crate) fn table_access(table: &str) -> Option<TableAccess> {
    static BY_TABLE: OnceLock<HashMap<&'static str, TableAccess>> = OnceLock::new();
    BY_TABLE
        .get_or_init(|| {
            let mut by_table: HashMap<&'static str, TableAccess> = HashMap::new();
            for e in inventory::iter::<ModelEntry> {
                let level = by_table.entry(e.schema.table).or_insert(TableAccess::Open);
                *level = (*level).max(e.schema.table_access());
            }
            by_table
        })
        .get(table)
        .copied()
}

/// Panic unless a non-admin generic surface (`who`) may serve `schema`.
pub(crate) fn assert_generic(schema: &ModelSchema, who: &str) {
    let access =
        table_access(schema.table).map_or(schema.table_access(), |r| r.max(schema.table_access()));
    assert!(
        access == TableAccess::Open,
        "{who}: `{}` is admin only (or written by its own flow); a generic view cannot serve it",
        schema.table
    );
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
            assert_eq!(table_access(t), Some(TableAccess::AdminOnly), "{t}");
        }
    }

    /// The tables the old hand-kept lists named keep their level.
    #[test]
    fn listed_tables_keep_their_level() {
        use crate::core::Model;
        let mut expect = vec![
            (
                <crate::tenancy::DbServer as Model>::SCHEMA,
                TableAccess::Never,
            ),
            (
                <crate::tenancy::TenantDatabase as Model>::SCHEMA,
                TableAccess::Never,
            ),
            (
                <crate::tenancy::Operator as Model>::SCHEMA,
                TableAccess::AdminOnly,
            ),
            (
                <crate::tenancy::auth_backends::ApiKey as Model>::SCHEMA,
                TableAccess::AdminOnly,
            ),
            (
                <crate::tenancy::UsedJti as Model>::SCHEMA,
                TableAccess::Never,
            ),
        ];
        #[cfg(feature = "admin")]
        expect.push((
            <crate::admin::user::AdminUser as Model>::SCHEMA,
            TableAccess::AdminOnly,
        ));
        #[cfg(all(feature = "admin", feature = "totp"))]
        expect.push((
            <crate::admin::totp_store::AdminTotp as Model>::SCHEMA,
            TableAccess::Never,
        ));
        #[cfg(feature = "totp")]
        expect.push((
            <crate::tenancy::OperatorTotp as Model>::SCHEMA,
            TableAccess::Never,
        ));
        #[cfg(feature = "passkey")]
        expect.push((
            <crate::passkey::WebauthnCredential as Model>::SCHEMA,
            TableAccess::Never,
        ));
        #[cfg(feature = "mcp")]
        expect.extend([
            (
                <crate::tenancy::Agent as Model>::SCHEMA,
                TableAccess::AdminOnly,
            ),
            (
                <crate::tenancy::AgentGrant as Model>::SCHEMA,
                TableAccess::AdminOnly,
            ),
        ]);
        #[cfg(feature = "sso")]
        expect.extend([
            (
                <crate::sso::SsoProvider as Model>::SCHEMA,
                TableAccess::AdminOnly,
            ),
            (
                <crate::sso::SsoLink as Model>::SCHEMA,
                TableAccess::AdminOnly,
            ),
            (
                <crate::sso::policy::SsoInvite as Model>::SCHEMA,
                TableAccess::AdminOnly,
            ),
            (
                <crate::sso::policy::SsoSharedOverride as Model>::SCHEMA,
                TableAccess::AdminOnly,
            ),
            (
                <crate::sso::policy::SsoTenantPolicy as Model>::SCHEMA,
                TableAccess::AdminOnly,
            ),
            (
                <crate::sso::policy::SsoLandingRule as Model>::SCHEMA,
                TableAccess::AdminOnly,
            ),
        ]);
        #[cfg(feature = "admin-sso")]
        expect.push((
            <crate::tenancy::sso::SharedSsoProvider as Model>::SCHEMA,
            TableAccess::AdminOnly,
        ));
        for (schema, level) in expect {
            assert_eq!(table_access(schema.table), Some(level), "{}", schema.table);
        }
    }

    /// A table no model registers has no declared level.
    #[test]
    fn an_unknown_table_has_no_level() {
        assert_eq!(table_access("no_such_table_anywhere"), None);
    }
}
