//! A tenant's own SSO settings: its override of each shared provider and
//! the tenant-wide row. An override only tightens the shared provider.

use super::domain::PolicyError;
use super::models::{SsoSharedOverride, SsoTenantPolicy};
use super::{Lifecycle, PasswordLogin, RoleNames, SignInColumns, SignInPolicy, SignupMode};
use crate::core::Model as _;
use crate::query::QuerySet;
use crate::sql::{FetcherPool as _, Pool};

/// Sign-ups per hour when the tenant sets no limit.
pub const DEFAULT_SIGNUPS_PER_HOUR: u32 = 100;

fn named(col: &'static str) -> impl Fn(PolicyError) -> PolicyError {
    move |e| PolicyError(format!("{col}: {e}"))
}

/// A tenant's override of one shared provider, parsed.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct SharedOverride {
    /// One more layer of sign-in rules.
    pub sign_in: SignInPolicy,
    /// `Some(true)`: every layer accepts only a verified email.
    pub require_verified_email: Option<bool>,
    pub signup_mode: Option<SignupMode>,
    pub lifecycle: Lifecycle,
    /// The roles the shared provider may grant here.
    pub allowed_roles: RoleNames,
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
            require_verified_email: row.require_verified_email.unwrap_or(true),
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
            require_verified_email: row.require_verified_email,
            signup_mode,
            lifecycle: Lifecycle::parse(
                row.sso_only.unwrap_or(false),
                None,
                row.session_ttl_secs,
                row.superuser_session_ttl_secs,
            )?,
            allowed_roles: RoleNames::parse_opt(row.allowed_roles.as_deref())
                .map_err(named("allowed_roles"))?,
        })
    }

    /// This tenant's override of shared provider `shared_slug`. A table not
    /// yet migrated holds none; any other read error is an error.
    ///
    /// # Errors
    /// The read failed, or the row does not parse.
    pub async fn load(pool: &Pool, shared_slug: &str) -> Result<Option<Self>, PolicyError> {
        let table = SsoSharedOverride::SCHEMA.table;
        match QuerySet::<SsoSharedOverride>::new()
            .filter("shared_slug", shared_slug.to_owned())
            .fetch(pool)
            .await
        {
            // `=` may compare case-blind (MySQL): match exactly.
            Ok(rows) => rows
                .iter()
                .find(|r| r.shared_slug == shared_slug)
                .map(Self::parse)
                .transpose(),
            Err(e) => match crate::migrate::try_table_exists_here(pool, table).await {
                Ok(false) => Ok(None),
                _ => Err(PolicyError(format!("{table}: {e}"))),
            },
        }
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
            .filter("name", "default")
            .fetch(pool)
            .await
        {
            Ok(rows) => rows
                .first()
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
