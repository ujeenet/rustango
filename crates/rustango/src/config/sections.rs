//! Typed section structs that the loader fills in.
//!
//! Every section is `#[serde(default)]` so missing TOML keys fall back
//! to their `Default::default()`. New fields can be added without
//! breaking older config files.

use serde::Deserialize;

/// The whole config. Every section is optional and falls back to its
/// `Default`. The loader fills it from `config/default.toml`, then
/// `config/{env}_settings.toml`, then `RUSTANGO__*` env vars.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// `[database]`: connection URL, pool sizing, TLS.
    pub database: DatabaseSettings,

    /// `[secret_key]`: base64 HMAC key for session cookies. It can be
    /// a bare top-level string or a section; the loader accepts both.
    pub secret_key: Option<String>,

    /// `[admin]`: which tables the admin shows, and branding.
    pub admin: AdminSettings,

    /// `[tenancy]`: apex domain and secrets resolver style.
    pub tenancy: TenancySettings,

    /// `[cache]`: which cache backend to use.
    pub cache: CacheSettings,

    /// `[jobs]`: background job runner.
    pub jobs: JobsSettings,

    /// `[mail]`: mailer.
    pub mail: MailSettings,

    /// `[server]`: HTTP bind address and request timeout.
    pub server: ServerSettings,

    /// `[auth]`: JWT lifetimes, password hashing cost, lockout.
    pub auth: AuthSettings,

    /// `[brand]`: display strings for the admin and operator console.
    pub brand: BrandSettings,

    /// `[security]`: headers preset, CSP, allowed CORS origins.
    pub security: SecuritySettings,

    /// `[routes]`: URL prefixes for login, admin, audit, static files
    /// and the rest, so a project need not set them up in code.
    pub routes: RoutesSettings,

    /// `[audit]`: retention and redaction policy.
    pub audit: AuditSettings,

    /// `[logging]`: level filter, pretty or JSON output, optional
    /// rolling file. Unset fields keep the `logging::Setup` defaults.
    pub logging: LoggingSettings,

    /// `[i18n]`: default language, supported languages and locale
    /// paths. Builds a [`crate::i18n::Translator`] from TOML; see
    /// [`crate::i18n::Translator::from_settings`].
    pub i18n: I18nSettings,

    /// `[mcp]`: Model Context Protocol server. Does nothing unless
    /// the `mcp` feature is compiled in.
    pub mcp: McpSettings,
    /// `[sso]`: admin SSO for an app with no tenants. Does nothing
    /// unless the `admin-sso` feature is compiled in. Multi-tenant
    /// apps set SSO per `Org` instead.
    pub sso: SsoSettings,
}

impl Settings {
    /// The cargo features compiled into this build. Handy on version
    /// pages and in deploy audits, for example to spot that a prod
    /// binary lacks `oauth2` before the login button 500s. The list
    /// comes from `#[cfg(feature = …)]`, so it shows what is really
    /// linked, not what a `Cargo.toml` asks for.
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
        feat!("mcp");
        out
    }
}

/// `[mcp]`: Model Context Protocol server. Every field is optional;
/// the accessors below supply the defaults.
///
/// Example `config/default.toml`, showing each key at its default:
///
/// ```toml
/// [mcp]
/// # prefix = "/mcp"                 # URL prefix the MCP router mounts under
/// # token_ttl_secs = 900            # agent access-token lifetime (15 min)
/// # enable_sse = true               # serve the GET {prefix} SSE stream
/// # allowed_origins = []            # CORS allow-list (empty = same-origin only)
/// # rate_limit_per_minute = 0       # per-IP cap (0/unset = unlimited)
/// # max_tools_listed = 0            # tools/list page size (0/unset = unlimited)
/// ```
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct McpSettings {
    /// URL prefix the MCP router mounts under. Default `/mcp`.
    pub prefix: Option<String>,
    /// Agent access-token lifetime in seconds. Default 900 (15 min).
    pub token_ttl_secs: Option<i64>,
    /// Serve the `GET {prefix}` SSE notification stream. Default `true`.
    pub enable_sse: Option<bool>,
    /// CORS allow-list of origins for the MCP endpoint (empty = none).
    pub allowed_origins: Vec<String>,
    /// Per-IP request cap per minute (`None` = unlimited).
    pub rate_limit_per_minute: Option<u32>,
    /// Max tools returned by `tools/list` (`None` = unlimited).
    pub max_tools_listed: Option<usize>,
    /// Largest JSON-RPC request body, in bytes. Default 1 MiB. Raise
    /// it when a tool takes inline data, such as base64 uploads.
    pub max_body_bytes: Option<usize>,
}

