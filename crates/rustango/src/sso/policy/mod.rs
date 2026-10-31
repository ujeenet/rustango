//! Who may sign in through SSO. Every SSO surface (tenant console, member
//! flow, bare admin) passes through [`admit`]; a refusal names its rule
//! only in the server log and the audit log, never to the browser.
//!
//! A policy is a stack of layers (the provider, later a tenant override of
//! a shared provider). A sign-in passes only if every layer passes it on its
//! own, so a layer can only tighten.

#[cfg(feature = "admin")]
pub(crate) mod admin;
pub mod domain;
pub mod identity;
pub mod jwks;
pub mod models;
pub mod signup;

pub use domain::{DomainList, DomainRule, EmailAddr, PolicyError, TokenList};
pub use identity::{IdToken, Identity, RequiredClaims};
pub use models::{SsoInvite, SsoSharedOverride, SsoTenantPolicy};
pub use signup::{InviteTarget, RoleMap, RoleNames, SignupColumns, SignupPolicy};

use crate::sql::Pool;

use super::link::{self, AccountLookup, EmailLookup, LinkRefusal, ProviderKey};

macro_rules! policy_enum {
    ($(#[$m:meta])* $name:ident { $($(#[$vm:meta])* $v:ident = $s:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
        #[serde(rename_all = "snake_case")]
        #[non_exhaustive]
        pub enum $name { $($(#[$vm])* $v),+ }

        impl $name {
            /// The stored spelling.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$v => $s),+ }
            }
        }

        impl std::str::FromStr for $name {
            type Err = PolicyError;
            fn from_str(s: &str) -> Result<Self, PolicyError> {
                match s.trim() {
                    $($s => Ok(Self::$v),)+
                    other => Err(PolicyError(format!(
                        concat!("`{}` is not a ", stringify!($name)), other
                    ))),
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

policy_enum!(
    /// Who may sign up. Ordered loosest last, so the tighter of two is the `min`.
    #[derive(PartialOrd, Ord)]
    SignupMode {
        Closed = "closed",
        InviteOnly = "invite_only",
        Allowlist = "allowlist",
        Open = "open",
    }
);

policy_enum!(
    /// The tenant's password form.
    PasswordLogin {
        On = "on",
        Off = "off",
        /// Only superusers: the break-glass way in.
        SuperusersOnly = "superusers_only",
    }
);

policy_enum!(
    /// Where a new user's username comes from.
    UsernameFrom {
        EmailLocal = "email_local",
        Email = "email",
        PreferredUsername = "preferred_username",
    }
);

policy_enum!(
    /// What a taken username does on sign-up.
    UsernameClash {
        Suffix = "suffix",
        Refuse = "refuse",
    }
);

policy_enum!(
    /// Which login an SSO sign-in is for.
    Surface {
        TenantAdmin = "tenant_admin",
        Member = "member",
        BareAdmin = "admin",
    }
);

/// Why a sign-in was refused. [`Self::rule`] is the stable audit token.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Refusal {
    NoSubject,
    /// A rule needs a verified email and the IdP vouched for none.
    EmailUnverified,
    DomainNotAllowed {
        domain: String,
    },
    DomainBlocked {
        rule: String,
    },
    IdpTenant,
    HostedDomain,
    Claim {
        name: String,
    },
    /// A stored policy value does not parse, or cannot be read.
    PolicyInvalid,
    /// No account, and sign-up is closed.
    SignupClosed,
    NotInvited,
    InviteExpired,
    InviteUsedUp,
    UsernameTaken,
    EmailLinkDisabled,
    /// Email linking never links a privileged account.
    Privileged,
    EmailCollides,
    Inactive,
}

impl Refusal {
    /// Stable token for the audit log and alerts.
    #[must_use]
    pub const fn rule(&self) -> &'static str {
        match self {
            Self::NoSubject => "no_subject",
            Self::EmailUnverified => "email_unverified",
            Self::DomainNotAllowed { .. } => "domain_not_allowed",
            Self::DomainBlocked { .. } => "domain_blocked",
            Self::IdpTenant => "idp_tenant",
            Self::HostedDomain => "hosted_domain",
            Self::Claim { .. } => "claim",
            Self::PolicyInvalid => "policy_invalid",
            Self::SignupClosed => "signup_closed",
            Self::NotInvited => "not_invited",
            Self::InviteExpired => "invite_expired",
            Self::InviteUsedUp => "invite_used_up",
            Self::UsernameTaken => "username_taken",
            Self::EmailLinkDisabled => "email_link_disabled",
            Self::Privileged => "privileged",
            Self::EmailCollides => "email_collides",
            Self::Inactive => "inactive",
        }
    }

    /// The `user_login_failed` reason.
    #[cfg(feature = "signals")]
    #[must_use]
    pub fn failure_reason(&self) -> crate::signals::auth::AuthFailureReason {
        use crate::signals::auth::AuthFailureReason as R;
        match self {
            Self::Inactive => R::Inactive,
            _ => R::InvalidCredentials,
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DomainNotAllowed { domain } => write!(f, "domain_not_allowed ({domain})"),
            Self::DomainBlocked { rule } => write!(f, "domain_blocked ({rule})"),
            Self::Claim { name } => write!(f, "claim ({name})"),
            other => f.write_str(other.rule()),
        }
    }
}

/// [`admit`]'s failure: a refusal, or a storage error (no refusal; nothing is audited).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Denied {
    Refused(Refusal),
    Storage(String),
}

impl From<Refusal> for Denied {
    fn from(r: Refusal) -> Self {
        Self::Refused(r)
    }
}

impl std::fmt::Display for Denied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(r) => write!(f, "refused: {r}"),
            Self::Storage(e) => write!(f, "storage: {e}"),
        }
    }
}

/// One layer's sign-in rules. The default has none.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SignInPolicy {
    pub allowed_email_domains: Option<DomainList>,
    pub blocked_email_domains: Option<DomainList>,
    pub allowed_idp_tenants: Option<TokenList>,
    pub allowed_hosted_domains: Option<DomainList>,
    pub required_claims: RequiredClaims,
    /// The domain allowlist accepts only an IdP-verified email.
    pub require_verified_email: bool,
}

impl Default for SignInPolicy {
    fn default() -> Self {
        Self {
            allowed_email_domains: None,
            blocked_email_domains: None,
            allowed_idp_tenants: None,
            allowed_hosted_domains: None,
            required_claims: RequiredClaims::default(),
            require_verified_email: true,
        }
    }
}

/// A provider row's stored sign-in columns, unparsed.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct SignInColumns<'a> {
    pub allowed_email_domains: Option<&'a str>,
    pub blocked_email_domains: Option<&'a str>,
    pub allowed_idp_tenants: Option<&'a str>,
    pub allowed_hosted_domains: Option<&'a str>,
    pub required_claims: Option<&'a serde_json::Value>,
    pub require_verified_email: bool,
}

