//! Backend abstraction for macro-emitted code.
//!
//! Slice 17.1 — backend trait dispatch — replaces `#[cfg(feature = "…")]`
//! arms emitted by `#[derive(Model)]` with helper-function calls and
//! type aliases that compile in any consumer-side feature
//! configuration. The cfg evaluation moves entirely into rustango's
//! own crate, where `feature = "postgres"` / `feature = "mysql"` mean
//! what the rustango build was *actually* compiled with.
//!
//! ## Why
//!
//! Pre-17.1, the derive output looked like:
//!
//! ```ignore
//! match _result {
//!     #[cfg(feature = "postgres")]
//!     ::rustango::sql::InsertReturningPool::PgRow(row) => { /* … */ }
//!     #[cfg(feature = "mysql")]
//!     ::rustango::sql::InsertReturningPool::MySqlAutoId(id) => { /* … */ }
//! }
//! ```
//!
//! When this expanded inside a downstream crate (e.g. `rustango-cms`),
//! `cfg(feature = "postgres")` evaluated against the *consumer's* Cargo
//! features — almost always false, since no library re-declares
//! rustango's feature names — and the arm was dropped, leaving a
//! non-exhaustive match against `InsertReturningPool::PgRow(_)`.
//!
//! Now the macro emits one cfg-free call:
//!
//! ```ignore
//! ::rustango::sql::apply_auto_pk_pool(
//!     _result, self,
//!     |slf, row| { slf.id = ::rustango::sql::try_get_returning(row, "id")?; Ok(()) },
//!     |slf, id| { slf.id = ::rustango::sql::Auto::Set(id); Ok(()) },
//! )?;
//! ```
//!
//! The cfg lives inside [`apply_auto_pk_pool`] and on the type aliases
//! [`PgReturningRow`] / [`MyReturningRow`], all in this module. Closure
//! bodies stay typecheckable in either feature config because the
//! aliases resolve to uninhabited types when the feature is off and
//! the helper functions ([`try_get_returning`], [`try_get_returning_my`])
//! short-circuit on those uninhabited values.

use super::ExecError;

/// The Postgres returning row type, aliased so macro-emitted code can
/// name it without a `#[cfg]` guard. Resolves to `sqlx::postgres::PgRow`
/// when rustango was built with `postgres`, otherwise to an uninhabited
/// enum so the relevant code path is statically dead.
#[cfg(feature = "postgres")]
pub type PgReturningRow = sqlx::postgres::PgRow;

/// Uninhabited stand-in for the Postgres returning row when the
/// `postgres` feature is off. No value of this type can be constructed,
/// so any code path that takes a `&PgReturningRow` is unreachable at
/// runtime — the helper functions in this module exploit that to keep
/// macro-emitted closures typecheckable in either feature config.
#[cfg(not(feature = "postgres"))]
pub enum PgReturningRow {}

/// MySQL row alias — same role as [`PgReturningRow`] for the MySQL
/// audit-save SELECT path that decodes BEFORE-snapshot columns.
#[cfg(feature = "mysql")]
pub type MyReturningRow = sqlx::mysql::MySqlRow;

#[cfg(not(feature = "mysql"))]
pub enum MyReturningRow {}

/// Macro-emitted code calls this in place of `sqlx::Row::try_get` on
/// a Postgres returning row, so the call site carries no `#[cfg]`
/// guard. With the `postgres` feature on this is a thin forward to
/// `sqlx::Row::try_get`; with it off, the parameter is uninhabited and
/// the body `match` is empty.
///
/// # Errors
/// `sqlx::Error` from the underlying decode.
#[cfg(feature = "postgres")]
pub fn try_get_returning<'r, T>(row: &'r PgReturningRow, name: &str) -> Result<T, sqlx::Error>
where
    T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    use sqlx::Row as _;
    row.try_get(name)
}

#[cfg(not(feature = "postgres"))]
#[allow(clippy::missing_errors_doc)]
pub fn try_get_returning<T>(row: &PgReturningRow, _name: &str) -> Result<T, sqlx::Error> {
    match *row {}
}

/// MySQL counterpart of [`try_get_returning`] — used by the audit
/// save_pool path to decode BEFORE-snapshot columns from a MySqlRow
/// without leaking `#[cfg]` into the macro output.
///
/// # Errors
/// `sqlx::Error` from the underlying decode.
#[cfg(feature = "mysql")]
pub fn try_get_returning_my<'r, T>(row: &'r MyReturningRow, name: &str) -> Result<T, sqlx::Error>
where
    T: sqlx::Decode<'r, sqlx::MySql> + sqlx::Type<sqlx::MySql>,
{
    use sqlx::Row as _;
    row.try_get(name)
}

#[cfg(not(feature = "mysql"))]
#[allow(clippy::missing_errors_doc)]
pub fn try_get_returning_my<T>(row: &MyReturningRow, _name: &str) -> Result<T, sqlx::Error> {
    match *row {}
}

