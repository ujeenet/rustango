//! Attribute casts — a per-field get/set transform layer (Eloquent
//! `$casts` / `CastsAttributes`; Django `from_db_value`/`get_prep_value`).
//! Issue #819.
//!
//! A model field typed [`Cast<C>`] stores its logical value `C::Value`
//! in memory but reads/writes the database through the [`CastValue`]
//! impl `C` — `to_db` on the way in, `from_db` on the way out. The
//! column itself is plain `TEXT`, so casts are backend-neutral and need
//! no migration changes.
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
//! ## Extension point
//!
//! Implement [`CastValue`] for your own marker type and use it as
//! `Cast<MyCast>`. `to_db` serializes the logical value to the stored
//! `String`; `from_db` parses it back (fallible). The
//! [`EncryptedString`] built-in is the reference implementation; a
//! JSON-value-object cast is one line of `serde_json`.

use std::fmt;
use std::marker::PhantomData;

/// Error returned by [`CastValue::from_db`] when stored data can't be
/// parsed back into the logical value (corrupt/tampered ciphertext,
/// malformed JSON, …). Surfaces through sqlx's decode path as a
/// `ColumnDecode` error.
#[derive(Debug)]
pub struct CastError(pub String);

impl fmt::Display for CastError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "attribute cast error: {}", self.0)
    }
}

impl std::error::Error for CastError {}

/// Bridges a logical Rust value to/from its stored `TEXT` representation.
///
/// `to_db` is infallible (a misconfiguration — e.g. a missing encryption
/// key — should fail fast); `from_db` is fallible (stored data may be
/// corrupt). Implement this on a marker type and use [`Cast<Self>`].
pub trait CastValue {
    /// Logical in-memory type (what the model field "is").
    type Value;
    /// Serialize the logical value to the stored `TEXT` form.
    fn to_db(value: &Self::Value) -> String;
    /// Parse the stored `TEXT` form back into the logical value.
    ///
    /// # Errors
    /// [`CastError`] when the stored form can't be decoded.
    fn from_db(stored: &str) -> Result<Self::Value, CastError>;
}

/// A model field whose database representation is transformed by the
/// [`CastValue`] impl `C`. See the [module docs](self).
pub struct Cast<C: CastValue> {
    value: C::Value,
    _marker: PhantomData<fn() -> C>,
}

impl<C: CastValue> Cast<C> {
    /// Wrap a logical value.
    pub fn new(value: C::Value) -> Self {
        Self {
            value,
            _marker: PhantomData,
        }
    }

    /// Consume the wrapper, returning the logical value.
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

// NB: no `impl From<C::Value> for Cast<C>` — `C::Value` is an
// unconstrained associated type that could equal `Cast<C>`, which
// collides with the std reflexive `From<T> for T`. Construct via
// [`Cast::new`].

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

// ---- sqlx Type + Decode (FromRow read path): the column is TEXT ----
//
// Decode the stored `String` then run `C::from_db`. Type/`compatible`
// delegate to `String`, so the column is an ordinary text column on
// every backend.

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
// Built-in: EncryptedString (AEAD-at-rest)
// ====================================================================

mod encrypted;
pub use encrypted::EncryptedString;
