//! `SsoProvider` — one configurable OpenID Connect / social login provider,
//! stored as a row and managed from the admin UI (`sso` feature).
//!
//! This replaces the old flat `Org.sso_*` columns (single provider per
//! tenant). A tenant/app can now have **many** providers, each a row:
//! enter an OIDC `issuer_url` + `client_id` + a `secret_ref`, and the login
//! endpoints are auto-discovered at login time via
//! `OAuth2Provider::from_discovery`. Known social keys (`google`,
//! `microsoft`, `github`, `gitlab`, `discord`) use the built-in presets and
//! need no issuer.
//!
//! Scope: **tenant** (default) — the table lives in each tenant's own
//! storage, so tenant admins manage their own providers (granular) and the
//! bare/standalone admin uses it as a plain global table. The registry-wide
//! shared set is the sibling [`crate::tenancy::sso::SharedSsoProvider`].
//!
//! The client secret is stored in `client_secret`, **encrypted at rest**
//! (XChaCha20-Poly1305, key from `RUSTANGO_SECRET_KEY`) and decrypted in-memory
//! only at login time — so a leaked DB dump never exposes it, and each tenant
//! keeps its own secret without any per-tenant env var.

use crate::casts::{Cast, EncryptedString};
use crate::sql::Auto;

/// A single SSO/OpenID provider offered on the login page.
///
/// `slug` is the stable route key and button id (`{login}/sso/{slug}`);
/// `kind` selects a built-in preset or `"oidc"` for issuer discovery.
#[derive(crate::Model, Debug, Clone)]
#[rustango(
    table = "rustango_sso_providers",
    admin(
        list_display = "slug, label, kind, enabled, sort_order",
        ordering = "sort_order",
        readonly_fields = "created_at, updated_at",
        formfield_overrides = "client_secret: password",
    )
)]
#[allow(dead_code)]
pub struct SsoProvider {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    /// Stable route key + button id — used in `{login_url}/sso/{slug}` and
    /// its callback. Unique within the table (per-tenant in tenancy mode,
    /// since the table is physically per-tenant).
    #[rustango(max_length = 64, unique)]
    pub slug: String,

    /// Button label, e.g. `"Sign in with Google"`.
    #[rustango(max_length = 150)]
    pub label: String,

    /// Provider kind: a built-in preset key (`"google"`, `"microsoft"`,
    /// `"github"`, `"gitlab"`, `"discord"`) or `"oidc"` for a generic
    /// OpenID Connect provider configured via `issuer_url`.
    #[rustango(max_length = 32)]
    pub kind: String,

    /// OIDC issuer base URL (e.g. a Keycloak realm or Okta domain), used
    /// with `kind = "oidc"` to auto-discover endpoints via
    /// `{issuer}/.well-known/openid-configuration`. Unused for presets.
    #[rustango(max_length = 255)]
    pub issuer_url: Option<String>,

    /// OAuth2 client id issued by the IdP.
    #[rustango(max_length = 255)]
    pub client_id: String,

    /// The OAuth2 client secret, **encrypted at rest** (XChaCha20-Poly1305,
    /// key from `RUSTANGO_SECRET_KEY`). Each tenant stores its own here via the
    /// admin UI; decrypted in-memory only at login time to authenticate to the
    /// IdP's token endpoint. The single home for a provider's secret — no env
    /// var, no plaintext-in-DB.
    #[rustango(max_length = 1024)]
    pub client_secret: Cast<EncryptedString>,

    /// Offer this provider on the login page when `true`.
    #[rustango(default = "true")]
    pub enabled: bool,

    /// Button ordering on the login page (ascending).
    #[rustango(default = "0")]
    pub sort_order: i32,

    /// Optional space-separated OAuth scope override. When set, replaces the
    /// default `openid email profile` scopes for this provider.
    #[rustango(max_length = 255)]
    pub scopes: Option<String>,

    /// Link a first-time SSO user to the account with the same verified
    /// email. Never links a superuser or staff account; the bare admin ignores it.
    /// Only a superuser can add or change provider rows in the admin.
    #[rustango(default = "false")]
    pub allow_email_link: bool,

    /// Set on INSERT via `DEFAULT NOW()`.
    #[rustango(auto_now_add)]
    pub created_at: Auto<chrono::DateTime<chrono::Utc>>,

    /// Bumped to `NOW()` on every save.
    #[rustango(auto_now)]
    pub updated_at: Auto<chrono::DateTime<chrono::Utc>>,
}

use super::link::{LinkSource, ProviderKey};
use super::{parse_scopes, ProviderButton, ResolvedSso, SsoError};
use crate::core::{Model, SqlValue};
use crate::query::QuerySet;
use crate::sql::{ExecError, Pool};

// The admin writes raw form values, so the secret is encrypted here, as
// `Cast<EncryptedString>` does for ORM writes (#1764).
#[cfg(feature = "admin")]
pub(crate) fn admin_encrypt_secret<'a>(
    values: &'a mut Vec<(&'static str, SqlValue)>,
    _before: Option<&'a serde_json::Value>,
) -> crate::admin::derived_fields::DeriveFuture<'a> {
    let out = match crate::admin::derived_fields::take_secret(values, "client_secret") {
        None => Ok(()),
        Some(plain) => crate::casts::encrypt(plain.as_bytes())
            .map(|c| values.push(("client_secret", SqlValue::String(c))))
            .map_err(|e| e.to_string()),
    };
    Box::pin(async move { out })
}

#[cfg(feature = "admin")]
inventory::submit! {
    crate::admin::derived_fields::AdminDerivedField {
        table: "rustango_sso_providers",
        derive: admin_encrypt_secret,
    }
}