impl McpSettings {
    /// Mount prefix, defaulting to `/mcp`.
    #[must_use]
    pub fn prefix(&self) -> &str {
        self.prefix.as_deref().unwrap_or("/mcp")
    }
    /// Token TTL in seconds, defaulting to 900 (15 minutes).
    #[must_use]
    pub fn token_ttl_secs(&self) -> i64 {
        self.token_ttl_secs.unwrap_or(900)
    }
    /// Whether the SSE notification stream is served (default `true`).
    #[must_use]
    pub fn sse_enabled(&self) -> bool {
        self.enable_sse.unwrap_or(true)
    }
    /// Request-body cap in bytes, defaulting to 1 MiB.
    #[must_use]
    pub fn max_body_bytes(&self) -> usize {
        self.max_body_bytes.unwrap_or(1024 * 1024)
    }
}

/// SSO for the admin login on an app with no tenants (`admin-sso`
/// feature). The admin `Builder` reads it at boot. The email the
/// provider returns must already belong to an admin user: SSO signs
/// people in but never creates accounts. Multi-tenant apps configure
/// SSO per `Org` instead.
///
/// ```toml
/// [sso]
/// # enabled = true
/// # provider = "google"      # google | microsoft | github | gitlab | discord | oidc
/// # issuer_url = ""          # required for provider = "oidc" (OIDC discovery base URL)
/// # client_id = ""
/// # client_secret = ""       # prefer the RUSTANGO__SSO__CLIENT_SECRET env overlay
/// # redirect_uri = ""        # must match the mounted /login/sso/{provider}/callback
/// ```
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct SsoSettings {
    /// Shows the "Sign in with …" button on the admin login page.
    pub enabled: Option<bool>,
    /// Which provider: `"google"`, `"microsoft"`, `"github"`,
    /// `"gitlab"`, `"discord"`, or `"oidc"` for any OpenID Connect
    /// provider named by `issuer_url`.
    pub provider: Option<String>,
    /// OIDC issuer base URL, used with `provider = "oidc"`.
    pub issuer_url: Option<String>,
    /// OAuth2 client id from the provider.
    pub client_id: Option<String>,
    /// OAuth2 client secret. Set it through
    /// `RUSTANGO__SSO__CLIENT_SECRET` rather than committing it.
    pub client_secret: Option<String>,
    /// Redirect URI registered with the provider. It must match the
    /// mounted callback route. When unset, the admin builds it from
    /// the request host and the login prefix.
    pub redirect_uri: Option<String>,
}

impl SsoSettings {
    /// Whether SSO login is enabled (default `false`).
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled.unwrap_or(false)
    }
    /// Provider key, if configured.
    #[must_use]
    pub fn provider(&self) -> Option<&str> {
        self.provider.as_deref()
    }
}

