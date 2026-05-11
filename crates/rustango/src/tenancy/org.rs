//! `Org` — the registry-level tenant record.
//!
//! Lives in the **registry database** (the one configured at boot via
//! `RUSTANGO_REGISTRY_URL`). Every other database the application
//! reaches is discovered through this table. Adding a tenant is
//! `INSERT INTO rustango_orgs (...)`, no config edit and no restart.
//!
//! ## Fields and what they mean
//!
//! | Field            | Purpose                                                     |
//! |------------------|-------------------------------------------------------------|
//! | `id`             | Server-assigned PK (`Auto<i64>` → `BIGSERIAL`).             |
//! | `slug`           | Globally-unique URL-safe handle (e.g. `"acme"`).            |
//! | `display_name`   | Human-friendly label for the admin UI.                      |
//! | `storage_mode`   | `"schema"` (in the registry DB) or `"database"` (dedicated).|
//! | `database_url`   | Secret reference (`vault://...`/`env://...`/literal URL).   |
//! | `schema_name`    | Per-tenant schema in `"schema"` mode (defaults to `slug`).  |
//! | `host_pattern`   | `Host` header to match (e.g. `"acme.app.com"`).             |
//! | `port`           | Incoming port to match for port-based routing.              |
//! | `path_prefix`    | URL path prefix (e.g. `"/acme"`) — opt-in, null by default. |
//! | `active`         | Soft-disable without dropping data.                         |
//! | `created_at`     | Set on insert.                                              |
//!
//! ## What's missing in Slice 1
//!
//! * `slug` carries `#[rustango(unique)]` so the DDL writer emits the
//!   `UNIQUE` constraint inline; the bootstrap migration uses a plain
//!   `CreateTable` op with no separate `DataOp`.
//! * `storage_mode` is a raw `String` because rustango models can't
//!   carry custom enums yet. The [`StorageMode`] helper enum in this
//!   module wraps the conversion.
//! * `database_url` carries plaintext for now. v0.5 Slice 3.5 adds the
//!   `SecretsResolver` indirection so the value can be a vault
//!   reference instead of a literal URL.

use crate::Model;

/// Registry record describing one tenant.
///
/// `#[derive(Model)]` registers it in the inventory, so any binary
/// linking `rustango-tenancy` automatically picks up the table when
/// `migrate::apply_all` walks the registry — or via
/// `make_migrations` against the registry DB.
#[derive(Model, Debug, Clone)]
#[rustango(table = "rustango_orgs", display = "slug", scope = "registry")]
#[allow(dead_code)] // Slices 2+ wire these fields into resolvers / pools / migrations.
pub struct Org {
    #[rustango(primary_key)]
    pub id: rustango::Auto<i64>,

    #[rustango(max_length = 64, unique)]
    pub slug: String,

    #[rustango(max_length = 255)]
    pub display_name: String,

    /// `"schema"` or `"database"`. See [`StorageMode`].
    #[rustango(max_length = 16)]
    pub storage_mode: String,

    /// Backend driver for this tenant. `"postgres"` (default),
    /// `"mysql"`, or `"sqlite"`. See [`BackendKind`]. v0.33
    /// constrains `storage_mode = "schema" ⟹ backend_kind = "postgres"`
    /// — see [`BackendKind::validate_storage_mode`].
    ///
    /// Defaults to `"postgres"` for orgs created before v0.33; the
    /// migration backfills the column with that value.
    #[rustango(max_length = 16, default = "'postgres'")]
    pub backend_kind: String,

    /// Secret reference resolved by `SecretsResolver` at pool-build
    /// time. For `storage_mode = "schema"` this is `None` (the
    /// registry pool is reused). For `"database"` it is the
    /// connection-URL reference (literal, env-var, or vault path).
    pub database_url: Option<String>,

    /// Schema name in `"schema"` mode. `None` means "use the slug".
    #[rustango(max_length = 64)]
    pub schema_name: Option<String>,

    /// `Host` header pattern (e.g. `"acme.app.com"`). The default
    /// resolver chain matches subdomain-first, so `manage
    /// create-tenant <slug>` populates this as `"<slug>.<apex>"`
    /// where `<apex>` comes from `RUSTANGO_APEX_DOMAIN`.
    #[rustango(max_length = 255)]
    pub host_pattern: Option<String>,

    /// Incoming port for port-based resolution. Niche — used for
    /// hard-isolated tenant ports in compliance scenarios.
    pub port: Option<i32>,

