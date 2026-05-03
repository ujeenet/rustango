//! `FileField` ORM type — a `String` newtype that's also a typed
//! reference to a file in [`crate::storage::Storage`].
//!
//! Stored on the database as `TEXT` / `VARCHAR` (the storage key);
//! reads + writes are byte-equivalent to a plain `String` field, so
//! migrations don't need to change.
//!
//! ## Why a newtype?
//!
//! A plain `String` field works for storing the key, but the type
//! system can't tell which `String`s are file refs. With `FileField`:
//!
//! - The macro recognizes it (treats as `FieldType::String`).
//! - Methods `.url(&storage)`, `.load(&storage)`, `.delete(&storage)`
//!   ride along — no need to remember to pass keys through helpers.
//! - The `auto_cleanup` helper walks every model field at delete-time
//!   and removes the corresponding storage object — no orphaned files.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::file_field::FileField;
//!
//! #[derive(rustango::Model, Clone)]
//! #[rustango(table = "users")]
//! pub struct User {
//!     #[rustango(primary_key)]
//!     pub id: rustango::Auto<i64>,
//!     pub name: String,
//!     pub avatar: Option<FileField>,
//! }
//!
//! // Save:
//! let saved = uploads::save_uploads(mp, &cfg, &storage).await?;
//! user.avatar = Some(FileField::new(saved[0].key.clone()));
//! user.save(&pool).await?;
//!
//! // Read:
//! if let Some(avatar) = &user.avatar {
//!     let url = avatar.url(&storage);
//! }
//!
//! // Delete the row + the underlying file:
//! file_field::auto_cleanup_for(&storage, &[&user.avatar]).await;
//! user.delete(&pool).await?;
//! ```
//!
//! ## Wiring auto-cleanup as a `post_delete` signal
//!
//! For models you want to clean up automatically:
//!
//! ```ignore
//! use rustango::file_field::register_post_delete_cleanup;
//!
//! // Once at startup, per model:
//! register_post_delete_cleanup::<User>(storage.clone(),
//!     |u| vec![u.avatar.as_ref()]);
//! ```
//!
//! The closure picks out which `FileField`s on the instance to delete.
//! Failures are logged via tracing — they don't fail the model
//! delete (the row is gone by the time the post_delete fires).

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::storage::{BoxedStorage, StorageError};

/// Typed reference to a file in [`crate::storage::Storage`]. Stored
/// on the database as the storage key (a `String`).
///
/// Cheap to clone (it's just a `String`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FileField(pub String);

impl FileField {
    /// New file field with the given storage key.
    #[must_use]
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    /// `true` for the empty key (default value — a model with this
    /// field unset).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Borrow the storage key.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.0
    }

    /// Convenience: ask the storage backend for a public URL.
    /// Returns `None` for empty fields and for backends that don't
    /// expose URLs.
    #[must_use]
    pub fn url(&self, storage: &BoxedStorage) -> Option<String> {
        if self.0.is_empty() {
            None
        } else {
            storage.url(&self.0)
        }
    }

    /// Read the bytes from storage.
    ///
    /// # Errors
    /// [`StorageError::NotFound`] when the file is missing;
    /// [`StorageError::Io`] for transport issues.
    pub async fn load(&self, storage: &BoxedStorage) -> Result<Vec<u8>, StorageError> {
        if self.0.is_empty() {
            return Err(StorageError::NotFound("FileField is empty".into()));
        }
        storage.load(&self.0).await
    }

    /// Delete the file from storage. No-op for empty fields.
    ///
    /// # Errors
    /// [`StorageError::Io`] for transport issues. Missing keys are
    /// not errors (matches Storage trait semantics).
    pub async fn delete(&self, storage: &BoxedStorage) -> Result<(), StorageError> {
        if self.0.is_empty() {
            return Ok(());
        }
        storage.delete(&self.0).await
    }

    /// Save bytes to a fresh storage key, returning a `FileField`
    /// pointing at them. Convenience for the common
    /// "upload + assign" handler pattern.
    ///
    /// # Errors
    /// Underlying storage error.
    pub async fn save(
        storage: &BoxedStorage,
        key: impl Into<String>,
        bytes: &[u8],
    ) -> Result<Self, StorageError> {
        let key = key.into();
        storage.save(&key, bytes).await?;
        Ok(Self(key))
    }
}

