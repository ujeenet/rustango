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
//! * The `slug` `UNIQUE` constraint must be added by a hand-authored
//!   bootstrap migration (or via raw SQL in test setup) — the macro
//!   doesn't yet support `#[rustango(unique)]`. Until then, callers
//!   should validate uniqueness at the application layer before
//!   inserting.
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
#[rustango(table = "rustango_orgs", display = "slug")]
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
