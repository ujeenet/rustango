//! Tenant sign-in by the `[auth] login_field` column: one lookup and one
//! lock key for the sign-in form, HTTP Basic and `/auth/login`.

use super::auth::User;
use crate::login_field::LoginField;

static FIELD: crate::boot_slot::BootSlot<LoginField> = crate::boot_slot::BootSlot::new();

/// The field tenant users sign in with: [`LoginField::Username`] unless
/// [`configure_login_field`] or `[auth] login_field` set it.
#[must_use]
pub fn login_field() -> LoginField {
    *FIELD.get(LoginField::default)
}

/// Set the sign-in field at boot. It beats `[auth] login_field`; `false`
/// if an earlier call already set it.
pub fn configure_login_field(field: LoginField) -> bool {
    FIELD.set_explicit(field)
}

/// The `[auth] login_field` value; `false` if app code already set one.
#[cfg(all(feature = "config", feature = "manage"))]
pub(crate) fn configure_login_field_from_settings(field: LoginField) -> bool {
    FIELD.set_from_settings(field)
}

/// The user `identifier` signs in as under `field`. An email matches
/// ignoring case; one that matches more than one user signs nobody in.
///
/// # Errors
/// A driver failure.
pub async fn find_login_user(
    pool: &crate::sql::Pool,
    field: LoginField,
    identifier: &str,
) -> Result<Option<User>, crate::sql::ExecError> {
    use crate::core::Column as _;
    use crate::sql::FetcherPool as _;
    match field {
        LoginField::Username => Ok(User::objects()
            .where_(User::username.eq(identifier.to_owned()))
            .fetch(pool)
            .await?
            .into_iter()
            .next()),
        #[cfg(feature = "sso")]
        LoginField::Email => {
            let wanted = identifier.trim();
            if wanted.is_empty() {
                return Ok(None);
            }
            let rows = User::objects()
                .where_(User::email.iexact(wanted))
                .fetch(pool)
                .await?;
            match crate::sso::link::EmailLookup::pick(&rows, wanted, |u| u.email.as_deref()) {
                Ok(user) => Ok(user.cloned()),
                Err(()) => {
                    tracing::warn!(
                        target: "rustango::tenancy::login",
                        "sign-in refused: the email matches more than one user"
                    );
                    Ok(None)
                }
            }
        }
    }
}

/// The stored identifier the login lock follows under `field`, so every
/// spelling that finds `user` shares one lock.
#[must_use]
pub fn login_key(field: LoginField, user: &User) -> &str {
    match field {
        LoginField::Username => &user.username,
        #[cfg(feature = "sso")]
        LoginField::Email => user.email.as_deref().unwrap_or(&user.username),
    }
}

/// Users of one tenant who cannot sign in by email.
#[cfg(feature = "sso")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct EmailGaps {
    /// Active users with no email.
    pub without_email: usize,
    /// Users whose email another user also has, ignoring case.
    pub shared_email: usize,
}

#[cfg(feature = "sso")]
impl EmailGaps {
    /// Nobody is locked out.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.without_email == 0 && self.shared_email == 0
    }
}

/// Count the tenant users email sign-in leaves out.
///
/// # Errors
/// A driver failure.
#[cfg(feature = "sso")]
pub async fn email_gaps(pool: &crate::sql::Pool) -> Result<EmailGaps, crate::sql::ExecError> {
    use crate::core::Column as _;
    let active: Vec<Option<String>> = User::objects()
        .where_(User::active.eq(true))
        .values_list_flat("email")
        .fetch::<Option<String>>(pool)
        .await?;
    let all: Vec<Option<String>> = User::objects()
        .values_list_flat("email")
        .fetch::<Option<String>>(pool)
        .await?;
    let blank = |e: &Option<String>| e.as_deref().map_or(true, |e| e.trim().is_empty());
    let mut seen = std::collections::HashMap::<String, usize>::new();
    for e in all.iter().filter(|e| !blank(e)).flatten() {
        *seen.entry(e.trim().to_lowercase()).or_default() += 1;
    }
    Ok(EmailGaps {
        without_email: active.iter().filter(|e| blank(e)).count(),
        shared_email: seen.values().filter(|n| **n > 1).sum(),
    })
}

/// Warn, once per tenant, about users email sign-in leaves out. A warning
/// and not a refusal: one tenant's data must not stop every tenant.
#[cfg(feature = "sso")]
pub(crate) async fn warn_email_gaps<DB>(pools: &super::TenantPools<DB>)
where
    DB: sqlx::Database,
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let sweep =
        super::for_each_tenant(pools, |_, pool| async move { email_gaps(&pool).await }).await;
    let sweep = match sweep {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                target: "rustango::tenancy::login",
                error = %e,
                "login_field = \"email\": could not list tenants to check user emails"
            );
            return;
        }
    };
    for (slug, gaps) in sweep.values().filter(|(_, g)| !g.is_empty()) {
        tracing::warn!(
            target: "rustango::tenancy::login",
            slug,
            without_email = gaps.without_email,
            shared_email = gaps.shared_email,
            "login_field = \"email\": these users cannot sign in until each has its own email"
        );
    }
    for (slug, e) in sweep.errors() {
        tracing::warn!(
            target: "rustango::tenancy::login",
            slug,
            error = %e,
            "login_field = \"email\": could not check user emails"
        );
    }
}
