//! Sign-up through SSO: who may, under which username, with which roles.
//! A new user, its link, its roles and the invite it used commit in one
//! transaction, or none of them does.

use std::collections::BTreeSet;
use std::str::FromStr;

use serde_json::Value;

use super::domain::{DomainRule, EmailAddr, PolicyError};
use super::{Identity, SignupMode, UsernameClash, UsernameFrom};

impl Default for UsernameFrom {
    fn default() -> Self {
        Self::EmailLocal
    }
}

impl Default for UsernameClash {
    fn default() -> Self {
        Self::Suffix
    }
}

/// Longest role name (`Role::name`).
const MAX_ROLE_NAME: usize = 150;

/// Tenant role names, comma or newline separated. Case kept.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RoleNames(Vec<String>);

impl RoleNames {
    /// A stored column: blank or NULL is none.
    ///
    /// # Errors
    /// A name longer than a role name can be.
    pub fn parse_opt(s: Option<&str>) -> Result<Self, PolicyError> {
        s.map_or(Ok(Self::default()), str::parse)
    }

    #[must_use]
    pub fn names(&self) -> &[String] {
        &self.0
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The names `allowed` also has.
    #[must_use]
    pub fn only(&self, allowed: &Self) -> Self {
        Self(
            self.0
                .iter()
                .filter(|n| allowed.0.contains(n))
                .cloned()
                .collect(),
        )
    }
}

impl FromStr for RoleNames {
    type Err = PolicyError;

    fn from_str(s: &str) -> Result<Self, PolicyError> {
        let mut out: Vec<String> = Vec::new();
        for name in s.split([',', '\n', '\r']).map(str::trim) {
            if name.is_empty() {
                continue;
            }
            if name.chars().count() > MAX_ROLE_NAME {
                return Err(PolicyError(format!("role `{name}` is too long")));
            }
            if !out.iter().any(|n| n == name) {
                out.push(name.to_owned());
            }
        }
        Ok(Self(out))
    }
}

impl std::fmt::Display for RoleNames {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0.join(", "))
    }
}

/// IdP claim values (groups, app roles) to tenant roles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleMap {
    /// The claims read; default `groups`, `roles`.
    claims: Vec<String>,
    map: Vec<(String, RoleNames)>,
}

impl Default for RoleMap {
    fn default() -> Self {
        Self {
            claims: vec!["groups".into(), "roles".into()],
            map: Vec::new(),
        }
    }
}

impl RoleMap {
    /// `map` is `{"<claim value>": "Role" | ["Role", ...]}`; `claims` the
    /// claim names to read (blank: `groups,roles`).
    ///
    /// # Errors
    /// Not an object, a value that is not a role name or a list of them, or
    /// a claim name with characters other than letters, digits, `_ - . :`.
    pub fn parse(map: Option<&Value>, claims: Option<&str>) -> Result<Self, PolicyError> {
        let mut out = Self::default();
        if let Some(c) = claims.map(str::trim).filter(|c| !c.is_empty()) {
            out.claims.clear();
            for name in c.split([',', '\n', '\r']).map(str::trim) {
                if name.is_empty() {
                    continue;
                }
                if !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'))
                {
                    return Err(PolicyError(format!(
                        "role claim `{name}` is not a claim name"
                    )));
                }
                if !out.claims.iter().any(|n| n == name) {
                    out.claims.push(name.to_owned());
                }
            }
        }
        let obj = match map {
            None | Some(Value::Null) => return Ok(out),
            Some(Value::Object(o)) => o,
            Some(_) => return Err(PolicyError("role_map must be a JSON object".into())),
        };
        for (value, roles) in obj {
            let roles: RoleNames = match roles {
                Value::String(s) => s.parse()?,
                Value::Array(list) => list
                    .iter()
                    .map(|v| v.as_str())
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(|| PolicyError(format!("role_map.{value}: role names only")))?
                    .join(",")
                    .parse()?,
                _ => {
                    return Err(PolicyError(format!(
                        "role_map.{value}: a role name or a list of them"
                    )))
                }
            };
            if value.is_empty() || roles.is_empty() {
                return Err(PolicyError("role_map: an empty entry".into()));
            }
            out.map.push((value.clone(), roles));
        }
        Ok(out)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Drop every role `allowed` lacks, and the entries left with none.
    pub fn restrict(&mut self, allowed: &RoleNames) {
        for (_, roles) in &mut self.map {
            *roles = roles.only(allowed);
        }
        self.map.retain(|(_, roles)| !roles.is_empty());
    }

    /// Every role the map can grant: the only roles sync may add or remove.
    #[must_use]
    pub fn managed(&self) -> BTreeSet<&str> {
        self.map
            .iter()
            .flat_map(|(_, r)| r.names().iter().map(String::as_str))
            .collect()
    }

    /// The roles `identity`'s claims map to; `None` when `identity` carries none of the role claims (or, for a
    /// provider that signs id_tokens, no verified one): nothing to sync from.
    #[must_use]
    pub fn wanted(&self, identity: &Identity) -> Option<BTreeSet<String>> {
        let mut values: Vec<&str> = Vec::new();
        let mut seen = false;
        for claim in &self.claims {
            match identity.role_claim(claim) {
                Some(Value::String(s)) => values.push(s),
                Some(Value::Array(list)) => values.extend(list.iter().filter_map(Value::as_str)),
                Some(_) => {}
                None => continue,
            }
            seen = true;
        }
        seen.then(|| {
            self.map
                .iter()
                .filter(|(v, _)| values.contains(&v.as_str()))
                .flat_map(|(_, r)| r.names().iter().cloned())
                .collect()
        })
    }
}

/// One layer's sign-up settings. The default is the behaviour before them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct SignupPolicy {
    /// `None`: the sign-in surface decides.
    pub mode: Option<SignupMode>,
    pub default_roles: RoleNames,
    pub role_map: RoleMap,
    pub sync_roles_on_login: bool,
    pub username_from: UsernameFrom,
    pub username_clash: UsernameClash,
    pub sync_profile: bool,
    /// Email linking may also link role holders, never superusers.
    pub allow_email_link_staff: bool,
}

