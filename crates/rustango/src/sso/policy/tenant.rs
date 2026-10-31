//! A tenant's own SSO settings: its override of each shared provider and
//! the tenant-wide row. An override only tightens the shared provider.

use super::domain::PolicyError;
use super::models::{SsoSharedOverride, SsoTenantPolicy};
use super::{Lifecycle, PasswordLogin, RoleNames, SignInColumns, SignInPolicy, SignupMode};
use crate::core::Model as _;
use crate::query::QuerySet;
use crate::sql::{FetcherPool as _, Pool};
use crate::sso::link::ProviderKey;

/// Sign-ups per hour when the tenant sets no limit.
pub const DEFAULT_SIGNUPS_PER_HOUR: u32 = 100;

/// The one name the tenant settings row may have.
pub(crate) const SETTINGS_ROW: &str = "default";

/// Admin hook: the settings row is named exactly [`SETTINGS_ROW`]; any other
/// spelling would be ignored on some backends.
#[cfg(feature = "admin")]
pub(crate) fn admin_settings_name<'a>(
    values: &'a mut Vec<(&'static str, crate::core::SqlValue)>,
    _before: Option<&'a serde_json::Value>,
) -> crate::admin::derived_fields::DeriveFuture<'a> {
    let mut out = Ok(());
    for (col, v) in values.iter_mut() {
        if *col == "name" {
            match v {
                crate::core::SqlValue::String(s) if s.trim().eq_ignore_ascii_case(SETTINGS_ROW) => {
                    *s = SETTINGS_ROW.to_owned();
                }
                _ => out = Err(format!("name: must be `{SETTINGS_ROW}`")),
            }
        }
    }
    Box::pin(async move { out })
}

#[cfg(feature = "admin")]
inventory::submit! {
    crate::admin::derived_fields::AdminDerivedField {
        table: "rustango_sso_tenant_policy",
        derive: admin_settings_name,
    }
}

fn named(col: &'static str) -> impl Fn(PolicyError) -> PolicyError {
    move |e| PolicyError(format!("{col}: {e}"))
}

/// A tenant's override of one shared provider, parsed.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct SharedOverride {
    /// One more layer of sign-in rules.
    pub sign_in: SignInPolicy,
    pub signup_mode: Option<SignupMode>,
    pub lifecycle: Lifecycle,
    /// The roles the shared provider may grant here.
    pub allowed_roles: RoleNames,
    /// Email linking is off here, whatever the shared row says.
    pub email_link_off: bool,
    /// Email linking never links a role holder here.
    pub email_link_staff_off: bool,
}