/// Connection URL + pool sizing for the primary database.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct DatabaseSettings {
    /// Connection URL. Needed at runtime, but the loader does not
    /// require it here, so a config can leave it to
    /// `RUSTANGO__DATABASE__URL`. Schemes: `postgres://`, `mysql://`,
    /// `sqlite:`. [`crate::sql::Pool::connect`] picks the driver from
    /// the scheme.
    pub url: Option<String>,
    /// Which backend this deploy expects: `"postgres"`, `"mysql"` or
    /// `"sqlite"`. Optional. When unset,
    /// [`DatabaseSettings::resolved_backend`] reads it from the URL
    /// scheme, so admin and template code can branch on dialect
    /// without holding a [`crate::sql::Pool`].
    ///
    /// This is an assertion, not an override: sqlx still uses the
    /// backend the URL names. `manage check --deploy` reports a value
    /// that disagrees with the URL.
    pub backend: Option<String>,
    /// Largest number of pooled connections. `None` uses sqlx's
    /// default of 10, which is usually too small for a web server and
    /// far more than a SQLite file needs.
    pub pool_max_size: Option<u32>,
    /// How many connections stay open when idle. `None` means 0, so
    /// the first request after a quiet spell pays to connect.
    pub pool_min_size: Option<u32>,
    /// Seconds to wait for a connection before erroring, covering
    /// both dialling a new one and queueing for a free one. `None`
    /// uses rustango's 5s, tighter than sqlx's 30s on purpose: on a
    /// request path, 30s lets one unreachable database tie up a
    /// worker for half a minute per request.
    pub pool_acquire_timeout_secs: Option<u64>,
    /// Seconds a connection may sit idle before it is closed. Guards
    /// against a load balancer or server timeout cutting it from the
    /// other end. `None` keeps sqlx's default.
    pub pool_idle_timeout_secs: Option<u64>,
    /// Seconds a connection may live, used or not. This matters after
    /// a failover or a credential rotation: without it a pool can
    /// hold connections to the old server, or with revoked
    /// credentials. `None` keeps sqlx's default.
    pub pool_max_lifetime_secs: Option<u64>,
}

impl DatabaseSettings {
    /// This section as pool tuning, for [`crate::sql::configure_pools`].
    ///
    /// Kept next to the fields it reads. Adding a knob above and
    /// forgetting to forward it here is how a setting ends up parsed,
    /// tested, and applied to nothing.
    #[must_use]
    pub fn pool_tuning(&self) -> crate::sql::PoolTuning {
        use std::time::Duration;
        crate::sql::PoolTuning {
            max_connections: self.pool_max_size,
            min_connections: self.pool_min_size,
            acquire_timeout: self.pool_acquire_timeout_secs.map(Duration::from_secs),
            idle_timeout: self.pool_idle_timeout_secs.map(Duration::from_secs),
            max_lifetime: self.pool_max_lifetime_secs.map(Duration::from_secs),
        }
    }
}

impl DatabaseSettings {
    /// Work out the backend: `self.backend` if set, else the scheme
    /// of `self.url`. `None` when neither is given. The result is
    /// always `"postgres"`, `"mysql"` or `"sqlite"`; aliases such as
    /// `"postgresql"` and `"mariadb"` collapse into those.
    ///
    /// Use it in admin or template code that must branch on dialect.
    /// The pool is not always reachable from a render, but the
    /// settings are.
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

/// Map a backend alias to its canonical name.
fn canonicalize_backend(raw: &str) -> &'static str {
    match raw.to_ascii_lowercase().as_str() {
        "postgres" | "postgresql" | "pg" => "postgres",
        "mysql" | "mariadb" => "mysql",
        "sqlite" | "sqlite3" => "sqlite",
        _ => "postgres", // safest guess: most deploys are PG
    }
}

/// Admin settings read at boot. They mirror the `admin::Builder`
/// methods, so a config-driven project need not call them by hand.
/// `admin::Builder::from_settings(pool, &Settings)` applies every
/// field that is not `None`. Builder calls made after that still win.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct AdminSettings {
    /// Tables the admin shows. Empty means every registered model.
    pub allowed_tables: Vec<String>,
    /// Tables whose write routes are blocked. Empty means all tables
    /// are read-write.
    pub read_only_tables: Vec<String>,

    // ---- branding and chrome ---------------------------------
    /// Title in the sidebar and the `<title>` tag. Falls back to
    /// `Settings.brand.name`, then `"Rustango Admin"`.
    pub title: Option<String>,
    /// Tagline under the brand name. Falls back to
    /// `Settings.brand.tagline`.
    pub subtitle: Option<String>,
    /// Logo URL next to the title. Falls back to
    /// `Settings.brand.logo_url`, then the built-in asset.
    pub logo_url: Option<String>,
    /// Accent color in hex, such as `"#2c6fb0"`. Falls back to
    /// `Settings.brand.primary_color`. `manage check --deploy`
    /// checks the format.
    pub primary_color: Option<String>,
    /// `"auto"` (default), `"light"` or `"dark"`. Falls back to
    /// `Settings.brand.theme_mode`.
    pub theme_mode: Option<String>,
    /// Admin URL prefix. Overrides `Settings.routes.admin_url` and
    /// the default, so you can move the admin without changing the
    /// whole route preset.
    pub url_prefix: Option<String>,

    // ---- deploy and session ----------------------------------
    /// `true` marks the CSRF cookie `Secure`, so it is sent over
    /// HTTPS only. `false` is for dev; `manage check --deploy`
    /// reports it as an error in prod.
    pub csrf_cookie_secure: Option<bool>,
    /// Idle timeout for an admin session, in minutes. `None` uses
    /// the default of 60. `0` means no idle timeout, so the session
    /// lasts as long as the browser keeps it.
    pub session_timeout_minutes: Option<u32>,
}