impl SignupPolicy {
    /// Grant only roles in `allowed`.
    pub(crate) fn restrict_roles(&mut self, allowed: &RoleNames) {
        self.default_roles = self.default_roles.only(allowed);
        self.role_map.restrict(allowed);
    }
}

/// A provider row's stored sign-up columns, unparsed.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct SignupColumns<'a> {
    pub signup_mode: Option<&'a str>,
    pub default_roles: Option<&'a str>,
    pub role_map: Option<&'a Value>,
    pub role_claims: Option<&'a str>,
    pub sync_roles_on_login: bool,
    pub username_from: Option<&'a str>,
    pub username_clash: Option<&'a str>,
    pub sync_profile: bool,
    pub allow_email_link_staff: bool,
}

impl SignupPolicy {
    /// Parse stored columns.
    ///
    /// # Errors
    /// The first value that does not parse, naming its column.
    pub fn parse(c: &SignupColumns<'_>) -> Result<Self, PolicyError> {
        fn named(col: &'static str) -> impl Fn(PolicyError) -> PolicyError {
            move |e| PolicyError(format!("{col}: {e}"))
        }
        let mode = match c.signup_mode.map(str::trim).filter(|s| !s.is_empty()) {
            None => None,
            Some(s) => Some(s.parse().map_err(named("signup_mode"))?),
        };
        Ok(Self {
            mode,
            default_roles: RoleNames::parse_opt(c.default_roles).map_err(named("default_roles"))?,
            role_map: RoleMap::parse(c.role_map, c.role_claims).map_err(named("role_map"))?,
            sync_roles_on_login: c.sync_roles_on_login,
            username_from: c
                .username_from
                .map(str::parse)
                .transpose()
                .map_err(named("username_from"))?
                .unwrap_or_default(),
            username_clash: c
                .username_clash
                .map(str::parse)
                .transpose()
                .map_err(named("username_clash"))?
                .unwrap_or_default(),
            sync_profile: c.sync_profile,
            allow_email_link_staff: c.allow_email_link_staff,
        })
    }
}

/// Longest IdP display name stored.
const MAX_DISPLAY_NAME: usize = 150;

/// The IdP's name, trimmed and cut to [`MAX_DISPLAY_NAME`]; `None` when blank.
#[cfg_attr(not(feature = "tenancy"), allow(dead_code))]
fn display_name(raw: &str) -> Option<String> {
    let name: String = raw.trim().chars().take(MAX_DISPLAY_NAME).collect();
    let name = name.trim_end().to_owned();
    (!name.is_empty()).then_some(name)
}

/// Who an invite is for: one email, or a whole domain.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum InviteTarget {
    Email(EmailAddr),
    Domain(DomainRule),
}

impl InviteTarget {
    #[must_use]
    pub fn matches(&self, email: &EmailAddr) -> bool {
        match self {
            Self::Email(e) => e == email,
            Self::Domain(d) => d.matches(email.domain()),
        }
    }

    /// The stored targets that can cover `email`, for one indexed lookup.
    #[must_use]
    pub fn candidates(email: &EmailAddr) -> Vec<String> {
        let domain = email.domain();
        let mut out = vec![email.as_str().to_owned(), format!("@{domain}")];
        let mut rest = domain;
        while let Some((_, parent)) = rest.split_once('.') {
            if parent.contains('.') {
                out.push(format!("*.{parent}"));
            }
            rest = parent;
        }
        out
    }
}

