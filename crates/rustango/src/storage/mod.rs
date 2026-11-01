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
//! | [`azure::AzureBlobStorage`] | Azure Blob Storage or Azurite. Behind the `storage-azure` feature. |
//! | Anything else | Implement `Storage` over the SDK you want |
//!
//! They all share one trait, so you can pick a backend at startup
//! without changing the code that uses it. Several can run side by side
//! as named disks in a [`StorageRegistry`], built in code or from the
//! `[storage]` config section.
//!
//! [`LocalStorage`]: crate::storage::LocalStorage
//! [`InMemoryStorage`]: crate::storage::InMemoryStorage
//! [`s3::S3Storage`]: crate::storage::s3::S3Storage
//! [`azure::AzureBlobStorage`]: crate::storage::azure::AzureBlobStorage

#[cfg(feature = "storage-s3")]
pub mod s3;

#[cfg(feature = "storage-azure")]
pub mod azure;

#[cfg(any(feature = "storage-s3", feature = "storage-azure"))]
mod transport;

pub mod registry;
pub use registry::StorageRegistry;

use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_util::{Stream, StreamExt as _};

/// Re-exported so implementing [`Storage`] needs no `async-trait`
/// entry in your own `Cargo.toml`, and cannot end up on a different
/// version from the one the trait was declared with.
pub use async_trait::async_trait;

// ------------------------------------------------------------------ Errors

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StorageError {
    #[error("file not found: {0}")]
    NotFound(String),
    #[error("invalid path: {0}")]
    InvalidPath(String),
    #[error("io error: {0}")]
    Io(String),
}

/// The error a create-only write gives when `key` is already there.
fn already_exists(key: &str) -> StorageError {
    StorageError::Io(format!("`{key}` already exists"))
}

/// Percent-encode a key for a URL path. `/` stays as it is, because
/// keys use it as a separator; everything outside the unreserved set
/// is escaped.
fn encode_key(key: &str) -> String {
    percent_encode(key, true)
}

/// Like [`encode_key`], but it escapes `/` as well, for query values.
#[cfg(any(feature = "storage-s3", feature = "storage-azure"))]
fn encode_query(s: &str) -> String {
    percent_encode(s, false)
}

fn percent_encode(s: &str, keep_slash: bool) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'_' | b'.' | b'~')
            || (keep_slash && b == b'/')
        {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Refuse a `put.content_length` that disagrees with the length passed.
fn check_declared_len(key: &str, len: u64, put: &PutConditions) -> Result<(), StorageError> {
    match put.content_length {
        Some(declared) if declared != len => Err(StorageError::Io(format!(
            "`{key}`: content length {declared} does not match the {len} bytes to write"
        ))),
        _ => Ok(()),
    }
}

// ------------------------------------------------------------------ ByteStream

/// An object's bytes as a stream of chunks, for [`Storage::save_stream`]
/// and [`Storage::load_stream`].
///
/// Build one from an `axum::body::Body`, a `reqwest::Response`, `Bytes`
/// or a `Vec<u8>` with `ByteStream::from`, or from any stream with
/// [`ByteStream::new`]. It is a `Stream`, so `axum::body::Body::from_stream`
/// takes it as a response body.
pub struct ByteStream(Pin<Box<dyn Stream<Item = Result<Bytes, StorageError>> + Send>>);

impl ByteStream {
    pub fn new<S>(stream: S) -> Self
    where
        S: Stream<Item = Result<Bytes, StorageError>> + Send + 'static,
    {
        Self(Box::pin(stream))
    }

    /// Yields exactly `len` bytes, or ends with an error, so a body of
    /// another size never reaches the backend whole.
    fn exact(self, len: u64) -> Self {
        Self::new(Exact {
            inner: self,
            len,
            seen: 0,
            done: false,
        })
    }

    /// The whole body, exactly `len` bytes.
    async fn read_all(self, len: u64) -> Result<Vec<u8>, StorageError> {
        // `len` comes from the caller; do not trust it for a huge allocation.
        let cap = usize::try_from(len).unwrap_or(usize::MAX).min(8 << 20);
        let mut out = Vec::with_capacity(cap);
        let mut body = self.exact(len);
        while let Some(chunk) = body.next().await {
            out.extend_from_slice(&chunk?);
        }
        Ok(out)
    }
}

impl Stream for ByteStream {
    type Item = Result<Bytes, StorageError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.0.as_mut().poll_next(cx)
    }
}