impl SharedOverride {
    /// Parse a stored row.
    ///
    /// # Errors
    /// The first value that does not parse, naming its column.
    pub fn parse(row: &SsoSharedOverride) -> Result<Self, PolicyError> {
        let claims = match &row.required_claims {
            serde_json::Value::Null => serde_json::json!({}),
            v => v.clone(),
        };
        let sign_in = SignInPolicy::parse(&SignInColumns {
            allowed_email_domains: row.allowed_email_domains.as_deref(),
            blocked_email_domains: row.blocked_email_domains.as_deref(),
            allowed_idp_tenants: row.allowed_idp_tenants.as_deref(),
            allowed_hosted_domains: row.allowed_hosted_domains.as_deref(),
            required_claims: Some(&claims),
        })?;
        let signup_mode = match row
            .signup_mode
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            None => None,
            Some(m) => Some(m.parse().map_err(named("signup_mode"))?),
        };
        Ok(Self {
            sign_in,
            signup_mode,
            lifecycle: Lifecycle::parse(
                row.sso_only.unwrap_or(false),
                None,
                row.session_ttl_secs,
                row.superuser_session_ttl_secs,
            )?,
            allowed_roles: RoleNames::parse_opt(row.allowed_roles.as_deref())
                .map_err(named("allowed_roles"))?,
            // Only `false` counts: an override never turns linking on.
            email_link_off: row.allow_email_link == Some(false),
            email_link_staff_off: row.allow_email_link_staff == Some(false),
        })
    }

    /// This tenant's override of the shared provider `key` names, now
    /// called `slug`. An override is bound to the provider row on first use:
    /// a renamed row keeps it, a new row under the old slug does not, and a
    /// row repointed at another issuer refuses until the override is made
    /// again. A table not yet migrated holds none.
    ///
    /// # Errors
    /// The read failed, or the row does not parse or is ambiguous.
    pub async fn load(
        pool: &Pool,
        key: &ProviderKey,
        slug: &str,
    ) -> Result<Option<Self>, PolicyError> {
        let table = SsoSharedOverride::SCHEMA.table;
        let fail = |e: crate::sql::ExecError| PolicyError(format!("{table}: {e}"));
        let id = key.provider_id();
        let bound = match QuerySet::<SsoSharedOverride>::new()
            .filter("shared_provider_id", id)
            .fetch(pool)
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                return match crate::migrate::try_table_exists_here(pool, table).await {
                    Ok(false) => Ok(None),
                    _ => Err(fail(e)),
                }
            }
        };
        if let Some(row) = bound.first() {
            if row.shared_issuer.as_deref() != Some(key.issuer()) {
                return Err(PolicyError(format!(
                    "{table}: the override of `{slug}` was made for another issuer; make it again"
                )));
            }
            return Self::parse(row).map(Some);
        }
        // Not bound yet: the one unbound override named `slug`.
        let unbound: Vec<SsoSharedOverride> = QuerySet::<SsoSharedOverride>::new()
            .filter("shared_slug", slug.to_owned())
            .filter("shared_provider_id__isnull", true)
            .fetch(pool)
            .await
            .map_err(fail)?
            .into_iter()
            // `=` may compare case-blind (MySQL): match exactly.
            .filter(|r| r.shared_slug == slug)
            .collect();
        let row = match unbound.as_slice() {
            [] => return Ok(None),
            [row] => row,
            _ => {
                return Err(PolicyError(format!(
                    "{table}: more than one override of `{slug}`"
                )))
            }
        };
        let parsed = Self::parse(row)?;
        {
            use crate::sql::UpdaterPool as _;
            let row_id = row.id.get().copied().unwrap_or_default();
            // Only while still unbound: a concurrent first use binds it the same way.
            SsoSharedOverride::objects()
                .filter("id", row_id)
                .filter("shared_provider_id__isnull", true)
                .update()
                .set("shared_provider_id", id)
                .set("shared_issuer", key.issuer().to_owned())
                .execute_pool(pool)
                .await
                .map_err(fail)?;
        }
        Ok(Some(parsed))
    }
}

/// Admin hook: an override whose slug changes binds again on its next use.
#[cfg(feature = "admin")]
pub(crate) fn admin_rebind<'a>(
    values: &'a mut Vec<(&'static str, crate::core::SqlValue)>,
    before: Option<&'a serde_json::Value>,
) -> crate::admin::derived_fields::DeriveFuture<'a> {
    use crate::core::SqlValue;
    let was = before.and_then(|b| b.get("shared_slug")?.as_str().map(str::to_owned));
    let slug = values.iter_mut().find(|(c, _)| *c == "shared_slug");
    let changed = match slug {
        Some((_, SqlValue::String(s))) => {
            *s = s.trim().to_owned();
            was.as_deref() != Some(s.as_str())
        }
        _ => false,
    };
    values.retain(|(c, _)| !matches!(*c, "shared_provider_id" | "shared_issuer"));
    if changed {
        values.push(("shared_provider_id", SqlValue::Null));
        values.push(("shared_issuer", SqlValue::Null));
    }
    Box::pin(async move { Ok(()) })
}

#[cfg(feature = "admin")]
inventory::submit! {
    crate::admin::derived_fields::AdminDerivedField {
        table: "rustango_sso_shared_overrides",
        derive: admin_rebind,
    }
}

/// The tenant-wide SSO settings. The default is the behaviour without them.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TenantSettings {
    pub password_login: PasswordLogin,
    /// No user but a superuser signs in with a password.
    pub sso_only: bool,
    pub default_provider: Option<String>,
    pub access_denied_message: Option<String>,
    /// `None`: [`DEFAULT_SIGNUPS_PER_HOUR`]; `Some(0)`: no limit.
    pub signups_per_hour: Option<u32>,
}

impl Default for TenantSettings {
    fn default() -> Self {
        Self {
            password_login: PasswordLogin::On,
            sso_only: false,
            default_provider: None,
            access_denied_message: None,
            signups_per_hour: None,
        }
    }
}

