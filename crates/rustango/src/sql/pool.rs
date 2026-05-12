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
//! - **batch1** (shipped) — `mysql` Cargo feature wired; connecting
//!   via `mysql://` returns a soft-error.
//! - **batch2** (this batch) — `Pool::Mysql(MySqlPool)` variant lands;
//!   `Pool::connect("mysql://…")` opens a real `MySqlPool` and
//!   `pool.dialect()` returns the [`crate::sql::MySql`] dialect with
//!   correct identifier quoting (backticks), placeholder shape (`?`),
//!   `BIGINT AUTO_INCREMENT` for `Auto<T>` PKs, and `GET_LOCK`-based
//!   advisory locking. The query-compilation methods on `MySql`
//!   error with [`crate::sql::SqlError::DialectQueryCompilationNotImplemented`]
//!   — ORM queries against `MySQL` light up in batch3.
//! - **batch3** — port the IR-to-SQL writers off Postgres-only
//!   assumptions so `Model::objects().filter(...).fetch(...)` works
//!   against either backend.

use std::time::Duration;

use crate::env::{database_url_from_env, EnvError};

use super::Dialect;

#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    /// `sqlx` couldn't connect (auth, host unreachable, schema bad…).
    #[error("connect: {0}")]
    Connect(String),

    /// URL didn't start with a recognized scheme (`postgres://`,
    /// `postgresql://`, `mysql://`, or `sqlite:`).
    #[error("unsupported scheme in URL `{0}` — expected postgres://, mysql://, or sqlite:")]
    UnsupportedScheme(String),

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