/// Host-wide tenancy settings. Per-tenant resolver config lives on
/// the `Org` row in the registry.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct TenancySettings {
    /// Apex domain for subdomain-based tenant resolution. Mirrors the
    /// `RUSTANGO_APEX_DOMAIN` env var, which still works as a
    /// fallback.
    pub apex_domain: Option<String>,
}

/// Which cache backend to build.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct CacheSettings {
    /// One of `"memory"` (the default), `"null"` / `"none"`,
    /// `"file"`, `"redis"`, or `"db"` / `"database"`. Note that the
    /// database backend is `"db"`, not `"postgres"`.
    ///
    /// [`cache::from_settings`](crate::cache::from_settings) is sync
    /// and cannot build `"redis"` or `"db"`; it panics rather than
    /// swap in another backend. Use `cache::from_settings_async` for
    /// redis, and build the DB backend where the `Pool` is.
    pub backend: Option<String>,
    /// Redis connection URL when `backend = "redis"`.
    pub redis_url: Option<String>,
    /// Directory the `"file"` backend stores entries in, one file per
    /// key. If
    /// `backend = "file"` but this is unset, the resolver warns and
    /// uses `InMemoryCache` so boot is not blocked.
    pub file_cache_dir: Option<std::path::PathBuf>,
}

/// Background job runner.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct JobsSettings {
    /// Which queue to use. Only `jobs::inmemory_from_settings` reads
    /// this, and it builds an in-memory queue whatever the value, so
    /// anything but `"memory"` only earns a warning. Wire another
    /// backend yourself; see [`crate::jobs`].
    pub backend: Option<String>,
    /// How many jobs run at once. `None` means one at a time.
    pub concurrency: Option<u32>,
}

/// Mailer.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct MailSettings {
    /// `"smtp"`, `"console"` (the dev default) or `"memory"` (tests).
    pub backend: Option<String>,
    /// SMTP host. Required when `backend = "smtp"`.
    pub smtp_host: Option<String>,
    /// SMTP port. The default follows the TLS mode: 25 for `none`,
    /// 587 for `starttls`, 465 for `implicit`.
    pub smtp_port: Option<u16>,
    /// SMTP username. Set it with `smtp_password` to use PLAIN or
    /// LOGIN auth. Without both, the transport connects anonymously.
    pub smtp_username: Option<String>,
    /// SMTP password. Set it through an env var such as
    /// `RUSTANGO_MAIL__SMTP_PASSWORD` rather than committing it.
    pub smtp_password: Option<String>,
    /// TLS mode: `"none"`, `"starttls"` (the default, an upgrade on
    /// port 587) or `"implicit"` (TLS from the first byte, port 465).
    /// An unknown value warns and uses `"starttls"`.
    pub smtp_tls: Option<String>,
    /// SMTP connection timeout in seconds.
    /// `None` leaves lettre with no timeout. Set it in production: a
    /// stuck relay otherwise holds request workers for minutes.
    #[serde(default)]
    pub smtp_timeout_secs: Option<u64>,
    /// The `From:` address on normal outgoing mail.
    /// `default_from_email` is the other name for this field and
    /// wins when both are set.
    pub from_address: Option<String>,
    /// The `From:` address on mail the server generates, such as
    /// `mail_admins`. Falls back to
    /// `from_address`. Set it to send ops mail from its own address.
    #[serde(default)]
    pub server_email: Option<String>,
    /// Text put in front of subjects sent by `mail_admins` and
    /// `mail_managers`. Empty by
    /// default. Use something like `"[Acme] "`, with the space.
    #[serde(default)]
    pub email_subject_prefix: Option<String>,
    /// Addresses `email::mail_admins` writes to — usually the people
    /// paged for a 5xx.
    #[serde(default)]
    pub admins: Vec<String>,
    /// Addresses `email::mail_managers` writes to — a wider, less
    /// urgent list than `admins`.
    #[serde(default)]
    pub managers: Vec<String>,
    /// Directory the `"file"` mail backend writes `.eml` files to
    /// instead of sending. If
    /// `backend = "file"` but this is unset, the resolver warns and
    /// uses `ConsoleMailer`.
    pub file_email_dir: Option<std::path::PathBuf>,
}

