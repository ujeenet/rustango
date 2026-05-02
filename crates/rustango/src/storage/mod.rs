//! File storage backends — upload, retrieve, delete files via a pluggable trait.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::storage::{Storage, LocalStorage};
//! use std::sync::Arc;
//! use std::path::PathBuf;
//!
//! let storage: Arc<dyn Storage> = Arc::new(LocalStorage::new(PathBuf::from("./uploads")));
//!
//! // Save a file
//! storage.save("avatars/alice.png", &png_bytes).await?;
//!
//! // Read it back
//! let bytes = storage.load("avatars/alice.png").await?;
//!
//! // Generate a public URL (when configured)
//! let url = storage.url("avatars/alice.png");
//! ```
//!
//! ## Backends
//!
//! | Backend | When to use |
//! |---------|-------------|
//! | [`LocalStorage`] | Single-server deployments — files on local disk |
//! | [`InMemoryStorage`] | Tests — files in a `HashMap`, never touch disk |
//! | S3 / GCS / Azure Blob | Plug your own — implement `Storage` for the SDK of your choice |

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

// ------------------------------------------------------------------ Errors

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("file not found: {0}")]
    NotFound(String),
    #[error("invalid path: {0}")]
    InvalidPath(String),
    #[error("io error: {0}")]
    Io(String),
}

// ------------------------------------------------------------------ Storage trait

/// Pluggable async file storage backend.
///
/// `key` is a logical path within the storage root (e.g. `"avatars/alice.png"`).
/// Backends MUST reject keys containing `..`, leading `/`, or null bytes.
#[async_trait]
pub trait Storage: Send + Sync + 'static {
    /// Write `data` to `key`, overwriting any existing file at that path.
    async fn save(&self, key: &str, data: &[u8]) -> Result<(), StorageError>;

    /// Read the bytes at `key`. Returns [`StorageError::NotFound`] if absent.
    async fn load(&self, key: &str) -> Result<Vec<u8>, StorageError>;

    /// Remove the file at `key`. No-op if absent.
    async fn delete(&self, key: &str) -> Result<(), StorageError>;

    /// `true` if a file exists at `key`.
    async fn exists(&self, key: &str) -> Result<bool, StorageError>;

    /// Return a URL where the file at `key` can be served, if the backend
    /// supports public URLs. Returns `None` for backends that don't (e.g. tests).
    fn url(&self, key: &str) -> Option<String>;
}

/// `Arc<dyn Storage>` alias.
pub type BoxedStorage = Arc<dyn Storage>;

/// Reject keys with `..`, leading `/`, or null bytes — defends against
/// path traversal in `LocalStorage`. Backends that store keys verbatim
/// (e.g. S3) should still call this to keep keys consistent.
pub fn validate_key(key: &str) -> Result<(), StorageError> {
    if key.is_empty() {
        return Err(StorageError::InvalidPath("empty key".into()));
    }
    if key.starts_with('/') {
        return Err(StorageError::InvalidPath(format!("key must be relative: {key}")));
    }
    if key.contains("..") {
        return Err(StorageError::InvalidPath(format!("key contains `..`: {key}")));
    }
    if key.contains('\0') {
        return Err(StorageError::InvalidPath(format!("key contains null byte")));
    }
    Ok(())
}

// ------------------------------------------------------------------ LocalStorage

/// Filesystem-backed storage rooted at a directory.
///
/// All keys are joined onto `root` as relative paths. The validator
/// rejects path-traversal keys (`..`, leading `/`).
pub struct LocalStorage {
    root: PathBuf,
    base_url: Option<String>,
}

impl LocalStorage {
    /// Create a local storage rooted at `root`. The directory is created
    /// on first save if it doesn't exist.
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root, base_url: None }
    }

    /// Set the public URL prefix for files served from this storage. The
    /// resulting URL for key `k` is `{base_url}/{k}`.
    #[must_use]
    pub fn with_base_url(mut self, base: impl Into<String>) -> Self {
        self.base_url = Some(base.into());
        self
    }

    fn full_path(&self, key: &str) -> PathBuf {
        self.root.join(key)
    }
}

#[async_trait]
impl Storage for LocalStorage {
    async fn save(&self, key: &str, data: &[u8]) -> Result<(), StorageError> {
        validate_key(key)?;
        let path = self.full_path(key);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| StorageError::Io(e.to_string()))?;
        }
        tokio::fs::write(&path, data)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))
    }

    async fn load(&self, key: &str) -> Result<Vec<u8>, StorageError> {
        validate_key(key)?;
        let path = self.full_path(key);
        match tokio::fs::read(&path).await {
            Ok(bytes) => Ok(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(StorageError::NotFound(key.to_owned()))
            }
            Err(e) => Err(StorageError::Io(e.to_string())),
        }
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        validate_key(key)?;
        let path = self.full_path(key);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(StorageError::Io(e.to_string())),
        }
    }

    async fn exists(&self, key: &str) -> Result<bool, StorageError> {
        validate_key(key)?;
        Ok(tokio::fs::try_exists(self.full_path(key))
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?)
    }

    fn url(&self, key: &str) -> Option<String> {
        let base = self.base_url.as_ref()?;
        Some(format!("{}/{}", base.trim_end_matches('/'), key))
    }
}

