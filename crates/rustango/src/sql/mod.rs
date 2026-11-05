//! SQL compilation and execution for rustango.
//!
//! The `Clause` IR (in `rustango-core`) is dialect-neutral. This crate
//! contains the writers that turn the IR into a parameterized statement
//! per dialect — Postgres, `MySQL` and `SQLite` — plus the async executor
//! that binds and runs them. [`Pool`] is the entry point.

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
/// Validated names for tenant database and login DDL.
pub mod server;
mod sqlite;
mod vector;
mod writers;
#[doc(hidden)]
pub use writers::DeletePlan;
pub(crate) use writers::{compile_date_buckets, compile_delete_plan, DateBucket};

pub use array::Array;
pub use auto::Auto;
pub use backend::{
    apply_auto_pk, try_get_returning, try_get_returning_my, try_get_returning_sqlite,
    AssignAutoPkPool, MyReturningRow, MysqlAutoIdSet, PgReturningRow, SqliteReturningRow,
};
pub use compiled::CompiledStatement;
pub use connect_diagnosis::{ConnectDiagnosis, ConnectFault};
pub(crate) use dialect::is_uuid_expr;
#[cfg(any(feature = "cache", feature = "postgres"))]
pub(crate) use dialect::quote_qualified;
// Its callers outside `sql` (media listing, inspectdb) are feature-gated.
#[allow(unused_imports)]
pub(crate) use mysql::ByServer;
// Its callers are feature-gated (PG schemas, SQLite inspectdb).
#[allow(unused_imports)]
pub(crate) use dialect::quote_with;
pub(crate) use dialect::refuse_unbindable_values;
pub(crate) use dialect::split_qualified;
pub use dialect::Dialect;
pub use error::{is_mysql_dup_index_error, is_pg_dup_object_error, ExecError, SqlError};
#[cfg(any(feature = "template_views", feature = "admin", feature = "tenancy"))]
pub(crate) use error::{Refusal, STILL_REFERENCED};
pub use geometry::{Point, SRID_WGS84};
pub use hstore::HStore;
pub use range::Range;
pub use server::{
    AdminRight, Collation, DatabaseName, InvalidName, Login, LoginName, LoginPassword,
    ServerAccount,
};
pub use vector::Vector;
// Always-on: tri-dialect entry points + traits that don't pin on PG.
#[cfg(feature = "jobs")]
pub(crate) use executor::in_any_block;
#[cfg(feature = "jobs-postgres")]
pub(crate) use executor::in_block;
pub(crate) use executor::rolled_back;
#[cfg(feature = "mysql")]
pub use executor::row_to_json_my;
#[cfg(feature = "sqlite")]
pub use executor::row_to_json_sqlite;
#[cfg(feature = "template_views")]
pub(crate) use executor::select_stored_uuids;
pub use executor::{
    __reverse_fk_key, __rustango_dedupe_by_pk, __rustango_delete_all_or_none,
    __rustango_in_batches, __rustango_one_in_list, __rustango_require_join, atomic,
    atomic_immediate, bulk_insert_pool, bulk_insert_tx, bulk_update_pool, count_rows_pool,
    count_tx, delete_pool, delete_tx, explain_pool, fetch_aggregate_dict, fetch_aggregate_pool,
    fetch_dates_pool, fetch_datetimes_pool, fetch_paginated_pool, fetch_with_prefetch_filtered,
    fetch_with_prefetch_pool, fetch_with_prefetch_related, fetch_with_prefetch_related_filtered,
    get_or_create, get_or_create_tx, insert_or_ignore, insert_pool, insert_returning_pool,
    insert_returning_tx, insert_tx, on_commit, on_commit_pending, raw_execute_pool, raw_execute_tx,
    raw_query_pool, raw_query_tx, run_ddl_idempotent, select_one_row_as_json, select_one_row_pool,
    select_rows_as_json, select_rows_pool, select_rows_pool_with_related,
    select_rows_tx_with_related, transaction_pool, try_get_flat, try_get_flat_my,
    try_get_flat_sqlite, update_or_create, update_pool, update_tx, write_transaction, AtomicTx,
    CounterPool, ExistsPool, ExplainFormat, ExplainOptions, FetcherPool, FetcherTx, FkPkAccess,
    FlatScalar, HasPkValue, InsertReturningPool, LoadRelated, MaybeMyFromRow, MaybeMyLoadRelated,
    MaybeMyScalar, MaybePgFromRow, MaybePgScalar, MaybeSqliteFromRow, MaybeSqliteLoadRelated,
    MaybeSqliteScalar, Page, PoolConn, PoolTx, PooledConn, SqlExecutor, TxConn, TxGuard,
    UpdaterPool,
};
pub(crate) use executor::{
    bulk_insert_pks_tx, compile_bulk_update_batches, count_rows_on, execute_batches_tx,
    execute_unprepared_pool, fetch_in_chunks, fetch_select_in_chunks, fetch_values_list_on,
    inserted_pk, rows_per_batch, run_delete_statements_tx, truncate_refusal, truncate_tx,
    write_tx,
};
#[cfg(feature = "admin")]
pub(crate) use executor::{select_one_row_as_json_tx, select_rows_as_json_tx};
pub(crate) use executor::{select_rows_as_json_with, CellDecode, OnBadCell};
#[cfg(any(feature = "admin", feature = "tenancy", feature = "template_views"))]
pub(crate) use executor::{stored_key, stored_keys};
pub use executor::{truncate, truncate_cascade, TruncateMode};
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

