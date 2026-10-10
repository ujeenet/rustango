//! `.values()` and `.values_list()`: fetch chosen columns as plain
//! data instead of decoding whole models.
//!
//! Each backend decodes a cell into a `SqlValue` its own way, then
//! the `fetch_values_*` functions shape the rows into dicts, tuples
//! or a flat list.

#[cfg(feature = "postgres")]
use sqlx::postgres::{PgArguments, PgRow};
#[cfg(feature = "postgres")]
use sqlx::query::Query;

use super::ExecError;
use crate::core::{AggregateQuery, SelectQuery, SqlValue};
use crate::sql::Pool;
use crate::sql::{
    MaybeMyFromRow, MaybeMyLoadRelated, MaybePgFromRow, MaybeSqliteFromRow, MaybeSqliteLoadRelated,
    UpdaterPool as _,
};

#[cfg(feature = "postgres")]
use super::bind_query;
#[cfg(feature = "mysql")]
use super::bind_query_my;
#[cfg(feature = "sqlite")]
use super::bind_query_sqlite;

/// Decode column `i` of a row into a `SqlValue`. It tries scalars
/// first and JSON after, the same order the aggregate decoder uses,
/// so both agree on mixed types. A NULL or an unknown type gives
/// [`SqlValue::Null`].
#[cfg(feature = "postgres")]
fn pg_cell_to_sqlvalue(row: &PgRow, i: usize) -> SqlValue {
    use sqlx::Row as _;
    if let Ok(v) = row.try_get::<i64, _>(i) {
        SqlValue::I64(v)
    } else if let Ok(v) = row.try_get::<i32, _>(i) {
        SqlValue::I32(v)
    } else if let Ok(v) = row.try_get::<f64, _>(i) {
        SqlValue::F64(v)
    } else if let Ok(v) = row.try_get::<bool, _>(i) {
        SqlValue::Bool(v)
    } else if let Ok(v) = row.try_get::<rust_decimal::Decimal, _>(i) {
        // `NUMERIC`, which PG widens a `SUM` to. Neither the i64 nor
        // the f64 probe decodes it, so it would come back as Null.
        SqlValue::Decimal(v)
    } else if let Ok(v) = row.try_get::<uuid::Uuid, _>(i) {
        SqlValue::Uuid(v)
    } else if let Some(v) = chrono_cell(row, i) {
        v
    } else if let Ok(v) = row.try_get::<String, _>(i) {
        SqlValue::String(v)
    } else if let Ok(v) = row.try_get::<serde_json::Value, _>(i) {
        SqlValue::Json(v)
    } else if let Ok(v) = row.try_get::<Vec<u8>, _>(i) {
        SqlValue::Binary(v)
    } else {
        SqlValue::Null
    }
}

#[cfg(any(feature = "mysql", feature = "sqlite"))]
use crate::core::joins::joined_label;

/// A leading result column: its output name and the type its model declares.
#[cfg(any(feature = "mysql", feature = "sqlite"))]
type TypedCol = (
    std::borrow::Cow<'static, str>,
    Option<crate::core::FieldType>,
);

/// Per result column, the type its model declares. MySQL and SQLite
/// store a UUID as text and MySQL a bool as an integer, so only the model tells.
///
/// Only the leading `typed` columns count: an annotation or aggregate alias that
/// shares a column name (`SUM(x) AS flag`) is not that column (#2296).
#[cfg(any(feature = "mysql", feature = "sqlite"))]
fn column_types<R: sqlx::Row>(
    rows: &[R],
    typed: &[TypedCol],
) -> Vec<Option<crate::core::FieldType>> {
    use sqlx::Column as _;
    rows.first().map_or_else(Vec::new, |row| {
        row.columns()
            .iter()
            .enumerate()
            .map(|(i, c)| {
                typed
                    .get(i)
                    .filter(|(name, _)| name == c.name())
                    .and_then(|(_, ty)| *ty)
            })
            .collect()
    })
}

/// The model columns a SELECT emits first: its projection, else every
/// field, then each join's projected `alias__col`.
#[cfg(any(feature = "mysql", feature = "sqlite"))]
fn select_model_cols(query: &SelectQuery) -> Vec<TypedCol> {
    let model = query.model;
    let typed = |c: &'static str| (c.into(), model.field_by_column(c).map(|f| f.ty));
    let mut cols: Vec<TypedCol> = match &query.projection {
        Some(cols) => cols.iter().copied().map(typed).collect(),
        None => model.scalar_fields().map(|f| typed(f.column)).collect(),
    };
    for j in &query.joins {
        cols.extend(j.project.iter().map(|&c| {
            let ty = j.target.field_by_column(c).map(|f| f.ty);
            (joined_label(j.alias, c).into(), ty)
        }));
    }
    cols
}

/// An aggregate's group columns. A joined `alias.col` comes back as
/// `alias__col`, typed by the joined model (#2322).
#[cfg(any(feature = "mysql", feature = "sqlite"))]
fn aggregate_group_cols(query: &AggregateQuery) -> Vec<TypedCol> {
    let model = query.model;
    query
        .group_by
        .iter()
        .map(|&col| match col.split_once('.') {
            None => (col.into(), model.field_by_column(col).map(|f| f.ty)),
            Some((alias, c)) => {
                let target = if alias == model.table {
                    Some(model)
                } else {
                    let derived = query.source.iter().flat_map(|s| s.joins.iter());
                    query
                        .joins
                        .iter()
                        .chain(derived)
                        .find(|j| j.alias == alias)
                        .map(|j| j.target)
                };
                let ty = target.and_then(|m| m.field_by_column(c)).map(|f| f.ty);
                (joined_label(alias, c).into(), ty)
            }
        })
        .collect()
}

