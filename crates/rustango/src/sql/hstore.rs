//! `HStore` — typed PostgreSQL `hstore` (string→string map) column
//! wrapper (Django's `HStoreField`, issue #342).
//!
//! Declare an `hstore` column on a model:
//!
//! ```ignore
//! use rustango::sql::{Auto, HStore};
//!
//! #[derive(Model)]
//! #[rustango(table = "product")]
//! struct Product {
//!     #[rustango(primary_key)]
//!     id: Auto<i64>,
//!     // → DDL `attrs hstore`; round-trips as a string→string map.
//!     attrs: HStore,
//! }
//!
//! let p = Product {
//!     id: Auto::default(),
//!     attrs: HStore::from_iter([("color", "red"), ("size", "L")]),
//! };
//! ```
//!
//! `hstore` stores a flat map of text keys to **nullable** text values
//! (`"k" => NULL` is valid), so the inner type is
//! `BTreeMap<String, Option<String>>` — matching sqlx's
//! [`sqlx::postgres::types::PgHstore`], which `HStore` round-trips
//! through. The migration writer emits the `hstore` column type and the
//! values bind / decode as a native hstore (no text-literal escaping).
//!
//! ## Requires the `hstore` extension
//!
//! `hstore` is a Postgres contrib type — the database must have
//! `CREATE EXTENSION hstore` applied before an `hstore` column can be
//! created or queried.
//!
//! ## PostgreSQL only, by language semantics
//!
//! MySQL and SQLite have no `hstore` equivalent. Like
//! [`Array`](crate::sql::Array) / [`Range`](crate::sql::Range), `HStore`
//! is **PG-only by language semantics**: the migration writer degrades to
//! `TEXT` on MySQL / SQLite and the bind / decode paths error there. The
//! type still *compiles* under every backend (the per-backend
//! [`sqlx::Type`] / [`sqlx::Decode`] impls are total) so the
//! `sqlite,tenancy` litmus build keeps passing.

use std::collections::BTreeMap;
use std::ops::{Deref, DerefMut};

/// Typed PostgreSQL `hstore` column — see the [module docs](self).
///
/// Transparent newtype over `BTreeMap<String, Option<String>>`:
/// `Deref`s to the inner map and (de)serializes as a JSON object.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HStore(pub BTreeMap<String, Option<String>>);

impl HStore {
    /// Empty map.
    #[must_use]
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// Consume the wrapper, returning the inner map.
    #[must_use]
    pub fn into_inner(self) -> BTreeMap<String, Option<String>> {
        self.0
    }
}

impl Deref for HStore {
    type Target = BTreeMap<String, Option<String>>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for HStore {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<BTreeMap<String, Option<String>>> for HStore {
    fn from(m: BTreeMap<String, Option<String>>) -> Self {
        Self(m)
    }
}

impl From<BTreeMap<String, String>> for HStore {
    fn from(m: BTreeMap<String, String>) -> Self {
        Self(m.into_iter().map(|(k, v)| (k, Some(v))).collect())
    }
}

/// `from_iter([("k", "v"), …])` — convenience for all-present (non-NULL)
/// values. Keys/values can be anything `Into<String>` (e.g. `&str`).
impl<K: Into<String>, V: Into<String>> FromIterator<(K, V)> for HStore {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        Self(
            iter.into_iter()
                .map(|(k, v)| (k.into(), Some(v.into())))
                .collect(),
        )
    }
}

// ---- serde: a JSON object (`null` values allowed) ----

impl serde::Serialize for HStore {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for HStore {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        BTreeMap::<String, Option<String>>::deserialize(deserializer).map(Self)
    }
}

// ---- `HStore` → `SqlValue` (INSERT / UPDATE bind) ----
//
// Lowers to the backend-neutral `SqlValue::HStore` pair list; the PG
// bind path reconstructs a native `PgHstore` (no text-literal escaping).

impl From<HStore> for crate::core::SqlValue {
    fn from(h: HStore) -> Self {
        crate::core::SqlValue::HStore(h.0.into_iter().collect())
    }
}

// ---- sqlx Type + Decode (FromRow read path) ----
//
// Postgres: delegate to sqlx's `PgHstore`. MySQL / SQLite: total stubs so
// the shared `FromRow` body compiles — `Type` borrows a placeholder
// type-info, `Decode` errors at runtime. Encode is intentionally NOT
// implemented: the bind path goes through `SqlValue::HStore`.

#[cfg(feature = "postgres")]
impl<'r> sqlx::Decode<'r, sqlx::Postgres> for HStore {
    fn decode(
        value: <sqlx::Postgres as sqlx::Database>::ValueRef<'r>,
    ) -> Result<Self, sqlx::error::BoxDynError> {
        let pg =
            <sqlx::postgres::types::PgHstore as sqlx::Decode<'r, sqlx::Postgres>>::decode(value)?;
        Ok(Self(pg.0))
    }
}

#[cfg(feature = "postgres")]
impl sqlx::Type<sqlx::Postgres> for HStore {
    fn type_info() -> sqlx::postgres::PgTypeInfo {
        <sqlx::postgres::types::PgHstore as sqlx::Type<sqlx::Postgres>>::type_info()
    }

