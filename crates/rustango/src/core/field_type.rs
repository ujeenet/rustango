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
        }
    }
}

impl core::fmt::Display for FieldType {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}
