//! Run one test body against every backend that is compiled in and
//! configured — the shared harness behind [`tri_dialect_test!`](crate::tri_dialect_test).
//!
//! ## Why this exists
//!
//! The integration suite is shaped by dialect rather than by feature
//! (#1461). 30 feature stems have a near-duplicate file per backend, and
//! 167 SQLite files have no MySQL or PG counterpart at all — not because
//! those features are SQLite-only, but because writing the second and
//! third copy by hand costs more than it returns. The `django6_*` files
//! already solved this for eight features; nothing generalized it.
//!
//! Each of those files carries ~55 lines of per-dialect module that is
//! the same everywhere: a `OnceLock<Mutex>`, a `fresh_pool` that reads an
//! env var, hand-written `CREATE TABLE` per dialect, and a `macro_rules!`
//! fanning the scenario list out — with the list repeated once per
//! backend. This module owns all of that once.
//!
//! ## The skip policy, decided in one place
//!
//! **Unset means "this backend is not configured here" and skips. Set but
//! unreachable is a broken database and panics.**
//!
//! That distinction is the whole of #1440: 66 suites read `DATABASE_URL`,
//! turned a failed connect into a skip, and reported `ok. N passed`
//! having done nothing. 128 files still implement this decision by hand,
//! which is 128 chances to get it wrong. [`Backend::pool`] implements it
//! once.
//!
//! ## Divergence is declared, never flattened
//!
//! The risk in sharing a test body across dialects is that genuine
//! differences get averaged away — the assertion weakens to whatever all
//! three can satisfy, and the suite goes quieter while looking greener.
//!
//! [`by_dialect!`](crate::by_dialect) is the answer: it takes an arm per
//! backend and **will not compile with one missing**, so a divergence has
//! to be written down rather than assumed. Each arm carries a `because`
//! string that is printed when the assertion fails, so the next reader
//! gets the reason and not just the number.

use std::sync::OnceLock;

use crate::sql::Pool;

/// A backend a tri-dialect test can run against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Backend {
    /// Reads `DATABASE_URL`. Needs a server.
    Postgres,
    /// Reads `MYSQL_TEST_URL` — deliberately **not** `DATABASE_URL`,
    /// which names the Postgres server in every CI job that sets both.
    MySql,
    /// `sqlite::memory:`. Needs no server and is always available, which
    /// is why 192 files chose it and stopped there.
    Sqlite,
}

impl Backend {
    /// The env var this backend's URL comes from, or `None` for SQLite,
    /// which needs no configuration.
    #[must_use]
    pub fn env_var(self) -> Option<&'static str> {
        match self {
            Backend::Postgres => Some("DATABASE_URL"),
            Backend::MySql => Some("MYSQL_TEST_URL"),
            Backend::Sqlite => None,
        }
    }

    /// What [`crate::sql::Dialect::name`] calls this backend, so a test
    /// can match a pool back to its `Backend`.
    #[must_use]
    pub fn dialect_name(self) -> &'static str {
        match self {
            Backend::Postgres => "postgres",
            Backend::MySql => "mysql",
            Backend::Sqlite => "sqlite",
        }
    }

    /// Open a pool, or `None` when this backend is not configured here.
    ///
    /// # Panics
    ///
    /// When the env var is **set but the server cannot be reached**. That
    /// is a broken environment, not an absent one, and reporting it as a
    /// skip is what let 66 suites pass against nothing (#1440). The
    /// message carries the URL and the driver's own error.
    pub async fn pool(self) -> Option<Pool> {
        match self {
            #[cfg(feature = "postgres")]
            Backend::Postgres => {
                let url = std::env::var("DATABASE_URL").ok()?;
                Some(
                    Pool::connect(&url).await.unwrap_or_else(|e| {
                        panic!("DATABASE_URL is set but unreachable ({url}): {e}")
                    }),
                )
            }
            #[cfg(feature = "mysql")]
            Backend::MySql => {
                let url = std::env::var("MYSQL_TEST_URL").ok()?;
                Some(Pool::connect(&url).await.unwrap_or_else(|e| {
                    panic!("MYSQL_TEST_URL is set but unreachable ({url}): {e}")
                }))
            }
            #[cfg(feature = "sqlite")]
            Backend::Sqlite => Some(
                Pool::connect("sqlite::memory:")
                    .await
                    .expect("in-memory SQLite is always available"),
            ),
            // A backend whose feature is off cannot produce a pool. Not
            // a skip with a message — the test simply does not exist on
            // this build, and the macro never reaches here.
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }
}

