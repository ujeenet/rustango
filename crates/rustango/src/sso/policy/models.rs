//! Tenant-scope tables of the SSO access policy. Only a superuser adds,
//! changes, deletes or views their rows in the admin.

use crate::sql::Auto;

/// One invitation to sign up through SSO: an email or a whole domain.
#[derive(crate::Model, Debug, Clone)]
#[rustango(
    table = "rustango_sso_invites",
    access = "admin_only",
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
    #[rustango(verbose_name = "Provider slug")]
    pub provider_slug: Option<String>,
    /// `ann@client.org`, or `@client.org` / `*.client.org` for a domain.
    #[rustango(max_length = 254, index)]
    #[rustango(
        help_text = "Who may sign up: one email (`ann@client.org`) or a domain (`@client.org`, or `*.client.org` for its subdomains). A domain invite needs max uses or an expiry."
    )]
    #[rustango(verbose_name = "Email or domain")]
    pub target: String,
    /// Roles granted on sign-up, comma separated.
    #[rustango(max_length = 1000)]
    #[rustango(
        help_text = "Roles the new user gets, comma separated. They replace the provider's default roles."
    )]
    #[rustango(verbose_name = "Roles")]
    pub roles: Option<String>,
    #[rustango(
        help_text = "After this time the invite no longer works. Empty: it does not expire."
    )]
    #[rustango(verbose_name = "Expires at")]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Empty: unlimited.
    #[rustango(help_text = "How many sign-ups may use this invite. Empty: no limit.")]
    #[rustango(verbose_name = "Max uses")]
    pub max_uses: Option<i32>,
    #[rustango(default = "0")]
    #[rustango(verbose_name = "Uses")]
    #[rustango(help_text = "How many sign-ups this invite has let in. Counted on each use.")]
    pub uses: i32,
    #[rustango(verbose_name = "Used at")]
    #[rustango(help_text = "When the invite was last used.")]
    pub used_at: Option<chrono::DateTime<chrono::Utc>>,
    #[rustango(verbose_name = "Used by")]
    #[rustango(help_text = "Id of the user who last signed up with it.")]
    pub used_by: Option<i64>,
    #[rustango(verbose_name = "Invited by")]
    #[rustango(help_text = "Id of the admin who created the invite.")]
    pub invited_by: Option<i64>,
    /// Re-link: the next sign-in links this existing user instead of creating one.
    #[rustango(
        help_text = "Re-link only: the existing user the next sign-in links, instead of a new account. Set by the Re-link action."
    )]
    #[rustango(verbose_name = "Re-link user")]
    pub link_user_id: Option<i64>,
    /// Re-link: [`crate::sso::ProviderKey::fingerprint`] of the old link's
    /// provider; only a sign-in through that provider may use the invite.
    #[rustango(max_length = 64)]
    #[rustango(
        help_text = "Re-link only: the provider the next sign-in must come through. Set by the Re-link action; leave empty."
    )]
    #[rustango(verbose_name = "Re-link provider")]
    pub link_key: Option<String>,
    #[rustango(auto_now_add)]
    #[rustango(verbose_name = "Created at")]
    #[rustango(help_text = "When the row was created. Set by the database.")]
    pub created_at: Auto<chrono::DateTime<chrono::Utc>>,
}

