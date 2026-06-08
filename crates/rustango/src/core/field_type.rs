//! `FieldType` — the dialect-neutral classification of a column.
//!
//! The query layer uses this to decide which lookups apply, and the SQL
//! layer uses it to drive type-aware coercion. It does not encode
//! length/precision; those are dialect-specific concerns expressed via
//! attributes that ride alongside the schema.

/// Kind of value stored in a column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FieldType {
    /// `i16` — Postgres `SMALLINT` / MySQL `SMALLINT`. 2 bytes signed,
    /// range `-32768..=32767`. Smallest portable integer width — both
    /// backends support it natively, no CHECK-constraint emulation. We
    /// don't ship `i8` because Postgres has no 1-byte signed integer
    /// type (its `"char"` type is not a portable signed scalar), so an
    /// `i8` field would silently store as 2 bytes on PG and 1 byte on
    /// MySQL — the kind of cross-dialect skew rustango avoids.
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
    /// Fixed-point exact decimal — Django's `DecimalField`. Postgres
    /// `NUMERIC` (arbitrary precision), MySQL `DECIMAL(38, 10)`
    /// (default precision/scale; override at schema level via attrs
    /// once we expose them), SQLite `NUMERIC` (text affinity, exact
    /// arithmetic via the `decimal` module of `sqlx-sqlite`). Rust
    /// type: `rust_decimal::Decimal`. Use for money / metric data
    /// where `f64` rounding would be unacceptable.
    Decimal,
    /// Binary blob — Django's `BinaryField`. Postgres `BYTEA`, MySQL
    /// `LONGBLOB`, SQLite `BLOB`. Rust type: `Vec<u8>`. No length cap
    /// at the type level; deployments add CHECK constraints if needed.
    Binary,
    /// Time of day, no date component — Django's `TimeField`. Postgres
    /// `TIME`, MySQL `TIME(6)`, SQLite `TIME` (text affinity, `HH:MM:SS`
    /// shape). Rust type: `chrono::NaiveTime`.
    Time,
    /// Native PostgreSQL array — Django's `ArrayField` (#341). Rust type:
    /// [`crate::sql::Array<T>`]. The element kind selects the column type
    /// (`text[]` / `integer[]` / `bigint[]`). **PG-only by language
    /// semantics**: MySQL / SQLite have no array column type, so the DDL
    /// writer degrades to `TEXT` and the bind / decode paths error there.
    Array(ArrayElem),
    /// Native PostgreSQL range — Django's `RangeField` family (#343).
    /// Rust type: [`crate::sql::Range<T>`]. The element kind selects the
    /// column type (`int4range` / `int8range` / `numrange` / `daterange`
    /// / `tstzrange`). **PG-only by language semantics** like
    /// [`Self::Array`]: MySQL / SQLite degrade to `TEXT` and the decode
    /// path errors there.
    Range(RangeElem),
    /// Native PostgreSQL `hstore` — Django's `HStoreField` (#342). A flat
    /// string→string map. Rust type: [`crate::sql::HStore`]. **PG-only by
    /// language semantics** like [`Self::Array`] / [`Self::Range`]; MySQL
    /// / SQLite degrade to `TEXT` and the decode path errors there.
    /// Requires the `hstore` extension on the database.
    HStore,
}

/// Element type of a [`FieldType::Range`] column (#343). Selects the
/// PostgreSQL range column type emitted by the migration writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RangeElem {
    /// `int4range` — element `i32` ([`crate::sql::Range<i32>`]).
    /// Django `IntegerRangeField`.
    Int,
    /// `int8range` — element `i64` ([`crate::sql::Range<i64>`]).
    /// Django `BigIntegerRangeField`.
    BigInt,
    /// `numrange` — element `rust_decimal::Decimal`. Django `DecimalRangeField`.
    Numeric,
    /// `daterange` — element `chrono::NaiveDate`. Django `DateRangeField`.
    Date,
    /// `tstzrange` — element `chrono::DateTime<Utc>`. Django `DateTimeRangeField`.
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

/// Element type of a [`FieldType::Array`] column (#341). Selects the
/// PostgreSQL array column type emitted by the migration writer. Kept a
/// small `Copy` enum so [`FieldType`] stays `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArrayElem {
    /// `text[]` — element type `String` ([`crate::sql::Array<String>`]).
    /// Django `ArrayField(CharField/TextField)`.
    Text,
    /// `integer[]` — element type `i32` ([`crate::sql::Array<i32>`]).
    /// Django `ArrayField(IntegerField)`.
    Int,
    /// `bigint[]` — element type `i64` ([`crate::sql::Array<i64>`]).
    /// Django `ArrayField(BigIntegerField)`.
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
        }
    }
}

impl core::fmt::Display for FieldType {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}
