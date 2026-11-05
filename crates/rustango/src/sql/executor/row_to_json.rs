//! Dynamic row-to-JSON decoders (`row_to_json` family) + tri-dialect
//! `select_rows_as_json` / `select_one_row_as_json` entry points.
//!
//! Extracted from `executor/mod.rs` as part of #116 step 7. The
//! schema-driven JSON decoders are consumed by the admin/viewset
//! list/retrieve handlers + `contenttypes::fetch_row_as_json` —
//! pulling them out drops 500 LOC from mod.rs.

#[cfg(feature = "postgres")]
use sqlx::postgres::PgArguments;
#[cfg(feature = "postgres")]
use sqlx::query::Query;

#[cfg(feature = "postgres")]
use super::bind_query;
#[cfg(feature = "mysql")]
use super::bind_query_my;
#[cfg(feature = "sqlite")]
use super::bind_query_sqlite;
use super::ExecError;
use crate::core::SelectQuery;
use crate::sql::Pool;

/// Schema-driven decode of a Postgres row into a JSON object.
/// Walks `fields` and pulls each column out via `try_get`,
/// mapping the model's `FieldType` to the right Rust type, then
/// to JSON. Used by the viewset list/retrieve handlers (#80) and
/// by `contenttypes::fetch_row_as_json` (#89).
///
/// Failures on individual columns degrade gracefully to
/// `Value::Null` — the response shape stays stable even if one
/// field's bytes are unexpected (e.g. a NULL where the schema
/// says NOT NULL because of a manual SQL edit). Strict
/// row-to-T decoding lives on the Model derive's `from_row` path
/// and is the right tool when you control the data shape.
// #562 — single `hex_encode` implementation lives in `crate::hex`;
// the `row_to_json` family used to ship a verbatim copy. Re-export
// so the local `hex_encode(...)` call sites in the `Binary` arm
// stay byte-identical.
use crate::hex::hex_encode;

/// True when the cell is SQL NULL or missing. sqlx-sqlite decodes NULL
/// as `0` / `false` / `""` instead of erroring (#1766), so check first.
#[cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]
pub(super) fn cell_is_null<R: sqlx::Row, I: sqlx::ColumnIndex<R>>(row: &R, index: I) -> bool {
    use sqlx::ValueRef as _;
    match row.try_get_raw(index) {
        Ok(v) => v.is_null(),
        Err(_) => true,
    }
}

/// Rewrites a decoded JSON column value; SQL NULL never reaches it.
pub(crate) type JsonCell = fn(serde_json::Value) -> serde_json::Value;

fn json_as_is(v: serde_json::Value) -> serde_json::Value {
    v
}

/// What a row decoder does with a cell it cannot read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OnBadCell {
    /// `null`, so a page still renders a dirty row.
    Null,
    /// The decode error: an export must not turn data into NULL.
    Fail,
}

impl OnBadCell {
    fn settle(
        self,
        cell: Result<serde_json::Value, sqlx::Error>,
    ) -> Result<serde_json::Value, sqlx::Error> {
        match (cell, self) {
            (Err(_), Self::Null) => Ok(serde_json::Value::Null),
            (cell, _) => cell,
        }
    }
}

/// How the `row_to_json` family reads each cell.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CellDecode {
    pub(crate) json: JsonCell,
    pub(crate) on_bad: OnBadCell,
}

impl CellDecode {
    const LENIENT: Self = Self {
        json: json_as_is,
        on_bad: OnBadCell::Null,
    };
}

/// A [`OnBadCell::Null`] decode, which has no error to return.
fn lenient(row: Result<serde_json::Value, sqlx::Error>) -> serde_json::Value {
    row.unwrap_or_else(|e| unreachable!("a lenient row decode failed: {e}"))
}

/// A float as JSON; NaN and infinity have no JSON form.
fn finite(field: &crate::core::FieldSchema, n: f64) -> Result<serde_json::Value, sqlx::Error> {
    serde_json::Number::from_f64(n)
        .map(serde_json::Value::Number)
        .ok_or_else(|| bad_cell(field, format!("{n} has no JSON form")))
}

fn bad_cell(field: &crate::core::FieldSchema, why: String) -> sqlx::Error {
    sqlx::Error::ColumnDecode {
        index: format!("{:?}", field.column),
        source: why.into(),
    }
}

