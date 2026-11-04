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
    #[rustango(help_text = "Slug of the provider this invite is for. Empty: any provider.")]
    pub provider_slug: Option<String>,
    /// `ann@client.org`, or `@client.org` / `*.client.org` for a domain.
    #[rustango(max_length = 254, index)]
    #[rustango(
        help_text = "Who may sign up: one email (`ann@client.org`) or a domain (`@client.org`, or `*.client.org` for its subdomains). A domain invite needs max uses or an expiry."
    )]
    pub target: String,
    /// Roles granted on sign-up, comma separated.
    #[rustango(max_length = 1000)]
    #[rustango(
        help_text = "Roles the new user gets, comma separated. They replace the provider's default roles."
    )]
    pub roles: Option<String>,
    #[rustango(
        help_text = "After this time the invite no longer works. Empty: it does not expire."
    )]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Empty: unlimited.
    #[rustango(help_text = "How many sign-ups may use this invite. Empty: no limit.")]
    pub max_uses: Option<i32>,
    #[rustango(default = "0")]
    pub uses: i32,
    pub used_at: Option<chrono::DateTime<chrono::Utc>>,
    pub used_by: Option<i64>,
    pub invited_by: Option<i64>,
    /// Re-link: the next sign-in links this existing user instead of creating one.
    #[rustango(
        help_text = "Re-link only: the existing user the next sign-in links, instead of a new account. Set by the Re-link action."
    )]
    pub link_user_id: Option<i64>,
    /// Re-link: [`crate::sso::ProviderKey::fingerprint`] of the old link's
    /// provider; only a sign-in through that provider may use the invite.
    #[rustango(max_length = 64)]
    #[rustango(
        help_text = "Re-link only: the provider the next sign-in must come through. Set by the Re-link action; leave empty."
    )]
    pub link_key: Option<String>,
    #[rustango(auto_now_add)]
    pub created_at: Auto<chrono::DateTime<chrono::Utc>>,
}

/// A tenant's tightening of one shared provider. Empty fields keep the
/// shared value; a set one can only narrow it.
#[derive(crate::Model, Debug, Clone)]
#[rustango(
    table = "rustango_sso_shared_overrides",
    admin(
        list_display = "shared_slug, signup_mode, sso_only, allowed_roles",
        ordering = "shared_slug",
        readonly_fields = "shared_provider_id, shared_issuer",
    )
)]
#[allow(dead_code)]
pub struct SsoSharedOverride {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    /// The shared provider's slug when the override was bound, for display.
    #[rustango(max_length = 64, index)]
    #[rustango(help_text = "Slug of the shared provider this row narrows for this tenant.")]
    pub shared_slug: String,
    /// The shared provider row this override is bound to: set on its first
    /// use, so a renamed row keeps it and a new row with the slug does not.
    #[rustango(unique)]
    #[rustango(
        help_text = "The shared provider row this override is bound to, set at its first use."
    )]
    pub shared_provider_id: Option<i64>,
    /// That row's issuer when bound: a repointed row refuses until re-bound.
    #[rustango(max_length = 300)]
    #[rustango(
        help_text = "That row's issuer when bound. If the shared row moves to another issuer, sign-in is refused until the override is added again."
    )]
    pub shared_issuer: Option<String>,
    #[rustango(max_length = 2000)]
    #[rustango(
        help_text = "Security: only these email domains may sign in here, on top of the shared rules. Same comma or newline format as on the provider."
    )]
    pub allowed_email_domains: Option<String>,
    #[rustango(max_length = 2000)]
    #[rustango(help_text = "More email domains that never sign in here.")]
    pub blocked_email_domains: Option<String>,
    #[rustango(max_length = 1000)]
    #[rustango(
        help_text = "Microsoft directory IDs allowed here, comma separated. Narrows the shared list."
    )]
    pub allowed_idp_tenants: Option<String>,
    #[rustango(max_length = 1000)]
    #[rustango(
        help_text = "Google Workspace domains allowed here, comma separated. Narrows the shared list."
    )]
    pub allowed_hosted_domains: Option<String>,
    #[rustango(default = "'{}'")]
    #[rustango(
        help_text = "More claims a sign-in must carry here, as JSON: `{\"roles\": [\"App.User\"]}`."
    )]
    pub required_claims: serde_json::Value,
    #[rustango(
        max_length = 16,
        choices = "closed:Closed, invite_only:Invite only, allowlist:Allowlist, open:Open"
    )]
    #[rustango(
        help_text = "Security: sign-up here. It can only be stricter than the shared provider's. Empty: the shared value."
    )]
    pub signup_mode: Option<String>,
    /// Off: no email linking here. On or empty: the shared value.
    #[rustango(help_text = "Off: no email linking here. Empty or on: the shared setting.")]
    pub allow_email_link: Option<bool>,
    /// Off: email linking never links a role holder here.
    #[rustango(
        help_text = "Off: email linking never links a user with roles here. Empty or on: the shared setting."
    )]
    pub allow_email_link_staff: Option<bool>,
    #[rustango(help_text = "On: users of this provider may not sign in with a password here.")]
    pub sso_only: Option<bool>,
    #[rustango(help_text = "A shorter longest session here, in seconds. Empty: the shared value.")]
    pub session_ttl_secs: Option<i64>,
    #[rustango(
        help_text = "A shorter longest superuser session here, in seconds. Empty: the shared value."
    )]
    pub superuser_session_ttl_secs: Option<i64>,
    /// Role names the shared provider may grant here, comma separated.
    /// Empty: none.
    #[rustango(max_length = 1000)]
    #[rustango(
        help_text = "Roles the shared provider may grant here, comma separated. Empty: none."
    )]
    pub allowed_roles: Option<String>,
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
    #[rustango(help_text = "Always `default`: the tenant uses this one row.")]
    pub name: String,
    /// Shown, escaped, on the login page after a refused SSO sign-in.
    #[rustango(max_length = 500)]
    #[rustango(
        help_text = "Message shown on the sign-in page after a refused SSO sign-in, e.g. who to contact. Shown as plain text."
    )]
    pub access_denied_message: Option<String>,
    #[rustango(
        max_length = 16,
        default = "'on'",
        choices = "on:On, off:Off, superusers_only:Superusers only"
    )]
    #[rustango(
        help_text = "The password form: `on`, `off` (also for superusers), or `superusers_only` (shown only at `?local=1`, for superusers)."
    )]
    pub password_login: String,
    #[rustango(default = "false")]
    #[rustango(help_text = "No one except a superuser may sign in with a password.")]
    pub sso_only: bool,
    #[rustango(max_length = 64)]
    #[rustango(
        help_text = "Slug of the provider the sign-in page opens straight away, except at `?local=1` or after an error."
    )]
    pub default_provider: Option<String>,
    /// SSO sign-ups allowed per hour. Empty: 100; `0`: no limit.
    #[rustango(help_text = "Most SSO sign-ups per hour. Empty: 100; `0`: no limit.")]
    pub signups_per_hour: Option<i32>,
}

