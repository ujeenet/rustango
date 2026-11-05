//! Tenancy errors.
//!
//! Slice 1 ships a small surface — `Resolution`, `Validation`,
//! `Driver`. Later slices will grow the variants (e.g. `SecretsResolve`,
//! `PoolExhausted`, `MissingApex`).

use crate::sql::sqlx;

use super::secrets::SecretsError;

/// Errors raised while resolving, provisioning, or operating on
/// tenants.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
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

    /// `Org.database_url` resolution via [`super::SecretsResolver`]
    /// failed (vault outage, missing env var, malformed reference).
    /// `migrate_tenants` skips the affected tenant and logs; the
    /// resolver layer surfaces it as a hard error.
    #[error("secrets resolution failed: {0}")]
    Secrets(#[from] SecretsError),

    /// Migration runner errors (file I/O, JSON, validation, SQL)
    /// surfaced from `crate::migrate::MigrateError` while running
    /// per-tenant migrations.
    #[error(transparent)]
    Migrate(#[from] rustango::migrate::MigrateError),

    /// Per-tenant query orchestration errors (compile/validation/SQL)
    /// surfaced from `crate::sql::ExecError`.
    #[error(transparent)]
    Exec(#[from] rustango::sql::ExecError),

    /// SQL or pool-management failure (raw sqlx error).
    #[error(transparent)]
    Driver(#[from] sqlx::Error),

    /// I/O failure when writing user-facing output through the
    /// `manage` runner's writer (broken pipe etc.).
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// No password-hashing slot freed up in time. Answer 503.
    #[error("password hashing is busy")]
    Busy,

    /// Too many tenant pools are being built; see
    /// [`super::TenantPoolsConfig::max_pool_builds_per_minute`]. Answer 503.
    #[error("tenant pool builds are busy")]
    PoolBusy,
}

impl TenancyError {
    /// The 503 for [`Self::PoolBusy`], with `Retry-After`.
    pub(crate) fn pool_busy_response() -> axum::response::Response {
        use axum::response::IntoResponse as _;
        let mut resp =
            crate::api_errors::ApiError::service_unavailable("tenant is starting up, try again")
                .into_response();
        resp.headers_mut()
            .insert(axum::http::header::RETRY_AFTER, 1u64.into());
        resp
    }

    /// The text of an error the operator can act on; `None` for a cause to withhold (#2171).
    pub(crate) fn user_facing(&self) -> Option<String> {
        match self {
            // The variant's prefix is for logs, not for the operator.
            Self::Validation(m) => Some(m.clone()),
            Self::Busy | Self::PoolBusy => Some(self.to_string()),
            _ => None,
        }
    }
}
