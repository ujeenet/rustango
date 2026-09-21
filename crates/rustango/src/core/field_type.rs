//! `FieldType` — the dialect-neutral kind of a column.
//!
//! The query layer uses it to pick the lookups that apply. The SQL layer
//! uses it to coerce values. It carries no length or precision; those
//! are dialect-specific and come from schema attributes.

/// Kind of value stored in a column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FieldType {
    /// `i16` — `SMALLINT` on Postgres and MySQL, range
    /// `-32768..=32767`. This is the smallest portable integer width.
    /// There is no `i8`: Postgres has no 1-byte signed type, so such a
    /// field would use a different width on each backend.
    I16,
    I32,
    I64,
    F32,
    F64,
    Bool,
    String,
    DateTime,
    Date,
    Uuid,
    Json,
    /// Exact fixed-point decimal.
    /// Postgres `NUMERIC`, MySQL `DECIMAL(38, 10)`, SQLite `NUMERIC`.
    /// Rust type: `rust_decimal::Decimal`. Use it for money and other
    /// data where `f64` rounding would be wrong.
    Decimal,
    /// Binary blob. Postgres `BYTEA`,
    /// MySQL `LONGBLOB`, SQLite `BLOB`. Rust type: `Vec<u8>`. There is
    /// no length cap; add a CHECK constraint if you need one.
    Binary,
    /// Time of day with no date. Postgres
    /// `TIME`, MySQL `TIME(6)`, SQLite `TIME` (text, `HH:MM:SS`). Rust
    /// type: `chrono::NaiveTime`.
    Time,
    /// Postgres array. Rust type:
    /// [`crate::sql::Array<T>`]. The element kind picks the column type
    /// (`text[]` / `integer[]` / `bigint[]`). **Postgres only**: MySQL
    /// and SQLite have no array type, so the DDL falls back to `TEXT`
    /// and the bind and decode paths fail there.
    Array(ArrayElem),
    /// Postgres range. Rust type:
    /// [`crate::sql::Range<T>`]. The element kind picks the column type
    /// (`int4range` / `int8range` / `numrange` / `daterange` /
    /// `tstzrange`). **Postgres only**, like [`Self::Array`].
    Range(RangeElem),
    /// Postgres `hstore`: a flat string-to-string map.
    /// Rust type: [`crate::sql::HStore`].
    /// **Postgres only**, like [`Self::Array`] / [`Self::Range`].
    /// Needs the `hstore` extension.
    HStore,
    /// pgvector `vector(N)` embedding column. `N` is the dimension; `0`
    /// means no fixed dimension. Rust type: [`crate::sql::Vector`].
    /// **Postgres only**, like [`Self::Array`]; the distance operators
    /// fail elsewhere. Needs the `vector` extension.
    Vector(u32),
    /// PostGIS `geometry(Point, SRID)` column. The `u32` is the SRID
    /// (4326 = WGS 84). Rust type: [`crate::sql::Point`]. **PostGIS
    /// only**, like [`Self::Vector`]. Only the `Point` subtype is
    /// supported.
    Geometry(u32),
}

/// Element type of a [`FieldType::Range`] column. Picks the Postgres
/// range type the migration writer emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RangeElem {
    /// `int4range` — element `i32` ([`crate::sql::Range<i32>`]).
    Int,
    /// `int8range` — element `i64` ([`crate::sql::Range<i64>`]).
    BigInt,
    /// `numrange` — element `rust_decimal::Decimal`.
    Numeric,
    /// `daterange` — element `chrono::NaiveDate`.
    Date,
    /// `tstzrange` — element `chrono::DateTime<Utc>`.
    DateTime,
}

impl RangeElem {
    /// PostgreSQL range type name.
    #[must_use]
    pub const fn pg_range_type(self) -> &'static str {
        match self {
            Self::Int => "int4range",
            Self::BigInt => "int8range",
            Self::Numeric => "numrange",
            Self::Date => "daterange",
            Self::DateTime => "tstzrange",
        }
    }
}

/// Element type of a [`FieldType::Array`] column. Picks the Postgres
/// array type the migration writer emits. Kept a small `Copy` enum so
/// [`FieldType`] stays `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArrayElem {
    /// `text[]` — element type `String` ([`crate::sql::Array<String>`]).
    Text,
    /// `integer[]` — element type `i32` ([`crate::sql::Array<i32>`]).
    Int,
    /// `bigint[]` — element type `i64` ([`crate::sql::Array<i64>`]).
    BigInt,
}

impl ArrayElem {
    /// Scalar SQL element type (the part before `[]`). Postgres spelling.
    #[must_use]
    pub const fn pg_element_type(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Int => "integer",
            Self::BigInt => "bigint",
        }
    }
}

impl FieldType {
    /// Human-readable name, used in error messages.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::I16 => "i16",
            Self::I32 => "i32",
            Self::I64 => "i64",
            Self::F32 => "f32",
            Self::F64 => "f64",
            Self::Bool => "bool",
            Self::String => "String",
            Self::DateTime => "DateTime<Utc>",
            Self::Date => "NaiveDate",
            Self::Uuid => "Uuid",
            Self::Json => "serde_json::Value",
            Self::Decimal => "rust_decimal::Decimal",
            Self::Binary => "Vec<u8>",
            Self::Time => "NaiveTime",
            Self::Array(ArrayElem::Text) => "Array<String>",
            Self::Array(ArrayElem::Int) => "Array<i32>",
            Self::Array(ArrayElem::BigInt) => "Array<i64>",
            Self::Range(RangeElem::Int) => "Range<i32>",
            Self::Range(RangeElem::BigInt) => "Range<i64>",
            Self::Range(RangeElem::Numeric) => "Range<Decimal>",
            Self::Range(RangeElem::Date) => "Range<NaiveDate>",
            Self::Range(RangeElem::DateTime) => "Range<DateTime<Utc>>",
            Self::HStore => "HStore",
            Self::Vector(_) => "Vector",
            Self::Geometry(_) => "Point",
        }
    }
}

impl core::fmt::Display for FieldType {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}
