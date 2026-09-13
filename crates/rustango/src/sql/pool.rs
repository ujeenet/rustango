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

use super::connect_diagnosis::{ConnectDiagnosis, ConnectFault};
use super::Dialect;

/// Why a connect attempt failed, before it is rendered into whichever
/// error type the caller asked for.
///
/// The two are genuinely different: a driver failure has a host, a
/// cause and advice; a scheme or missing-feature failure never dialled
/// anything and wants `PoolError`'s typed variants instead.
enum ConnectFail {
    Driver(ConnectDiagnosis),
    Pool(PoolError),
}

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

/// Apply [`tuning`] to a `PoolOptions` of any backend.
///
/// A macro rather than a generic function because sqlx's three
/// `PoolOptions` types share no trait — the builder methods have the
/// same names on each, but nothing relates them. Defined here because
/// `macro_rules!` must precede its call sites.
#[allow(unused_macros)]
macro_rules! tuned {
    ($opts:expr) => {{
        let t = tuning();
        let mut o = $opts.acquire_timeout(
            t.acquire_timeout
                .unwrap_or_else(|| Duration::from_secs(ACQUIRE_TIMEOUT_DEFAULT_SECS)),
        );
        if let Some(n) = t.max_connections {
            o = o.max_connections(n);
        }
        if let Some(n) = t.min_connections {
            o = o.min_connections(n);
        }
        // These two already take `Option`, where `None` means "no
        // bound" — the same thing `None` means in `PoolTuning`, so an
        // unset knob is left entirely alone rather than set to nothing.
        if t.idle_timeout.is_some() {
            o = o.idle_timeout(t.idle_timeout);
        }
        if t.max_lifetime.is_some() {
            o = o.max_lifetime(t.max_lifetime);
        }
        o
    }};
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

    /// Same as [`Self::connect`] but with an explicit acquire timeout
    /// for this one pool. [`Self::connect`] already applies a bounded
    /// default — see [`ACQUIRE_TIMEOUT_ENV`] — so reach for this only
    /// when a single pool needs to differ from the rest.
    ///
    /// # Errors
    /// Same set as [`Self::connect`], plus a `Connect` error if `sqlx`
    /// times out before the database accepts the connection.
    pub async fn connect_with_timeout(url: &str, timeout: Duration) -> Result<Self, PoolError> {
        Self::connect_inner(url, timeout)
            .await
            .map_err(|f| match f {
                ConnectFail::Driver(d) => PoolError::Connect(d.to_string()),
                ConnectFail::Pool(e) => e,
            })
    }

    /// [`Self::connect_with_timeout`], but a failure comes back as a
    /// structured [`ConnectDiagnosis`] rather than a rendered string.
    ///
    /// `PoolError::Connect` carries a `String`, so the classification
    /// made during the attempt is gone by the time a caller sees it.
    /// A caller that needs to *branch* on the fault — the tenant
    /// pre-flight, an operator console rendering per-fault advice —
    /// wants the enum, not prose it has to parse back.
    ///
    /// # Errors
    /// A `ConnectDiagnosis` naming the fault, the endpoint tried (with
    /// the password removed) and the driver's own message.
    pub async fn connect_diagnosed(url: &str, timeout: Duration) -> Result<Self, ConnectDiagnosis> {
        Self::connect_inner(url, timeout)
            .await
            .map_err(|f| match f {
                ConnectFail::Driver(d) => d,
                // A scheme this build cannot speak is not a *connection*
                // fault — nothing was dialled — but a caller asking for a
                // diagnosis still needs one, and the message is already
                // specific about what to add to Cargo.toml.
                ConnectFail::Pool(e) => {
                    ConnectDiagnosis::new(ConnectFault::Other, url, e.to_string())
                }
            })
    }

    /// The one place the scheme dispatch lives. Keeps the driver's
    /// classified failure and a scheme/feature failure distinct, so
    /// each public wrapper can render whichever shape it promises.
    async fn connect_inner(url: &str, timeout: Duration) -> Result<Self, ConnectFail> {
        // Deliberately *not* tuned beyond the caller's timeout. The only
        // users of this path are one-shot probes — tenant preflight opens
        // a pool, asks whether the database answers, and closes it. Giving
        // a probe the application's sizing would have it eagerly open
        // `min_connections` connections it is about to throw away.
        let scheme = url.split(':').next().unwrap_or("").to_ascii_lowercase();
        match scheme.as_str() {
            #[cfg(feature = "postgres")]
            "postgres" | "postgresql" => {
                let pool = sqlx::postgres::PgPoolOptions::new()
                    .acquire_timeout(timeout)
                    .connect(url)
                    .await
                    .map_err(|e| ConnectFail::Driver(ConnectDiagnosis::of(url, &e)))?;
                Ok(Self::Postgres(pool))
            }
            #[cfg(not(feature = "postgres"))]
            "postgres" | "postgresql" => Err(ConnectFail::Pool(PoolError::FeatureNotEnabled {
                scheme: "postgres",
                feature: "postgres",
            })),
            #[cfg(feature = "mysql")]
            "mysql" => {
                let pool = sqlx::mysql::MySqlPoolOptions::new()
                    .acquire_timeout(timeout)
                    .connect(url)
                    .await
                    .map_err(|e| ConnectFail::Driver(ConnectDiagnosis::of(url, &e)))?;
                Ok(Self::Mysql(pool))
            }
            #[cfg(not(feature = "mysql"))]
            "mysql" => Err(ConnectFail::Pool(PoolError::FeatureNotEnabled {
                scheme: "mysql",
                feature: "mysql",
            })),
            #[cfg(feature = "sqlite")]
            "sqlite" => {
                let opts = sqlite_connect_options(url).map_err(ConnectFail::Pool)?;
                let pool = sqlx::sqlite::SqlitePoolOptions::new()
                    .acquire_timeout(timeout)
                    .connect_with(opts)
                    .await
                    .map_err(|e| ConnectFail::Driver(ConnectDiagnosis::of(url, &e)))?;
                Ok(Self::Sqlite(pool))
            }
            #[cfg(not(feature = "sqlite"))]
            "sqlite" => Err(ConnectFail::Pool(PoolError::FeatureNotEnabled {
                scheme: "sqlite",
                feature: "sqlite",
            })),
            _ => Err(ConnectFail::Pool(PoolError::UnsupportedScheme(
                url.to_owned(),
            ))),
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

    /// v0.38 — backend-agnostic counterpart of
    /// `sqlx::PgPool::connect_lazy`. Builds a pool that defers the
    /// first connection until the first query, dispatching to
    /// sqlx's per-backend `connect_lazy` based on the URL scheme.
    /// Used by `manage::Cli` for "no-db verbs" (`help` / `startapp`
    /// / `makemigrations` etc.) so we don't open a TCP socket just
    /// to print help text.
    ///
    /// # Errors
    /// As [`Self::connect`].
    pub fn connect_lazy(url: &str) -> Result<Self, PoolError> {
        let scheme = url.split(':').next().unwrap_or("").to_ascii_lowercase();
        match scheme.as_str() {
            #[cfg(feature = "postgres")]
            "postgres" | "postgresql" => Ok(Self::Postgres(Self::connect_postgres_lazy(url)?)),
            #[cfg(not(feature = "postgres"))]
            "postgres" | "postgresql" => Err(PoolError::FeatureNotEnabled {
                scheme: "postgres",
                feature: "postgres",
            }),
            #[cfg(feature = "mysql")]
            "mysql" => Ok(Self::Mysql(Self::connect_mysql_lazy(url)?)),
            #[cfg(not(feature = "mysql"))]
            "mysql" => Err(PoolError::FeatureNotEnabled {
                scheme: "mysql",
                feature: "mysql",
            }),
            #[cfg(feature = "sqlite")]
            "sqlite" => Ok(Self::Sqlite(Self::connect_sqlite_lazy(url)?)),
            #[cfg(not(feature = "sqlite"))]
            "sqlite" => Err(PoolError::FeatureNotEnabled {
                scheme: "sqlite",
                feature: "sqlite",
            }),
            _ => Err(PoolError::UnsupportedScheme(url.to_owned())),
        }
    }

    /// Borrow the dialect for this pool. Stable [`Dialect`] reference
    /// Close the pool, waiting for its connections to be released.
    ///
    /// Dropping a pool schedules the close but does not wait for it, so
    /// a short-lived pool — a connection probe, a one-shot migration
    /// against a tenant — can outlive the code that made it and hold a
    /// socket open. Call this when the pool is finished with and the
    /// release should have happened by the time you return.
    pub async fn close(&self) {
        match self {
            #[cfg(feature = "postgres")]
            Self::Postgres(p) => p.close().await,
            #[cfg(feature = "mysql")]
            Self::Mysql(p) => p.close().await,
            #[cfg(feature = "sqlite")]
            Self::Sqlite(p) => p.close().await,
        }
    }

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

    // ---- typed constructors ----
    //
    // Stage 1 made these the single place a pool is built; the `tuned!`
    // macro below is what that bought — one edit reaches every pool in
    // the process, including the ones that used to call sqlx directly.
    //
    // These are the **only** places a backend pool is built. Everything
    // else — `connect`, `connect_lazy`, `connect_inner`, and the
    // `manage` dispatch paths — routes through them.
    //
    // They exist because the callers that need a typed pool
    // (`TenantPools<DB>` takes `sqlx::Pool<DB>`, not the enum) were
    // otherwise reaching for `PgPool::connect(&url)` directly, which
    // skips every option the framework applies. The main `runserver`
    // pool was one of them.

    /// Connect to Postgres, returning the typed pool.
    ///
    /// Prefer [`Self::connect`] unless you need `sqlx::PgPool` itself.
    /// This applies the same options as `connect` — use it rather than
    /// `PgPool::connect`, which applies none.
    ///
    /// # Errors
    /// As [`Self::connect`].
    #[cfg(feature = "postgres")]
    pub async fn connect_postgres(url: &str) -> Result<sqlx::PgPool, PoolError> {
        tuned!(sqlx::postgres::PgPoolOptions::new())
            .connect(url)
            .await
            .map_err(|e| PoolError::Connect(ConnectDiagnosis::of(url, &e).to_string()))
    }

    /// [`Self::connect_postgres`] without dialling — the pool connects
    /// on first use. For verbs that may never touch the database.
    ///
    /// # Errors
    /// As [`Self::connect`].
    #[cfg(feature = "postgres")]
    pub fn connect_postgres_lazy(url: &str) -> Result<sqlx::PgPool, PoolError> {
        tuned!(sqlx::postgres::PgPoolOptions::new())
            .connect_lazy(url)
            .map_err(|e| PoolError::Connect(ConnectDiagnosis::of(url, &e).to_string()))
    }

    /// Connect to MySQL, returning the typed pool. See
    /// [`Self::connect_postgres`].
    ///
    /// # Errors
    /// As [`Self::connect`].
    #[cfg(feature = "mysql")]
    pub async fn connect_mysql(url: &str) -> Result<sqlx::MySqlPool, PoolError> {
        tuned!(sqlx::mysql::MySqlPoolOptions::new())
            .connect(url)
            .await
            .map_err(|e| PoolError::Connect(ConnectDiagnosis::of(url, &e).to_string()))
    }

    /// [`Self::connect_mysql`] without dialling.
    ///
    /// # Errors
    /// As [`Self::connect`].
    #[cfg(feature = "mysql")]
    pub fn connect_mysql_lazy(url: &str) -> Result<sqlx::MySqlPool, PoolError> {
        tuned!(sqlx::mysql::MySqlPoolOptions::new())
            .connect_lazy(url)
            .map_err(|e| PoolError::Connect(ConnectDiagnosis::of(url, &e).to_string()))
    }

    /// Connect to SQLite, returning the typed pool.
    ///
    /// Unlike the other two this also applies the framework's pragmas
    /// via [`sqlite_connect_options`] — WAL for file-backed databases,
    /// and `?mode=rwc` so a missing file is created. Sites that built
    /// their own `SqlitePoolOptions` were silently getting neither.
    ///
    /// # Errors
    /// As [`Self::connect`].
    #[cfg(feature = "sqlite")]
    pub async fn connect_sqlite(url: &str) -> Result<sqlx::SqlitePool, PoolError> {
        let opts = sqlite_connect_options(url)?;
        tuned!(sqlx::sqlite::SqlitePoolOptions::new())
            .connect_with(opts)
            .await
            .map_err(|e| PoolError::Connect(ConnectDiagnosis::of(url, &e).to_string()))
    }

    /// [`Self::connect_sqlite`] without dialling.
    ///
    /// # Errors
    /// As [`Self::connect`].
    #[cfg(feature = "sqlite")]
    pub fn connect_sqlite_lazy(url: &str) -> Result<sqlx::SqlitePool, PoolError> {
        let opts = sqlite_connect_options(url)?;
        Ok(tuned!(sqlx::sqlite::SqlitePoolOptions::new()).connect_lazy_with(opts))
    }

    // ---- internal connect helpers ----
    // (see `default_acquire_timeout` below for the timeout they share)

    #[cfg(feature = "postgres")]
    async fn connect_postgres_inner(url: &str) -> Result<Self, PoolError> {
        Ok(Self::Postgres(Self::connect_postgres(url).await?))
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
        Ok(Self::Mysql(Self::connect_mysql(url).await?))
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
        // v0.37 friendly-default: missing files are created on connect
        // via `ensure_sqlite_rwc_default` (applied inside
        // `sqlite_connect_options`). v0.40: `sqlite_connect_options`
        // also turns on `foreign_keys`, sets `busy_timeout = 5s`, and
        // enables WAL journal mode for file-backed databases.
        Ok(Self::Sqlite(Self::connect_sqlite(url).await?))
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

/// Env override for the pool acquire timeout, in seconds.
pub const ACQUIRE_TIMEOUT_ENV: &str = "RUSTANGO_DB_ACQUIRE_TIMEOUT_SECS";

/// Default seconds a request will wait for a pooled connection.
///
/// sqlx defaults this to **30s**, which is a batch-tool number, not a
/// web-server one. On a request path it means a database that is simply
/// unreachable pins a worker for half a minute per request — so an
/// outage of one database saturates the server and takes down surfaces
/// that never touch it. Five seconds still leaves ample room for a
/// saturated-but-healthy pool to hand back a connection, while failing
/// an unreachable one six times sooner.
const ACQUIRE_TIMEOUT_DEFAULT_SECS: u64 = 5;

/// Env overrides for the rest of the pool knobs. Same shape and same
/// reason as [`ACQUIRE_TIMEOUT_ENV`]: a deploy can retune a pool without
/// a config push and a restart of the config pipeline.
pub const MAX_CONNECTIONS_ENV: &str = "RUSTANGO_DB_MAX_CONNECTIONS";
/// See [`MAX_CONNECTIONS_ENV`].
pub const MIN_CONNECTIONS_ENV: &str = "RUSTANGO_DB_MIN_CONNECTIONS";
/// See [`MAX_CONNECTIONS_ENV`].
pub const IDLE_TIMEOUT_ENV: &str = "RUSTANGO_DB_IDLE_TIMEOUT_SECS";
/// See [`MAX_CONNECTIONS_ENV`].
pub const MAX_LIFETIME_ENV: &str = "RUSTANGO_DB_MAX_LIFETIME_SECS";

/// How every pool this process opens is sized and timed.
///
/// `None` means "leave sqlx's default alone", not "zero" — a knob nobody
/// set must behave exactly as it did before the knob existed.
///
/// Installed once at boot by [`configure_pools`], which
/// [`crate::manage::Cli::with_settings`] calls from `[database]`. Read by
/// every constructor on [`Pool`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PoolTuning {
    /// Upper bound on pooled connections. sqlx defaults to 10, which is
    /// usually too small for a web server and far too large for SQLite.
    pub max_connections: Option<u32>,
    /// Connections kept open even when idle. sqlx defaults to 0, so the
    /// first request after a quiet period pays the connect round-trip.
    pub min_connections: Option<u32>,
    /// How long a caller waits for a connection — both dialling a new
    /// one and queueing for a free one. See
    /// [`ACQUIRE_TIMEOUT_DEFAULT_SECS`] for why this one has a rustango
    /// default rather than sqlx's.
    pub acquire_timeout: Option<Duration>,
    /// Close a connection that has sat idle this long. Defends against
    /// a load balancer or `idle_in_transaction_session_timeout` cutting
    /// it from the other end, which surfaces as a broken connection on
    /// the next unlucky request.
    pub idle_timeout: Option<Duration>,
    /// Close a connection this old regardless of use. The knob that
    /// matters behind a failover or a credential rotation: without it a
    /// pool can hold connections to a server that is no longer the one
    /// you want, or with credentials that have since been revoked.
    pub max_lifetime: Option<Duration>,
}

static TUNING: std::sync::OnceLock<PoolTuning> = std::sync::OnceLock::new();

/// Pools opened before anything called [`configure_pools`].
///
/// Counted rather than ignored because those pools silently ran on
/// env-only tuning, and the operator who configured `[database]` has no
/// other way to find out.
static POOLS_BEFORE_CONFIG: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Install the pool tuning for this process. **First call wins**, like
/// the other boot globals, so a library cannot retune an application's
/// pools out from under it.
///
/// Returns `false` if tuning was already set and this call changed
/// nothing.
///
/// Env vars win over the values passed here — the same precedence
/// `with_settings` uses for `bind`, so a deploy-time override does not
/// need a config push.
pub fn configure_pools(from_settings: PoolTuning) -> bool {
    let installed = TUNING.set(merge_env_over(from_settings)).is_ok();
    let missed = POOLS_BEFORE_CONFIG.load(std::sync::atomic::Ordering::Relaxed);
    if installed && missed > 0 {
        tracing::warn!(
            target: "rustango::sql",
            pools = missed,
            "{missed} database pool(s) were opened before settings were applied \
             and are running on environment defaults — move `.with_settings(…)` \
             ahead of any pool construction"
        );
    }
    installed
}

/// The tuning in force.
///
/// **Reading must never seal the cell.** An earlier cut used
/// `get_or_init`, which meant any pool opened before `with_settings` —
/// an app connecting in `main()`, a second test in the same binary —
/// permanently froze env-only tuning for the whole process, silently.
/// Now a read falls back without writing, and notes that it happened.
fn tuning() -> PoolTuning {
    match TUNING.get() {
        Some(t) => *t,
        None => {
            POOLS_BEFORE_CONFIG.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            merge_env_over(PoolTuning::default())
        }
    }
}

/// Overlay the env vars onto `base`, warning about values that do not
/// parse rather than failing a boot over a typo'd knob.
fn merge_env_over(base: PoolTuning) -> PoolTuning {
    PoolTuning {
        max_connections: parse_env(MAX_CONNECTIONS_ENV).or(base.max_connections),
        min_connections: parse_env(MIN_CONNECTIONS_ENV).or(base.min_connections),
        // The one knob with a rustango default rather than an sqlx one,
        // so it is never `None`.
        acquire_timeout: Some(
            env_secs(ACQUIRE_TIMEOUT_ENV)
                .or(base.acquire_timeout)
                .unwrap_or_else(|| Duration::from_secs(ACQUIRE_TIMEOUT_DEFAULT_SECS)),
        ),
        idle_timeout: env_secs(IDLE_TIMEOUT_ENV).or(base.idle_timeout),
        max_lifetime: env_secs(MAX_LIFETIME_ENV).or(base.max_lifetime),
    }
}

fn env_secs(key: &str) -> Option<Duration> {
    parse_env::<u64>(key).map(Duration::from_secs)
}

/// Parse a positive whole number from `key`, warning and ignoring
/// anything else. Zero is rejected: every knob here is a bound, and a
/// bound of zero is a mistake rather than an instruction.
fn parse_env<T: std::str::FromStr + PartialEq + Default>(key: &str) -> Option<T> {
    let raw = std::env::var(key).ok()?;
    match raw.trim().parse::<T>() {
        Ok(v) if v != T::default() => Some(v),
        _ => {
            tracing::warn!(
                target: "rustango::sql",
                value = %raw,
                "{key} must be a positive whole number; ignoring it"
            );
            None
        }
    }
}

/// v0.40 — build a `SqliteConnectOptions` with the pragmas every
/// rustango SQLite pool needs:
///
/// - `foreign_keys = ON` — SQLite ships with FK enforcement OFF, so
///   without this an ORM that emits `ForeignKey` columns silently
///   accepts orphaned references.
/// - `busy_timeout = 5s` — contended writes wait instead of
///   immediately returning `database is locked`.
/// - `journal_mode = WAL` for file-backed databases — concurrent
///   readers don't block writers. Skipped for `:memory:` and
///   `mode=memory` URLs (WAL is file-only; setting it on memory
///   databases silently falls back to MEMORY mode).
///
/// Applies `ensure_sqlite_rwc_default` to the URL first, so missing
/// files are created on connect.
///
/// # Errors
/// [`PoolError::Connect`] if the URL can't be parsed as a SQLite URL.
#[cfg(feature = "sqlite")]
pub(crate) fn sqlite_connect_options(
    url: &str,
) -> Result<sqlx::sqlite::SqliteConnectOptions, PoolError> {
    use std::str::FromStr;
    let url_with_default = ensure_sqlite_rwc_default(url);
    let mut opts = sqlx::sqlite::SqliteConnectOptions::from_str(&url_with_default)
        .map_err(|e| PoolError::Connect(ConnectDiagnosis::of(url, &e).to_string()))?
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5));
    if !url_with_default.contains(":memory:") && !url_with_default.contains("mode=memory") {
        opts = opts.journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);
    }
    Ok(opts)
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
mod tuning_tests {
    use super::*;

    /// The env knobs are process-global, so these serialize.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    fn clear() {
        for k in [
            MAX_CONNECTIONS_ENV,
            MIN_CONNECTIONS_ENV,
            IDLE_TIMEOUT_ENV,
            MAX_LIFETIME_ENV,
            ACQUIRE_TIMEOUT_ENV,
        ] {
            std::env::remove_var(k);
        }
    }

    /// Unconfigured, every knob stays `None` so sqlx keeps its own
    /// defaults — except the acquire timeout, which rustango
    /// deliberately tightens from 30s to 5s.
    #[test]
    fn an_unconfigured_process_changes_nothing_but_the_acquire_timeout() {
        let _g = env_lock();
        clear();
        let t = merge_env_over(PoolTuning::default());
        assert_eq!(t.max_connections, None);
        assert_eq!(t.min_connections, None);
        assert_eq!(t.idle_timeout, None);
        assert_eq!(t.max_lifetime, None);
        assert_eq!(
            t.acquire_timeout,
            Some(Duration::from_secs(ACQUIRE_TIMEOUT_DEFAULT_SECS))
        );
    }

    #[test]
    fn settings_are_carried_through_when_no_env_is_set() {
        let _g = env_lock();
        clear();
        let from_toml = PoolTuning {
            max_connections: Some(50),
            min_connections: Some(5),
            acquire_timeout: Some(Duration::from_secs(9)),
            idle_timeout: Some(Duration::from_secs(600)),
            max_lifetime: Some(Duration::from_secs(1800)),
        };
        assert_eq!(merge_env_over(from_toml), from_toml);
    }

    /// Same precedence `with_settings` uses for `bind`: a deploy-time
    /// override must not need a config push and a restart.
    #[test]
    fn env_wins_over_settings() {
        let _g = env_lock();
        clear();
        std::env::set_var(MAX_CONNECTIONS_ENV, "99");
        let t = merge_env_over(PoolTuning {
            max_connections: Some(50),
            ..PoolTuning::default()
        });
        clear();
        assert_eq!(t.max_connections, Some(99));
    }

    /// A typo'd knob must not take a bound to zero or fail the boot.
    #[test]
    fn an_unparseable_or_zero_value_is_ignored_not_obeyed() {
        let _g = env_lock();
        clear();
        for bad in ["nonsense", "0", "-1", ""] {
            std::env::set_var(MAX_CONNECTIONS_ENV, bad);
            let t = merge_env_over(PoolTuning {
                max_connections: Some(20),
                ..PoolTuning::default()
            });
            assert_eq!(t.max_connections, Some(20), "{bad:?} should be ignored");
        }
        clear();
    }

    #[test]
    fn seconds_become_durations() {
        let _g = env_lock();
        clear();
        std::env::set_var(IDLE_TIMEOUT_ENV, "300");
        let t = merge_env_over(PoolTuning::default());
        clear();
        assert_eq!(t.idle_timeout, Some(Duration::from_secs(300)));
    }

    /// **The assertion nobody wrote the first time.** `pool_max_size`
    /// was parsed, type-checked and unit-tested for years while
    /// reaching no pool at all, because every test asserted the
    /// *parsed value* and none asserted the *effect*.
    ///
    /// A lazy pool is enough: `connect_lazy` builds the pool without
    /// dialling, so the options are observable with no database — but
    /// sqlx still wants a runtime to hang the pool's reaper on, hence
    /// `#[tokio::test]`.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn a_configured_size_reaches_the_pool_itself() {
        let _g = env_lock();
        clear();
        std::env::set_var(MAX_CONNECTIONS_ENV, "37");
        std::env::set_var(MIN_CONNECTIONS_ENV, "3");
        std::env::set_var(ACQUIRE_TIMEOUT_ENV, "11");

        let pool = Pool::connect_sqlite_lazy("sqlite::memory:").expect("build a lazy pool");
        let opts = pool.options();
        assert_eq!(opts.get_max_connections(), 37, "max_connections");
        assert_eq!(opts.get_min_connections(), 3, "min_connections");
        assert_eq!(
            opts.get_acquire_timeout(),
            Duration::from_secs(11),
            "acquire_timeout"
        );
        clear();
    }

    /// Reading the tuning must not seal it. An earlier cut used
    /// `get_or_init`, so a pool opened before `with_settings` froze
    /// env-only tuning for the entire process — silently, and for
    /// every pool after it.
    #[test]
    fn reading_the_tuning_leaves_it_configurable() {
        let _g = env_lock();
        clear();
        let before = tuning();
        assert_eq!(before.max_connections, None);
        // The cell must still be empty, i.e. still settable. Asserting
        // on `TUNING.get()` rather than calling `configure_pools`,
        // which would seal it for every other test in this binary.
        assert!(
            TUNING.get().is_none(),
            "a read sealed the tuning cell — settings applied later would be ignored"
        );
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