impl SignInPolicy {
    /// Parse stored columns.
    ///
    /// # Errors
    /// The first value that does not parse, naming its column.
    pub fn parse(c: &SignInColumns<'_>) -> Result<Self, PolicyError> {
        fn named(col: &'static str) -> impl Fn(PolicyError) -> PolicyError {
            move |e| PolicyError(format!("{col}: {e}"))
        }
        Ok(Self {
            allowed_email_domains: DomainList::parse_opt(c.allowed_email_domains)
                .map_err(named("allowed_email_domains"))?,
            blocked_email_domains: DomainList::parse_opt(c.blocked_email_domains)
                .map_err(named("blocked_email_domains"))?,
            allowed_idp_tenants: TokenList::parse_opt(c.allowed_idp_tenants)
                .map_err(named("allowed_idp_tenants"))?,
            allowed_hosted_domains: DomainList::parse_opt(c.allowed_hosted_domains)
                .map_err(named("allowed_hosted_domains"))?,
            required_claims: c
                .required_claims
                .map(RequiredClaims::parse)
                .transpose()?
                .unwrap_or_default(),
            require_verified_email: c.require_verified_email,
        })
    }

    /// Whether this layer pins Microsoft tenants and `identity`'s `tid` is one.
    fn pins(&self, identity: &Identity) -> bool {
        self.allowed_idp_tenants
            .as_ref()
            .zip(identity.claim_str("tid"))
            .is_some_and(|(list, tid)| list.contains(tid))
    }

    /// This layer's rules alone.
    ///
    /// # Errors
    /// The first rule `identity` fails.
    pub fn check(&self, identity: &Identity) -> Result<(), Refusal> {
        if identity.subject().is_empty() {
            return Err(Refusal::NoSubject);
        }
        if let Some(blocked) = &self.blocked_email_domains {
            // Any email the IdP sent, verified or not.
            if let Some(rule) = identity.email().and_then(|e| blocked.find(e.domain())) {
                return Err(Refusal::DomainBlocked {
                    rule: rule.to_string(),
                });
            }
        }
        if let Some(allowed) = &self.allowed_email_domains {
            let email = if self.require_verified_email {
                identity.verified_email(self.pins(identity))
            } else {
                identity.email()
            };
            let email = email.ok_or(Refusal::EmailUnverified)?;
            if allowed.find(email.domain()).is_none() {
                return Err(Refusal::DomainNotAllowed {
                    domain: email.domain().to_owned(),
                });
            }
        }
        if self.allowed_idp_tenants.is_some() && !self.pins(identity) {
            return Err(Refusal::IdpTenant);
        }
        if let Some(hosted) = &self.allowed_hosted_domains {
            if !identity
                .claim_str("hd")
                .is_some_and(|hd| hosted.find(hd).is_some())
            {
                return Err(Refusal::HostedDomain);
            }
        }
        self.required_claims.check(identity)
    }
}