    /// URL path prefix (e.g. `"/acme"`). Opt-in, null by default —
    /// the headline routing mode is subdomain. Operators set this
    /// per-tenant when they need both modes (subdomain in prod,
    /// path in dev).
    #[rustango(max_length = 255)]
    pub path_prefix: Option<String>,

    /// Soft-disable. Inactive orgs return 404 from the resolver
    /// without dropping their data.
    pub active: bool,

    pub created_at: chrono::DateTime<chrono::Utc>,

    // ----- Branding (v0.26+).
    //
    // Per-tenant branding for the admin UI. All optional — when unset
    // the admin falls back to the static `Builder.title()/subtitle()`
    // defaults. Logo / favicon paths point inside the brand storage
    // root (configurable via `RUSTANGO_BRAND_STORAGE_DIR`); the
    // operator console serves them at `/__brand__/{slug}/{filename}`.
    /// Display name shown in the tenant admin sidebar header. When
    /// `None`, the admin falls back to `display_name`, then to the
    /// static admin title.
    #[rustango(max_length = 80)]
    pub brand_name: Option<String>,

    /// Optional tagline shown below the brand name.
    #[rustango(max_length = 200)]
    pub brand_tagline: Option<String>,

    /// Relative filename of the uploaded logo inside the per-org brand
    /// directory. Set by `POST /orgs/{slug}/edit/branding`. Read-only
    /// in the regular config form — uploads happen through the
    /// multipart sub-form.
    #[rustango(max_length = 120)]
    pub logo_path: Option<String>,

    /// Relative filename of the uploaded favicon. Same shape as
    /// `logo_path`.
    #[rustango(max_length = 120)]
    pub favicon_path: Option<String>,

    /// Hex primary color (e.g. `"#b04a2c"`). Maps to the
    /// `--color-accent` CSS variable; the branding module derives
    /// hover + soft-bg shades from it.
    #[rustango(max_length = 7)]
    pub primary_color: Option<String>,

    /// Theme mode — one of `"light"`, `"dark"`, `"auto"`. `None` is
    /// treated as `"auto"` (follow `prefers-color-scheme`).
    #[rustango(max_length = 8)]
    pub theme_mode: Option<String>,
}

/// Convenience enum for [`Org::storage_mode`]. The model field stays
/// a raw `String` because rustango doesn't yet carry custom enums in
/// the schema layer; this wrapper bridges to/from `&str`.
///
/// ```
/// use rustango::tenancy::StorageMode;
/// assert_eq!(StorageMode::Schema.as_str(), "schema");
/// assert_eq!(StorageMode::Database.as_str(), "database");
/// assert_eq!(StorageMode::parse("schema").unwrap(), StorageMode::Schema);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageMode {
    /// Tenant data lives in a dedicated schema in the **registry**
    /// Postgres database. Cheap (one connection pool for all
    /// tenants), good for small/medium tenant counts.
    Schema,
    /// Tenant data lives in a fully separate Postgres database
    /// (possibly on a different server). Strong isolation,
    /// per-tenant pools.
    Database,
}

impl StorageMode {
    /// Stable string form persisted to `Org.storage_mode`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Schema => "schema",
            Self::Database => "database",
        }
    }

    /// Parse the string form. Errors are returned as
    /// [`super::TenancyError::Validation`] by the caller — this
    /// returns a static `&str` so the error message can name the
    /// allowed values without an alloc.
    ///
    /// # Errors
    /// Returns `Err(unrecognized_value)` when `s` doesn't match a
    /// known variant — caller wraps in `TenancyError::Validation`.
    pub fn parse(s: &str) -> Result<Self, &str> {
        match s {
            "schema" => Ok(Self::Schema),
            "database" => Ok(Self::Database),
            other => Err(other),
        }
    }
}

impl core::fmt::Display for StorageMode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Backend driver for a tenant's storage. Mirrors [`StorageMode`]'s
/// string-on-the-row pattern — the column is a raw `String`, this
/// enum is the typed bridge.
///
/// **Storage-mode constraint:** `StorageMode::Schema` is only valid
/// for `BackendKind::Postgres`. MySQL "schema" ≡ "database" with
/// different transaction semantics; SQLite has no namespaces at all.
/// [`Self::validate_storage_mode`] enforces this — the admin's
/// org form, the `manage create-tenant` CLI, and
/// `migrate-tenant-storage` all call it before persisting.
///
/// ```
/// use rustango::tenancy::{BackendKind, StorageMode};
/// assert_eq!(BackendKind::Postgres.as_str(), "postgres");
/// assert!(BackendKind::Postgres.validate_storage_mode(StorageMode::Schema).is_ok());
/// assert!(BackendKind::Sqlite.validate_storage_mode(StorageMode::Schema).is_err());
/// assert!(BackendKind::MySql.validate_storage_mode(StorageMode::Database).is_ok());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackendKind {
    /// Postgres — the v0.5+ default. Supports schema-mode + database-mode.
    #[default]
    Postgres,
    /// MySQL — database-mode only (schema-mode rejected at validation time).
    MySql,
    /// SQLite — database-mode only. Each tenant gets its own file.
    Sqlite,
}

