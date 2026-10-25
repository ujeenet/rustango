//! A new tenant's first admin.
//!
//! Provisioning creates the user as a superuser with a password nobody
//! holds, then hands over a set-password link: mailed when a mailer is
//! set, else shown once to the operator. The link works once (setting a
//! password stamps `password_changed_at`, which ends it), expires, and is
//! bound to this tenant's id. Neither the link nor a password is ever
//! stored, audited or logged.

use std::time::Duration;

use crate::core::Column as _;
use crate::sql::FetcherPool as _;

use super::auth::{NewUser, User};
use super::error::TenancyError;
use super::org::Org;
use super::pools::TenantPools;
use super::session::SessionSecret;

/// `rustango_users.username` is at most this long.
const USERNAME_MAX: usize = 64;

/// The signed part of a link is over this base, not a URL, so a link
/// does not depend on the host, scheme or mount it is served under. The
/// tenant binding comes from the link's scope.
const LINK_BASE: &str = "rustango:set-password";

/// Who the first admin is, checked on construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirstAdmin {
    email: String,
    username: String,
}

impl FirstAdmin {
    /// `username` defaults to the email.
    ///
    /// # Errors
    /// An operator-readable reason when the email or username is unusable.
    pub fn new(email: &str, username: Option<&str>) -> Result<Self, String> {
        let email = email.trim();
        if !crate::validators::is_email(email) {
            return Err(format!("`{email}` is not a valid email address."));
        }
        let username = username.map(str::trim).filter(|u| !u.is_empty()).unwrap_or(email);
        if username.chars().count() > USERNAME_MAX {
            return Err(format!(
                "The admin username is longer than {USERNAME_MAX} characters; give a shorter one."
            ));
        }
        if username.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err("The admin username may not contain spaces.".to_owned());
        }
        Ok(Self {
            email: email.to_owned(),
            username: username.to_owned(),
        })
    }

    #[must_use]
    pub fn email(&self) -> &str {
        &self.email
    }

    #[must_use]
    pub fn username(&self) -> &str {
        &self.username
    }
}

/// The first admin a run created.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CreatedAdmin {
    /// The tenant's `rustango_users.id`.
    pub user_id: i64,
    pub username: String,
    pub email: String,
    /// How the set-password link went out; `None` until handed over.
    pub handover: Option<Handover>,
}

/// A set-password link. No `Display` and a redacted `Debug`, so it
/// reaches only the place that calls [`Self::expose`].
#[derive(Clone)]
pub struct SetPasswordLink(String);

impl SetPasswordLink {
    /// The link itself. Show it once; never log or store it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SetPasswordLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SetPasswordLink([redacted])")
    }
}

/// How a first admin got their set-password link.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Handover {
    /// Mailed to this address.
    Mailed { to: String },
    /// No mailer: give this to the admin.
    Link(SetPasswordLink),
    /// The mail failed; give this to the admin instead.
    MailFailed(SetPasswordLink),
}

impl Handover {
    /// The link the operator must pass on, if it was not mailed.
    #[must_use]
    pub fn link(&self) -> Option<&SetPasswordLink> {
        match self {
            Self::Mailed { .. } => None,
            Self::Link(l) | Self::MailFailed(l) => Some(l),
        }
    }
}

/// How a deployment hands a first admin their set-password link.
///
/// The key is derived from the tenant session secret, so the tenant admin
/// that redeems the link must run with the same one.
#[derive(Clone)]
pub struct FirstAdminHandover {
    key: [u8; 32],
    set_password_path: String,
    ttl: Duration,
    #[cfg(feature = "email")]
    mail: Option<Mail>,
}

#[cfg(feature = "email")]
#[derive(Clone)]
struct Mail {
    mailer: crate::email::BoxedMailer,
    from: String,
}

impl std::fmt::Debug for FirstAdminHandover {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FirstAdminHandover")
            .field("set_password_path", &self.set_password_path)
            .field("ttl", &self.ttl)
            .field("mails", &self.mails())
            .finish_non_exhaustive()
    }
}

impl FirstAdminHandover {
    /// Links last three days by default.
    pub const DEFAULT_TTL: Duration = Duration::from_secs(72 * 60 * 60);

    /// Sign links with `tenant_secret`, the tenant admin's session secret.
    #[must_use]
    pub fn new(tenant_secret: &SessionSecret) -> Self {
        Self {
            key: link_key(tenant_secret),
            set_password_path: super::routes::RouteConfig::default().change_password_url,
            ttl: Self::DEFAULT_TTL,
            #[cfg(feature = "email")]
            mail: None,
        }
    }

    /// The tenant admin's change-password path, if not the default.
    #[must_use]
    pub fn set_password_path(mut self, path: impl Into<String>) -> Self {
        self.set_password_path = path.into();
        self
    }

    #[must_use]
    pub fn ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// Mail links from `from` instead of returning them.
    #[cfg(feature = "email")]
    #[must_use]
    pub fn mailer(mut self, mailer: crate::email::BoxedMailer, from: impl Into<String>) -> Self {
        self.mail = Some(Mail {
            mailer,
            from: from.into(),
        });
        self
    }