/// A tenant's tightening of one shared provider. Empty fields keep the
/// shared value; a set one can only narrow it.
#[derive(crate::Model, Debug, Clone)]
#[rustango(
    table = "rustango_sso_shared_overrides",
    access = "admin_only",
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
    #[rustango(verbose_name = "Shared provider slug")]
    pub shared_slug: String,
    /// The shared provider row this override is bound to: set on its first
    /// use, so a renamed row keeps it and a new row with the slug does not.
    #[rustango(unique)]
    #[rustango(
        help_text = "The shared provider row this override is bound to, set at its first use."
    )]
    #[rustango(verbose_name = "Shared provider row")]
    pub shared_provider_id: Option<i64>,
    /// That row's issuer when bound: a repointed row refuses until re-bound.
    #[rustango(max_length = 300)]
    #[rustango(
        help_text = "That row's issuer when bound. If the shared row moves to another issuer, sign-in is refused until the override is added again."
    )]
    #[rustango(verbose_name = "Bound issuer")]
    pub shared_issuer: Option<String>,
    #[rustango(max_length = 2000)]
    #[rustango(
        help_text = "Security: only these email domains may sign in here, on top of the shared rules. Same comma or newline format as on the provider."
    )]
    #[rustango(verbose_name = "Allowed email domains")]
    pub allowed_email_domains: Option<String>,
    #[rustango(max_length = 2000)]
    #[rustango(help_text = "More email domains that never sign in here.")]
    #[rustango(verbose_name = "Blocked email domains")]
    pub blocked_email_domains: Option<String>,
    #[rustango(max_length = 1000)]
    #[rustango(
        help_text = "Microsoft directory IDs allowed here, comma separated. Narrows the shared list."
    )]
    #[rustango(verbose_name = "Allowed Microsoft directories")]
    pub allowed_idp_tenants: Option<String>,
    #[rustango(max_length = 1000)]
    #[rustango(
        help_text = "Google Workspace domains allowed here, comma separated. Narrows the shared list."
    )]
    #[rustango(verbose_name = "Allowed Google Workspace domains")]
    pub allowed_hosted_domains: Option<String>,
    #[rustango(default = "'{}'")]
    #[rustango(
        help_text = "More claims a sign-in must carry here, as JSON: `{\"roles\": [\"App.User\"]}`."
    )]
    #[rustango(verbose_name = "Required claims")]
    pub required_claims: serde_json::Value,
    #[rustango(
        max_length = 16,
        choices = "closed:Closed, invite_only:Invite only, allowlist:Allowlist, open:Open"
    )]
    #[rustango(
        help_text = "Security: sign-up here. It can only be stricter than the shared provider's. Empty: the shared value."
    )]
    #[rustango(verbose_name = "Sign-up mode")]
    pub signup_mode: Option<String>,
    /// Off: no email linking here. On or empty: the shared value.
    #[rustango(help_text = "Off: no email linking here. Empty or on: the shared setting.")]
    #[rustango(verbose_name = "Allow email linking")]
    pub allow_email_link: Option<bool>,
    /// Off: email linking never links a role holder here.
    #[rustango(
        help_text = "Off: email linking never links a user with any role or permission here. Empty or on: the shared setting."
    )]
    #[rustango(verbose_name = "Allow email linking for staff")]
    pub allow_email_link_staff: Option<bool>,
    #[rustango(help_text = "On: users of this provider may not sign in with a password here.")]
    #[rustango(verbose_name = "Sign-in only through SSO")]
    pub sso_only: Option<bool>,
    #[rustango(help_text = "A shorter longest session here, in seconds. Empty: the shared value.")]
    #[rustango(verbose_name = "Session length (seconds)")]
    pub session_ttl_secs: Option<i64>,
    #[rustango(
        help_text = "A shorter longest superuser session here, in seconds. Empty: the shared value."
    )]
    #[rustango(verbose_name = "Superuser session length (seconds)")]
    pub superuser_session_ttl_secs: Option<i64>,
    /// Role names the shared provider may grant here, comma separated.
    /// Empty: none.
    #[rustango(max_length = 1000)]
    #[rustango(
        help_text = "Roles the shared provider may grant here, comma separated. Empty: none."
    )]
    #[rustango(verbose_name = "Roles it may grant")]
    pub allowed_roles: Option<String>,
}

