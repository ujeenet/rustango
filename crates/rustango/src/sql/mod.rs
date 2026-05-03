//! SQL compilation and execution for rustango.
//!
//! The `Clause` IR (in `rustango-core`) is dialect-neutral. This crate
//! contains the writers that turn the IR into a parameterized statement
//! per dialect, plus the async executor that binds and runs them. v0.1
//! ships Postgres only; `SQLite` and `MySQL` slot in as additional
//! `Dialect` arms in v0.2+.

mod auto;
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
mod writers;

pub use auto::Auto;
pub use compiled::CompiledStatement;
pub use dialect::Dialect;
pub use error::{ExecError, SqlError};
pub use executor::{
    annotate_count_children, annotate_count_children_on, bulk_insert, bulk_insert_on,
    bulk_insert_pool, bulk_update, bulk_update_on, bulk_update_pool, count_rows, count_rows_on,
    count_rows_pool, delete, delete_on, delete_pool, fetch_aggregate, fetch_aggregate_on,
    fetch_with_prefetch, insert, insert_on, insert_pool, insert_returning, insert_returning_on,
    raw_execute, raw_execute_on, raw_execute_pool, raw_query, raw_query_on, select_one_row,
    select_rows, transaction, update, update_on, update_pool, Counter, Deleter, Fetcher,
    FkPkAccess, HasPkValue, LoadRelated, Page, Updater,
};
pub use foreign_key::ForeignKey;
pub use m2m::M2MManager;
#[cfg(feature = "mysql")]
pub use mysql::MySql;
pub use pool::{Pool, PoolError};
pub use postgres::Postgres;

/// Re-exported so `#[derive(Model)]` output can name `sqlx` types without
/// requiring downstream crates to add their own dependency on it.
#[doc(hidden)]
pub use sqlx;
