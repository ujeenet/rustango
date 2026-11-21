//! Checked [`BindParams`] onto a sqlx query, one function per backend.
//! A value the backend cannot take is an error, never a panic.

use crate::core::SqlValue;
use crate::sql::compiled::unbindable;
use crate::sql::{BindParams, DialectKind, SqlError};

/// `Query` and `QueryAs`, which share no bind trait in sqlx.
pub(crate) trait Bind<'q, DB: sqlx::Database>: Sized {
    fn push<T: 'q + sqlx::Encode<'q, DB> + sqlx::Type<DB>>(self, value: T) -> Self;
}

impl<'q, DB: sqlx::Database> Bind<'q, DB>
    for sqlx::query::Query<'q, DB, <DB as sqlx::Database>::Arguments<'q>>
{
    fn push<T: 'q + sqlx::Encode<'q, DB> + sqlx::Type<DB>>(self, value: T) -> Self {
        self.bind(value)
    }
}

impl<'q, DB: sqlx::Database, O> Bind<'q, DB>
    for sqlx::query::QueryAs<'q, DB, O, <DB as sqlx::Database>::Arguments<'q>>
{
    fn push<T: 'q + sqlx::Encode<'q, DB> + sqlx::Type<DB>>(self, value: T) -> Self {
        self.bind(value)
    }
}

/// The error for a value no arm of `kind`'s binder takes.
fn refused(value: &SqlValue, kind: DialectKind) -> SqlError {
    SqlError::ValueNotSupportedInDialect {
        value: unbindable(value, kind == DialectKind::Postgres).unwrap_or("this"),
        dialect: kind.name(),
    }
}

/// A NULL with no type attached, so PostgreSQL infers the type from
/// the column it lands in.
///
/// Binding `None::<String>` would send the text OID, and Postgres then
/// rejects the parameter for any non-text column. OID 0 is the wire
/// protocol's "unspecified", so the server resolves the type during
/// Parse. MySQL and SQLite type parameters loosely and don't need this.
#[cfg(feature = "postgres")]
struct UntypedNull;

#[cfg(feature = "postgres")]
impl sqlx::Type<sqlx::Postgres> for UntypedNull {
    fn type_info() -> sqlx::postgres::PgTypeInfo {
        sqlx::postgres::PgTypeInfo::with_oid(sqlx::postgres::types::Oid(0))
    }
}

#[cfg(feature = "postgres")]
impl sqlx::Encode<'_, sqlx::Postgres> for UntypedNull {
    fn encode_by_ref(
        &self,
        _buf: &mut sqlx::postgres::PgArgumentBuffer,
    ) -> Result<sqlx::encode::IsNull, sqlx::error::BoxDynError> {
        Ok(sqlx::encode::IsNull::Yes)
    }
}

/// Bind `params` on Postgres. Arrays bind as typed PG arrays.
///
/// # Errors
/// [`SqlError::ValueNotSupportedInDialect`] for a value Postgres cannot take.
#[cfg(feature = "postgres")]
pub(crate) fn bind_pg<'q, Q: Bind<'q, sqlx::Postgres>>(
    q: Q,
    params: BindParams,
) -> Result<Q, SqlError> {
    params.into_values().into_iter().try_fold(q, bind_one_pg)
}

