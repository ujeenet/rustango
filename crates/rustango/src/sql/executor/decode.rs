//! Result cells to [`SqlValue`]. Each column gets a [`Cell`] once per
//! result set, so a cell costs one typed read instead of a chain of
//! failing ones, each of which built an sqlx error (#2655). The plan
//! follows the type the database reports or the model declares (#2486).

use std::borrow::Cow;
use std::collections::HashMap;

use crate::core::{FieldType, SqlValue};

/// A leading result column: its output name and the type its model declares.
pub(super) type TypedCol = (Cow<'static, str>, Option<FieldType>);

/// How one result column decodes. Some variants are one backend's only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    not(all(feature = "postgres", feature = "mysql", feature = "sqlite")),
    allow(dead_code)
)]
pub(super) enum Cell {
    /// No decoder for the type (PG `interval`, …): always `Null`.
    Null,
    I16,
    I32,
    I64,
    /// MySQL `BIGINT UNSIGNED` (`COUNT`, ranks); above `i64::MAX` reads as text.
    U64,
    F32,
    F64,
    Bool,
    Decimal,
    Uuid,
    /// A UUID kept as text (MySQL `CHAR(36)`).
    UuidText,
    String,
    Json,
    Binary,
    DateTime,
    /// A timestamp without zone, read as UTC.
    NaiveDateTime,
    Date,
    Time,
    TextArray,
    I64Array,
    I32Array,
    /// SQLite with no declared type: the cell's storage class decides.
    Dynamic,
}

/// A row type the values readers decode.
pub(super) trait DecodeRow: sqlx::Row {
    /// The decoder for column `i`, given the type its model declares.
    fn plan_cell(&self, i: usize, declared: Option<FieldType>) -> Cell;
    /// Column `i` read as `cell`. NULL or a failed read gives `Null`.
    fn decode_cell(&self, i: usize, cell: Cell) -> SqlValue;
}

/// The decoders for a result set's first `width` columns, from its first row.
struct RowPlan {
    cells: Vec<Cell>,
}

impl RowPlan {
    /// Only the leading `typed` columns count: an alias that shares a
    /// column name (`SUM(x) AS flag`) is not that column (#2296).
    fn new<R: DecodeRow>(first: &R, typed: &[TypedCol], width: usize) -> Self {
        use sqlx::Column as _;
        let cells = first
            .columns()
            .iter()
            .take(width)
            .enumerate()
            .map(|(i, c)| {
                let declared = typed
                    .get(i)
                    .filter(|(name, _)| name == c.name())
                    .and_then(|(_, ty)| *ty);
                first.plan_cell(i, declared)
            })
            .collect();
        Self { cells }
    }

    fn row<'a, R: DecodeRow>(&'a self, row: &'a R) -> impl Iterator<Item = SqlValue> + 'a {
        self.cells
            .iter()
            .enumerate()
            .map(move |(i, &cell)| row.decode_cell(i, cell))
    }
}

/// Each row as its first `width` cells, in column order.
pub(super) fn rows_to_lists<R: DecodeRow>(
    rows: &[R],
    typed: &[TypedCol],
    width: usize,
) -> Vec<Vec<SqlValue>> {
    let Some(first) = rows.first() else {
        return Vec::new();
    };
    let plan = RowPlan::new(first, typed, width);
    rows.iter().map(|row| plan.row(row).collect()).collect()
}

/// Each row as a map from column name to its first `width` cells.
pub(super) fn rows_to_dicts<R: DecodeRow>(
    rows: &[R],
    typed: &[TypedCol],
    width: usize,
) -> Vec<HashMap<String, SqlValue>> {
    let Some(first) = rows.first() else {
        return Vec::new();
    };
    let plan = RowPlan::new(first, typed, width);
    rows.iter()
        .map(|row| {
            use sqlx::Column as _;
            let mut map = HashMap::new();
            for (col, v) in row.columns().iter().zip(plan.row(row)) {
                map.insert(col.name().to_owned(), v);
            }
            map
        })
        .collect()
}

