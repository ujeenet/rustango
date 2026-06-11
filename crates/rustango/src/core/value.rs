//! `SqlValue` — the dialect-neutral value carrier between the query layer
//! and the SQL backend.
//!
//! Every concrete bound parameter ultimately becomes one of these.

use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use rust_decimal::Decimal;
use uuid::Uuid;

use super::FieldType;

/// A typed value that can be bound to a query parameter.
#[derive(Debug, Clone, PartialEq)]
pub enum SqlValue {
    Null,
    /// 2-byte signed integer — Postgres `SMALLINT`, MySQL `SMALLINT`.
    I16(i16),
    I32(i32),
    I64(i64),
    F32(f32),
    F64(f64),
    Bool(bool),
    String(String),
    DateTime(DateTime<Utc>),
    Date(NaiveDate),
    Uuid(Uuid),
    Json(serde_json::Value),
    /// Fixed-point exact decimal — pairs with [`FieldType::Decimal`].
    /// Postgres / MySQL `NUMERIC` / `DECIMAL`; SQLite text-affinity
    /// `NUMERIC`. Rust type: `rust_decimal::Decimal`.
    Decimal(Decimal),
    /// Binary blob — pairs with [`FieldType::Binary`]. PG `BYTEA`,
    /// MySQL `LONGBLOB`, SQLite `BLOB`.
    Binary(Vec<u8>),
    /// Time of day, no date component — pairs with
    /// [`FieldType::Time`]. Rust type: `chrono::NaiveTime`.
    Time(NaiveTime),
    /// Used for `IN` and `BETWEEN` lookups. Expanded to `($1, $2, $3)`
    /// placeholders at writer time — each element is a separate bound
    /// parameter.
    List(Vec<SqlValue>),
    /// PG **single-parameter** array — binds as a single placeholder
    /// (`$1`) holding a `Vec<T>` value. Distinct from [`Self::List`],
    /// which fans out to one placeholder per element. Used by
    /// [`crate::core::Op::ArrayContains`] / `ArrayContainedBy` /
    /// `ArrayOverlap` (PG `@>` / `<@` / `&&`).
    ///
    /// The first element's variant determines the bind shape. **v1
    /// supports `I64` and `String` element types** (the two most
    /// common Django `ArrayField(IntegerField/CharField)` shapes);
    /// other element kinds error at bind time. Empty arrays bind as
    /// `Vec::<i32>::new()` — PG infers the element type from the
    /// `<col> <op>` clause. Issue #30.
    Array(Vec<SqlValue>),
    /// PG range literal as text — bound as a `String` parameter that
    /// Postgres implicit-casts to the column's range type when the
    /// operator's LHS is a range column. Pairs with
    /// [`crate::core::Op::RangeContains`] / `RangeContainedBy` /
    /// `RangeOverlap` / `RangeStrictlyLeft` / `RangeStrictlyRight` /
    /// `RangeAdjacent`. Issue #31.
    ///
    /// Example literals (one element type per column type):
    /// - int4range / int8range: `"[1, 10)"`, `"(1, 10]"`, `"[5,)"`
    /// - daterange: `"[2025-01-01, 2025-02-01)"`
    /// - tstzrange: `"[2025-01-01 00:00+00, 2025-02-01 00:00+00)"`
    /// - empty range: `"empty"`
    /// - unbounded: `"(,)"`
    ///
    /// The string is bound as-is; we don't parse or validate the
    /// shape (sticking to the same "PG handles the cast" pattern as
    /// our other text-bound types like Json).
    RangeLiteral(String),
    /// PG `hstore` map — a flat list of `(key, value)` pairs where the
    /// value is nullable (`"k" => NULL`). Issue #342. Backend-neutral at
    /// this layer (a `Vec` of pairs); the PG bind path reconstructs a
    /// native `sqlx::postgres::types::PgHstore`, MySQL / SQLite reject
    /// (no `hstore` type). Built from [`crate::sql::HStore`] via
    /// `Into<SqlValue>`.
    HStore(Vec<(String, Option<String>)>),
    /// pgvector embedding — a dense `Vec<f32>`. Issue #824. Backend-
    /// neutral at this layer; the PG bind path encodes it as the
    /// pgvector `vector` wire format (via [`crate::sql::Vector`]), MySQL
    /// / SQLite reject (no `vector` type). Built from
    /// [`crate::sql::Vector`] via `Into<SqlValue>`.
    Vector(Vec<f32>),
}