#[cfg(feature = "mysql")]
fn my_cell_to_sqlvalue(
    row: &sqlx::mysql::MySqlRow,
    i: usize,
    ty: Option<crate::core::FieldType>,
) -> SqlValue {
    use crate::core::FieldType;
    use sqlx::{Column as _, Row as _, TypeInfo as _};
    if ty == Some(FieldType::Bool) {
        if let Ok(Some(b)) = row.try_get::<Option<bool>, _>(i) {
            return SqlValue::Bool(b);
        }
    }
    // A JSON column decodes neither as String nor as bytes (#2296).
    if row.column(i).type_info().name() == "JSON" {
        if let Ok(Some(v)) = row.try_get::<Option<serde_json::Value>, _>(i) {
            return SqlValue::Json(v);
        }
    }
    if ty == Some(FieldType::Uuid) {
        if let Ok(Some(s)) = row.try_get::<Option<String>, _>(i) {
            if let Ok(u) = uuid::Uuid::parse_str(&s) {
                return SqlValue::Uuid(u);
            }
        }
    }
    // `COUNT(*)` and the window ranking functions return BIGINT
    // UNSIGNED, which the i64 probe cannot decode. sqlx's permissive
    // bool decode would then claim it and lose the number, so check
    // the column type first. A real boolean column is not UNSIGNED,
    // so it still reaches the bool probe below.
    if row.column(i).type_info().name().contains("UNSIGNED") {
        if let Ok(v) = row.try_get::<u64, _>(i) {
            // A value above i64::MAX becomes a string rather than
            // saturating, though no real rank gets that large.
            return i64::try_from(v)
                .map_or_else(|_| SqlValue::String(v.to_string()), SqlValue::I64);
        }
        if let Ok(v) = row.try_get::<u32, _>(i) {
            return SqlValue::I64(i64::from(v));
        }
    }
    if let Ok(v) = row.try_get::<i64, _>(i) {
        SqlValue::I64(v)
    } else if let Ok(v) = row.try_get::<i32, _>(i) {
        SqlValue::I32(v)
    } else if let Ok(v) = row.try_get::<f64, _>(i) {
        SqlValue::F64(v)
    } else if let Ok(v) = row.try_get::<bool, _>(i) {
        SqlValue::Bool(v)
    } else if let Ok(v) = row.try_get::<rust_decimal::Decimal, _>(i) {
        // `DECIMAL`, which a `SUM` over an integer column returns.
        // Neither the i64 nor the f64 probe decodes it.
        SqlValue::Decimal(v)
    } else if let Some(v) = chrono_cell(row, i) {
        v
    } else if let Ok(v) = row.try_get::<String, _>(i) {
        SqlValue::String(v)
    } else if let Ok(v) = row.try_get::<Vec<u8>, _>(i) {
        SqlValue::Binary(v)
    } else {
        SqlValue::Null
    }
}

/// A date or timestamp cell, which the other probes leave as `Null` (#2004).
#[cfg(any(feature = "postgres", feature = "mysql"))]
fn chrono_cell<'r, R>(row: &'r R, i: usize) -> Option<SqlValue>
where
    R: sqlx::Row,
    usize: sqlx::ColumnIndex<R>,
    chrono::DateTime<chrono::Utc>: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    chrono::NaiveDateTime: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    chrono::NaiveDate: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    if let Ok(v) = row.try_get::<chrono::DateTime<chrono::Utc>, _>(i) {
        Some(SqlValue::DateTime(v))
    } else if let Ok(v) = row.try_get::<chrono::NaiveDateTime, _>(i) {
        Some(SqlValue::DateTime(v.and_utc()))
    } else {
        row.try_get::<chrono::NaiveDate, _>(i)
            .ok()
            .map(SqlValue::Date)
    }
}

#[cfg(feature = "sqlite")]
fn sqlite_cell_to_sqlvalue(
    row: &sqlx::sqlite::SqliteRow,
    i: usize,
    ty: Option<crate::core::FieldType>,
) -> SqlValue {
    use sqlx::{Row as _, TypeInfo as _, ValueRef as _};
    let is_uuid = ty == Some(crate::core::FieldType::Uuid);
    // SQLite is dynamically typed, and an expression column such as
    // a scalar subquery has a storage class but no useful declared
    // type. `try_get::<T>` checks against the declared type, which
    // reads as INTEGER here, so `try_get::<i64>` would turn a text
    // value into `0` while `try_get::<String>` would fail outright.
    //
    // So when the runtime storage class is TEXT, read the bytes with
    // `try_get_unchecked`. Everything else goes to the probe below,
    // which handles it correctly — including correlated aggregates,
    // whose `type_info()` SQLite also misreports.
    if let Ok(raw) = row.try_get_raw(i) {
        let is_null = raw.is_null();
        let (is_text, is_blob) = {
            let storage = raw.type_info();
            (storage.name() == "TEXT", storage.name() == "BLOB")
        };
        // Decode while `raw` is still in scope: calling `type_info()`
        // inline just before this can make it fail for no reason.
        let as_text = row.try_get_unchecked::<String, _>(i);
        // A NULL would decode as `0` below (#1766).
        if is_null {
            return SqlValue::Null;
        }
        if is_text {
            return match as_text {
                Ok(s) if is_uuid => {
                    uuid::Uuid::parse_str(&s).map_or(SqlValue::String(s), SqlValue::Uuid)
                }
                // JSON is stored as text; read it as `Json` like PG and MySQL.
                Ok(s) if ty == Some(crate::core::FieldType::Json) => {
                    serde_json::from_str(&s).map_or(SqlValue::String(s), SqlValue::Json)
                }
                Ok(s) => SqlValue::String(s),
                Err(_) => SqlValue::Null,
            };
        }
        // sqlx writes a Uuid as 16 raw bytes; no probe below reads a BLOB.
        if is_blob {
            return match row.try_get_unchecked::<Vec<u8>, _>(i) {
                Ok(b) if is_uuid && b.len() == 16 => {
                    uuid::Uuid::from_slice(&b).map_or(SqlValue::Binary(b), SqlValue::Uuid)
                }
                Ok(b) => SqlValue::Binary(b),
                Err(_) => SqlValue::Null,
            };
        }
    }
    // A bool column stores an integer; read it as `Bool` like PG (#2296).
    if ty == Some(crate::core::FieldType::Bool) {
        if let Ok(v) = row.try_get_unchecked::<bool, _>(i) {
            return SqlValue::Bool(v);
        }
    }
    if let Ok(v) = row.try_get::<i64, _>(i) {
        SqlValue::I64(v)
    } else if let Ok(v) = row.try_get::<i32, _>(i) {
        SqlValue::I32(v)
    } else if let Ok(v) = row.try_get::<f64, _>(i) {
        SqlValue::F64(v)
    } else if let Ok(v) = row.try_get::<bool, _>(i) {
        SqlValue::Bool(v)
    } else if let Ok(v) = row.try_get::<String, _>(i) {
        SqlValue::String(v)
    } else {
        SqlValue::Null
    }
}

