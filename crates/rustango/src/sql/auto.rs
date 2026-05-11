//! `Auto<T>` — primary-key wrapper for server-assigned values.
//!
//! `#[derive(Model)] struct User { #[rustango(primary_key)] id: Auto<i64>, … }`
//! lets the database fill in the value via `BIGSERIAL` (or `SERIAL` for
//! `Auto<i32>`). On INSERT, `Auto::Unset` is *omitted* from the column
//! list so Postgres' DEFAULT (nextval on the sequence) takes effect; the
//! value is then read back via `RETURNING` and stored on the model.
//!
//! `Auto::Set(v)` lets the user supply an explicit value when needed
//! (data fixtures, replication, idempotent re-inserts).
//!
//! Rust side, the type is just an enum with `Default = Unset` and
//! `From<T>` for ergonomics: `let mut u = User { id: Auto::default(), … };`
//! or `let mut u = User { id: 42_i64.into(), … };`.

/// Server-assigned primary key wrapper. See module docs.
///
/// JSON shape (REST-friendly, since v0.29): `Auto::Set(v)` serializes
/// as the bare inner value (`42`); `Auto::Unset` serializes as JSON
/// `null`. Mirrors how `ForeignKey<T, K>` lowers to its bare PK on
/// the wire. Deserialize is symmetric — accepts either the bare
/// value (`42` → `Set(42)`, `null` → `Unset`) OR the legacy
/// tagged-enum shape (`{"Set": 42}` / `"Unset"`) for callers that
/// emit it from older serializers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Auto<T> {
    /// "Let the database fill this in." Default state for new rows.
    Unset,
    /// Explicit value — either supplied by the user or returned by the
    /// database after INSERT.
    Set(T),
}

impl<T: serde::Serialize> serde::Serialize for Auto<T> {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Set(v) => v.serialize(ser),
            Self::Unset => ser.serialize_none(),
        }
    }
}

impl<'de, T> serde::Deserialize<'de> for Auto<T>
where
    T: serde::de::DeserializeOwned,
{
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        // Three accepted shapes (Auto::Unset is normalized from the
        // first two; the third is the v0.28 tagged-enum kept for
        // backwards compat with payloads minted before the wire
        // shape flipped):
        //   1. Bare value:         42                  -> Set(42)
        //   2. Bare null:          null                -> Unset
        //   3. Legacy tag enum:    {"Set": 42} | "Unset"
        //
        // We read the input into a `serde_json::Value` first, branch on
        // the shape, and then deserialize the matching variant. The
        // `DeserializeOwned` bound on `T` is required because
        // `serde_json::from_value` allocates owned data; that bound is
        // never restrictive in practice — `Auto<T>` is used for
        // server-assigned PKs (i32/i64/Uuid/DateTime) which are all
        // owned types.
        use serde::de::Error as _;
        let value = serde_json::Value::deserialize(de)?;
        match value {
            serde_json::Value::Null => Ok(Self::Unset),
            serde_json::Value::String(ref s) if s == "Unset" => Ok(Self::Unset),
            serde_json::Value::Object(ref map) if map.len() == 1 && map.contains_key("Set") => {
                let inner = map.get("Set").cloned().unwrap_or(serde_json::Value::Null);
                let v: T = serde_json::from_value(inner).map_err(D::Error::custom)?;
                Ok(Self::Set(v))
            }
            other => {
                let v: T = serde_json::from_value(other).map_err(D::Error::custom)?;
                Ok(Self::Set(v))
            }
        }
    }
}

impl<T> Default for Auto<T> {
    fn default() -> Self {
        Self::Unset
    }
}

impl<T> From<T> for Auto<T> {
    fn from(v: T) -> Self {
        Self::Set(v)
    }
}

impl<T> Auto<T> {
    /// Borrow the inner value if set.
    #[must_use]
    pub fn get(&self) -> Option<&T> {
        match self {
            Self::Set(v) => Some(v),
            Self::Unset => None,
        }
    }

    /// `true` if the database has not (yet) assigned a value.
    #[must_use]
    pub fn is_unset(&self) -> bool {
        matches!(self, Self::Unset)
    }

