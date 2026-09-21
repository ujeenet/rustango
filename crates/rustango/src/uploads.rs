//! Multipart file uploads. Joins axum's multipart extractor to the
//! [`crate::storage::Storage`] trait, and handles size limits,
//! extension checks and filename sanitizing for you.
//!
//! ## Security
//!
//! Set `max_bytes` and `allowed_extensions` on every upload route. With
//! no extension list any type is accepted, including scripts. The
//! client's `Content-Type` is never trusted: it is stored for reference
//! only, so sniff or re-encode the bytes yourself if the type matters.
//! Filenames are stripped to a basename, so a client-sent path such as
//! `../../etc/passwd` cannot escape the prefix.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::uploads::{UploadConfig, save_uploads};
//! use rustango::storage::BoxedStorage;
//! use axum::extract::{Multipart, State};
//! use axum::Json;
//!
//! async fn upload(
//!     State(store): State<BoxedStorage>,
//!     mp: Multipart,
//! ) -> Json<Vec<String>> {
//!     let cfg = UploadConfig::new("uploads/")
//!         .max_bytes(5 * 1024 * 1024)
//!         .allowed_extensions(&["png", "jpg", "jpeg", "webp"]);
//!     let saved = save_uploads(mp, &cfg, &store).await.unwrap();
//!     Json(saved.into_iter().map(|s| s.key).collect())
//! }
//! ```

use std::collections::HashSet;
use std::path::Path;

use axum::extract::Multipart;

use crate::storage::{BoxedStorage, StorageError};

#[derive(Debug, thiserror::Error)]
pub enum UploadError {
    #[error("multipart parse error: {0}")]
    Parse(String),
    #[error("file too large: {actual} bytes (max {max})")]
    TooLarge { actual: usize, max: usize },
    #[error("file extension `{0}` not allowed")]
    BadExtension(String),
    #[error("filename missing in multipart field")]
    MissingFilename,
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
}

impl From<axum::extract::multipart::MultipartError> for UploadError {
    fn from(e: axum::extract::multipart::MultipartError) -> Self {
        Self::Parse(e.to_string())
    }
}

/// Per-upload configuration. Cheap to clone.
#[derive(Clone, Debug)]
pub struct UploadConfig {
    /// Storage prefix (e.g. `"avatars/"` or `"uploads/2026/05/"`).
    /// The original filename is appended after sanitization.
    pub prefix: String,
    /// Hard cap on a single file's size. Default: 10 MiB.
    pub max_bytes: usize,
    /// Extensions to accept (lowercase, no leading dot). An empty set
    /// accepts anything, which is rarely what you want.
    pub allowed_extensions: HashSet<String>,
    /// How many non-file fields to skip while iterating.
    pub skip_fields: usize,
    /// Prepend a timestamp to the key so two uploads with the same name
    /// do not overwrite each other. Default: true.
    pub randomize_filename: bool,
}

impl UploadConfig {
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: ensure_trailing_slash(prefix.into()),
            max_bytes: 10 * 1024 * 1024,
            allowed_extensions: HashSet::new(),
            skip_fields: 0,
            randomize_filename: true,
        }
    }

    #[must_use]
    pub fn max_bytes(mut self, n: usize) -> Self {
        self.max_bytes = n;
        self
    }

    #[must_use]
    pub fn allowed_extensions(mut self, exts: &[&str]) -> Self {
        self.allowed_extensions = exts.iter().map(|e| e.to_ascii_lowercase()).collect();
        self
    }

    #[must_use]
    pub fn randomize_filename(mut self, on: bool) -> Self {
        self.randomize_filename = on;
        self
    }
}

/// Result of saving one uploaded file.
#[derive(Debug, Clone)]
pub struct SavedUpload {
    /// Storage key. Pass to `storage.url(&saved.key)` for a public URL.
    pub key: String,
    /// Original filename from the multipart field.
    pub original_filename: String,
    /// Content type the client claimed. Clients lie; do not trust it
    /// for access or rendering decisions.
    pub content_type: Option<String>,
    pub size_bytes: usize,
}