/// HTTP bind address and request timeout.
///
/// `RUSTANGO_BIND` is still read when `bind` is unset. Set both while
/// migrating to check the new path picks the same value.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct ServerSettings {
    /// Listener address. `"127.0.0.1:8080"` in dev and
    /// `"0.0.0.0:8080"` in prod. The prod value is reachable from
    /// outside, so put a reverse proxy in front of it.
    pub bind: Option<String>,
    /// Handler timeout in seconds. `None` means no timeout. Set it
    /// to about 30 in production so one stuck handler cannot hold a
    /// worker.
    pub request_timeout_secs: Option<u64>,
    /// Largest body accepted on POST, PUT and PATCH. `None` means
    /// 2 MiB. Raise it for upload routes.
    pub max_body_bytes: Option<u64>,
}

/// Authentication: JWT lifetimes, password hashing cost and account
/// lockout. Every default matches what the framework already did, so
/// an upgrade changes nothing on its own.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct AuthSettings {
    /// JWT lifetimes. The field names match
    /// `rustango::tenancy::auth_routes::Config`, so this section can
    /// go straight to `auth_routes::jwt_router(...)`.
    pub jwt: JwtSettings,
    /// Argon2id memory cost in KiB. Default `19456`, about 19 MiB,
    /// which is the OWASP floor. Less memory means faster logins and
    /// weaker resistance to brute force. Stay at or above 15 MiB.
    pub argon2_memory_kib: Option<u32>,
    /// Argon2id iteration count. Default `2`, which OWASP pairs with
    /// 19456 KiB of memory.
    pub argon2_iterations: Option<u32>,
    /// Argon2id lanes. Default `1`. On a busy server one lane is
    /// fastest overall, since extra lanes only move work between
    /// cores.
    pub argon2_parallelism: Option<u32>,
    /// Failed-login attempts before lockout. Default `5`.
    pub lockout_threshold: Option<u32>,
    /// Lockout duration in seconds. Default `900` (15 min).
    pub lockout_duration_secs: Option<u64>,
}

/// JWT lifetimes. The defaults match
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

/// Display strings for the operator console and tenant admin. These
/// also come from `RUSTANGO_OPERATOR_*` env vars; putting them in
/// TOML makes per-tier branding easier, such as a different logo in
/// staging. Per-tenant branding stays on the `Org` row.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct BrandSettings {
    /// Name shown in the operator console and admin, such as
    /// `"Acme Operator"`. Default `"Rustango"`.
    pub name: Option<String>,
    /// Tagline under the brand name.
    pub tagline: Option<String>,
    /// Logo URL for the operator console. Defaults to the built-in
    /// asset.
    pub logo_url: Option<String>,
    /// Accent color in hex, such as `"#2c6fb0"`. The theme uses it
    /// to tint primary buttons and links.
    pub primary_color: Option<String>,
    /// Starting theme: `"auto"` (default), `"light"` or `"dark"`.
    /// The tenant admin reads it too, but `Org.theme_mode` wins.
    pub theme_mode: Option<String>,
}

