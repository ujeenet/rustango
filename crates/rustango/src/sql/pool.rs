//! Dialect-agnostic database pool wrapper.
//!
//! [`Pool`] reaches Postgres, MySQL or SQLite through one handle and
//! picks the matching [`Dialect`] for you. The older `&PgPool` APIs
//! still work; this is an addition, not a replacement.
//!
//! Wrap a pool you already have:
//!
//! ```ignore
//! let pg: sqlx::PgPool = sqlx::PgPool::connect(&url).await?;
//! let pool: rustango::sql::Pool = pg.into();
//! ```
//!
//! Or let it connect for you, from a URL or the environment:
//!
//! ```ignore
//! use rustango::sql::Pool;
//!
//! let pool = Pool::connect("postgres://user:pass@host/db").await?;
//! let pool = Pool::connect_from_env().await?;
//! ```
//!
//! Then ask which backend you got:
//!
//! ```ignore
//! let dialect: &dyn rustango::sql::Dialect = pool.dialect();
//! tracing::info!(name = dialect.name(), "started against backend");
//! ```

use std::time::Duration;

use crate::env::{database_url_from_env, EnvError};

use super::connect_diagnosis::{ConnectDiagnosis, ConnectFault};
use super::Dialect;

/// Why a connect attempt failed, before it becomes whichever error
/// type the caller asked for.
///
/// The two cases differ: a driver failure has a host, a cause and
/// some advice, while a bad scheme never dialled anything at all.
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

    /// The URL names a backend whose Cargo feature is off.
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

/// A wrapper around any sqlx pool rustango supports. Cloning is
/// cheap: sqlx already keeps the pool behind an `Arc`.
#[derive(Clone)]
pub enum Pool {
    #[cfg(feature = "postgres")]
    Postgres(sqlx::PgPool),
    #[cfg(feature = "mysql")]
    Mysql(sqlx::MySqlPool),
    /// SQLite, either file-backed or `:memory:`.
    #[cfg(feature = "sqlite")]
    Sqlite(sqlx::SqlitePool),
}