/// Serializes the tests **within one suite** that share a live server.
///
/// ## What this does not do
///
/// It does not serialize across suites, and an earlier version of this
/// doc claimed it did. Cargo compiles each file in `tests/` into its own
/// binary and runs those binaries as separate processes, so this
/// `static` is a different `Mutex` in each one. Two suites running
/// concurrently against the same PostgreSQL can still drop each other's
/// tables, exactly as before.
///
/// What it does buy is one lock per suite instead of one per file that
/// remembered to declare one — the per-file `OnceLock<Mutex>` these
/// files used to carry protected a suite from itself, and so does this,
/// with less to forget.
///
/// Cross-suite isolation needs something a lock cannot provide:
/// per-suite table names (which `fresh_table::<M>` already gives, since
/// every converted suite names its own model's table), or a per-suite
/// schema or database. Table naming is why this has not bitten.
#[must_use]
pub fn live_lock() -> &'static tokio::sync::Mutex<()> {
    static M: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    M.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Drop `table` if it exists, quoted for this pool's dialect.
///
/// Live backends keep their tables between runs, so a suite that creates
/// with `IF NOT EXISTS` silently inherits the previous run's columns.
///
/// # Panics
///
/// If the `DROP` fails for any reason other than the table's absence.
pub async fn drop_table(pool: &Pool, table: &str) {
    let d = pool.dialect();
    let sql = format!("DROP TABLE IF EXISTS {}", d.quote_ident(table));
    crate::sql::raw_execute_pool(pool, &sql, Vec::new())
        .await
        .unwrap_or_else(|e| panic!("dropping {table} on {}: {e}", d.name()));
}

/// Drop `M`'s table and recreate it from `M::SCHEMA`.
///
/// The DDL comes from the same emitter the migration runner uses, so the
/// test exercises the schema the framework actually produces. That is the
/// half of #1461 that makes a port mechanical rather than expensive:
/// `queryset_pluck_sqlite_live.rs` hand-writes `title TEXT NOT NULL` for
/// a `max_length = 80` field, which is right on SQLite and wrong on
/// MySQL — where the emitter produces `VARCHAR(80)`, and a hand-written
/// `TEXT` walks into the TEXT-in-index rejection.
///
/// # Panics
///
/// If the table cannot be created.
pub async fn fresh_table<M: crate::core::Model>(pool: &Pool) {
    drop_table(pool, M::SCHEMA.table).await;
    crate::testkit::create_tables_for::<M>(pool)
        .await
        .unwrap_or_else(|e| {
            panic!(
                "creating {} on {}: {e}",
                M::SCHEMA.table,
                pool.dialect().name()
            )
        });
}