/// Run a [`SelectQuery`] with a projection and return each row as a
/// map from column name to value. Backs
/// [`crate::query::ValuesQuerySet::fetch`].
///
/// # Errors
/// SQL compilation or driver failure.
pub async fn fetch_values_dict(
    pool: &Pool,
    query: &SelectQuery,
) -> Result<Vec<std::collections::HashMap<String, SqlValue>>, ExecError> {
    let stmt = pool.dialect().compile_select(query)?;
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => {
            let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query(q, v);
            }
            let rows = q.fetch_all(pg).await?;
            let mut out = Vec::with_capacity(rows.len());
            for row in &rows {
                use sqlx::Column as _;
                use sqlx::Row as _;
                let mut map = std::collections::HashMap::new();
                for (i, col) in row.columns().iter().enumerate() {
                    map.insert(col.name().to_owned(), pg_cell_to_sqlvalue(row, i));
                }
                out.push(map);
            }
            Ok(out)
        }
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => {
            let mut q: sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query_my(q, v);
            }
            let rows = q.fetch_all(my).await?;
            let col_types = column_types(&rows, &select_model_cols(query));
            let mut out = Vec::with_capacity(rows.len());
            for row in &rows {
                use sqlx::Column as _;
                use sqlx::Row as _;
                let mut map = std::collections::HashMap::new();
                for (i, col) in row.columns().iter().enumerate() {
                    map.insert(
                        col.name().to_owned(),
                        my_cell_to_sqlvalue(row, i, col_types[i]),
                    );
                }
                out.push(map);
            }
            Ok(out)
        }
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => {
            let mut q: sqlx::query::Query<'_, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'_>> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query_sqlite(q, v);
            }
            let rows = q.fetch_all(sq).await?;
            let col_types = column_types(&rows, &select_model_cols(query));
            let mut out = Vec::with_capacity(rows.len());
            for row in &rows {
                use sqlx::Column as _;
                use sqlx::Row as _;
                let mut map = std::collections::HashMap::new();
                for (i, col) in row.columns().iter().enumerate() {
                    map.insert(
                        col.name().to_owned(),
                        sqlite_cell_to_sqlvalue(row, i, col_types[i]),
                    );
                }
                out.push(map);
            }
            Ok(out)
        }
    }
}

/// [`fetch_values_dict`] for an [`AggregateQuery`]: each row comes
/// back keyed by output-column alias. Backs
/// [`crate::query::AggregateBuilder::fetch`].
///
/// It goes through [`Pool`], so `.annotate(…)` works on every
/// backend. [`super::fetch_aggregate_on`] does the same on a
/// borrowed Postgres executor.
///
/// # Errors
/// SQL compilation or driver failure.
pub async fn fetch_aggregate_dict(
    pool: &Pool,
    query: &AggregateQuery,
) -> Result<Vec<std::collections::HashMap<String, SqlValue>>, ExecError> {
    let stmt = pool.dialect().compile_aggregate(query)?;
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => {
            let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query(q, v);
            }
            let rows = q.fetch_all(pg).await?;
            let mut out = Vec::with_capacity(rows.len());
            for row in &rows {
                use sqlx::Column as _;
                use sqlx::Row as _;
                let mut map = std::collections::HashMap::new();
                for (i, col) in row.columns().iter().enumerate() {
                    map.insert(col.name().to_owned(), pg_cell_to_sqlvalue(row, i));
                }
                out.push(map);
            }
            Ok(out)
        }
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => {
            let mut q: sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query_my(q, v);
            }
            let rows = q.fetch_all(my).await?;
            let col_types = column_types(&rows, &aggregate_group_cols(query));
            let mut out = Vec::with_capacity(rows.len());
            for row in &rows {
                use sqlx::Column as _;
                use sqlx::Row as _;
                let mut map = std::collections::HashMap::new();
                for (i, col) in row.columns().iter().enumerate() {
                    map.insert(
                        col.name().to_owned(),
                        my_cell_to_sqlvalue(row, i, col_types[i]),
                    );
                }
                out.push(map);
            }
            Ok(out)
        }
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => {
            let mut q: sqlx::query::Query<'_, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'_>> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query_sqlite(q, v);
            }
            let rows = q.fetch_all(sq).await?;
            let col_types = column_types(&rows, &aggregate_group_cols(query));
            let mut out = Vec::with_capacity(rows.len());
            for row in &rows {
                use sqlx::Column as _;
                use sqlx::Row as _;
                let mut map = std::collections::HashMap::new();
                for (i, col) in row.columns().iter().enumerate() {
                    map.insert(
                        col.name().to_owned(),
                        sqlite_cell_to_sqlvalue(row, i, col_types[i]),
                    );
                }
                out.push(map);
            }
            Ok(out)
        }
    }
}

/// Run a [`SelectQuery`] with a projection and return each row as a
/// `Vec<SqlValue>` in the projection's column order. Backs
/// [`crate::query::ValuesListQuerySet::fetch`].
///
/// # Errors
/// SQL compilation or driver failure.
pub async fn fetch_values_list(
    pool: &Pool,
    query: &SelectQuery,
) -> Result<Vec<Vec<SqlValue>>, ExecError> {
    let stmt = pool.dialect().compile_select(query)?;
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => {
            let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query(q, v);
            }
            let rows = q.fetch_all(pg).await?;
            let mut out = Vec::with_capacity(rows.len());
            for row in &rows {
                use sqlx::Row as _;
                let n = row.columns().len();
                let mut v = Vec::with_capacity(n);
                for i in 0..n {
                    v.push(pg_cell_to_sqlvalue(row, i));
                }
                out.push(v);
            }
            Ok(out)
        }
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => {
            let mut q: sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query_my(q, v);
            }
            let rows = q.fetch_all(my).await?;
            let col_types = column_types(&rows, &select_model_cols(query));
            let mut out = Vec::with_capacity(rows.len());
            for row in &rows {
                use sqlx::Row as _;
                let n = row.columns().len();
                let mut v = Vec::with_capacity(n);
                for i in 0..n {
                    v.push(my_cell_to_sqlvalue(row, i, col_types[i]));
                }
                out.push(v);
            }
            Ok(out)
        }
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => {
            let mut q: sqlx::query::Query<'_, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'_>> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query_sqlite(q, v);
            }
            let rows = q.fetch_all(sq).await?;
            let col_types = column_types(&rows, &select_model_cols(query));
            let mut out = Vec::with_capacity(rows.len());
            for row in &rows {
                use sqlx::Row as _;
                let n = row.columns().len();
                let mut v = Vec::with_capacity(n);
                for i in 0..n {
                    v.push(sqlite_cell_to_sqlvalue(row, i, col_types[i]));
                }
                out.push(v);
            }
            Ok(out)
        }
    }
}