impl fmt::Display for FileField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for FileField {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl From<String> for FileField {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for FileField {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl From<FileField> for String {
    fn from(f: FileField) -> Self {
        f.0
    }
}

// =====================================================================
// sqlx integration — encode/decode as TEXT
// =====================================================================

#[cfg(feature = "postgres")]
mod sqlx_impl {
    use super::FileField;
    use sqlx::error::BoxDynError;
    use sqlx::postgres::{PgArgumentBuffer, PgTypeInfo, PgValueRef, Postgres};
    use sqlx::{Decode, Encode, Type};

    impl Type<Postgres> for FileField {
        fn type_info() -> PgTypeInfo {
            <String as Type<Postgres>>::type_info()
        }
    }

    impl<'q> Encode<'q, Postgres> for FileField {
        fn encode_by_ref(
            &self,
            buf: &mut PgArgumentBuffer,
        ) -> Result<sqlx::encode::IsNull, BoxDynError> {
            <String as Encode<Postgres>>::encode_by_ref(&self.0, buf)
        }
    }

    impl<'r> Decode<'r, Postgres> for FileField {
        fn decode(value: PgValueRef<'r>) -> Result<Self, BoxDynError> {
            let s = <String as Decode<Postgres>>::decode(value)?;
            Ok(FileField(s))
        }
    }
}

// =====================================================================
// auto_cleanup — delete-time orphan removal
// =====================================================================

/// Delete all non-empty file fields in `fields` from `storage`.
/// Failures are logged via tracing but never propagated — by the
/// time you'd call this from a `post_delete` signal, the database
/// row is already gone, so a partial cleanup is the best the system
/// can do anyway.
pub async fn auto_cleanup_for(storage: &BoxedStorage, fields: &[Option<&FileField>]) {
    for f in fields.iter().flatten() {
        if f.is_empty() {
            continue;
        }
        if let Err(e) = storage.delete(&f.0).await {
            tracing::warn!(key = %f.0, error = %e, "FileField cleanup failed");
        }
    }
}

/// Wire a [`signals::post_delete`](crate::signals) receiver that
/// extracts file fields from the deleted instance and removes them
/// from `storage`. Call once at app startup per model.
///
/// `extract` returns whatever file fields the model owns (use
/// `Vec::new()` to skip cleanup for an instance — useful when the
/// caller is moving the file rather than discarding it).
///
/// `T` must implement [`crate::core::Model`] + `Clone` (the signals
/// machinery's contract).
#[cfg(feature = "signals")]
pub fn register_post_delete_cleanup<T, F>(
    storage: BoxedStorage,
    extract: F,
) -> crate::signals::ReceiverId
where
    T: crate::core::Model + Clone + Send + Sync + 'static,
    F: Fn(&T) -> Vec<Option<FileField>> + Send + Sync + 'static,
{
    use std::sync::Arc;
    let extract = Arc::new(extract);
    crate::signals::connect_post_delete::<T, _, _>(move |model: Arc<T>| {
        let storage = storage.clone();
        let extract = extract.clone();
        Box::pin(async move {
            let fields = extract(&model);
            let refs: Vec<Option<&FileField>> = fields.iter().map(Option::as_ref).collect();
            auto_cleanup_for(&storage, &refs).await;
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::InMemoryStorage;
    use std::sync::Arc as StdArc;

    fn storage() -> BoxedStorage {
        StdArc::new(InMemoryStorage::new())
    }

    // -------- FileField API

    #[test]
    fn new_and_default_have_expected_keys() {
        let f = FileField::new("avatars/alice.png");
        assert_eq!(f.key(), "avatars/alice.png");
        assert!(!f.is_empty());

        let d = FileField::default();
        assert!(d.is_empty());
    }

    #[test]
    fn from_string_and_str_round_trip() {
        let from_string: FileField = String::from("a/b").into();
        assert_eq!(from_string.key(), "a/b");

        let from_str: FileField = "x/y".into();
        assert_eq!(from_str.key(), "x/y");

        let back: String = from_str.into();
        assert_eq!(back, "x/y");
    }

    #[test]
    fn display_renders_the_key() {
        let f = FileField::new("k");
        assert_eq!(f.to_string(), "k");
    }

    #[test]
    fn serde_serializes_transparently_as_string() {
        let f = FileField::new("a/b");
        let json = serde_json::to_string(&f).unwrap();
        assert_eq!(json, "\"a/b\"");
        let back: FileField = serde_json::from_str(&json).unwrap();
        assert_eq!(back, f);
    }

    #[tokio::test]
    async fn url_returns_none_for_empty_field() {
        let s = storage();
        let f = FileField::default();
        assert!(f.url(&s).is_none());
    }

    #[tokio::test]
    async fn save_round_trip_via_storage() {
        let s = storage();
        let f = FileField::save(&s, "uploads/x.bin", b"hello").await.unwrap();
        assert_eq!(f.key(), "uploads/x.bin");
        let bytes = f.load(&s).await.unwrap();
        assert_eq!(&bytes, b"hello");
    }

    #[tokio::test]
    async fn load_empty_field_returns_not_found() {
        let s = storage();
        let f = FileField::default();
        let err = f.load(&s).await.unwrap_err();
        assert!(matches!(err, StorageError::NotFound(_)));
    }

    #[tokio::test]
    async fn delete_empty_field_is_noop() {
        let s = storage();
        let f = FileField::default();
        f.delete(&s).await.unwrap();
    }

    #[tokio::test]
    async fn delete_removes_underlying_file() {
        let s = storage();
        s.save("k", b"data").await.unwrap();
        let f = FileField::new("k");
        assert!(s.exists("k").await.unwrap());
        f.delete(&s).await.unwrap();
        assert!(!s.exists("k").await.unwrap());
    }

    // -------- auto_cleanup_for

    #[tokio::test]
    async fn auto_cleanup_removes_each_non_empty_field() {
        let s = storage();
        for k in ["a/1", "a/2", "a/3"] {
            s.save(k, b"x").await.unwrap();
        }
        let f1 = FileField::new("a/1");
        let f2 = FileField::new("a/2");
        let f3 = FileField::new("a/3");
        auto_cleanup_for(&s, &[Some(&f1), Some(&f2), Some(&f3)]).await;
        for k in ["a/1", "a/2", "a/3"] {
            assert!(!s.exists(k).await.unwrap(), "key {k} should be gone");
        }
    }

    #[tokio::test]
    async fn auto_cleanup_skips_none_and_empty_entries() {
        let s = storage();
        s.save("a/1", b"x").await.unwrap();
        let present = FileField::new("a/1");
        let empty = FileField::default();
        auto_cleanup_for(&s, &[Some(&present), Some(&empty), None]).await;
        assert!(!s.exists("a/1").await.unwrap());
    }

    #[tokio::test]
    async fn auto_cleanup_swallows_errors_does_not_panic() {
        // InMemoryStorage::delete always succeeds, so we test the
        // contract by passing a non-existent key — the trait's
        // semantic is "delete missing key is a no-op", so this is
        // fine; the goal is "doesn't panic" which is checked by
        // reaching the assert.
        let s = storage();
        let f = FileField::new("does/not/exist");
        auto_cleanup_for(&s, &[Some(&f)]).await;
    }

    // post_delete signal wiring is exercised by integration tests
    // against a real `#[derive(Model)]` struct (see
    // tests/file_field_signal.rs once we have a derived test fixture).
    // The pure auto_cleanup_for tests above prove the cleanup logic;
    // register_post_delete_cleanup is just sugar over connect_post_delete
    // (which has its own coverage in signals/mod.rs).
}