#[cfg(feature = "postgres")]
fn bind_one_pg<'q, Q: Bind<'q, sqlx::Postgres>>(q: Q, value: SqlValue) -> Result<Q, SqlError> {
    /// The elements as one typed PG array, a NULL kept as a NULL element.
    /// `BindParams::check` refused a mixed array, so `f` maps every other one.
    fn elems<T>(v: Vec<SqlValue>, f: fn(SqlValue) -> Option<T>) -> Vec<Option<T>> {
        v.into_iter().map(f).collect()
    }
    Ok(match value {
        SqlValue::Null => q.push(UntypedNull),
        SqlValue::I16(v) => q.push(v),
        SqlValue::I32(v) => q.push(v),
        SqlValue::I64(v) => q.push(v),
        SqlValue::F32(v) => q.push(v),
        SqlValue::F64(v) => q.push(v),
        SqlValue::Bool(v) => q.push(v),
        SqlValue::String(v) => q.push(v),
        SqlValue::DateTime(v) => q.push(v),
        SqlValue::Date(v) => q.push(v),
        SqlValue::Time(v) => q.push(v),
        SqlValue::Uuid(v) => q.push(v),
        SqlValue::Json(v) => q.push(sqlx::types::Json(v)),
        SqlValue::Decimal(v) => q.push(v),
        SqlValue::Binary(v) => q.push(v),
        // Range literal: bound as text, cast by PG to the column's range type.
        SqlValue::RangeLiteral(s) => q.push(s),
        // hstore: bound as a native `PgHstore`, no text escaping.
        SqlValue::HStore(pairs) => {
            q.push(sqlx::postgres::types::PgHstore(pairs.into_iter().collect()))
        }
        // pgvector: binary wire format via the `Vector` newtype.
        SqlValue::Vector(v) => q.push(crate::sql::Vector(v)),
        // PostGIS: EWKB binary via the `Point` newtype.
        SqlValue::Geometry { x, y, srid } => q.push(crate::sql::Point { x, y, srid }),
        // PG arrays are typed, so elements must be homogeneous.
        SqlValue::Array(v) => match v.iter().find(|e| !matches!(e, SqlValue::Null)) {
            // Empty: `BindParams::check` refused an all-NULL one.
            None => q.push(Vec::<i32>::new()),
            Some(SqlValue::I16(_)) => q.push(elems(v, |e| match e {
                SqlValue::I16(n) => Some(n),
                _ => None,
            })),
            Some(SqlValue::I32(_)) => q.push(elems(v, |e| match e {
                SqlValue::I32(n) => Some(n),
                _ => None,
            })),
            Some(SqlValue::I64(_)) => q.push(elems(v, |e| match e {
                SqlValue::I64(n) => Some(n),
                _ => None,
            })),
            Some(SqlValue::String(_)) => q.push(elems(v, |e| match e {
                SqlValue::String(s) => Some(s),
                _ => None,
            })),
            Some(SqlValue::Bool(_)) => q.push(elems(v, |e| match e {
                SqlValue::Bool(b) => Some(b),
                _ => None,
            })),
            Some(SqlValue::Uuid(_)) => q.push(elems(v, |e| match e {
                SqlValue::Uuid(u) => Some(u),
                _ => None,
            })),
            Some(_) => return Err(refused(&SqlValue::Array(v), DialectKind::Postgres)),
        },
        list @ SqlValue::List(_) => return Err(refused(&list, DialectKind::Postgres)),
    })
}

/// Bind `params` on MySQL or MariaDB.
///
/// # Errors
/// [`SqlError::ValueNotSupportedInDialect`] for a Postgres-only value or a `List`.
#[cfg(feature = "mysql")]
pub(crate) fn bind_my<'q, Q: Bind<'q, sqlx::MySql>>(
    q: Q,
    params: BindParams,
) -> Result<Q, SqlError> {
    params.into_values().into_iter().try_fold(q, |q, value| {
        Ok(match value {
            SqlValue::Null => q.push(None::<String>),
            SqlValue::I16(v) => q.push(v),
            SqlValue::I32(v) => q.push(v),
            SqlValue::I64(v) => q.push(v),
            SqlValue::F32(v) => q.push(v),
            SqlValue::F64(v) => q.push(v),
            SqlValue::Bool(v) => q.push(v),
            SqlValue::String(v) => q.push(v),
            SqlValue::DateTime(v) => q.push(v),
            SqlValue::Date(v) => q.push(v),
            SqlValue::Time(v) => q.push(v),
            // The column is CHAR(36); sqlx would send `Uuid` as 16 raw bytes.
            SqlValue::Uuid(v) => q.push(v.hyphenated()),
            SqlValue::Json(v) => q.push(sqlx::types::Json(v)),
            SqlValue::Decimal(v) => q.push(v),
            SqlValue::Binary(v) => q.push(v),
            other @ (SqlValue::List(_)
            | SqlValue::Array(_)
            | SqlValue::RangeLiteral(_)
            | SqlValue::HStore(_)
            | SqlValue::Vector(_)
            | SqlValue::Geometry { .. }) => return Err(refused(&other, DialectKind::Mysql)),
        })
    })
}