impl TenantSettings {
    /// Parse the stored row.
    ///
    /// # Errors
    /// An unknown `password_login`, or a negative limit.
    pub fn parse(row: &SsoTenantPolicy) -> Result<Self, PolicyError> {
        let trimmed = |s: &Option<String>| {
            s.as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        Ok(Self {
            password_login: row
                .password_login
                .parse()
                .map_err(named("password_login"))?,
            sso_only: row.sso_only,
            default_provider: trimmed(&row.default_provider),
            access_denied_message: trimmed(&row.access_denied_message),
            signups_per_hour: row
                .signups_per_hour
                .map(u32::try_from)
                .transpose()
                .map_err(|_| PolicyError("signups_per_hour: must not be negative".into()))?,
        })
    }

    /// This tenant's settings: the defaults when the table or its row is
    /// missing. Any other read error is an error, and the caller refuses.
    ///
    /// # Errors
    /// The read failed, or the row does not parse.
    pub async fn load(pool: &Pool) -> Result<Self, PolicyError> {
        let table = SsoTenantPolicy::SCHEMA.table;
        match QuerySet::<SsoTenantPolicy>::new()
            .filter("name", SETTINGS_ROW)
            .fetch(pool)
            .await
        {
            // `=` may compare case-blind (MySQL): match exactly.
            Ok(rows) => rows
                .iter()
                .find(|r| r.name == SETTINGS_ROW)
                .map_or_else(|| Ok(Self::default()), Self::parse),
            Err(e) => match crate::migrate::try_table_exists_here(pool, table).await {
                Ok(false) => Ok(Self::default()),
                _ => Err(PolicyError(format!("{table}: {e}"))),
            },
        }
    }

    /// What an unreadable settings row means: only superusers use a
    /// password, through `?local=1`.
    #[must_use]
    pub fn break_glass() -> Self {
        Self {
            password_login: PasswordLogin::SuperusersOnly,
            ..Self::default()
        }
    }

    /// Sign-ups allowed per hour; `None`: no limit.
    #[must_use]
    pub fn signup_limit(&self) -> Option<u32> {
        match self.signups_per_hour {
            None => Some(DEFAULT_SIGNUPS_PER_HOUR),
            Some(0) => None,
            Some(n) => Some(n),
        }
    }

    /// Whether these settings alone let a (super)user sign in with a
    /// password. A superuser follows only `password_login`.
    #[must_use]
    pub fn password_allowed(&self, superuser: bool) -> bool {
        match self.password_login {
            PasswordLogin::Off => false,
            PasswordLogin::SuperusersOnly => superuser,
            _ => superuser || !self.sso_only,
        }
    }

    /// Whether the login page shows the password form; `local` is `?local=1`.
    #[must_use]
    pub fn shows_password_form(&self, local: bool) -> bool {
        match self.password_login {
            PasswordLogin::Off => false,
            PasswordLogin::SuperusersOnly => local,
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(password_login: PasswordLogin, sso_only: bool) -> TenantSettings {
        TenantSettings {
            password_login,
            sso_only,
            ..TenantSettings::default()
        }
    }

    /// Superusers follow only `password_login`; `sso_only` never locks them out.
    #[test]
    fn password_rules_by_role() {
        use PasswordLogin as P;
        for (login, sso_only, user, superuser) in [
            (P::On, false, true, true),
            (P::On, true, false, true),
            (P::SuperusersOnly, false, false, true),
            (P::SuperusersOnly, true, false, true),
            (P::Off, false, false, false),
            (P::Off, true, false, false),
        ] {
            let s = settings(login, sso_only);
            assert_eq!(s.password_allowed(false), user, "{login:?} {sso_only}");
            assert_eq!(s.password_allowed(true), superuser, "{login:?} {sso_only}");
        }
        assert!(!settings(P::SuperusersOnly, false).shows_password_form(false));
        assert!(settings(P::SuperusersOnly, false).shows_password_form(true));
        assert!(!settings(P::Off, false).shows_password_form(true));
        assert!(settings(P::On, true).shows_password_form(false));
    }

    #[test]
    fn signup_limit_defaults_and_zero_is_none() {
        let mut s = TenantSettings::default();
        assert_eq!(s.signup_limit(), Some(DEFAULT_SIGNUPS_PER_HOUR));
        s.signups_per_hour = Some(0);
        assert_eq!(s.signup_limit(), None);
        s.signups_per_hour = Some(3);
        assert_eq!(s.signup_limit(), Some(3));
    }
}