/// Apply [`tuning`] to any backend's `PoolOptions`.
///
/// A macro, not a function: sqlx's three `PoolOptions` types share
/// the same method names but no trait.
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
        // These two take an `Option` where `None` means "no bound",
        // which is not what an unset setting means, so skip them.
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
    /// Connect from a URL. The scheme picks the backend and must be
    /// `postgres://`, `postgresql://`, `mysql://` or `sqlite:`, and
    /// its Cargo feature must be on.
    ///
    /// # Errors
    ///
    /// - [`PoolError::UnsupportedScheme`] for any other scheme.
    /// - [`PoolError::FeatureNotEnabled`] when the scheme is known
    ///   but its feature was off at build time.
    /// - [`PoolError::Connect`] when sqlx cannot reach the database.
    pub async fn connect(url: &str) -> Result<Self, PoolError> {
        // SQLite is written both `sqlite:./path.db` and
        // `sqlite://…`, so read the scheme up to the first colon.
        let scheme = url.split(':').next().unwrap_or("").to_ascii_lowercase();
        match scheme.as_str() {
            "postgres" | "postgresql" => Self::connect_postgres_inner(url).await,
            "mysql" => Self::connect_mysql_inner(url).await,
            "sqlite" => Self::connect_sqlite_inner(url).await,
            _ => Err(PoolError::UnsupportedScheme(url.to_owned())),
        }
    }

    /// [`Self::connect`] with its own acquire timeout. That already
    /// has a bounded default, see [`ACQUIRE_TIMEOUT_ENV`], so use
    /// this only when one pool must differ from the rest.
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
    /// [`ConnectDiagnosis`] instead of a string.
    ///
    /// Use this when the caller needs to branch on the kind of
    /// fault, as a tenant pre-flight check or an operator console
    /// showing per-fault advice does.
    ///
    /// # Errors
    /// A `ConnectDiagnosis` naming the fault, the endpoint tried (with
    /// the password removed) and the driver's own message.
    pub async fn connect_diagnosed(url: &str, timeout: Duration) -> Result<Self, ConnectDiagnosis> {
        Self::connect_inner(url, timeout)
            .await
            .map_err(|f| match f {
                ConnectFail::Driver(d) => d,
                // A scheme this build cannot speak is not a
                // connection fault, but the caller still needs a
                // diagnosis, and the message already says what to add
                // to Cargo.toml.
                ConnectFail::Pool(e) => {
                    ConnectDiagnosis::new(ConnectFault::Other, url, e.to_string())
                }
            })
    }

    /// The only place the scheme dispatch lives. It keeps a driver
    /// failure and a scheme failure apart, so each public wrapper can
    /// return the shape it promises.
    async fn connect_inner(url: &str, timeout: Duration) -> Result<Self, ConnectFail> {
        // Not tuned beyond the caller's timeout. This path serves
        // one-shot probes, which open a pool, ask whether the
        // database answers and close it. Applying the app's sizing
        // would open `min_connections` connections for nothing.
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

    /// [`Self::connect`] without dialling: the pool connects on the
    /// first query. `manage::Cli` uses it for commands that may never
    /// touch the database, so printing help opens no socket.
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

    /// Close the pool, waiting for its connections to be released.
    ///
    /// Dropping a pool schedules the close but does not wait, so a
    /// short-lived pool such as a connection probe can outlive the
    /// code that made it and keep a socket open. Call this when you
    /// are done with the pool and the release must have happened
    /// before you return.
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

    /// The [`Dialect`] for this pool's backend. Use it to ask about
    /// identifier quoting, placeholder shape and the rest without
    /// caring which backend is underneath.
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

    /// The backend's short name, for logs and `manage` output. The
    /// same as `pool.dialect().name()`.
    #[must_use]
    pub fn backend_name(&self) -> &'static str {
        self.dialect().name()
    }

    /// A stable hash of **which database this pool talks to**. Use it
    /// to partition any process-global cache whose rows are facts
    /// about one database.
    ///
    /// `ContentType` is the case that needs it: its `id` comes from
    /// that database's own sequence, so the same natural key is a
    /// different number in every tenant. A cache keyed on the natural
    /// key alone would hand one tenant another's id.
    ///
    /// **On Postgres the hash includes the connect options, and that
    /// part is load-bearing.** Schema-mode tenants share one host,
    /// port and database, and differ only in their `search_path`.
    /// Hashing the connection details alone would collide across all
    /// of them.
    ///
    /// Two pools to the same database hash the same, which is what a
    /// cache wants. Cheap enough for a hot path.
    #[must_use]
    pub fn scope_key(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.backend_name().hash(&mut h);
        match self {
            #[cfg(feature = "postgres")]
            Pool::Postgres(p) => {
                let o = p.connect_options();
                o.get_host().hash(&mut h);
                o.get_port().hash(&mut h);
                o.get_database().hash(&mut h);
                o.get_options().hash(&mut h);
            }
            #[cfg(feature = "mysql")]
            Pool::Mysql(p) => {
                let o = p.connect_options();
                o.get_host().hash(&mut h);
                o.get_port().hash(&mut h);
                o.get_database().hash(&mut h);
            }
            #[cfg(feature = "sqlite")]
            Pool::Sqlite(p) => {
                let o = p.connect_options();
                o.get_filename().hash(&mut h);
            }
        }
        h.finish()
    }

    /// Borrow as a `PgPool` for code that needs Postgres itself.
    /// `None` on any other backend.
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

    /// Borrow as a `MySqlPool`. [`Self::as_postgres`] for MySQL.
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

    /// Borrow as a `SqlitePool`. [`Self::as_postgres`] for SQLite.
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
    // These are the only places a backend pool is built; everything
    // else routes through them. That is what makes the `tuned!` macro
    // work: one edit reaches every pool in the process.
    //
    // They are public because some callers need the typed pool —
    // `TenantPools<DB>` takes `sqlx::Pool<DB>`, not the enum — and
    // would otherwise call sqlx directly and skip every option the
    // framework applies.

    /// Connect to Postgres and return the typed pool.
    ///
    /// Prefer [`Self::connect`] unless you need `sqlx::PgPool`
    /// itself. Use this over `PgPool::connect`, which applies none of
    /// the framework's pool options.
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

    /// [`Self::connect_postgres`] without dialling: the pool connects
    /// on first use. For commands that may never touch the database.
    ///
    /// # Errors
    /// As [`Self::connect`].
    #[cfg(feature = "postgres")]
    pub fn connect_postgres_lazy(url: &str) -> Result<sqlx::PgPool, PoolError> {
        tuned!(sqlx::postgres::PgPoolOptions::new())
            .connect_lazy(url)
            .map_err(|e| PoolError::Connect(ConnectDiagnosis::of(url, &e).to_string()))
    }

    /// Connect to MySQL. [`Self::connect_postgres`] for MySQL.
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

    /// Connect to SQLite. This also applies the framework's pragmas
    /// through [`sqlite_connect_options`]: WAL for a file-backed
    /// database, and `?mode=rwc` so a missing file is created.
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

    // Async so the call site looks the same with the feature off.
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
        // sqlx accepts several SQLite URL forms:
        //   `sqlite::memory:`             anonymous in-memory
        //   `sqlite:./path.db`            relative path
        //   `sqlite:///abs/path.db`       absolute path
        //   `sqlite:?mode=memory&cache=shared`
        //
        // `sqlite_connect_options` creates a missing file, turns on
        // foreign keys, sets a 5s busy timeout, and uses WAL for a
        // file-backed database.
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

