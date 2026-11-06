//! Which generic surfaces may serve a table. The admin, the ViewSet and every
//! template view ask here, so they cannot disagree. Each model declares its
//! level with `#[rustango(access = "...")]`; nothing here names a model.

use std::collections::HashMap;
use std::sync::OnceLock;

use super::{ModelEntry, ModelSchema, TableAccess};

/// The strictest level the models registered on `table` declare, so a proxy
/// cannot open what its base closes. `None` when no model registers it:
/// there are no rows to serve, only the admin's custom views use such names.
///
/// Keyed ASCII-lowercase: SQLite and MySQL (`lower_case_table_names`) read
/// `Rustango_Admin_Totp` as the same table.
pub(crate) fn table_access(table: &str) -> Option<TableAccess> {
    static BY_TABLE: OnceLock<HashMap<String, TableAccess>> = OnceLock::new();
    BY_TABLE
        .get_or_init(|| {
            let mut by_table: HashMap<String, TableAccess> = HashMap::new();
            for e in inventory::iter::<ModelEntry> {
                let level = by_table
                    .entry(e.schema.table.to_ascii_lowercase())
                    .or_insert(TableAccess::Open);
                *level = (*level).max(e.schema.table_access());
            }
            by_table
        })
        .get(&table.to_ascii_lowercase())
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

/// Panic when `names` (the fields a generic surface `who` renders, accepts,
/// filters, searches or sorts on) holds a credential of `schema`.
pub(crate) fn assert_no_credentials<'a>(
    schema: &ModelSchema,
    who: &str,
    names: impl IntoIterator<Item = &'a str>,
) {
    let names: Vec<&str> = names.into_iter().collect();
    let found: Vec<&str> = schema
        .scalar_fields()
        .filter(|f| f.is_credential() && names.iter().any(|n| *n == f.name || *n == f.column))
        .map(|f| f.name)
        .collect();
    assert!(
        found.is_empty(),
        "{who} `{}`: {found:?} hold credentials, which no generic view may render or accept; \
         drop them from the view's field lists, or serve them from a handler of your own",
        schema.table
    );
}

#[cfg(all(test, feature = "tenancy"))]
mod credential_tests {
    use super::assert_no_credentials;

    fn refusal(f: impl FnOnce()) -> String {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
            .err()
            .and_then(|e| e.downcast_ref::<String>().cloned())
            .unwrap_or_default()
    }

    /// A tenant user's hash and an org's database URL are credentials.
    #[test]
    fn user_hash_and_org_url_are_credentials_and_refused() {
        use crate::core::Model as _;
        let user = crate::tenancy::User::SCHEMA;
        let org = crate::tenancy::Org::SCHEMA;
        assert!(user.field("password_hash").unwrap().is_credential());
        assert!(org.field("database_url").unwrap().is_credential());
        assert!(
            refusal(|| assert_no_credentials(user, "t", ["id", "password_hash"]))
                .contains("password_hash")
        );
        assert!(
            refusal(|| assert_no_credentials(org, "t", ["database_url"])).contains("database_url")
        );
        assert_no_credentials(user, "t", ["id", "username", "email"]);
    }
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

    /// SQLite and MySQL fold table-name case, so the lookup does too.
    #[test]
    fn table_lookup_ignores_case() {
        assert_eq!(
            table_access("Rustango_Operators"),
            Some(TableAccess::AdminOnly)
        );
    }

    /// A table no model registers has no declared level.
    #[test]
    fn an_unknown_table_has_no_level() {
        assert_eq!(table_access("no_such_table_anywhere"), None);
    }
}
