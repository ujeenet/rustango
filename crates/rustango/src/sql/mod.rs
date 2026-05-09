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
    apply_auto_pk_pool, try_get_returning, try_get_returning_my, try_get_returning_sqlite,
    AssignAutoPkPool, MyReturningRow, MysqlAutoIdSet, PgReturningRow, SqliteReturningRow,
};
pub use compiled::CompiledStatement;
pub use dialect::Dialect;
pub use error::{ExecError, SqlError};
pub use executor::{
    annotate_count_children, annotate_count_children_on, bulk_insert, bulk_insert_on,
    bulk_insert_pool, bulk_update, bulk_update_on, bulk_update_pool, count_rows, count_rows_on,
    count_rows_pool, delete, delete_on, delete_pool, fetch_aggregate, fetch_aggregate_on,
    fetch_aggregate_pool, fetch_paginated_pool, fetch_with_prefetch, fetch_with_prefetch_pool,
    insert, insert_on, insert_pool, insert_returning, insert_returning_on, insert_returning_pool,
    raw_execute, raw_execute_on, raw_execute_pool, raw_query, raw_query_on, raw_query_pool,
    select_one_row, select_one_row_on, select_one_row_pool, select_rows, select_rows_on,
    select_rows_pool, select_rows_pool_with_related, transaction, transaction_pool, update,
    update_on, update_pool, Counter, CounterPool, Deleter, ExplainFormat, ExplainOptions, Fetcher,
    FetcherPool, FkPkAccess, HasPkValue, InsertReturningPool, LoadRelated, MaybeMyFromRow,
    MaybeMyLoadRelated, Page, PoolTx, Updater,
};

#[cfg(feature = "mysql")]
pub use executor::LoadRelatedMy;
#[cfg(feature = "sqlite")]
pub use executor::{LoadRelatedSqlite, MaybeSqliteFromRow, MaybeSqliteLoadRelated};
pub use foreign_key::ForeignKey;
pub use m2m::M2MManager;
#[cfg(feature = "mysql")]
pub use mysql::MySql;
pub use pool::{Pool, PoolError};
pub use postgres::Postgres;
#[cfg(feature = "sqlite")]
pub use sqlite::Sqlite;

/// Re-exported so `#[derive(Model)]` output can name `sqlx` types without
/// requiring downstream crates to add their own dependency on it.
#[doc(hidden)]
pub use sqlx;