/// Cheap-to-clone wrapper around any rustango-supported sqlx pool.
/// `Arc`-wrapping is handled by `sqlx` itself — cloning a Pool is
/// cloning the underlying `Arc`.
#[derive(Clone)]
pub enum Pool {
    #[cfg(feature = "postgres")]
    Postgres(sqlx::PgPool),
    #[cfg(feature = "mysql")]
    Mysql(sqlx::MySqlPool),
    /// SQLite — file-backed or `:memory:`. Phase 2 of the v0.27
    /// SQLite rollout (item #37).
    #[cfg(feature = "sqlite")]
    Sqlite(sqlx::SqlitePool),
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
        // SQLite URLs use `sqlite:` (no `//`) for the colon form
        // (`sqlite::memory:`, `sqlite:./path.db`) AND `sqlite://` for
        // the URI form. Strip up to the first colon to detect.
        let scheme = url.split(':').next().unwrap_or("").to_ascii_lowercase();
        match scheme.as_str() {
            "postgres" | "postgresql" => Self::connect_postgres_inner(url).await,
            "mysql" => Self::connect_mysql_inner(url).await,
            "sqlite" => Self::connect_sqlite_inner(url).await,
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
        let scheme = url.split(':').next().unwrap_or("").to_ascii_lowercase();
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
            #[cfg(feature = "mysql")]
            "mysql" => {
                let pool = sqlx::mysql::MySqlPoolOptions::new()
                    .acquire_timeout(timeout)
                    .connect(url)
                    .await
                    .map_err(|e| PoolError::Connect(e.to_string()))?;
                Ok(Self::Mysql(pool))
            }
            #[cfg(not(feature = "mysql"))]
            "mysql" => Err(PoolError::FeatureNotEnabled {
                scheme: "mysql",
                feature: "mysql",
            }),
            #[cfg(feature = "sqlite")]
            "sqlite" => {
                let pool = sqlx::sqlite::SqlitePoolOptions::new()
                    .acquire_timeout(timeout)
                    .connect(url)
                    .await
                    .map_err(|e| PoolError::Connect(e.to_string()))?;
                Ok(Self::Sqlite(pool))
            }
            #[cfg(not(feature = "sqlite"))]
            "sqlite" => Err(PoolError::FeatureNotEnabled {
                scheme: "sqlite",
                feature: "sqlite",
            }),
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
            #[cfg(feature = "mysql")]
            Pool::Mysql(_) => super::mysql::DIALECT,
            #[cfg(feature = "sqlite")]
            Pool::Sqlite(_) => super::sqlite::DIALECT,
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
            #[cfg(feature = "mysql")]
            Pool::Mysql(_) => None,
            #[cfg(feature = "sqlite")]
            Pool::Sqlite(_) => None,
        }
    }

    /// Borrow as a `MySqlPool`. Symmetric with [`Self::as_postgres`] —
    /// returns `None` when the pool wraps a non-MySQL backend. Lets
    /// MySQL-specific code paths reach the underlying `sqlx` handle
    /// without having to re-dispatch through `Pool`'s enum each time.
    #[must_use]
    #[cfg(feature = "mysql")]
    pub fn as_mysql(&self) -> Option<&sqlx::MySqlPool> {
        match self {
            #[cfg(feature = "postgres")]
            Pool::Postgres(_) => None,
            Pool::Mysql(p) => Some(p),
            #[cfg(feature = "sqlite")]
            Pool::Sqlite(_) => None,
        }
    }

    /// Borrow as a `SqlitePool`. Symmetric with [`Self::as_postgres`] —
    /// returns `None` when the pool wraps a non-SQLite backend.
    #[must_use]
    #[cfg(feature = "sqlite")]
    pub fn as_sqlite(&self) -> Option<&sqlx::SqlitePool> {
        match self {
            #[cfg(feature = "postgres")]
            Pool::Postgres(_) => None,
            #[cfg(feature = "mysql")]
            Pool::Mysql(_) => None,
            Pool::Sqlite(p) => Some(p),
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

    #[cfg(feature = "mysql")]
    async fn connect_mysql_inner(url: &str) -> Result<Self, PoolError> {
        let pool = sqlx::MySqlPool::connect(url)
            .await
            .map_err(|e| PoolError::Connect(e.to_string()))?;
        Ok(Self::Mysql(pool))
    }

    // Stays async so the call-site `.await` shape matches across
    // feature configurations.
    #[cfg(not(feature = "mysql"))]
    #[allow(clippy::unused_async)]
    async fn connect_mysql_inner(_url: &str) -> Result<Self, PoolError> {
        Err(PoolError::FeatureNotEnabled {
            scheme: "mysql",
            feature: "mysql",
        })
    }

    #[cfg(feature = "sqlite")]
    async fn connect_sqlite_inner(url: &str) -> Result<Self, PoolError> {
        // Phase 3 — bi-dialect executor surface now dispatches to
        // SqliteRow, so `Pool::connect("sqlite:…")` returns a usable
        // pool. SQLite URL forms accepted by sqlx:
        //   - `sqlite::memory:` — anonymous in-memory database
        //   - `sqlite:./path.db` — relative path
        //   - `sqlite:///abs/path.db` — absolute path
        //   - `sqlite:?mode=memory&cache=shared` — query-string options
        //
        // v0.37 friendly-default: when the URL points at a file path
        // and doesn't already carry a `mode=` query param, append
        // `?mode=rwc` so a missing file is created (matching the
        // Django-shape "just works" tone). Without this, sqlx
        // returns `unable to open database file (code: 14)` on first
        // boot, which is a confusing error for new users. In-memory
        // databases (`sqlite::memory:`) and explicit `mode=` URLs
        // bypass this — the user's intent always wins.
        let url_with_default = ensure_sqlite_rwc_default(url);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect(&url_with_default)
            .await
            .map_err(|e| PoolError::Connect(e.to_string()))?;
        Ok(Self::Sqlite(pool))
    }

    #[cfg(not(feature = "sqlite"))]
    #[allow(clippy::unused_async)]
    async fn connect_sqlite_inner(_url: &str) -> Result<Self, PoolError> {
        Err(PoolError::FeatureNotEnabled {
            scheme: "sqlite",
            feature: "sqlite",
        })
    }
}

/// v0.37 — append `?mode=rwc` to a sqlite file URL that doesn't
/// already specify a `mode=`. In-memory URLs (`sqlite::memory:`) and
/// URLs that already opt into a mode (`mode=ro` / `mode=rwc` / etc.)
/// pass through unchanged. Public for tests in
/// `pool_sqlite_rwc_default_tests`.
#[cfg(feature = "sqlite")]
fn ensure_sqlite_rwc_default(url: &str) -> String {
    // In-memory: no file to create, no default needed.
    if url.contains(":memory:") || url.starts_with("sqlite::memory:") {
        return url.to_owned();
    }
    // Caller already opted into a mode — respect their intent.
    if url.contains("mode=") {
        return url.to_owned();
    }
    // Append `?mode=rwc` or `&mode=rwc` depending on existing query.
    if url.contains('?') {
        format!("{url}&mode=rwc")
    } else {
        format!("{url}?mode=rwc")
    }
}

#[cfg(all(test, feature = "sqlite"))]
mod pool_sqlite_rwc_default_tests {
    use super::ensure_sqlite_rwc_default;

    #[test]
    fn memory_url_passes_through() {
        assert_eq!(
            ensure_sqlite_rwc_default("sqlite::memory:"),
            "sqlite::memory:"
        );
    }

    #[test]
    fn memory_query_passes_through() {
        assert_eq!(
            ensure_sqlite_rwc_default("sqlite:?mode=memory&cache=shared"),
            "sqlite:?mode=memory&cache=shared"
        );
    }

    #[test]
    fn explicit_mode_passes_through() {
        assert_eq!(
            ensure_sqlite_rwc_default("sqlite:./db.db?mode=ro"),
            "sqlite:./db.db?mode=ro"
        );
        assert_eq!(
            ensure_sqlite_rwc_default("sqlite:///abs/db.db?mode=rwc"),
            "sqlite:///abs/db.db?mode=rwc"
        );
    }

    #[test]
    fn file_path_without_mode_gets_rwc() {
        assert_eq!(
            ensure_sqlite_rwc_default("sqlite:./db.db"),
            "sqlite:./db.db?mode=rwc"
        );
        assert_eq!(
            ensure_sqlite_rwc_default("sqlite:///abs/db.db"),
            "sqlite:///abs/db.db?mode=rwc"
        );
    }

    #[test]
    fn existing_query_gets_appended_with_amp() {
        assert_eq!(
            ensure_sqlite_rwc_default("sqlite:./db.db?cache=shared"),
            "sqlite:./db.db?cache=shared&mode=rwc"
        );
    }
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(feature = "postgres")]
            Pool::Postgres(_) => f.write_str("Pool::Postgres(<sqlx::PgPool>)"),
            #[cfg(feature = "mysql")]
            Pool::Mysql(_) => f.write_str("Pool::Mysql(<sqlx::MySqlPool>)"),
            #[cfg(feature = "sqlite")]
            Pool::Sqlite(_) => f.write_str("Pool::Sqlite(<sqlx::SqlitePool>)"),
        }
    }
}