/// Security headers, CSP and CORS. The defaults match
/// `SecurityHeadersLayer::strict()`. Override single fields in dev or
/// staging, for example to allow inline scripts locally.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct SecuritySettings {
    /// `"strict"` (default), `"relaxed"`, `"dev"`, `"none"`. Picks
    /// the [`SecurityHeadersLayer`](crate::security_headers::SecurityHeadersLayer)
    /// preset.
    pub headers_preset: Option<String>,
    /// Content-Security-Policy header. `None` sends no CSP. You can
    /// mount a stricter one on the admin router alone.
    pub csp: Option<String>,
    /// HSTS `max-age` in seconds. Default `31536000`, one year. `0`
    /// turns HSTS off, which helps in dev when switching between
    /// http and https.
    pub hsts_max_age_secs: Option<u64>,
    /// Origins allowed by CORS. Empty means no CORS layer. `["*"]`
    /// allows everything, though browsers reject it with
    /// credentials.
    pub cors_allowed_origins: Vec<String>,
    /// Allowed Host headers, enforced by
    /// [`crate::host_validation::AllowedHostsLayer`]. An entry is a
    /// hostname, a `.example.com` wildcard, or `*`. Empty turns the
    /// layer off, which `manage check --deploy` warns about in prod.
    pub allowed_hosts: Vec<String>,
    /// Extra origins that pass the CSRF Origin check, on top of
    /// same-host requests. Each
    /// entry is scheme plus host, such as `"https://app.example.com"`
    /// or `"https://*.example.com"`. Empty skips the Origin check.
    /// Used by
    /// [`crate::forms::csrf::CsrfConfig::with_trusted_origins`].
    pub csrf_trusted_origins: Vec<String>,
    /// `true` mounts [`crate::ssl_redirect::SslRedirectLayer`], which
    /// redirects plain HTTP to HTTPS. Default `false`.
    pub secure_ssl_redirect: Option<bool>,
    /// Path prefixes that skip the SSL redirect even when
    /// [`Self::secure_ssl_redirect`] is on. Useful for health checks
    /// that arrive over plain HTTP.
    pub secure_redirect_exempt: Vec<String>,
    /// The header name and value a reverse proxy sets to say the
    /// original request was HTTPS. It feeds
    /// [`crate::ssl_redirect::SslRedirectLayer::proxy_ssl_header`],
    /// so there is no redirect loop behind a TLS-terminating load
    /// balancer, and the real-IP and Host checks. Must hold exactly
    /// two entries; otherwise it is ignored.
    pub secure_proxy_ssl_header: Vec<String>,
    /// `true` marks the framework's auth cookies `Secure`, so
    /// browsers send them over HTTPS only. `None`
    /// counts as `true`. Set `false` in `dev_settings.toml` for local
    /// HTTP; `manage check --deploy` reports `false` in prod.
    pub secure_cookies: Option<bool>,
}

/// URL prefixes for the framework's built-in routes, so the admin
/// path can move without a code change.
///
/// The defaults are `/login`, `/admin`, `/audit`, `/_static`,
/// `/_brand` and `/_impersonation_handoff`. Set
/// `legacy_preset = true` to switch to the `__`-prefixed names
/// without listing each field.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct RoutesSettings {
    /// `true` applies `RouteConfig::legacy()`, the `__`-prefixed
    /// names, before the per-field overrides below. Default `false`.
    pub legacy_preset: Option<bool>,
    /// `/login` (default) or `/__login` (legacy).
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
    /// `/_impersonation_handoff` or `/__impersonation_handoff`.
    pub impersonation_handoff_url: Option<String>,
}

/// How long audit rows are kept, and what is redacted.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct AuditSettings {
    /// Retention in days. `None` keeps rows forever; you can still
    /// run `audit-cleanup --days <N>` from cron. Most deployments
    /// set 90 to 365, depending on their rules.
    pub retention_days: Option<u32>,
    /// More query-parameter names to redact in access logs. The
    /// built-in list already covers `password`, `token`, `secret`,
    /// `api_key`, `access_token`, `refresh_token` and `signature`.
    pub redact_query_params: Vec<String>,
}

