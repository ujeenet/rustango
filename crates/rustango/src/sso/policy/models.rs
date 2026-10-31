//! Tenant-scope tables of the SSO access policy. Only a superuser adds,
//! changes or deletes their rows in the admin.

use crate::sql::Auto;

/// One invitation to sign up through SSO: an email or a whole domain.
#[derive(crate::Model, Debug, Clone)]
#[rustango(
    table = "rustango_sso_invites",
    admin(
        list_display = "target, provider_slug, uses, max_uses, expires_at",
        search_fields = "target",
        ordering = "target",
        readonly_fields = "uses, used_at, used_by, invited_by, created_at",
    )
)]
#[allow(dead_code)]
pub struct SsoInvite {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    /// The provider it is good for; empty: any.
    #[rustango(max_length = 64)]
    pub provider_slug: Option<String>,
    /// `ann@client.org`, or `@client.org` / `*.client.org` for a domain.
    #[rustango(max_length = 254, index)]
    pub target: String,
    /// Roles granted on sign-up, comma separated.
    #[rustango(max_length = 1000)]
    pub roles: Option<String>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Empty: unlimited.
    pub max_uses: Option<i32>,
    #[rustango(default = "0")]
    pub uses: i32,
    pub used_at: Option<chrono::DateTime<chrono::Utc>>,
    pub used_by: Option<i64>,
    pub invited_by: Option<i64>,
    /// Re-link: the next sign-in links this existing user instead of creating one.
    pub link_user_id: Option<i64>,
    #[rustango(auto_now_add)]
    pub created_at: Auto<chrono::DateTime<chrono::Utc>>,
}

/// A tenant's tightening of one shared provider. Empty fields keep the
/// shared value; a set one can only narrow it.
#[derive(crate::Model, Debug, Clone)]
#[rustango(
    table = "rustango_sso_shared_overrides",
    admin(
        list_display = "shared_slug, signup_mode, sso_only",
        ordering = "shared_slug"
    )
)]
#[allow(dead_code)]
pub struct SsoSharedOverride {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64, unique)]
    pub shared_slug: String,
    #[rustango(max_length = 2000)]
    pub allowed_email_domains: Option<String>,
    #[rustango(max_length = 2000)]
    pub blocked_email_domains: Option<String>,
    #[rustango(max_length = 1000)]
    pub allowed_idp_tenants: Option<String>,
    #[rustango(max_length = 1000)]
    pub allowed_hosted_domains: Option<String>,
    #[rustango(default = "'{}'")]
    pub required_claims: serde_json::Value,
    pub require_verified_email: Option<bool>,
    #[rustango(
        max_length = 16,
        choices = "closed:Closed, invite_only:Invite only, allowlist:Allowlist, open:Open"
    )]
    pub signup_mode: Option<String>,
    pub sso_only: Option<bool>,
    pub session_ttl_secs: Option<i64>,
    pub superuser_session_ttl_secs: Option<i64>,
}

/// The tenant-wide SSO settings: one row, named `default`.
#[derive(crate::Model, Debug, Clone)]
#[rustango(
    table = "rustango_sso_tenant_policy",
    admin(list_display = "name, password_login, sso_only, default_provider")
)]
#[allow(dead_code)]
pub struct SsoTenantPolicy {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 16, unique, default = "'default'")]
    pub name: String,
    /// Shown, escaped, on the login page after a refused SSO sign-in.
    #[rustango(max_length = 500)]
    pub access_denied_message: Option<String>,
    #[rustango(
        max_length = 16,
        default = "'on'",
        choices = "on:On, off:Off, superusers_only:Superusers only"
    )]
    pub password_login: String,
    #[rustango(default = "false")]
    pub sso_only: bool,
    #[rustango(max_length = 64)]
    pub default_provider: Option<String>,
}

#[cfg(feature = "admin")]
fn superuser_only(parts: &axum::http::request::Parts, _: Option<&serde_json::Value>) -> bool {
    crate::admin::session::is_superuser(parts)
}

// These rows decide who may sign up and sign in.
#[cfg(feature = "admin")]
crate::register_admin_object_permission!("rustango_sso_invites", "add", superuser_only);
#[cfg(feature = "admin")]
crate::register_admin_object_permission!("rustango_sso_invites", "change", superuser_only);
#[cfg(feature = "admin")]
crate::register_admin_object_permission!("rustango_sso_invites", "delete", superuser_only);
#[cfg(feature = "admin")]
crate::register_admin_object_permission!("rustango_sso_shared_overrides", "add", superuser_only);
#[cfg(feature = "admin")]
crate::register_admin_object_permission!("rustango_sso_shared_overrides", "change", superuser_only);
#[cfg(feature = "admin")]
crate::register_admin_object_permission!("rustango_sso_shared_overrides", "delete", superuser_only);
#[cfg(feature = "admin")]
crate::register_admin_object_permission!("rustango_sso_tenant_policy", "add", superuser_only);
#[cfg(feature = "admin")]
crate::register_admin_object_permission!("rustango_sso_tenant_policy", "change", superuser_only);
#[cfg(feature = "admin")]
crate::register_admin_object_permission!("rustango_sso_tenant_policy", "delete", superuser_only);
#[cfg(feature = "admin")]
crate::register_admin_object_permission!("rustango_sso_invites", "view", superuser_only);
#[cfg(feature = "admin")]
crate::register_admin_object_permission!("rustango_sso_shared_overrides", "view", superuser_only);
#[cfg(feature = "admin")]
crate::register_admin_object_permission!("rustango_sso_tenant_policy", "view", superuser_only);
