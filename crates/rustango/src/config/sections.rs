//! Typed section structs that the loader fills in.
//!
//! Every section is `#[serde(default)]` so missing TOML keys fall back
//! to their `Default::default()`. New fields can be added without
//! breaking older config files.

use serde::Deserialize;

/// Top-level config — every section is optional and defaults to its
/// type's `Default` impl. The loader fills sections from
/// `config/default.toml` + `config/{env}_settings.toml` (or the
/// legacy `{env}.toml`) + `RUSTANGO__*` env-var overrides.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// `[database]` — connection URL, pool sizing, TLS.
    pub database: DatabaseSettings,

    /// `[secret_key]` — base64-encoded HMAC key for session cookies.
    /// Lives at top level (not nested under a section header) when
    /// it's a bare string in TOML; the loader normalises both shapes.
    pub secret_key: Option<String>,

    /// `[admin]` — auto-admin allowlist + read-only marker.
    pub admin: AdminSettings,

    /// `[tenancy]` — apex domain, secrets resolver style.
    pub tenancy: TenancySettings,

    /// `[cache]` — in-memory / Redis / Postgres backend selection.
    pub cache: CacheSettings,

    /// `[jobs]` — background-jobs runner config.
    pub jobs: JobsSettings,

    /// `[mail]` — mailer config.
    pub mail: MailSettings,

    /// `[server]` — HTTP listener bind address + request timeout (#87).
    pub server: ServerSettings,

    /// `[auth]` — JWT TTLs, password hashing cost, account lockout
    /// thresholds (#87).
    pub auth: AuthSettings,

    /// `[brand]` — operator console + tenant admin display strings
    /// (name, tagline, logo URL, accent color) (#87, mirrors #72).
    pub brand: BrandSettings,

    /// `[security]` — security-headers preset, CSP, CORS allowed
    /// origins (#87).
    pub security: SecuritySettings,

    /// `[routes]` — URL-prefix overrides for login / admin / audit /
    /// static / brand / change-password / impersonation handoff (#87,
    /// mirrors #74 + #88). Sections of `tenancy::RouteConfig` exposed
    /// declaratively so projects don't need to call
    /// `Cli::routes(RouteConfig::legacy())` from code.
    pub routes: RoutesSettings,

    /// `[audit]` — retention + redaction policy (#87).
    pub audit: AuditSettings,
}

impl Settings {
    /// List of cargo features compiled into this `rustango` build.
    /// Useful for telemetry, version pages, and deployment audits
    /// ("the prod binary doesn't have `oauth2` enabled — auth will
    /// 500 on the social-login button"). Lazy-evaluated against
    /// `#[cfg(feature = "...")]` so the list reflects what's
    /// actually linked, not what the project's `Cargo.toml`
    /// declares (which could differ in a multi-crate workspace).
    #[must_use]
    pub fn detected_features() -> Vec<&'static str> {
        let mut out: Vec<&'static str> = Vec::new();
        macro_rules! feat {
            ($name:literal) => {
                #[cfg(feature = $name)]
                out.push($name);
            };
        }
        feat!("postgres");
        feat!("mysql");
        feat!("sqlite");
        feat!("tenancy");
        feat!("admin");
        feat!("manage");
        feat!("config");
        feat!("forms");
        feat!("serializer");
        feat!("cache");
        feat!("signals");
        feat!("email");
        feat!("storage");
        feat!("storage-s3");
        feat!("scheduler");
        feat!("secrets");
        feat!("totp");
        feat!("webhook");
        feat!("webhook-delivery");
        feat!("api_keys");
        feat!("passwords");
        feat!("signed_url");
        feat!("notifications");
        feat!("jobs");
        feat!("jobs-postgres");
        feat!("auth_flows");
        feat!("sse");
        feat!("websocket");
        feat!("oauth2");
        feat!("http-client");
        feat!("compression");
        feat!("openapi");
        feat!("csp-nonce");
        feat!("sessions");
        feat!("hmac-auth");
        feat!("jwt");
        feat!("uploads");
        feat!("media");
        feat!("runserver");
        out
    }
}

/// Connection URL + pool sizing for the primary database.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct DatabaseSettings {
    /// Postgres connection URL. Required at runtime; loader doesn't
    /// enforce presence so callers can ship a config that overrides
    /// this from `RUSTANGO__DATABASE__URL` only.
    pub url: Option<String>,
    /// Maximum number of pooled connections. `None` means use sqlx's
    /// default.
    pub pool_max_size: Option<u32>,
    /// Minimum number of pooled connections kept warm. `None` =
    /// driver default.
    pub pool_min_size: Option<u32>,
}

