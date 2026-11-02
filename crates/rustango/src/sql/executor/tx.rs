//! `PoolTx` + `transaction_pool` user-facing helper.
//!
//! Extracted from `executor/mod.rs` as part of #116 step 4. The per-
//! backend `*_tx` operations (`insert_tx`, `update_tx`, …) and
//! `atomic()` consume `PoolTx`; they still live in `mod.rs` for now
//! and will move into `pool/tx`-shaped modules in a later step.

use super::{Dialect, ExecError};
use crate::sql::{Pool, PoolId};

// ====================================================================
// `transaction_pool` — user-facing bi-dialect transaction helper
// ====================================================================
//
// Wraps a closure in a per-backend `BEGIN`/`COMMIT`/`ROLLBACK`. Same
// shape as the existing PgPool [`transaction`] helper, but the
// closure receives a `PoolTx` whose `TxConn` the caller pattern-matches
// to get a typed connection. sqlx's `Transaction<DB>` is generic over
// backend, so we can't hand callers a single "any-DB" transaction
// handle without erasing the bind types — exposing the per-arm enum
// keeps users in control of which backend they're talking to and
// avoids surprise driver mismatches.

/// A transaction handle borrowed from one of [`Pool`]'s backend arms.
/// Yielded by [`transaction_pool`] so callers run their queries against
/// the right driver-typed connection, reached through [`Self::conn`].
///
/// It remembers the pool it was opened on, so audit rows written in it
/// keep a tenant-bound source on that tenant's pool.
pub struct PoolTx<'a> {
    pub(crate) conn: TxConn<'a>,
    pool: PoolId,
}

/// A driver connection a [`PoolTx`] can begin on.
pub(crate) trait BeginTx {
    fn begin_tx(&mut self) -> futures_util::future::BoxFuture<'_, Result<TxConn<'_>, sqlx::Error>>;
}

macro_rules! begin_tx {
    ($feature:literal, $conn:ty, $variant:ident) => {
        #[cfg(feature = $feature)]
        impl BeginTx for $conn {
            fn begin_tx(
                &mut self,
            ) -> futures_util::future::BoxFuture<'_, Result<TxConn<'_>, sqlx::Error>> {
                use sqlx::Connection as _;
                Box::pin(async move { Ok(TxConn::$variant(self.begin().await?)) })
            }
        }
    };
}
begin_tx!("postgres", sqlx::PgConnection, Postgres);
begin_tx!("mysql", sqlx::MySqlConnection, Mysql);
begin_tx!("sqlite", sqlx::SqliteConnection, Sqlite);