/// Operations on a borrowed [`SqlExecutor`] rather than a pool, for one
/// connection you have already scoped (tenancy schema mode's `search_path`).
pub use executor::{annotate_count_children_on, fetch_aggregate_on};
/// Writes on any [`SqlExecutor`].
pub use executor::{bulk_insert_on, insert_on, update_on};

/// The write path the `#[derive(Model)]` methods share. Not public API.
#[doc(hidden)]
pub mod __macro_internals {
    pub use super::executor::refuse_audited;
    pub use super::executor::{
        bulk_insert_on, delete_on, insert_on, insert_returning_on, update_on,
    };
    pub use super::executor::{row_saved, write_tx, WriteTx};
}

/// The per-dialect `SqlValue` binders, so nothing outside `executor`
/// has to hand-copy what `bind_match_*!` expands to.
///
/// `audit` carried 116 lines of exactly that copy across three
/// dialects, and the SQLite one silently missed #1464's encoding change
/// because a copy cannot track the macro it was copied from.
#[cfg(feature = "postgres")]
pub(crate) use executor::bind_query;
#[cfg(feature = "mysql")]
pub(crate) use executor::bind_query_my;
#[cfg(feature = "sqlite")]
pub(crate) use executor::bind_query_sqlite;
#[cfg(feature = "mysql")]
pub use executor::LoadRelatedMy;
#[cfg(feature = "sqlite")]
pub use executor::LoadRelatedSqlite;
pub use foreign_key::ForeignKey;
pub use m2m::{GenericM2MManager, M2MManager};
pub use mysql::MySql;
#[cfg(all(feature = "postgres", feature = "tenancy"))]
pub(crate) use pool::tag_schema_scope;
pub(crate) use pool::PoolId;
#[cfg(feature = "tenancy")]
pub(crate) use pool::{checkout_check, tuning as pool_tuning};
pub use pool::{configure_pools, Pool, PoolError, PoolTuning};
pub use postgres::Postgres;
/// The canonical `SQLite` timestamp encoder. Gated because its only
/// caller is the `SQLite` bind path, which needs the driver linked.
#[cfg(feature = "sqlite")]
pub(crate) use sqlite::encode_datetime;
#[cfg(feature = "sqlite")]
pub(crate) use sqlite::is_now_expr;
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
