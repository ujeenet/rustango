//! Tenancy errors.
//!
//! Slice 1 ships a small surface — `Resolution`, `Validation`,
//! `Driver`. Later slices will grow the variants (e.g. `SecretsResolve`,
//! `PoolExhausted`, `MissingApex`).

use rustango::sql::sqlx;

use crate::secrets::SecretsError;

/// Errors raised while resolving, provisioning, or operating on
/// tenants.
#[derive(Debug, thiserror::Error)]
pub enum TenancyError {
    /// Per-request tenant resolution failed — no `Org` matched the
    /// request's host / path / header / port. The handler should
    /// usually surface this as 404.
    #[error("tenant resolution failed: {0}")]
    Resolution(String),

    /// User-supplied data (slug, host_pattern, database_url shape)
    /// is internally inconsistent or violates a uniqueness invariant.
    #[error("tenancy validation: {0}")]
    Validation(String),

    /// `Org.database_url` resolution via [`crate::SecretsResolver`]
    /// failed (vault outage, missing env var, malformed reference).
    /// `migrate_tenants` skips the affected tenant and logs; the
    /// resolver layer surfaces it as a hard error.
    #[error("secrets resolution failed: {0}")]
    Secrets(#[from] SecretsError),

    /// Migration runner errors (file I/O, JSON, validation, SQL)
    /// surfaced from `rustango_migrate::MigrateError` while running
    /// per-tenant migrations.
    #[error(transparent)]
    Migrate(#[from] rustango::migrate::MigrateError),

    /// Per-tenant query orchestration errors (compile/validation/SQL)
    /// surfaced from `rustango_sql::ExecError`.
    #[error(transparent)]
    Exec(#[from] rustango::sql::ExecError),

    /// SQL or pool-management failure (raw sqlx error).
    #[error(transparent)]
    Driver(#[from] sqlx::Error),
}