/// The policy one sign-in runs under: every layer must admit it.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct EffectivePolicy {
    layers: Vec<Result<SignInPolicy, PolicyError>>,
    allow_email_link: bool,
    signup: Result<SignupPolicy, PolicyError>,
    /// The surface's sign-up mode when the provider sets none.
    default_signup: SignupMode,
}

impl EffectivePolicy {
    /// A provider's own policy.
    #[must_use]
    pub fn new(provider: Result<SignInPolicy, PolicyError>, allow_email_link: bool) -> Self {
        Self {
            layers: vec![provider],
            allow_email_link,
            signup: Ok(SignupPolicy::default()),
            default_signup: SignupMode::Closed,
        }
    }

    /// No sign-in rules: the behaviour before policies existed.
    #[must_use]
    pub fn legacy(allow_email_link: bool) -> Self {
        Self {
            layers: Vec::new(),
            allow_email_link,
            signup: Ok(SignupPolicy::default()),
            default_signup: SignupMode::Closed,
        }
    }

    /// The provider's sign-up settings; an error refuses every sign-in.
    #[must_use]
    pub fn with_signup(mut self, signup: Result<SignupPolicy, PolicyError>) -> Self {
        self.signup = signup;
        self
    }

    /// The sign-up mode when the provider sets none (default closed).
    #[must_use]
    pub fn default_signup(mut self, mode: SignupMode) -> Self {
        self.default_signup = mode;
        self
    }

    /// Who may sign up on `surface`. The bare admin never signs anyone up.
    fn signup_mode(&self, surface: Surface, signup: &SignupPolicy) -> SignupMode {
        match surface {
            Surface::BareAdmin => SignupMode::Closed,
            _ => signup.mode.unwrap_or(self.default_signup),
        }
    }

    /// Add a layer. It can only narrow what the others admit.
    #[must_use]
    pub fn tighten(mut self, layer: Result<SignInPolicy, PolicyError>) -> Self {
        self.layers.push(layer);
        self
    }

    /// Every layer's rules, each on its own.
    ///
    /// # Errors
    /// The first refusal; [`Refusal::PolicyInvalid`] for a layer that did not parse.
    pub fn check(&self, identity: &Identity) -> Result<(), Refusal> {
        if identity.subject().is_empty() {
            return Err(Refusal::NoSubject);
        }
        for layer in &self.layers {
            match layer {
                Ok(p) => p.check(identity)?,
                Err(e) => {
                    tracing::error!(target: "rustango::sso", "SSO policy does not parse: {e}");
                    return Err(Refusal::PolicyInvalid);
                }
            }
        }
        Ok(())
    }

    /// The email to link by. A pinned `tid` vouches only when every layer pins it.
    pub(crate) fn link_email<'a>(&self, identity: &'a Identity) -> Option<&'a EmailAddr> {
        let pinned = !self.layers.is_empty()
            && self
                .layers
                .iter()
                .all(|l| l.as_ref().is_ok_and(|p| p.pins(identity)));
        identity.verified_email(pinned)
    }
}

/// One sign-in to decide.
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct AdmitRequest<'a> {
    pub pool: &'a Pool,
    pub surface: Surface,
    pub key: &'a ProviderKey,
    pub policy: &'a EffectivePolicy,
    pub identity: &'a Identity,
    /// The provider's slug: invites for one provider match only it.
    pub provider: &'a str,
}

impl<'a> AdmitRequest<'a> {
    #[must_use]
    pub fn new(
        pool: &'a Pool,
        surface: Surface,
        key: &'a ProviderKey,
        policy: &'a EffectivePolicy,
        identity: &'a Identity,
    ) -> Self {
        Self {
            pool,
            surface,
            key,
            policy,
            identity,
            provider: "",
        }
    }

    /// Name the provider, so invites for it apply.
    #[must_use]
    pub fn provider(mut self, slug: &'a str) -> Self {
        self.provider = slug;
        self
    }
}

/// An admitted sign-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Admitted {
    pub user_id: i64,
    /// Sign-up made the account.
    pub created: bool,
}

