//! `ForeignKey<T, K>` — lazy-loaded parent reference, generic over
//! the parent's primary-key type `K` (defaults to `i64`).
//!
//! `#[derive(Model)] struct Post { author: ForeignKey<User>, … }` —
//! the same shape that's been working since v0.7. The generic `K`
//! parameter (default `i64`) lets the same wrapper carry non-integer
//! PKs:
//!
//! ```ignore
//! #[derive(Model)]
//! struct Comment {
//!     #[rustango(primary_key)] id: Auto<i64>,
//!     // String FK to users.user_uuid (parent's PK is String)
//!     user_uuid: ForeignKey<User, String>,
//!     body: String,
//! }
//! ```
//!
//! State machine:
//!
//! * Just-fetched rows hold `ForeignKey::Unloaded(pk)` — sqlx's
//!   `Decode` impl reads the column and builds this variant.
//! * After the first `.get(&pool)`, the variant becomes
//!   `ForeignKey::Loaded { pk, value }` with the parent cached in a
//!   `Box<T>`. Subsequent `.get()` calls are zero-cost (no SQL).
//!
//! On the write path, `ForeignKey<T, K>` lowers to `K`'s `SqlValue`
//! variant regardless of state — matches the column's declared type.
//!
//! Supported `K` values:
//! `i32`, `i64`, `String`, `Uuid`, `chrono::DateTime<Utc>`,
//! `chrono::NaiveDate`, `bool`, `f32`, `f64`. Anything that implements
//! `Into<SqlValue> + Clone + sqlx::Decode + sqlx::Type` works.
//!
//! Limitations:
//! * Target's PK column defaults to `"id"` for the
//!   `Relation::Fk { on: … }` schema entry. Override with
//!   `#[rustango(on = "user_uuid")]` on the FK field.

use crate::core::SqlValue;
// PG-typed surfaces below import these. Pulled in only when the
// `postgres` feature is on; sqlite/mysql-only builds get just the
// tri-dialect `get_pool` entry point further down.
#[cfg(feature = "postgres")]
use crate::core::{Model, Op};
#[cfg(feature = "postgres")]
use crate::query::QuerySet;
#[cfg(feature = "postgres")]
use sqlx::postgres::{PgPool, PgRow};
#[cfg(feature = "postgres")]
use sqlx::FromRow;

#[cfg(feature = "postgres")]
use super::ExecError;

/// Lazy-loaded reference to a parent row. See module docs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ForeignKey<T, K = i64> {
    /// Just-deserialized state — only the PK is known.
    Unloaded(K),
    /// Resolved state — the parent row is cached on the field.
    Loaded {
        /// Carried alongside the value so writes can grab the PK
        /// without inspecting `T` (whose PK could be `Auto<…>` or a
        /// plain field).
        pk: K,
        value: Box<T>,
    },
}

impl<T, K> ForeignKey<T, K> {
    /// Construct from a known PK without loading. Equivalent to
    /// `pk.into()`.
    #[must_use]
    pub fn unloaded(pk: K) -> Self {
        Self::Unloaded(pk)
    }