/// The first type in the list sqlx would decode a `ty` column as.
#[cfg(any(feature = "postgres", feature = "mysql"))]
macro_rules! first_compatible {
    ($db:ty, $ty:expr, [$($t:ty => $cell:ident),* $(,)?]) => {{
        $(
            if <$t as sqlx::Type<$db>>::compatible($ty) {
                return Cell::$cell;
            }
        )*
        Cell::Null
    }};
}

/// One typed read; NULL and a failed read are both `None`.
#[cfg(any(feature = "postgres", feature = "mysql"))]
fn get<'r, R, T>(row: &'r R, i: usize) -> Option<T>
where
    R: sqlx::Row,
    usize: sqlx::ColumnIndex<R>,
    T: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    row.try_get::<Option<T>, _>(i).ok().flatten()
}

/// The cells PG and MySQL decode alike.
#[cfg(any(feature = "postgres", feature = "mysql"))]
fn decode_common<'r, R>(row: &'r R, i: usize, cell: Cell) -> Option<SqlValue>
where
    R: sqlx::Row,
    usize: sqlx::ColumnIndex<R>,
    i16: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    i32: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    i64: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    f32: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    f64: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    bool: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    rust_decimal::Decimal: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    String: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    serde_json::Value: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    Vec<u8>: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    chrono::DateTime<chrono::Utc>: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    chrono::NaiveDateTime: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    chrono::NaiveDate: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    chrono::NaiveTime: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    match cell {
        Cell::I16 => get(row, i).map(SqlValue::I16),
        Cell::I32 => get(row, i).map(SqlValue::I32),
        Cell::I64 => get(row, i).map(SqlValue::I64),
        Cell::F32 => get(row, i).map(SqlValue::F32),
        Cell::F64 => get(row, i).map(SqlValue::F64),
        Cell::Bool => get(row, i).map(SqlValue::Bool),
        Cell::Decimal => get(row, i).map(SqlValue::Decimal),
        Cell::String => get(row, i).map(SqlValue::String),
        Cell::Json => get(row, i).map(SqlValue::Json),
        Cell::Binary => get(row, i).map(SqlValue::Binary),
        Cell::DateTime => get(row, i).map(SqlValue::DateTime),
        Cell::NaiveDateTime => {
            get::<_, chrono::NaiveDateTime>(row, i).map(|v| SqlValue::DateTime(v.and_utc()))
        }
        Cell::Date => get(row, i).map(SqlValue::Date),
        Cell::Time => get(row, i).map(SqlValue::Time),
        _ => None,
    }
}

#[cfg(feature = "postgres")]
impl DecodeRow for sqlx::postgres::PgRow {
    /// PG reports the exact column type, so it alone decides.
    fn plan_cell(&self, i: usize, _declared: Option<FieldType>) -> Cell {
        use sqlx::{Column as _, Row as _};
        first_compatible!(sqlx::Postgres, self.column(i).type_info(), [
            i64 => I64,
            i32 => I32,
            i16 => I16,
            f64 => F64,
            f32 => F32,
            bool => Bool,
            rust_decimal::Decimal => Decimal,
            uuid::Uuid => Uuid,
            chrono::DateTime<chrono::Utc> => DateTime,
            chrono::NaiveDateTime => NaiveDateTime,
            chrono::NaiveDate => Date,
            chrono::NaiveTime => Time,
            String => String,
            serde_json::Value => Json,
            Vec<u8> => Binary,
            Vec<String> => TextArray,
            Vec<i64> => I64Array,
            Vec<i32> => I32Array,
        ])
    }

    fn decode_cell(&self, i: usize, cell: Cell) -> SqlValue {
        // `SqlValue` has no list variant, so an `array_agg` reads as a JSON array.
        fn array<T: Into<serde_json::Value>>(v: Vec<T>) -> SqlValue {
            SqlValue::Json(serde_json::Value::Array(
                v.into_iter().map(Into::into).collect(),
            ))
        }
        match cell {
            Cell::Uuid => get(self, i).map(SqlValue::Uuid),
            Cell::TextArray => get::<_, Vec<String>>(self, i).map(array),
            Cell::I64Array => get::<_, Vec<i64>>(self, i).map(array),
            Cell::I32Array => get::<_, Vec<i32>>(self, i).map(array),
            other => decode_common(self, i, other),
        }
        .unwrap_or(SqlValue::Null)
    }
}