impl BackendKind {
    /// Stable string form persisted to `Org.backend_kind`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::MySql => "mysql",
            Self::Sqlite => "sqlite",
        }
    }

    /// Parse the string form.
    ///
    /// # Errors
    /// Returns `Err(unrecognized_value)` when `s` doesn't match a
    /// known variant — caller wraps in `TenancyError::Validation`.
    pub fn parse(s: &str) -> Result<Self, &str> {
        match s {
            "postgres" | "postgresql" | "pg" => Ok(Self::Postgres),
            "mysql" | "mariadb" => Ok(Self::MySql),
            "sqlite" | "sqlite3" => Ok(Self::Sqlite),
            other => Err(other),
        }
    }

    /// Validate the `(storage_mode, backend_kind)` pair. Returns
    /// `Err` with a human-readable explanation when the
    /// combination isn't supported.
    ///
    /// The only currently-disallowed combination is
    /// `Schema + (MySql | Sqlite)` — see the type-level comment on
    /// [`Self`] for rationale. All other pairs are permitted.
    ///
    /// # Errors
    /// `Err(&'static str)` on an unsupported combination.
    pub const fn validate_storage_mode(self, mode: StorageMode) -> Result<(), &'static str> {
        match (self, mode) {
            (Self::Postgres, _) => Ok(()),
            (_, StorageMode::Database) => Ok(()),
            (Self::MySql, StorageMode::Schema) => {
                Err("MySQL backend doesn't support schema-mode tenancy \
                 (use storage_mode=database)")
            }
            (Self::Sqlite, StorageMode::Schema) => {
                Err("SQLite backend doesn't support schema-mode tenancy \
                 (use storage_mode=database; each tenant gets its own file)")
            }
        }
    }
}

impl core::fmt::Display for BackendKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_aliases() {
        assert_eq!(
            BackendKind::parse("postgres").unwrap(),
            BackendKind::Postgres
        );
        assert_eq!(
            BackendKind::parse("postgresql").unwrap(),
            BackendKind::Postgres
        );
        assert_eq!(BackendKind::parse("pg").unwrap(), BackendKind::Postgres);
        assert_eq!(BackendKind::parse("mysql").unwrap(), BackendKind::MySql);
        assert_eq!(BackendKind::parse("mariadb").unwrap(), BackendKind::MySql);
        assert_eq!(BackendKind::parse("sqlite").unwrap(), BackendKind::Sqlite);
        assert_eq!(BackendKind::parse("sqlite3").unwrap(), BackendKind::Sqlite);
    }

    #[test]
    fn parse_rejects_unknown() {
        assert!(BackendKind::parse("oracle").is_err());
        assert!(BackendKind::parse("").is_err());
    }

    #[test]
    fn validate_storage_mode_postgres_accepts_both() {
        assert!(BackendKind::Postgres
            .validate_storage_mode(StorageMode::Schema)
            .is_ok());
        assert!(BackendKind::Postgres
            .validate_storage_mode(StorageMode::Database)
            .is_ok());
    }

    #[test]
    fn validate_storage_mode_mysql_rejects_schema() {
        let err = BackendKind::MySql
            .validate_storage_mode(StorageMode::Schema)
            .unwrap_err();
        assert!(err.contains("MySQL"));
    }

    #[test]
    fn validate_storage_mode_sqlite_rejects_schema() {
        let err = BackendKind::Sqlite
            .validate_storage_mode(StorageMode::Schema)
            .unwrap_err();
        assert!(err.contains("SQLite"));
    }

    #[test]
    fn non_postgres_accepts_database_mode() {
        assert!(BackendKind::MySql
            .validate_storage_mode(StorageMode::Database)
            .is_ok());
        assert!(BackendKind::Sqlite
            .validate_storage_mode(StorageMode::Database)
            .is_ok());
    }

    #[test]
    fn default_is_postgres() {
        assert_eq!(BackendKind::default(), BackendKind::Postgres);
    }
}
