//! Dialect-agnostic database pool wrapper.
//!
//! Existing rustango code talks directly to `sqlx::PgPool`. The
//! v0.23.0 series introduces this `Pool` wrapper so callers can
//! reach either Postgres or `MySQL` through the same handle, with
//! the right [`Dialect`] dispatch baked in.
//!
//! ## Backwards compatibility
//!
//! Every existing `&PgPool` API in the framework keeps working. The
//! `Pool` wrapper is *additive* — new code can take `&Pool` and get
//! cross-dialect support; legacy code that takes `&PgPool` still
//! does and is migrated module-by-module in subsequent batches.
//!
//! Construct a `Pool` from a `PgPool` you already have:
//!
//! ```ignore
//! let pg: sqlx::PgPool = sqlx::PgPool::connect(&url).await?;
//! let pool: rustango::sql::Pool = pg.into();
//! ```
//!
//! Or let the wrapper build it for you:
//!
//! ```ignore
//! use rustango::sql::Pool;
//!
//! // Scheme-sniffed (postgres:// or mysql://):
//! let pool = Pool::connect("postgres://user:pass@host/db").await?;
//!
//! // Or assembled from env vars (DATABASE_URL OR DB_HOST/DB_USER/...):
//! let pool = Pool::connect_from_env().await?;
//! ```
//!
//! ## Dialect dispatch
//!
//! ```ignore
//! let dialect: &dyn rustango::sql::Dialect = pool.dialect();
//! tracing::info!(name = dialect.name(), "started against backend");
//! ```
//!
//! ## `MySQL` status
//!
//! The `mysql` Cargo feature is wired in v0.23.0-batch1 but the
//! `MySqlDialect` impl lands in batch2. Connecting with a
//! `mysql://` URL in batch1 returns
//! [`PoolError::MysqlNotYetImplemented`] — a soft, clear error so
//! apps building against `mysql` ahead of batch2 get an actionable
//! message instead of a panic.

use std::time::Duration;

use crate::env::{database_url_from_env, EnvError};

use super::Dialect;

#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    /// `sqlx` couldn't connect (auth, host unreachable, schema bad…).
    #[error("connect: {0}")]
    Connect(String),

    /// URL didn't start with a recognized scheme (`postgres://`,
    /// `postgresql://`, or `mysql://`).
    #[error("unsupported scheme in URL `{0}` — expected postgres:// or mysql://")]
    UnsupportedScheme(String),

    /// `mysql` feature is enabled but the `MySqlDialect` lands in
    /// rustango v0.23.0-batch2; this version returns a clear error
    /// rather than panicking. (Batch 1 ships the connection plumbing
    /// only.)
    #[error(
        "MySQL connections require rustango ≥ v0.23.0-batch2 (the MySqlDialect impl); \
         this build is on v0.23.0-batch1. Switch to a postgres:// URL or wait for batch 2."
    )]
    MysqlNotYetImplemented,

    /// Tried to construct a Pool with a backend whose Cargo feature
    /// isn't enabled (e.g. `mysql://` URL with `default-features = false`
    /// and no `mysql` feature added).
    #[error(
        "URL scheme `{scheme}` requires the `{feature}` Cargo feature on rustango \
         — add `features = [\"{feature}\"]` to your dependency"
    )]
    FeatureNotEnabled {
        scheme: &'static str,
        feature: &'static str,
    },

    #[error(transparent)]
    Env(#[from] EnvError),
}

/// Cheap-to-clone wrapper around either a `PgPool` or a `MySqlPool`.
/// `Arc`-wrapping is handled by `sqlx` itself — cloning a Pool is
/// cloning the underlying `Arc`.
#[derive(Clone)]
pub enum Pool {
    #[cfg(feature = "postgres")]
    Postgres(sqlx::PgPool),
    // MySql variant added in v0.23.0-batch2 alongside MySqlDialect.
    // Leaving it out of the enum until the dialect impl lands keeps
    // every checkpoint a working state.
}

