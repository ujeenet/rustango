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
pub mod tenant;

pub use domain::{DomainList, DomainRule, EmailAddr, PolicyError, TokenList};
pub use identity::{IdToken, Identity, RequiredClaims};
pub use models::{SsoInvite, SsoSharedOverride, SsoTenantPolicy};
pub use signup::{InviteTarget, RoleMap, RoleNames, SignupColumns, SignupPolicy};
pub use tenant::{SharedOverride, TenantSettings};

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
    /// The tenant's hourly sign-up limit is reached.
    SignupRate,
    /// A signed id_token carried another sign-in's `nonce`.
    IdTokenReplayed,
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
            Self::SignupRate => "signup_rate",
            Self::IdTokenReplayed => "id_token_replayed",
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
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct SignInPolicy {
    pub allowed_email_domains: Option<DomainList>,
    pub blocked_email_domains: Option<DomainList>,
    pub allowed_idp_tenants: Option<TokenList>,
    pub allowed_hosted_domains: Option<DomainList>,
    pub required_claims: RequiredClaims,
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
            // Only a verified email: an unverified one is whatever the
            // user typed at the IdP.
            let email = identity
                .verified_email(self.pins(identity))
                .ok_or(Refusal::EmailUnverified)?;
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
        if !identity.tid_pinned() && self.pins(identity) {
            return self.required_claims.check(&identity.clone().pinned(true));
        }
        self.required_claims.check(identity)
    }
}

/// A layer's password and session rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct Lifecycle {
    /// Users of this provider may not sign in with a password.
    pub sso_only: bool,
    pub auto_deactivate_after_days: Option<i32>,
    pub session_ttl_secs: Option<i64>,
    pub superuser_session_ttl_secs: Option<i64>,
}

fn min_opt(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, None) | (None, x) => x,
    }
}

impl Lifecycle {
    /// Stored values; a number must be positive.
    ///
    /// # Errors
    /// A zero or negative number, naming its column.
    pub fn parse(
        sso_only: bool,
        auto_deactivate_after_days: Option<i64>,
        session_ttl_secs: Option<i64>,
        superuser_session_ttl_secs: Option<i64>,
    ) -> Result<Self, PolicyError> {
        let positive = |col: &str, v: Option<i64>| match v {
            Some(n) if n <= 0 => Err(PolicyError(format!("{col}: must be positive"))),
            v => Ok(v),
        };
        let days = positive("auto_deactivate_after_days", auto_deactivate_after_days)?
            .map(|d| i32::try_from(d).unwrap_or(i32::MAX));
        Ok(Self {
            sso_only,
            auto_deactivate_after_days: days,
            session_ttl_secs: positive("session_ttl_secs", session_ttl_secs)?,
            superuser_session_ttl_secs: positive(
                "superuser_session_ttl_secs",
                superuser_session_ttl_secs,
            )?,
        })
    }

    /// Both layers' rules: `sso_only` if either, the shorter lifetimes.
    #[must_use]
    pub fn tighten(self, other: Self) -> Self {
        Self {
            sso_only: self.sso_only || other.sso_only,
            auto_deactivate_after_days: self.auto_deactivate_after_days,
            session_ttl_secs: min_opt(self.session_ttl_secs, other.session_ttl_secs),
            superuser_session_ttl_secs: min_opt(
                self.superuser_session_ttl_secs,
                other.superuser_session_ttl_secs,
            ),
        }
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
    /// A tenant override's sign-up mode: the effective one is never looser.
    signup_cap: Option<SignupMode>,
    lifecycle: Result<Lifecycle, PolicyError>,
    /// Roles this provider may grant here; `None`: any.
    roles_allowed: Option<RoleNames>,
    /// Sign-ups per hour; `None`: no limit.
    signups_per_hour: Option<u32>,
    /// A tenant override turned off email-linking role holders.
    staff_link_off: bool,
}

impl EffectivePolicy {
    /// A provider's own policy.
    #[must_use]
    pub fn new(provider: Result<SignInPolicy, PolicyError>, allow_email_link: bool) -> Self {
        Self {
            layers: vec![provider],
            ..Self::legacy(allow_email_link)
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
            signup_cap: None,
            lifecycle: Ok(Lifecycle::default()),
            roles_allowed: None,
            signups_per_hour: None,
            staff_link_off: false,
        }
    }