/// Decide one SSO sign-in: the policy's rules, the link, a re-link
/// invite, a link by verified email, then sign-up. Stamps the link's
/// `last_used_at`; on a tenant user table also syncs mapped roles and the profile.
///
/// # Errors
/// [`Denied::Refused`] with the rule; [`Denied::Storage`] on a driver error.
pub async fn admit<A: AccountLookup>(
    req: AdmitRequest<'_>,
    accounts: &A,
) -> Result<Admitted, Denied> {
    let AdmitRequest {
        pool,
        surface,
        key,
        policy,
        identity,
        ..
    } = req;
    policy.check(identity)?;
    let signup = policy.signup.as_ref().map_err(|e| {
        tracing::error!(target: "rustango::sso", "SSO sign-up policy does not parse: {e}");
        Refusal::PolicyInvalid
    })?;
    // Sign-up, role and profile sync write the tenant user table only.
    let tenant_users = A::TENANT_USERS && surface != Surface::BareAdmin;
    let subject = identity.subject();
    if let Some(found) = link::linked_user(pool, key, subject)
        .await
        .map_err(Denied::Storage)?
    {
        match accounts
            .by_id(found.user_id)
            .await
            .map_err(Denied::Storage)?
        {
            Some(a)
                if !link::stale(pool, &found, &a)
                    .await
                    .map_err(Denied::Storage)? =>
            {
                if !a.active {
                    return Err(Refusal::Inactive.into());
                }
                if tenant_users {
                    after_sign_in(&req, signup, a.user_id, signup.sync_roles_on_login).await?;
                }
                link::touch(pool, &found).await;
                return Ok(Admitted {
                    user_id: a.user_id,
                    created: false,
                });
            }
            // Dropped as stale: sign in as if unlinked.
            Some(_) => {}
            // The user is gone (a clean miss, not an error): drop the stale link.
            None => link::drop_link(pool, &found)
                .await
                .map_err(Denied::Storage)?,
        }
    }
    // An existing account is linked only by an email the IdP vouches for.
    let email = policy
        .link_email(identity)
        .ok_or(Refusal::EmailUnverified)?;
    #[cfg(feature = "tenancy")]
    if tenant_users {
        if let Some(admitted) = relink(&req, accounts, signup, email).await? {
            after_sign_in(&req, signup, admitted.user_id, true).await?;
            return Ok(admitted);
        }
    }
    let found = match accounts
        .by_email(email.as_str())
        .await
        .map_err(Denied::Storage)?
    {
        EmailLookup::Found(a) => a,
        EmailLookup::Missing if tenant_users => return sign_up(&req, signup, email).await,
        EmailLookup::Missing => return Err(Refusal::SignupClosed.into()),
        EmailLookup::Collides => return Err(Refusal::EmailCollides.into()),
    };
    if !policy.allow_email_link {
        return Err(Refusal::EmailLinkDisabled.into());
    }
    // Never a superuser; a role holder only when the provider opts in.
    let staff_ok = tenant_users && signup.allow_email_link_staff;
    if found.superuser || (found.privileged && !staff_ok) {
        return Err(Refusal::Privileged.into());
    }
    if !found.active {
        return Err(Refusal::Inactive.into());
    }
    if let Err(e) = link::create_link(pool, key, subject, found.user_id).await {
        // A concurrent first login may have linked it already.
        return match link::linked_user(pool, key, subject)
            .await
            .map_err(Denied::Storage)?
        {
            Some(l) if l.user_id == found.user_id => Ok(Admitted {
                user_id: found.user_id,
                created: false,
            }),
            _ => Err(Denied::Storage(e.to_string())),
        };
    }
    if tenant_users {
        after_sign_in(&req, signup, found.user_id, true).await?;
    }
    if let Ok(Some(l)) = link::linked_user(pool, key, subject).await {
        link::touch(pool, &l).await;
    }
    Ok(Admitted {
        user_id: found.user_id,
        created: false,
    })
}

/// Sync a tenant user's mapped roles (when `roles`) and profile after a sign-in.
#[allow(unused_variables)]
async fn after_sign_in(
    req: &AdmitRequest<'_>,
    signup: &SignupPolicy,
    user_id: i64,
    roles: bool,
) -> Result<(), Denied> {
    #[cfg(feature = "tenancy")]
    {
        if roles {
            let unknown = signup::sync_roles(req.pool, user_id, req.identity, signup).await?;
            signup::audit_unknown_roles(req.pool, req.provider, user_id, &unknown).await;
        }
        if signup.sync_profile {
            // A profile refresh never fails the sign-in.
            let email = req.policy.link_email(req.identity);
            if let Err(e) = signup::sync_profile(req.pool, user_id, req.identity, email).await {
                tracing::warn!(target: "rustango::sso", user_id, "profile sync skipped: {e}");
            }
        }
    }
    Ok(())
}

/// A re-link invite for `email` made for this very provider: link its user
/// while that user still has this email. The email-link gate applies: never
/// a superuser, a role holder only with `allow_email_link_staff`.
#[cfg(feature = "tenancy")]
async fn relink(
    req: &AdmitRequest<'_>,
    accounts: &impl AccountLookup,
    signup: &SignupPolicy,
    email: &EmailAddr,
) -> Result<Option<Admitted>, Denied> {
    let kind = signup::InviteKind::Relink(req.key);
    let invite = match signup::find_invite(req.pool, req.provider, email, kind)
        .await
        .map_err(Denied::Storage)?
    {
        Ok(i) => i,
        // No usable re-link invite: carry on as an ordinary sign-in.
        Err(_) => return Ok(None),
    };
    let Some(user_id) = invite.link_user_id else {
        return Ok(None);
    };
    if accounts
        .by_id(user_id)
        .await
        .map_err(Denied::Storage)?
        .is_none()
    {
        // The user is gone: the invite can never be used.
        signup::drop_invite(req.pool, &invite).await;
        return Ok(None);
    }
    // The user's email must still be the invited one.
    let account = match accounts
        .by_email(email.as_str())
        .await
        .map_err(Denied::Storage)?
    {
        EmailLookup::Found(a) if a.user_id == user_id => a,
        _ => return Ok(None),
    };
    if account.superuser || (account.privileged && !signup.allow_email_link_staff) {
        return Err(Refusal::Privileged.into());
    }
    if !account.active {
        return Err(Refusal::Inactive.into());
    }
    signup::relink(req.pool, req.key, req.identity.subject(), &invite, user_id).await?;
    Ok(Some(Admitted {
        user_id,
        created: false,
    }))
}

