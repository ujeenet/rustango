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

use serde::{Deserialize, Serialize};

/// Server-assigned primary key wrapper. See module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Auto<T> {
    /// "Let the database fill this in." Default state for new rows.
    Unset,
    /// Explicit value — either supplied by the user or returned by the
    /// database after INSERT.
    Set(T),
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
impl<T> From<Auto<T>> for rustango_core::SqlValue
where
    T: Into<rustango_core::SqlValue>,
{
    fn from(a: Auto<T>) -> Self {
        match a {
            Auto::Unset => Self::Null,
            Auto::Set(v) => v.into(),
        }
    }
}

/// Decoded `Auto<T>` is always `Set` — the database always returns a
/// concrete value when reading rows.
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
}
