//! Who may sign in through SSO. Every SSO surface (tenant console, member
//! flow, bare admin) passes through [`admit`]; a refusal names its rule
//! only in the server log and the audit log, never to the browser.
//!
//! A policy is a stack of layers (the provider, later a tenant override of
//! a shared provider). A sign-in passes only if every layer passes it on its
//! own, so a layer can only tighten.

pub mod domain;
pub mod identity;
pub mod jwks;
pub mod models;

pub use domain::{DomainList, DomainRule, EmailAddr, PolicyError, TokenList};
pub use identity::{IdToken, Identity, RequiredClaims};
pub use models::{SsoInvite, SsoSharedOverride, SsoTenantPolicy};

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
}

impl EffectivePolicy {
    /// A provider's own policy.
    #[must_use]
    pub fn new(provider: Result<SignInPolicy, PolicyError>, allow_email_link: bool) -> Self {
        Self {
            layers: vec![provider],
            allow_email_link,
        }
    }

    /// No sign-in rules: the behaviour before policies existed.
    #[must_use]
    pub fn legacy(allow_email_link: bool) -> Self {
        Self {
            layers: Vec::new(),
            allow_email_link,
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
        }
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

/// Decide one SSO sign-in: the policy's rules, then the link, then a link
/// by verified email. Stamps the link's `last_used_at`.
///
/// # Errors
/// [`Denied::Refused`] with the rule; [`Denied::Storage`] on a driver error.
pub async fn admit(
    req: AdmitRequest<'_>,
    accounts: &impl AccountLookup,
) -> Result<Admitted, Denied> {
    let AdmitRequest {
        pool,
        key,
        policy,
        identity,
        ..
    } = req;
    policy.check(identity)?;
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
    let found = match accounts
        .by_email(email.as_str())
        .await
        .map_err(Denied::Storage)?
    {
        EmailLookup::Found(a) => a,
        EmailLookup::Missing => return Err(Refusal::SignupClosed.into()),
        EmailLookup::Collides => return Err(Refusal::EmailCollides.into()),
    };
    if !policy.allow_email_link {
        return Err(Refusal::EmailLinkDisabled.into());
    }
    if found.privileged || found.superuser {
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
    if let Ok(Some(l)) = link::linked_user(pool, key, subject).await {
        link::touch(pool, &l).await;
    }
    Ok(Admitted {
        user_id: found.user_id,
        created: false,
    })
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
                if let SqlValue::String(raw) = v {
                    *v = match normalise_list(col, raw)? {
                        Some(s) => SqlValue::String(s),
                        None => SqlValue::Null,
                    };
                }
            } else if *col == "required_claims" {
                let parsed = match v {
                    SqlValue::Json(j) => j.clone(),
                    SqlValue::String(s) if s.trim().is_empty() => serde_json::json!({}),
                    SqlValue::String(s) => {
                        serde_json::from_str(s).map_err(|e| format!("required claims: {e}"))?
                    }
                    _ => continue,
                };
                RequiredClaims::parse(&parsed).map_err(|e| e.to_string())?;
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