    /// `true` if a value is present (user-supplied or database-assigned).
    #[must_use]
    pub fn is_set(&self) -> bool {
        matches!(self, Self::Set(_))
    }

    /// Take the inner value, leaving `Unset` behind. Returns `None` if
    /// already unset.
    pub fn take(&mut self) -> Option<T> {
        match std::mem::replace(self, Self::Unset) {
            Self::Set(v) => Some(v),
            Self::Unset => None,
        }
    }

    /// Consume and return the inner value if set.
    #[must_use]
    pub fn into_inner(self) -> Option<T> {
        match self {
            Self::Set(v) => Some(v),
            Self::Unset => None,
        }
    }
}

/// `Auto::Set(v)` carries the inner value into the SQL parameter slot;
/// `Auto::Unset` becomes SQL `NULL`. The macro-generated INSERT path
/// **also** drops `Unset` columns from the column list so Postgres'
/// DEFAULT (sequence) fires; the `Null` here is the safe fallback for
/// any context that didn't get that special-case treatment.
impl<T> From<Auto<T>> for crate::core::SqlValue
where
    T: Into<crate::core::SqlValue>,
{
    fn from(a: Auto<T>) -> Self {
        match a {
            Auto::Unset => Self::Null,
            Auto::Set(v) => v.into(),
        }
    }
}

/// Decoded `Auto<T>` is always `Set` — the database always returns a
/// concrete value when reading rows. Per-backend `sqlx::Decode` /
/// `sqlx::Type` impls are required by sqlx's row-decode machinery
/// (no Pool-enum equivalent exists at the trait level); parallel
/// MySQL + SQLite impls below.
#[cfg(feature = "postgres")]
impl<'r, T> sqlx::Decode<'r, sqlx::Postgres> for Auto<T>
where
    T: sqlx::Decode<'r, sqlx::Postgres>,
{
    fn decode(
        value: <sqlx::Postgres as sqlx::Database>::ValueRef<'r>,
    ) -> Result<Self, sqlx::error::BoxDynError> {
        Ok(Self::Set(T::decode(value)?))
    }
}

/// `Auto<T>` carries the same Postgres type as `T`. (`BIGSERIAL` on the
/// DDL side resolves to `BIGINT`; reading it as `i64` is correct.)
#[cfg(feature = "postgres")]
impl<T> sqlx::Type<sqlx::Postgres> for Auto<T>
where
    T: sqlx::Type<sqlx::Postgres>,
{
    fn type_info() -> sqlx::postgres::PgTypeInfo {
        T::type_info()
    }

    fn compatible(ty: &sqlx::postgres::PgTypeInfo) -> bool {
        T::compatible(ty)
    }
}

// ---- MySQL Decode/Type for Auto<T> (v0.23.0-batch22) ----
//
// Mirror of the Postgres impls above so `#[derive(Model)]` types
// with `Auto<T>` PKs satisfy `FromRow<MySqlRow>` (the macro-emitted
// `try_get::<Auto<i64>, _>(row, "id")` requires
// `Auto<i64>: Decode<MySql> + Type<MySql>`). MySQL's
// `BIGINT AUTO_INCREMENT` resolves to `BIGINT`, same as PG.

#[cfg(feature = "mysql")]
impl<'r, T> sqlx::Decode<'r, sqlx::MySql> for Auto<T>
where
    T: sqlx::Decode<'r, sqlx::MySql>,
{
    fn decode(
        value: <sqlx::MySql as sqlx::Database>::ValueRef<'r>,
    ) -> Result<Self, sqlx::error::BoxDynError> {
        Ok(Self::Set(T::decode(value)?))
    }
}

#[cfg(feature = "mysql")]
impl<T> sqlx::Type<sqlx::MySql> for Auto<T>
where
    T: sqlx::Type<sqlx::MySql>,
{
    fn type_info() -> sqlx::mysql::MySqlTypeInfo {
        T::type_info()
    }

    fn compatible(ty: &sqlx::mysql::MySqlTypeInfo) -> bool {
        T::compatible(ty)
    }
}

// ---- SQLite Decode/Type for Auto<T> (v0.27 Phase 3) ----
//
// Mirror of the Postgres + MySQL impls above so `#[derive(Model)]`
// types with `Auto<T>` PKs satisfy `FromRow<SqliteRow>`.
// `INTEGER PRIMARY KEY AUTOINCREMENT` resolves to `INTEGER`, which
// sqlx-sqlite decodes as `i64`.

