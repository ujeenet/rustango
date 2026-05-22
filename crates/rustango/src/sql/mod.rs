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
    atomic, bulk_insert_pool, bulk_update_pool, count_rows_pool, delete_pool, delete_tx,
    explain_pool, fetch_aggregate_pool, fetch_dates_pool, fetch_datetimes_pool,
    fetch_paginated_pool, fetch_with_prefetch_filtered_pool, fetch_with_prefetch_pool,
    get_or_create, insert_pool, insert_returning_pool, insert_returning_tx, insert_tx, on_commit,
    on_commit_pending, raw_execute_pool, raw_query_pool, select_one_row_as_json,
    select_one_row_pool, select_rows_as_json, select_rows_pool, select_rows_pool_with_related,
    select_rows_tx_with_related, transaction_pool, update_or_create, update_pool, update_tx,
    CounterPool, ExistsPool, ExplainFormat, ExplainOptions, FetcherPool, FetcherTx, FkPkAccess,
    HasPkValue, InsertReturningPool, LoadRelated, MaybeMyFromRow, MaybeMyLoadRelated,
    MaybePgFromRow, MaybeSqliteFromRow, MaybeSqliteLoadRelated, Page, PoolTx, UpdaterPool,
};
// PG-typed back-compat surface gone (issue #270 / T1.8 waves 1–4):
// the entire family of `_on` functions + `&PgPool` wrappers + the
// `Fetcher`/`Counter`/`Updater`/`Deleter` extension traits is deleted
// from the public API. Use the tri-dialect `_pool` family
// (`insert_pool`, `update_pool`, `select_rows_pool`, `count_rows_pool`,
// …) or the inherent `_on` methods on QuerySet / UpdateBuilder
// (`fetch_on`, `count_on`, `delete_on`, `execute_on`). `row_to_json` is
// still useful for raw-row decoders and stays exported.
#[cfg(feature = "postgres")]
pub use executor::row_to_json;

/// Hidden path for the `#[derive(Model)]` macro's generic-executor
/// emissions. The functions inside are PG-typed (`E: sqlx::Executor<
/// Database = Postgres>`) and exist purely to support the macro's
/// `Self::save_on` / `Self::delete_on` / `Self::insert_on` /
/// `Self::insert_returning_on` / `Self::bulk_insert_on` codegen that
/// needs a generic executor for tenancy schema-mode `SET search_path`
/// scoping. **Not part of the public API; do not import.**
#[cfg(feature = "postgres")]
#[doc(hidden)]
pub mod __macro_internals {
    pub use super::executor::{
        annotate_count_children, annotate_count_children_on, bulk_insert_on, delete_on,
        fetch_aggregate_on, fetch_with_prefetch, insert_on, insert_returning_on, raw_query_on,
        select_one_row_on, select_rows_on, update_on,
    };
}

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