impl Pool {
    /// Connect to a database from a URL. Recognized schemes:
    ///
    /// - `postgres://` (alias `postgresql://`) — requires the
    ///   `postgres` feature (default).
    /// - `mysql://` — requires the `mysql` feature; returns
    ///   [`PoolError::MysqlNotYetImplemented`] in batch1, full
    ///   support in batch2.
    ///
    /// # Errors
    ///
    /// - [`PoolError::UnsupportedScheme`] — URL didn't start with a
    ///   recognized scheme.
    /// - [`PoolError::FeatureNotEnabled`] — scheme is recognized but
    ///   the corresponding Cargo feature wasn't enabled at build time.
    /// - [`PoolError::Connect`] — sqlx couldn't reach the database.
    /// - [`PoolError::MysqlNotYetImplemented`] — see error variant.
    pub async fn connect(url: &str) -> Result<Self, PoolError> {
        let scheme = url
            .split("://")
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        match scheme.as_str() {
            "postgres" | "postgresql" => Self::connect_postgres_inner(url).await,
            "mysql" => Self::connect_mysql_inner(url).await,
            _ => Err(PoolError::UnsupportedScheme(url.to_owned())),
        }
    }

    /// Same as [`Self::connect`] but with a connection timeout.
    /// Default timeout when callers use [`Self::connect`] is whatever
    /// `sqlx::PoolOptions` defaults to (currently 30s).
    ///
    /// # Errors
    /// Same set as [`Self::connect`], plus a `Connect` error if `sqlx`
    /// times out before the database accepts the connection.
    pub async fn connect_with_timeout(url: &str, timeout: Duration) -> Result<Self, PoolError> {
        let scheme = url
            .split("://")
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        match scheme.as_str() {
            #[cfg(feature = "postgres")]
            "postgres" | "postgresql" => {
                let pool = sqlx::postgres::PgPoolOptions::new()
                    .acquire_timeout(timeout)
                    .connect(url)
                    .await
                    .map_err(|e| PoolError::Connect(e.to_string()))?;
                Ok(Self::Postgres(pool))
            }
            #[cfg(not(feature = "postgres"))]
            "postgres" | "postgresql" => Err(PoolError::FeatureNotEnabled {
                scheme: "postgres",
                feature: "postgres",
            }),
            "mysql" => Self::connect_mysql_inner(url).await,
            _ => Err(PoolError::UnsupportedScheme(url.to_owned())),
        }
    }

    /// Read connection details from environment variables and connect.
    /// See [`crate::env::database_url_from_env`] for the var resolution
    /// order (`DATABASE_URL` takes precedence; otherwise assembled from
    /// `DB_DRIVER` / `DB_HOST` / `DB_PORT` / `DB_USER` / `DB_PASSWORD`
    /// / `DB_NAME` / `DB_PARAMS`).
    ///
    /// # Errors
    /// [`PoolError::Env`] when the env-var pass fails (missing required
    /// vars or unsupported driver), plus the same set as [`Self::connect`].
    pub async fn connect_from_env() -> Result<Self, PoolError> {
        let url = database_url_from_env()?;
        Self::connect(&url).await
    }