/// A **file-backed** SQLite pool, for suites that need more than one
/// connection to see the same data.
///
/// 29 of the 192 SQLite suites reach for a temp file, citing in-memory
/// databases being per-connection. **That premise does not hold here**:
/// sqlx shares an in-memory database across a pool's connections, and a
/// barrier-forced test with eight simultaneous connections passes
/// against `sqlite::memory:` as readily as against a file. So this
/// helper is not the unlock those suites thought it was.
///
/// It is kept because a real file still differs in ways in-memory cannot
/// emulate — WAL journalling, file locking, and anything a second
/// *process* must open — and because converting those 29 suites should
/// not also change their storage under them. Whether they still need it
/// is worth checking, one at a time, with evidence.
///
/// The path is built from `std` rather than the `tempfile` crate, which
/// is a **dev-dependency** here — `testkit` compiles into the library, so
/// it cannot reach one.
///
/// The file is deliberately not cleaned up. A temp handle dropped at the
/// end of setup takes the database with it while the pool is still open,
/// and that surfaces much later as unrelated `disk I/O error` rows. The
/// suites this replaces leaked it too, via `mem::forget`; leaking a few
/// KB into the OS temp directory per run is the cheaper mistake.
///
/// # Panics
///
/// If the pool cannot be opened.
#[cfg(feature = "sqlite")]
pub async fn sqlite_file_pool() -> Pool {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);

    // pid + a counter: two suites in one process must not share a file,
    // and two processes must not either.
    let mut path = std::env::temp_dir();
    path.push(format!(
        "rustango-testkit-{}-{}.db",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    // A leftover from a previous run with the same pid would otherwise
    // be inherited, schema and rows included.
    let _ = std::fs::remove_file(&path);

    let url = format!("sqlite://{}?mode=rwc", path.display());
    Pool::connect(&url)
        .await
        .unwrap_or_else(|e| panic!("file-backed SQLite at {url}: {e}"))
}

/// Pick a value per backend, with every backend spelled out.
///
/// This is the answer to the one real risk in sharing a test body across
/// dialects: that a genuine difference gets averaged away. The tempting
/// move, when PG says `3` and SQLite says `0`, is to assert something
/// both satisfy — `>= 0`, or nothing at all. The suite then goes quieter
/// while looking greener, and the dialect gap it was built to expose
/// stops being visible.
///
/// So there is no default arm and no fallback. Every compiled-in backend
/// must be named, or this does not compile. A divergence has to be
/// written down.
///
/// The `because` string is not decoration — it is printed when the value
/// is used in a failing assertion, so the next person reads the reason
/// rather than rediscovering it.
///
/// ```ignore
/// let want = by_dialect! { pool,
///     postgres => 3,
///     mysql    => 3,
///     sqlite   => 0, because "SQLite has no STDDEV_POP; the emitter \
///                             falls back to NULL and COUNT sees no rows (#1024)",
/// };
/// assert_eq!(got, want.value, "{}", want.why);
/// ```
#[macro_export]
macro_rules! by_dialect {
    // All three backends, in a fixed order, each with its reason.
    //
    // Fixed order and no optional arms is what makes the guarantee real:
    // this is a single rule, so omitting a dialect is a macro match
    // failure at compile time rather than a panic on the one CI leg that
    // happens to run it. An earlier draft accumulated arms into a slice
    // and looked them up at runtime — which documented itself as
    // compile-time enforced and was not.
    //
    // `because` is mandatory for the same reason. When the values agree
    // across dialects the reason is still the useful part: it records
    // that the author checked, rather than that they copied.
    ($pool:expr,
     postgres => $pg:expr, because $pgw:expr,
     mysql    => $my:expr, because $myw:expr,
     sqlite   => $sq:expr, because $sqw:expr $(,)?
    ) => {{
        match $pool.dialect().name() {
            "postgres" => $crate::testkit::matrix::Divergence {
                value: $pg,
                why: $pgw,
            },
            "mysql" => $crate::testkit::matrix::Divergence {
                value: $my,
                why: $myw,
            },
            "sqlite" => $crate::testkit::matrix::Divergence {
                value: $sq,
                why: $sqw,
            },
            other => panic!(
                "by_dialect!: unknown dialect `{other}`. A backend was added \
                 without teaching this macro about it, so every shared test \
                 body is silently missing an expectation for it."
            ),
        }
    }};
}

/// What [`by_dialect!`](crate::by_dialect) resolves to: the value for
/// this pool's dialect, and why it is what it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Divergence<T> {
    /// The expectation for the dialect the pool is on.
    pub value: T,
    /// The `because` string for that arm; empty when none was given.
    pub why: &'static str,
}

impl<T> Divergence<T> {
    /// The value, discarding the reason — for the common case where the
    /// assertion message does not need it.
    pub fn into_value(self) -> T {
        self.value
    }
}