/// Save every file field in `mp` to `storage` and return one
/// [`SavedUpload`] per file. Fields without a `filename` are skipped.
///
/// The first error stops the loop. Files saved before it stay in
/// storage, so clean them up if that matters.
///
/// # Errors
/// See [`UploadError`].
pub async fn save_uploads(
    mut mp: Multipart,
    cfg: &UploadConfig,
    storage: &BoxedStorage,
) -> Result<Vec<SavedUpload>, UploadError> {
    let mut out = Vec::new();
    let mut skipped = 0;
    while let Some(mut field) = mp.next_field().await? {
        let Some(filename) = field.file_name().map(str::to_owned) else {
            // Not a file field.
            if skipped < cfg.skip_fields {
                skipped += 1;
            }
            continue;
        };
        let content_type = field.content_type().map(str::to_owned);

        // Read chunk by chunk and stop at the first chunk that crosses
        // `max_bytes`. Buffering the whole field first would let a huge
        // body eat memory even when the cap is small.
        let mut bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = field.chunk().await? {
            if bytes.len().saturating_add(chunk.len()) > cfg.max_bytes {
                return Err(UploadError::TooLarge {
                    actual: bytes.len() + chunk.len(),
                    max: cfg.max_bytes,
                });
            }
            bytes.extend_from_slice(&chunk);
        }

        let ext = lowercase_ext(&filename);
        if !cfg.allowed_extensions.is_empty() {
            let allowed = match &ext {
                Some(e) => cfg.allowed_extensions.contains(e),
                None => false,
            };
            if !allowed {
                return Err(UploadError::BadExtension(
                    ext.unwrap_or_else(|| "<none>".into()),
                ));
            }
        }

        let safe_name = sanitize_filename(&filename);
        let key_filename = if cfg.randomize_filename {
            randomize(&safe_name)
        } else {
            safe_name
        };
        let key = format!("{}{key_filename}", cfg.prefix);

        let size = bytes.len();
        storage.save(&key, &bytes).await?;
        out.push(SavedUpload {
            key,
            original_filename: filename,
            content_type,
            size_bytes: size,
        });
    }
    Ok(out)
}

// ---- pure helpers ----

fn ensure_trailing_slash(mut s: String) -> String {
    if !s.is_empty() && !s.ends_with('/') {
        s.push('/');
    }
    s
}

/// Drop directory parts and replace anything outside `[a-zA-Z0-9._-]`
/// with `_`, so the result is safe to use as a storage key. This is
/// what stops a client-supplied path from escaping the upload prefix.
pub fn sanitize_filename(name: &str) -> String {
    // Keep the basename only; clients sometimes send a full path.
    // Split on both separators instead of `Path::file_name`, which
    // treats `\` as a separator only on Windows. Otherwise the same
    // upload gets a different name depending on the server's OS.
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(name);
    let mut out = String::with_capacity(base.len());
    for c in base.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push_str("upload");
    }
    out
}

/// The lowercase extension without its dot, or `None`.
fn lowercase_ext(name: &str) -> Option<String> {
    Path::new(name)
        .extension()
        .and_then(|s| s.to_str())
        .map(str::to_ascii_lowercase)
}