/// Columns provider reads leave out. `allow_email_link` is read on its own,
/// so a table not yet migrated still serves logins.
const DEFERRED: &[&str] = &["allow_email_link", "created_at", "updated_at"];

/// A provider row, shared by [`SsoProvider`] and `SharedSsoProvider`.
/// `client_secret` is still encrypted.
pub(crate) struct ProviderRow {
    pub id: i64,
    pub slug: String,
    pub label: String,
    pub kind: String,
    pub issuer_url: Option<String>,
    pub client_id: String,
    client_secret: String,
    pub enabled: bool,
    pub sort_order: i32,
    pub scopes: Option<String>,
}

fn text(v: Option<&SqlValue>) -> Option<String> {
    match v {
        Some(SqlValue::String(s)) => Some(s.clone()),
        _ => None,
    }
}

fn int(v: Option<&SqlValue>) -> i64 {
    match v {
        Some(SqlValue::I64(n)) => *n,
        Some(SqlValue::I32(n)) => i64::from(*n),
        Some(SqlValue::I16(n)) => i64::from(*n),
        Some(SqlValue::Bool(b)) => i64::from(*b),
        _ => 0,
    }
}

/// Provider rows matching `qs`, without the [`DEFERRED`] columns. A missing
/// table holds none, so a tenant without one falls through to the shared set (#2366).
pub(crate) async fn load_rows<T: Model>(
    qs: QuerySet<T>,
    pool: &Pool,
) -> Result<Vec<ProviderRow>, ExecError> {
    if !crate::migrate::try_table_exists_here(pool, T::SCHEMA.table).await? {
        return Ok(Vec::new());
    }
    let rows = qs.defer(DEFERRED).fetch(pool).await?;
    Ok(rows
        .iter()
        .map(|r| ProviderRow {
            id: int(r.get("id")),
            slug: text(r.get("slug")).unwrap_or_default(),
            label: text(r.get("label")).unwrap_or_default(),
            kind: text(r.get("kind")).unwrap_or_default(),
            issuer_url: text(r.get("issuer_url")),
            client_id: text(r.get("client_id")).unwrap_or_default(),
            client_secret: text(r.get("client_secret")).unwrap_or_default(),
            enabled: int(r.get("enabled")) != 0,
            sort_order: i32::try_from(int(r.get("sort_order"))).unwrap_or(0),
            scopes: text(r.get("scopes")),
        })
        .collect())
}

/// A resolved provider plus what the link step needs.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ResolvedProvider {
    pub sso: ResolvedSso,
    /// Provider row id.
    pub id: i64,
    /// The row's `allow_email_link`; `false` when it can't be read.
    pub allow_email_link: bool,
}

impl ResolvedProvider {
    /// Link key for this provider as seen from `source`.
    #[must_use]
    pub fn key(&self, source: LinkSource) -> ProviderKey {
        ProviderKey::for_row(
            source,
            self.id,
            &self.sso.provider,
            self.sso.issuer_url.as_deref(),
        )
    }
}

/// Resolve the enabled `T` row with this `slug`. `Ok(None)` when none matches.
pub(crate) async fn resolve_row<T: Model>(
    pool: &Pool,
    slug: &str,
    redirect_uri: String,
) -> Result<Option<ResolvedProvider>, SsoError> {
    use crate::casts::CastValue as _;
    let row = load_rows(QuerySet::<T>::new().filter("slug", slug.to_owned()), pool)
        .await
        .map_err(|e| SsoError::Config(format!("db: {e}")))?
        .into_iter()
        .find(|r| r.enabled);
    let Some(r) = row else {
        return Ok(None);
    };
    let client_secret =
        EncryptedString::from_db(&r.client_secret).map_err(|e| SsoError::Secret(e.to_string()))?;
    let allow_email_link = match QuerySet::<T>::new()
        .filter("id", r.id)
        .values_list_flat("allow_email_link")
        .fetch::<bool>(pool)
        .await
    {
        Ok(v) => v.first().copied().unwrap_or(false),
        Err(e) => {
            tracing::warn!(target: "rustango::sso", "allow_email_link unreadable, treated as off: {e}");
            false
        }
    };
    Ok(Some(ResolvedProvider {
        sso: ResolvedSso {
            provider: r.kind,
            issuer_url: r.issuer_url,
            client_id: r.client_id,
            client_secret,
            redirect_uri,
            scopes: parse_scopes(r.scopes.as_deref()),
        },
        id: r.id,
        allow_email_link,
    }))
}

/// Enabled providers for the bare admin login page, sorted by `sort_order`.
/// `login_base` is the login path (e.g. `/admin/login`); each button links
/// to `{login_base}/sso/{slug}`. A DB error yields an empty list (the login
/// page still renders the password form).
pub async fn list_enabled(pool: &Pool, login_base: &str) -> Vec<ProviderButton> {
    let mut rows = load_rows(QuerySet::<SsoProvider>::new(), pool)
        .await
        .unwrap_or_default();
    rows.retain(|r| r.enabled);
    rows.sort_by_key(|r| r.sort_order);
    rows.into_iter()
        .map(|r| ProviderButton {
            login_url: format!("{login_base}/sso/{}", r.slug),
            slug: r.slug,
            label: r.label,
        })
        .collect()
}

/// Resolve one enabled [`SsoProvider`] by `slug`. `Ok(None)` when no enabled
/// row matches.
///
/// # Errors
/// [`SsoError::Config`] on a DB error, [`SsoError::Secret`] when the
/// stored secret can't be decrypted.
pub async fn resolve_by_slug(
    pool: &Pool,
    slug: &str,
    redirect_uri: String,
) -> Result<Option<ResolvedProvider>, SsoError> {
    resolve_row::<SsoProvider>(pool, slug, redirect_uri).await
}