/// How many seconds a request waits for a pooled connection.
///
/// sqlx would wait 30s, which suits a batch tool, not a web server:
/// an unreachable database would pin a worker for half a minute per
/// request, so one database's outage saturates the whole server.
/// Five seconds is still plenty for a busy but healthy pool.
const ACQUIRE_TIMEOUT_DEFAULT_SECS: u64 = 5;

/// Env overrides for the rest of the pool settings, so a deploy can
/// retune a pool without a config change. See [`ACQUIRE_TIMEOUT_ENV`].
pub const MAX_CONNECTIONS_ENV: &str = "RUSTANGO_DB_MAX_CONNECTIONS";
/// See [`MAX_CONNECTIONS_ENV`].
pub const MIN_CONNECTIONS_ENV: &str = "RUSTANGO_DB_MIN_CONNECTIONS";
/// See [`MAX_CONNECTIONS_ENV`].
pub const IDLE_TIMEOUT_ENV: &str = "RUSTANGO_DB_IDLE_TIMEOUT_SECS";
/// See [`MAX_CONNECTIONS_ENV`].
pub const MAX_LIFETIME_ENV: &str = "RUSTANGO_DB_MAX_LIFETIME_SECS";

/// How every pool this process opens is sized and timed.
///
/// `None` means "leave sqlx's default alone", not zero.
///
/// [`configure_pools`] installs it once at boot from `[database]`,
/// and every [`Pool`] constructor reads it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PoolTuning {
    /// Most connections to keep. sqlx's 10 is usually too few for a
    /// web server and far too many for SQLite.
    pub max_connections: Option<u32>,
    /// Connections held open while idle. sqlx keeps none, so the
    /// first request after a quiet spell pays for a new connection.
    pub min_connections: Option<u32>,
    /// How long a caller waits for a connection, whether dialling a
    /// new one or queueing for a free one. See
    /// [`ACQUIRE_TIMEOUT_DEFAULT_SECS`].
    pub acquire_timeout: Option<Duration>,
    /// Close a connection idle this long. This guards against the
    /// other end dropping it first, which shows up as a broken
    /// connection on some later request.
    pub idle_timeout: Option<Duration>,
    /// Close a connection this old, however busy. Without it, a pool
    /// can keep connections to a server that has failed over, or
    /// with credentials that have since been revoked.
    pub max_lifetime: Option<Duration>,
}

static TUNING: std::sync::OnceLock<PoolTuning> = std::sync::OnceLock::new();

/// Pools opened before anything called [`configure_pools`].
///
/// They ran on env-only tuning, and the count is the only way the
/// operator who configured `[database]` finds out.
static POOLS_BEFORE_CONFIG: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Install the pool tuning for this process. **The first call
/// wins**, so a library cannot retune an application's pools.
/// Returns `false` when tuning was already set.
///
/// Env vars beat the values passed here, so a deploy can override
/// without a config change.
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
        // The only setting with a rustango default, so never `None`.
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