/// Fan a list of scenario functions out into one `#[tokio::test]` per
/// backend.
///
/// Replaces the three near-identical `mod pg_live / mysql_live /
/// sqlite_live` blocks each converted file used to carry — about 55
/// lines each, of which the only real content was the DDL and the pool
/// URL. Both of those now come from [`Backend::pool`] and
/// [`fresh_table`], so what is left is the scenario list, written once
/// instead of three times.
///
/// Each scenario is an `async fn(&Pool)`. The generated test takes the
/// shared [`live_lock`], opens the backend's pool, skips with a message
/// if that backend is not configured here, and calls the scenario.
///
/// ```ignore
/// tri_dialect_test! {
///     model: Demo,
///     scenarios: [
///         returns_plan_text,
///         returns_plan_json,
///     ],
/// }
/// ```
///
/// generates `postgres::returns_plan_text`, `mysql::returns_plan_text`,
/// `sqlite::returns_plan_text`, and so on — each gated on its backend's
/// feature, so a build without `mysql` simply has no MySQL tests rather
/// than tests that skip.
#[macro_export]
macro_rules! tri_dialect_test {
    // The common case: tables come from a model's schema, SQLite is
    // in-memory.
    (model: $model:ty, scenarios: [ $($name:ident),* $(,)? ] $(,)?) => {
        $crate::tri_dialect_test!(@build
            sqlite_pool = $crate::testkit::matrix::Backend::Sqlite.pool(),
            setup = $crate::testkit::matrix::fresh_table::<$model>,
            scenarios = [$($name),*]);
    };

    // A model, but SQLite must be file-backed — for WAL journalling,
    // file locking, or a second *process*.
    //
    // Not for cross-connection visibility: sqlx shares an in-memory
    // database across a pool's connections, measured with a barrier
    // forcing eight simultaneous ones. `sqlite_file_pool`'s own doc says
    // so; this comment used to assert the opposite, which is the premise
    // 29 suites were written on and which does not hold.
    (model: $model:ty, sqlite: file, scenarios: [ $($name:ident),* $(,)? ] $(,)?) => {
        $crate::tri_dialect_test!(@build
            sqlite_pool = $crate::testkit::matrix::sqlite_file_pool(),
            setup = $crate::testkit::matrix::fresh_table::<$model>,
            scenarios = [$($name),*]);
    };

    // No model: the suite builds its own tables — `ensure_table_pool`
    // for the job queue, a migration run, whatever it is. `setup` is an
    // `async fn(&Pool)`.
    (setup: $setup:path, scenarios: [ $($name:ident),* $(,)? ] $(,)?) => {
        $crate::tri_dialect_test!(@build
            sqlite_pool = $crate::testkit::matrix::Backend::Sqlite.pool(),
            setup = $setup,
            scenarios = [$($name),*]);
    };

    (setup: $setup:path, sqlite: file, scenarios: [ $($name:ident),* $(,)? ] $(,)?) => {
        $crate::tri_dialect_test!(@build
            sqlite_pool = $crate::testkit::matrix::sqlite_file_pool(),
            setup = $setup,
            scenarios = [$($name),*]);
    };

    // The one place the three modules are written. Everything above
    // normalizes into this so the bodies cannot drift — which is the
    // failure this whole harness exists to stop, and it would be a poor
    // joke to reintroduce it inside the macro.
    (@build
        sqlite_pool = $sqlite_pool:expr,
        setup = $setup:expr,
        scenarios = [ $($name:ident),* ]
    ) => {
        #[cfg(feature = "postgres")]
        mod tri_postgres {
            use super::*;

            $(
                #[tokio::test]
                async fn $name() {
                    let _guard = $crate::testkit::matrix::live_lock().lock().await;
                    let Some(pool) =
                        $crate::testkit::matrix::Backend::Postgres.pool().await
                    else {
                        eprintln!(
                            "DATABASE_URL not set — skipping the Postgres arm of `{}`",
                            stringify!($name)
                        );
                        return;
                    };
                    ($setup)(&pool).await;
                    super::$name(&pool).await;
                }
            )*
        }

        #[cfg(feature = "mysql")]
        mod tri_mysql {
            use super::*;

            $(
                #[tokio::test]
                async fn $name() {
                    let _guard = $crate::testkit::matrix::live_lock().lock().await;
                    let Some(pool) =
                        $crate::testkit::matrix::Backend::MySql.pool().await
                    else {
                        eprintln!(
                            "MYSQL_TEST_URL not set — skipping the MySQL arm of `{}`",
                            stringify!($name)
                        );
                        return;
                    };
                    ($setup)(&pool).await;
                    super::$name(&pool).await;
                }
            )*
        }

        #[cfg(feature = "sqlite")]
        mod tri_sqlite {
            use super::*;

            $(
                #[tokio::test]
                async fn $name() {
                    // No lock for the in-memory case: each test gets its
                    // own private database, so there is nothing to
                    // contend over, and taking the shared lock would
                    // serialize 192 suites for no reason. A file-backed
                    // pool is also per-test — a fresh path each call —
                    // so the same holds.
                    let pool = $sqlite_pool.await;
                    let Some(pool) = Into::<Option<$crate::sql::Pool>>::into(
                        $crate::testkit::matrix::IntoPoolOption(pool)
                    ) else {
                        unreachable!("SQLite is always available")
                    };
                    ($setup)(&pool).await;
                    super::$name(&pool).await;
                }
            )*
        }
    };
}

/// Lets the macro accept either `Pool` or `Option<Pool>` for the SQLite
/// arm without knowing which it got.
///
/// `Backend::Sqlite.pool()` yields `Option<Pool>` for symmetry with the
/// live backends; `sqlite_file_pool()` yields a bare `Pool`, because
/// there is nothing to be absent. Rather than force one shape on both
/// and make the common case awkward, the macro wraps whatever it has.
#[doc(hidden)]
pub struct IntoPoolOption<T>(pub T);