/// Public suffixes and free mail domains a domain invite would open to anyone.
/// A short built-in list of common ones: NOT the full public suffix list, so
/// an invite for an unlisted shared domain still goes through.
const OPEN_DOMAINS: &[&str] = &[
    // Second-level public suffixes.
    "edu.au",
    "gov.au",
    "co.th",
    "com.vn",
    "com.eg",
    "com.ng",
    "co.ke",
    "ac.jp",
    // Hosting suffixes anyone can register under.
    "blogspot.com",
    "onmicrosoft.com",
    "workers.dev",
    "ngrok.io",
    "s3.amazonaws.com",
    "myshopify.com",
    "wordpress.com",
    // Free mail.
    "yahoo.co.uk",
    "yahoo.fr",
    "hotmail.co.uk",
    "hotmail.fr",
    "outlook.de",
    "live.co.uk",
    "gmx.at",
    "co.uk",
    "org.uk",
    "me.uk",
    "ltd.uk",
    "plc.uk",
    "ac.uk",
    "gov.uk",
    "com.au",
    "net.au",
    "org.au",
    "co.nz",
    "org.nz",
    "co.jp",
    "ne.jp",
    "or.jp",
    "co.kr",
    "com.br",
    "com.cn",
    "com.hk",
    "com.sg",
    "com.tw",
    "com.mx",
    "com.ar",
    "com.tr",
    "com.ua",
    "co.in",
    "co.za",
    "co.il",
    "com.pl",
    "co.id",
    "com.my",
    "com.ph",
    // Hosting suffixes anyone can register under.
    "github.io",
    "gitlab.io",
    "herokuapp.com",
    "vercel.app",
    "netlify.app",
    "pages.dev",
    "web.app",
    "firebaseapp.com",
    "azurewebsites.net",
    "cloudfront.net",
    "appspot.com",
    // Free mail.
    "gmail.com",
    "googlemail.com",
    "outlook.com",
    "hotmail.com",
    "live.com",
    "msn.com",
    "yahoo.com",
    "ymail.com",
    "icloud.com",
    "me.com",
    "mac.com",
    "aol.com",
    "proton.me",
    "protonmail.com",
    "pm.me",
    "gmx.com",
    "gmx.net",
    "gmx.de",
    "web.de",
    "mail.com",
    "mail.ru",
    "yandex.ru",
    "yandex.com",
    "qq.com",
    "163.com",
    "126.com",
    "zoho.com",
    "fastmail.com",
    "tutanota.com",
];

/// Whether a domain invite for `domain` (or its subdomains) admits the public.
fn open_domain(domain: &str) -> bool {
    OPEN_DOMAINS.contains(&domain)
}

impl FromStr for InviteTarget {
    type Err = PolicyError;

    /// `ann@client.org`; `@client.org` or `client.org` for the domain;
    /// `*.client.org` for its subdomains. A public suffix or free mail
    /// domain is refused: it would invite anyone.
    fn from_str(s: &str) -> Result<Self, PolicyError> {
        let s = s.trim();
        let domain = |d: &str| {
            let rule: DomainRule = d.parse()?;
            if open_domain(rule.domain()) {
                return Err(PolicyError(format!(
                    "`{s}` would invite anyone; invite emails instead"
                )));
            }
            Ok(Self::Domain(rule))
        };
        if let Some(d) = s.strip_prefix('@') {
            if d.starts_with("*.") {
                return Err(PolicyError(format!("`{s}`: write `{d}`")));
            }
            return domain(d);
        }
        if s.contains('@') {
            return s.parse().map(Self::Email);
        }
        domain(s)
    }
}

impl std::fmt::Display for InviteTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Email(e) => f.write_str(e.as_str()),
            Self::Domain(d) => {
                let d = d.to_string();
                if d.starts_with("*.") {
                    f.write_str(&d)
                } else {
                    write!(f, "@{d}")
                }
            }
        }
    }
}

#[cfg(feature = "tenancy")]
pub(crate) use store::*;

#[cfg(feature = "tenancy")]
mod store {
    use super::{EmailAddr, Identity, InviteTarget, RoleNames, SignupPolicy};
    use crate::core::Model as _;
    use crate::core::{SqlValue, F};
    use crate::sql::{ExecError, FetcherPool as _, FetcherTx as _, Pool, PoolTx};
    use crate::sso::link::{self, ProviderKey};
    use crate::sso::policy::SsoInvite;
    use crate::sso::policy::{Denied, Refusal, UsernameClash, UsernameFrom};
    use crate::tenancy::permissions::{Role, UserRole};
    use crate::tenancy::User;

    /// Longest username (`User::username`).
    const MAX_USERNAME: usize = 64;

    /// Which invites a sign-in may use.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum InviteKind<'a> {
        /// Sign-up by an invited email.
        Email,
        /// Sign-up by an invited email or domain.
        EmailOrDomain,
        /// Re-link an existing user, through the old link's provider only.
        Relink(&'a ProviderKey),
    }

    /// An invite a sign-in may use, as read.
    #[derive(Debug, Clone)]
    pub(crate) struct Invite {
        id: i64,
        max_uses: Option<i32>,
        pub(crate) roles: RoleNames,
        pub(crate) link_user_id: Option<i64>,
    }

    impl Invite {
        pub(crate) fn id(&self) -> i64 {
            self.id
        }
    }

    /// Sign-ups in this tenant over the last hour: links made with their user.
    pub(crate) async fn signups_last_hour(pool: &Pool) -> Result<i64, Denied> {
        use crate::sql::CounterPool as _;
        let since = chrono::Utc::now() - chrono::TimeDelta::hours(1);
        link::SsoLink::objects()
            .filter("provisioned", true)
            .filter("created_at__gte", since)
            .count(pool)
            .await
            .map_err(|e| Denied::Storage(format!("sign-up count: {e}")))
    }

    /// What a role sync changed.
    #[derive(Debug, Default)]
    pub(crate) struct RoleSync {
        pub(crate) added: Vec<String>,
        pub(crate) removed: Vec<String>,
        /// Wanted names no role row has.
        pub(crate) unknown: Vec<String>,
    }