/// Logging: level, format and an optional rolling file. Feeds
/// [`crate::logging::Setup::from_settings`], the same builder you
/// would otherwise write by hand.
///
/// # Building one in code
///
/// Start from the default and assign; the fields are `pub`:
///
/// ```ignore
/// let mut logging = rustango::config::LoggingSettings::default();
/// logging.level = Some("debug".into());
/// ```
///
/// Do **not** write `LoggingSettings { level, ..Default::default() }`.
/// The struct is `#[non_exhaustive]`, which forbids every struct
/// expression outside this crate, including that one. It is marked
/// that way so a new field can never break your code.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
#[non_exhaustive]
pub struct LoggingSettings {
    /// A `RUST_LOG`-style filter, used when the `RUST_LOG` env var
    /// itself is unset. For example `"info"`, `"info,sqlx=warn"` or
    /// `"debug,hyper=warn,h2=warn"`. `None` gives `"info,sqlx=warn"`.
    pub level: Option<String>,
    /// Output format: `"full"` (default, one line), `"pretty"`
    /// (several lines, one field each), `"compact"` (a shorter
    /// single line) or `"json"` for log aggregators. An unknown
    /// value warns and uses `full`.
    pub format: Option<String>,
    /// Terminal colour: `"auto"` (default), `"always"` or `"never"`.
    ///
    /// `auto` colours only when stdout is a terminal, so piping to a
    /// file or running in CI stays plain with no extra config. The
    /// file sink and JSON output are never coloured.
    pub color: Option<String>,
    /// Include thread IDs in events. Default off.
    pub with_thread_ids: Option<bool>,
    /// Include source line numbers. Default off: handy in dev, noisy
    /// in prod.
    pub with_line_numbers: Option<bool>,
    /// Hide the module path on each event in pretty output. Default
    /// `false`, so paths are shown.
    pub without_targets: Option<bool>,
    /// Also write logs to a rolling file in this directory. The
    /// directory is created on first write. Leave it `None` to log
    /// to stdout only.
    pub file_dir: Option<String>,
    /// Filename prefix for the rolling file. Default `"app"`, giving
    /// `{file_dir}/{file_prefix}.YYYY-MM-DD` or the equivalent for
    /// the chosen rotation.
    pub file_prefix: Option<String>,
    /// How often to rotate: `"daily"` (default), `"hourly"`,
    /// `"minutely"` or `"never"`. An unknown value warns and uses
    /// `daily`.
    pub file_rotation: Option<String>,
    /// With `file_dir` set, `true` drops the stdout layer so logs go
    /// only to the file. Good for headless workers. Does nothing
    /// when `file_dir` is unset.
    pub file_only: Option<bool>,
    /// One INFO line per request, plus a span that carries `tenant`
    /// into every event a handler emits. Default `true`.
    ///
    /// Set `false` when something at the edge already logs requests,
    /// or when the traffic makes per-request lines too costly.
    pub access_log: Option<bool>,
}

/// `[i18n]`: the default language, the supported languages and the
/// locale paths. [`crate::i18n::Translator::from_settings`] builds
/// a `Translator` from them, so locale wiring stays out of
/// `src/main.rs`.
///
/// Example `config/default.toml`:
///
/// ```toml
/// [i18n]
/// default_locale = "en"
/// languages = ["en", "fr", "es"]
/// locale_paths = ["locales", "vendor/locales"]
/// fallback_chain = ["en"]
/// ```
///
/// An absent `[i18n]` section means no i18n is configured;
/// `Translator::new(Locale::new("en"))` still works at runtime.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct I18nSettings {
    /// The locale used when nothing else picks one: no URL prefix,
    /// cookie or Accept-Language match. `None` means `"en"`.
    pub default_locale: Option<String>,

    /// The locales the project supports.
    /// `LocaleMiddleware` treats this as the allowlist when reading
    /// Accept-Language. An empty list activates every catalog found
    /// under `locale_paths`.
    pub languages: Vec<String>,

    /// Directories holding catalog files at `<dir>/<lang>.json`.
    /// They are searched in order, so a
    /// later path can shadow an earlier one for the same key. An
    /// empty list skips the loader, which suits apps that call
    /// `add_locale` themselves.
    pub locale_paths: Vec<String>,

    /// Locales to try, in order, when a key is in neither the
    /// requested locale's catalog nor its base language. Use it when
    /// `default_locale` alone is too blunt, such as
    /// "fr-CA, then fr, then pt, then en".
    pub fallback_chain: Vec<String>,
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

    // The optional AdminSettings fields default to None, so an
    // existing TOML file keeps working after an upgrade.
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