impl SqlValue {
    /// Render the value as a short human-readable string for use in
    /// error messages. Lists collapse into `[a, b, …]`; `Null` shows
    /// `NULL`; binary-ish payloads (`Json`) show their `Display` form.
    /// This is NOT for SQL emission — use the dialect formatter for
    /// that. Only callers that want to embed the value in a
    /// user-facing error message should reach for this.
    #[must_use]
    pub fn to_display_string(&self) -> String {
        match self {
            Self::Null => "NULL".to_owned(),
            Self::I16(v) => v.to_string(),
            Self::I32(v) => v.to_string(),
            Self::I64(v) => v.to_string(),
            Self::F32(v) => v.to_string(),
            Self::F64(v) => v.to_string(),
            Self::Bool(v) => v.to_string(),
            Self::String(v) => v.clone(),
            Self::DateTime(v) => v.to_rfc3339(),
            Self::Date(v) => v.to_string(),
            Self::Uuid(v) => v.to_string(),
            Self::Json(v) => v.to_string(),
            Self::Decimal(v) => v.to_string(),
            Self::Binary(bytes) => format!("<binary {} bytes>", bytes.len()),
            Self::Time(v) => v.to_string(),
            Self::List(items) => {
                let inner: Vec<String> = items.iter().map(Self::to_display_string).collect();
                format!("[{}]", inner.join(", "))
            }
            Self::Array(items) => {
                let inner: Vec<String> = items.iter().map(Self::to_display_string).collect();
                format!("array[{}]", inner.join(", "))
            }
            Self::RangeLiteral(s) => format!("range{s}"),
            Self::HStore(pairs) => {
                let inner: Vec<String> = pairs
                    .iter()
                    .map(|(k, v)| match v {
                        Some(v) => format!("{k}=>{v}"),
                        None => format!("{k}=>NULL"),
                    })
                    .collect();
                format!("hstore{{{}}}", inner.join(", "))
            }
            Self::Vector(items) => {
                let inner: Vec<String> = items.iter().map(f32::to_string).collect();
                format!("vector[{}]", inner.join(", "))
            }
        }
    }

    /// Returns the `FieldType` this value corresponds to, or `None` for `Null` / `List`.
    #[must_use]
    pub fn field_type(&self) -> Option<FieldType> {
        Some(match self {
            Self::Null
            | Self::List(_)
            | Self::Array(_)
            | Self::RangeLiteral(_)
            | Self::HStore(_)
            // `Vector` carries no dimension here; the column's
            // `FieldType::Vector(dims)` comes from the schema, so a bound
            // value can't be type-checked against it. Skip (like Array).
            | Self::Vector(_) => return None,
            Self::I16(_) => FieldType::I16,
            Self::I32(_) => FieldType::I32,
            Self::I64(_) => FieldType::I64,
            Self::F32(_) => FieldType::F32,
            Self::F64(_) => FieldType::F64,
            Self::Bool(_) => FieldType::Bool,
            Self::String(_) => FieldType::String,
            Self::DateTime(_) => FieldType::DateTime,
            Self::Date(_) => FieldType::Date,
            Self::Uuid(_) => FieldType::Uuid,
            Self::Json(_) => FieldType::Json,
            Self::Decimal(_) => FieldType::Decimal,
            Self::Binary(_) => FieldType::Binary,
            Self::Time(_) => FieldType::Time,
        })
    }
}

macro_rules! sql_value_from {
    ($($t:ty => $variant:ident),+ $(,)?) => {
        $(
            impl From<$t> for SqlValue {
                fn from(v: $t) -> Self { Self::$variant(v) }
            }
        )+
    };
}

sql_value_from! {
    i16 => I16,
    i32 => I32,
    i64 => I64,
    f32 => F32,
    f64 => F64,
    bool => Bool,
    String => String,
    DateTime<Utc> => DateTime,
    NaiveDate => Date,
    NaiveTime => Time,
    Uuid => Uuid,
    serde_json::Value => Json,
    Decimal => Decimal,
    Vec<u8> => Binary,
}

impl From<&str> for SqlValue {
    fn from(v: &str) -> Self {
        Self::String(v.to_owned())
    }
}

impl<T: Into<SqlValue>> From<Option<T>> for SqlValue {
    fn from(v: Option<T>) -> Self {
        match v {
            Some(x) => x.into(),
            None => Self::Null,
        }
    }
}
