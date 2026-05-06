//! `SqlValue` — the dialect-neutral value carrier between the query layer
//! and the SQL backend.
//!
//! Every concrete bound parameter ultimately becomes one of these.

use chrono::{DateTime, NaiveDate, Utc};
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
    /// Used for `IN` and `BETWEEN` lookups.
    List(Vec<SqlValue>),
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
            Self::List(items) => {
                let inner: Vec<String> = items.iter().map(Self::to_display_string).collect();
                format!("[{}]", inner.join(", "))
            }
        }
    }

    /// Returns the `FieldType` this value corresponds to, or `None` for `Null` / `List`.
    #[must_use]
    pub fn field_type(&self) -> Option<FieldType> {
        Some(match self {
            Self::Null | Self::List(_) => return None,
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
    Uuid => Uuid,
    serde_json::Value => Json,
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
