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

    /// `[logging]` — tracing-subscriber config: level filter,
    /// pretty vs JSON output, optional rolling file sink. v0.30.11
    /// (roadmap #8). Fields are `Option`-typed so missing keys
    /// fall through to `logging::Setup::new()` defaults.
    pub logging: LoggingSettings,
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
    /// Connection URL. Required at runtime; loader doesn't enforce
    /// presence so callers can ship a config that overrides this from
    /// `RUSTANGO__DATABASE__URL` only. Supported schemes: `postgres://`,
    /// `mysql://`, `sqlite:` — [`crate::sql::Pool::connect`] dispatches
    /// per-backend by URL scheme.
    pub url: Option<String>,
    /// Explicit backend selector — `"postgres"` / `"mysql"` / `"sqlite"`.
    /// **Optional.** When unset, the backend is inferred from the URL
    /// scheme at [`crate::sql::Pool::connect`] time; the inferred
    /// value is reflected back here by
    /// [`DatabaseSettings::resolved_backend`] so admin/templates can
    /// branch on dialect without owning the [`crate::sql::Pool`].
    ///
    /// Explicit values that don't match the URL scheme are flagged by
    /// `manage check --deploy` as a misconfiguration; they DON'T
    /// override the URL — sqlx still binds the backend dictated by
    /// the URL prefix. This field is a deploy-intent assertion, not
    /// an override.
    pub backend: Option<String>,
    /// Maximum number of pooled connections. `None` means use sqlx's
    /// default.
    pub pool_max_size: Option<u32>,
    /// Minimum number of pooled connections kept warm. `None` =
    /// driver default.
    pub pool_min_size: Option<u32>,
}

impl DatabaseSettings {
    /// Resolve the backend kind: explicit `self.backend` wins, else
    /// sniff the scheme from `self.url`. Returns `None` when neither
    /// is set. Output is normalized: `"postgres"` / `"mysql"` /
    /// `"sqlite"` (aliases like `"postgresql"`, `"mariadb"` collapse
    /// to canonical names).
    ///
    /// Use this in admin/template code that wants to branch on
    /// dialect at config-load time — the pool isn't always reachable
    /// from the rendering path, but Settings is.
    #[must_use]
    pub fn resolved_backend(&self) -> Option<&'static str> {
        if let Some(b) = self.backend.as_deref() {
            return Some(canonicalize_backend(b));
        }
        let url = self.url.as_deref()?;
        let scheme = url.split(':').next().unwrap_or("").to_ascii_lowercase();
        match scheme.as_str() {
            "postgres" | "postgresql" => Some("postgres"),
            "mysql" | "mariadb" => Some("mysql"),
            "sqlite" => Some("sqlite"),
            _ => None,
        }
    }
}

/// Normalize backend aliases to the canonical string. Returns the
/// input unchanged when the alias isn't recognized — caller decides
/// how to handle unknown backends.
fn canonicalize_backend(raw: &str) -> &'static str {
    match raw.to_ascii_lowercase().as_str() {
        "postgres" | "postgresql" | "pg" => "postgres",
        "mysql" | "mariadb" => "mysql",
        "sqlite" | "sqlite3" => "sqlite",
        _ => "postgres", // safest fallback — most existing deploys are PG
    }
}

