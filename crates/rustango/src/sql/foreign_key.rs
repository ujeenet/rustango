//! `ForeignKey<T>` — lazy-loaded parent reference (v0.7 slice 3).
//!
//! `#[derive(Model)] struct Post { author: ForeignKey<User>, … }`
//! stores the parent's primary key in the column, just like the
//! older `#[rustango(fk = "users")] author_id: i64` form. The
//! difference is on the Rust side: the field type carries the
//! target model statically, so `post.author.get(&pool).await?`
//! resolves the parent row on demand without the caller juggling
//! `User::objects().filter(…)`.
//!
//! State machine:
//!
//! * Just-fetched rows hold `ForeignKey::Unloaded(pk)` — sqlx's
//!   `Decode` impl reads the `BIGINT` column and builds this variant.
//! * After the first `.get(&pool)`, the variant becomes
//!   `ForeignKey::Loaded { pk, value }` with the parent cached in a
//!   `Box<T>`. Subsequent `.get()` calls are zero-cost (no SQL).
//!
//! On the write path, `ForeignKey<T>` lowers to `SqlValue::I64(pk)`
//! regardless of state — matches the existing FK column shape.
//!
//! Limitations (v1):
//!
//! * Target's PK must be `i64` (or `Auto<i64>`). `i32` and other
//!   shapes are deferred until they're asked for.
//! * Target's PK column defaults to `"id"` for the
//!   `Relation::Fk { on: … }` schema entry. Override with
//!   `#[rustango(on = "user_uuid")]` on the FK field.

use crate::core::{Model, Op, SqlValue};
use crate::query::QuerySet;
use sqlx::postgres::{PgPool, PgRow};
use sqlx::FromRow;

use super::ExecError;

/// Lazy-loaded reference to a parent row. See module docs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ForeignKey<T> {
    /// Just-deserialized state — only the PK is known.
    Unloaded(i64),
    /// Resolved state — the parent row is cached on the field.
    Loaded {
        /// Carried alongside the value so writes can grab the PK
        /// without inspecting `T` (whose PK could be `Auto<i64>` or
        /// plain `i64`).
        pk: i64,
        value: Box<T>,
    },
}

impl<T> ForeignKey<T> {
    /// Construct from a known PK without loading. Equivalent to
    /// `pk.into()`.
    #[must_use]
    pub fn unloaded(pk: i64) -> Self {
        Self::Unloaded(pk)
    }

    /// Construct from an already-loaded parent. Caller supplies the
    /// PK explicitly because `ForeignKey<T>` does not assume a fixed
    /// shape for `T`'s PK field.
    #[must_use]
    pub fn loaded(pk: i64, value: T) -> Self {
        Self::Loaded {
            pk,
            value: Box::new(value),
        }
    }

    /// The PK regardless of state.
    #[must_use]
    pub fn pk(&self) -> i64 {
        match self {
            Self::Unloaded(pk) | Self::Loaded { pk, .. } => *pk,
        }
    }

    /// `true` once `.get()` (or `loaded()`) has populated the cache.
    #[must_use]
    pub fn is_loaded(&self) -> bool {
        matches!(self, Self::Loaded { .. })
    }

    /// Borrow the cached parent if loaded.
    #[must_use]
    pub fn value(&self) -> Option<&T> {
        match self {
            Self::Loaded { value, .. } => Some(value),
            Self::Unloaded(_) => None,
        }
    }

    /// Consume and yield the cached parent if loaded.
    #[must_use]
    pub fn into_value(self) -> Option<T> {
        match self {
            Self::Loaded { value, .. } => Some(*value),
            Self::Unloaded(_) => None,
        }
    }
}

impl<T> From<i64> for ForeignKey<T> {
    fn from(pk: i64) -> Self {
        Self::Unloaded(pk)
    }
}

/// Always lowers to the PK regardless of state. Saves & inserts of
/// the *parent's* row only need the FK column value, not the loaded
/// child object.
impl<T> From<ForeignKey<T>> for SqlValue {
    fn from(fk: ForeignKey<T>) -> Self {
        Self::I64(fk.pk())
    }
}

/// `ForeignKey<T>` decodes from a Postgres `BIGINT` column into the
/// `Unloaded` variant. The lazy-load happens later via `.get()`.
impl<'r, T> sqlx::Decode<'r, sqlx::Postgres> for ForeignKey<T> {
    fn decode(
        value: <sqlx::Postgres as sqlx::Database>::ValueRef<'r>,
    ) -> Result<Self, sqlx::error::BoxDynError> {
        Ok(Self::Unloaded(<i64 as sqlx::Decode<sqlx::Postgres>>::decode(value)?))
    }
}