impl From<IntoPoolOption<Pool>> for Option<Pool> {
    fn from(w: IntoPoolOption<Pool>) -> Self {
        Some(w.0)
    }
}

impl From<IntoPoolOption<Option<Pool>>> for Option<Pool> {
    fn from(w: IntoPoolOption<Option<Pool>>) -> Self {
        w.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The env var each backend reads. `MYSQL_TEST_URL` is the one that
    /// matters: every CI job setting both points `DATABASE_URL` at
    /// PostgreSQL, so a MySQL suite reading it would connect to the
    /// wrong server and pass.
    #[test]
    fn each_backend_reads_its_own_env_var() {
        assert_eq!(Backend::Postgres.env_var(), Some("DATABASE_URL"));
        assert_eq!(Backend::MySql.env_var(), Some("MYSQL_TEST_URL"));
        assert_eq!(
            Backend::Sqlite.env_var(),
            None,
            "SQLite is in-memory and must never gate on configuration"
        );
    }

    /// The names must match `Dialect::name`, because that string is what
    /// `by_dialect!` matches a pool against. A mismatch would send every
    /// shared test body down the panic arm.
    #[test]
    fn dialect_names_match_the_dialect_impls() {
        assert_eq!(Backend::Postgres.dialect_name(), "postgres");
        assert_eq!(Backend::MySql.dialect_name(), "mysql");
        assert_eq!(Backend::Sqlite.dialect_name(), "sqlite");
    }

    /// The same invariant, proven against real pools rather than a second
    /// copy of the table: whatever is configured here must report the
    /// name its `Backend` claims. Asserting the mapping twice would only
    /// prove the two lists agree with each other.
    #[tokio::test]
    async fn a_pool_reports_the_name_its_backend_claims() {
        let mut checked = 0;
        for backend in [Backend::Postgres, Backend::MySql, Backend::Sqlite] {
            if let Some(pool) = backend.pool().await {
                assert_eq!(
                    pool.dialect().name(),
                    backend.dialect_name(),
                    "{backend:?} opens a pool whose dialect calls itself something else, \
                     so `by_dialect!` would fall through to its panic arm"
                );
                checked += 1;
            }
        }
        // SQLite needs no configuration, so when it is compiled in there
        // is no honest way to check nothing. Without it, a build with
        // only `postgres` and no `DATABASE_URL` legitimately has no
        // backend — that is a skip, not a failure. Demanding a pool
        // unconditionally made this fail on `--features tenancy`, which
        // is the feature set the pre-push hook uses.
        #[cfg(feature = "sqlite")]
        assert!(
            checked > 0,
            "SQLite is compiled in and needs no configuration, so at least \
             one backend must have been checked — a zero here means the \
             loop is not reaching `Backend::pool` at all"
        );
        #[cfg(not(feature = "sqlite"))]
        if checked == 0 {
            eprintln!(
                "no backend configured on this build (sqlite not compiled in, \
                 no DATABASE_URL / MYSQL_TEST_URL) — nothing to check"
            );
        }
    }

    /// `by_dialect!` selects the arm for the pool it is given, and hands
    /// back that arm's reason.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn by_dialect_selects_the_running_dialects_arm() {
        let pool = Backend::Sqlite.pool().await.expect("sqlite");

        let picked = crate::by_dialect! { &pool,
            postgres => 1, because "pg",
            mysql    => 2, because "my",
            sqlite   => 3, because "sq",
        };
        assert_eq!(picked.value, 3, "should take the SQLite arm");
        assert_eq!(
            picked.why, "sq",
            "the reason must travel with the value — it is what a failing \
             assertion prints"
        );
    }

    /// An unset backend skips; it does not fail and does not fabricate a
    /// pool. This is the half of the #1440 policy that stays permissive.
    #[tokio::test]
    async fn an_unconfigured_backend_yields_no_pool() {
        // Guarded rather than assumed: if the developer running this has
        // MYSQL_TEST_URL exported, the correct outcome is a pool.
        if std::env::var("MYSQL_TEST_URL").is_ok() {
            eprintln!("MYSQL_TEST_URL is set — skipping the unset-path assertion");
            return;
        }
        assert!(
            Backend::MySql.pool().await.is_none(),
            "an unset env var means `not configured here`, which skips"
        );
    }