/// Auto-admin tweaks read at boot. Mirrors the `admin::Builder`
/// flags so `Settings`-driven projects don't need to hand-wire them.
///
/// v0.36 expansion (#87 admin section) — branding + URL prefix +
/// session knobs that previously required imperative builder calls.
/// `admin::Builder::from_settings(pool, &Settings)` walks these
/// fields and applies each non-`None` value through the existing
/// builder methods; imperative overrides after `from_settings`
/// still win.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct AdminSettings {
    /// Tables visible in the admin. Empty / missing = every
    /// registered model.
    pub allowed_tables: Vec<String>,
    /// Tables whose mutating routes are blocked. Empty / missing =
    /// every table is read-write.
    pub read_only_tables: Vec<String>,

    // ---- v0.36 — branding + chrome (#87) ----------------------
    /// Title rendered in the sidebar + `<title>` tag. Falls through
    /// to `Settings.brand.name` then the framework default
    /// `"Rustango Admin"` when unset.
    pub title: Option<String>,
    /// Tagline rendered under the brand name in the sidebar.
    /// Falls through to `Settings.brand.tagline`.
    pub subtitle: Option<String>,
    /// Logo URL rendered next to the title. Falls through to
    /// `Settings.brand.logo_url`, then the embedded
    /// `/__static__/rustango.png`.
    pub logo_url: Option<String>,
    /// Hex-encoded accent color (e.g. `"#2c6fb0"`). Falls through to
    /// `Settings.brand.primary_color`. `manage check --deploy`
    /// validates the format.
    pub primary_color: Option<String>,
    /// `"auto"` (default), `"light"`, `"dark"`. Falls through to
    /// `Settings.brand.theme_mode`.
    pub theme_mode: Option<String>,
    /// Admin URL prefix. When set, overrides `Settings.routes.admin_url`
    /// and the framework default (`/admin` in friendly preset,
    /// `/__admin` in legacy). Useful for projects that want
    /// admin-section-only prefix overrides without flipping the
    /// whole route preset.
    pub url_prefix: Option<String>,

    // ---- v0.36 — deploy + session knobs (#87) -----------------
    /// `true` (default in prod) = CSRF cookie is `Secure` (HTTPS
    /// only). `false` is dev-only. `manage check --deploy` flags
    /// `false` in prod as an error.
    pub csrf_cookie_secure: Option<bool>,
    /// Admin session idle timeout in minutes. `None` = framework
    /// default (60 minutes today). 0 = no idle timeout (browser
    /// session only).
    pub session_timeout_minutes: Option<u32>,
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
    /// SMTP port. Defaults vary by TLS mode — 25 for `none`, 587 for
    /// `starttls`, 465 for `implicit`. Issue #48.
    pub smtp_port: Option<u16>,
    /// SMTP AUTH username. Set alongside `smtp_password` to enable
    /// PLAIN/LOGIN auth — both must be present, otherwise the
    /// transport connects anonymously. Issue #48.
    pub smtp_username: Option<String>,
    /// SMTP AUTH password. Prefer reading from an env var rather than
    /// committing to TOML — the config loader's env-overlay (see
    /// [`crate::config`]) makes `RUSTANGO_MAIL__SMTP_PASSWORD` Just
    /// Work. Issue #48.
    pub smtp_password: Option<String>,
    /// TLS mode: `"none"`, `"starttls"` (default — RFC 3207 upgrade
    /// on port 587), `"implicit"` (SMTPS — TLS from byte one on
    /// port 465). Unknown values fall back to `"starttls"` with a
    /// warning. Issue #48.
    pub smtp_tls: Option<String>,
    /// `From:` address for sent mail.
    pub from_address: Option<String>,
    /// Django-shape `ADMINS` — list of email addresses that
    /// `email::mail_admins(...)` sends to. Typically the project's
    /// site operators (the "5xx pages me at 3am" cohort). Issue #416.
    #[serde(default)]
    pub admins: Vec<String>,
    /// Django-shape `MANAGERS` — list of email addresses that
    /// `email::mail_managers(...)` sends to. Conventionally a
    /// broader-but-less-urgent ops list than `admins`. Issue #416.
    #[serde(default)]
    pub managers: Vec<String>,
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