/// Auto-admin tweaks read at boot. Mirrors the `admin::Builder`
/// flags so `Settings`-driven projects don't need to hand-wire them.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct AdminSettings {
    /// Tables visible in the admin. Empty / missing = every
    /// registered model.
    pub allowed_tables: Vec<String>,
    /// Tables whose mutating routes are blocked. Empty / missing =
    /// every table is read-write.
    pub read_only_tables: Vec<String>,
}

/// Multi-tenancy operator-side settings. Tenant-side resolver
/// config is per-Org row in the registry; this section is for the
/// host-wide knobs.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct TenancySettings {
    /// Apex domain for subdomain-based tenant resolution. Mirror of
    /// the `RUSTANGO_APEX_DOMAIN` env var; the env-var path stays as
    /// a fallback for the `tenancy_manage` example binary.
    pub apex_domain: Option<String>,
}

/// Cache-backend selection. Slice 10.3 lights up; the section is
/// in v0.8 so config files written today survive the v0.10 upgrade.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct CacheSettings {
    /// `"memory"` (default), `"redis"`, `"postgres"`.
    pub backend: Option<String>,
    /// Redis connection URL when `backend = "redis"`.
    pub redis_url: Option<String>,
}

/// Background-jobs runner config. Slice 10.1 lights up.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct JobsSettings {
    /// `"pg"` (default), `"redis"`, `"memory"`.
    pub backend: Option<String>,
    /// Worker concurrency — number of jobs processed in parallel.
    /// `None` = single-threaded.
    pub concurrency: Option<u32>,
}

/// Mailer config. Slice 10.2 lights up.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct MailSettings {
    /// `"smtp"`, `"console"` (default for dev), `"memory"` (tests).
    pub backend: Option<String>,
    /// SMTP host. Required when `backend = "smtp"`.
    pub smtp_host: Option<String>,
    /// `From:` address for sent mail.
    pub from_address: Option<String>,
}

/// HTTP server bind + request-timeout knobs (#87).
///
/// The framework reads `RUSTANGO_BIND` today as a fallback when
/// `bind` is unset — set both during a migration to confirm the
/// new path picks the same value before retiring the env var.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct ServerSettings {
    /// Listener address. Default `"127.0.0.1:8080"` for dev,
    /// `"0.0.0.0:8080"` for prod (the latter exposes the bind
    /// publicly — make sure the deployment puts a reverse proxy
    /// in front).
    pub bind: Option<String>,
    /// Per-request handler timeout in seconds. `None` = no timeout
    /// (axum default). Production deployments typically set this to
    /// 30s so a wedged handler can't hold a worker hostage.
    pub request_timeout_secs: Option<u64>,
    /// Maximum body bytes accepted on POST/PUT/PATCH. `None` =
    /// 2 MiB (axum default). Raise for file-upload routes.
    pub max_body_bytes: Option<u64>,
}

/// Authentication knobs (#87) — JWT lifetimes, password hashing
/// cost, account lockout policy. Each field has a sensible default
/// matching the framework's hardcoded values, so existing
/// deployments don't behave differently after upgrading.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct AuthSettings {
    /// JWT-related lifetimes + behavior. Mirrors
    /// `rustango::tenancy::auth_routes::Config` field names so a
    /// `Settings`-driven project can hand the section straight to
    /// `auth_routes::jwt_router(...)`.
    pub jwt: JwtSettings,
    /// Argon2id memory cost (KiB). Default `19456` (~19 MiB) — the
    /// OWASP-recommended floor for password hashing as of 2024.
    /// Lower values speed up login at the cost of brute-force
    /// resistance. Keep ≥ 15 MiB in prod.
    pub argon2_memory_kib: Option<u32>,
    /// Argon2id iteration count. Default `2`. OWASP recommends
    /// `≥ 2` paired with `≥ 19456 KiB` memory.
    pub argon2_iterations: Option<u32>,
    /// Argon2id parallelism (lanes). Default `1` — single-threaded
    /// hashing is fastest on a busy server because parallel hashing
    /// just trades one core's bandwidth for another's.
    pub argon2_parallelism: Option<u32>,
    /// Failed-login attempts before lockout. Default `5`.
    pub lockout_threshold: Option<u32>,
    /// Lockout duration in seconds. Default `900` (15 min).
    pub lockout_duration_secs: Option<u64>,
}