/// No account has `email`: sign up under the surface's mode.
#[allow(unused_variables)]
async fn sign_up(
    req: &AdmitRequest<'_>,
    signup: &SignupPolicy,
    email: &EmailAddr,
) -> Result<Admitted, Denied> {
    let mode = req.policy.signup_mode(req.surface, signup);
    #[cfg(feature = "tenancy")]
    {
        use signup::InviteKind;
        let kind = match mode {
            SignupMode::InviteOnly => InviteKind::Email,
            SignupMode::Allowlist | SignupMode::Open => InviteKind::EmailOrDomain,
            _ => return Err(Refusal::SignupClosed.into()),
        };
        let invite = signup::find_invite(req.pool, req.provider, email, kind)
            .await
            .map_err(Denied::Storage)?;
        let invite = match (invite, mode) {
            (Ok(i), _) => Some(i),
            // Open needs no invite; one only adds its roles.
            (Err(_), SignupMode::Open) => None,
            (Err(r), _) => return Err(r.into()),
        };
        let made = signup::create_user(
            req.pool,
            req.key,
            req.identity,
            email,
            signup,
            invite.as_ref(),
        )
        .await?;
        signup::audit_unknown_roles(req.pool, req.provider, made.user_id, &made.unknown_roles)
            .await;
        Ok(Admitted {
            user_id: made.user_id,
            created: true,
        })
    }
    #[cfg(not(feature = "tenancy"))]
    Err(Refusal::SignupClosed.into())
}

/// [`link::sign_in`]: [`admit`] with no sign-in rules, as a [`LinkRefusal`].
pub(crate) async fn legacy_sign_in(
    pool: &Pool,
    key: &ProviderKey,
    allow_email_link: bool,
    profile: &crate::oauth2::NormalizedUser,
    accounts: &impl AccountLookup,
) -> Result<i64, LinkRefusal> {
    let identity = Identity::from_profile(profile.clone());
    let policy = EffectivePolicy::legacy(allow_email_link);
    let surface = match key.source() {
        link::LinkSource::Admin => Surface::BareAdmin,
        _ => Surface::Member,
    };
    match admit(
        AdmitRequest::new(pool, surface, key, &policy, &identity),
        accounts,
    )
    .await
    {
        Ok(a) => Ok(a.user_id),
        Err(Denied::Storage(e)) => Err(LinkRefusal::Storage(e)),
        Err(Denied::Refused(r)) => Err(r.into_link_refusal(identity.email())),
    }
}

impl Refusal {
    /// The legacy [`LinkRefusal`] for this refusal. `email` is the lowercased verified email.
    pub(crate) fn into_link_refusal(self, email: Option<&EmailAddr>) -> LinkRefusal {
        match self {
            Self::NoSubject => LinkRefusal::NoSubject,
            Self::EmailUnverified => LinkRefusal::Unverified,
            Self::SignupClosed => {
                LinkRefusal::NoAccount(email.map(|e| e.as_str().to_owned()).unwrap_or_default())
            }
            Self::EmailLinkDisabled => LinkRefusal::EmailLinkDisabled,
            Self::Privileged => LinkRefusal::Privileged,
            Self::EmailCollides => LinkRefusal::EmailCollides,
            Self::Inactive => LinkRefusal::Inactive,
            other => LinkRefusal::Policy(other.rule()),
        }
    }
}

/// The list columns of a sign-in policy and whether each holds domains.
pub(crate) const LIST_COLUMNS: &[(&str, bool)] = &[
    ("allowed_email_domains", true),
    ("blocked_email_domains", true),
    ("allowed_idp_tenants", false),
    ("allowed_hosted_domains", true),
];

/// The canonical spelling of a list column's value; blank is `None`.
///
/// # Errors
/// The entry that does not parse, naming the column.
pub(crate) fn normalise_list(column: &str, raw: &str) -> Result<Option<String>, String> {
    let domains = LIST_COLUMNS
        .iter()
        .find(|(c, _)| *c == column)
        .is_some_and(|(_, d)| *d);
    let out = if domains {
        DomainList::parse_opt(Some(raw)).map(|l| l.map(|l| l.to_string()))
    } else {
        TokenList::parse_opt(Some(raw)).map(|l| l.map(|l| l.to_string()))
    };
    out.map_err(|e| format!("{}: {e}", column.replace('_', " ")))
}

