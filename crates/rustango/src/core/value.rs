//! `SqlValue` — the backend-neutral value passed from the query layer
//! to the SQL backend. Every bound parameter becomes one of these.

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
    /// For `IN` and `BETWEEN`. The writer expands it to
    /// `($1, $2, $3)`, one bound parameter per element.
    List(Vec<SqlValue>),
    /// PG array bound as one placeholder holding a `Vec<T>`. Unlike
    /// [`Self::List`], it does not fan out to one placeholder per
    /// element. Used by [`crate::core::Op::ArrayContains`],
    /// `ArrayContainedBy` and `ArrayOverlap` (PG `@>` / `<@` / `&&`).
    ///
    /// The first element picks the bind type: `I32`, `I64`, `String`
    /// or `Bool`. Other element types error at bind time. An empty
    /// array binds as `Vec::<i32>::new()`; PG then infers the element
    /// type from the clause.
    Array(Vec<SqlValue>),
    /// PG range literal, bound as text. Postgres casts it to the
    /// column's range type. The string is bound as-is; we never parse
    /// or check it. Pairs with [`crate::core::Op::RangeContains`],
    /// `RangeContainedBy`, `RangeOverlap`, `RangeStrictlyLeft`,
    /// `RangeStrictlyRight` and `RangeAdjacent`.
    ///
    /// Examples: `"[1, 10)"`, `"[5,)"`,
    /// `"[2025-01-01, 2025-02-01)"`, `"empty"`, `"(,)"`.
    RangeLiteral(String),
    /// PG `hstore` map as `(key, value)` pairs, where a value may be
    /// NULL. The PG bind path turns it into a `PgHstore`; MySQL and
    /// SQLite reject it. Build it from [`crate::sql::HStore`].
    HStore(Vec<(String, Option<String>)>),
    /// pgvector embedding as a dense `Vec<f32>`. The PG bind path
    /// encodes the pgvector wire format; MySQL and SQLite reject it.
    /// Build it from [`crate::sql::Vector`].
    Vector(Vec<f32>),
    /// PostGIS `geometry(Point, …)`: 2-D coordinates plus an SRID. The
    /// PG bind path encodes it as EWKB; MySQL and SQLite reject it.
    /// Build it from [`crate::sql::Point`].
    Geometry {
        x: f64,
        y: f64,
        srid: u32,
    },
}

impl SqlValue {
    /// A short, readable form of the value for error messages. Lists
    /// become `[a, b, …]` and `Null` becomes `NULL`. Never use it to
    /// build SQL; the dialect formatter does that.
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
            Self::Geometry { x, y, srid } => format!("SRID={srid};POINT({x} {y})"),
        }
    }

    /// The [`FieldType`] for this value, or `None` for `Null` and the
    /// container variants (`List`, `Array`, `RangeLiteral`, `HStore`,
    /// `Vector`, `Geometry`), which carry no checkable type here.
    #[must_use]
    pub fn field_type(&self) -> Option<FieldType> {
        Some(match self {
            Self::Null
            | Self::List(_)
            | Self::Array(_)
            | Self::RangeLiteral(_)
            | Self::HStore(_)
            // No dimension here, so it can't be checked against the
            // column's `FieldType::Vector(dims)` from the schema.
            | Self::Vector(_)
            // Same for the SRID in `FieldType::Geometry(srid)`.
            | Self::Geometry { .. } => return None,
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
