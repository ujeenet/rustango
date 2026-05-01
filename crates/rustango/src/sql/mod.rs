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
mod postgres;

pub use auto::Auto;
pub use compiled::CompiledStatement;
pub use dialect::Dialect;
pub use error::{ExecError, SqlError};
pub use executor::{
    bulk_insert, bulk_insert_on, count_rows, count_rows_on, delete, delete_on, insert, insert_on,
    insert_returning, insert_returning_on, select_one_row, select_rows, update, update_on,
    Counter, Deleter, Fetcher, FkPkAccess, HasPkValue, LoadRelated, Page, Updater, annotate_count_children, annotate_count_children_on, fetch_with_prefetch,
};
pub use foreign_key::ForeignKey;
pub use postgres::Postgres;

/// Re-exported so `#[derive(Model)]` output can name `sqlx` types without
/// requiring downstream crates to add their own dependency on it.
#[doc(hidden)]
pub use sqlx;
