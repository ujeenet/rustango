//! `Array<T>` — typed PostgreSQL array column wrapper (Django's
//! `ArrayField`, issue #341).
//!
//! Declare a native PG array column on a model:
//!
//! ```ignore
//! #[derive(Model)]
//! #[rustango(table = "post")]
//! struct Post {
//!     #[rustango(primary_key)]
//!     id: Auto<i64>,
//!     #[rustango(max_length = 200)]
//!     title: String,
//!     // → DDL `tags text[]`; round-trips as a Rust `Vec<String>`.
//!     tags: Array<String>,
//! }
//! ```
//!
//! The element type drives the column type emitted by the migration
//! writer: `Array<String>` → `text[]`, `Array<i32>` → `integer[]`,
//! `Array<i64>` → `bigint[]`. Pairs with the already-shipped value-layer
//! array operators ([`crate::core::Op::ArrayContains`] / `ArrayContainedBy`
//! / `ArrayOverlap`, PG `@>` / `<@` / `&&`).
//!
//! ## PostgreSQL only, by language semantics
//!
//! Native typed arrays are a Postgres feature; MySQL and SQLite have no
//! equivalent column type. Like trigram / full-text / pgvector, `Array<T>`
//! is **PG-only by language semantics**: the migration writer emits a
//! degraded `TEXT` column on MySQL / SQLite, and the [`sqlx::Decode`] /
//! `SqlValue` bind paths on those backends return / raise a clear error
//! rather than silently mis-storing. Use `Array<T>` only on a Postgres
//! deployment. The type still *compiles* under every backend (the
//! per-backend [`sqlx::Type`] / [`sqlx::Decode`] impls below are total)
//! so the `sqlite,tenancy` litmus build keeps passing.
//!
//! ## Why a newtype rather than a bare `Vec<T>`
//!
//! `#[derive(Model)]` emits one shared `FromRow` body reused across the
//! Postgres / MySQL / SQLite decoders (`Row::try_get::<FieldTy>(…)`).
//! `Vec<String>` implements `sqlx::Decode` only for Postgres, so a bare
//! `Vec<String>` field would fail to compile the SQLite / MySQL decoder
//! arms. `Array<T>` carries total `Decode` / `Type` impls for all three
//! backends (real on PG, erroring stubs elsewhere), so the shared
//! `FromRow` body compiles everywhere. Same pattern as [`crate::sql::Auto`].

use std::ops::{Deref, DerefMut};

/// Typed PostgreSQL array column — see the [module docs](self).
///
/// Transparent newtype over `Vec<T>`: `Deref`s to the inner vector,
/// converts from/into `Vec<T>`, and (de)serializes as a plain JSON
/// array. The `Model` derive maps `Array<String>` / `Array<i32>` /
/// `Array<i64>` fields to `text[]` / `integer[]` / `bigint[]` DDL.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct Array<T>(pub Vec<T>);

impl<T> Array<T> {
    /// Wrap a `Vec<T>` as an array column value.
    #[must_use]
    pub fn new(items: Vec<T>) -> Self {
        Self(items)
    }

    /// Consume the wrapper, returning the inner `Vec<T>`.
    #[must_use]
    pub fn into_inner(self) -> Vec<T> {
        self.0
    }
}

impl<T> Deref for Array<T> {
    type Target = Vec<T>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for Array<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> From<Vec<T>> for Array<T> {
    fn from(v: Vec<T>) -> Self {
        Self(v)
    }
}

impl<T> From<Array<T>> for Vec<T> {
    fn from(a: Array<T>) -> Self {
        a.0
    }
}

impl<T> FromIterator<T> for Array<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

// ---- serde: behave exactly like the inner Vec (plain JSON array) ----

impl<T: serde::Serialize> serde::Serialize for Array<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de, T> serde::Deserialize<'de> for Array<T>
where
    T: serde::Deserialize<'de>,
{
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Vec::<T>::deserialize(deserializer).map(Self)
    }
}

// ---- `Array<T>` → `SqlValue` (INSERT / UPDATE bind, via SqlValue::Array) ----
//
// The Model derive binds field values through `Into<SqlValue>`, so each
// supported element type needs a concrete conversion into the
// single-parameter `SqlValue::Array` (which the PG bind path lowers to a
// typed array; the MySQL / SQLite bind paths reject it — PG-only).

impl From<Array<String>> for crate::core::SqlValue {
    fn from(a: Array<String>) -> Self {
        crate::core::SqlValue::Array(a.0.into_iter().map(crate::core::SqlValue::String).collect())
    }
}

impl From<Array<i32>> for crate::core::SqlValue {
    fn from(a: Array<i32>) -> Self {
        crate::core::SqlValue::Array(a.0.into_iter().map(crate::core::SqlValue::I32).collect())
    }
}