/// JWT lifetime knobs (#87). Defaults match
/// `rustango::tenancy::auth_routes::Config::default()`.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct JwtSettings {
    /// Access-token TTL in seconds. Default `900` (15 min).
    pub access_ttl_secs: Option<u64>,
    /// Refresh-token TTL in seconds. Default `604800` (7 days).
    pub refresh_ttl_secs: Option<u64>,
    /// JWT issuer claim (`iss`). Defaults to the framework's name.
    pub issuer: Option<String>,
    /// JWT audience claim (`aud`).
    pub audience: Option<String>,
}

/// Operator console + tenant admin display strings (#87, mirrors
/// #72). The framework today reads these via `RUSTANGO_OPERATOR_*`
/// env vars; declaring them in TOML makes per-tier branding
/// (different staging vs prod logo) cleaner. Per-tenant branding
/// stays on the `Org` row.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct BrandSettings {
    /// Display name shown on the operator console + tenant admin
    /// chrome (e.g. `"Acme Operator"`). Default `"Rustango"`.
    pub name: Option<String>,
    /// Optional tagline rendered under the brand name.
    pub tagline: Option<String>,
    /// Logo URL (operator console). Defaults to the embedded
    /// `/__static__/rustango.png` asset.
    pub logo_url: Option<String>,
    /// Hex-encoded accent color (e.g. `"#2c6fb0"`). Picked by the
    /// theme tokens to tint primary buttons / links.
    pub primary_color: Option<String>,
    /// `"auto"` (default), `"light"`, `"dark"` — initial theme mode
    /// for the operator console. Tenant admin reads this too, but
    /// per-tenant override on `Org.theme_mode` wins.
    pub theme_mode: Option<String>,
}

/// Security-headers + CSP + CORS knobs (#87). The defaults map
/// straight to `SecurityHeadersLayer::strict()`; per-section
/// overrides let staging/dev relax specific constraints (e.g. allow
/// inline scripts during local development).
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct SecuritySettings {
    /// `"strict"` (default), `"relaxed"`, `"dev"`, `"none"`. Picks
    /// the [`SecurityHeadersLayer`] preset.
    pub headers_preset: Option<String>,
    /// Content-Security-Policy header value. `None` = no CSP set.
    /// Use one CSP for the public app and a stricter one for the
    /// admin if needed (per-router layer).
    pub csp: Option<String>,
    /// HSTS `max-age` in seconds. Default `31536000` (1 year).
    /// `0` disables HSTS — useful in dev where you might switch
    /// between http and https.
    pub hsts_max_age_secs: Option<u64>,
    /// CORS allowed origins. Empty / missing = no CORS layer
    /// added. `["*"]` is permissive (browsers don't allow it with
    /// credentials).
    pub cors_allowed_origins: Vec<String>,
}

/// URL-prefix overrides for the framework's built-in routes (#87,
/// mirrors `tenancy::RouteConfig` from #74). Declaring these in
/// TOML lets ops change the admin path without touching code.
///
/// Fields default to the friendly v0.29 preset (`/login`, `/admin`,
/// `/audit`, `/_static`, `/_brand`, `/_impersonation_handoff`).
/// Set `legacy_preset = true` to flip to `/__login` / `/__admin` /
/// etc. without listing every field.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct RoutesSettings {
    /// Set `true` to apply `RouteConfig::legacy()` (the v0.28
    /// `__`-prefixed shape) before per-field overrides. Default
    /// `false` (friendly preset, post-#85).
    pub legacy_preset: Option<bool>,
    /// `/login` (friendly) / `/__login` (legacy).
    pub login_url: Option<String>,
    /// `/logout` / `/__logout`.
    pub logout_url: Option<String>,
    /// `/admin` / `/__admin`.
    pub admin_url: Option<String>,
    /// `/audit` / `/__audit`.
    pub audit_url: Option<String>,
    /// `/_static` / `/__static__`.
    pub static_url: Option<String>,
    /// `/_brand` / `/__brand__`.
    pub brand_url: Option<String>,
    /// `/change-password` / `/__change-password`.
    pub change_password_url: Option<String>,
    /// `/_impersonation_handoff` / `/__impersonation_handoff` (#88).
    pub impersonation_handoff_url: Option<String>,
}

/// Audit-log retention + redaction policy (#87).
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct AuditSettings {
    /// Retention in days. `None` = keep forever (cron `audit-cleanup
    /// --days <N>` is the operator-managed alternative). Production
    /// deployments typically set 90-365 depending on compliance.
    pub retention_days: Option<u32>,
    /// Extra query-param names whose values should be redacted in
    /// access logs (in addition to the framework's built-in
    /// `password` / `token` / `secret` / `api_key` / `access_token`
    /// / `refresh_token` / `signature` defaults).
    pub redact_query_params: Vec<String>,
}