impl std::fmt::Debug for ByteStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ByteStream { .. }")
    }
}

impl From<Bytes> for ByteStream {
    fn from(b: Bytes) -> Self {
        Self::new(futures_util::stream::iter([Ok(b)]))
    }
}

impl From<Vec<u8>> for ByteStream {
    fn from(v: Vec<u8>) -> Self {
        Self::from(Bytes::from(v))
    }
}

/// A request body, such as an upload an axum handler proxies on.
#[cfg(feature = "_axum")]
impl From<axum::body::Body> for ByteStream {
    fn from(body: axum::body::Body) -> Self {
        Self::new(
            body.into_data_stream()
                .map(|r| r.map_err(|e| StorageError::Io(format!("request body: {e}")))),
        )
    }
}

/// The response body. Check the status first: this streams any body.
#[cfg(any(
    feature = "storage-s3",
    feature = "storage-azure",
    feature = "http-client"
))]
impl From<reqwest::Response> for ByteStream {
    fn from(resp: reqwest::Response) -> Self {
        Self::new(futures_util::stream::try_unfold(resp, |mut r| async move {
            match r.chunk().await {
                Ok(Some(b)) => Ok(Some((b, r))),
                Ok(None) => Ok(None),
                Err(e) => Err(StorageError::Io(format!("read body: {e}"))),
            }
        }))
    }
}

/// [`ByteStream::exact`].
struct Exact {
    inner: ByteStream,
    len: u64,
    seen: u64,
    done: bool,
}

impl Stream for Exact {
    type Item = Result<Bytes, StorageError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }
        let next = std::task::ready!(self.inner.poll_next_unpin(cx));
        let (len, seen) = (self.len, self.seen);
        let out = match next {
            Some(Ok(b)) => {
                let seen = seen.saturating_add(b.len() as u64);
                if seen > len {
                    Err(StorageError::Io(format!(
                        "body is longer than the {len} bytes declared"
                    )))
                } else {
                    self.seen = seen;
                    return Poll::Ready(Some(Ok(b)));
                }
            }
            Some(Err(e)) => Err(e),
            None if seen < len => Err(StorageError::Io(format!(
                "body ended after {seen} of the {len} bytes declared"
            ))),
            None => {
                self.done = true;
                return Poll::Ready(None);
            }
        };
        self.done = true;
        Poll::Ready(Some(out))
    }
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

    /// [`Self::save`] with a MIME type, for backends that store one (S3).
    /// The default ignores it.
    async fn save_with_content_type(
        &self,
        key: &str,
        data: &[u8],
        _content_type: Option<&str>,
    ) -> Result<(), StorageError> {
        self.save(key, data).await
    }

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
    /// the backend. The browser must send every header in
    /// [`Self::presigned_put_headers`].
    ///
    /// S3 signs everything in `put`, so it refuses another size or type.
    /// Azure SAS cannot sign headers: there, confirm the upload with
    /// [`Self::metadata`]. `None` on a backend that cannot sign.
    async fn presigned_put_url(
        &self,
        _key: &str,
        _ttl: std::time::Duration,
        _put: &PutConditions,
    ) -> Option<String> {
        None
    }

    /// `true` when a [`Self::presigned_put_url`] refuses a body of another
    /// size or type than `put`. Azure SAS cannot, so media gives such a
    /// disk no direct-upload ticket. A wrapper must forward it.
    fn presigned_put_enforces_conditions(&self) -> bool {
        true
    }

    /// The headers a client must send with a [`Self::presigned_put_url`]
    /// upload. The default is [`PutConditions::headers`]; Azure adds
    /// `x-ms-blob-type`.
    fn presigned_put_headers(
        &self,
        put: &PutConditions,
    ) -> std::collections::BTreeMap<String, String> {
        put.headers()
    }

    /// What the backend holds at `key`, or `None` when nothing is there.
    ///
    /// The default is an error, so a backend that cannot report this
    /// never lets a direct upload be confirmed on trust (#1851).
    async fn metadata(&self, key: &str) -> Result<Option<ObjectMeta>, StorageError> {
        Err(StorageError::Io(format!(
            "this backend cannot report object metadata (key `{key}`)"
        )))
    }

    /// Write exactly `len` bytes from `body` to `key` as they arrive.
    /// A body of another size, or a `put.content_length` other than
    /// `len`, is refused; `put.create_only` refuses an existing key.
    /// A failed or dropped body leaves no object.
    ///
    /// The default collects the body, then calls
    /// [`Self::save_with_content_type`], so it holds the whole object in
    /// memory. It refuses `create_only`, which it cannot make atomic.
    /// A wrapper must forward this and [`Self::load_stream`].
    async fn save_stream(
        &self,
        key: &str,
        len: u64,
        body: ByteStream,
        put: &PutConditions,
    ) -> Result<(), StorageError> {
        validate_key(key)?;
        check_declared_len(key, len, put)?;
        if put.create_only {
            return Err(StorageError::Io(format!(
                "this backend cannot write create-only (key `{key}`)"
            )));
        }
        let data = body.read_all(len).await?;
        self.save_with_content_type(key, &data, put.content_type.as_deref())
            .await
    }

    /// Read `key` as a stream of chunks, or [`StorageError::NotFound`].
    /// The default is [`Self::load`] as one chunk.
    async fn load_stream(&self, key: &str) -> Result<ByteStream, StorageError> {
        Ok(ByteStream::from(self.load(key).await?))
    }
}