    /// Borrow the dialect for this pool. Stable [`Dialect`] reference
    /// usable by callers who need to inspect identifier quoting,
    /// placeholder syntax, etc., without caring which backend the
    /// pool actually wraps.
    #[must_use]
    pub fn dialect(&self) -> &'static dyn Dialect {
        match self {
            #[cfg(feature = "postgres")]
            Pool::Postgres(_) => super::postgres::DIALECT,
        }
    }

    /// Short identifier for the active backend — `"postgres"` or
    /// `"mysql"`. Convenience for logs and `manage` output; same as
    /// `pool.dialect().name()`.
    #[must_use]
    pub fn backend_name(&self) -> &'static str {
        self.dialect().name()
    }

    /// Borrow as a `PgPool` for callers (and existing code paths)
    /// that expect Postgres specifically. Returns `None` when the
    /// pool wraps a non-Postgres backend.
    ///
    /// During batch 1 → batch 5 of v0.23.0 most legacy `&PgPool`
    /// code paths use this to convert at the boundary; the goal is
    /// to flip them to `&Pool` directly in batch 5.
    #[must_use]
    #[cfg(feature = "postgres")]
    pub fn as_postgres(&self) -> Option<&sqlx::PgPool> {
        match self {
            Pool::Postgres(p) => Some(p),
        }
    }

    // ---- internal connect helpers ----

    #[cfg(feature = "postgres")]
    async fn connect_postgres_inner(url: &str) -> Result<Self, PoolError> {
        let pool = sqlx::PgPool::connect(url)
            .await
            .map_err(|e| PoolError::Connect(e.to_string()))?;
        Ok(Self::Postgres(pool))
    }

    #[cfg(not(feature = "postgres"))]
    async fn connect_postgres_inner(_url: &str) -> Result<Self, PoolError> {
        Err(PoolError::FeatureNotEnabled {
            scheme: "postgres",
            feature: "postgres",
        })
    }

    // Both branches must stay `async` so the call-site
    // `Self::connect_mysql_inner(url).await` resolves identically
    // regardless of feature — the unused-await on the non-mysql arm
    // is intentional, hence the local allow.
    #[cfg(feature = "mysql")]
    #[allow(clippy::unused_async)]
    async fn connect_mysql_inner(_url: &str) -> Result<Self, PoolError> {
        // batch 2 implements this. Until then, soft-error.
        Err(PoolError::MysqlNotYetImplemented)
    }

    #[cfg(not(feature = "mysql"))]
    #[allow(clippy::unused_async)]
    async fn connect_mysql_inner(_url: &str) -> Result<Self, PoolError> {
        Err(PoolError::FeatureNotEnabled {
            scheme: "mysql",
            feature: "mysql",
        })
    }
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(feature = "postgres")]
            Pool::Postgres(_) => f.write_str("Pool::Postgres(<sqlx::PgPool>)"),
        }
    }
}

#[cfg(feature = "postgres")]
impl From<sqlx::PgPool> for Pool {
    fn from(p: sqlx::PgPool) -> Self {
        Pool::Postgres(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unrecognized_scheme_errors_clearly() {
        let err = Pool::connect("oracle://user@host/db").await.unwrap_err();
        match err {
            PoolError::UnsupportedScheme(s) => assert!(s.starts_with("oracle://")),
            other => panic!("wrong error variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_url_errors_clearly() {
        let err = Pool::connect("").await.unwrap_err();
        assert!(matches!(err, PoolError::UnsupportedScheme(_)));
    }

    #[cfg(feature = "mysql")]
    #[tokio::test]
    async fn mysql_url_returns_not_yet_implemented_in_batch1() {
        let err = Pool::connect("mysql://user:pass@host:3306/db").await.unwrap_err();
        assert!(
            matches!(err, PoolError::MysqlNotYetImplemented),
            "expected MysqlNotYetImplemented, got {err:?}"
        );
    }

    #[cfg(all(feature = "postgres", not(feature = "mysql")))]
    #[tokio::test]
    async fn mysql_url_errors_when_feature_not_enabled() {
        let err = Pool::connect("mysql://user:pass@host:3306/db").await.unwrap_err();
        match err {
            PoolError::FeatureNotEnabled { scheme, feature } => {
                assert_eq!(scheme, "mysql");
                assert_eq!(feature, "mysql");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[cfg(feature = "postgres")]
    #[tokio::test]
    async fn from_pg_pool_wraps() {
        // `connect_lazy` doesn't actually dial but still spawns sqlx
        // internals on the current Tokio runtime — needs `#[tokio::test]`.
        let pg = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy("postgres://localhost:1/none")
            .unwrap();
        let pool: Pool = pg.into();
        assert_eq!(pool.backend_name(), "postgres");
        assert!(pool.as_postgres().is_some());
    }
}