/// Admin hook: rewrite the submitted policy lists in canonical form and
/// refuse a value that does not parse, so a saved row always parses.
#[cfg(feature = "admin")]
pub(crate) fn admin_normalise<'a>(
    values: &'a mut Vec<(&'static str, crate::core::SqlValue)>,
    _before: Option<&'a serde_json::Value>,
) -> crate::admin::derived_fields::DeriveFuture<'a> {
    use crate::core::SqlValue;
    let out = (|| {
        for (col, v) in values.iter_mut() {
            if LIST_COLUMNS.iter().any(|(c, _)| c == col) {
                *v = match v {
                    SqlValue::Null => SqlValue::Null,
                    SqlValue::String(raw) => match normalise_list(col, raw)? {
                        Some(s) => SqlValue::String(s),
                        None => SqlValue::Null,
                    },
                    _ => return Err(format!("{}: expected text", col.replace('_', " "))),
                };
            } else if *col == "default_roles" || *col == "role_claims" {
                *v = match v {
                    SqlValue::Null => SqlValue::Null,
                    SqlValue::String(raw) if *col == "default_roles" => {
                        let r: RoleNames =
                            raw.parse().map_err(|e| format!("default roles: {e}"))?;
                        if r.is_empty() {
                            SqlValue::Null
                        } else {
                            SqlValue::String(r.to_string())
                        }
                    }
                    SqlValue::String(raw) => {
                        RoleMap::parse(None, Some(raw)).map_err(|e| format!("role claims: {e}"))?;
                        if raw.trim().is_empty() {
                            SqlValue::Null
                        } else {
                            SqlValue::String(raw.trim().to_owned())
                        }
                    }
                    _ => return Err(format!("{}: expected text", col.replace('_', " "))),
                };
            } else if *col == "role_map" {
                let parsed = match v {
                    SqlValue::Json(j) => j.clone(),
                    SqlValue::Null => serde_json::json!({}),
                    SqlValue::String(s) if s.trim().is_empty() => serde_json::json!({}),
                    SqlValue::String(s) => {
                        serde_json::from_str(s).map_err(|e| format!("role map: {e}"))?
                    }
                    _ => return Err("role map: expected JSON".into()),
                };
                RoleMap::parse(Some(&parsed), None).map_err(|e| e.to_string())?;
                *v = SqlValue::Json(parsed);
            } else if *col == "required_claims" {
                let parsed = match v {
                    SqlValue::Json(j) => j.clone(),
                    SqlValue::Null => serde_json::json!({}),
                    SqlValue::String(s) if s.trim().is_empty() => serde_json::json!({}),
                    SqlValue::String(s) => {
                        serde_json::from_str(s).map_err(|e| format!("required claims: {e}"))?
                    }
                    _ => return Err("required claims: expected JSON".into()),
                };
                RequiredClaims::parse(&parsed).map_err(|e| e.to_string())?;
                *v = SqlValue::Json(parsed);
            }
        }
        Ok(())
    })();
    Box::pin(async move { out })
}

#[cfg(feature = "admin")]
inventory::submit! {
    crate::admin::derived_fields::AdminDerivedField {
        table: "rustango_sso_providers",
        derive: admin_normalise,
    }
}

#[cfg(feature = "admin")]
inventory::submit! {
    crate::admin::derived_fields::AdminDerivedField {
        table: "rustango_sso_shared_overrides",
        derive: admin_normalise,
    }
}

/// Where a refusal happened, for [`refuse`].
#[cfg(feature = "admin")]
pub(crate) struct RefusalContext<'a> {
    /// The database the sign-in ran against; its audit log gets the row.
    pub pool: &'a Pool,
    pub surface: Surface,
    /// The signal `source`.
    pub signal_source: &'static str,
    /// Provider slug.
    pub provider: &'a str,
    pub key: &'a ProviderKey,
    pub identity: &'a Identity,
    pub tenant: Option<String>,
    pub extensions: &'a axum::http::Extensions,
    pub headers: &'a axum::http::HeaderMap,
    pub path: &'a str,
}