/// Size and stored type of an object, as the backend reports them.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ObjectMeta {
    pub size: u64,
    /// `None` on a backend that stores no type, such as `LocalStorage`.
    pub content_type: Option<String>,
}

impl ObjectMeta {
    #[must_use]
    pub fn new(size: u64, content_type: Option<String>) -> Self {
        Self { size, content_type }
    }
}

/// What a presigned PUT signs. Built with the chained setters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct PutConditions {
    pub content_type: Option<String>,
    pub content_length: Option<u64>,
    /// Sign `If-None-Match: *`: the URL creates the object, never replaces it.
    pub create_only: bool,
}

impl PutConditions {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn content_type(mut self, ct: impl Into<String>) -> Self {
        self.content_type = Some(ct.into());
        self
    }

    #[must_use]
    pub fn content_length(mut self, len: u64) -> Self {
        self.content_length = Some(len);
        self
    }

    #[must_use]
    pub fn create_only(mut self) -> Self {
        self.create_only = true;
        self
    }

    /// The headers the client must send. `Content-Length` is left out:
    /// the client sets it from the body.
    #[must_use]
    pub fn headers(&self) -> std::collections::BTreeMap<String, String> {
        let mut h = std::collections::BTreeMap::new();
        if let Some(ct) = &self.content_type {
            h.insert("content-type".to_owned(), ct.clone());
        }
        if self.create_only {
            h.insert("if-none-match".to_owned(), "*".to_owned());
        }
        h
    }
}

/// `Arc<dyn Storage>` — the standard way to share a backend.
pub type BoxedStorage = Arc<dyn Storage>;

/// Reject a key that could escape the storage root: a `..` segment, a leading
/// `/` or `\`, a Windows drive prefix, or a null byte. Empty and `.`
/// segments are refused too. Backends that
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
    // A whole `..` segment only: `report..final.pdf` is a file name (#1903).
    if key.split(['/', '\\']).any(|seg| seg == "..") {
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
    // `a//b`, `./a` and `a/` name another file on disk than on S3.
    if key
        .split(['/', '\\'])
        .any(|seg| seg.is_empty() || seg == ".")
    {
        return Err(StorageError::InvalidPath(format!(
            "key has an empty or `.` segment: {key}"
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

    /// Write `body` to a temp file beside `key`, then move it into place.
    async fn write_file(
        &self,
        key: &str,
        body: LocalBody<'_>,
        create_only: bool,
    ) -> Result<(), StorageError> {
        validate_key(key)?;
        let path = self.full_path(key);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| StorageError::Io(e.to_string()))?;
        }
        // Temp file + rename: a reader never sees a torn file, a crash
        // leaves the old one (#1905).
        let tmp = path.with_file_name(format!(".{}.tmp", uuid::Uuid::new_v4().simple()));
        let io = |e: std::io::Error| StorageError::Io(e.to_string());
        // Armed before the open, so a cancel at any await removes it.
        let guard = TmpFile(Some(tmp.clone()));
        let mut file = match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .await
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                guard.keep();
                return Err(io(e));
            }
            Err(e) => return Err(io(e)),
        };
        let streamed = matches!(body, LocalBody::Stream(..));
        match body {
            LocalBody::Slice(data) => tokio::io::AsyncWriteExt::write_all(&mut file, data)
                .await
                .map_err(io)?,
            LocalBody::Stream(body, len) => {
                let mut body = body.exact(len);
                while let Some(chunk) = body.next().await {
                    tokio::io::AsyncWriteExt::write_all(&mut file, &chunk?)
                        .await
                        .map_err(io)?;
                }
            }
        }
        tokio::io::AsyncWriteExt::flush(&mut file)
            .await
            .map_err(io)?;
        if streamed {
            file.sync_all().await.map_err(io)?;
        }
        drop(file);
        if create_only {
            // A link, unlike a rename, refuses an existing name; the guard
            // then drops the temp name.
            return match tokio::fs::hard_link(&tmp, &path).await {
                Ok(()) => Ok(()),
                Err(_) if is_dir(&path).await => Err(names_a_directory(key)),
                Err(e) => Err(link_error(key, &e)),
            };
        }
        if let Err(e) = tokio::fs::rename(&tmp, &path).await {
            return Err(if is_dir(&path).await {
                names_a_directory(key)
            } else {
                io(e)
            });
        }
        guard.keep();
        Ok(())
    }
}

