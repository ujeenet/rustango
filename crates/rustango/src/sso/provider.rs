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

    /// Set on INSERT via `DEFAULT NOW()`.
    #[rustango(auto_now_add)]
    pub created_at: Auto<chrono::DateTime<chrono::Utc>>,

    /// Bumped to `NOW()` on every save.
    #[rustango(auto_now)]
    pub updated_at: Auto<chrono::DateTime<chrono::Utc>>,
}

use super::{parse_scopes, ProviderButton, ResolvedSso, SsoError};
use crate::sql::Pool;

/// Enabled providers for the bare admin login page, sorted by `sort_order`.
/// `login_base` is the login path (e.g. `/admin/login`); each button links
/// to `{login_base}/sso/{slug}`. A DB error yields an empty list (the login
/// page still renders the password form).
pub async fn list_enabled(pool: &Pool, login_base: &str) -> Vec<ProviderButton> {
    use crate::sql::FetcherPool as _;
    let mut rows: Vec<SsoProvider> = SsoProvider::objects().fetch(pool).await.unwrap_or_default();
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

/// Resolve one enabled provider by `slug` into a ready-to-build
/// [`ResolvedSso`] (secret dereferenced via `env://`/literal). `Ok(None)`
/// when no enabled row matches.
///
/// # Errors
/// [`SsoError::Config`] on a DB error, [`SsoError::Secret`] when the
/// `secret_ref` can't be resolved.
pub async fn resolve_by_slug(
    pool: &Pool,
    slug: &str,
    redirect_uri: String,
) -> Result<Option<ResolvedSso>, SsoError> {
    use crate::sql::FetcherPool as _;
    let row = SsoProvider::objects()
        .filter("slug", slug.to_owned())
        .fetch(pool)
        .await
        .map_err(|e| SsoError::Config(format!("db: {e}")))?
        .into_iter()
        .find(|r| r.enabled);
    let Some(r) = row else {
        return Ok(None);
    };
    let client_secret = r.client_secret.clone().into_inner();
    Ok(Some(ResolvedSso {
        provider: r.kind,
        issuer_url: r.issuer_url,
        client_id: r.client_id,
        client_secret,
        redirect_uri,
        scopes: parse_scopes(r.scopes.as_deref()),
    }))
}
