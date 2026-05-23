//! Tri-dialect marker traits for the `_pool` executor surface.
//!
//! Each marker trait is feature-gated:
//! - When the relevant backend feature (`postgres` / `mysql` / `sqlite`) is on,
//!   the trait has a real bound (e.g. `FromRow<PgRow>`).
//! - When the feature is off, the trait is empty and blanket-impl'd for every
//!   `T`, so the bound trivially holds at non-PG / non-MySQL / non-SQLite
//!   call sites.
//!
//! This lets the `_pool` family carry uniform `where T: MaybePgFromRow +
//! MaybeMyFromRow + MaybeSqliteFromRow` bounds without per-feature signature
//! divergence. Models derived via `#[derive(Model)]` get the real impls
//! materialized by the macro's cfg-gated emission.
//!
//! Extracted from `executor/mod.rs` as part of #116 step 8.

// ====================================================================
// `&Pool` FromRow dispatch — Phase B (v0.23.0-batch6)
// ====================================================================
//
// `select_rows_pool` / `Fetcher::fetch` for `&Pool`. The trait bound on
// `T` needs to flex: when rustango is built with the `mysql` feature,
// it must also include `FromRow<MySqlRow>`; without it, the MySql
// variant doesn't exist and the bound shouldn't either. The
// [`MaybeMyFromRow`] marker trait below absorbs that conditionality —
// it's auto-implemented for every type when `mysql` is off, and
// requires `FromRow<MySqlRow>` when on. Models derived via
// `#[derive(Model)]` get the right impl automatically: the proc macro
// emits a call to the cfg-gated `__impl_my_from_row!` macro_rules so
// the MySQL impl materializes when (and only when) it's needed.

/// Marker trait used as a feature-gated `FromRow<MySqlRow>` bound on
/// the `_pool` `FromRow`-using executor functions. Auto-implemented
/// for every `T` when rustango is built without the `mysql` feature
/// (so PG-only call sites compile unchanged); requires
/// `FromRow<MySqlRow>` when `mysql` is on.
#[cfg(feature = "mysql")]
pub trait MaybeMyFromRow: for<'r> sqlx::FromRow<'r, sqlx::mysql::MySqlRow> {}
#[cfg(feature = "mysql")]
impl<T> MaybeMyFromRow for T where T: for<'r> sqlx::FromRow<'r, sqlx::mysql::MySqlRow> {}
#[cfg(not(feature = "mysql"))]
pub trait MaybeMyFromRow {}
#[cfg(not(feature = "mysql"))]
impl<T> MaybeMyFromRow for T {}

/// MySQL counterpart of [`LoadRelated`]. The proc-macro emits this
/// alongside the existing `LoadRelated` impl whenever rustango is
/// built with the `mysql` feature, so `select_related` joins can
/// stitch parents onto FK fields when decoding from a `MySqlRow`.
///
/// FK-less models get a no-op impl; the `MaybeMyLoadRelated` marker
/// trait wraps this in the same cfg-gated way as
/// `MaybeMyFromRow` so executor bounds resolve in either feature
/// config.
#[cfg(feature = "mysql")]
pub trait LoadRelatedMy {
    /// Same contract as [`LoadRelated::__rustango_load_related`] —
    /// returns `Ok(true)` when `field_name` matched a known FK and
    /// the parent was decoded successfully, `Ok(false)` for unknown
    /// field names (graceful skip).
    ///
    /// # Errors
    /// `sqlx::Error` from `try_get` decoding the joined columns.
    fn __rustango_load_related_my(
        &mut self,
        row: &sqlx::mysql::MySqlRow,
        field_name: &str,
        alias: &str,
    ) -> Result<bool, sqlx::Error>;
}

/// Marker trait used as a feature-gated `LoadRelatedMy` bound on
/// future `_pool` join-decoding executor functions. Same shape as
/// [`MaybeMyFromRow`] — auto-implemented for every `T` when
/// rustango is built without `mysql`; requires `LoadRelatedMy`
/// when on.
#[cfg(feature = "mysql")]
pub trait MaybeMyLoadRelated: LoadRelatedMy {}
#[cfg(feature = "mysql")]
impl<T> MaybeMyLoadRelated for T where T: LoadRelatedMy {}
#[cfg(not(feature = "mysql"))]
pub trait MaybeMyLoadRelated {}
#[cfg(not(feature = "mysql"))]
impl<T> MaybeMyLoadRelated for T {}

// ---- v0.35 — Postgres parallel of the MySQL/SQLite marker traits ----
//
// Lets the tri-dialect `_pool` executor functions bound `T` with
// `MaybePgFromRow + MaybeMyFromRow + MaybeSqliteFromRow` so the
// signature is satisfiable in any feature configuration. When
// `postgres` is on the trait requires `FromRow<PgRow>`; when off
// it's a marker trait blanket-impl'd for every `T`, so the bound
// trivially holds and the `Pool::Postgres` arm (also gated) never
// instantiates.

#[cfg(feature = "postgres")]
pub trait MaybePgFromRow: for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> {}
#[cfg(feature = "postgres")]
impl<T> MaybePgFromRow for T where T: for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> {}
#[cfg(not(feature = "postgres"))]
pub trait MaybePgFromRow {}
#[cfg(not(feature = "postgres"))]
impl<T> MaybePgFromRow for T {}

// ---- v0.27 Phase 3 — SQLite parallels of the MySQL marker traits ----
//
// Same shape as `MaybeMyFromRow` / `LoadRelatedMy` /
// `MaybeMyLoadRelated`, gated on `sqlite` instead of `mysql`. The
// `_pool` executor functions add these as additional bounds on `T`
// so a `Pool::Sqlite` arm can decode `T` from a `SqliteRow`.

#[cfg(feature = "sqlite")]
pub trait MaybeSqliteFromRow: for<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> {}
#[cfg(feature = "sqlite")]
impl<T> MaybeSqliteFromRow for T where T: for<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> {}
#[cfg(not(feature = "sqlite"))]
#[allow(dead_code)]
pub trait MaybeSqliteFromRow {}
#[cfg(not(feature = "sqlite"))]
impl<T> MaybeSqliteFromRow for T {}

/// SQLite counterpart of [`LoadRelated`] / [`LoadRelatedMy`]. The
/// proc-macro emits this alongside the Postgres + MySQL impls so
/// `select_related` joins can stitch parents onto FK fields when
/// decoding from a `SqliteRow`.
#[cfg(feature = "sqlite")]
pub trait LoadRelatedSqlite {
    /// Same contract as [`LoadRelated::__rustango_load_related`].
    ///
    /// # Errors
    /// `sqlx::Error` from `try_get` decoding the joined columns.
    fn __rustango_load_related_sqlite(
        &mut self,
        row: &sqlx::sqlite::SqliteRow,
        field_name: &str,
        alias: &str,
    ) -> Result<bool, sqlx::Error>;
}

#[cfg(feature = "sqlite")]
pub trait MaybeSqliteLoadRelated: LoadRelatedSqlite {}
#[cfg(feature = "sqlite")]
impl<T> MaybeSqliteLoadRelated for T where T: LoadRelatedSqlite {}
#[cfg(not(feature = "sqlite"))]
#[allow(dead_code)]
pub trait MaybeSqliteLoadRelated {}
#[cfg(not(feature = "sqlite"))]
impl<T> MaybeSqliteLoadRelated for T {}