/// Bind `params` on SQLite. `sqlx-sqlite` has no `Decimal` impl, so a
/// decimal binds as text; NUMERIC affinity keeps ~15 digits. The writer
/// wraps the placeholder in `CAST(? AS NUMERIC)` so it compares as a number.
///
/// # Errors
/// [`SqlError::ValueNotSupportedInDialect`] for a Postgres-only value or a `List`.
#[cfg(feature = "sqlite")]
pub(crate) fn bind_sqlite<'q, Q: Bind<'q, sqlx::Sqlite>>(
    q: Q,
    params: BindParams,
) -> Result<Q, SqlError> {
    params.into_values().into_iter().try_fold(q, |q, value| {
        Ok(match value {
            SqlValue::Null => q.push(None::<String>),
            SqlValue::I16(v) => q.push(v),
            SqlValue::I32(v) => q.push(v),
            SqlValue::I64(v) => q.push(v),
            SqlValue::F32(v) => q.push(v),
            SqlValue::F64(v) => q.push(v),
            SqlValue::Bool(v) => q.push(v),
            SqlValue::String(v) => q.push(v),
            // Fixed width, unlike sqlx's 0/3/6/9 fractional digits: SQLite
            // compares datetimes as text, as the DDL default writes them.
            SqlValue::DateTime(v) => q.push(crate::sql::encode_datetime(v)),
            SqlValue::Date(v) => q.push(v),
            SqlValue::Time(v) => q.push(v),
            SqlValue::Uuid(v) => q.push(v),
            SqlValue::Json(v) => q.push(sqlx::types::Json(v)),
            SqlValue::Decimal(v) => q.push(v.to_string()),
            SqlValue::Binary(v) => q.push(v),
            other @ (SqlValue::List(_)
            | SqlValue::Array(_)
            | SqlValue::RangeLiteral(_)
            | SqlValue::HStore(_)
            | SqlValue::Vector(_)
            | SqlValue::Geometry { .. }) => return Err(refused(&other, DialectKind::Sqlite)),
        })
    })
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;

    /// A statement checked for Postgres but run elsewhere errors, not panics.
    #[cfg(feature = "mysql")]
    #[test]
    fn mysql_binder_refuses_a_value_checked_for_postgres() {
        let p = BindParams::check(&crate::sql::Postgres, vec![SqlValue::Vector(vec![1.0])]);
        let q: sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments> =
            sqlx::query("SELECT ?");
        match bind_my(q, p.unwrap()) {
            Err(SqlError::ValueNotSupportedInDialect { value, dialect }) => {
                assert_eq!((value, dialect), ("vector", "mysql"));
            }
            Err(e) => panic!("wrong error: {e}"),
            Ok(_) => panic!("bound a vector on MySQL"),
        }
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn sqlite_binder_refuses_a_value_checked_for_postgres() {
        let array = SqlValue::Array(vec![SqlValue::I64(1)]);
        let p = BindParams::check(&crate::sql::Postgres, vec![SqlValue::I64(1), array]);
        let q: sqlx::query::Query<'_, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'_>> =
            sqlx::query("SELECT ?, ?");
        match bind_sqlite(q, p.unwrap()) {
            Err(SqlError::ValueNotSupportedInDialect { value, dialect }) => {
                assert_eq!((value, dialect), ("array", "sqlite"));
            }
            Err(e) => panic!("wrong error: {e}"),
            Ok(_) => panic!("bound an array on SQLite"),
        }
    }
}