    /// Whether links are mailed rather than handed back.
    #[must_use]
    pub fn mails(&self) -> bool {
        #[cfg(feature = "email")]
        {
            self.mail.is_some()
        }
        #[cfg(not(feature = "email"))]
        {
            false
        }
    }

    /// Hours a link stays valid, for the text around it.
    #[must_use]
    pub fn ttl_hours(&self) -> u64 {
        self.ttl.as_secs() / 3600
    }

    /// Issue `admin`'s link and mail it, or hand it back. `request_host`
    /// is the caller's `Host`, the last fallback for the tenant's port.
    pub async fn hand_over(
        &self,
        org: &Org,
        admin: &CreatedAdmin,
        request_host: Option<&str>,
    ) -> Handover {
        let link = self.link(org, admin.user_id, request_host);
        #[cfg(feature = "email")]
        if let Some(mail) = &self.mail {
            return match mail.mailer.send(&self.email(org, admin, &mail.from, &link)).await {
                Ok(()) => Handover::Mailed {
                    to: admin.email.clone(),
                },
                Err(e) => {
                    tracing::warn!(
                        target: "rustango::tenancy::first_admin",
                        slug = %org.slug,
                        error = %e,
                        "could not mail the first admin's set-password link"
                    );
                    Handover::MailFailed(link)
                }
            };
        }
        Handover::Link(link)
    }

    /// Fill in `outcome`'s first admin handover, if it has one.
    pub async fn hand_over_outcome(
        &self,
        registry: &crate::sql::Pool,
        outcome: &mut super::provision::ProvisionOutcome,
        request_host: Option<&str>,
    ) -> Result<(), TenancyError> {
        let Some(admin) = outcome.first_admin.as_mut() else {
            return Ok(());
        };
        let org = Org::objects()
            .where_(Org::id.eq(outcome.org_id))
            .fetch(registry)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| TenancyError::Validation(format!("no tenant `{}`", outcome.slug)))?;
        admin.handover = Some(self.hand_over(&org, admin, request_host).await);
        Ok(())
    }

    fn link(&self, org: &Org, user_id: i64, request_host: Option<&str>) -> SetPasswordLink {
        let signed = crate::auth_flows::PasswordReset::issue(
            &crate::auth_flows::LinkScope::from(org),
            LINK_BASE,
            user_id,
            &self.key,
            self.ttl,
        );
        let query = signed.split_once('?').map_or("", |(_, q)| q);
        let page = super::server::tenant_link(org, &self.set_password_path, request_host);
        SetPasswordLink(format!("{page}?{query}"))
    }

    #[cfg(feature = "email")]
    fn email(
        &self,
        org: &Org,
        admin: &CreatedAdmin,
        from: &str,
        link: &SetPasswordLink,
    ) -> crate::email::Email {
        let name = org
            .brand_name
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&org.display_name);
        crate::email::Email::new()
            .to(admin.email.clone())
            .from(from)
            .subject(format!("Set your password for {name}"))
            .body(format!(
                "An administrator account was created for you on {name}.\n\n\
                 Username: {}\n\n\
                 Set your password here. The link works once and expires in {} hours:\n\
                 {}\n",
                admin.username,
                self.ttl_hours(),
                link.expose(),
            ))
    }
}

/// The key links are signed with: derived, so a link is never a valid MAC
/// of a session cookie or anything else signed with the same secret.
pub(crate) fn link_key(secret: &SessionSecret) -> [u8; 32] {
    crate::session::sign(secret, b"rustango-set-password-link-v1")
}

/// Whether a request query carries a set-password link.
pub(crate) fn is_link_query(query: &str) -> bool {
    query.split('&').any(|p| p == "purpose=pwreset")
        && query.split('&').any(|p| p.starts_with("signature="))
}

/// The signed form of a link whose query is `query`.
pub(crate) fn signed_link(query: &str) -> String {
    format!("{LINK_BASE}?{query}")
}

/// Create `admin` as a superuser of `org` with a password nobody holds.
pub(crate) async fn create<DB: sqlx::Database>(
    pools: &TenantPools<DB>,
    org: &Org,
    admin: &FirstAdmin,
) -> Result<CreatedAdmin, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let pool = pools.scoped_pool_dyn(org).await?;
    let taken: Vec<User> = User::objects()
        .where_(User::username.eq(admin.username.clone()))
        .limit(1)
        .fetch(&pool)
        .await?;
    if !taken.is_empty() {
        return Err(TenancyError::Validation(format!(
            "the tenant already has a user `{}`",
            admin.username
        )));
    }
    // Generated and dropped here: the admin sets their own through the link.
    let unusable = super::password::hash_async(&super::password::generate(32)).await?;
    let user = NewUser {
        username: &admin.username,
        password_hash: unusable,
        is_superuser: true,
        email: Some(&admin.email),
    }
    .insert(&pool)
    .await?;
    let created = CreatedAdmin {
        user_id: user.id.get().copied().unwrap_or_default(),
        username: admin.username.clone(),
        email: admin.email.clone(),
        handover: None,
    };
    audit(&pools.registry_pool(), org, &created).await;
    Ok(created)
}