    /// The provider's sign-up settings; an error refuses every sign-in.
    #[must_use]
    pub fn with_signup(mut self, signup: Result<SignupPolicy, PolicyError>) -> Self {
        self.signup = signup;
        self.restrict_roles();
        self
    }

    /// The provider's password and session rules; an error refuses every sign-in.
    #[must_use]
    pub fn with_lifecycle(mut self, lifecycle: Result<Lifecycle, PolicyError>) -> Self {
        self.lifecycle = lifecycle;
        self
    }

    /// A tenant's override of a shared provider, `None` when it has none.
    /// It only tightens; without one the shared provider grants no roles.
    #[must_use]
    pub fn shared_override(mut self, ov: Result<Option<SharedOverride>, PolicyError>) -> Self {
        let ov = match ov {
            Ok(Some(ov)) => ov,
            Ok(None) => {
                self.roles_allowed = Some(RoleNames::default());
                self.restrict_roles();
                return self;
            }
            Err(e) => {
                self.roles_allowed = Some(RoleNames::default());
                self.restrict_roles();
                return self.tighten(Err(e));
            }
        };
        let layer = ov.sign_in;
        self.signup_cap = match (self.signup_cap, ov.signup_mode) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        self.lifecycle = self.lifecycle.map(|l| l.tighten(ov.lifecycle));
        self.allow_email_link &= !ov.email_link_off;
        self.staff_link_off |= ov.email_link_staff_off;
        self.roles_allowed = Some(match self.roles_allowed.take() {
            Some(prev) => ov.allowed_roles.only(&prev),
            None => ov.allowed_roles,
        });
        self.restrict_roles();
        self.tighten(Ok(layer))
    }

    /// The tenant-wide settings: the sign-up rate limit.
    #[must_use]
    pub fn tenant(mut self, settings: &TenantSettings) -> Self {
        self.signups_per_hour = settings.signup_limit();
        self
    }

    fn restrict_roles(&mut self) {
        if let (Some(allowed), Ok(signup)) = (&self.roles_allowed, &mut self.signup) {
            signup.restrict_roles(allowed);
        }
        if let (true, Ok(signup)) = (self.staff_link_off, &mut self.signup) {
            signup.allow_email_link_staff = false;
        }
    }

    /// A session's lifetime: the shortest of `base` and every layer's, and
    /// for a superuser also of the superuser lifetime.
    #[must_use]
    pub fn session_ttl(&self, base: i64, superuser: bool) -> i64 {
        let Ok(l) = &self.lifecycle else {
            return base;
        };
        let mut ttl = min_opt(Some(base), l.session_ttl_secs);
        if superuser {
            ttl = min_opt(ttl, l.superuser_session_ttl_secs);
        }
        ttl.unwrap_or(base)
    }

    /// The effective sign-up mode before the surface default applies.
    #[must_use]
    pub fn signup_cap(&self) -> Option<SignupMode> {
        self.signup_cap
    }

    /// The sign-up settings in force; `None` when they do not parse.
    #[must_use]
    pub fn signup(&self) -> Option<&SignupPolicy> {
        self.signup.as_ref().ok()
    }

    /// The password and session rules in force; `None` when they do not parse.
    #[must_use]
    pub fn lifecycle(&self) -> Option<&Lifecycle> {
        self.lifecycle.as_ref().ok()
    }

    /// The sign-up mode when the provider sets none (default closed).
    #[must_use]
    pub fn default_signup(mut self, mode: SignupMode) -> Self {
        self.default_signup = mode;
        self
    }