impl From<Array<i64>> for crate::core::SqlValue {
    fn from(a: Array<i64>) -> Self {
        crate::core::SqlValue::Array(a.0.into_iter().map(crate::core::SqlValue::I64).collect())
    }
}

// ---- sqlx Type + Decode (FromRow read path) ----
//
// Postgres: delegate to `Vec<T>` (the real, functional array decode).
// MySQL / SQLite: total stubs so the shared `FromRow` body compiles —
// `Type` borrows a placeholder type-info, `Decode` errors at runtime.
// Encode is intentionally NOT implemented: the bind path goes through
// `SqlValue::Array`, never the raw `Array<T>`.

#[cfg(feature = "postgres")]
impl<'r, T> sqlx::Decode<'r, sqlx::Postgres> for Array<T>
where
    Vec<T>: sqlx::Decode<'r, sqlx::Postgres>,
{
    fn decode(
        value: <sqlx::Postgres as sqlx::Database>::ValueRef<'r>,
    ) -> Result<Self, sqlx::error::BoxDynError> {
        Ok(Self(<Vec<T> as sqlx::Decode<'r, sqlx::Postgres>>::decode(
            value,
        )?))
    }
}

#[cfg(feature = "postgres")]
impl<T> sqlx::Type<sqlx::Postgres> for Array<T>
where
    Vec<T>: sqlx::Type<sqlx::Postgres>,
{
    fn type_info() -> sqlx::postgres::PgTypeInfo {
        <Vec<T> as sqlx::Type<sqlx::Postgres>>::type_info()
    }

    fn compatible(ty: &sqlx::postgres::PgTypeInfo) -> bool {
        <Vec<T> as sqlx::Type<sqlx::Postgres>>::compatible(ty)
    }
}

#[cfg(feature = "mysql")]
impl<'r, T> sqlx::Decode<'r, sqlx::MySql> for Array<T> {
    fn decode(
        _value: <sqlx::MySql as sqlx::Database>::ValueRef<'r>,
    ) -> Result<Self, sqlx::error::BoxDynError> {
        Err("`Array<T>` columns are PostgreSQL-only; cannot decode on MySQL (issue #341)".into())
    }
}

#[cfg(feature = "mysql")]
impl<T> sqlx::Type<sqlx::MySql> for Array<T> {
    fn type_info() -> sqlx::mysql::MySqlTypeInfo {
        // Placeholder so the trait is total; the column is never actually
        // a MySQL array (decode errors before any value is produced).
        <Vec<u8> as sqlx::Type<sqlx::MySql>>::type_info()
    }
}

#[cfg(feature = "sqlite")]
impl<'r, T> sqlx::Decode<'r, sqlx::Sqlite> for Array<T> {
    fn decode(
        _value: <sqlx::Sqlite as sqlx::Database>::ValueRef<'r>,
    ) -> Result<Self, sqlx::error::BoxDynError> {
        Err("`Array<T>` columns are PostgreSQL-only; cannot decode on SQLite (issue #341)".into())
    }
}

#[cfg(feature = "sqlite")]
impl<T> sqlx::Type<sqlx::Sqlite> for Array<T> {
    fn type_info() -> sqlx::sqlite::SqliteTypeInfo {
        <Vec<u8> as sqlx::Type<sqlx::Sqlite>>::type_info()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deref_and_from_vec() {
        let a: Array<String> = vec!["x".to_owned(), "y".to_owned()].into();
        assert_eq!(a.len(), 2);
        assert_eq!(a[0], "x");
        assert_eq!(a.into_inner(), vec!["x".to_owned(), "y".to_owned()]);
    }

    #[test]
    fn serde_round_trips_as_plain_array() {
        let a: Array<i32> = Array::new(vec![1, 2, 3]);
        let json = serde_json::to_string(&a).unwrap();
        assert_eq!(json, "[1,2,3]");
        let back: Array<i32> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, a);
    }

    #[test]
    fn into_sqlvalue_array_string() {
        let v: crate::core::SqlValue = Array(vec!["a".to_owned(), "b".to_owned()]).into();
        match v {
            crate::core::SqlValue::Array(items) => {
                assert_eq!(items.len(), 2);
                assert!(matches!(items[0], crate::core::SqlValue::String(_)));
            }
            _ => panic!("expected SqlValue::Array"),
        }
    }

    #[test]
    fn into_sqlvalue_array_ints() {
        let v: crate::core::SqlValue = Array(vec![1i32, 2, 3]).into();
        match v {
            crate::core::SqlValue::Array(items) => {
                assert_eq!(items.len(), 3);
                assert!(matches!(items[0], crate::core::SqlValue::I32(1)));
            }
            _ => panic!("expected SqlValue::Array"),
        }
    }
}