// These rows decide who may sign up and sign in.
#[cfg(feature = "admin")]
crate::object_permissions::register_framework_object_permission!(
    "rustango_sso_invites",
    "add",
    crate::object_permissions::superuser_only
);
#[cfg(feature = "admin")]
crate::object_permissions::register_framework_object_permission!(
    "rustango_sso_invites",
    "change",
    crate::object_permissions::superuser_only
);
#[cfg(feature = "admin")]
crate::object_permissions::register_framework_object_permission!(
    "rustango_sso_invites",
    "delete",
    crate::object_permissions::superuser_only
);
#[cfg(feature = "admin")]
crate::object_permissions::register_framework_object_permission!(
    "rustango_sso_shared_overrides",
    "add",
    crate::object_permissions::superuser_only
);
#[cfg(feature = "admin")]
crate::object_permissions::register_framework_object_permission!(
    "rustango_sso_shared_overrides",
    "change",
    crate::object_permissions::superuser_only
);
#[cfg(feature = "admin")]
crate::object_permissions::register_framework_object_permission!(
    "rustango_sso_shared_overrides",
    "delete",
    crate::object_permissions::superuser_only
);
#[cfg(feature = "admin")]
crate::object_permissions::register_framework_object_permission!(
    "rustango_sso_tenant_policy",
    "add",
    crate::object_permissions::superuser_only
);
#[cfg(feature = "admin")]
crate::object_permissions::register_framework_object_permission!(
    "rustango_sso_tenant_policy",
    "change",
    crate::object_permissions::superuser_only
);
#[cfg(feature = "admin")]
crate::object_permissions::register_framework_object_permission!(
    "rustango_sso_tenant_policy",
    "delete",
    crate::object_permissions::superuser_only
);
#[cfg(feature = "admin")]
crate::object_permissions::register_framework_object_permission!(
    "rustango_sso_invites",
    "view",
    crate::object_permissions::superuser_only
);
#[cfg(feature = "admin")]
crate::object_permissions::register_framework_object_permission!(
    "rustango_sso_shared_overrides",
    "view",
    crate::object_permissions::superuser_only
);
#[cfg(feature = "admin")]
crate::object_permissions::register_framework_object_permission!(
    "rustango_sso_tenant_policy",
    "view",
    crate::object_permissions::superuser_only
);