/// Sealed conversion from MySQL's `LAST_INSERT_ID()` (always i64) into
/// the inner `T` of an `Auto<T>` PK. Implemented for the integer types
/// MySQL can actually fill in via `AUTO_INCREMENT`. Models with non-
/// numeric Auto PKs (e.g. `Auto<Uuid>`) intentionally don't implement
/// this — the resulting trait-bound failure is the right signal that
/// they're Postgres-only.
pub trait MysqlAutoIdSet: Sized + sealed::Sealed {
    /// Convert a `LAST_INSERT_ID()` value into `Self`.
    ///
    /// # Errors
    /// [`ExecError::Sql`] when the `i64` doesn't fit in the target
    /// integer width (i32 overflow).
    fn rustango_from_mysql_auto_id(id: i64) -> Result<Self, ExecError>;
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for i64 {}
    impl Sealed for i32 {}
    impl Sealed for uuid::Uuid {}
    impl Sealed for String {}
    impl Sealed for chrono::DateTime<chrono::Utc> {}
}

impl MysqlAutoIdSet for i64 {
    fn rustango_from_mysql_auto_id(id: i64) -> Result<Self, ExecError> {
        Ok(id)
    }
}

impl MysqlAutoIdSet for i32 {
    fn rustango_from_mysql_auto_id(id: i64) -> Result<Self, ExecError> {
        i32::try_from(id).map_err(|_| {
            ExecError::Sql(super::SqlError::OperatorNotSupportedInDialect {
                op: "i64 → i32 narrowing for AUTO_INCREMENT",
                dialect: "mysql",
            })
        })
    }
}

/// Stub impl so models with `Auto<Uuid>` PKs still compile when the
/// macro emits `<Uuid as MysqlAutoIdSet>::…`. Calling it errors —
/// MySQL `AUTO_INCREMENT` can't fill in a UUID, so any model that
/// reaches this branch is misconfigured for MySQL.
impl MysqlAutoIdSet for uuid::Uuid {
    fn rustango_from_mysql_auto_id(_id: i64) -> Result<Self, ExecError> {
        Err(ExecError::Sql(
            super::SqlError::OperatorNotSupportedInDialect {
                op: "Auto<Uuid> assigned by AUTO_INCREMENT",
                dialect: "mysql",
            },
        ))
    }
}

/// Same shape as the [`Uuid`](uuid::Uuid) impl — a stub so
/// `Auto<String>` models compile, with a runtime error if someone
/// actually tries to use AUTO_INCREMENT to fill in a string PK.
impl MysqlAutoIdSet for String {
    fn rustango_from_mysql_auto_id(_id: i64) -> Result<Self, ExecError> {
        Err(ExecError::Sql(
            super::SqlError::OperatorNotSupportedInDialect {
                op: "Auto<String> assigned by AUTO_INCREMENT",
                dialect: "mysql",
            },
        ))
    }
}

/// Stub for `Auto<DateTime<Utc>>` — DB DEFAULT NOW() fills these in,
/// not AUTO_INCREMENT, so reaching this branch on MySQL means the
/// model's first-Auto field happens to be a timestamp. Surface the
/// mismatch as a clear runtime error rather than silently overwriting
/// the timestamp with `LAST_INSERT_ID()`.
impl MysqlAutoIdSet for chrono::DateTime<chrono::Utc> {
    fn rustango_from_mysql_auto_id(_id: i64) -> Result<Self, ExecError> {
        Err(ExecError::Sql(
            super::SqlError::OperatorNotSupportedInDialect {
                op: "Auto<DateTime> assigned by AUTO_INCREMENT",
                dialect: "mysql",
            },
        ))
    }
}

/// Hidden trait every macro-emitted `#[derive(Model)]` with at least
/// one `Auto<T>` field implements. Lets [`apply_auto_pk_pool`] route
/// an `InsertReturningPool` result to the right per-backend assignment
/// without the macro emitting any `#[cfg]` arms.
///
/// Method bodies live in macro output but are unconditional —
/// they reference [`PgReturningRow`] (which resolves to an
/// uninhabited type when `postgres` is off) and call
/// [`try_get_returning`] (which short-circuits to a no-op match in
/// the same config), so they typecheck regardless of which features
/// the consumer crate happens to enable on rustango.
#[doc(hidden)]
pub trait AssignAutoPkPool {
    /// Assign every `Auto<T>` PK from the Postgres returning row.
    ///
    /// # Errors
    /// `sqlx::Error` from any column decode, wrapped into
    /// [`ExecError`].
    fn __rustango_assign_from_pg_row(&mut self, row: &PgReturningRow) -> Result<(), ExecError>;

    /// Assign the single `Auto<T>` PK from a MySQL `LAST_INSERT_ID()`.
    /// Models with multiple `Auto` columns surface
    /// `OperatorNotSupportedInDialect` from this method.
    ///
    /// # Errors
    /// [`ExecError::Sql`] when the model has more than one `Auto<T>`
    /// column (unsupported on MySQL).
    fn __rustango_assign_from_mysql_id(&mut self, id: i64) -> Result<(), ExecError>;
}

/// Apply an `InsertReturningPool` result to a model's `Auto<T>` PK
/// via the [`AssignAutoPkPool`] trait. Replaces the macro's per-arm
/// `match` so emitted code carries no `#[cfg(feature=…)]` guards.
///
/// # Errors
/// Whatever the dispatched trait method returns.
pub fn apply_auto_pk_pool<M: AssignAutoPkPool + ?Sized>(
    result: super::InsertReturningPool,
    model: &mut M,
) -> Result<(), ExecError> {
    let _ = model;
    match result {
        #[cfg(feature = "postgres")]
        super::InsertReturningPool::PgRow(row) => model.__rustango_assign_from_pg_row(&row),
        #[cfg(feature = "mysql")]
        super::InsertReturningPool::MySqlAutoId(id) => model.__rustango_assign_from_mysql_id(id),
    }
}