#[cfg(feature = "mysql")]
impl DecodeRow for sqlx::mysql::MySqlRow {
    /// MySQL keeps a bool as `TINYINT(1)` and a UUID as `CHAR(36)`, so
    /// only the model tells those apart.
    fn plan_cell(&self, i: usize, declared: Option<FieldType>) -> Cell {
        use sqlx::{Column as _, Row as _, TypeInfo as _};
        let ty = self.column(i).type_info();
        match declared {
            Some(FieldType::Bool) if <bool as sqlx::Type<sqlx::MySql>>::compatible(ty) => {
                return Cell::Bool
            }
            Some(FieldType::Uuid) if <String as sqlx::Type<sqlx::MySql>>::compatible(ty) => {
                return Cell::UuidText
            }
            _ => {}
        }
        // sqlx's i64 and bool decode every integer width, so the name sets
        // the width; `COUNT(*)` and ranks are BIGINT UNSIGNED.
        match ty.name() {
            // A JSON column decodes neither as String nor as bytes (#2296).
            "JSON" => return Cell::Json,
            "SMALLINT" => return Cell::I16,
            "INT" | "MEDIUMINT" => return Cell::I32,
            "FLOAT" => return Cell::F32,
            name if name.contains("UNSIGNED") => {
                if <u64 as sqlx::Type<sqlx::MySql>>::compatible(ty) {
                    return Cell::U64;
                }
            }
            _ => {}
        }
        first_compatible!(sqlx::MySql, ty, [
            i64 => I64,
            f64 => F64,
            bool => Bool,
            rust_decimal::Decimal => Decimal,
            chrono::DateTime<chrono::Utc> => DateTime,
            chrono::NaiveDateTime => NaiveDateTime,
            chrono::NaiveDate => Date,
            chrono::NaiveTime => Time,
            String => String,
            Vec<u8> => Binary,
        ])
    }

    fn decode_cell(&self, i: usize, cell: Cell) -> SqlValue {
        match cell {
            Cell::U64 => get::<_, u64>(self, i).map(|v| {
                i64::try_from(v).map_or_else(|_| SqlValue::String(v.to_string()), SqlValue::I64)
            }),
            Cell::UuidText => get::<_, String>(self, i)
                .map(|s| uuid::Uuid::parse_str(&s).map_or(SqlValue::String(s), SqlValue::Uuid)),
            other => decode_common(self, i, other),
        }
        .unwrap_or(SqlValue::Null)
    }
}

#[cfg(feature = "sqlite")]
impl DecodeRow for sqlx::sqlite::SqliteRow {
    /// SQLite is dynamically typed and reports no useful type for an
    /// expression column, so only the model's type can sharpen a cell.
    fn plan_cell(&self, _i: usize, declared: Option<FieldType>) -> Cell {
        match declared {
            Some(FieldType::I16) => Cell::I16,
            Some(FieldType::I32) => Cell::I32,
            Some(FieldType::F32) => Cell::F32,
            Some(FieldType::F64) => Cell::F64,
            Some(FieldType::Bool) => Cell::Bool,
            Some(FieldType::Decimal) => Cell::Decimal,
            Some(FieldType::Uuid) => Cell::Uuid,
            Some(FieldType::Json) => Cell::Json,
            Some(FieldType::DateTime) => Cell::DateTime,
            Some(FieldType::Date) => Cell::Date,
            Some(FieldType::Time) => Cell::Time,
            _ => Cell::Dynamic,
        }
    }