/// `true` when `path` is a directory: a key prefix, not an object.
async fn is_dir(path: &std::path::Path) -> bool {
    tokio::fs::metadata(path).await.is_ok_and(|m| m.is_dir())
}

fn names_a_directory(key: &str) -> StorageError {
    StorageError::Io(format!("`{key}` names a directory on this disk"))
}

/// A failed create-only link. FAT, SMB and some FUSE mounts have no
/// hard links; say so rather than give a bare I/O error.
fn link_error(key: &str, e: &std::io::Error) -> StorageError {
    use std::io::ErrorKind;
    match e.kind() {
        ErrorKind::AlreadyExists => already_exists(key),
        ErrorKind::Unsupported | ErrorKind::PermissionDenied => StorageError::Io(format!(
            "`{key}`: create-only writes need hard links, which this filesystem may not support: {e}"
        )),
        _ => StorageError::Io(e.to_string()),
    }
}

/// What [`LocalStorage::write_file`] writes.
enum LocalBody<'a> {
    Slice(&'a [u8]),
    /// Exactly this many bytes, fsynced before they are published.
    Stream(ByteStream, u64),
}

#[async_trait]
impl Storage for LocalStorage {
    async fn save(&self, key: &str, data: &[u8]) -> Result<(), StorageError> {
        self.write_file(key, LocalBody::Slice(data), false).await
    }

    async fn save_stream(
        &self,
        key: &str,
        len: u64,
        body: ByteStream,
        put: &PutConditions,
    ) -> Result<(), StorageError> {
        check_declared_len(key, len, put)?;
        // Fail before reading the body; the link below is what holds.
        if put.create_only {
            if self.exists(key).await? {
                return Err(already_exists(key));
            }
            if is_dir(&self.full_path(key)).await {
                return Err(names_a_directory(key));
            }
        }
        self.write_file(key, LocalBody::Stream(body, len), put.create_only)
            .await
    }

    async fn load_stream(&self, key: &str) -> Result<ByteStream, StorageError> {
        validate_key(key)?;
        let path = self.full_path(key);
        let file = match tokio::fs::File::open(&path).await {
            // Opening a directory works on unix and is refused on Windows;
            // either way it holds no object.
            Ok(_) if is_dir(&path).await => return Err(StorageError::NotFound(key.to_owned())),
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound || is_dir(&path).await => {
                return Err(StorageError::NotFound(key.to_owned()))
            }
            Err(e) => return Err(StorageError::Io(e.to_string())),
        };
        Ok(ByteStream::new(futures_util::stream::try_unfold(
            file,
            |mut file| async move {
                let mut buf = bytes::BytesMut::with_capacity(64 * 1024);
                let n = tokio::io::AsyncReadExt::read_buf(&mut file, &mut buf)
                    .await
                    .map_err(|e| StorageError::Io(e.to_string()))?;
                Ok((n > 0).then(|| (buf.freeze(), file)))
            },
        )))
    }