    /// Construct from an already-loaded parent. Caller supplies the
    /// PK explicitly because `ForeignKey<T, K>` does not assume a
    /// fixed shape for `T`'s PK field.
    #[must_use]
    pub fn loaded(pk: K, value: T) -> Self {
        Self::Loaded {
            pk,
            value: Box::new(value),
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

    /// Borrow the PK regardless of state. Cheap, no-clone variant of
    /// [`Self::pk`] for callers that just want to peek.
    #[must_use]
    pub fn pk_ref(&self) -> &K {
        match self {
            Self::Unloaded(pk) | Self::Loaded { pk, .. } => pk,
        }
    }
}

impl<T, K: Clone> ForeignKey<T, K> {
    /// The PK regardless of state. Returns by clone — cheap for the
    /// integer PK types most apps use, and small allocation for
    /// `String`/`Uuid`. Use [`Self::pk_ref`] if you need a borrow.
    #[must_use]
    pub fn pk(&self) -> K {
        self.pk_ref().clone()
    }
}

/// Serialize as the PK regardless of loaded/unloaded state. Lets
/// audited models include FK columns in `audit(track = "...")` and
/// have the audit JSON record the parent's PK without forcing every
/// FK target to also derive `Serialize`.
impl<T, K: serde::Serialize> serde::Serialize for ForeignKey<T, K> {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        self.pk_ref().serialize(ser)
    }
}

impl<T, K> From<K> for ForeignKey<T, K> {
    fn from(pk: K) -> Self {
        Self::Unloaded(pk)
    }
}

/// Always lowers to the PK regardless of state. Saves & inserts of
/// the *child's* row only need the FK column value, not the loaded
/// parent object.
impl<T, K: Clone + Into<SqlValue>> From<ForeignKey<T, K>> for SqlValue {
    fn from(fk: ForeignKey<T, K>) -> Self {
        match fk {
            ForeignKey::Unloaded(k) | ForeignKey::Loaded { pk: k, .. } => k.into(),
        }
    }
}

/// `ForeignKey<T, K>` decodes from the underlying column type into
/// the `Unloaded` variant. The lazy-load happens later via `.get()`.
#[cfg(feature = "postgres")]
impl<'r, T, K> sqlx::Decode<'r, sqlx::Postgres> for ForeignKey<T, K>
where
    K: sqlx::Decode<'r, sqlx::Postgres>,
{
    fn decode(
        value: <sqlx::Postgres as sqlx::Database>::ValueRef<'r>,
    ) -> Result<Self, sqlx::error::BoxDynError> {
        Ok(Self::Unloaded(<K as sqlx::Decode<sqlx::Postgres>>::decode(
            value,
        )?))
    }
}

/// `ForeignKey<T, K>` claims `K`'s Postgres type. The DDL writer
/// emits whatever column type the FK field's declared type maps to,
/// so this matches by construction.
#[cfg(feature = "postgres")]
impl<T, K> sqlx::Type<sqlx::Postgres> for ForeignKey<T, K>
where
    K: sqlx::Type<sqlx::Postgres>,
{
    fn type_info() -> sqlx::postgres::PgTypeInfo {
        <K as sqlx::Type<sqlx::Postgres>>::type_info()
    }

    fn compatible(ty: &sqlx::postgres::PgTypeInfo) -> bool {
        <K as sqlx::Type<sqlx::Postgres>>::compatible(ty)
    }
}

/// MySQL Decode mirror for the bi-dialect path.
#[cfg(feature = "mysql")]
impl<'r, T, K> sqlx::Decode<'r, sqlx::MySql> for ForeignKey<T, K>
where
    K: sqlx::Decode<'r, sqlx::MySql>,
{
    fn decode(
        value: <sqlx::MySql as sqlx::Database>::ValueRef<'r>,
    ) -> Result<Self, sqlx::error::BoxDynError> {
        Ok(Self::Unloaded(<K as sqlx::Decode<sqlx::MySql>>::decode(
            value,
        )?))
    }
}

#[cfg(feature = "mysql")]
impl<T, K> sqlx::Type<sqlx::MySql> for ForeignKey<T, K>
where
    K: sqlx::Type<sqlx::MySql>,
{
    fn type_info() -> sqlx::mysql::MySqlTypeInfo {
        <K as sqlx::Type<sqlx::MySql>>::type_info()
    }

    fn compatible(ty: &sqlx::mysql::MySqlTypeInfo) -> bool {
        <K as sqlx::Type<sqlx::MySql>>::compatible(ty)
    }
}

/// SQLite Decode mirror for the bi-dialect path. Mirrors the
/// Postgres + MySQL impls so `#[derive(Model)]` types with
/// `ForeignKey<T>` fields satisfy `FromRow<SqliteRow>`.
#[cfg(feature = "sqlite")]
impl<'r, T, K> sqlx::Decode<'r, sqlx::Sqlite> for ForeignKey<T, K>
where
    K: sqlx::Decode<'r, sqlx::Sqlite>,
{
    fn decode(
        value: <sqlx::Sqlite as sqlx::Database>::ValueRef<'r>,
    ) -> Result<Self, sqlx::error::BoxDynError> {
        Ok(Self::Unloaded(<K as sqlx::Decode<sqlx::Sqlite>>::decode(
            value,
        )?))
    }
}

#[cfg(feature = "sqlite")]
impl<T, K> sqlx::Type<sqlx::Sqlite> for ForeignKey<T, K>
where
    K: sqlx::Type<sqlx::Sqlite>,
{
    fn type_info() -> sqlx::sqlite::SqliteTypeInfo {
        <K as sqlx::Type<sqlx::Sqlite>>::type_info()
    }

    fn compatible(ty: &sqlx::sqlite::SqliteTypeInfo) -> bool {
        <K as sqlx::Type<sqlx::Sqlite>>::compatible(ty)
    }
}

// ============================================================ PG-typed get / get_on

#[cfg(feature = "postgres")]
impl<T, K> ForeignKey<T, K>
where
    T: Model + for<'r> FromRow<'r, PgRow> + Send + Unpin + crate::sql::LoadRelated,
    K: Clone + Into<SqlValue> + Send + Sync + 'static,
{
    /// Resolve the parent row and cache it on the field. Subsequent
    /// calls return the cached reference without hitting the DB.
    ///
    /// PG-typed shim — for the tri-dialect entry point see
    /// [`Self::get_pool`].
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
    /// PG-only by design (`sqlx::Executor<Database = Postgres>`) —
    /// schema-mode tenancy is a Postgres feature.
    ///
    /// # Errors
    /// As [`Self::get`].
    pub async fn get_on<'c, E>(&mut self, executor: E) -> Result<&T, ExecError>
    where
        E: sqlx::Executor<'c, Database = sqlx::Postgres>,
    {
        if matches!(self, Self::Unloaded(_)) {
            let pk = self.pk_ref().clone();
            let pk_field = T::SCHEMA
                .primary_key()
                .ok_or(ExecError::MissingPrimaryKey {
                    table: T::SCHEMA.table,
                })?;
            let mut rows: Vec<T> = QuerySet::<T>::new()
                .filter_op(pk_field.column, Op::Eq, pk.clone())
                .fetch_on(executor)
                .await?;
            let value = rows.pop().ok_or_else(|| {
                // Render the missing PK using its `Into<SqlValue>` shape
                // so error messages stay readable for non-integer keys.
                let sv: SqlValue = pk.clone().into();
                ExecError::ForeignKeyTargetMissing {
                    table: T::SCHEMA.table,
                    pk: sv.to_display_string(),
                }
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

// ============================================================ tri-dialect get_pool

/// Tri-dialect FK resolution: works against any backend the
/// [`crate::sql::Pool`] enum carries. Bounds mirror
/// [`crate::sql::FetcherPool`] — every `#[derive(Model)]` target
/// satisfies them via the macro's per-backend `FromRow` + `LoadRelated`
/// emissions. Use this in new framework code so sqlite/mysql apps
/// can lazy-load FK targets without going through the PG-typed
/// [`Self::get`].
///
/// v0.35: gated on `postgres` while `FetcherPool` still requires
/// `FromRow<PgRow>` unconditionally. v0.35 slice 6 introduces a
/// `MaybePgFromRow` marker trait paralleling the existing
/// `MaybeMyFromRow` / `MaybeSqliteFromRow` so this bound becomes
/// universally satisfiable + the gate drops.
#[cfg(feature = "postgres")]
impl<T, K> ForeignKey<T, K>
where
    T: Model
        + for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow>
        + crate::sql::MaybeMyFromRow
        + crate::sql::MaybeSqliteFromRow
        + crate::sql::LoadRelated
        + crate::sql::MaybeMyLoadRelated
        + crate::sql::MaybeSqliteLoadRelated
        + Send
        + Unpin,
    K: Clone + Into<SqlValue> + Send + Sync + 'static,
{
    /// Resolve the parent row and cache it on the field. Backend-
    /// agnostic counterpart of [`Self::get`] — routes through
    /// [`crate::sql::FetcherPool::fetch_pool`].
    ///
    /// # Errors
    /// As [`Self::get`].
    pub async fn get_pool(&mut self, pool: &crate::sql::Pool) -> Result<&T, ExecError> {
        use crate::sql::FetcherPool as _;
        if matches!(self, Self::Unloaded(_)) {
            let pk = self.pk_ref().clone();
            let pk_field = T::SCHEMA
                .primary_key()
                .ok_or(ExecError::MissingPrimaryKey {
                    table: T::SCHEMA.table,
                })?;
            let mut rows: Vec<T> = QuerySet::<T>::new()
                .filter_op(pk_field.column, Op::Eq, pk.clone())
                .fetch_pool(pool)
                .await?;
            let value = rows.pop().ok_or_else(|| {
                let sv: SqlValue = pk.clone().into();
                ExecError::ForeignKeyTargetMissing {
                    table: T::SCHEMA.table,
                    pk: sv.to_display_string(),
                }
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
        let fk = ForeignKey::loaded(7_i64, "alice".to_string());
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
        let unloaded: ForeignKey<()> = ForeignKey::unloaded(1_i64);
        let loaded = ForeignKey::loaded(2_i64, ());
        assert!(matches!(SqlValue::from(unloaded), SqlValue::I64(1)));
        assert!(matches!(SqlValue::from(loaded), SqlValue::I64(2)));
    }

    #[test]
    fn into_value_consumes_when_loaded() {
        let loaded = ForeignKey::loaded(3_i64, 100_u32);
        assert_eq!(loaded.into_value(), Some(100));
        let unloaded: ForeignKey<u32> = ForeignKey::unloaded(4_i64);
        assert_eq!(unloaded.into_value(), None);
    }

    // ----- Non-i64 PK shapes -----

    #[test]
    fn string_pk_unloaded_round_trip() {
        let fk: ForeignKey<(), String> = ForeignKey::unloaded("alice-uuid".to_owned());
        assert_eq!(fk.pk_ref(), "alice-uuid");
        assert_eq!(fk.pk(), "alice-uuid");
        assert!(!fk.is_loaded());
    }

    #[test]
    fn string_pk_lowers_to_sqlvalue_string() {
        let fk: ForeignKey<(), String> = ForeignKey::unloaded("k".to_owned());
        match SqlValue::from(fk) {
            SqlValue::String(s) => assert_eq!(s, "k"),
            other => panic!("expected SqlValue::String, got {other:?}"),
        }
    }

    #[test]
    fn uuid_pk_round_trip() {
        let id = uuid::Uuid::nil();
        let fk: ForeignKey<(), uuid::Uuid> = ForeignKey::unloaded(id);
        assert_eq!(fk.pk(), id);
        match SqlValue::from(fk) {
            SqlValue::Uuid(u) => assert_eq!(u, id),
            other => panic!("expected SqlValue::Uuid, got {other:?}"),
        }
    }

    #[test]
    fn from_string_yields_unloaded() {
        let fk: ForeignKey<(), String> = "x".to_owned().into();
        assert!(matches!(fk, ForeignKey::Unloaded(ref s) if s == "x"));
    }
}