/// Generic body for the PG + MySQL `row_to_json` variants — they
/// emit byte-identical match-on-`FieldType` decode tables. Issue
/// #562: extract into a generic-over-`sqlx::Row` function so the
/// 60-arm decode table lives once and both backends call into the
/// same code path. SQLite has slight divergences (chrono permissive
/// types) and continues to live in [`row_to_json_sqlite`].
///
/// The `where` clause looks heavy but it just enumerates the
/// `Decode<'r, R::Database> + Type<R::Database>` bounds for each
/// FieldType we decode. sqlx already provides these for `PgRow` and
/// `MySqlRow`; the bounds compile away to nothing at the call sites.
/// `uuid_cell` is the exception: MySQL keeps a UUID as `CHAR(36)` text.
#[cfg(any(feature = "postgres", feature = "mysql"))]
fn row_to_json_generic<'r, R>(
    row: &'r R,
    fields: &[&'static crate::core::FieldSchema],
    uuid_cell: fn(&'r R, &str) -> Result<uuid::Uuid, sqlx::Error>,
    pg_only: fn(&'r R, &crate::core::FieldSchema) -> Result<serde_json::Value, sqlx::Error>,
    decode: CellDecode,
) -> Result<serde_json::Value, sqlx::Error>
where
    R: sqlx::Row,
    &'r str: sqlx::ColumnIndex<R>,
    i16: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    i32: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    i64: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    f32: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    f64: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    bool: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    String: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    chrono::NaiveDate: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    chrono::NaiveTime: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    chrono::DateTime<chrono::Utc>: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    serde_json::Value: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    rust_decimal::Decimal: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    Vec<u8>: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    use crate::core::FieldType;
    use serde_json::{json, Value};
    let mut map = serde_json::Map::new();
    for field in fields {
        if cell_is_null(row, field.column) {
            map.insert(field.name.to_owned(), Value::Null);
            continue;
        }
        let value = match field.ty {
            FieldType::I16 => row.try_get::<i16, _>(field.column).map(|n| json!(n)),
            FieldType::I32 => row.try_get::<i32, _>(field.column).map(|n| json!(n)),
            FieldType::I64 => row.try_get::<i64, _>(field.column).map(|n| json!(n)),
            FieldType::F32 => row
                .try_get::<f32, _>(field.column)
                .and_then(|n| finite(field, f64::from(n)).map(|_| crate::core::__json_value(&n))),
            FieldType::F64 => row
                .try_get::<f64, _>(field.column)
                .and_then(|n| finite(field, n)),
            FieldType::Bool => row.try_get::<bool, _>(field.column).map(|b| json!(b)),
            FieldType::String => row.try_get::<String, _>(field.column).map(|s| json!(s)),
            FieldType::Date => row
                .try_get::<chrono::NaiveDate, _>(field.column)
                .map(|d| json!(d.to_string())),
            FieldType::DateTime => row
                .try_get::<chrono::DateTime<chrono::Utc>, _>(field.column)
                .map(|dt| json!(dt.to_rfc3339())),
            FieldType::Uuid => uuid_cell(row, field.column).map(|u| json!(u.to_string())),
            FieldType::Json => row
                .try_get::<serde_json::Value, _>(field.column)
                .map(decode.json),
            FieldType::Decimal => row
                .try_get::<rust_decimal::Decimal, _>(field.column)
                .map(|d| json!(d.to_string())),
            FieldType::Binary => row
                .try_get::<Vec<u8>, _>(field.column)
                .map(|b| json!(hex_encode(&b))),
            FieldType::Time => row
                .try_get::<chrono::NaiveTime, _>(field.column)
                .map(|t| json!(t.to_string())),
            // PG-only column types.
            FieldType::Array(_)
            | FieldType::Range(_)
            | FieldType::HStore
            | FieldType::Vector(_)
            | FieldType::Geometry(_) => pg_only(row, field),
        };
        map.insert(field.name.to_owned(), decode.on_bad.settle(value)?);
    }
    Ok(Value::Object(map))
}

#[must_use]
#[cfg(feature = "postgres")]
pub fn row_to_json(
    row: &sqlx::postgres::PgRow,
    fields: &[&'static crate::core::FieldSchema],
) -> serde_json::Value {
    lenient(row_to_json_pg_with(row, fields, CellDecode::LENIENT))
}

#[cfg(feature = "postgres")]
fn row_to_json_pg_with(
    row: &sqlx::postgres::PgRow,
    fields: &[&'static crate::core::FieldSchema],
    decode: CellDecode,
) -> Result<serde_json::Value, sqlx::Error> {
    row_to_json_generic(
        row,
        fields,
        |row, col| sqlx::Row::try_get::<uuid::Uuid, _>(row, col),
        pg_only_cell,
        decode,
    )
}

/// A PG-only column as the JSON the admin form parser reads back:
/// arrays and vectors as arrays, hstore and points as objects, a range as
/// its literal.
#[cfg(feature = "postgres")]
fn pg_only_cell(
    row: &sqlx::postgres::PgRow,
    field: &crate::core::FieldSchema,
) -> Result<serde_json::Value, sqlx::Error> {
    use crate::core::{ArrayElem, FieldType, RangeElem, SqlValue};
    use serde_json::Value;
    use sqlx::Row as _;
    let json = |r: Result<Value, serde_json::Error>| r.map_err(|e| bad_cell(field, e.to_string()));
    let literal = |v: SqlValue| match v {
        SqlValue::RangeLiteral(s) => Ok(Value::String(s)),
        other => Err(bad_cell(field, format!("not a range literal: {other:?}"))),
    };
    let col = field.column;
    match field.ty {
        FieldType::Array(ArrayElem::Text) => json(serde_json::to_value(
            row.try_get::<Vec<Option<String>>, _>(col)?,
        )),
        FieldType::Array(ArrayElem::Int) => json(serde_json::to_value(
            row.try_get::<Vec<Option<i32>>, _>(col)?,
        )),
        FieldType::Array(ArrayElem::BigInt) => json(serde_json::to_value(
            row.try_get::<Vec<Option<i64>>, _>(col)?,
        )),
        FieldType::Range(RangeElem::Int) => {
            literal(row.try_get::<crate::sql::Range<i32>, _>(col)?.into())
        }
        FieldType::Range(RangeElem::BigInt) => {
            literal(row.try_get::<crate::sql::Range<i64>, _>(col)?.into())
        }
        FieldType::Range(RangeElem::Numeric) => literal(
            row.try_get::<crate::sql::Range<rust_decimal::Decimal>, _>(col)?
                .into(),
        ),
        FieldType::Range(RangeElem::Date) => literal(
            row.try_get::<crate::sql::Range<chrono::NaiveDate>, _>(col)?
                .into(),
        ),
        FieldType::Range(RangeElem::DateTime) => literal(
            row.try_get::<crate::sql::Range<chrono::DateTime<chrono::Utc>>, _>(col)?
                .into(),
        ),
        FieldType::HStore => json(serde_json::to_value(
            row.try_get::<crate::sql::HStore, _>(col)?,
        )),
        FieldType::Vector(_) => json(serde_json::to_value(
            row.try_get::<crate::sql::Vector, _>(col)?,
        )),
        FieldType::Geometry(_) => json(serde_json::to_value(
            row.try_get::<crate::sql::Point, _>(col)?,
        )),
        _ => Ok(Value::Null),
    }
}

/// MySQL counterpart of [`row_to_json`]. Decodes each column by
/// `field.ty` against `&MySqlRow`. Type mappings mirror the
/// `sqlx::Type<MySql>` impls emitted by `#[derive(Model)]` —
/// `chrono::DateTime<Utc>` ↔ `DATETIME(6)`, `serde_json::Value` ↔
/// `JSON`, `uuid::Uuid` ↔ `CHAR(36)` hyphenated text.
#[cfg(feature = "mysql")]
#[must_use]
pub fn row_to_json_my(
    row: &sqlx::mysql::MySqlRow,
    fields: &[&'static crate::core::FieldSchema],
) -> serde_json::Value {
    lenient(row_to_json_my_with(row, fields, CellDecode::LENIENT))
}

#[cfg(feature = "mysql")]
fn row_to_json_my_with(
    row: &sqlx::mysql::MySqlRow,
    fields: &[&'static crate::core::FieldSchema],
    decode: CellDecode,
) -> Result<serde_json::Value, sqlx::Error> {
    row_to_json_generic(
        row,
        fields,
        crate::sql::try_get_flat_my::<uuid::Uuid>,
        // PG-only types never live on MySQL.
        |_, _| Ok(serde_json::Value::Null),
        decode,
    )
}

/// SQLite counterpart of [`row_to_json`]. SQLite's storage is more
/// permissive (TEXT for VARCHAR + JSON + UUID, NUMERIC for DATE,
/// REAL for f32/f64); decode targets here match the column types
/// `crate::migrate::ddl::CREATE_TABLE_SQL_SQLITE` emits. Best-effort:
/// any `try_get` failure yields `Value::Null` (matches the PG path's
/// laxity for admin rendering).
#[cfg(feature = "sqlite")]
#[must_use]
pub fn row_to_json_sqlite(
    row: &sqlx::sqlite::SqliteRow,
    fields: &[&'static crate::core::FieldSchema],
) -> serde_json::Value {
    lenient(row_to_json_sqlite_with(row, fields, CellDecode::LENIENT))
}

#[cfg(feature = "sqlite")]
fn row_to_json_sqlite_with(
    row: &sqlx::sqlite::SqliteRow,
    fields: &[&'static crate::core::FieldSchema],
    decode: CellDecode,
) -> Result<serde_json::Value, sqlx::Error> {
    use crate::core::FieldType;
    use serde_json::{json, Value};
    use sqlx::Row as _;
    // SQLite often stores DATE / TIME as TEXT: fall back to what is there.
    let or_text = |cell: Result<Value, sqlx::Error>, col: &str| {
        cell.or_else(|_| row.try_get::<String, _>(col).map(|s| json!(s)))
    };
    let mut map = serde_json::Map::new();
    for field in fields {
        if cell_is_null(row, field.column) {
            map.insert(field.name.to_owned(), Value::Null);
            continue;
        }
        let col = field.column;
        let value = match field.ty {
            FieldType::I16 => row.try_get::<i16, _>(col).map(|n| json!(n)),
            FieldType::I32 => row.try_get::<i32, _>(col).map(|n| json!(n)),
            FieldType::I64 => row.try_get::<i64, _>(col).map(|n| json!(n)),
            FieldType::F32 => row
                .try_get::<f32, _>(col)
                .and_then(|n| finite(field, f64::from(n)).map(|_| crate::core::__json_value(&n))),
            FieldType::F64 => row.try_get::<f64, _>(col).and_then(|n| finite(field, n)),
            FieldType::Bool => row.try_get::<bool, _>(col).map(|b| json!(b)),
            FieldType::String => row.try_get::<String, _>(col).map(|s| json!(s)),
            FieldType::Date => or_text(
                row.try_get::<chrono::NaiveDate, _>(col)
                    .map(|d| json!(d.to_string())),
                col,
            ),
            FieldType::DateTime => or_text(
                row.try_get::<chrono::DateTime<chrono::Utc>, _>(col)
                    .map(|dt| json!(dt.to_rfc3339())),
                col,
            ),
            // sqlx binds a Uuid as a 16-byte BLOB; older rows may be TEXT.
            FieldType::Uuid => or_text(
                row.try_get::<uuid::Uuid, _>(col)
                    .map(|u| json!(u.to_string())),
                col,
            ),
            // SQLite stores JSON as TEXT; text that does not parse is kept
            // as a string, or refused by a strict decode.
            FieldType::Json => row.try_get::<String, _>(col).and_then(|s| {
                match (serde_json::from_str(&s), decode.on_bad) {
                    (Ok(v), _) => Ok((decode.json)(v)),
                    (Err(_), OnBadCell::Null) => Ok(Value::String(s)),
                    (Err(e), OnBadCell::Fail) => Err(bad_cell(field, format!("not JSON: {e}"))),
                }
            }),
            // No `Decimal: Decode<Sqlite>`: read TEXT, as the binder writes it.
            // NUMERIC affinity stores `1` as INTEGER, `1.5` as REAL.
            FieldType::Decimal => row
                .try_get::<String, _>(col)
                .map(|s| json!(s))
                .or_else(|_| row.try_get::<i64, _>(col).map(|n| json!(n.to_string())))
                .or_else(|_| row.try_get::<f64, _>(col).map(|n| json!(n.to_string()))),
            FieldType::Binary => row
                .try_get::<Vec<u8>, _>(col)
                .map(|b| json!(hex_encode(&b))),
            FieldType::Time => or_text(
                row.try_get::<chrono::NaiveTime, _>(col)
                    .map(|t| json!(t.to_string())),
                col,
            ),
            // PG-only types never live on SQLite.
            FieldType::Array(_)
            | FieldType::Range(_)
            | FieldType::HStore
            | FieldType::Vector(_)
            | FieldType::Geometry(_) => Ok(Value::Null),
        };
        map.insert(field.name.to_owned(), decode.on_bad.settle(value)?);
    }
    Ok(Value::Object(map))
}

/// Tri-dialect SELECT → JSON: run `query` against `pool` and return
/// each row as a `serde_json::Value` map (`field.name → value`). The
/// canonical fetch path for admin / API surfaces that need to render
/// rows without a typed `T: FromRow` struct.
///
/// Dispatches per [`Pool`] variant to `row_to_json` / `row_to_json_my`
/// / `row_to_json_sqlite` and uses the appropriate sqlx query type.
/// Field-by-field decode is best-effort (decode errors → `Value::Null`)
/// to match the existing PG-only `row_to_json`'s laxity around admin
/// rendering of dirty rows.
///
/// # Errors
/// SQL compilation / driver failures only — per-cell decode errors
/// are swallowed into `Value::Null`.
pub async fn select_rows_as_json(
    pool: &Pool,
    query: &SelectQuery,
    fields: &[&'static crate::core::FieldSchema],
) -> Result<Vec<serde_json::Value>, ExecError> {
    select_rows_as_json_with(pool, query, fields, CellDecode::LENIENT).await
}

/// [`select_rows_as_json`] reading each cell as `decode` says.
pub(crate) async fn select_rows_as_json_with(
    pool: &Pool,
    query: &SelectQuery,
    fields: &[&'static crate::core::FieldSchema],
    decode: CellDecode,
) -> Result<Vec<serde_json::Value>, ExecError> {
    crate::test_assertions::query_counter::bump();
    let stmt = pool.dialect().compile_select(query)?;
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => {
            let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query(q, v);
            }
            let rows = q.fetch_all(pg).await?;
            Ok(rows
                .iter()
                .map(|r| {
                    let mut json = row_to_json_pg_with(r, fields, decode)?;
                    augment_joined_columns_pg(&mut json, r, &query.joins);
                    Ok(json)
                })
                .collect::<Result<_, sqlx::Error>>()?)
        }
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => {
            let mut q: sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query_my(q, v);
            }
            let rows = q.fetch_all(my).await?;
            Ok(rows
                .iter()
                .map(|r| {
                    let mut json = row_to_json_my_with(r, fields, decode)?;
                    augment_joined_columns_my(&mut json, r, &query.joins);
                    Ok(json)
                })
                .collect::<Result<_, sqlx::Error>>()?)
        }
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => {
            let mut q: sqlx::query::Query<'_, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'_>> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query_sqlite(q, v);
            }
            let rows = q.fetch_all(sq).await?;
            Ok(rows
                .iter()
                .map(|r| {
                    let mut json = row_to_json_sqlite_with(r, fields, decode)?;
                    augment_joined_columns_sqlite(&mut json, r, &query.joins);
                    Ok(json)
                })
                .collect::<Result<_, sqlx::Error>>()?)
        }
    }
}

/// v0.37 — copy joined-table columns (`<alias>__<col>`) into the
/// JSON row. The compile_select writer aliases joined columns this
/// way and the admin's `read_joined_value_as_html_json` reads them
/// out by the same key. Decoded as nullable strings — the admin only
/// uses these for FK display HTML rendering.
#[cfg(feature = "postgres")]
fn augment_joined_columns_pg(
    out: &mut serde_json::Value,
    row: &sqlx::postgres::PgRow,
    joins: &[crate::core::Join],
) {
    use sqlx::Row as _;
    let Some(map) = out.as_object_mut() else {
        return;
    };
    for join in joins {
        for col in &join.project {
            let key = crate::core::joins::joined_key(join.alias, col);
            let label = crate::core::joins::joined_label(join.alias, col);
            let v = row
                .try_get::<Option<String>, _>(label.as_str())
                .ok()
                .flatten();
            map.insert(
                key,
                v.map(serde_json::Value::String)
                    .unwrap_or(serde_json::Value::Null),
            );
        }
    }
}

#[cfg(feature = "mysql")]
fn augment_joined_columns_my(
    out: &mut serde_json::Value,
    row: &sqlx::mysql::MySqlRow,
    joins: &[crate::core::Join],
) {
    use sqlx::Row as _;
    let Some(map) = out.as_object_mut() else {
        return;
    };
    for join in joins {
        for col in &join.project {
            let key = crate::core::joins::joined_key(join.alias, col);
            let label = crate::core::joins::joined_label(join.alias, col);
            let v = row
                .try_get::<Option<String>, _>(label.as_str())
                .ok()
                .flatten();
            map.insert(
                key,
                v.map(serde_json::Value::String)
                    .unwrap_or(serde_json::Value::Null),
            );
        }
    }
}

#[cfg(feature = "sqlite")]
fn augment_joined_columns_sqlite(
    out: &mut serde_json::Value,
    row: &sqlx::sqlite::SqliteRow,
    joins: &[crate::core::Join],
) {
    use sqlx::Row as _;
    let Some(map) = out.as_object_mut() else {
        return;
    };
    for join in joins {
        for col in &join.project {
            let key = crate::core::joins::joined_key(join.alias, col);
            let label = crate::core::joins::joined_label(join.alias, col);
            let v = row
                .try_get::<Option<String>, _>(label.as_str())
                .ok()
                .flatten();
            map.insert(
                key,
                v.map(serde_json::Value::String)
                    .unwrap_or(serde_json::Value::Null),
            );
        }
    }
}

/// Single-row companion of [`select_rows_as_json`]. Returns
/// `Ok(None)` when no rows match.
///
/// # Errors
/// As [`select_rows_as_json`].
pub async fn select_one_row_as_json(
    pool: &Pool,
    query: &SelectQuery,
    fields: &[&'static crate::core::FieldSchema],
) -> Result<Option<serde_json::Value>, ExecError> {
    crate::test_assertions::query_counter::bump();
    let stmt = pool.dialect().compile_select(query)?;
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => {
            let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query(q, v);
            }
            Ok(q.fetch_optional(pg).await?.as_ref().map(|r| {
                let mut json = row_to_json(r, fields);
                augment_joined_columns_pg(&mut json, r, &query.joins);
                json
            }))
        }
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => {
            let mut q: sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query_my(q, v);
            }
            Ok(q.fetch_optional(my).await?.as_ref().map(|r| {
                let mut json = row_to_json_my(r, fields);
                augment_joined_columns_my(&mut json, r, &query.joins);
                json
            }))
        }
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => {
            let mut q: sqlx::query::Query<'_, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'_>> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query_sqlite(q, v);
            }
            Ok(q.fetch_optional(sq).await?.as_ref().map(|r| {
                let mut json = row_to_json_sqlite(r, fields);
                augment_joined_columns_sqlite(&mut json, r, &query.joins);
                json
            }))
        }
    }
}

/// [`select_rows_as_json`] inside an open transaction.
#[cfg(feature = "admin")]
pub(crate) async fn select_rows_as_json_tx(
    tx: &mut crate::sql::PoolTx<'_>,
    query: &SelectQuery,
    fields: &[&'static crate::core::FieldSchema],
) -> Result<Vec<serde_json::Value>, ExecError> {
    crate::test_assertions::query_counter::bump();
    let stmt = tx.dialect().compile_select(query)?;
    match &mut tx.conn {
        #[cfg(feature = "postgres")]
        crate::sql::TxConn::Postgres(t) => {
            let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query(q, v);
            }
            Ok(q.fetch_all(&mut **t)
                .await?
                .iter()
                .map(|r| {
                    let mut json = row_to_json(r, fields);
                    augment_joined_columns_pg(&mut json, r, &query.joins);
                    json
                })
                .collect())
        }
        #[cfg(feature = "mysql")]
        crate::sql::TxConn::Mysql(t) => {
            let mut q: sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query_my(q, v);
            }
            Ok(q.fetch_all(&mut **t)
                .await?
                .iter()
                .map(|r| {
                    let mut json = row_to_json_my(r, fields);
                    augment_joined_columns_my(&mut json, r, &query.joins);
                    json
                })
                .collect())
        }
        #[cfg(feature = "sqlite")]
        crate::sql::TxConn::Sqlite(t) => {
            let mut q: sqlx::query::Query<'_, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'_>> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query_sqlite(q, v);
            }
            Ok(q.fetch_all(&mut **t)
                .await?
                .iter()
                .map(|r| {
                    let mut json = row_to_json_sqlite(r, fields);
                    augment_joined_columns_sqlite(&mut json, r, &query.joins);
                    json
                })
                .collect())
        }
    }
}

/// [`select_one_row_as_json`] inside an open transaction.
#[cfg(feature = "admin")]
pub(crate) async fn select_one_row_as_json_tx(
    tx: &mut crate::sql::PoolTx<'_>,
    query: &SelectQuery,
    fields: &[&'static crate::core::FieldSchema],
) -> Result<Option<serde_json::Value>, ExecError> {
    Ok(select_rows_as_json_tx(tx, query, fields)
        .await?
        .into_iter()
        .next())
}