    fn compatible(ty: &sqlx::postgres::PgTypeInfo) -> bool {
        <sqlx::postgres::types::PgHstore as sqlx::Type<sqlx::Postgres>>::compatible(ty)
    }
}

#[cfg(feature = "mysql")]
impl<'r> sqlx::Decode<'r, sqlx::MySql> for HStore {
    fn decode(
        _value: <sqlx::MySql as sqlx::Database>::ValueRef<'r>,
    ) -> Result<Self, sqlx::error::BoxDynError> {
        Err("`HStore` columns are PostgreSQL-only; cannot decode on MySQL (issue #342)".into())
    }
}

#[cfg(feature = "mysql")]
impl sqlx::Type<sqlx::MySql> for HStore {
    fn type_info() -> sqlx::mysql::MySqlTypeInfo {
        <String as sqlx::Type<sqlx::MySql>>::type_info()
    }
}

#[cfg(feature = "sqlite")]
impl<'r> sqlx::Decode<'r, sqlx::Sqlite> for HStore {
    fn decode(
        _value: <sqlx::Sqlite as sqlx::Database>::ValueRef<'r>,
    ) -> Result<Self, sqlx::error::BoxDynError> {
        Err("`HStore` columns are PostgreSQL-only; cannot decode on SQLite (issue #342)".into())
    }
}

#[cfg(feature = "sqlite")]
impl sqlx::Type<sqlx::Sqlite> for HStore {
    fn type_info() -> sqlx::sqlite::SqliteTypeInfo {
        <String as sqlx::Type<sqlx::Sqlite>>::type_info()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::SqlValue;

    #[test]
    fn from_iter_and_deref() {
        let h = HStore::from_iter([("color", "red"), ("size", "L")]);
        assert_eq!(h.len(), 2);
        assert_eq!(h.get("color"), Some(&Some("red".to_owned())));
    }

    #[test]
    fn into_sqlvalue_hstore() {
        let v: SqlValue = HStore::from_iter([("k", "v")]).into();
        match v {
            SqlValue::HStore(pairs) => {
                assert_eq!(pairs, vec![("k".to_owned(), Some("v".to_owned()))]);
            }
            other => panic!("expected SqlValue::HStore, got {other:?}"),
        }
    }

    #[test]
    fn serde_round_trips_as_object() {
        let h = HStore::from_iter([("a", "1")]);
        let json = serde_json::to_string(&h).unwrap();
        assert_eq!(json, r#"{"a":"1"}"#);
        let back: HStore = serde_json::from_str(&json).unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn null_value_round_trips_through_serde() {
        let mut h = HStore::new();
        h.insert("present".to_owned(), Some("yes".to_owned()));
        h.insert("absent".to_owned(), None);
        let json = serde_json::to_string(&h).unwrap();
        let back: HStore = serde_json::from_str(&json).unwrap();
        assert_eq!(back, h);
    }
}