    /// Model for [`fresh_table_creates_a_writable_table_from_the_schema`].
    /// At module scope because `#[derive(Model)]` emits `super::`-relative
    /// paths and cannot be declared inside a function body.
    #[derive(crate::Model, Debug, Clone)]
    #[rustango(table = "matrix_selftest_row")]
    #[rustango(app = "testkit_matrix_selftest")]
    #[allow(dead_code)]
    #[cfg(feature = "sqlite")]
    pub struct Row {
        #[rustango(primary_key)]
        id: crate::sql::Auto<i64>,
        #[rustango(max_length = 32)]
        label: String,
    }

    /// A file-backed pool is one database across *simultaneous*
    /// connections.
    ///
    /// The barrier is load-bearing: without it eight spawned inserts
    /// finish fast enough to be served by one pooled connection —
    /// measured `max_connections=10` with `size=1` — so they prove
    /// nothing about sharing. Holding every connection open until all
    /// eight exist forces the pool to open eight.
    ///
    /// **What this does not show.** It was written to prove a file
    /// differs from `sqlite::memory:`, and it does not: the same test
    /// passes against `:memory:`. sqlx shares an in-memory database
    /// across a pool's connections, so the premise behind 29 suites
    /// reaching for a temp file — `jobs_sqlite_live` says in-memory
    /// "defeats the multi-worker test" — does not hold here, at least
    /// for connection sharing. Those suites may be simplifiable; that
    /// is a separate question and wants its own evidence.
    ///
    /// The invariant asserted is still worth holding: concurrent
    /// writers on distinct connections land in one database.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn a_file_backed_pool_is_one_database_across_connections() {
        use crate::sql::CounterPool as _;
        use std::sync::Arc;

        const N: usize = 8;
        let pool = sqlite_file_pool().await;
        fresh_table::<Row>(&pool).await;

        let barrier = Arc::new(tokio::sync::Barrier::new(N));
        let mut tasks = Vec::new();
        for i in 0..N {
            let pool = pool.clone();
            let barrier = Arc::clone(&barrier);
            tasks.push(tokio::spawn(async move {
                let crate::sql::Pool::Sqlite(sq) = &pool else {
                    unreachable!("sqlite_file_pool returns a SQLite pool")
                };
                // Hold a connection, and do not let go until every task
                // holds one too.
                let mut conn = sq.acquire().await.expect("acquire");
                barrier.wait().await;
                sqlx::query("INSERT INTO matrix_selftest_row (label) VALUES (?)")
                    .bind(format!("row-{i}"))
                    .execute(&mut *conn)
                    .await
                    .expect("insert on a held connection");
            }));
        }
        for t in tasks {
            t.await.expect("writer task");
        }

        assert_eq!(
            Row::objects().count(&pool).await.expect("count"),
            N as i64,
            "all {N} writers held their own connection at once and must still \
             share one database — a short count would mean each connection got \
             its own, which is what `:memory:` was assumed to do and measurably \
             does not"
        );
    }

    /// `fresh_table` builds from `M::SCHEMA` and leaves a table the ORM
    /// can actually write to — the property that makes a port mechanical
    /// instead of a hand-translation of DDL.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn fresh_table_creates_a_writable_table_from_the_schema() {
        use crate::core::Model as _;
        use crate::sql::FetcherPool as _;

        let pool = Backend::Sqlite.pool().await.expect("sqlite");
        fresh_table::<Row>(&pool).await;

        // Round-trip through the ORM, not a raw statement: the point is
        // that the emitted DDL matches what the ORM expects to write.
        let mut row = Row {
            id: crate::sql::Auto::Unset,
            label: "hello".into(),
        };
        row.save_pool(&pool)
            .await
            .expect("insert into emitted table");

        let found: Vec<Row> = Row::objects().fetch(&pool).await.expect("fetch");
        assert_eq!(found.len(), 1, "the row written should be the row read");
        assert_eq!(found[0].label, "hello");

        // And it is *fresh*: a second call drops and recreates rather
        // than inheriting the previous run's rows.
        fresh_table::<Row>(&pool).await;
        let after: Vec<Row> = Row::objects().fetch(&pool).await.expect("fetch");
        assert!(
            after.is_empty(),
            "fresh_table must drop the table, not reuse it — a live backend \
             keeps tables between runs and a stale one silently inherits \
             the previous schema"
        );
    }
}
