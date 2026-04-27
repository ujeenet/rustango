//! `FieldType` — the dialect-neutral classification of a column.
//!
//! The query layer uses this to decide which lookups apply, and the SQL
//! layer uses it to drive type-aware coercion. It does not encode
//! length/precision; those are dialect-specific concerns expressed via
//! attributes that ride alongside the schema.

/// Kind of value stored in a column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FieldType {
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