    /// The invite `email` may use through provider `slug`. The refusal says
    /// why none: none at all, only expired ones, or only used-up ones.
    ///
    /// # Errors
    /// Driver failures; a table not yet migrated holds no invites.
    pub(crate) async fn find_invite(
        pool: &Pool,
        slug: &str,
        email: &EmailAddr,
        kind: InviteKind<'_>,
    ) -> Result<Result<Invite, Refusal>, String> {
        let rows = match SsoInvite::objects()
            .filter(
                "target__in",
                SqlValue::List(
                    InviteTarget::candidates(email)
                        .into_iter()
                        .map(SqlValue::String)
                        .collect(),
                ),
            )
            .fetch(pool)
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                let table = SsoInvite::SCHEMA.table;
                return match crate::migrate::try_table_exists_here(pool, table).await {
                    Ok(false) => {
                        tracing::warn!(target: "rustango::sso", "{table} is missing; run `migrate`");
                        Ok(Err(Refusal::NotInvited))
                    }
                    _ => Err(format!("invites: {e}")),
                };
            }
        };
        let now = chrono::Utc::now();
        let relink_key = match kind {
            InviteKind::Relink(key) => Some(key.fingerprint()),
            _ => None,
        };
        let mut fits: Vec<(bool, &SsoInvite)> = rows
            .iter()
            .filter(|r| match &relink_key {
                // A re-link invite is good only through the old link's provider.
                Some(key) => {
                    r.link_user_id.is_some() && r.link_key.as_deref() == Some(key.as_str())
                }
                None => {
                    r.link_user_id.is_none() && r.provider_slug.as_deref().is_none_or(|s| s == slug)
                }
            })
            .filter_map(|r| {
                let target: InviteTarget = r.target.parse().ok()?;
                let email_target = matches!(target, InviteTarget::Email(_));
                (target.matches(email) && (email_target || kind == InviteKind::EmailOrDomain))
                    .then_some((email_target, r))
            })
            .collect();
        // An invite for this very email beats one for its domain.
        fits.sort_by_key(|(email_target, _)| !email_target);
        let expired = |r: &SsoInvite| r.expires_at.is_some_and(|at| at <= now);
        let used_up = |r: &SsoInvite| r.max_uses.is_some_and(|m| r.uses >= m);
        if let Some((_, r)) = fits.iter().find(|(_, r)| !expired(r) && !used_up(r)) {
            return Ok(Ok(Invite {
                id: r.id.get().copied().unwrap_or_default(),
                max_uses: r.max_uses,
                roles: RoleNames::parse_opt(r.roles.as_deref()).unwrap_or_default(),
                link_user_id: r.link_user_id,
            }));
        }
        Ok(Err(if fits.iter().any(|(_, r)| used_up(r)) {
            Refusal::InviteUsedUp
        } else if fits.is_empty() {
            Refusal::NotInvited
        } else {
            Refusal::InviteExpired
        }))
    }

    /// Count one use of `invite` by `user_id` in one statement: a limited
    /// invite only while `uses < max_uses`. `false`: it is used up.
    async fn consume(tx: &mut PoolTx<'_>, invite: &Invite, user_id: i64) -> Result<bool, String> {
        use crate::core::Column as _;
        let qs = SsoInvite::objects().filter("id", invite.id);
        let qs = match invite.max_uses {
            // A limit set since the read applies too.
            None => qs.filter("max_uses__isnull", true),
            Some(_) => qs.where_(SsoInvite::uses.lt_expr(F("max_uses"))),
        };
        let query = qs
            .update()
            .set_expr("uses", F("uses") + 1)
            .set("used_at", chrono::Utc::now())
            .set("used_by", user_id)
            .compile()
            .map_err(|e| e.to_string())?;
        let n = crate::sql::update_tx(tx, &query)
            .await
            .map_err(|e| format!("use invite: {e}"))?;
        Ok(n == 1)
    }

    /// Delete an invite that can never be used. Best effort.
    pub(crate) async fn drop_invite(pool: &Pool, invite: &Invite) {
        let deleted = match SsoInvite::objects()
            .filter("id", invite.id)
            .compile_delete()
        {
            Ok(q) => crate::sql::delete_pool(pool, &q)
                .await
                .map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        if let Err(e) = deleted {
            tracing::warn!(target: "rustango::sso", invite = invite.id, "dangling re-link invite not deleted: {e}");
        }
    }

    /// Link `subject` to the existing `user_id` named by a re-link invite,
    /// using the invite up in the same transaction.
    pub(crate) async fn relink(
        pool: &Pool,
        key: &ProviderKey,
        subject: &str,
        invite: &Invite,
        user_id: i64,
    ) -> Result<Revoked, Denied> {
        let lock = Lock::prepare(pool).await?;
        let mut tx = crate::sql::transaction_pool(pool)
            .await
            .map_err(|e| Denied::Storage(format!("begin: {e}")))?;
        if !consume(&mut tx, invite, user_id)
            .await
            .map_err(Denied::Storage)?
        {
            return Err(Refusal::InviteUsedUp.into());
        }
        link::insert_link_tx(&mut tx, key, subject, user_id, false)
            .await
            .map_err(|e| Denied::Storage(format!("link: {e}")))?;
        let revoked = lock.apply_tx(&mut tx, user_id).await?;
        tx.commit()
            .await
            .map_err(|e| Denied::Storage(format!("commit: {e}")))?;
        Ok(revoked)
    }

    /// Link `subject` to the existing `user_id` its verified email matched.
    /// The account is locked in the same transaction ([`Lock`]).
    pub(crate) async fn link_by_email(
        pool: &Pool,
        key: &ProviderKey,
        subject: &str,
        user_id: i64,
    ) -> Result<Revoked, Denied> {
        let lock = Lock::prepare(pool).await?;
        let mut tx = crate::sql::transaction_pool(pool)
            .await
            .map_err(|e| Denied::Storage(format!("begin: {e}")))?;
        link::insert_link_tx(&mut tx, key, subject, user_id, false)
            .await
            .map_err(|e| Denied::Storage(format!("link: {e}")))?;
        let revoked = lock.apply_tx(&mut tx, user_id).await?;
        tx.commit()
            .await
            .map_err(|e| Denied::Storage(format!("commit: {e}")))?;
        Ok(revoked)
    }

    /// What an email link takes from the account it attaches to, so whoever
    /// registered the email before its owner loses it: the password, every
    /// session, API keys and user-owned agent keys. Tenant users have no
    /// second-factor devices. Prepared outside the transaction: the hash is
    /// slow, and a missing table must not abort it.
    pub(crate) struct Lock {
        hash: String,
        api_keys: bool,
        #[cfg(feature = "mcp")]
        agents: bool,
    }

    /// What [`Lock`] revoked, for the audit row.
    #[derive(Debug, Default, Clone, Copy, serde::Serialize)]
    pub(crate) struct Revoked {
        pub(crate) api_keys: u64,
        pub(crate) agent_keys: u64,
    }

    impl Lock {
        async fn prepare(pool: &Pool) -> Result<Self, Denied> {
            let here = |table: &'static str| async move {
                crate::migrate::try_table_exists_here(pool, table)
                    .await
                    .map_err(|e| Denied::Storage(format!("probe {table}: {e}")))
            };
            Ok(Self {
                hash: unusable_hash().await?,
                api_keys: here("rustango_api_keys").await?,
                #[cfg(feature = "mcp")]
                agents: here("rustango_agents").await?,
            })
        }

        async fn apply_tx(&self, tx: &mut PoolTx<'_>, user_id: i64) -> Result<Revoked, Denied> {
            use crate::core::Column as _;
            let storage = |e: ExecError| Denied::Storage(format!("lock account: {e}"));
            let bad = |e: crate::core::QueryError| Denied::Storage(e.to_string());
            let user = || User::objects().filter("id", user_id);
            let q = user()
                .update()
                .set("password_hash", self.hash.clone())
                .compile()
                .map_err(bad)?;
            crate::sql::update_tx(tx, &q).await.map_err(storage)?;
            // Only ever later: a concurrent logout may have set a later cut-off.
            let now = chrono::Utc::now();
            for older in [
                user().filter("sessions_revoked_at__isnull", true),
                user().where_(User::sessions_revoked_at.lt(now)),
            ] {
                let q = older
                    .update()
                    .set("sessions_revoked_at", now)
                    .compile()
                    .map_err(bad)?;
                crate::sql::update_tx(tx, &q).await.map_err(storage)?;
            }
            let mut revoked = Revoked::default();
            if self.api_keys {
                let q = crate::tenancy::auth_backends::ApiKey::objects()
                    .filter("user_id", user_id)
                    .compile_delete()
                    .map_err(bad)?;
                revoked.api_keys = crate::sql::delete_tx(tx, &q).await.map_err(storage)?;
            }
            #[cfg(feature = "mcp")]
            if self.agents {
                let q = crate::tenancy::agents::Agent::objects()
                    .filter("user_id", user_id)
                    .filter("active", true)
                    .update()
                    .set("active", false)
                    .compile()
                    .map_err(bad)?;
                revoked.agent_keys = crate::sql::update_tx(tx, &q).await.map_err(storage)?;
            }
            Ok(revoked)
        }
    }

    /// Whether `name` may be a username: letters, digits and `._@+-`, within
    /// the column. Non-ASCII letters stay allowed: verified emails carry them.
    fn valid_username(name: &str) -> bool {
        !name.is_empty()
            && name.chars().count() <= MAX_USERNAME
            && name
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '@' | '+' | '-'))
    }

    /// The username a new user starts from.
    fn base_username(from: UsernameFrom, email: &EmailAddr, identity: &Identity) -> String {
        let base = match from {
            UsernameFrom::Email => email.as_str(),
            UsernameFrom::PreferredUsername => identity
                .claim_str("preferred_username")
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| email.local()),
            _ => email.local(),
        };
        let base = if base.is_empty() {
            email.as_str()
        } else {
            base
        };
        base.chars().take(MAX_USERNAME).collect()
    }

    /// `base` with a random suffix, within the column's length.
    fn suffixed(base: &str) -> String {
        let suffix = crate::tenancy::password::generate(6).to_ascii_lowercase();
        let keep = MAX_USERNAME - suffix.len() - 1;
        format!("{}-{suffix}", base.chars().take(keep).collect::<String>())
    }

    /// Whether a user has this username, or this email, ignoring case: a
    /// sign-in by either must name one account.
    async fn username_taken(pool: &Pool, username: &str) -> Result<bool, Denied> {
        let storage = |e: ExecError| Denied::Storage(format!("username lookup: {e}"));
        for column in ["username__iexact", "email__iexact"] {
            if !User::objects()
                .filter(column, username.to_owned())
                .limit(1)
                .fetch(pool)
                .await
                .map_err(storage)?
                .is_empty()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// A real, unusable password hash: SSO users sign in through the IdP.
    async fn unusable_hash() -> Result<String, Denied> {
        use base64::Engine as _;
        use rand::RngCore as _;
        let mut buf = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut buf);
        crate::tenancy::password::hash_async(&base64::engine::general_purpose::STANDARD.encode(buf))
            .await
            .map_err(|e| Denied::Storage(format!("hash: {e}")))
    }

    /// What a successful sign-up made.
    pub(crate) struct SignedUp {
        pub(crate) user_id: i64,
        /// Role names no role row has: skipped, for the audit log.
        pub(crate) unknown_roles: Vec<String>,
    }

    /// Create a user for `email` (verified), its link and its roles, and use
    /// `invite` up, in one transaction. Never a superuser.
    pub(crate) async fn create_user(
        pool: &Pool,
        key: &ProviderKey,
        identity: &Identity,
        email: &EmailAddr,
        policy: &SignupPolicy,
        invite: Option<&Invite>,
    ) -> Result<SignedUp, Denied> {
        let base = base_username(policy.username_from, email, identity);
        if !valid_username(&base) {
            tracing::warn!(target: "rustango::sso", "sign-up: {base:?} is not a valid username");
            return Err(Refusal::UsernameTaken.into());
        }
        let mut roles: Vec<String> = match invite {
            Some(i) if !i.roles.is_empty() => i.roles.names().to_vec(),
            _ => policy.default_roles.names().to_vec(),
        };
        for r in policy.role_map.wanted(identity).unwrap_or_default() {
            if !roles.contains(&r) {
                roles.push(r);
            }
        }
        let display_name = identity
            .profile
            .name
            .as_deref()
            .and_then(super::display_name)
            .unwrap_or_else(|| base.clone());
        let data = serde_json::json!({
            "display_name": display_name,
            "avatar_url": identity.profile.avatar_url,
        });
        let password_hash = unusable_hash().await?;
        let refuse_clash = policy.username_clash == UsernameClash::Refuse;
        for attempt in 0..3 {
            let username = if attempt == 0 && !username_taken(pool, &base).await? {
                base.clone()
            } else if refuse_clash {
                return Err(Refusal::UsernameTaken.into());
            } else {
                suffixed(&base)
            };
            let mut user = User {
                id: crate::sql::Auto::Unset,
                username,
                password_hash: password_hash.clone(),
                email: Some(email.as_str().to_owned()),
                is_superuser: false,
                active: true,
                created_at: chrono::Utc::now(),
                data: data.clone(),
                password_changed_at: None,
                sessions_revoked_at: None,
            };
            let mut tx = crate::sql::transaction_pool(pool)
                .await
                .map_err(|e| Denied::Storage(format!("begin: {e}")))?;
            if let Err(e) = user.insert_tx(&mut tx).await {
                // Likely a username taken since the check: try a suffix.
                tracing::debug!(target: "rustango::sso", error = %e, "sign-up insert conflict");
                drop(tx);
                if attempt == 2 {
                    return Err(Denied::Storage(format!("insert user: {e}")));
                }
                continue;
            }
            let user_id = user
                .id
                .get()
                .copied()
                .ok_or_else(|| Denied::Storage("insert returned no id".into()))?;
            link::insert_link_tx(&mut tx, key, identity.subject(), user_id, true)
                .await
                .map_err(|e| Denied::Storage(format!("link: {e}")))?;
            if let Some(invite) = invite {
                if !consume(&mut tx, invite, user_id)
                    .await
                    .map_err(Denied::Storage)?
                {
                    return Err(Refusal::InviteUsedUp.into());
                }
            }
            let unknown_roles = grant_roles_tx(&mut tx, user_id, &roles)
                .await
                .map_err(|e| Denied::Storage(format!("roles: {e}")))?;
            tx.commit()
                .await
                .map_err(|e| Denied::Storage(format!("commit: {e}")))?;
            return Ok(SignedUp {
                user_id,
                unknown_roles,
            });
        }
        Err(Denied::Storage("no free username".into()))
    }

    /// The role rows named exactly `names` (the column may compare case-blind).
    async fn roles_named_tx(tx: &mut PoolTx<'_>, names: &[String]) -> Result<Vec<Role>, ExecError> {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let list = SqlValue::List(names.iter().cloned().map(SqlValue::String).collect());
        let rows = Role::objects()
            .filter("name__in", list)
            .fetch_tx(tx)
            .await?;
        Ok(rows
            .into_iter()
            .filter(|r| names.contains(&r.name))
            .collect())
    }

    /// Give `user_id` the roles `names`; the names no role has are returned.
    async fn grant_roles_tx(
        tx: &mut PoolTx<'_>,
        user_id: i64,
        names: &[String],
    ) -> Result<Vec<String>, ExecError> {
        let found = roles_named_tx(tx, names).await?;
        for r in &found {
            let role_id = r.id.get().copied().unwrap_or_default();
            crate::tenancy::permissions::assign_role_tx(tx, user_id, role_id).await?;
        }
        Ok(names
            .iter()
            .filter(|n| !found.iter().any(|r| &r.name == *n))
            .cloned()
            .collect())
    }

    /// Bring `user_id`'s mapped roles in line with `identity`'s claims:
    /// only roles the map names are added or removed. Returns unknown names.
    pub(crate) async fn sync_roles(
        pool: &Pool,
        user_id: i64,
        identity: &Identity,
        policy: &SignupPolicy,
    ) -> Result<RoleSync, Denied> {
        let map = &policy.role_map;
        if map.is_empty() {
            return Ok(RoleSync::default());
        }
        let managed: Vec<String> = map.managed().into_iter().map(str::to_owned).collect();
        // No role claim to read: leave the mapped roles as they are.
        let Some(wanted) = map.wanted(identity) else {
            return Ok(RoleSync::default());
        };
        let mut out = RoleSync::default();
        let storage = |e: ExecError| Denied::Storage(format!("role sync: {e}"));
        // Reads first: on SQLite take the write lock up front.
        let mut tx = crate::sql::write_transaction(pool).await.map_err(storage)?;
        let roles = roles_named_tx(&mut tx, &managed).await.map_err(storage)?;
        let role_ids: Vec<SqlValue> = roles
            .iter()
            .filter_map(|r| r.id.get().copied())
            .map(SqlValue::I64)
            .collect();
        let held: Vec<i64> = if role_ids.is_empty() {
            Vec::new()
        } else {
            UserRole::objects()
                .filter("user_id", user_id)
                .filter("role_id__in", SqlValue::List(role_ids))
                .fetch_tx(&mut tx)
                .await
                .map_err(storage)?
                .into_iter()
                .map(|ur| ur.role_id)
                .collect()
        };
        for r in &roles {
            let id = r.id.get().copied().unwrap_or_default();
            match (wanted.contains(&r.name), held.contains(&id)) {
                (true, false) => {
                    crate::tenancy::permissions::assign_role_tx(&mut tx, user_id, id)
                        .await
                        .map_err(storage)?;
                    out.added.push(r.name.clone());
                }
                (false, true) => {
                    crate::tenancy::permissions::remove_role_tx(&mut tx, user_id, id)
                        .await
                        .map_err(storage)?;
                    out.removed.push(r.name.clone());
                }
                _ => {}
            }
        }
        tx.commit()
            .await
            .map_err(|e| Denied::Storage(format!("role sync commit: {e}")))?;
        out.unknown = wanted
            .into_iter()
            .filter(|n| !roles.iter().any(|r| &r.name == n))
            .collect();
        Ok(out)
    }

    /// Refresh a non-superuser's name, and its email when the IdP vouches
    /// for it and no other user has it.
    pub(crate) async fn sync_profile(
        pool: &Pool,
        user_id: i64,
        identity: &Identity,
        verified_email: Option<&EmailAddr>,
    ) -> Result<(), Denied> {
        use crate::sql::UpdaterPool as _;
        let storage = |e: ExecError| Denied::Storage(format!("profile sync: {e}"));
        let Some(user) = crate::tenancy::member_auth::find_tenant_user(pool, user_id)
            .await
            .map_err(storage)?
        else {
            return Ok(());
        };
        if user.is_superuser {
            return Ok(());
        }
        let mut update = User::objects().filter("id", user_id).update();
        let mut changed = false;
        if let Some(name) = identity
            .profile
            .name
            .as_deref()
            .and_then(super::display_name)
        {
            if user.data.get("display_name").and_then(|v| v.as_str()) != Some(name.as_str()) {
                let mut data = user.data.clone();
                if !data.is_object() {
                    data = serde_json::json!({});
                }
                data["display_name"] = serde_json::Value::String(name);
                update = update.set("data", data);
                changed = true;
            }
        }
        if let Some(email) = verified_email {
            let same = user
                .email
                .as_deref()
                .is_some_and(|e| e.eq_ignore_ascii_case(email.as_str()));
            if !same {
                let taken = User::objects()
                    .filter("email__iexact", email.as_str().to_owned())
                    .fetch(pool)
                    .await
                    .map_err(storage)?
                    .iter()
                    .any(|u| u.id.get().copied() != Some(user_id));
                if taken {
                    tracing::warn!(target: "rustango::sso", user_id, "profile sync: the new email belongs to another user; kept the old one");
                } else {
                    update = update.set("email", email.as_str().to_owned());
                    changed = true;
                }
            }
        }
        if changed {
            update.execute_pool(pool).await.map_err(storage)?;
        }
        Ok(())
    }

    /// Audit role names no role row has.
    pub(crate) async fn audit_unknown_roles(
        pool: &Pool,
        provider: &str,
        user_id: i64,
        names: &[String],
    ) {
        if names.is_empty() {
            return;
        }
        tracing::warn!(target: "rustango::sso", user_id, provider, roles = ?names, "SSO role names with no role row were skipped");
        // Every sign-in repeats it: capped like refusals.
        let table = "rustango_user_roles";
        let pk = user_id.to_string();
        if crate::sso::policy::recent_audit_rows(pool, table, &pk).await
            >= crate::sso::policy::MAX_REFUSAL_ROWS
        {
            return;
        }
        let entry = crate::audit::PendingEntry {
            entity_table: table,
            entity_pk: pk,
            operation: crate::audit::AuditOp::Action,
            source: crate::audit::AuditSource::Custom("sso".into()),
            changes: serde_json::json!({
                "event": "sso_role_unknown",
                "provider": provider,
                "roles": names,
            }),
        };
        if let Err(e) = crate::audit::emit_one_pool(pool, &entry).await {
            tracing::error!(target: "rustango::sso", error = %e, "an unknown SSO role was NOT written to the audit log");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invite_targets_parse_to_one_spelling() {
        for (raw, canon) in [
            (" Ann@Client.org ", "ann@client.org"),
            ("@Client.org", "@client.org"),
            ("client.org", "@client.org"),
            ("*.client.org", "*.client.org"),
        ] {
            assert_eq!(raw.parse::<InviteTarget>().unwrap().to_string(), canon);
        }
        for bad in ["@*.client.org", "gmail", "@", "a@", "*"] {
            assert!(bad.parse::<InviteTarget>().is_err(), "{bad}");
        }
        let email: EmailAddr = "ann@eu.client.org".parse().unwrap();
        assert_eq!(
            InviteTarget::candidates(&email),
            vec![
                "ann@eu.client.org".to_owned(),
                "@eu.client.org".to_owned(),
                "*.client.org".to_owned()
            ]
        );
        for c in InviteTarget::candidates(&email) {
            assert!(c.parse::<InviteTarget>().unwrap().matches(&email), "{c}");
        }
        assert!(!"@client.org"
            .parse::<InviteTarget>()
            .unwrap()
            .matches(&email));
    }

    #[test]
    fn display_names_are_trimmed_and_capped() {
        assert_eq!(display_name("  Ann  ").as_deref(), Some("Ann"));
        assert_eq!(display_name("   "), None);
        let long = "é".repeat(400);
        assert_eq!(display_name(&long).unwrap().chars().count(), 150);
    }

    /// A domain invite for a public suffix or free mail would invite anyone.
    #[test]
    fn open_domains_are_refused_as_invite_targets() {
        for bad in [
            "@gmail.com",
            "gmail.com",
            "*.co.uk",
            "@co.uk",
            "*.github.io",
            "*.onmicrosoft.com",
            "@yahoo.co.uk",
            "*.s3.amazonaws.com",
            "@edu.au",
        ] {
            assert!(bad.parse::<InviteTarget>().is_err(), "{bad}");
        }
        assert!("ann@gmail.com".parse::<InviteTarget>().is_ok());
        assert!("*.client.co.uk".parse::<InviteTarget>().is_ok());
    }

    #[test]
    fn role_map_reads_only_its_claims_and_names_what_it_manages() {
        use crate::oauth2::NormalizedUser;
        let map = RoleMap::parse(
            Some(&serde_json::json!({"g-ed": "Editor", "App.Admin": ["Admin", "Editor"]})),
            None,
        )
        .unwrap();
        assert_eq!(
            map.managed().into_iter().collect::<Vec<_>>(),
            vec!["Admin", "Editor"]
        );
        let id = super::super::identity::test_identity(
            NormalizedUser {
                provider: "oidc".into(),
                provider_user_id: "s".into(),
                email: None,
                email_verified: false,
                name: None,
                avatar_url: None,
                raw: serde_json::json!({"groups": ["g-ed", "x"], "other": ["App.Admin"]}),
            },
            None,
            false,
        );
        assert_eq!(
            map.wanted(&id).unwrap().into_iter().collect::<Vec<_>>(),
            vec!["Editor".to_owned()]
        );
        for bad in [
            serde_json::json!([]),
            serde_json::json!({"g": 1}),
            serde_json::json!({"g": ""}),
        ] {
            assert!(RoleMap::parse(Some(&bad), None).is_err(), "{bad}");
        }
        assert!(RoleMap::parse(None, Some("groups, bad claim")).is_err());
    }

    #[test]
    fn signup_columns_parse_and_default() {
        let p = SignupPolicy::parse(&SignupColumns::default()).unwrap();
        assert_eq!(p, SignupPolicy::default());
        assert_eq!(p.username_from, UsernameFrom::EmailLocal);
        let p = SignupPolicy::parse(&SignupColumns {
            signup_mode: Some("invite_only"),
            default_roles: Some("Viewer,\n Editor, Viewer"),
            username_clash: Some("refuse"),
            ..SignupColumns::default()
        })
        .unwrap();
        assert_eq!(p.mode, Some(SignupMode::InviteOnly));
        assert_eq!(p.default_roles.names(), ["Viewer", "Editor"]);
        assert_eq!(p.username_clash, UsernameClash::Refuse);
        assert!(SignupPolicy::parse(&SignupColumns {
            signup_mode: Some("sometimes"),
            ..SignupColumns::default()
        })
        .is_err());
    }
}