/// The bound `.values_list_flat::<U>(…)` needs on Postgres. With the
/// feature off it is an empty blanket impl, so such builds compile.
/// See [`super::MaybePgFromRow`].
#[cfg(feature = "postgres")]
pub trait MaybePgScalar:
    for<'r> sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>
{
}
#[cfg(feature = "postgres")]
impl<T> MaybePgScalar for T where
    T: for<'r> sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>
{
}
#[cfg(not(feature = "postgres"))]
pub trait MaybePgScalar {}
#[cfg(not(feature = "postgres"))]
impl<T> MaybePgScalar for T {}

#[cfg(feature = "mysql")]
pub trait MaybeMyScalar: for<'r> sqlx::Decode<'r, sqlx::MySql> + sqlx::Type<sqlx::MySql> {}
#[cfg(feature = "mysql")]
impl<T> MaybeMyScalar for T where T: for<'r> sqlx::Decode<'r, sqlx::MySql> + sqlx::Type<sqlx::MySql> {}
#[cfg(not(feature = "mysql"))]
pub trait MaybeMyScalar {}
#[cfg(not(feature = "mysql"))]
impl<T> MaybeMyScalar for T {}

#[cfg(feature = "sqlite")]
pub trait MaybeSqliteScalar:
    for<'r> sqlx::Decode<'r, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite>
{
}
#[cfg(feature = "sqlite")]
impl<T> MaybeSqliteScalar for T where
    T: for<'r> sqlx::Decode<'r, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite>
{
}
#[cfg(not(feature = "sqlite"))]
pub trait MaybeSqliteScalar {}
#[cfg(not(feature = "sqlite"))]
impl<T> MaybeSqliteScalar for T {}

mod flat_sealed {
    pub trait Sealed {}
}

/// A type the flat `values_list` / `pluck` / `value` decode can return.
/// Sealed; `Option<T>` reads NULL as `None`, a bare `T` errors on NULL.
/// `u8`–`u64` need a build without `postgres`, `Decimal` one without `sqlite`.
pub trait FlatScalar: flat_sealed::Sealed + Send + Unpin + Sized {
    #[doc(hidden)]
    type Cell: MaybePgScalar + MaybeMyScalar + MaybeSqliteScalar + Send + Unpin;
    #[doc(hidden)]
    fn from_cell(cell: Self::Cell) -> Self;
    #[doc(hidden)]
    fn from_null(column: &str) -> Result<Self, sqlx::Error>;
}

/// The error PG and MySQL raise themselves, so all three agree.
fn unexpected_null(column: &str) -> sqlx::Error {
    sqlx::Error::ColumnDecode {
        index: format!("{column:?}"),
        source: Box::new(sqlx::error::UnexpectedNullError),
    }
}

macro_rules! flat_scalar {
    ($($t:ty),* $(,)?) => {$(
        impl flat_sealed::Sealed for $t {}
        impl flat_sealed::Sealed for Option<$t> {}
        impl FlatScalar for $t {
            type Cell = $t;
            fn from_cell(cell: $t) -> Self {
                cell
            }
            fn from_null(column: &str) -> Result<Self, sqlx::Error> {
                Err(unexpected_null(column))
            }
        }
        impl FlatScalar for Option<$t> {
            type Cell = $t;
            fn from_cell(cell: $t) -> Self {
                Some(cell)
            }
            fn from_null(_: &str) -> Result<Self, sqlx::Error> {
                Ok(None)
            }
        }
    )*};
}

flat_scalar!(
    i8,
    i16,
    i32,
    i64,
    f32,
    f64,
    bool,
    String,
    Vec<u8>,
    serde_json::Value,
    chrono::DateTime<chrono::Utc>,
    chrono::NaiveDateTime,
    chrono::NaiveDate,
    chrono::NaiveTime,
);
// sqlx-sqlite has no `Decimal` decode, so only builds without SQLite get it.
#[cfg(not(feature = "sqlite"))]
flat_scalar!(rust_decimal::Decimal);
// sqlx-postgres has no unsigned decode (MySQL reads them from UNSIGNED columns).
#[cfg(not(feature = "postgres"))]
flat_scalar!(u8, u16, u32, u64);

/// The one place a UUID cell is decoded. MySQL keeps it as `CHAR(36)` text,
/// and sqlx's `Uuid` there wants 16 raw bytes (#1733).
#[doc(hidden)]
pub struct UuidCell(uuid::Uuid);

#[cfg(feature = "postgres")]
impl sqlx::Type<sqlx::Postgres> for UuidCell {
    fn type_info() -> sqlx::postgres::PgTypeInfo {
        <uuid::Uuid as sqlx::Type<sqlx::Postgres>>::type_info()
    }
    fn compatible(ty: &sqlx::postgres::PgTypeInfo) -> bool {
        <uuid::Uuid as sqlx::Type<sqlx::Postgres>>::compatible(ty)
    }
}
#[cfg(feature = "postgres")]
impl<'r> sqlx::Decode<'r, sqlx::Postgres> for UuidCell {
    fn decode(v: sqlx::postgres::PgValueRef<'r>) -> Result<Self, sqlx::error::BoxDynError> {
        <uuid::Uuid as sqlx::Decode<sqlx::Postgres>>::decode(v).map(Self)
    }
}
#[cfg(feature = "mysql")]
impl sqlx::Type<sqlx::MySql> for UuidCell {
    fn type_info() -> sqlx::mysql::MySqlTypeInfo {
        <uuid::fmt::Hyphenated as sqlx::Type<sqlx::MySql>>::type_info()
    }
    fn compatible(ty: &sqlx::mysql::MySqlTypeInfo) -> bool {
        <uuid::fmt::Hyphenated as sqlx::Type<sqlx::MySql>>::compatible(ty)
    }
}
#[cfg(feature = "mysql")]
impl<'r> sqlx::Decode<'r, sqlx::MySql> for UuidCell {
    fn decode(v: sqlx::mysql::MySqlValueRef<'r>) -> Result<Self, sqlx::error::BoxDynError> {
        <uuid::fmt::Hyphenated as sqlx::Decode<sqlx::MySql>>::decode(v).map(|h| Self(h.into_uuid()))
    }
}
#[cfg(feature = "sqlite")]
impl sqlx::Type<sqlx::Sqlite> for UuidCell {
    fn type_info() -> sqlx::sqlite::SqliteTypeInfo {
        <uuid::Uuid as sqlx::Type<sqlx::Sqlite>>::type_info()
    }
    fn compatible(ty: &sqlx::sqlite::SqliteTypeInfo) -> bool {
        <uuid::Uuid as sqlx::Type<sqlx::Sqlite>>::compatible(ty)
    }
}
#[cfg(feature = "sqlite")]
impl<'r> sqlx::Decode<'r, sqlx::Sqlite> for UuidCell {
    fn decode(v: sqlx::sqlite::SqliteValueRef<'r>) -> Result<Self, sqlx::error::BoxDynError> {
        // sqlx binds 16 raw bytes, but a column default or raw SQL may store text.
        let bytes = <&[u8] as sqlx::Decode<sqlx::Sqlite>>::decode(v)?;
        let u = match std::str::from_utf8(bytes) {
            Ok(text) if bytes.len() != 16 => uuid::Uuid::parse_str(text)?,
            _ => uuid::Uuid::from_slice(bytes)?,
        };
        Ok(Self(u))
    }
}