/// Record a refusal: server log, an audit row naming the rule, the
/// `user_login_failed` signal, and a failure against the client IP.
#[cfg(feature = "admin")]
pub(crate) async fn refuse(ctx: &RefusalContext<'_>, refusal: &Refusal) {
    let subject = ctx.identity.subject();
    let email = ctx.identity.email().map(EmailAddr::as_str);
    tracing::warn!(
        target: "rustango::sso",
        surface = ctx.surface.as_str(),
        tenant = ctx.tenant.as_deref().unwrap_or(""),
        provider = ctx.provider,
        issuer = ctx.key.issuer(),
        subject,
        rule = refusal.rule(),
        "sso refused: {refusal}"
    );
    let entry = crate::audit::PendingEntry {
        entity_table: "rustango_sso_links",
        entity_pk: format!("{}:{subject}", ctx.provider)
            .chars()
            .take(255)
            .collect(),
        operation: crate::audit::AuditOp::Action,
        source: crate::audit::AuditSource::Custom(format!("sso:{}", ctx.surface.as_str())),
        changes: serde_json::json!({
            "event": "sso_refused",
            "rule": refusal.rule(),
            "detail": refusal.to_string(),
            "provider": ctx.provider,
            "subject": subject,
            "email": email,
        }),
    };
    if let Err(e) = crate::audit::emit_one_pool(ctx.pool, &entry).await {
        tracing::error!(target: "rustango::sso", error = %e, "an SSO refusal was NOT written to the audit log");
    }
    crate::sso::link::signal_refused(
        ctx.signal_source,
        &ctx.identity.profile,
        refusal.failure_reason(),
        crate::signals::auth::meta_from_parts(ctx.extensions, ctx.headers, Some(ctx.path)),
        ctx.tenant.clone(),
    )
    .await;
    use crate::login_throttle::{ClientIp, LoginScope};
    let scope = match (&ctx.surface, &ctx.tenant) {
        (Surface::BareAdmin, _) | (_, None) => LoginScope::Admin,
        (_, Some(t)) => LoginScope::Tenant(t.clone()),
    };
    let ip = ClientIp::from_parts(ctx.extensions, ctx.headers);
    if let Ok(attempt) = crate::login_throttle::shared()
        .begin(&scope, &ip, &format!("sso:{subject}"))
        .await
    {
        attempt.failed().await;
    }
}

#[cfg(test)]
mod tests {
    use super::identity::test_identity;
    use super::*;
    use crate::oauth2::NormalizedUser;

    fn person(email: &str, verified: bool, raw: serde_json::Value) -> Identity {
        test_identity(
            NormalizedUser {
                provider: "oidc".into(),
                provider_user_id: "sub-1".into(),
                email: Some(email.into()),
                email_verified: verified,
                name: None,
                avatar_url: None,
                raw,
            },
            None,
            false,
        )
    }

    fn policy(allowed: Option<&str>, blocked: Option<&str>) -> SignInPolicy {
        SignInPolicy::parse(&SignInColumns {
            allowed_email_domains: allowed,
            blocked_email_domains: blocked,
            require_verified_email: true,
            ..SignInColumns::default()
        })
        .unwrap()
    }

    #[test]
    fn blocked_beats_allowed_and_unverified_never_matches() {
        let p = policy(
            Some("client.org, *.client.org"),
            Some("contractors.client.org"),
        );
        let ok = |e: &str| p.check(&person(e, true, serde_json::json!({})));
        assert!(ok("ann@client.org").is_ok());
        assert!(ok("ann@eu.client.org").is_ok());
        assert_eq!(
            ok("bob@contractors.client.org"),
            Err(Refusal::DomainBlocked {
                rule: "contractors.client.org".into()
            })
        );
        assert_eq!(
            ok("eve@gmail.com"),
            Err(Refusal::DomainNotAllowed {
                domain: "gmail.com".into()
            })
        );
        assert_eq!(
            ok("eve@evilclient.org").unwrap_err().rule(),
            "domain_not_allowed"
        );
        assert_eq!(
            p.check(&person("ann@client.org", false, serde_json::json!({}))),
            Err(Refusal::EmailUnverified)
        );
        // A blocked domain refuses even an unverified email.
        let b = policy(None, Some("gmail.com"));
        assert!(b
            .check(&person("eve@gmail.com", false, serde_json::json!({})))
            .is_err());
    }

    #[test]
    fn the_default_policy_admits_anyone_with_a_subject() {
        let p = SignInPolicy::default();
        assert!(p
            .check(&person("x@y.z", false, serde_json::json!({})))
            .is_ok());
        let mut nobody = person("x@y.z", true, serde_json::json!({}));
        nobody.profile.provider_user_id.clear();
        assert_eq!(
            EffectivePolicy::legacy(true).check(&nobody),
            Err(Refusal::NoSubject)
        );
    }

    #[test]
    fn tenant_and_hosted_domain_pins() {
        let p = SignInPolicy::parse(&SignInColumns {
            allowed_idp_tenants: Some("T1"),
            allowed_hosted_domains: Some("client.org"),
            require_verified_email: true,
            ..SignInColumns::default()
        })
        .unwrap();
        let ms = |tid: Option<&str>, hd: &str| {
            let token = tid.map(|t| {
                serde_json::json!({"sub": "sub-1", "tid": t})
                    .as_object()
                    .unwrap()
                    .clone()
            });
            test_identity(
                NormalizedUser {
                    provider: "microsoft".into(),
                    provider_user_id: "sub-1".into(),
                    email: None,
                    email_verified: false,
                    name: None,
                    avatar_url: None,
                    raw: serde_json::json!({"hd": hd, "tid": "t1"}),
                },
                token,
                true,
            )
        };
        assert!(p.check(&ms(Some("t1"), "client.org")).is_ok());
        assert_eq!(
            p.check(&ms(Some("t2"), "client.org")),
            Err(Refusal::IdpTenant)
        );
        // userinfo's own `tid` is never trusted.
        assert_eq!(p.check(&ms(None, "client.org")), Err(Refusal::IdpTenant));
        assert_eq!(
            p.check(&ms(Some("t1"), "other.org")),
            Err(Refusal::HostedDomain)
        );
    }

