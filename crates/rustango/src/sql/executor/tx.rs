//! `PoolTx` enum + `transaction_pool` user-facing helper.
//!
//! Extracted from `executor/mod.rs` as part of #116 step 4. The per-
//! backend `*_tx` operations (`insert_tx`, `update_tx`, …) and
//! `atomic()` consume `PoolTx`; they still live in `mod.rs` for now
//! and will move into `pool/tx`-shaped modules in a later step.

use super::{Dialect, ExecError};
use crate::sql::Pool;

// ====================================================================
// `transaction_pool` — user-facing bi-dialect transaction helper
// ====================================================================
//
// Wraps a closure in a per-backend `BEGIN`/`COMMIT`/`ROLLBACK`. Same
// shape as the existing PgPool [`transaction`] helper, but the
// closure receives a backend-tagged enum the caller pattern-matches
// to get a typed connection. sqlx's `Transaction<DB>` is generic over
// backend, so we can't hand callers a single "any-DB" transaction
// handle without erasing the bind types — exposing the per-arm enum
// keeps users in control of which backend they're talking to and
// avoids surprise driver mismatches.

/// A transaction handle borrowed from one of [`Pool`]'s backend arms.
/// Yielded by [`transaction_pool`]'s closure so callers run their
/// queries against the right driver-typed connection.
///
/// In practice users `match` on the variant and call sqlx-style
/// `.execute(&mut **tx)` against the inner connection. Mixing the
/// two arms in one closure body is fine — Rust ensures the closure
/// runs in exactly one arm per pool variant.
pub enum PoolTx<'a> {
    #[cfg(feature = "postgres")]
    Postgres(sqlx::Transaction<'a, sqlx::Postgres>),
    #[cfg(feature = "mysql")]
    Mysql(sqlx::Transaction<'a, sqlx::MySql>),
    #[cfg(feature = "sqlite")]
    Sqlite(sqlx::Transaction<'a, sqlx::Sqlite>),
}

impl<'a> PoolTx<'a> {
    /// Commit this transaction. Consumes the wrapper.
    ///
    /// # Errors
    /// `sqlx::Error` from the underlying `COMMIT`.
    pub async fn commit(self) -> Result<(), sqlx::Error> {
        match self {
            #[cfg(feature = "postgres")]
            PoolTx::Postgres(tx) => tx.commit().await,
            #[cfg(feature = "mysql")]
            PoolTx::Mysql(tx) => tx.commit().await,
            #[cfg(feature = "sqlite")]
            PoolTx::Sqlite(tx) => tx.commit().await,
        }
    }

    /// Roll back this transaction. Consumes the wrapper. Best-effort —
    /// drop semantics auto-roll-back too if the caller forgets to
    /// invoke this explicitly.
    ///
    /// # Errors
    /// `sqlx::Error` from the underlying `ROLLBACK`.
    pub async fn rollback(self) -> Result<(), sqlx::Error> {
        match self {
            #[cfg(feature = "postgres")]
            PoolTx::Postgres(tx) => tx.rollback().await,
            #[cfg(feature = "mysql")]
            PoolTx::Mysql(tx) => tx.rollback().await,
            #[cfg(feature = "sqlite")]
            PoolTx::Sqlite(tx) => tx.rollback().await,
        }
    }

    /// Return the dialect for this transaction's backend — same
    /// dispatch as [`crate::sql::Pool::dialect`] but sourced from the
    /// `PoolTx` variant rather than the pool. Used internally by the
    /// `_tx` executor helpers to compile SQL against the right backend.
    #[must_use]
    pub fn dialect(&self) -> &'static dyn Dialect {
        match self {
            #[cfg(feature = "postgres")]
            PoolTx::Postgres(_) => crate::sql::postgres::DIALECT,
            #[cfg(feature = "mysql")]
            PoolTx::Mysql(_) => crate::sql::mysql::DIALECT,
            #[cfg(feature = "sqlite")]
            PoolTx::Sqlite(_) => crate::sql::sqlite::DIALECT,
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
/// match &mut tx {
///     #[cfg(feature = "postgres")]
///     rustango::sql::PoolTx::Postgres(t) => {
///         sqlx::query("UPDATE accounts SET balance = balance - $1 WHERE id = $2")
///             .bind(amount).bind(from).execute(&mut **t).await?;
///     }
///     #[cfg(feature = "mysql")]
///     rustango::sql::PoolTx::Mysql(t) => {
///         sqlx::query("UPDATE accounts SET balance = balance - ? WHERE id = ?")
///             .bind(amount).bind(from).execute(&mut **t).await?;
///     }
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
pub async fn transaction_pool(pool: &Pool) -> Result<PoolTx<'_>, ExecError> {
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => Ok(PoolTx::Postgres(pg.begin().await?)),
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => Ok(PoolTx::Mysql(my.begin().await?)),
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => Ok(PoolTx::Sqlite(sq.begin().await?)),
    }
}