// ------------------------------------------------------------------ InMemoryStorage

/// `HashMap<String, Vec<u8>>` storage — for tests. Never touches disk.
#[derive(Default)]
pub struct InMemoryStorage {
    files: Mutex<HashMap<String, Vec<u8>>>,
}

impl InMemoryStorage {
    #[must_use]
    pub fn new() -> Self {
        Self { files: Mutex::new(HashMap::new()) }
    }
}

#[async_trait]
impl Storage for InMemoryStorage {
    async fn save(&self, key: &str, data: &[u8]) -> Result<(), StorageError> {
        validate_key(key)?;
        self.files
            .lock()
            .expect("storage mutex poisoned")
            .insert(key.to_owned(), data.to_vec());
        Ok(())
    }

    async fn load(&self, key: &str) -> Result<Vec<u8>, StorageError> {
        validate_key(key)?;
        self.files
            .lock()
            .expect("storage mutex poisoned")
            .get(key)
            .cloned()
            .ok_or_else(|| StorageError::NotFound(key.to_owned()))
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        validate_key(key)?;
        self.files
            .lock()
            .expect("storage mutex poisoned")
            .remove(key);
        Ok(())
    }

    async fn exists(&self, key: &str) -> Result<bool, StorageError> {
        validate_key(key)?;
        Ok(self
            .files
            .lock()
            .expect("storage mutex poisoned")
            .contains_key(key))
    }

    fn url(&self, _key: &str) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_empty() {
        assert!(matches!(validate_key(""), Err(StorageError::InvalidPath(_))));
    }

    #[test]
    fn validate_rejects_leading_slash() {
        assert!(matches!(validate_key("/etc/passwd"), Err(StorageError::InvalidPath(_))));
    }

    #[test]
    fn validate_rejects_dotdot() {
        assert!(matches!(validate_key("../escape"), Err(StorageError::InvalidPath(_))));
        assert!(matches!(validate_key("safe/../bad"), Err(StorageError::InvalidPath(_))));
    }

    #[test]
    fn validate_accepts_normal_keys() {
        assert!(validate_key("avatars/alice.png").is_ok());
        assert!(validate_key("file.txt").is_ok());
    }

    #[tokio::test]
    async fn in_memory_save_and_load() {
        let s = InMemoryStorage::new();
        s.save("k", b"hello").await.unwrap();
        assert_eq!(s.load("k").await.unwrap(), b"hello".to_vec());
    }

    #[tokio::test]
    async fn in_memory_load_missing_is_not_found() {
        let s = InMemoryStorage::new();
        let err = s.load("nope").await.unwrap_err();
        assert!(matches!(err, StorageError::NotFound(_)));
    }

    #[tokio::test]
    async fn in_memory_delete_then_load_is_not_found() {
        let s = InMemoryStorage::new();
        s.save("k", b"data").await.unwrap();
        s.delete("k").await.unwrap();
        let err = s.load("k").await.unwrap_err();
        assert!(matches!(err, StorageError::NotFound(_)));
    }

    #[tokio::test]
    async fn in_memory_exists() {
        let s = InMemoryStorage::new();
        assert!(!s.exists("k").await.unwrap());
        s.save("k", b"d").await.unwrap();
        assert!(s.exists("k").await.unwrap());
    }

    #[tokio::test]
    async fn in_memory_save_validates_key() {
        let s = InMemoryStorage::new();
        let err = s.save("../nope", b"x").await.unwrap_err();
        assert!(matches!(err, StorageError::InvalidPath(_)));
    }

    #[tokio::test]
    async fn local_storage_save_and_load() {
        let dir = tempdir();
        let s = LocalStorage::new(dir.clone());
        s.save("hello.txt", b"world").await.unwrap();
        let bytes = s.load("hello.txt").await.unwrap();
        assert_eq!(bytes, b"world");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn local_storage_creates_subdirs() {
        let dir = tempdir();
        let s = LocalStorage::new(dir.clone());
        s.save("a/b/c/file.txt", b"deep").await.unwrap();
        assert_eq!(s.load("a/b/c/file.txt").await.unwrap(), b"deep");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn local_storage_url_with_base() {
        let dir = tempdir();
        let s = LocalStorage::new(dir.clone()).with_base_url("https://cdn.example.com/uploads");
        assert_eq!(
            s.url("avatars/alice.png").as_deref(),
            Some("https://cdn.example.com/uploads/avatars/alice.png"),
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let mut p = std::env::temp_dir();
        p.push(format!("rustango_storage_test_{pid}_{n}"));
        let _ = std::fs::remove_dir_all(&p);
        p
    }
}