/// The driver-typed transaction inside a [`PoolTx`].
#[non_exhaustive]
pub enum TxConn<'a> {
    #[cfg(feature = "postgres")]
    Postgres(sqlx::Transaction<'a, sqlx::Postgres>),
    #[cfg(feature = "mysql")]
    Mysql(sqlx::Transaction<'a, sqlx::MySql>),
    #[cfg(feature = "sqlite")]
    Sqlite(sqlx::Transaction<'a, sqlx::Sqlite>),
}

impl<'a> PoolTx<'a> {
    /// `conn` opened on `pool`.
    pub(crate) fn new(conn: TxConn<'a>, pool: PoolId) -> Self {
        Self { conn, pool }
    }

    /// Begin on `conn`, a connection checked out of `pool`.
    pub(crate) async fn begin_on<C: BeginTx + ?Sized>(
        conn: &'a mut C,
        pool: &PoolId,
    ) -> Result<Self, ExecError> {
        Ok(Self::new(conn.begin_tx().await?, pool.clone()))
    }

    /// The driver-typed transaction, to run sqlx queries on.
    pub fn conn(&mut self) -> &mut TxConn<'a> {
        &mut self.conn
    }

    /// The pool this transaction was opened on.
    pub(crate) fn pool_id(&self) -> &PoolId {
        &self.pool
    }

    /// Commit this transaction. Consumes the wrapper.
    ///
    /// # Errors
    /// `sqlx::Error` from the underlying `COMMIT`.
    pub async fn commit(self) -> Result<(), sqlx::Error> {
        match self.conn {
            #[cfg(feature = "postgres")]
            TxConn::Postgres(tx) => tx.commit().await,
            #[cfg(feature = "mysql")]
            TxConn::Mysql(tx) => tx.commit().await,
            #[cfg(feature = "sqlite")]
            TxConn::Sqlite(tx) => tx.commit().await,
        }
    }

    /// Roll back this transaction. Consumes the wrapper. Best-effort —
    /// drop semantics auto-roll-back too if the caller forgets to
    /// invoke this explicitly.
    ///
    /// # Errors
    /// `sqlx::Error` from the underlying `ROLLBACK`.
    pub async fn rollback(self) -> Result<(), sqlx::Error> {
        match self.conn {
            #[cfg(feature = "postgres")]
            TxConn::Postgres(tx) => tx.rollback().await,
            #[cfg(feature = "mysql")]
            TxConn::Mysql(tx) => tx.rollback().await,
            #[cfg(feature = "sqlite")]
            TxConn::Sqlite(tx) => tx.rollback().await,
        }
    }

    /// Run a bind-free statement unprepared. MySQL refuses `SAVEPOINT`
    /// as a prepared statement (error 1295).
    pub(crate) async fn execute_unprepared(&mut self, sql: &str) -> Result<(), sqlx::Error> {
        use sqlx::Executor as _;
        match &mut self.conn {
            #[cfg(feature = "postgres")]
            TxConn::Postgres(tx) => (&mut **tx).execute(sqlx::raw_sql(sql)).await.map(drop),
            #[cfg(feature = "mysql")]
            TxConn::Mysql(tx) => (&mut **tx).execute(sqlx::raw_sql(sql)).await.map(drop),
            #[cfg(feature = "sqlite")]
            TxConn::Sqlite(tx) => (&mut **tx).execute(sqlx::raw_sql(sql)).await.map(drop),
        }
    }

    /// `false` when the server already ended this transaction: PG after a
    /// failed statement (its COMMIT would silently roll back), MySQL after a
    /// deadlock, timeout rollback or implicit DDL commit (later statements
    /// autocommit). Two round trips on MySQL, one on PG; SQLite reports
    /// its rollbacks through [`Self::on_sqlite_rollback`] instead.
    pub(crate) async fn still_open(&mut self) -> bool {
        match &mut self.conn {
            #[cfg(feature = "postgres")]
            TxConn::Postgres(_) => self.execute_unprepared("SELECT 1").await.is_ok(),
            #[cfg(feature = "mysql")]
            TxConn::Mysql(tx) => {
                // sqlx checks the server's in-transaction flag after its
                // SAVEPOINT; MySQL has no `@@in_transaction` to read.
                use sqlx::Connection as _;
                match (**tx).begin().await {
                    Ok(probe) => probe.commit().await.is_ok(),
                    Err(_) => false,
                }
            }
            #[cfg(feature = "sqlite")]
            TxConn::Sqlite(_) => true,
        }
    }

    /// Call `f` whenever SQLite rolls this connection's transaction back
    /// (not on `ROLLBACK TO`). A no-op on other backends.
    pub(crate) async fn on_sqlite_rollback(
        &mut self,
        f: impl FnMut() + Send + 'static,
    ) -> Result<(), sqlx::Error> {
        match &mut self.conn {
            #[cfg(feature = "sqlite")]
            TxConn::Sqlite(tx) => {
                (**tx).lock_handle().await?.set_rollback_hook(f);
                Ok(())
            }
            #[allow(unreachable_patterns)]
            _ => {
                drop(f);
                Ok(())
            }
        }
    }

    /// Remove the hook [`Self::on_sqlite_rollback`] set.
    pub(crate) async fn clear_sqlite_rollback(&mut self) {
        #[cfg(feature = "sqlite")]
        #[allow(irrefutable_let_patterns)]
        if let TxConn::Sqlite(tx) = &mut self.conn {
            if let Ok(mut h) = (**tx).lock_handle().await {
                h.remove_rollback_hook();
            }
        }
    }

    /// Return the dialect for this transaction's backend — same
    /// dispatch as [`crate::sql::Pool::dialect`] but sourced from the
    /// `PoolTx` variant rather than the pool. Used internally by the
    /// `_tx` executor helpers to compile SQL against the right backend.
    #[must_use]
    pub fn dialect(&self) -> &'static dyn Dialect {
        match &self.conn {
            #[cfg(feature = "postgres")]
            TxConn::Postgres(_) => crate::sql::postgres::DIALECT,
            #[cfg(feature = "mysql")]
            TxConn::Mysql(_) => crate::sql::mysql::DIALECT,
            #[cfg(feature = "sqlite")]
            TxConn::Sqlite(_) => crate::sql::sqlite::DIALECT,
        }
    }
}

