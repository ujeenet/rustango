//! SQL compilation and execution for rustango.
//!
//! The `Clause` IR (in `rustango-core`) is dialect-neutral. This crate
//! contains the writers that turn the IR into a parameterized statement
//! per dialect, plus the async executor that binds and runs them. v0.1
//! ships Postgres only; `SQLite` and `MySQL` slot in as additional
//! `Dialect` arms in v0.2+.

mod auto;
mod backend;
mod compiled;
mod dialect;
mod error;
mod executor;
mod foreign_key;
pub mod m2m;
#[cfg(feature = "mysql")]
mod mysql;
mod pool;
mod postgres;
#[cfg(feature = "sqlite")]
mod sqlite;
mod writers;

pub use auto::Auto;
pub use backend::{
    apply_auto_pk, try_get_returning, try_get_returning_my, try_get_returning_sqlite,
    AssignAutoPkPool, MyReturningRow, MysqlAutoIdSet, PgReturningRow, SqliteReturningRow,
};
pub use compiled::CompiledStatement;
pub use dialect::Dialect;
pub use error::{ExecError, SqlError};
// Always-on: tri-dialect entry points + traits that don't pin on PG.
#[cfg(feature = "mysql")]
pub use executor::row_to_json_my;
#[cfg(feature = "sqlite")]
pub use executor::row_to_json_sqlite;
pub use executor::{
    bulk_insert_pool, bulk_update_pool, count_rows_pool, delete_pool, delete_tx,
    fetch_aggregate_pool, fetch_paginated_pool, fetch_with_prefetch_pool, get_or_create,
    insert_pool, insert_returning_pool, insert_returning_tx, insert_tx, raw_execute_pool,
    raw_query_pool, select_one_row_as_json, select_one_row_pool, select_rows_as_json,
    select_rows_pool, select_rows_pool_with_related, select_rows_tx_with_related, transaction_pool,
    update_or_create, update_pool, update_tx, CounterPool, ExplainFormat, ExplainOptions,
    FetcherPool, FetcherTx, FkPkAccess, HasPkValue, InsertReturningPool, LoadRelated,
    MaybeMyFromRow, MaybeMyLoadRelated, MaybePgFromRow, MaybeSqliteFromRow, MaybeSqliteLoadRelated,
    Page, PoolTx, UpdaterPool,
};
// PG-typed back-compat surface: only re-exported when `postgres` is on.
#[cfg(feature = "postgres")]
pub use executor::{
    annotate_count_children, annotate_count_children_on, bulk_insert, bulk_insert_on, bulk_update,
    bulk_update_on, count_rows, count_rows_on, delete, delete_on, fetch_aggregate,
    fetch_aggregate_on, fetch_with_prefetch, insert, insert_on, insert_returning,
    insert_returning_on, raw_execute, raw_execute_on, raw_query, raw_query_on, row_to_json,
    select_one_row, select_one_row_on, select_rows, select_rows_on, transaction, update, update_on,
    Counter, Deleter, Fetcher, Updater,
};

#[cfg(feature = "mysql")]
pub use executor::LoadRelatedMy;
#[cfg(feature = "sqlite")]
pub use executor::LoadRelatedSqlite;
pub use foreign_key::ForeignKey;
pub use m2m::M2MManager;
#[cfg(feature = "mysql")]
pub use mysql::MySql;
#[cfg(all(feature = "sqlite", feature = "manage"))]
pub(crate) use pool::sqlite_connect_options;
pub use pool::{Pool, PoolError};
pub use postgres::Postgres;
#[cfg(feature = "sqlite")]
pub use sqlite::Sqlite;

/// Re-exported so `#[derive(Model)]` output can name `sqlx` types without
/// requiring downstream crates to add their own dependency on it.
#[doc(hidden)]
pub use sqlx;