/// `ForeignKey<T>` claims the same Postgres type as `i64`. The DDL
/// writer emits `BIGINT` for FK columns, so this matches.
impl<T> sqlx::Type<sqlx::Postgres> for ForeignKey<T> {
    fn type_info() -> sqlx::postgres::PgTypeInfo {
        <i64 as sqlx::Type<sqlx::Postgres>>::type_info()
    }

    fn compatible(ty: &sqlx::postgres::PgTypeInfo) -> bool {
        <i64 as sqlx::Type<sqlx::Postgres>>::compatible(ty)
    }
}

impl<T> ForeignKey<T>
where
    T: Model + for<'r> FromRow<'r, PgRow> + Send + Unpin,
{
    /// Resolve the parent row and cache it on the field. Subsequent
    /// calls return the cached reference without hitting the DB.
    ///
    /// # Errors
    /// * [`ExecError::ForeignKeyTargetMissing`] — no row in the
    ///   target table has the FK's PK. Usually means the parent was
    ///   deleted under a non-CASCADE constraint, or the FK was hand-
    ///   built with an out-of-band value.
    /// * [`ExecError::MissingPrimaryKey`] — the target model has no
    ///   `#[rustango(primary_key)]` field (programming error).
    /// * Any [`ExecError`] produced by the underlying [`Fetcher`].
    pub async fn get(&mut self, pool: &PgPool) -> Result<&T, ExecError> {
        self.get_on(pool).await
    }

    /// Like [`Self::get`] but accepts any sqlx executor — needed for
    /// tenant-scoped lookups, where the calling connection has the
    /// `search_path` already set and a fresh checkout from `&PgPool`
    /// would land in the wrong schema.
    ///
    /// # Errors
    /// As [`Self::get`].
    pub async fn get_on<'c, E>(&mut self, executor: E) -> Result<&T, ExecError>
    where
        E: sqlx::Executor<'c, Database = sqlx::Postgres>,
    {
        if matches!(self, Self::Unloaded(_)) {
            let pk = self.pk();
            let pk_field = T::SCHEMA
                .primary_key()
                .ok_or(ExecError::MissingPrimaryKey {
                    table: T::SCHEMA.table,
                })?;
            let mut rows: Vec<T> = QuerySet::<T>::new()
                .filter(pk_field.column, Op::Eq, pk)
                .fetch_on(executor)
                .await?;
            let value = rows
                .pop()
                .ok_or(ExecError::ForeignKeyTargetMissing {
                    table: T::SCHEMA.table,
                    pk,
                })?;
            *self = Self::Loaded {
                pk,
                value: Box::new(value),
            };
        }
        match self {
            Self::Loaded { value, .. } => Ok(value),
            Self::Unloaded(_) => unreachable!("just transitioned to Loaded above"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unloaded_constructor_and_pk_accessor() {
        let fk: ForeignKey<()> = ForeignKey::unloaded(42);
        assert_eq!(fk.pk(), 42);
        assert!(!fk.is_loaded());
        assert!(fk.value().is_none());
    }

    #[test]
    fn loaded_constructor_caches_value() {
        let fk = ForeignKey::loaded(7, "alice".to_string());
        assert_eq!(fk.pk(), 7);
        assert!(fk.is_loaded());
        assert_eq!(fk.value(), Some(&"alice".to_string()));
    }

    #[test]
    fn from_i64_yields_unloaded() {
        let fk: ForeignKey<()> = 99_i64.into();
        match fk {
            ForeignKey::Unloaded(pk) => assert_eq!(pk, 99),
            ForeignKey::Loaded { .. } => panic!("expected Unloaded"),
        }
    }

    #[test]
    fn into_sqlvalue_gives_i64_in_either_state() {
        let unloaded: ForeignKey<()> = ForeignKey::unloaded(1);
        let loaded = ForeignKey::loaded(2, ());
        assert!(matches!(SqlValue::from(unloaded), SqlValue::I64(1)));
        assert!(matches!(SqlValue::from(loaded), SqlValue::I64(2)));
    }

    #[test]
    fn into_value_consumes_when_loaded() {
        let loaded = ForeignKey::loaded(3, 100_u32);
        assert_eq!(loaded.into_value(), Some(100));
        let unloaded: ForeignKey<u32> = ForeignKey::unloaded(4);
        assert_eq!(unloaded.into_value(), None);
    }
}
