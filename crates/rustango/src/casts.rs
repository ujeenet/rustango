//! Transform a field on its way to and from the database, like
//! Eloquent's `$casts` or Django's `from_db_value`.
//!
//! A field typed [`Cast<C>`](crate::casts::Cast) holds the value you work with in Rust,
//! but stores it through the [`CastValue`] impl `C`: `to_db` when
//! writing, `from_db` when reading. The column is plain `TEXT`, so
//! casts work on every backend and need no migration.
//!
//! ```ignore
//! use rustango::casts::{Cast, EncryptedString};
//!
//! #[derive(Model)]
//! #[rustango(table = "patient")]
//! struct Patient {
//!     #[rustango(primary_key)] id: Auto<i64>,
//!     // Stored AEAD-encrypted at rest; transparent `String` in Rust.
//!     ssn: Cast<EncryptedString>,
//! }
//!
//! let p = Patient { id: Auto::default(), ssn: Cast::new("123-45-6789".to_owned()) };
//! // `&*p.ssn` is the plaintext; the `ssn` column holds ciphertext.
//! ```
//!
//! ## Writing your own
//!
//! Implement [`CastValue`] on a marker type and use `Cast<MyCast>`.
//! `to_db` writes the value out as a `String`, `from_db` reads it
//! back and may fail. [`EncryptedString`] is a worked example.
//!
//! [`CastValue`]: crate::casts::CastValue
//! [`EncryptedString`]: crate::casts::EncryptedString

use std::fmt;
use std::marker::PhantomData;

/// [`CastValue::from_db`] could not read the stored value, for
/// example because the data is corrupt. sqlx reports it as a
/// `ColumnDecode` error.
#[derive(Debug)]
pub struct CastError(pub String);

impl fmt::Display for CastError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "attribute cast error: {}", self.0)
    }
}

impl std::error::Error for CastError {}

/// Converts a Rust value to and from the `TEXT` stored in the
/// database.
///
/// `to_db` cannot fail: a bad setup, such as a missing encryption
/// key, should panic early instead. `from_db` can fail, because
/// stored data may be corrupt. Implement this on a marker type and
/// use [`Cast<Self>`].
pub trait CastValue {
    /// The type the model field holds in memory.
    type Value;
    /// Turn the value into the stored `TEXT`.
    fn to_db(value: &Self::Value) -> String;
    /// Read the stored `TEXT` back into a value.
    ///
    /// # Errors
    /// [`CastError`] when the stored form cannot be decoded.
    fn from_db(stored: &str) -> Result<Self::Value, CastError>;
}

/// A model field stored through the [`CastValue`] impl `C`. See the
/// [module docs](self).
pub struct Cast<C: CastValue> {
    value: C::Value,
    _marker: PhantomData<fn() -> C>,
}

impl<C: CastValue> Cast<C> {
    /// Wrap a value.
    pub fn new(value: C::Value) -> Self {
        Self {
            value,
            _marker: PhantomData,
        }
    }

    /// Unwrap and return the value.
    pub fn into_inner(self) -> C::Value {
        self.value
    }
}

impl<C: CastValue> std::ops::Deref for Cast<C> {
    type Target = C::Value;
    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl<C: CastValue> std::ops::DerefMut for Cast<C> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.value
    }
}

// No `From<C::Value> for Cast<C>`: `C::Value` could itself be
// `Cast<C>`, which clashes with std's `From<T> for T`. Use
// `Cast::new`.

impl<C: CastValue> fmt::Debug for Cast<C>
where
    C::Value: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Cast").field(&self.value).finish()
    }
}

impl<C: CastValue> Clone for Cast<C>
where
    C::Value: Clone,
{
    fn clone(&self) -> Self {
        Self::new(self.value.clone())
    }
}

impl<C: CastValue> PartialEq for Cast<C>
where
    C::Value: PartialEq,
{
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

// ---- serde: behave as the logical value ----

impl<C: CastValue> serde::Serialize for Cast<C>
where
    C::Value: serde::Serialize,
{
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.value.serialize(serializer)
    }
}

impl<'de, C: CastValue> serde::Deserialize<'de> for Cast<C>
where
    C::Value: serde::Deserialize<'de>,
{
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        C::Value::deserialize(deserializer).map(Self::new)
    }
}

// ---- `Cast<C>` → `SqlValue` (INSERT / UPDATE bind, as TEXT) ----

impl<C: CastValue> From<Cast<C>> for crate::core::SqlValue {
    fn from(c: Cast<C>) -> Self {
        crate::core::SqlValue::String(C::to_db(&c.value))
    }
}

// ---- sqlx Type + Decode (read path): the column is TEXT ----
//
// Decode a `String`, then run `C::from_db`. Type and `compatible`
// defer to `String`, so this is an ordinary text column everywhere.

macro_rules! cast_sqlx_impls {
    ($db:ty) => {
        impl<'r, C: CastValue> sqlx::Decode<'r, $db> for Cast<C>
        where
            String: sqlx::Decode<'r, $db>,
        {
            fn decode(
                value: <$db as sqlx::Database>::ValueRef<'r>,
            ) -> Result<Self, sqlx::error::BoxDynError> {
                let stored = <String as sqlx::Decode<'r, $db>>::decode(value)?;
                let v = C::from_db(&stored).map_err(|e| Box::new(e) as sqlx::error::BoxDynError)?;
                Ok(Self::new(v))
            }
        }

        impl<C: CastValue> sqlx::Type<$db> for Cast<C> {
            fn type_info() -> <$db as sqlx::Database>::TypeInfo {
                <String as sqlx::Type<$db>>::type_info()
            }

            fn compatible(ty: &<$db as sqlx::Database>::TypeInfo) -> bool {
                <String as sqlx::Type<$db>>::compatible(ty)
            }
        }
    };
}

#[cfg(feature = "postgres")]
cast_sqlx_impls!(sqlx::Postgres);
#[cfg(feature = "mysql")]
cast_sqlx_impls!(sqlx::MySql);
#[cfg(feature = "sqlite")]
cast_sqlx_impls!(sqlx::Sqlite);

// ====================================================================
// Built-in: EncryptedString (AEAD at rest)
// ====================================================================

mod encrypted;
pub use encrypted::EncryptedString;