    async fn load(&self, key: &str) -> Result<Vec<u8>, StorageError> {
        validate_key(key)?;
        let path = self.full_path(key);
        match tokio::fs::read(&path).await {
            Ok(bytes) => Ok(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound || is_dir(&path).await => {
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
            // A directory holds no object: nothing to delete.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound || is_dir(&path).await => Ok(()),
            Err(e) => Err(StorageError::Io(e.to_string())),
        }
    }

    async fn exists(&self, key: &str) -> Result<bool, StorageError> {
        // Only a file is an object; a directory is a key prefix.
        Ok(self.metadata(key).await?.is_some())
    }

    async fn metadata(&self, key: &str) -> Result<Option<ObjectMeta>, StorageError> {
        validate_key(key)?;
        match tokio::fs::metadata(self.full_path(key)).await {
            Ok(m) if m.is_file() => Ok(Some(ObjectMeta::new(m.len(), None))),
            Ok(_) => Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(StorageError::Io(e.to_string())),
        }
    }

    fn url(&self, key: &str) -> Option<String> {
        let base = self.base_url.as_ref()?;
        Some(format!(
            "{}/{}",
            base.trim_end_matches('/'),
            encode_key(key)
        ))
    }
}

/// Removes a temp file on drop, unless [`Self::keep`] ran: an error or a
/// cancelled save leaves nothing behind.
struct TmpFile(Option<PathBuf>);

impl TmpFile {
    fn keep(mut self) {
        self.0 = None;
    }
}

impl Drop for TmpFile {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            let _ = std::fs::remove_file(p);
        }
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

    /// Buffers like the default, but checks `create_only` under the lock.
    async fn save_stream(
        &self,
        key: &str,
        len: u64,
        body: ByteStream,
        put: &PutConditions,
    ) -> Result<(), StorageError> {
        validate_key(key)?;
        check_declared_len(key, len, put)?;
        let data = body.read_all(len).await?;
        let mut files = self.files.lock().expect("storage mutex poisoned");
        if put.create_only && files.contains_key(key) {
            return Err(already_exists(key));
        }
        files.insert(key.to_owned(), data);
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

    async fn metadata(&self, key: &str) -> Result<Option<ObjectMeta>, StorageError> {
        validate_key(key)?;
        Ok(self
            .files
            .lock()
            .expect("storage mutex poisoned")
            .get(key)
            .map(|v| ObjectMeta::new(v.len() as u64, None)))
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
    fn validate_rejects_empty_and_dot_segments() {
        for bad in ["a//b", "./a", "a/./b", "a/", "a\\\\b", "."] {
            assert!(
                matches!(validate_key(bad), Err(StorageError::InvalidPath(_))),
                "accepted {bad:?}"
            );
        }
        for ok in [".hidden", "a.b/c.d", "a/..b", "a/b."] {
            assert!(validate_key(ok).is_ok(), "rejected {ok:?}");
        }
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

    /// #1903: dots inside a file name are not a path segment.
    #[test]
    fn validate_accepts_dots_inside_a_segment() {
        for key in [
            "report..final.pdf",
            "a/..b/c",
            "x/y../z",
            "...",
            "a/.hidden",
        ] {
            assert!(validate_key(key).is_ok(), "`{key}` must be accepted");
        }
        for key in ["..", "a/..", r"a\..\b", "a/../b"] {
            assert!(validate_key(key).is_err(), "`{key}` must be rejected");
        }
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

    /// A directory is not an object: local disk answers like memory.
    #[tokio::test]
    async fn local_directory_key_is_not_an_object() {
        let dir = tempdir();
        let s = LocalStorage::new(dir.clone());
        let mem = InMemoryStorage::new();
        for st in [&s as &dyn Storage, &mem] {
            st.save("avatars/a.png", b"x").await.unwrap();
            assert!(!st.exists("avatars").await.unwrap());
            assert!(st.metadata("avatars").await.unwrap().is_none());
            assert!(matches!(
                st.load("avatars").await,
                Err(StorageError::NotFound(_))
            ));
            assert!(matches!(
                st.load_stream("avatars").await,
                Err(StorageError::NotFound(_))
            ));
            st.delete("avatars").await.unwrap();
            assert!(st.exists("avatars/a.png").await.unwrap());
        }
        let put = PutConditions::new().create_only();
        let err = s
            .save_stream("avatars", 1, ByteStream::from(vec![1u8]), &put)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("directory"), "{err}");
        let err = s.save("avatars", b"x").await.unwrap_err();
        assert!(err.to_string().contains("directory"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_url_percent_encodes_the_key() {
        let s = LocalStorage::new(PathBuf::from("/tmp")).with_base_url("https://x/m/");
        assert_eq!(
            s.url("docs/Q3 report #2.pdf").as_deref(),
            Some("https://x/m/docs/Q3%20report%20%232.pdf")
        );
    }

    #[test]
    fn tmp_file_guard_removes_unless_kept() {
        let dir = tempdir();
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.tmp"), dir.join("b.tmp"));
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(&b, b"x").unwrap();
        drop(TmpFile(Some(a.clone())));
        TmpFile(Some(b.clone())).keep();
        assert!(!a.exists(), "a dropped guard left its file");
        assert!(b.exists(), "a kept guard removed its file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A save dropped mid-write leaves no temp file.
    #[tokio::test]
    async fn a_cancelled_local_save_leaves_no_temp_file() {
        use std::future::Future as _;
        let dir = tempdir();
        let s = LocalStorage::new(dir.clone());
        let data = vec![0u8; 64 << 20];
        let tmp_seen = || {
            std::fs::read_dir(&dir)
                .into_iter()
                .flatten()
                .any(|e| e.unwrap().file_name().to_string_lossy().ends_with(".tmp"))
        };
        {
            let mut fut = std::pin::pin!(s.save("big.bin", &data));
            loop {
                let polled =
                    std::future::poll_fn(|cx| std::task::Poll::Ready(fut.as_mut().poll(cx))).await;
                assert!(polled.is_pending(), "save finished before it was cancelled");
                if tmp_seen() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        }
        // Let a write already in flight on the blocking pool finish.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(!tmp_seen(), "a cancelled save left its temp file");
        assert!(!dir.join("big.bin").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #1905: a load racing a save sees the old file or the new one,
    /// never a torn one, and no temp file is left behind.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn local_storage_save_is_atomic() {
        let dir = tempdir();
        let s = Arc::new(LocalStorage::new(dir.clone()));
        const N: usize = 4 << 20;
        s.save("f.bin", &vec![b'a'; N]).await.unwrap();
        let writer = {
            let s = s.clone();
            tokio::spawn(async move {
                for i in 0..20u8 {
                    s.save("f.bin", &vec![b'b' + i % 2; N]).await.unwrap();
                }
            })
        };
        while !writer.is_finished() {
            let got = s.load("f.bin").await.unwrap();
            assert_eq!(got.len(), N, "torn read");
        }
        writer.await.unwrap();
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("f.bin")]);
        let meta = s.metadata("f.bin").await.unwrap().unwrap();
        assert_eq!((meta.size, meta.content_type), (N as u64, None));
        assert_eq!(s.metadata("nope").await.unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -------- streaming --------

    fn parts(parts: &[&'static [u8]]) -> ByteStream {
        let parts: Vec<_> = parts.iter().map(|p| Ok(Bytes::from_static(p))).collect();
        ByteStream::new(futures_util::stream::iter(parts))
    }

    /// `first`, then an error.
    fn failing(first: &'static [u8]) -> ByteStream {
        ByteStream::new(futures_util::stream::iter([
            Ok(Bytes::from_static(first)),
            Err(StorageError::Io("client went away".into())),
        ]))
    }

    async fn collect(s: ByteStream) -> Vec<u8> {
        let mut out = Vec::new();
        let mut s = s;
        while let Some(c) = s.next().await {
            out.extend_from_slice(&c.unwrap());
        }
        out
    }

    fn tmp_files(dir: &std::path::Path) -> Vec<String> {
        walk(dir)
            .into_iter()
            .filter(|n| n.ends_with(".tmp"))
            .collect()
    }

    fn walk(dir: &std::path::Path) -> Vec<String> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            if e.path().is_dir() {
                out.extend(walk(&e.path()));
            } else {
                out.push(e.file_name().to_string_lossy().into_owned());
            }
        }
        out
    }

    /// A backend with only the required methods, so the defaults run.
    struct Plain(InMemoryStorage);

    #[async_trait]
    impl Storage for Plain {
        async fn save(&self, key: &str, data: &[u8]) -> Result<(), StorageError> {
            self.0.save(key, data).await
        }
        async fn load(&self, key: &str) -> Result<Vec<u8>, StorageError> {
            self.0.load(key).await
        }
        async fn delete(&self, key: &str) -> Result<(), StorageError> {
            self.0.delete(key).await
        }
        async fn exists(&self, key: &str) -> Result<bool, StorageError> {
            self.0.exists(key).await
        }
        fn url(&self, _key: &str) -> Option<String> {
            None
        }
    }

    #[tokio::test]
    async fn default_save_stream_checks_the_length() {
        let s = Plain(InMemoryStorage::new());
        let none = PutConditions::new();
        s.save_stream("k", 6, parts(&[b"abc", b"def"]), &none)
            .await
            .unwrap();
        assert_eq!(s.load("k").await.unwrap(), b"abcdef");
        assert_eq!(collect(s.load_stream("k").await.unwrap()).await, b"abcdef");
        for (len, body) in [(7, parts(&[b"abc", b"def"])), (5, parts(&[b"abc", b"def"]))] {
            let err = s.save_stream("short", len, body, &none).await.unwrap_err();
            assert!(err.to_string().contains("bytes declared"), "{err}");
        }
        let err = s
            .save_stream("k2", 3, parts(&[b"abc"]), &none.clone().content_length(4))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("does not match"), "{err}");
        assert!(s.save_stream("f", 9, failing(b"abc"), &none).await.is_err());
        for k in ["short", "k2", "f"] {
            assert!(!s.exists(k).await.unwrap(), "{k} was written");
        }
        assert!(matches!(
            s.load_stream("nope").await.unwrap_err(),
            StorageError::NotFound(_)
        ));
    }

    /// The default cannot make create-only atomic, so it refuses it.
    #[tokio::test]
    async fn default_save_stream_refuses_create_only() {
        let s = Plain(InMemoryStorage::new());
        let err = s
            .save_stream("k", 1, parts(&[b"x"]), &PutConditions::new().create_only())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("create-only"), "{err}");
        assert!(!s.exists("k").await.unwrap());
    }

    #[tokio::test]
    async fn in_memory_save_stream_create_only_refuses_an_existing_key() {
        let s = InMemoryStorage::new();
        let once = PutConditions::new().create_only();
        s.save_stream("k", 3, parts(&[b"old"]), &once)
            .await
            .unwrap();
        let err = s
            .save_stream("k", 3, parts(&[b"new"]), &once)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
        assert_eq!(s.load("k").await.unwrap(), b"old");
        s.save_stream("k", 3, parts(&[b"new"]), &PutConditions::new())
            .await
            .unwrap();
        assert_eq!(s.load("k").await.unwrap(), b"new");
    }

    #[tokio::test]
    async fn local_save_stream_and_load_stream_round_trip() {
        let dir = tempdir();
        let s = LocalStorage::new(dir.clone());
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let body = ByteStream::new(futures_util::stream::iter(
            data.chunks(7_000)
                .map(|c| Ok(Bytes::copy_from_slice(c)))
                .collect::<Vec<_>>(),
        ));
        s.save_stream("a/b.bin", data.len() as u64, body, &PutConditions::new())
            .await
            .unwrap();
        assert_eq!(s.load("a/b.bin").await.unwrap(), data);
        let mut stream = s.load_stream("a/b.bin").await.unwrap();
        let (mut got, mut pieces) = (Vec::new(), 0);
        while let Some(c) = stream.next().await {
            got.extend_from_slice(&c.unwrap());
            pieces += 1;
        }
        assert_eq!(got, data);
        assert!(pieces > 1, "load_stream read the file in one piece");
        assert!(tmp_files(&dir).is_empty());
        assert!(matches!(
            s.load_stream("nope").await.unwrap_err(),
            StorageError::NotFound(_)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A body of another size, or one that fails, writes nothing.
    #[tokio::test]
    async fn local_save_stream_refuses_a_wrong_length_and_leaves_nothing() {
        let dir = tempdir();
        let s = LocalStorage::new(dir.clone());
        let none = PutConditions::new();
        s.save("k", b"old").await.unwrap();
        for (len, body) in [
            (7, parts(&[b"abc", b"def"])),
            (5, parts(&[b"abc", b"def"])),
            (9, failing(b"abc")),
        ] {
            assert!(s.save_stream("k", len, body, &none).await.is_err());
            assert_eq!(s.load("k").await.unwrap(), b"old", "len {len}");
        }
        let err = s
            .save_stream("k", 6, parts(&[b"abcdef"]), &none.clone().content_length(5))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("does not match"), "{err}");
        assert_eq!(walk(&dir), vec!["k".to_owned()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A save whose body stalls, then is dropped, leaves no temp file.
    #[tokio::test]
    async fn a_dropped_local_save_stream_leaves_no_temp_file() {
        let dir = tempdir();
        let s = LocalStorage::new(dir.clone());
        let body = ByteStream::new(
            futures_util::stream::iter([Ok(Bytes::from_static(b"abc"))])
                .chain(futures_util::stream::pending()),
        );
        let put = PutConditions::new();
        let save = s.save_stream("k", 10, body, &put);
        let r = tokio::time::timeout(std::time::Duration::from_millis(300), save).await;
        assert!(r.is_err(), "the save finished on a stalled body");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(walk(&dir).is_empty(), "{:?}", walk(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn local_save_stream_create_only_refuses_an_existing_key() {
        let dir = tempdir();
        let s = LocalStorage::new(dir.clone());
        let once = PutConditions::new().create_only();
        s.save_stream("k", 3, parts(&[b"old"]), &once)
            .await
            .unwrap();
        let err = s
            .save_stream("k", 3, parts(&[b"new"]), &once)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
        assert_eq!(s.load("k").await.unwrap(), b"old");
        assert_eq!(walk(&dir), vec!["k".to_owned()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// No hard links (FAT, SMB, some FUSE) gives an error that says so.
    #[test]
    fn a_failed_create_only_link_names_the_cause() {
        use std::io::{Error, ErrorKind};
        for kind in [ErrorKind::Unsupported, ErrorKind::PermissionDenied] {
            let err = link_error("k", &Error::from(kind)).to_string();
            assert!(err.contains("need hard links"), "{kind:?}: {err}");
        }
        let err = link_error("k", &Error::from(ErrorKind::AlreadyExists)).to_string();
        assert!(err.contains("already exists"), "{err}");
    }

    /// Racing create-only writers past the early check: one wins, the
    /// rest are refused, and the winner's bytes stay whole.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn local_create_only_writes_race_to_one_winner() {
        let dir = tempdir();
        let s = Arc::new(LocalStorage::new(dir.clone()));
        let tasks: Vec<_> = (0..8u8)
            .map(|i| {
                let s = s.clone();
                tokio::spawn(async move {
                    let body = ByteStream::from(vec![i; 1 << 16]);
                    s.write_file("k", LocalBody::Stream(body, 1 << 16), true)
                        .await
                })
            })
            .collect();
        let mut won = 0;
        for t in tasks {
            match t.await.unwrap() {
                Ok(()) => won += 1,
                Err(e) => assert!(e.to_string().contains("already exists"), "{e}"),
            }
        }
        assert_eq!(won, 1);
        let got = s.load("k").await.unwrap();
        assert!(got.iter().all(|b| *b == got[0]), "a torn create-only write");
        assert_eq!(walk(&dir), vec!["k".to_owned()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 512 MiB streams through while the writer never runs more than a
    /// few chunks ahead of what is on disk: memory stays flat.
    #[tokio::test]
    async fn local_save_stream_never_buffers_the_object() {
        const CHUNK: usize = 1 << 20;
        const CHUNKS: usize = 512;
        let dir = tempdir();
        let s = LocalStorage::new(dir.clone());
        let on_disk = {
            let dir = dir.clone();
            move || -> u64 {
                std::fs::read_dir(&dir)
                    .into_iter()
                    .flatten()
                    .flatten()
                    // Not `DirEntry::metadata`: on Windows that is the
                    // directory's copy, stale while the file is open.
                    .filter_map(|e| std::fs::metadata(e.path()).ok())
                    .map(|m| m.len())
                    .sum()
            }
        };
        let chunk = Bytes::from(vec![7u8; CHUNK]);
        let body = ByteStream::new(futures_util::stream::iter(0..CHUNKS).map(move |i| {
            let ahead = (i * CHUNK) as u64 - on_disk();
            assert!(
                ahead <= 4 * CHUNK as u64,
                "chunk {i}: {ahead} bytes not yet written"
            );
            Ok(chunk.clone())
        }));
        let len = (CHUNK * CHUNKS) as u64;
        s.save_stream("big.bin", len, body, &PutConditions::new())
            .await
            .unwrap();
        assert_eq!(s.metadata("big.bin").await.unwrap().unwrap().size, len);
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