#[cfg(feature = "postgres")]
impl From<sqlx::PgPool> for Pool {
    fn from(p: sqlx::PgPool) -> Self {
        Pool::Postgres(p)
    }
}

#[cfg(feature = "mysql")]
impl From<sqlx::MySqlPool> for Pool {
    fn from(p: sqlx::MySqlPool) -> Self {
        Pool::Mysql(p)
    }
}

#[cfg(feature = "sqlite")]
impl From<sqlx::SqlitePool> for Pool {
    fn from(p: sqlx::SqlitePool) -> Self {
        Pool::Sqlite(p)
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

    #[cfg(all(feature = "postgres", not(feature = "mysql")))]
    #[tokio::test]
    async fn mysql_url_errors_when_feature_not_enabled() {
        let err = Pool::connect("mysql://user:pass@host:3306/db")
            .await
            .unwrap_err();
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
        #[cfg(feature = "mysql")]
        assert!(pool.as_mysql().is_none());
    }

    #[cfg(feature = "mysql")]
    #[tokio::test]
    async fn from_mysql_pool_wraps() {
        // Symmetric with `from_pg_pool_wraps`. `connect_lazy` defers
        // the actual TCP dial, but the pool's spawn surface still
        // needs a Tokio runtime.
        let my = sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(1)
            .connect_lazy("mysql://user:pass@localhost:1/none")
            .unwrap();
        let pool: Pool = my.into();
        assert_eq!(pool.backend_name(), "mysql");
        assert!(pool.as_mysql().is_some());
        #[cfg(feature = "postgres")]
        assert!(pool.as_postgres().is_none());
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn sqlite_url_connect_succeeds_in_memory() {
        // Phase 3: `Pool::connect("sqlite::memory:")` returns a
        // usable pool now that the bi-dialect executor surface
        // dispatches to SqliteRow.
        let pool = Pool::connect("sqlite::memory:").await.unwrap();
        assert_eq!(pool.backend_name(), "sqlite");
        assert!(pool.as_sqlite().is_some());
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn sqlite_from_pool_dispatches_to_sqlite_dialect() {
        // Users CAN build a Pool::Sqlite manually via From<SqlitePool>
        // — confirms dialect dispatch + accessors work end-to-end.
        let sqlite_pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_lazy("sqlite::memory:")
            .unwrap();
        let pool: Pool = sqlite_pool.into();
        assert_eq!(pool.backend_name(), "sqlite");
        let d = pool.dialect();
        assert_eq!(d.name(), "sqlite");
        assert!(d.supports_returning());
        assert_eq!(d.bool_literal(true), "1");
        assert!(pool.as_sqlite().is_some());
        #[cfg(feature = "postgres")]
        assert!(pool.as_postgres().is_none());
        #[cfg(feature = "mysql")]
        assert!(pool.as_mysql().is_none());
    }

    #[cfg(feature = "mysql")]
    #[tokio::test]
    async fn mysql_pool_dialect_is_mysql() {
        // Confirms Pool::dialect() dispatches to the MySql singleton —
        // identifier quoting on the borrowed dialect must be backticks
        // even though the pool itself can't be reached without a
        // running MySQL.
        let my = sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(1)
            .connect_lazy("mysql://user:pass@localhost:1/none")
            .unwrap();
        let pool: Pool = my.into();
        let d = pool.dialect();
        assert_eq!(d.name(), "mysql");
        assert_eq!(d.quote_ident("col"), "`col`");
        assert_eq!(d.placeholder(1), "?");
        assert!(!d.supports_returning());
    }
}