    /// Read by the cell's storage class, then shape it by `cell`. Typed
    /// reads use `try_get_unchecked`: sqlx checks the declared type, which
    /// for an expression column is wrong (a scalar subquery reads as INTEGER).
    fn decode_cell(&self, i: usize, cell: Cell) -> SqlValue {
        use sqlx::{Row as _, TypeInfo as _, ValueRef as _};
        let Ok(raw) = self.try_get_raw(i) else {
            return SqlValue::Null;
        };
        // A NULL would decode as `0` (#1766).
        if raw.is_null() {
            return SqlValue::Null;
        }
        match raw.type_info().name() {
            "TEXT" => sqlite_text(self, i, cell),
            "BLOB" => match self.try_get_unchecked::<Vec<u8>, _>(i) {
                // sqlx binds a Uuid as 16 raw bytes.
                Ok(b) if cell == Cell::Uuid && b.len() == 16 => {
                    uuid::Uuid::from_slice(&b).map_or(SqlValue::Binary(b), SqlValue::Uuid)
                }
                Ok(b) => SqlValue::Binary(b),
                Err(_) => SqlValue::Null,
            },
            "INTEGER" => match self.try_get_unchecked::<i64, _>(i) {
                Ok(v) => sqlite_int(v, cell),
                Err(_) => SqlValue::Null,
            },
            "REAL" => match self.try_get_unchecked::<f64, _>(i) {
                Ok(v) => sqlite_real(v, cell),
                Err(_) => SqlValue::Null,
            },
            _ => SqlValue::Null,
        }
    }
}

/// A TEXT cell: dates, times, UUIDs, JSON and decimals are stored as text.
/// Text that does not parse as `cell` stays a `String`.
#[cfg(feature = "sqlite")]
fn sqlite_text(row: &sqlx::sqlite::SqliteRow, i: usize, cell: Cell) -> SqlValue {
    use sqlx::Row as _;
    let typed = match cell {
        Cell::DateTime => row
            .try_get_unchecked::<chrono::DateTime<chrono::Utc>, _>(i)
            .ok()
            .map(SqlValue::DateTime),
        Cell::Date => row
            .try_get_unchecked::<chrono::NaiveDate, _>(i)
            .ok()
            .map(SqlValue::Date),
        Cell::Time => row
            .try_get_unchecked::<chrono::NaiveTime, _>(i)
            .ok()
            .map(SqlValue::Time),
        _ => None,
    };
    if let Some(v) = typed {
        return v;
    }
    let Ok(s) = row.try_get_unchecked::<String, _>(i) else {
        return SqlValue::Null;
    };
    match cell {
        Cell::Uuid => uuid::Uuid::parse_str(&s).map_or(SqlValue::String(s), SqlValue::Uuid),
        Cell::Json => serde_json::from_str(&s).map_or(SqlValue::String(s), SqlValue::Json),
        Cell::Decimal => s.parse().map_or(SqlValue::String(s), SqlValue::Decimal),
        _ => SqlValue::String(s),
    }
}

/// An INTEGER cell; a value too wide for the declared type stays `I64`.
#[cfg(feature = "sqlite")]
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn sqlite_int(v: i64, cell: Cell) -> SqlValue {
    match cell {
        Cell::Bool => SqlValue::Bool(v != 0),
        Cell::I16 => i16::try_from(v).map_or(SqlValue::I64(v), SqlValue::I16),
        Cell::I32 => i32::try_from(v).map_or(SqlValue::I64(v), SqlValue::I32),
        Cell::F32 => SqlValue::F32(v as f32),
        Cell::F64 => SqlValue::F64(v as f64),
        Cell::Decimal => SqlValue::Decimal(v.into()),
        _ => SqlValue::I64(v),
    }
}

/// A REAL cell. A NUMERIC column stores a decimal as REAL when it fits.
#[cfg(feature = "sqlite")]
#[allow(clippy::cast_possible_truncation)]
fn sqlite_real(v: f64, cell: Cell) -> SqlValue {
    match cell {
        Cell::F32 => SqlValue::F32(v as f32),
        Cell::Decimal => {
            rust_decimal::Decimal::try_from(v).map_or(SqlValue::F64(v), SqlValue::Decimal)
        }
        _ => SqlValue::F64(v),
    }
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;

    /// A `Decimal` model does not build with `sqlite`, so its cells are
    /// checked here: NUMERIC keeps a decimal as INTEGER, REAL or TEXT.
    #[test]
    fn sqlite_decimal_cells_read_as_decimal() {
        let d = |s: &str| SqlValue::Decimal(s.parse().unwrap());
        assert_eq!(sqlite_int(3, Cell::Decimal), d("3"));
        assert_eq!(sqlite_real(12.34, Cell::Decimal), d("12.34"));
        assert_eq!(sqlite_real(12.34, Cell::Dynamic), SqlValue::F64(12.34));
        assert_eq!(sqlite_int(70_000, Cell::I16), SqlValue::I64(70_000));
    }
}