/// Build `{unix_nanos}-{name}` so two uploads with the same name do
/// not overwrite each other.
fn randomize(name: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{nanos}-{name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------- sanitize_filename

    #[test]
    fn sanitize_keeps_safe_ascii() {
        assert_eq!(sanitize_filename("photo.jpg"), "photo.jpg");
        assert_eq!(sanitize_filename("My_File-1.PDF"), "My_File-1.PDF");
    }

    #[test]
    fn sanitize_strips_directory_components() {
        // A client-sent path must not escape the prefix.
        assert_eq!(sanitize_filename("/etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        // Windows separators too, on every platform.
        assert_eq!(sanitize_filename("C:\\windows\\evil.exe"), "evil.exe");
        assert_eq!(sanitize_filename("C:/Users/me/photo.jpg"), "photo.jpg");
    }

    /// A path that is nothing but separators has no basename to keep.
    #[test]
    fn sanitize_handles_a_path_with_no_filename() {
        assert_eq!(sanitize_filename("/"), "_");
        assert_eq!(sanitize_filename("dir/"), "dir_");
    }

    #[test]
    fn sanitize_replaces_unsafe_chars() {
        assert_eq!(sanitize_filename("a b c.txt"), "a_b_c.txt");
        assert_eq!(sanitize_filename("hello+world.png"), "hello_world.png");
        assert_eq!(sanitize_filename("évil.jpg"), "_vil.jpg");
    }

    #[test]
    fn sanitize_empty_falls_back_to_upload() {
        assert_eq!(sanitize_filename(""), "upload");
    }

    // -------- extension extraction

    #[test]
    fn lowercase_ext_works() {
        assert_eq!(lowercase_ext("a.PNG").as_deref(), Some("png"));
        assert_eq!(lowercase_ext("a.tar.gz").as_deref(), Some("gz"));
        assert_eq!(lowercase_ext("noext"), None);
    }

    // -------- ensure_trailing_slash

    #[test]
    fn ensure_trailing_slash_idempotent() {
        assert_eq!(ensure_trailing_slash("a/".into()), "a/");
        assert_eq!(ensure_trailing_slash("a".into()), "a/");
        assert_eq!(ensure_trailing_slash(String::new()), "");
    }

    // -------- UploadConfig builder

    #[test]
    fn config_defaults() {
        let c = UploadConfig::new("uploads");
        assert_eq!(c.prefix, "uploads/");
        assert_eq!(c.max_bytes, 10 * 1024 * 1024);
        assert!(c.allowed_extensions.is_empty());
        assert!(c.randomize_filename);
    }

    #[test]
    fn config_allowed_extensions_normalize_to_lowercase() {
        let c = UploadConfig::new("u").allowed_extensions(&["PNG", "Jpg"]);
        assert!(c.allowed_extensions.contains("png"));
        assert!(c.allowed_extensions.contains("jpg"));
        assert!(!c.allowed_extensions.contains("PNG"));
    }

    // -------- randomize

    #[test]
    fn randomize_preserves_name_and_prepends_nanos() {
        let r = randomize("photo.jpg");
        assert!(r.ends_with("-photo.jpg"));
        // Nanos prefix is purely digits before the dash.
        let dash = r.find('-').unwrap();
        assert!(r[..dash].chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn randomize_two_calls_produce_different_prefixes() {
        let a = randomize("x");
        std::thread::sleep(std::time::Duration::from_micros(1));
        let b = randomize("x");
        assert_ne!(a, b);
    }

    // -------- save_uploads end-to-end via a real multipart body

    #[tokio::test]
    async fn save_uploads_writes_file_to_storage() {
        use crate::storage::InMemoryStorage;
        use axum::body::Body;
        use axum::http::header;
        use axum::http::Request;
        use axum::routing::post;
        use axum::Router;
        use std::sync::Arc as StdArc;
        use tower::ServiceExt;

        // A tiny app so the test runs the real multipart parser.
        let storage: BoxedStorage = StdArc::new(InMemoryStorage::new());
        let storage_for_handler = storage.clone();
        let app = Router::new().route(
            "/upload",
            post(move |mp: Multipart| {
                let storage = storage_for_handler.clone();
                async move {
                    let cfg = UploadConfig::new("uploads/").randomize_filename(false);
                    let saved = save_uploads(mp, &cfg, &storage).await.unwrap();
                    saved
                        .into_iter()
                        .map(|s| s.key)
                        .collect::<Vec<_>>()
                        .join(",")
                }
            }),
        );

        let boundary = "test-boundary";
        let body = format!(
            "--{boundary}\r\n\
             Content-Disposition: form-data; name=\"file\"; filename=\"hello.txt\"\r\n\
             Content-Type: text/plain\r\n\
             \r\n\
             hello world\r\n\
             --{boundary}--\r\n"
        );
        let req = Request::builder()
            .method("POST")
            .uri("/upload")
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(body))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        let body_bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
            .await
            .unwrap();
        let body_str = std::str::from_utf8(&body_bytes).unwrap();
        assert_eq!(body_str, "uploads/hello.txt");

        // The file really landed in storage.
        let stored = storage.load("uploads/hello.txt").await.unwrap();
        assert_eq!(&stored, b"hello world");
    }

    #[tokio::test]
    async fn save_uploads_rejects_oversize_file() {
        use crate::storage::InMemoryStorage;
        use axum::body::Body;
        use axum::http::header;
        use axum::http::Request;
        use axum::routing::post;
        use axum::Router;
        use std::sync::Arc as StdArc;
        use tower::ServiceExt;

        let storage: BoxedStorage = StdArc::new(InMemoryStorage::new());
        let storage_for_handler = storage.clone();
        let app = Router::new().route(
            "/upload",
            post(move |mp: Multipart| {
                let storage = storage_for_handler.clone();
                async move {
                    let cfg = UploadConfig::new("u/").max_bytes(5);
                    match save_uploads(mp, &cfg, &storage).await {
                        Ok(_) => (axum::http::StatusCode::OK, "ok".to_owned()),
                        Err(e) => (axum::http::StatusCode::BAD_REQUEST, e.to_string()),
                    }
                }
            }),
        );

        let boundary = "b";
        // 100 bytes of payload, max is 5.
        let payload = "x".repeat(100);
        let body = format!(
            "--{boundary}\r\n\
             Content-Disposition: form-data; name=\"f\"; filename=\"big.bin\"\r\n\
             Content-Type: application/octet-stream\r\n\
             \r\n\
             {payload}\r\n\
             --{boundary}--\r\n"
        );
        let req = Request::builder()
            .method("POST")
            .uri("/upload")
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(body))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 400);
        let body_str = std::str::from_utf8(
            &axum::body::to_bytes(resp.into_body(), 1 << 16)
                .await
                .unwrap(),
        )
        .unwrap()
        .to_owned();
        assert!(body_str.contains("file too large"), "got: {body_str}");
    }

    /// An oversize body must error and write nothing. We cannot see the
    /// early abort from outside, but we can check the error surfaces
    /// and storage stays empty.
    #[tokio::test]
    async fn save_uploads_aborts_streaming_on_oversize() {
        use crate::storage::InMemoryStorage;
        use axum::body::Body;
        use axum::http::header;
        use axum::http::Request;
        use axum::routing::post;
        use axum::Router;
        use std::sync::Arc as StdArc;
        use tower::ServiceExt;

        let storage: BoxedStorage = StdArc::new(InMemoryStorage::new());
        let storage_for_handler = storage.clone();
        let app = Router::new().route(
            "/upload",
            post(move |mp: Multipart| {
                let storage = storage_for_handler.clone();
                async move {
                    let cfg = UploadConfig::new("u/").max_bytes(1024); // 1 KiB cap
                    match save_uploads(mp, &cfg, &storage).await {
                        Ok(_) => (axum::http::StatusCode::OK, "ok".to_owned()),
                        Err(e) => (axum::http::StatusCode::BAD_REQUEST, e.to_string()),
                    }
                }
            }),
        );

        let boundary = "b";
        // 50 KiB payload against a 1 KiB cap.
        let payload = "x".repeat(50 * 1024);
        let body = format!(
            "--{boundary}\r\n\
             Content-Disposition: form-data; name=\"f\"; filename=\"big.bin\"\r\n\
             Content-Type: application/octet-stream\r\n\
             \r\n\
             {payload}\r\n\
             --{boundary}--\r\n"
        );
        let req = Request::builder()
            .method("POST")
            .uri("/upload")
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(body))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 400);
        let body_str = std::str::from_utf8(
            &axum::body::to_bytes(resp.into_body(), 1 << 16)
                .await
                .unwrap(),
        )
        .unwrap()
        .to_owned();
        assert!(body_str.contains("file too large"), "got: {body_str}");
        // Nothing was written.
        assert!(
            storage.load("u/big.bin").await.is_err(),
            "no file should have been saved"
        );
    }

    #[tokio::test]
    async fn save_uploads_rejects_disallowed_extension() {
        use crate::storage::InMemoryStorage;
        use axum::body::Body;
        use axum::http::header;
        use axum::http::Request;
        use axum::routing::post;
        use axum::Router;
        use std::sync::Arc as StdArc;
        use tower::ServiceExt;

        let storage: BoxedStorage = StdArc::new(InMemoryStorage::new());
        let storage_for_handler = storage.clone();
        let app = Router::new().route(
            "/upload",
            post(move |mp: Multipart| {
                let storage = storage_for_handler.clone();
                async move {
                    let cfg = UploadConfig::new("u/").allowed_extensions(&["png", "jpg"]);
                    match save_uploads(mp, &cfg, &storage).await {
                        Ok(_) => (axum::http::StatusCode::OK, "ok".to_owned()),
                        Err(e) => (axum::http::StatusCode::BAD_REQUEST, e.to_string()),
                    }
                }
            }),
        );

        let boundary = "b";
        let body = format!(
            "--{boundary}\r\n\
             Content-Disposition: form-data; name=\"f\"; filename=\"evil.exe\"\r\n\
             \r\n\
             MZ\r\n\
             --{boundary}--\r\n"
        );
        let req = Request::builder()
            .method("POST")
            .uri("/upload")
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(body))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 400);
        let body_str = std::str::from_utf8(
            &axum::body::to_bytes(resp.into_body(), 1 << 16)
                .await
                .unwrap(),
        )
        .unwrap()
        .to_owned();
        assert!(body_str.contains("not allowed"), "got: {body_str}");
    }
}