/// Tracing-subscriber config — level + format + optional rolling
/// file sink (roadmap #8, v0.30.11). Drives
/// [`crate::logging::Setup::from_settings`] which is the same
/// builder users construct manually for ad-hoc setups; installing
/// via Settings + [`crate::manage::Cli::with_logging`] just
/// removes the boilerplate.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct LoggingSettings {
    /// `RUST_LOG`-style env filter applied when the actual
    /// `RUST_LOG` env var isn't set. Examples: `"info"`,
    /// `"info,sqlx=warn"`, `"debug,hyper=warn,h2=warn"`. Default
    /// (`None`) lets `logging::Setup::new()` choose `"info,sqlx=warn"`.
    pub level: Option<String>,
    /// Output format. Recognised values: `"pretty"` (default,
    /// human-friendly), `"json"` (production / log aggregators),
    /// `"compact"` (single-line, dev-friendly). Unknown values fall
    /// back to `pretty` with a `tracing::warn!`.
    pub format: Option<String>,
    /// Include thread IDs in events. Default off.
    pub with_thread_ids: Option<bool>,
    /// Include source-file line numbers in events. Default off.
    /// Useful in dev, noisy in prod.
    pub with_line_numbers: Option<bool>,
    /// Hide event targets (the module path) in pretty output.
    /// Default false (targets shown).
    pub without_targets: Option<bool>,
    /// When set, tee logs to a rolling file in this directory in
    /// addition to stdout. Created on first write. Required to
    /// activate the file sink — leave `None` to log to stdout only.
    pub file_dir: Option<String>,
    /// Filename prefix for the rolling file. Default `"app"`.
    /// Files land at `{file_dir}/{file_prefix}.YYYY-MM-DD` (or
    /// the equivalent for the chosen rotation).
    pub file_prefix: Option<String>,
    /// File rotation cadence: `"daily"` (default), `"hourly"`,
    /// `"minutely"`, `"never"`. Unknown values fall back to
    /// `daily` with a `tracing::warn!`.
    pub file_rotation: Option<String>,
    /// When `true` AND `file_dir` is set, drop the stdout layer so
    /// logs land in the file ONLY. Useful for headless workers /
    /// daemonized processes. No-op when `file_dir` is unset.
    pub file_only: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolved_backend_uses_explicit_value() {
        let mut s = DatabaseSettings::default();
        s.backend = Some("mysql".into());
        s.url = Some("postgres://x".into()); // mismatched, explicit wins
        assert_eq!(s.resolved_backend(), Some("mysql"));
    }

    #[test]
    fn resolved_backend_canonicalizes_aliases() {
        let s = DatabaseSettings {
            backend: Some("postgresql".into()),
            ..Default::default()
        };
        assert_eq!(s.resolved_backend(), Some("postgres"));
        let s = DatabaseSettings {
            backend: Some("mariadb".into()),
            ..Default::default()
        };
        assert_eq!(s.resolved_backend(), Some("mysql"));
    }

    #[test]
    fn resolved_backend_sniffs_from_url_scheme() {
        let s = DatabaseSettings {
            url: Some("sqlite::memory:".into()),
            ..Default::default()
        };
        assert_eq!(s.resolved_backend(), Some("sqlite"));
        let s = DatabaseSettings {
            url: Some("mysql://root@localhost/x".into()),
            ..Default::default()
        };
        assert_eq!(s.resolved_backend(), Some("mysql"));
        let s = DatabaseSettings {
            url: Some("postgresql://x".into()),
            ..Default::default()
        };
        assert_eq!(s.resolved_backend(), Some("postgres"));
    }

    #[test]
    fn resolved_backend_none_when_neither_set() {
        let s = DatabaseSettings::default();
        assert_eq!(s.resolved_backend(), None);
    }

    // v0.36 slice 7 — AdminSettings extended fields default to None
    // so projects upgrading from v0.35 keep their existing TOML.
    #[test]
    fn admin_settings_extended_fields_default_to_none() {
        let s = AdminSettings::default();
        assert!(s.title.is_none());
        assert!(s.subtitle.is_none());
        assert!(s.logo_url.is_none());
        assert!(s.primary_color.is_none());
        assert!(s.theme_mode.is_none());
        assert!(s.url_prefix.is_none());
        assert!(s.csrf_cookie_secure.is_none());
        assert!(s.session_timeout_minutes.is_none());
        assert!(s.allowed_tables.is_empty());
        assert!(s.read_only_tables.is_empty());
    }

    #[test]
    fn admin_settings_parses_full_section() {
        let toml = r##"
title = "Acme Admin"
subtitle = "Tenant management"
logo_url = "/assets/acme.png"
primary_color = "#2c6fb0"
theme_mode = "dark"
url_prefix = "/admin"
csrf_cookie_secure = true
session_timeout_minutes = 30
allowed_tables = ["post", "author"]
read_only_tables = ["audit_log"]
"##;
        let parsed: AdminSettings = toml::from_str(toml).expect("valid TOML");
        assert_eq!(parsed.title.as_deref(), Some("Acme Admin"));
        assert_eq!(parsed.subtitle.as_deref(), Some("Tenant management"));
        assert_eq!(parsed.logo_url.as_deref(), Some("/assets/acme.png"));
        assert_eq!(parsed.primary_color.as_deref(), Some("#2c6fb0"));
        assert_eq!(parsed.theme_mode.as_deref(), Some("dark"));
        assert_eq!(parsed.url_prefix.as_deref(), Some("/admin"));
        assert_eq!(parsed.csrf_cookie_secure, Some(true));
        assert_eq!(parsed.session_timeout_minutes, Some(30));
        assert_eq!(parsed.allowed_tables, vec!["post", "author"]);
        assert_eq!(parsed.read_only_tables, vec!["audit_log"]);
    }
}
