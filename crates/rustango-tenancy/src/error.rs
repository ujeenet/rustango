//! Tenancy errors.
//!
//! Slice 1 ships a small surface — `Resolution`, `Validation`,
//! `Driver`. Later slices will grow the variants (e.g. `SecretsResolve`,
//! `PoolExhausted`, `MissingApex`).

use rustango::sql::sqlx;

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

    /// SQL or pool-management failure.
    #[error(transparent)]
    Driver(#[from] sqlx::Error),
}