    #[test]
    fn an_unparseable_layer_refuses() {
        let bad = SignInPolicy::parse(&SignInColumns {
            allowed_email_domains: Some("client.org, gmail"),
            ..SignInColumns::default()
        });
        assert!(bad.is_err());
        let p = EffectivePolicy::new(bad, true);
        assert_eq!(
            p.check(&person("a@client.org", true, serde_json::json!({}))),
            Err(Refusal::PolicyInvalid)
        );
    }

    /// Over a grid of layers and identities, adding a layer never admits
    /// anyone the shared layer alone refuses, nor links by a weaker email.
    #[test]
    fn a_second_layer_only_tightens() {
        let lists = [
            None,
            Some("client.org"),
            Some("*.client.org"),
            Some("gmail.com"),
        ];
        let tenants = [None, Some("t1"), Some("t2")];
        let mut layers = Vec::new();
        for allowed in lists {
            for blocked in lists {
                for tid in tenants {
                    for verified in [true, false] {
                        layers.push(SignInPolicy::parse(&SignInColumns {
                            allowed_email_domains: allowed,
                            blocked_email_domains: blocked,
                            allowed_idp_tenants: tid,
                            require_verified_email: verified,
                            ..SignInColumns::default()
                        }));
                    }
                }
            }
        }
        let mut people = Vec::new();
        for email in ["a@client.org", "a@eu.client.org", "a@gmail.com"] {
            for verified in [true, false] {
                for tid in [None, Some("t1"), Some("t2")] {
                    let token = tid.map(|t| {
                        serde_json::json!({"sub": "sub-1", "tid": t})
                            .as_object()
                            .unwrap()
                            .clone()
                    });
                    people.push(test_identity(
                        NormalizedUser {
                            provider: "microsoft".into(),
                            provider_user_id: "sub-1".into(),
                            email: Some(email.into()),
                            email_verified: verified,
                            name: None,
                            avatar_url: None,
                            raw: serde_json::json!({}),
                        },
                        token,
                        true,
                    ));
                }
            }
        }
        let mut checked = 0;
        for shared in &layers {
            let alone = EffectivePolicy::new(shared.clone(), true);
            for extra in &layers {
                let both = EffectivePolicy::new(shared.clone(), true).tighten(extra.clone());
                for id in &people {
                    if both.check(id).is_ok() {
                        assert!(
                            alone.check(id).is_ok(),
                            "{shared:?} + {extra:?} admitted {id:?}"
                        );
                    }
                    if both.link_email(id).is_some() {
                        assert!(alone.link_email(id).is_some());
                    }
                    checked += 1;
                }
            }
        }
        assert!(checked > 10_000);
    }

    #[cfg(feature = "admin")]
    #[tokio::test]
    async fn the_admin_hook_normalises_and_refuses() {
        use crate::core::SqlValue as V;
        let mut ok = vec![
            (
                "allowed_email_domains",
                V::String(" Client.org,\n*.client.org ".into()),
            ),
            ("blocked_email_domains", V::String("  ".into())),
            ("required_claims", V::Null),
        ];
        admin_normalise(&mut ok, None).await.unwrap();
        assert_eq!(ok[0].1, V::String("client.org, *.client.org".into()));
        assert_eq!(ok[1].1, V::Null);
        assert_eq!(ok[2].1, V::Json(serde_json::json!({})));
        for bad in [
            vec![("allowed_email_domains", V::I64(1))],
            vec![("allowed_idp_tenants", V::Bool(true))],
            vec![("allowed_email_domains", V::String("gmail".into()))],
            vec![("required_claims", V::I64(3))],
            vec![("required_claims", V::String("[1]".into()))],
        ] {
            let mut v = bad.clone();
            assert!(admin_normalise(&mut v, None).await.is_err(), "{bad:?}");
        }
    }

    #[test]
    fn rule_tokens_are_stable() {
        assert_eq!(Refusal::SignupClosed.rule(), "signup_closed");
        assert_eq!(Refusal::Claim { name: "x".into() }.rule(), "claim");
        assert!(SignupMode::Closed < SignupMode::Open);
        assert_eq!(
            "invite_only".parse::<SignupMode>().unwrap(),
            SignupMode::InviteOnly
        );
        assert!("sometimes".parse::<PasswordLogin>().is_err());
    }
}