    /// Who may sign up on `surface`. The bare admin never signs anyone up.
    fn signup_mode(&self, surface: Surface, signup: &SignupPolicy) -> SignupMode {
        let mode = match surface {
            Surface::BareAdmin => SignupMode::Closed,
            _ => signup.mode.unwrap_or(self.default_signup),
        };
        self.signup_cap.map_or(mode, |cap| mode.min(cap))
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
        if identity.replayed() {
            return Err(Refusal::IdTokenReplayed);
        }
        if let Err(e) = &self.lifecycle {
            tracing::error!(target: "rustango::sso", "SSO session rules do not parse: {e}");
            return Err(Refusal::PolicyInvalid);
        }
        let identity = &self.scope(identity);
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

    /// `identity` with its directory's claims trusted when a layer pins its
    /// `tid`; every layer must pass, so one pin bounds the directory.
    pub(crate) fn scope(&self, identity: &Identity) -> Identity {
        let pinned = self
            .layers
            .iter()
            .any(|l| l.as_ref().is_ok_and(|p| p.pins(identity)));
        identity.clone().pinned(pinned)
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
    // Role sync and sign-up read the directory's claims only under a `tid` pin.
    let scoped = policy.scope(identity);
    let req = AdmitRequest {
        identity: &scoped,
        ..req
    };
    let identity = &scoped;
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
            after_sign_in(&req, signup, admitted.user_id, signup.sync_roles_on_login).await?;
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
    let revoked = match link_by_email(pool, key, subject, found.user_id, tenant_users).await {
        Ok(r) => r,
        Err(e) => {
            // A concurrent first login may have linked it already.
            return match link::linked_user(pool, key, subject)
                .await
                .map_err(Denied::Storage)?
            {
                Some(l) if l.user_id == found.user_id => Ok(Admitted {
                    user_id: found.user_id,
                    created: false,
                }),
                _ => Err(Denied::Storage(e)),
            };
        }
    };
    let locked = serde_json::json!({"password_locked": tenant_users, "revoked": revoked});
    audit_success(&req, "sso_linked", found.user_id, locked).await;
    if tenant_users {
        after_sign_in(&req, signup, found.user_id, signup.sync_roles_on_login).await?;
    }
    if let Ok(Some(l)) = link::linked_user(pool, key, subject).await {
        link::touch(pool, &l).await;
    }
    Ok(Admitted {
        user_id: found.user_id,
        created: false,
    })
}

/// Link `subject` to an account its verified email matched. On the tenant
/// user table the account is locked in the same transaction; returns what
/// was revoked (`null` elsewhere).
#[allow(unused_variables)]
async fn link_by_email(
    pool: &Pool,
    key: &ProviderKey,
    subject: &str,
    user_id: i64,
    tenant_users: bool,
) -> Result<serde_json::Value, String> {
    #[cfg(feature = "tenancy")]
    if tenant_users {
        return signup::link_by_email(pool, key, subject, user_id)
            .await
            .map(|r| serde_json::json!(r))
            .map_err(|e| e.to_string());
    }
    link::create_link(pool, key, subject, user_id)
        .await
        .map(|()| serde_json::Value::Null)
        .map_err(|e| e.to_string())
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
            let sync = signup::sync_roles(req.pool, user_id, req.identity, signup).await?;
            signup::audit_unknown_roles(req.pool, req.provider, user_id, &sync.unknown).await;
            if !sync.added.is_empty() || !sync.removed.is_empty() {
                let diff = serde_json::json!({"added": sync.added, "removed": sync.removed});
                audit_success(req, "sso_roles_synced", user_id, diff).await;
            }
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
    let revoked =
        signup::relink(req.pool, req.key, req.identity.subject(), &invite, user_id).await?;
    let used =
        serde_json::json!({"invite": invite.id(), "password_locked": true, "revoked": revoked});
    audit_success(req, "sso_relinked", user_id, used).await;
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
        if let Some(limit) = req.policy.signups_per_hour {
            if signup::signups_last_hour(req.pool).await? >= i64::from(limit) {
                return Err(Refusal::SignupRate.into());
            }
        }
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
        let used = serde_json::json!({"invite": invite.as_ref().map(signup::Invite::id)});
        audit_success(req, "sso_signup", made.user_id, used).await;
        Ok(Admitted {
            user_id: made.user_id,
            created: true,
        })
    }
    #[cfg(not(feature = "tenancy"))]
    Err(Refusal::SignupClosed.into())
}

/// Record an SSO account change in the audit log, in the shape of a refusal row.
async fn audit_success(
    req: &AdmitRequest<'_>,
    event: &'static str,
    user_id: i64,
    extra: serde_json::Value,
) {
    let subject = req.identity.subject();
    let mut changes = serde_json::json!({
        "event": event,
        "provider": req.provider,
        "subject": subject,
        "email": req.identity.email().map(EmailAddr::as_str),
        "user_id": user_id,
    });
    if let (Some(out), serde_json::Value::Object(more)) = (changes.as_object_mut(), extra) {
        out.extend(more);
    }
    let entry = crate::audit::PendingEntry {
        entity_table: "rustango_sso_links",
        entity_pk: format!("{}:{subject}", req.provider)
            .chars()
            .take(255)
            .collect(),
        operation: crate::audit::AuditOp::Action,
        source: crate::audit::AuditSource::Custom(format!("sso:{}", req.surface.as_str())),
        changes,
    };
    if let Err(e) = crate::audit::emit_one_pool(req.pool, &entry).await {
        tracing::error!(target: "rustango::sso", error = %e, event, "an SSO account change was NOT written to the audit log");
    }
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
            } else if matches!(*col, "default_roles" | "allowed_roles" | "role_claims") {
                *v = match v {
                    SqlValue::Null => SqlValue::Null,
                    SqlValue::String(raw) if *col != "role_claims" => {
                        let r: RoleNames = raw
                            .parse()
                            .map_err(|e| format!("{}: {e}", col.replace('_', " ")))?;
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

/// The slug, kind and issuer of a provider row, as the operator console
/// checks them. `dev`: a plain `http://` issuer is allowed.
#[cfg(feature = "admin")]
pub(crate) fn check_provider_fields(
    values: &[(&'static str, crate::core::SqlValue)],
    before: Option<&serde_json::Value>,
    dev: bool,
) -> Result<(), String> {
    use crate::core::SqlValue;
    let field = |col: &str| match values.iter().find(|(c, _)| *c == col) {
        Some((_, SqlValue::String(s))) => Some(s.trim().to_owned()).filter(|s| !s.is_empty()),
        Some(_) => None,
        None => before?.get(col)?.as_str().map(str::to_owned),
    };
    let slug = field("slug").unwrap_or_default();
    if slug.len() > 64 || crate::validators::validate_slug(&slug).is_err() {
        return Err("slug must be 1-64 letters, digits, `_` or `-`".into());
    }
    let issuer = field("issuer_url");
    super::check_provider(&field("kind").unwrap_or_default(), issuer.as_deref())
        .map_err(|e| e.to_string())?;
    if !dev && issuer.as_deref().is_some_and(super::insecure_url) {
        return Err("issuer URL must use https".into());
    }
    Ok(())
}

/// Admin hook: [`check_provider_fields`] on a tenant's own provider row.
#[cfg(feature = "admin")]
fn admin_check_provider<'a>(
    values: &'a mut Vec<(&'static str, crate::core::SqlValue)>,
    before: Option<&'a serde_json::Value>,
) -> crate::admin::derived_fields::DeriveFuture<'a> {
    let dev = matches!(crate::env::tier_from_env().as_str(), "dev" | "test");
    let out = check_provider_fields(values, before, dev);
    Box::pin(async move { out })
}

#[cfg(feature = "admin")]
inventory::submit! {
    crate::admin::derived_fields::AdminDerivedField {
        table: "rustango_sso_providers",
        derive: admin_check_provider,
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
    if recent_audit_rows(ctx.pool, entry.entity_table, &entry.entity_pk).await < MAX_REFUSAL_ROWS {
        if let Err(e) = crate::audit::emit_one_pool(ctx.pool, &entry).await {
            tracing::error!(target: "rustango::sso", error = %e, "an SSO refusal was NOT written to the audit log");
        }
    } else {
        tracing::debug!(target: "rustango::sso", subject, "sso refusal not audited again: too many recently");
    }
    crate::sso::link::signal_refused(
        ctx.signal_source,
        &ctx.identity.profile,
        refusal.failure_reason(),
        crate::signals::auth::meta_from_parts(ctx.extensions, ctx.headers, Some(ctx.path)),
        ctx.tenant.clone(),
    )
    .await;
}

/// Repeated SSO audit rows one subject gets per [`REFUSAL_WINDOW`].
pub(crate) const MAX_REFUSAL_ROWS: i64 = 5;
const REFUSAL_WINDOW: chrono::TimeDelta = chrono::TimeDelta::minutes(10);

/// Audit rows for `table`/`entity_pk` in the last [`REFUSAL_WINDOW`]; 0 when unreadable.
pub(crate) async fn recent_audit_rows(pool: &Pool, table: &'static str, entity_pk: &str) -> i64 {
    use crate::sql::CounterPool as _;
    crate::audit::AuditLog::objects()
        .filter("entity_table", table)
        .filter("entity_pk", entity_pk.to_owned())
        .filter("occurred_at__gte", chrono::Utc::now() - REFUSAL_WINDOW)
        .count(pool)
        .await
        .unwrap_or(0)
}

/// The SSO throttle scope of a tenant (or the bare admin, `None`).
#[cfg(feature = "admin")]
pub(crate) fn throttle_scope(tenant: Option<&str>) -> crate::login_throttle::LoginScope {
    crate::login_throttle::LoginScope::Sso(match tenant {
        Some(t) => format!("tenant/{t}"),
        None => "admin".to_owned(),
    })
}

/// Refuse an SSO start from an IP whose SSO failures are spent. Reserves nothing.
///
/// # Errors
/// The `429` to send.
#[cfg(feature = "admin")]
pub(crate) async fn throttle_check(
    scope: &crate::login_throttle::LoginScope,
    extensions: &axum::http::Extensions,
    headers: &axum::http::HeaderMap,
) -> Result<(), axum::response::Response> {
    let ip = crate::login_throttle::ClientIp::from_parts(extensions, headers);
    crate::login_throttle::shared()
        .check(scope, &ip, "")
        .await
        .map_err(crate::login_throttle::LoginRefused::into_response)
}

/// Admit one SSO callback. Report a refusal on the attempt with `failed`,
/// a sign-in with `succeeded`; dropped, it counts nothing.
///
/// # Errors
/// The `429` / `503` to send.
#[cfg(feature = "admin")]
pub(crate) async fn throttle_begin(
    scope: &crate::login_throttle::LoginScope,
    extensions: &axum::http::Extensions,
    headers: &axum::http::HeaderMap,
) -> Result<crate::login_throttle::LoginAttempt, axum::response::Response> {
    let ip = crate::login_throttle::ClientIp::from_parts(extensions, headers);
    crate::login_throttle::shared()
        .begin(scope, &ip, "")
        .await
        .map_err(crate::login_throttle::LoginRefused::into_response)
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

    /// Any directory signs for a multi-tenant Microsoft app: its roles and
    /// claims count only once a layer pins its `tid`.
    #[test]
    fn microsoft_claims_count_only_under_a_tid_pin() {
        let token = |tid: &str| {
            serde_json::json!({"sub": "sub-1", "tid": tid, "roles": ["Admin"], "groups": ["Admin"]})
                .as_object()
                .unwrap()
                .clone()
        };
        let ms = |tid: &str| {
            test_identity(
                NormalizedUser {
                    provider: "microsoft".into(),
                    provider_user_id: "sub-1".into(),
                    email: None,
                    email_verified: false,
                    name: None,
                    avatar_url: None,
                    raw: serde_json::json!({}),
                },
                Some(token(tid)),
                true,
            )
            .with_jwks()
        };
        let layer = |pin: Option<&str>| {
            SignInPolicy::parse(&SignInColumns {
                allowed_idp_tenants: pin,
                required_claims: Some(&serde_json::json!({"roles": ["Admin"]})),
                ..SignInColumns::default()
            })
        };
        let map = RoleMap::parse(Some(&serde_json::json!({"Admin": "Editor"})), None).unwrap();
        let unpinned = EffectivePolicy::new(layer(None), false);
        assert_eq!(
            unpinned.check(&ms("t1")),
            Err(Refusal::Claim {
                name: "roles".into()
            })
        );
        assert_eq!(map.wanted(&unpinned.scope(&ms("t1"))), None);
        let pinned = EffectivePolicy::new(layer(Some("t1")), false);
        assert!(pinned.check(&ms("t1")).is_ok());
        assert_eq!(
            map.wanted(&pinned.scope(&ms("t1"))),
            Some(["Editor".to_owned()].into())
        );
        assert_eq!(pinned.check(&ms("t2")), Err(Refusal::IdpTenant));
        assert_eq!(map.wanted(&pinned.scope(&ms("t2"))), None);
        // A pin on another layer bounds the directory too.
        let shared =
            EffectivePolicy::new(layer(None), false).tighten(SignInPolicy::parse(&SignInColumns {
                allowed_idp_tenants: Some("t1"),
                ..SignInColumns::default()
            }));
        assert!(shared.check(&ms("t1")).is_ok());
        // Not Microsoft: claims count as before.
        let plain = person("a@b.org", true, serde_json::json!({"roles": ["Admin"]}));
        assert!(unpinned.check(&plain).is_ok());
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
                    layers.push(SignInPolicy::parse(&SignInColumns {
                        allowed_email_domains: allowed,
                        blocked_email_domains: blocked,
                        allowed_idp_tenants: tid,
                        ..SignInColumns::default()
                    }));
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

    /// A shared provider with every rule set to something to loosen.
    fn shared_base() -> EffectivePolicy {
        let sign_in = SignInPolicy::parse(&SignInColumns {
            allowed_email_domains: Some("client.org"),
            ..SignInColumns::default()
        });
        let signup = SignupPolicy::parse(&SignupColumns {
            signup_mode: Some("invite_only"),
            default_roles: Some("Viewer, Editor"),
            ..SignupColumns::default()
        });
        EffectivePolicy::new(sign_in, true)
            .with_signup(signup)
            .with_lifecycle(Lifecycle::parse(true, Some(30), Some(3600), Some(600)))
    }

    fn people() -> Vec<Identity> {
        let mut out = Vec::new();
        for email in ["a@client.org", "a@other.org"] {
            for verified in [true, false] {
                out.push(person(email, verified, serde_json::json!({})));
            }
        }
        out
    }

    /// Over every override field: the loosest value admits nobody the
    /// shared provider refuses, and grants nothing it would not.
    #[test]
    fn an_override_never_loosens_any_field() {
        let roles = |p: &EffectivePolicy| p.signup().unwrap().default_roles.clone();
        let loosest = [
            SharedOverride::default(),
            SharedOverride {
                signup_mode: Some(SignupMode::Open),
                ..SharedOverride::default()
            },
            SharedOverride {
                lifecycle: Lifecycle::parse(false, None, Some(999_999), Some(999_999)).unwrap(),
                ..SharedOverride::default()
            },
            SharedOverride {
                allowed_roles: "Viewer, Editor, Admin".parse().unwrap(),
                ..SharedOverride::default()
            },
        ];
        let base = shared_base();
        for ov in loosest {
            let both = shared_base().shared_override(Ok(Some(ov.clone())));
            for id in people() {
                if both.check(&id).is_ok() {
                    assert!(base.check(&id).is_ok(), "{ov:?} admitted {id:?}");
                }
            }
            let sp = both.signup().unwrap();
            for surface in [Surface::TenantAdmin, Surface::Member] {
                assert!(
                    both.signup_mode(surface, sp)
                        <= base.signup_mode(surface, base.signup().unwrap()),
                    "{ov:?}"
                );
            }
            assert!(both.lifecycle().unwrap().sso_only, "{ov:?}");
            for su in [false, true] {
                assert!(both.session_ttl(86_400, su) <= base.session_ttl(86_400, su));
            }
            let granted = roles(&both);
            assert!(
                granted
                    .names()
                    .iter()
                    .all(|r| roles(&base).names().contains(r)),
                "{ov:?} granted {granted:?}"
            );
        }
    }

    /// Each override field does tighten.
    #[test]
    fn an_override_tightens_every_field() {
        let tight = SharedOverride {
            sign_in: SignInPolicy::parse(&SignInColumns {
                blocked_email_domains: Some("client.org"),
                ..SignInColumns::default()
            })
            .unwrap(),
            ..SharedOverride::default()
        };
        let p = shared_base().shared_override(Ok(Some(tight)));
        assert_eq!(
            p.check(&person("a@client.org", true, serde_json::json!({})))
                .unwrap_err()
                .rule(),
            "domain_blocked"
        );
        let closed = SharedOverride {
            signup_mode: Some(SignupMode::Closed),
            lifecycle: Lifecycle::parse(false, None, Some(60), Some(30)).unwrap(),
            allowed_roles: "Viewer".parse().unwrap(),
            ..SharedOverride::default()
        };
        let p = shared_base().shared_override(Ok(Some(closed)));
        let sp = p.signup().unwrap();
        assert_eq!(p.signup_mode(Surface::TenantAdmin, sp), SignupMode::Closed);
        assert_eq!(p.session_ttl(86_400, false), 60);
        assert_eq!(p.session_ttl(86_400, true), 30);
        assert_eq!(sp.default_roles.names(), ["Viewer"]);
        // Email linking: an override turns it off, never on.
        let staffy = || {
            shared_base().with_signup(SignupPolicy::parse(&SignupColumns {
                allow_email_link_staff: true,
                ..SignupColumns::default()
            }))
        };
        let off = SharedOverride {
            email_link_off: true,
            email_link_staff_off: true,
            ..SharedOverride::default()
        };
        let p = staffy().shared_override(Ok(Some(off)));
        assert!(!p.allow_email_link);
        assert!(!p.signup().unwrap().allow_email_link_staff);
        let kept = staffy().shared_override(Ok(Some(SharedOverride::default())));
        assert!(kept.allow_email_link);
        assert!(kept.signup().unwrap().allow_email_link_staff);
        let mut row_off = EffectivePolicy::new(Ok(SignInPolicy::default()), false);
        row_off = row_off.shared_override(Ok(Some(SharedOverride::default())));
        assert!(!row_off.allow_email_link, "an override never turns it on");
        // No override: the shared provider grants no role here.
        let none = shared_base().shared_override(Ok(None));
        assert!(none.signup().unwrap().default_roles.is_empty());
        // An unreadable override refuses.
        let bad = shared_base().shared_override(Err(PolicyError("x".into())));
        assert_eq!(
            bad.check(&person("a@client.org", true, serde_json::json!({}))),
            Err(Refusal::PolicyInvalid)
        );
    }

    #[test]
    fn session_ttl_is_the_shortest_and_bad_numbers_refuse() {
        let p = shared_base();
        assert_eq!(p.session_ttl(86_400, false), 3600);
        assert_eq!(p.session_ttl(86_400, true), 600);
        assert_eq!(p.session_ttl(100, true), 100);
        assert!(Lifecycle::parse(false, None, Some(0), None).is_err());
        let bad = EffectivePolicy::legacy(true).with_lifecycle(Lifecycle::parse(
            false,
            Some(-1),
            None,
            None,
        ));
        assert_eq!(
            bad.check(&person("a@b.org", true, serde_json::json!({}))),
            Err(Refusal::PolicyInvalid)
        );
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

    /// A tenant's own provider row gets the operator console's checks.
    #[cfg(feature = "admin")]
    #[test]
    fn provider_rows_need_a_slug_a_kind_and_an_https_issuer() {
        use crate::core::SqlValue as V;
        let row = |slug: &str, kind: &str, issuer: Option<&str>| {
            vec![
                ("slug", V::String(slug.into())),
                ("kind", V::String(kind.into())),
                (
                    "issuer_url",
                    issuer.map_or(V::Null, |i| V::String(i.into())),
                ),
            ]
        };
        let ok = |v: Vec<_>, dev| check_provider_fields(&v, None, dev).is_ok();
        assert!(ok(row("corp", "google", None), false));
        assert!(ok(row("corp", "oidc", Some("https://idp.example")), false));
        assert!(ok(row("corp", "oidc", Some("http://127.0.0.1:9")), false));
        assert!(ok(row("corp", "oidc", Some("http://idp.example")), true));
        for bad in [
            row("a b", "google", None),
            row("", "google", None),
            row("corp", "myspace", None),
            row("corp", "oidc", None),
            row("corp", "oidc", Some("http://idp.example")),
        ] {
            assert!(!ok(bad.clone(), false), "{bad:?}");
        }
        // An edit that leaves the slug out keeps the stored one.
        let before = serde_json::json!({"slug": "corp", "kind": "google"});
        assert!(
            check_provider_fields(&[("label", V::String("x".into()))], Some(&before), false)
                .is_ok()
        );
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