impl flat_sealed::Sealed for uuid::Uuid {}
impl flat_sealed::Sealed for Option<uuid::Uuid> {}
impl FlatScalar for uuid::Uuid {
    type Cell = UuidCell;
    fn from_cell(cell: UuidCell) -> Self {
        cell.0
    }
    fn from_null(column: &str) -> Result<Self, sqlx::Error> {
        Err(unexpected_null(column))
    }
}
impl FlatScalar for Option<uuid::Uuid> {
    type Cell = UuidCell;
    fn from_cell(cell: UuidCell) -> Self {
        Some(cell.0)
    }
    fn from_null(_: &str) -> Result<Self, sqlx::Error> {
        Ok(None)
    }
}

impl<T> flat_sealed::Sealed for sqlx::types::Json<T> {}
impl<T> flat_sealed::Sealed for Option<sqlx::types::Json<T>> {}
impl<T> FlatScalar for sqlx::types::Json<T>
where
    T: serde::de::DeserializeOwned + Send + Unpin + 'static,
{
    type Cell = Self;
    fn from_cell(cell: Self) -> Self {
        cell
    }
    fn from_null(column: &str) -> Result<Self, sqlx::Error> {
        Err(unexpected_null(column))
    }
}
impl<T> FlatScalar for Option<sqlx::types::Json<T>>
where
    T: serde::de::DeserializeOwned + Send + Unpin + 'static,
{
    type Cell = sqlx::types::Json<T>;
    fn from_cell(cell: Self::Cell) -> Self {
        Some(cell)
    }
    fn from_null(_: &str) -> Result<Self, sqlx::Error> {
        Ok(None)
    }
}

#[cfg(test)]
mod flat_scalar_tests {
    fn is_flat<U: super::FlatScalar>() {}

    /// The types `pluck_pairs` decoded before `FlatScalar` narrowed it.
    #[test]
    fn small_ints_and_json_are_flat_scalars() {
        is_flat::<i8>();
        is_flat::<Option<i8>>();
        is_flat::<sqlx::types::Json<Vec<String>>>();
        is_flat::<Option<sqlx::types::Json<Vec<String>>>>();
    }

    #[cfg(not(feature = "postgres"))]
    #[test]
    fn unsigned_ints_are_flat_scalars_without_postgres() {
        is_flat::<u8>();
        is_flat::<u16>();
        is_flat::<u32>();
        is_flat::<Option<u64>>();
    }

    /// `pluck::<Decimal>` compiled before `FlatScalar`; keep it that way.
    #[cfg(not(feature = "sqlite"))]
    #[test]
    fn decimal_is_a_flat_scalar() {
        is_flat::<rust_decimal::Decimal>();
        is_flat::<Option<rust_decimal::Decimal>>();
    }
}

/// Decode column 0, checking NULL first: sqlx-sqlite reads NULL as `0` (#1773).
#[cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]
fn decode_flat<R, U>(row: &R) -> Result<U, sqlx::Error>
where
    R: sqlx::Row,
    usize: sqlx::ColumnIndex<R>,
    U: FlatScalar,
    U::Cell: for<'r> sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    decode_cell(row, 0)
}

/// Decode column `index` the same NULL-checked way.
#[cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]
fn decode_cell<R, U>(row: &R, index: usize) -> Result<U, sqlx::Error>
where
    R: sqlx::Row,
    usize: sqlx::ColumnIndex<R>,
    U: FlatScalar,
    U::Cell: for<'r> sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    use sqlx::Column as _;
    if super::row_to_json::cell_is_null(row, index) {
        let name = row.columns().get(index).map(|c| c.name().to_owned());
        return U::from_null(name.as_deref().unwrap_or("?"));
    }
    row.try_get::<U::Cell, _>(index).map(U::from_cell)
}

/// Decode the named column of a MySQL row through [`FlatScalar`], for
/// macro-emitted code; a missing column is an error, not NULL.
///
/// # Errors
/// The driver's decode error, or NULL for a bare `U`.
#[cfg(feature = "mysql")]
#[doc(hidden)]
pub fn try_get_flat_my<U: FlatScalar>(
    row: &crate::sql::MyReturningRow,
    name: &str,
) -> Result<U, sqlx::Error> {
    use sqlx::{Row as _, ValueRef as _};
    if row.try_get_raw(name)?.is_null() {
        return U::from_null(name);
    }
    row.try_get::<U::Cell, _>(name).map(U::from_cell)
}

#[cfg(not(feature = "mysql"))]
#[doc(hidden)]
#[allow(clippy::missing_errors_doc)]
pub fn try_get_flat_my<U>(row: &crate::sql::MyReturningRow, _name: &str) -> Result<U, sqlx::Error> {
    match *row {}
}

/// Decode a `(K, V)` row, each cell NULL-checked (#1808).
#[cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]
fn decode_pair<R, K, V>(row: &R) -> Result<(K, V), sqlx::Error>
where
    R: sqlx::Row,
    usize: sqlx::ColumnIndex<R>,
    K: FlatScalar,
    V: FlatScalar,
    K::Cell: for<'r> sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    V::Cell: for<'r> sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    Ok((decode_cell(row, 0)?, decode_cell(row, 1)?))
}

/// Run a one-column [`SelectQuery`] and decode each row's only cell
/// into `U` — the flat form of `.values_list()`.
///
/// # Errors
/// SQL compilation or driver failure, including a decode error when
/// `U` does not match the column's type or a bare `U` meets NULL.
pub async fn fetch_values_flat<U: FlatScalar>(
    pool: &Pool,
    query: &SelectQuery,
) -> Result<Vec<U>, ExecError> {
    let stmt = pool.dialect().compile_select(query)?;
    fetch_flat_raw(pool, &stmt.sql, stmt.params).await
}