/// The tenant-wide SSO settings: one row, named `default`.
#[derive(crate::Model, Debug, Clone)]
#[rustango(
    table = "rustango_sso_tenant_policy",
    access = "admin_only",
    admin(list_display = "name, password_login, sso_only, default_provider")
)]
#[allow(dead_code)]
pub struct SsoTenantPolicy {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 16, unique, default = "'default'")]
    #[rustango(help_text = "Always `default`: the tenant uses this one row.")]
    #[rustango(verbose_name = "Name")]
    pub name: String,
    /// Shown, escaped, on the login page after a refused SSO sign-in.
    #[rustango(max_length = 500)]
    #[rustango(
        help_text = "Message shown on the sign-in page after a refused SSO sign-in, e.g. who to contact. Shown as plain text."
    )]
    #[rustango(verbose_name = "Access denied message")]
    pub access_denied_message: Option<String>,
    #[rustango(
        max_length = 16,
        default = "'on'",
        choices = "on:On, off:Off, superusers_only:Superusers only"
    )]
    #[rustango(
        help_text = "The password form: `on`, `off` (also for superusers), or `superusers_only` (shown only at `?local=1`, for superusers)."
    )]
    #[rustango(verbose_name = "Password login")]
    pub password_login: String,
    #[rustango(default = "false")]
    #[rustango(help_text = "No one except a superuser may sign in with a password.")]
    #[rustango(verbose_name = "Sign-in only through SSO")]
    pub sso_only: bool,
    #[rustango(max_length = 64)]
    #[rustango(
        help_text = "Slug of the provider the sign-in page opens straight away, except at `?local=1` or after an error."
    )]
    #[rustango(verbose_name = "Default provider")]
    pub default_provider: Option<String>,
    /// SSO sign-ups allowed per hour. Empty: 100; `0`: no limit.
    #[rustango(help_text = "Most SSO sign-ups per hour. Empty: 100; `0`: no limit.")]
    #[rustango(verbose_name = "Sign-ups per hour")]
    pub signups_per_hour: Option<i32>,
}

/// Where a sign-in without `?next=` lands: the first matching rule by `position`.
#[derive(crate::Model, Debug, Clone)]
#[rustango(
    table = "rustango_sso_landing_rules",
    access = "admin_only",
    admin(
        list_display = "position, match_kind, match_name, match_value, landing_url",
        ordering = "position",
    )
)]
#[allow(dead_code)]
pub struct SsoLandingRule {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(default = "0", index)]
    #[rustango(
        help_text = "Rules are tried from the lowest number up and the first match wins. Equal numbers go by id."
    )]
    #[rustango(verbose_name = "Position")]
    pub position: i32,
    #[rustango(max_length = 16, choices = "role:Role, claim:Claim, any:Any")]
    #[rustango(
        help_text = "`role`: the user holds the role in Value (never on the bare admin, which has no roles). `claim`: the SSO sign-in carries claim Name with Value. `any`: every sign-in; put it last as the default."
    )]
    #[rustango(verbose_name = "Match")]
    pub match_kind: String,
    #[rustango(max_length = 128)]
    #[rustango(help_text = "The claim name, for `claim` rules only. Empty for the others.")]
    #[rustango(verbose_name = "Name")]
    pub match_name: Option<String>,
    #[rustango(max_length = 254)]
    #[rustango(
        help_text = "The role name (`role`) or the claim value (`claim`), matched exactly. Empty for `any`."
    )]
    #[rustango(verbose_name = "Value")]
    pub match_value: Option<String>,
    #[rustango(max_length = 500)]
    #[rustango(
        help_text = "A path on this site starting with `/`, as on the tenant's own host, e.g. `/admin/reports`. A path prefix is added for you. Used only when the sign-in has no `?next=`. Not access control: the page must check permissions itself."
    )]
    #[rustango(verbose_name = "Landing path")]
    pub landing_url: String,
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
// Landing rules name roles and claims.
#[cfg(feature = "admin")]
crate::object_permissions::register_framework_object_permission!(
    "rustango_sso_landing_rules",
    "add",
    crate::object_permissions::superuser_only
);
#[cfg(feature = "admin")]
crate::object_permissions::register_framework_object_permission!(
    "rustango_sso_landing_rules",
    "change",
    crate::object_permissions::superuser_only
);
#[cfg(feature = "admin")]
crate::object_permissions::register_framework_object_permission!(
    "rustango_sso_landing_rules",
    "delete",
    crate::object_permissions::superuser_only
);
#[cfg(feature = "admin")]
crate::object_permissions::register_framework_object_permission!(
    "rustango_sso_landing_rules",
    "view",
    crate::object_permissions::superuser_only
);
