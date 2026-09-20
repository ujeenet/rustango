//! SQL compilation and execution for rustango.
//!
//! The `Clause` IR (in `rustango-core`) is dialect-neutral. This crate
//! contains the writers that turn the IR into a parameterized statement
//! per dialect, plus the async executor that binds and runs them. v0.1
//! ships Postgres only; `SQLite` and `MySQL` slot in as additional
//! `Dialect` arms in v0.2+.

mod array;
mod auto;
mod backend;
mod compiled;
/// Turning a driver's connect failure into something an operator can
/// act on — which host, which cause, what to change.
pub mod connect_diagnosis;
mod dialect;
mod error;
mod executor;
mod foreign_key;
mod geometry;
mod hstore;
pub mod m2m;
#[doc(hidden)]
pub mod model_shortcuts;
// The three dialect emitters are pure IR-to-string compilation — no
// driver, no sqlx. Gating them on their driver feature made a
// tri-dialect *emission* test impossible to compile unless the binary
// also linked all three drivers, which is the opposite of the point.
// `postgres` was always ungated; these two now match it.
mod mysql;
mod pool;
mod postgres;
mod range;
mod sqlite;
mod vector;
mod writers;

pub use array::Array;
pub use auto::Auto;
pub use backend::{
    apply_auto_pk, try_get_returning, try_get_returning_my, try_get_returning_sqlite,
    AssignAutoPkPool, MyReturningRow, MysqlAutoIdSet, PgReturningRow, SqliteReturningRow,
};
pub use compiled::CompiledStatement;
pub use connect_diagnosis::{ConnectDiagnosis, ConnectFault};
pub use dialect::Dialect;
pub use error::{is_mysql_dup_index_error, is_pg_dup_object_error, ExecError, SqlError};
pub use geometry::{Point, SRID_WGS84};
pub use hstore::HStore;
pub use range::Range;
pub use vector::Vector;
// Always-on: tri-dialect entry points + traits that don't pin on PG.
#[cfg(feature = "mysql")]
pub use executor::row_to_json_my;
#[cfg(feature = "sqlite")]
pub use executor::row_to_json_sqlite;
pub use executor::{
    atomic, bulk_insert_pool, bulk_update_pool, count_rows_pool, delete_pool, delete_tx,
    explain_pool, fetch_aggregate_dict, fetch_aggregate_pool, fetch_dates_pool,
    fetch_datetimes_pool, fetch_paginated_pool, fetch_with_prefetch_filtered,
    fetch_with_prefetch_pool, get_or_create, insert_pool, insert_returning_pool,
    insert_returning_tx, insert_tx, on_commit, on_commit_pending, raw_execute_pool, raw_execute_tx,
    raw_query_pool, raw_query_tx, run_ddl_idempotent, select_one_row_as_json, select_one_row_pool,
    select_rows_as_json, select_rows_pool, select_rows_pool_with_related,
    select_rows_tx_with_related, transaction_pool, update_or_create, update_pool, update_tx,
    CounterPool, ExistsPool, ExplainFormat, ExplainOptions, FetcherPool, FetcherTx, FkPkAccess,
    HasPkValue, InsertReturningPool, LoadRelated, MaybeMyFromRow, MaybeMyLoadRelated,
    MaybeMyScalar, MaybePgFromRow, MaybePgScalar, MaybeSqliteFromRow, MaybeSqliteLoadRelated,
    MaybeSqliteScalar, Page, PoolTx, UpdaterPool,
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

/// Query operations that take a **borrowed executor** rather than a
/// pool — for running against one connection you have already scoped,
/// which is how tenancy schema-mode applies `SET search_path`.
///
/// These lived in the `#[doc(hidden)]` `__macro_internals` module until
/// #1431, under a "do not import" notice, with no supported alternative.
/// Nine in-tree tests, the `cookbook_blog` example — both its request
/// handlers and its chapter-3 test — and **rustango's own library**
/// imported them anyway, because there was no other way to run an
/// aggregate or a prefetch against a specific connection. A
/// prohibition the framework itself violates is not a prohibition:
/// `tenancy::permissions` still calls `__macro_internals::delete_on`
/// today, and the guard in `macro_internals_stays_internal` walks only
/// `tests/` and `examples/`, so it cannot see it (#1519, #1516).
///
/// The macro never emitted `fetch_aggregate_on`,
/// `annotate_count_children{,_on}` or `select_rows_on` at all — they
/// were filed as codegen support and were never that.
///
/// **PostgreSQL only.** They are typed `E: sqlx::Executor<Database =
/// Postgres>`, so unlike the rest of the query surface they are not
/// tri-dialect. That is a real gap, not a design choice; see #1293.
#[cfg(feature = "postgres")]
pub use executor::{
    annotate_count_children, annotate_count_children_on, bulk_insert_on, fetch_aggregate_on,
    fetch_with_prefetch, insert_on, select_rows_on, update_on,
};

/// Hidden path for the `#[derive(Model)]` macro's generic-executor
/// emissions. The functions inside are PG-typed (`E: sqlx::Executor<
/// Database = Postgres>`) and exist purely to support the macro's
/// `Self::save_on` / `Self::delete_on` / `Self::insert_on` /
/// `Self::insert_returning_on` / `Self::bulk_insert_on` codegen that
/// needs a generic executor for tenancy schema-mode `SET search_path`
/// scoping. **Not part of the public API; do not import.**
///
/// Now holds only what the macro actually emits (#1431). The operations
/// callers need are public above; `raw_query_on` and `select_one_row_on`
/// were emitted by nothing and used by nobody, and are gone.
///
/// `delete_on` and `insert_returning_on` stay here because nothing
/// outside codegen has asked for them — the twelve re-exports were
/// promoted on evidence of use, not on symmetry. If you need either
/// against a scoped connection, say so on #1431 rather than importing
/// this module: the point of the issue was that reaching in here is
/// what a missing public API looks like.
#[cfg(feature = "postgres")]
#[doc(hidden)]
pub mod __macro_internals {
    pub use super::executor::{
        bulk_insert_on, delete_on, fetch_with_prefetch, insert_on, insert_returning_on, update_on,
    };
}

#[cfg(feature = "mysql")]
pub use executor::LoadRelatedMy;
#[cfg(feature = "sqlite")]
pub use executor::LoadRelatedSqlite;
pub use foreign_key::ForeignKey;
pub use m2m::{GenericM2MManager, M2MManager};
pub use mysql::MySql;
pub use pool::{configure_pools, Pool, PoolError, PoolTuning};
pub use postgres::Postgres;
/// The canonical `SQLite` timestamp encoder. Gated because its only
/// caller is the `SQLite` bind path, which needs the driver linked.
#[cfg(feature = "sqlite")]
pub(crate) use sqlite::encode_datetime;
pub use sqlite::Sqlite;
/// The sweep's "already canonical?" shape test. Gated with its only
/// reader, which needs a live `SQLite` pool.
#[cfg(feature = "sqlite")]
pub(crate) use sqlite::SQLITE_CANONICAL_GLOB;
/// The one text shape a `SQLite` datetime column may hold.
/// `pub(crate)` because the migration sweep and `audit`'s retention
/// DELETE need it and `sql::sqlite` is a private module — an internal
/// contract between the writers and those two readers, not API.
///
/// Ungated, matching the dialect emitters above: the retention DELETE
/// builds its `SQLite` branch through `Dialect`, and that renderer
/// compiles in every build whether or not the driver is linked.
pub(crate) use sqlite::SQLITE_DATETIME_FORMAT;

/// Re-exported so `#[derive(Model)]` output can name `sqlx` types without
/// requiring downstream crates to add their own dependency on it.
#[doc(hidden)]
pub use sqlx;