/// [`fetch_values_flat`] for a one-column SQL string in the dialect's shape.
///
/// # Errors
/// Driver failure, or a decode error as for [`fetch_values_flat`].
async fn fetch_flat_raw<U: FlatScalar>(
    pool: &Pool,
    sql: &str,
    params: Vec<SqlValue>,
) -> Result<Vec<U>, ExecError> {
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => {
            let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(sql);
            for v in params {
                q = bind_query(q, v);
            }
            let rows = q.fetch_all(pg).await?;
            Ok(rows.iter().map(decode_flat).collect::<Result<_, _>>()?)
        }
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => {
            let mut q: sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments> =
                sqlx::query(sql);
            for v in params {
                q = bind_query_my(q, v);
            }
            let rows = q.fetch_all(my).await?;
            Ok(rows.iter().map(decode_flat).collect::<Result<_, _>>()?)
        }
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => {
            let mut q: sqlx::query::Query<'_, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'_>> =
                sqlx::query(sql);
            for v in params {
                q = bind_query_sqlite(q, v);
            }
            let rows = q.fetch_all(sq).await?;
            Ok(rows.iter().map(decode_flat).collect::<Result<_, _>>()?)
        }
    }
}

/// Run a two-column [`SelectQuery`] and decode each row into
/// `(K, V)`. Backs [`crate::query::QuerySet::pluck_pairs`].
///
/// # Errors
/// SQL compilation or driver failure, including a decode error when
/// `K` or `V` does not match its column's type.
pub async fn fetch_values_pairs<K, V>(
    pool: &Pool,
    query: &SelectQuery,
) -> Result<Vec<(K, V)>, ExecError>
where
    K: FlatScalar,
    V: FlatScalar,
{
    let stmt = pool.dialect().compile_select(query)?;
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => {
            let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query(q, v);
            }
            let rows = q.fetch_all(pg).await?;
            Ok(rows.iter().map(decode_pair).collect::<Result<_, _>>()?)
        }
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => {
            let mut q: sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query_my(q, v);
            }
            let rows = q.fetch_all(my).await?;
            Ok(rows.iter().map(decode_pair).collect::<Result<_, _>>()?)
        }
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => {
            let mut q: sqlx::query::Query<'_, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'_>> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query_sqlite(q, v);
            }
            let rows = q.fetch_all(sq).await?;
            Ok(rows.iter().map(decode_pair).collect::<Result<_, _>>()?)
        }
    }
}

// Bridge methods on the values builders so callers chain `.fetch(&pool)`.

impl<T: crate::core::Model> crate::query::ValuesQuerySet<T> {
    /// Run the projection and return one map per row.
    ///
    /// # Errors
    /// - [`ExecError::Query`] when the SQL fails to compile, for
    ///   example on a misspelled column.
    /// - [`ExecError::Driver`] for driver, network or decode failures.
    pub async fn fetch(
        self,
        pool: &Pool,
    ) -> Result<Vec<std::collections::HashMap<String, SqlValue>>, ExecError> {
        let q = self.compile()?;
        fetch_values_dict(pool, &q).await
    }
}

impl<T: crate::core::Model> crate::query::ValuesQuerySet<T> {
    /// [`Self::fetch`] on a borrowed executor instead of the pool.
    /// Use it for a tenant-scoped query: a schema-mode tenant is
    /// selected by `SET search_path` on one connection, so going
    /// through the pool would read `public` instead.
    ///
    /// # Errors
    /// As [`Self::fetch`].
    #[cfg(feature = "postgres")]
    pub async fn fetch_on<'c, E>(
        self,
        executor: E,
    ) -> Result<Vec<std::collections::HashMap<String, SqlValue>>, ExecError>
    where
        E: sqlx::Executor<'c, Database = sqlx::Postgres>,
    {
        let query = self.compile()?;
        let stmt = {
            use crate::sql::Dialect as _;
            crate::sql::Postgres.compile_select(&query)?
        };
        let rows = super::pg_on_query(&stmt.sql, stmt.params)
            .fetch_all(executor)
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            use sqlx::{Column as _, Row as _};
            let mut map = std::collections::HashMap::new();
            for (i, col) in row.columns().iter().enumerate() {
                map.insert(col.name().to_owned(), pg_cell_to_sqlvalue(row, i));
            }
            out.push(map);
        }
        Ok(out)
    }
}

impl<T: crate::core::Model> crate::query::AggregateBuilder<T> {
    /// Run the aggregate query and return one map per row, keyed by
    /// output-column name.
    ///
    /// This backs every `.aggregate()` and `.annotate(…)` chain,
    /// including [`crate::query::QuerySet::annotate_count`] and its
    /// siblings, where each row carries the parent's own columns plus
    /// the derived one.
    ///
    /// # Errors
    /// - [`ExecError::Query`] when the SQL fails to compile, for
    ///   example on an unknown relation or a bad GROUP BY.
    /// - [`ExecError::Driver`] for driver, network or decode failures.
    pub async fn fetch(
        self,
        pool: &Pool,
    ) -> Result<Vec<std::collections::HashMap<String, SqlValue>>, ExecError> {
        let q = self.compile()?;
        fetch_aggregate_dict(pool, &q).await
    }

    /// [`Self::fetch`] on a borrowed executor instead of the pool,
    /// for a tenant-scoped query. See
    /// [`crate::query::QuerySet::fetch_on`].
    ///
    /// ```ignore
    /// SetLog::objects()
    ///     .values(&["member_id"])
    ///     .annotate("total", sum("volume").into())
    ///     .fetch_on(t.conn())   // runs in the tenant's schema
    ///     .await?;
    /// ```
    ///
    /// # Errors
    /// As [`Self::fetch`].
    #[cfg(feature = "postgres")]
    pub async fn fetch_on<'c, E>(
        self,
        executor: E,
    ) -> Result<Vec<std::collections::HashMap<String, SqlValue>>, ExecError>
    where
        E: sqlx::Executor<'c, Database = sqlx::Postgres>,
    {
        let q = self.compile()?;
        super::fetch_aggregate_on(&q, executor).await
    }
}

impl<T: crate::core::Model> crate::query::ValuesListQuerySet<T> {
    /// Execute the projection and return rows as `Vec<Vec<SqlValue>>`.
    ///
    /// # Errors
    /// As [`crate::query::ValuesQuerySet::fetch`].
    pub async fn fetch(self, pool: &Pool) -> Result<Vec<Vec<SqlValue>>, ExecError> {
        let q = self.compile()?;
        fetch_values_list(pool, &q).await
    }
}