/// Open a transaction against either backend. Bi-dialect counterpart
/// of `pool.begin().await?`. Caller owns the returned [`PoolTx`] and
/// must call `.commit().await?` (or `.rollback().await?`) before
/// dropping; otherwise sqlx auto-rolls-back on drop.
///
/// Most user code wants the macro-generated `delete_pool` /
/// `save_pool` / `insert_pool` instead — those already wrap each
/// per-row write in a transaction. `transaction_pool` is for
/// cross-row / cross-table atomicity:
///
/// ```ignore
/// let mut tx = rustango::sql::transaction_pool(&pool).await?;
/// match tx.conn() {
///     #[cfg(feature = "postgres")]
///     rustango::sql::TxConn::Postgres(t) => {
///         sqlx::query("UPDATE accounts SET balance = balance - $1 WHERE id = $2")
///             .bind(amount).bind(from).execute(&mut **t).await?;
///     }
///     #[cfg(feature = "mysql")]
///     rustango::sql::TxConn::Mysql(t) => {
///         sqlx::query("UPDATE accounts SET balance = balance - ? WHERE id = ?")
///             .bind(amount).bind(from).execute(&mut **t).await?;
///     }
///     _ => unreachable!(),
/// }
/// tx.commit().await?;
/// ```
///
/// The match-on-variant ceremony stays explicit because sqlx's
/// `Transaction<DB>` is generic over backend — there's no
/// driver-erased connection type to hand callers without losing
/// bind-side type checking. A future batch may add a `tx_pool!`
/// macro to abstract the match for the common per-backend-mirror
/// pattern.
///
/// # Errors
/// Driver errors from `BEGIN`.
pub async fn transaction_pool(pool: &Pool) -> Result<PoolTx<'static>, ExecError> {
    let conn = match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => TxConn::Postgres(pg.begin().await?),
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => TxConn::Mysql(my.begin().await?),
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => TxConn::Sqlite(sq.begin().await?),
    };
    Ok(PoolTx::new(conn, PoolId::of(pool)))
}

/// [`transaction_pool`] that takes SQLite's write lock at `BEGIN`. Use it
/// for read-then-write work such as `select_for_update`: on SQLite a
/// deferred one fails its first write with `SQLITE_BUSY` when another
/// transaction wrote since its read.
///
/// # Errors
/// Driver errors from `BEGIN`.
pub async fn write_transaction(pool: &Pool) -> Result<PoolTx<'static>, ExecError> {
    match pool {
        // Take the write lock up front: a deferred tx that reads first
        // fails at once on a busy WAL database instead of waiting.
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => Ok(PoolTx::new(
            TxConn::Sqlite(sq.begin_with("BEGIN IMMEDIATE").await?),
            PoolId::of(pool),
        )),
        #[allow(unreachable_patterns)]
        _ => transaction_pool(pool).await,
    }
}