#[cfg(feature = "sqlite")]
impl<'r, T> sqlx::Decode<'r, sqlx::Sqlite> for Auto<T>
where
    T: sqlx::Decode<'r, sqlx::Sqlite>,
{
    fn decode(
        value: <sqlx::Sqlite as sqlx::Database>::ValueRef<'r>,
    ) -> Result<Self, sqlx::error::BoxDynError> {
        Ok(Self::Set(T::decode(value)?))
    }
}

#[cfg(feature = "sqlite")]
impl<T> sqlx::Type<sqlx::Sqlite> for Auto<T>
where
    T: sqlx::Type<sqlx::Sqlite>,
{
    fn type_info() -> sqlx::sqlite::SqliteTypeInfo {
        T::type_info()
    }

    fn compatible(ty: &sqlx::sqlite::SqliteTypeInfo) -> bool {
        T::compatible(ty)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_unset() {
        let a: Auto<i64> = Auto::default();
        assert!(a.is_unset());
        assert!(!a.is_set());
        assert!(a.get().is_none());
    }

    #[test]
    fn from_t_is_set() {
        let a: Auto<i64> = 42_i64.into();
        assert!(a.is_set());
        assert_eq!(a.get(), Some(&42));
    }

    #[test]
    fn into_inner_returns_value_or_none() {
        assert_eq!(Auto::Set(7_i64).into_inner(), Some(7));
        assert_eq!(Auto::<i64>::Unset.into_inner(), None);
    }

    #[test]
    fn take_leaves_unset_behind() {
        let mut a: Auto<i64> = 99_i64.into();
        let v = a.take();
        assert_eq!(v, Some(99));
        assert!(a.is_unset());
        // Second take is None.
        assert_eq!(a.take(), None);
    }

    #[test]
    fn serde_round_trip_set() {
        let a: Auto<i64> = Auto::Set(42);
        let json = serde_json::to_string(&a).unwrap();
        let back: Auto<i64> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, Auto::Set(42));
    }

    #[test]
    fn serde_round_trip_unset() {
        let a: Auto<i64> = Auto::Unset;
        let json = serde_json::to_string(&a).unwrap();
        let back: Auto<i64> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, Auto::Unset);
    }

    /// The wire shape since v0.29: bare value, no enum tag.
    #[test]
    fn serialize_set_emits_bare_value() {
        assert_eq!(serde_json::to_string(&Auto::Set(42_i64)).unwrap(), "42");
        assert_eq!(
            serde_json::to_string(&Auto::Set("hello".to_owned())).unwrap(),
            "\"hello\""
        );
    }

    #[test]
    fn serialize_unset_emits_null() {
        assert_eq!(serde_json::to_string(&Auto::<i64>::Unset).unwrap(), "null");
    }

    /// Bare values + null deserialize directly.
    #[test]
    fn deserialize_accepts_bare_shape() {
        assert_eq!(
            serde_json::from_str::<Auto<i64>>("42").unwrap(),
            Auto::Set(42)
        );
        assert_eq!(
            serde_json::from_str::<Auto<i64>>("null").unwrap(),
            Auto::Unset
        );
    }

    /// Backwards compat: the v0.28 tagged-enum shape still parses.
    #[test]
    fn deserialize_accepts_legacy_tagged_enum() {
        assert_eq!(
            serde_json::from_str::<Auto<i64>>("\"Unset\"").unwrap(),
            Auto::Unset
        );
        assert_eq!(
            serde_json::from_str::<Auto<i64>>(r#"{"Set": 42}"#).unwrap(),
            Auto::Set(42)
        );
    }

    #[test]
    fn within_a_struct_round_trip_is_clean() {
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Row {
            id: Auto<i64>,
            name: String,
        }
        let row = Row {
            id: Auto::Set(7),
            name: "x".to_owned(),
        };
        let json = serde_json::to_string(&row).unwrap();
        assert_eq!(json, r#"{"id":7,"name":"x"}"#);
        let back: Row = serde_json::from_str(&json).unwrap();
        assert_eq!(back, row);
    }
}