impl<T: crate::core::Model> crate::query::QuerySet<T> {
    /// Read one column from the matching rows into a `Vec<U>`.
    ///
    /// ```ignore
    /// let titles: Vec<String> = Post::objects()
    ///     .filter("published", true)
    ///     .pluck::<String>("title", &pool).await?;
    /// ```
    ///
    /// `U` is a [`FlatScalar`]: `i64`, `String`, `bool`, `f64` and the
    /// like, or `Option<_>` of one for a nullable column. Unlike `Model::pluck`, this respects the queryset's
    /// filters, ordering and limits.
    ///
    /// # Errors
    /// As [`crate::query::ValuesFlatQuerySet::fetch`].
    pub async fn pluck<U>(self, col: &'static str, pool: &Pool) -> Result<Vec<U>, ExecError>
    where
        U: FlatScalar,
    {
        self.values_list_flat(col).fetch::<U>(pool).await
    }

    /// [`Self::pluck`] of the primary key, whose name comes from the
    /// model's schema so the call site need not repeat it.
    ///
    /// ```ignore
    /// let ids: Vec<i64> = Post::objects()
    ///     .filter("published", true)
    ///     .pks::<i64>(&pool).await?;
    /// ```
    ///
    /// # Errors
    /// [`ExecError::Query(QueryError::UnknownField)`] when the model
    /// has no primary key, otherwise as [`Self::pluck`].
    pub async fn pks<K>(self, pool: &Pool) -> Result<Vec<K>, ExecError>
    where
        K: FlatScalar,
    {
        let pk_col = T::SCHEMA.primary_key().map(|f| f.column).ok_or_else(|| {
            ExecError::Query(crate::core::QueryError::UnknownField {
                model: T::SCHEMA.name,
                field: "<pk>".to_string(),
            })
        })?;
        self.pluck::<K>(pk_col, pool).await
    }

    /// Read two columns from the matching rows into `Vec<(K, V)>`.
    /// Collect it into a map at the call site if you want lookups.
    /// The key column comes first, matching the tuple.
    ///
    /// ```ignore
    /// let pairs: Vec<(i64, String)> = Post::objects()
    ///     .filter("published", true)
    ///     .pluck_pairs::<i64, String>("id", "title", &pool).await?;
    /// let map: std::collections::BTreeMap<_, _> = pairs.into_iter().collect();
    /// ```
    ///
    /// # Errors
    /// As [`crate::sql::FetcherPool::fetch`], plus a decode error
    /// when `K` or `V` does not match its column's type.
    pub async fn pluck_pairs<K, V>(
        self,
        key_col: &'static str,
        value_col: &'static str,
        pool: &Pool,
    ) -> Result<Vec<(K, V)>, ExecError>
    where
        K: FlatScalar,
        V: FlatScalar,
    {
        let q = self.values_list(&[key_col, value_col]).compile()?;
        fetch_values_pairs::<K, V>(pool, &q).await
    }

    /// Read one column from the first matching row, or `None` when
    /// nothing matches. The query carries `LIMIT 1`, so the database
    /// does not build rows you will not read.
    ///
    /// ```ignore
    /// let email: Option<String> = User::objects()
    ///     .filter("id", 1_i64)
    ///     .value::<String>("email", &pool).await?;
    /// ```
    ///
    /// Unlike `Model::value`, this respects the queryset's filters
    /// and ordering when picking the row.
    ///
    /// # Errors
    /// As [`crate::query::ValuesFlatQuerySet::first`].
    pub async fn value<U>(self, col: &'static str, pool: &Pool) -> Result<Option<U>, ExecError>
    where
        U: FlatScalar,
    {
        self.values_list_flat(col).first::<U>(pool).await
    }

    /// Render this queryset to SQL in the pool's dialect, without
    /// running it. Handy for debugging, logging and snapshot tests.
    ///
    /// The string holds placeholders, not the bound values. Use
    /// [`Self::to_compiled`] when you need those too.
    ///
    /// ```ignore
    /// let sql = Post::objects()
    ///     .filter("published", true)
    ///     .order_by(&[("created_at", true)])
    ///     .limit(10)
    ///     .to_sql(&pool)?;
    /// // -> "SELECT … FROM \"post\" WHERE \"published\" = $1 …"
    /// ```
    ///
    /// # Errors
    /// [`ExecError::Query`] when the queryset fails to compile, and
    /// [`ExecError::Sql`] from the dialect's writer.
    pub fn to_sql(self, pool: &Pool) -> Result<String, ExecError> {
        let q = self.compile()?;
        let stmt = pool.dialect().compile_select(&q)?;
        Ok(stmt.sql)
    }

    /// `SELECT SUM(col)` over the matching rows, or `Ok(None)` when
    /// none match. `Model::sum` is this over the default queryset.
    ///
    /// # Errors
    /// As [`crate::sql::fetch_aggregate_pool`], plus
    /// [`ExecError::Query(QueryError::UnknownField)`] when the model
    /// has no such column.
    pub async fn sum<U>(self, col: &str, pool: &Pool) -> Result<Option<U>, ExecError>
    where
        (Option<U>,): crate::sql::MaybePgFromRow
            + crate::sql::MaybeMyFromRow
            + crate::sql::MaybeSqliteFromRow
            + Send
            + Unpin,
    {
        self.queryset_aggregate_one::<U>(col, crate::core::AggregateExpr::Sum, pool)
            .await
    }

    /// Fetch one page of rows and the matching total, as
    /// `(rows, total)`. Unlike `Model::paginate`, the total counts
    /// only rows the queryset's filters match.
    ///
    /// It runs two queries, a `COUNT(*)` and the page itself, both
    /// with the same WHERE clause.
    ///
    /// `page` starts at 1, so `paginate(1, 10)` gives the first ten
    /// rows and `paginate(2, 10)` the next ten. An unordered queryset
    /// pages in PK order.
    ///
    /// # Errors
    /// As [`crate::sql::CounterPool::count`] and
    /// [`crate::sql::FetcherPool::fetch`].
    pub async fn paginate(
        self,
        page: i64,
        per_page: i64,
        pool: &Pool,
    ) -> Result<(Vec<T>, i64), ExecError>
    where
        T: crate::core::Model
            + crate::sql::MaybePgFromRow
            + crate::sql::MaybeMyFromRow
            + crate::sql::MaybeSqliteFromRow
            + crate::sql::LoadRelated
            + crate::sql::MaybeMyLoadRelated
            + crate::sql::MaybeSqliteLoadRelated
            + Send
            + Unpin,
    {
        use crate::sql::{CounterPool as _, FetcherPool as _};
        let total = <crate::query::QuerySet<T> as ::core::clone::Clone>::clone(&self)
            .count(pool)
            .await?;
        let offset = crate::list_params::page_offset(page, per_page);
        let rows = self
            .ordered_or_by_pk()
            .limit(per_page)
            .offset(offset)
            .fetch(pool)
            .await?;
        Ok((rows, total))
    }

    /// [`Self::sum`] as an average.
    ///
    /// # Errors
    /// As [`Self::sum`].
    pub async fn avg<U>(self, col: &str, pool: &Pool) -> Result<Option<U>, ExecError>
    where
        (Option<U>,): crate::sql::MaybePgFromRow
            + crate::sql::MaybeMyFromRow
            + crate::sql::MaybeSqliteFromRow
            + Send
            + Unpin,
    {
        self.queryset_aggregate_one::<U>(col, crate::core::AggregateExpr::Avg, pool)
            .await
    }

    /// [`Self::sum`] as a minimum.
    ///
    /// # Errors
    /// As [`Self::sum`].
    pub async fn min<U>(self, col: &str, pool: &Pool) -> Result<Option<U>, ExecError>
    where
        (Option<U>,): crate::sql::MaybePgFromRow
            + crate::sql::MaybeMyFromRow
            + crate::sql::MaybeSqliteFromRow
            + Send
            + Unpin,
    {
        self.queryset_aggregate_one::<U>(col, crate::core::AggregateExpr::Min, pool)
            .await
    }

    /// [`Self::sum`] as a maximum.
    ///
    /// # Errors
    /// As [`Self::sum`].
    pub async fn max<U>(self, col: &str, pool: &Pool) -> Result<Option<U>, ExecError>
    where
        (Option<U>,): crate::sql::MaybePgFromRow
            + crate::sql::MaybeMyFromRow
            + crate::sql::MaybeSqliteFromRow
            + Send
            + Unpin,
    {
        self.queryset_aggregate_one::<U>(col, crate::core::AggregateExpr::Max, pool)
            .await
    }

    /// Shared body of `sum`, `avg`, `min` and `max`. It checks the
    /// column against the model, moves the queryset's filters into
    /// the aggregate's WHERE clause, and returns the one value.
    async fn queryset_aggregate_one<U>(
        self,
        col: &str,
        build: fn(&'static str) -> crate::core::AggregateExpr,
        pool: &Pool,
    ) -> Result<Option<U>, ExecError>
    where
        (Option<U>,): crate::sql::MaybePgFromRow
            + crate::sql::MaybeMyFromRow
            + crate::sql::MaybeSqliteFromRow
            + Send
            + Unpin,
    {
        let col_static = crate::sql::model_shortcuts::resolve_col::<T>(col)?;
        let Some(select_q) = self.compile_unless_none()? else {
            return Ok(None);
        };
        // One aggregate column, so the tuple decode lines up.
        let aggregate_q = crate::core::AggregateQuery::over_select(
            select_q,
            vec![("v".into(), build(col_static))],
        );
        let rows: Vec<(Option<U>,)> = crate::sql::fetch_aggregate_pool(pool, &aggregate_q).await?;
        Ok(rows.into_iter().next().and_then(|t| t.0))
    }

    /// [`Self::to_sql`] plus the bound parameters, as a
    /// [`crate::sql::CompiledStatement`].
    ///
    /// # Errors
    /// As [`Self::to_sql`].
    pub fn to_compiled(self, pool: &Pool) -> Result<crate::sql::CompiledStatement, ExecError> {
        let q = self.compile()?;
        let stmt = pool.dialect().compile_select(&q)?;
        Ok(stmt)
    }
}

impl<T> crate::query::QuerySet<T>
where
    T: crate::core::Model
        + Send
        + Unpin
        + MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + crate::sql::LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated,
{
    /// `UPDATE … SET col = col + by` over the matching rows.
    /// Returns the number of rows affected. Unlike
    /// `Model::increment_each`, this respects the queryset's
    /// filters, so you can bump a counter on some rows only.
    ///
    /// A negative `by` subtracts; [`Self::decrement`] reads better
    /// at the call site.
    ///
    /// ```ignore
    /// Post::objects()
    ///     .filter("published", true)
    ///     .increment("views", 1, &pool)
    ///     .await?;
    /// ```
    ///
    /// # Errors
    /// As [`UpdaterPool::execute_pool`](crate::sql::UpdaterPool::execute_pool),
    /// plus [`ExecError::Query`] with `QueryError::UnknownField` when
    /// `col` is not a declared field on `T`.
    pub async fn increment(self, col: &str, by: i64, pool: &Pool) -> Result<u64, ExecError> {
        let col_static = crate::sql::model_shortcuts::resolve_col::<T>(col)?;
        self.update()
            .set_expr(
                col,
                crate::sql::model_shortcuts::add_signed_expr(col_static, by),
            )
            .execute_pool(pool)
            .await
    }

    /// [`Self::increment`] the other way, subtracting `by`.
    ///
    /// ```ignore
    /// User::objects()
    ///     .filter("vip", true)
    ///     .decrement("credits", 10, &pool)
    ///     .await?;
    /// ```
    ///
    /// # Errors
    /// As [`Self::increment`].
    pub async fn decrement(self, col: &str, by: i64, pool: &Pool) -> Result<u64, ExecError> {
        self.increment(col, -by, pool).await
    }
}

impl<T: crate::core::Model> crate::query::ValuesFlatQuerySet<T> {
    /// Run the one-column projection and decode each cell into `U`.
    ///
    /// # Errors
    /// As [`crate::query::ValuesQuerySet::fetch`], plus a decode
    /// error when `U` does not match the column's type.
    pub async fn fetch<U>(self, pool: &Pool) -> Result<Vec<U>, ExecError>
    where
        U: FlatScalar,
    {
        let q = self.compile()?;
        fetch_values_flat::<U>(pool, &q).await
    }

    /// Return the first row's cell, or `None` when nothing matches.
    /// It adds `LIMIT 1`, so unlike [`Self::fetch`] the database
    /// does not build rows you will not read.
    ///
    /// ```ignore
    /// let name: Option<String> = User::query()
    ///     .filter("id", 1_i64)
    ///     .values_list_flat("name")
    ///     .first::<String>(&pool).await?;
    /// ```
    ///
    /// # Errors
    /// As [`Self::fetch`].
    pub async fn first<U>(self, pool: &Pool) -> Result<Option<U>, ExecError>
    where
        U: FlatScalar,
    {
        // Rebuild with `LIMIT 1`. `compile()` consumes `self.qs`, so
        // the limit has to go on through the builder.
        let col = self.col;
        let qs = self.qs.limit(1);
        let q = crate::query::ValuesFlatQuerySet { qs, col }.compile()?;
        let rows = fetch_values_flat::<U>(pool, &q).await?;
        Ok(rows.into_iter().next())
    }
}
