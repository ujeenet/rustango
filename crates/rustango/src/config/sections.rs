//! Typed section structs that the loader fills in.
//!
//! Every section is `#[serde(default)]` so missing TOML keys fall back
//! to their `Default::default()`. New fields can be added without
//! breaking older config files.

use serde::Deserialize;

/// Top-level config — every section is optional and defaults to its
/// type's `Default` impl. The loader fills sections from
/// `config/default.toml` + `config/{env}.toml` + env-var overrides.
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