/// Record the creation in the registry's audit log. Never the link.
async fn audit(registry: &crate::sql::Pool, org: &Org, admin: &CreatedAdmin) {
    let entry = crate::audit::PendingEntry {
        entity_table: "rustango_users",
        entity_pk: format!("{}:{}", org.slug, admin.user_id),
        operation: crate::audit::AuditOp::Create,
        source: crate::audit::AuditSource::Custom("provision:first_admin".to_owned()),
        changes: serde_json::json!({
            "tenant_slug": org.slug,
            "username": admin.username,
            "is_superuser": true,
        }),
    };
    if let Err(e) = crate::audit::emit_one_pool(registry, &entry).await {
        tracing::error!(
            target: "rustango::tenancy::first_admin",
            slug = %org.slug,
            error = %e,
            "the first admin's creation was NOT written to the audit log"
        );
    }
}

/// Issue a fresh set-password link for an existing tenant user.
///
/// # Errors
/// No such user, or a driver failure.
pub async fn hand_over_user<DB: sqlx::Database>(
    pools: &TenantPools<DB>,
    org: &Org,
    username: &str,
    handover: &FirstAdminHandover,
) -> Result<Handover, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let pool = pools.scoped_pool_dyn(org).await?;
    let user = User::objects()
        .where_(User::username.eq(username.to_owned()))
        .fetch(&pool)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| {
            TenancyError::Validation(format!("no user `{username}` in tenant `{}`", org.slug))
        })?;
    #[cfg(feature = "sso")]
    let email = user.email.clone().unwrap_or_default();
    #[cfg(not(feature = "sso"))]
    let email = if crate::validators::is_email(&user.username) {
        user.username.clone()
    } else {
        String::new()
    };
    let admin = CreatedAdmin {
        user_id: user.id.get().copied().unwrap_or_default(),
        username: user.username,
        email,
        handover: None,
    };
    if admin.email.is_empty() && handover.mails() {
        // Nowhere to mail it: hand it back.
        return Ok(Handover::Link(handover.link(org, admin.user_id, None)));
    }
    Ok(handover.hand_over(org, &admin, None).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret() -> SessionSecret {
        SessionSecret::from_bytes(vec![9; 32])
    }

    fn org(id: i64) -> Org {
        Org {
            id: crate::sql::Auto::from(id),
            slug: "acme".into(),
            display_name: "Acme".into(),
            storage_mode: "database".into(),
            backend_kind: "sqlite".into(),
            database_url: None,
            schema_name: None,
            host_pattern: None,
            port: None,
            path_prefix: None,
            active: true,
            created_at: chrono::Utc::now(),
            brand_name: None,
            brand_tagline: None,
            logo_path: None,
            favicon_path: None,
            primary_color: None,
            theme_mode: None,
        }
    }

    #[test]
    fn the_username_defaults_to_the_email_and_bad_input_is_refused() {
        let a = FirstAdmin::new(" ann@acme.test ", None).unwrap();
        assert_eq!((a.email(), a.username()), ("ann@acme.test", "ann@acme.test"));
        let b = FirstAdmin::new("ann@acme.test", Some("ann")).unwrap();
        assert_eq!(b.username(), "ann");
        assert!(FirstAdmin::new("not-an-email", None).is_err());
        assert!(FirstAdmin::new("ann@acme.test", Some("a b")).is_err());
        assert!(FirstAdmin::new("ann@acme.test", Some(&"x".repeat(65))).is_err());
    }

    #[test]
    fn a_link_never_prints() {
        let link = SetPasswordLink("https://acme.test/change-password?signature=s".into());
        let shown = format!("{:?} {:?}", link, Handover::Link(link.clone()));
        assert!(!shown.contains("signature"), "{shown}");
        let h = FirstAdminHandover::new(&secret());
        assert!(!format!("{h:?}").contains("key"));
    }

    /// The link's query, re-based, verifies for its own tenant only.
    #[test]
    fn a_link_verifies_for_its_tenant_and_no_other() {
        let tenant = org(7);
        let h = FirstAdminHandover::new(&secret());
        let link = h.link(&tenant, 42, None);
        let query = link.expose().split_once('?').unwrap().1;
        assert!(link.expose().contains("/change-password?"), "{}", link.expose());
        assert!(is_link_query(query));
        let key = link_key(&secret());
        let scope = crate::auth_flows::LinkScope::from(&tenant);
        assert_eq!(
            crate::auth_flows::PasswordReset::verify(&scope, &signed_link(query), &key),
            Ok(42)
        );
        // Same slug, new id: a purged and re-created tenant.
        let other = crate::auth_flows::LinkScope::from(&org(8));
        assert!(crate::auth_flows::PasswordReset::verify(&other, &signed_link(query), &key).is_err());
        let wrong = link_key(&SessionSecret::from_bytes(vec![1; 32]));
        assert!(crate::auth_flows::PasswordReset::verify(&scope, &signed_link(query), &wrong).is_err());
    }
}
