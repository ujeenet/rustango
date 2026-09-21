//! File storage: save, read and delete files through one trait, with
//! a backend you choose.
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
//! | [`LocalStorage`] | One server: files on local disk |
//! | [`InMemoryStorage`] | Tests: files in a `HashMap`, never on disk |
//! | [`s3::S3Storage`] | S3, R2, B2, MinIO, or any S3-compatible API. Behind the `storage-s3` feature. |
//! | Anything else | Implement `Storage` over the SDK you want |
//!
//! They all share one trait, so you can pick a backend at startup
//! without changing the code that uses it.
//!
//! [`LocalStorage`]: crate::storage::LocalStorage
//! [`InMemoryStorage`]: crate::storage::InMemoryStorage
//! [`s3::S3Storage`]: crate::storage::s3::S3Storage

#[cfg(feature = "storage-s3")]
pub mod s3;

pub mod registry;
pub use registry::StorageRegistry;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Re-exported so implementing [`Storage`] needs no `async-trait`
/// entry in your own `Cargo.toml`, and cannot end up on a different
/// version from the one the trait was declared with.
pub use async_trait::async_trait;

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

/// An async file storage backend.
///
/// A `key` is a path inside the storage root, such as
/// `"avatars/alice.png"`. Every backend must reject a key with `..`,
/// a leading `/`, or a null byte. [`validate_key`] does that.
#[async_trait]
pub trait Storage: Send + Sync + 'static {
    /// Write `data` to `key`, replacing any file already there.
    async fn save(&self, key: &str, data: &[u8]) -> Result<(), StorageError>;

    /// Read the bytes at `key`, or [`StorageError::NotFound`].
    async fn load(&self, key: &str) -> Result<Vec<u8>, StorageError>;

    /// Delete the file at `key`. Does nothing if it is not there.
    async fn delete(&self, key: &str) -> Result<(), StorageError>;

    /// Whether a file exists at `key`.
    async fn exists(&self, key: &str) -> Result<bool, StorageError>;

    /// A public URL for `key`, or `None` on a backend that has none.
    fn url(&self, key: &str) -> Option<String>;

    /// A GET URL for `key` that expires, so a private file can be
    /// served without going through your handler. `ttl` is how long
    /// it stays valid; AWS caps that at 7 days, and shorter is safer.
    ///
    /// `None` on a backend that cannot sign, such as `LocalStorage`.
    /// `S3Storage` signs with SigV4.
    async fn presigned_get_url(&self, _key: &str, _ttl: std::time::Duration) -> Option<String> {
        None
    }

    /// A PUT URL that expires, so a browser can upload straight to
    /// the backend. With a `content_type`, the signature is tied to
    /// it and the browser must send a matching header.
    ///
    /// `None` on a backend that cannot sign.
    async fn presigned_put_url(
        &self,
        _key: &str,
        _ttl: std::time::Duration,
        _content_type: Option<&str>,
    ) -> Option<String> {
        None
    }
}

/// `Arc<dyn Storage>` — the standard way to share a backend.
pub type BoxedStorage = Arc<dyn Storage>;

/// Reject a key that could escape the storage root: `..`, a leading
/// `/` or `\`, a Windows drive prefix, or a null byte. Backends that
/// store keys as given, such as S3, should still call it so keys stay
/// consistent.
///
/// # Errors
/// [`StorageError::InvalidPath`] naming what was wrong with the key.
pub fn validate_key(key: &str) -> Result<(), StorageError> {
    if key.is_empty() {
        return Err(StorageError::InvalidPath("empty key".into()));
    }
    if key.starts_with('/') {
        return Err(StorageError::InvalidPath(format!(
            "key must be relative: {key}"
        )));
    }
    if key.contains("..") {
        return Err(StorageError::InvalidPath(format!(
            "key contains `..`: {key}"
        )));
    }
    if key.contains('\0') {
        return Err(StorageError::InvalidPath(format!("key contains null byte")));
    }
    // Windows-shaped absolute keys. `Path::join` drops the root when
    // the argument is absolute, so `C:\secrets` or `\Windows` would
    // land outside the storage directory.
    //
    // The check is structural, not `Path::is_absolute`, whose answer
    // depends on the platform: on Unix it calls these relative, so a
    // Unix-only guard would let a key through that escapes as soon as
    // the same service runs on Windows.
    if key.starts_with('\\') {
        // `\dir\file`, and UNC `\\server\share`.
        return Err(StorageError::InvalidPath(format!(
            "key must be relative: {key}"
        )));
    }
    let mut chars = key.chars();
    if matches!(
        (chars.next(), chars.next()),
        (Some(drive), Some(':')) if drive.is_ascii_alphabetic()
    ) {
        // `C:\file` is absolute. Bare `C:file` resolves against that
        // drive's working directory, which is not ours either.
        return Err(StorageError::InvalidPath(format!(
            "key names a drive: {key}"
        )));
    }
    Ok(())
}

// ------------------------------------------------------------------ LocalStorage

/// Storage on the local filesystem, under one directory.
///
/// Keys are joined onto `root` as relative paths, and
/// [`validate_key`] rejects anything that would escape it.
pub struct LocalStorage {
    root: PathBuf,
    base_url: Option<String>,
}

impl LocalStorage {
    /// Store files under `root`, which is created on the first save.
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            base_url: None,
        }
    }

    /// Set the public URL prefix, so key `k` gets `{base_url}/{k}`.
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

/// Storage in a `HashMap`, for tests. It never touches disk.
#[derive(Default)]
pub struct InMemoryStorage {
    files: Mutex<HashMap<String, Vec<u8>>>,
}

impl InMemoryStorage {
    #[must_use]
    pub fn new() -> Self {
        Self {
            files: Mutex::new(HashMap::new()),
        }
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
        assert!(matches!(
            validate_key(""),
            Err(StorageError::InvalidPath(_))
        ));
    }

    #[test]
    fn validate_rejects_leading_slash() {
        assert!(matches!(
            validate_key("/etc/passwd"),
            Err(StorageError::InvalidPath(_))
        ));
    }

    #[test]
    fn validate_rejects_dotdot() {
        assert!(matches!(
            validate_key("../escape"),
            Err(StorageError::InvalidPath(_))
        ));
        assert!(matches!(
            validate_key("safe/../bad"),
            Err(StorageError::InvalidPath(_))
        ));
    }

    #[test]
    fn validate_accepts_normal_keys() {
        assert!(validate_key("avatars/alice.png").is_ok());
        assert!(validate_key("file.txt").is_ok());
    }

    /// `Path::join` drops the root for an absolute key, so on Windows
    /// these would land outside the storage directory.
    ///
    /// The test runs on every platform on purpose. `Path::is_absolute`
    /// calls all of them relative on Unix, so a platform-gated guard
    /// would pass here and still leave the hole on Windows.
    #[test]
    fn validate_rejects_windows_absolute_keys() {
        for key in [
            r"C:\Windows\System32\config\SAM",
            r"c:\lower\drive",
            r"C:relative-to-drive-cwd",
            r"\Windows\System32",
            r"\\server\share\file",
        ] {
            assert!(
                matches!(validate_key(key), Err(StorageError::InvalidPath(_))),
                "`{key}` escapes the storage root on Windows and must be rejected"
            );
        }
    }

    /// A colon or backslash elsewhere in the key is still fine. The
    /// guard is about escaping the root, not about the characters.
    #[test]
    fn validate_still_accepts_keys_that_merely_contain_those_characters() {
        assert!(validate_key("logs/2026-09-13T12:00:00Z.json").is_ok());
        assert!(validate_key("odd/name\\with-backslash.txt").is_ok());
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