/// Parse a positive whole number from `key`, warning about and
/// ignoring anything else. Zero is rejected: these are all bounds,
/// and a bound of zero is a mistake.
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

/// Build a `SqliteConnectOptions` with the pragmas every rustango
/// SQLite pool needs:
///
/// - `foreign_keys = ON`. SQLite leaves FK enforcement off, so
///   without this the database accepts orphaned references.
/// - `busy_timeout = 5s`, so a contended write waits instead of
///   returning `database is locked` at once.
/// - `journal_mode = WAL` for a file-backed database, so readers do
///   not block writers. Skipped for in-memory databases, where WAL
///   quietly falls back to MEMORY mode.
///
/// The URL passes through `ensure_sqlite_rwc_default` first, so a
/// missing file is created on connect.
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

/// Add `?mode=rwc` to a SQLite file URL that names no mode, so a
/// missing file is created. An in-memory URL, or one that already
/// names a mode, passes through unchanged.
#[cfg(feature = "sqlite")]
fn ensure_sqlite_rwc_default(url: &str) -> String {
    // In-memory: no file to create.
    if url.contains(":memory:") || url.starts_with("sqlite::memory:") {
        return url.to_owned();
    }
    // The caller chose a mode; leave it alone.
    if url.contains("mode=") {
        return url.to_owned();
    }
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

    /// With nothing configured, every setting stays `None` and sqlx
    /// keeps its defaults, except the acquire timeout.
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

    /// A deploy-time override must not need a config change.
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

    /// A typo must not set a bound to zero, nor fail the boot.
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

    /// The settings must reach the pool, not merely parse. A lazy
    /// pool is enough: it is built without dialling, so the options
    /// are readable with no database. It still needs a tokio runtime
    /// for the pool's reaper.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn a_configured_size_reaches_the_pool_itself() {
        let _g = env_lock();
        clear();
        std::env::set_var(MAX_CONNECTIONS_ENV, "37");
        std::env::set_var(MIN_CONNECTIONS_ENV, "3");
        std::env::set_var(ACQUIRE_TIMEOUT_ENV, "11");

        // Compare against the tuning in force, not the env vars just
        // set: another test may already have sealed it, and then the
        // result would depend on test order. The tests above already
        // cover the parsing; this one covers the effect.
        let expect = tuning();
        let pool = Pool::connect_sqlite_lazy("sqlite::memory:").expect("build a lazy pool");
        let opts = pool.options();

        if let Some(n) = expect.max_connections {
            assert_eq!(opts.get_max_connections(), n, "max_connections");
        }
        if let Some(n) = expect.min_connections {
            assert_eq!(opts.get_min_connections(), n, "min_connections");
        }
        assert_eq!(
            opts.get_acquire_timeout(),
            expect
                .acquire_timeout
                .unwrap_or_else(|| Duration::from_secs(ACQUIRE_TIMEOUT_DEFAULT_SECS)),
            "acquire_timeout"
        );
        clear();
    }

    /// Reading the tuning must not seal it. With `get_or_init`, a
    /// pool opened before `with_settings` would freeze env-only
    /// tuning for the whole process.
    #[test]
    fn reading_the_tuning_leaves_it_configurable() {
        let _g = env_lock();
        // Check that a read does not change whether the cell is set,
        // rather than its state. Another test may already have
        // called `configure_pools`, so asserting `is_none()` would
        // pass or fail by test order.
        let set_before = TUNING.get().is_some();
        let _ = tuning();
        let _ = tuning();
        assert_eq!(
            TUNING.get().is_some(),
            set_before,
            "reading the tuning changed whether it was set — with `get_or_init` a read \
             sealed the cell, so any pool built before `with_settings` silently froze \
             the whole process on environment defaults"
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
        // `connect_lazy` does not dial, but sqlx still spawns on the
        // current Tokio runtime, so this needs `#[tokio::test]`.
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
        let pool = Pool::connect("sqlite::memory:").await.unwrap();
        assert_eq!(pool.backend_name(), "sqlite");
        assert!(pool.as_sqlite().is_some());
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn sqlite_from_pool_dispatches_to_sqlite_dialect() {
        // A hand-built `Pool::Sqlite` gets the right dialect too.
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
        // The dialect must be MySQL's even with no server to reach.
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
