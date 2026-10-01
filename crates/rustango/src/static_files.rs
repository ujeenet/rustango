//! Static file serving: read files from a directory and send them with
//! `Content-Type`, `Cache-Control` and `Last-Modified`, and answer 304
//! to `If-Modified-Since`.
//!
//! Each file gets a strong `ETag` from its size and mtime, which
//! [`crate::etag::EtagLayer`] keeps, so `If-Range` resumes match it.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::static_files::{StaticFiles, static_router};
//!
//! // Mount /static/* over ./assets
//! let app = axum::Router::new()
//!     .nest("/static", static_router(StaticFiles::new("./assets")));
//! ```
//!
//! ## Security
//!
//! - Path traversal gets a 404. The path is normalised before it is
//!   joined to the root, so a `..` segment cannot read above the root,
//!   URL-encoded or not.
//! - A symlink out of the root gets a 404 too: the resolved path is
//!   checked against the root. [`StaticFiles::no_canonicalize`] turns
//!   that check off, so use it only on a directory layout you trust.
//! - A name starting with `.` gets a 404 unless
//!   [`StaticFiles::serve_hidden`] is on.
//! - Mount user uploads with [`StaticFiles::user_content`]: HTML, SVG
//!   and XML then download instead of running script on your origin.
//!
//! ## Caching
//!
//! `Cache-Control` defaults to `public, max-age=3600`; change it with
//! [`StaticFiles::cache_control`]. `Last-Modified` comes from the
//! file's mtime, and a matching `If-Modified-Since` gets a 304 with no
//! body.
//!
//! Bodies stream from disk, and a single `Range: bytes=` span gets a 206.
//!
//! [`StaticFiles::no_canonicalize`]: crate::static_files::StaticFiles::no_canonicalize
//! [`StaticFiles::serve_hidden`]: crate::static_files::StaticFiles::serve_hidden
//! [`StaticFiles::cache_control`]: crate::static_files::StaticFiles::cache_control

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{Path as PathExt, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::Response;
use axum::routing::get;
use axum::Router;

/// Static file server config.
#[derive(Clone, Debug)]
pub struct StaticFiles {
    root: PathBuf,
    cache_control: String,
    serve_hidden: bool,
    canonicalize: bool,
    user_content: bool,
}

impl StaticFiles {
    /// New server rooted at `dir`, caching `public, max-age=3600`.
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            root: dir.into(),
            cache_control: "public, max-age=3600".into(),
            serve_hidden: false,
            canonicalize: true,
            user_content: false,
        }
    }

    /// The directory files are served from.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Serve files users uploaded: HTML, SVG and XML go out as
    /// `attachment`, and every response carries `nosniff` (#1849).
    #[must_use]
    pub fn user_content(mut self) -> Self {
        self.user_content = true;
        self
    }

    /// Override the `Cache-Control` header value.
    #[must_use]
    pub fn cache_control(mut self, v: impl Into<String>) -> Self {
        self.cache_control = v.into();
        self
    }

    /// Set `Cache-Control: public, max-age=<secs>, immutable`. Use it
    /// for hashed file names like `app.a3f8b2.js`, where new content
    /// always means a new URL.
    #[must_use]
    pub fn immutable(mut self, max_age: Duration) -> Self {
        self.cache_control = format!("public, max-age={}, immutable", max_age.as_secs());
        self
    }

    /// Allow files whose name starts with `.`. Off by default.
    #[must_use]
    pub fn serve_hidden(mut self, on: bool) -> Self {
        self.serve_hidden = on;
        self
    }

    /// Turn off the check that stops a symlink leading out of `root`.
    /// Only do this on a directory layout you have checked yourself.
    #[must_use]
    pub fn no_canonicalize(mut self) -> Self {
        self.canonicalize = false;
        self
    }
}

/// Warn when a plain static mount looks like it serves user uploads,
/// where HTML or SVG would run on the app origin (#1849).
#[cfg_attr(not(any(feature = "manage", feature = "tenancy")), allow(dead_code))]
pub(crate) fn warn_if_uploads_prefix(prefix: &str) {
    let p = prefix.to_ascii_lowercase();
    if p.contains("upload") || p.contains("media") {
        tracing::warn!(
            target: "rustango::static_files",
            prefix,
            "static mount looks like user uploads; mount it with `with_uploads` so HTML and SVG \
             download instead of running on this origin"
        );
    }
}

/// Router that serves everything under `files.root`. Mount it with
/// `.nest("/static", static_router(...))`.
#[must_use]
pub fn static_router(files: StaticFiles) -> Router {
    let root = if files.canonicalize {
        std::fs::canonicalize(&files.root).ok()
    } else {
        None
    };
    Router::new()
        .route("/{*path}", get(serve))
        .with_state(Arc::new(Mount {
            files,
            root: std::sync::RwLock::new(root.map(|r| (r, std::time::Instant::now()))),
        }))
}

/// A mount plus its canonical root, cached rather than resolved per request (#1531).
struct Mount {
    files: StaticFiles,
    root: std::sync::RwLock<Option<(PathBuf, std::time::Instant)>>,
}

/// How long a cached root is trusted before it is resolved again, so a
/// re-pointed root symlink cannot keep serving the old target's tree.
const ROOT_TTL: Duration = if cfg!(test) {
    Duration::from_millis(50)
} else {
    Duration::from_secs(1)
};

/// An open file, checked to be a regular file under the root.
struct Opened {
    path: PathBuf,
    file: std::fs::File,
    len: u64,
    mtime: Option<u64>,
}

/// Strong validator from size and mtime, so a `206` and its `200` share it.
/// No mtime, no validator: size alone can't tell two versions apart.
fn file_etag(len: u64, mtime: Option<u64>) -> Option<String> {
    mtime.map(|m| format!("\"{m:x}-{len:x}\""))
}

async fn serve(
    State(mount): State<Arc<Mount>>,
    PathExt(rel): PathExt<String>,
    headers: HeaderMap,
    method: Method,
) -> Response {
    // Every filesystem call runs off the async workers (#1531).
    let m = Arc::clone(&mount);
    let opened = match tokio::task::spawn_blocking(move || open_file(&m, &rel)).await {
        Ok(Some(o)) => o,
        Ok(None) | Err(_) => return not_found(),
    };
    let files = &mount.files;
    let Opened {
        path,
        file,
        len,
        mtime,
    } = opened;

    let etag = file_etag(len, mtime);
    let etag = etag.as_deref();
    // `If-None-Match` wins over `If-Modified-Since` (RFC 9110 §13.1.3).
    if let Some(inm) = headers.get(header::IF_NONE_MATCH) {
        let inm = inm.to_str().unwrap_or("");
        if etag.is_some_and(|e| crate::etag::if_none_match(inm, e)) {
            return not_modified(mtime, etag, &files.cache_control);
        }
    } else if let Some(secs) = mtime {
        if let Some(ims) = headers.get(header::IF_MODIFIED_SINCE) {
            if let Some(client_secs) = parse_http_date(ims.to_str().unwrap_or("")) {
                if secs <= client_secs {
                    return not_modified(mtime, etag, &files.cache_control);
                }
            }
        }
    }

    let last_modified = mtime.map(format_http_date);
    let range = match requested_range(&headers, etag, last_modified.as_deref(), len) {
        Ok(r) => r,
        Err(()) => return range_not_satisfiable(len),
    };
    let (start, count) = range.unwrap_or((0, len));

    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};
        let mut file = tokio::fs::File::from_std(file);
        if start > 0 && file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
            return not_found();
        }
        // Streamed, so memory per request is one read buffer, not the file.
        Body::new(KnownLen {
            inner: Body::from_stream(tokio_util::io::ReaderStream::new(file.take(count))),
            len: count,
        })
    };

    let mime = mime_for(&path);
    let status = if range.is_some() {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    let mut resp = Response::builder()
        .status(status)
        .body(body)
        .unwrap_or_else(|_| Response::new(Body::empty()));
    let h = resp.headers_mut();
    if files.user_content {
        h.insert(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        );
        if runs_script(&mime) {
            h.insert(
                header::CONTENT_DISPOSITION,
                HeaderValue::from_static("attachment"),
            );
        }
    }
    h.insert(header::CONTENT_TYPE, mime);
    if !files.cache_control.is_empty() {
        if let Ok(v) = HeaderValue::from_str(&files.cache_control) {
            h.insert(header::CACHE_CONTROL, v);
        }
    }
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(count));
    if range.is_some() {
        let cr = format!("bytes {start}-{}/{len}", start + count - 1);
        if let Ok(v) = HeaderValue::from_str(&cr) {
            h.insert(header::CONTENT_RANGE, v);
        }
    }
    if let Some(v) = last_modified.and_then(|s| HeaderValue::from_str(&s).ok()) {
        h.insert(header::LAST_MODIFIED, v);
    }
    if let Some(v) = etag.and_then(|e| HeaderValue::from_str(e).ok()) {
        h.insert(header::ETAG, v);
    }
    resp
}

/// A streamed body that still reports its exact size, so a size-aware
/// layer such as `CompressionLayer` treats it like a buffered one.
struct KnownLen {
    inner: Body,
    len: u64,
}

impl http_body::Body for KnownLen {
    type Data = axum::body::Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        std::pin::Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn size_hint(&self) -> http_body::SizeHint {
        http_body::SizeHint::with_exact(self.len)
    }
}

/// Resolve, open and stat `rel`. Blocking: call it from `spawn_blocking`.
fn open_file(mount: &Mount, rel: &str) -> Option<Opened> {
    let path = resolve_path(mount, rel)?;
    let file = std::fs::File::open(&path).ok()?;
    // Stat the open handle, so the checked file is the one served.
    let meta = file.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    Some(Opened {
        path,
        file,
        len: meta.len(),
        mtime,
    })
}

/// The single `Range: bytes=` span to serve, as `(start, count)`.
///
/// `Ok(None)` serves the whole file: no header, several ranges, a
/// stale `If-Range`, a malformed spec, or a unit we don't know. `Err`
/// is a 416: a valid range that starts past the end.
fn requested_range(
    headers: &HeaderMap,
    etag: Option<&str>,
    last_modified: Option<&str>,
    len: u64,
) -> Result<Option<(u64, u64)>, ()> {
    let Some(spec) = headers.get(header::RANGE).and_then(|v| v.to_str().ok()) else {
        return Ok(None);
    };
    if let Some(if_range) = headers.get(header::IF_RANGE) {
        let v = if_range.to_str().unwrap_or_default().trim();
        // An entity tag compares strongly, so a weak one never matches.
        let fresh = if v.starts_with('"') || v.starts_with("W/") {
            Some(v) == etag
        } else {
            Some(v) == last_modified
        };
        if !fresh {
            return Ok(None);
        }
    }
    let Some(spec) = spec.trim().strip_prefix("bytes=") else {
        return Ok(None);
    };
    if spec.contains(',') {
        return Ok(None);
    }
    let Some((a, b)) = spec.trim().split_once('-') else {
        return Ok(None);
    };
    let (start, end) = match (a.trim(), b.trim()) {
        ("", "") => return Ok(None),
        // `bytes=-N`: the last N bytes.
        ("", n) => {
            // Malformed specs are ignored, not refused (RFC 9110 §14.2).
            let Ok(n) = n.parse::<u64>() else {
                return Ok(None);
            };
            if n == 0 || len == 0 {
                return Err(());
            }
            (len.saturating_sub(n), len - 1)
        }
        (a, b) => {
            let (Ok(start), Ok(last)) = (
                a.parse::<u64>(),
                if b.is_empty() {
                    Ok(u64::MAX)
                } else {
                    b.parse::<u64>()
                },
            ) else {
                return Ok(None);
            };
            if last < start {
                return Ok(None);
            }
            if start >= len {
                return Err(());
            }
            (start, last.min(len - 1))
        }
    };
    Ok(Some((start, end - start + 1)))
}

fn range_not_satisfiable(len: u64) -> Response {
    Response::builder()
        .status(StatusCode::RANGE_NOT_SATISFIABLE)
        .header(header::CONTENT_RANGE, format!("bytes */{len}"))
        .body(Body::empty())
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

fn resolve_path(mount: &Mount, rel: &str) -> Option<PathBuf> {
    let files = &mount.files;
    // 1. An empty path is never valid.
    if rel.is_empty() {
        return None;
    }
    let rel_path = Path::new(rel);

    // 2. Normalise before joining: drop `.`, and reject `..`, a root or a
    //    drive prefix. This is what blocks path traversal.
    let mut normalized = PathBuf::new();
    for c in rel_path.components() {
        match c {
            Component::Normal(s) => {
                if !files.serve_hidden {
                    if let Some(name) = s.to_str() {
                        if name.starts_with('.') {
                            return None;
                        }
                    }
                }
                normalized.push(s);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) | Component::RootDir => return None,
        }
    }
    if normalized.as_os_str().is_empty() {
        return None;
    }

    let joined = files.root.join(&normalized);

    // 3. Resolve the real path, so a symlink cannot lead out of the root.
    if files.canonicalize {
        let canon = std::fs::canonicalize(&joined).ok()?;
        let cached = mount.root.read().ok().and_then(|r| r.clone());
        if cached.is_some_and(|(root, at)| at.elapsed() < ROOT_TTL && canon.starts_with(root)) {
            return Some(canon);
        }
        // Stale, missing, or a re-pointed root symlink: resolve it again.
        // A root that no longer resolves serves nothing.
        let root_canon = std::fs::canonicalize(&files.root).ok();
        if let Ok(mut w) = mount.root.write() {
            *w = root_canon.clone().map(|r| (r, std::time::Instant::now()));
        }
        return canon.starts_with(root_canon?).then_some(canon);
    }
    Some(joined)
}

fn not_found() -> Response {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from("404 Not Found"))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

fn not_modified(mtime: Option<u64>, etag: Option<&str>, cache_control: &str) -> Response {
    let mut resp = Response::builder()
        .status(StatusCode::NOT_MODIFIED)
        .body(Body::empty())
        .unwrap_or_else(|_| Response::new(Body::empty()));
    let h = resp.headers_mut();
    if !cache_control.is_empty() {
        if let Ok(v) = HeaderValue::from_str(cache_control) {
            h.insert(header::CACHE_CONTROL, v);
        }
    }
    if let Some(v) = mtime.and_then(|s| HeaderValue::from_str(&format_http_date(s)).ok()) {
        h.insert(header::LAST_MODIFIED, v);
    }
    if let Some(v) = etag.and_then(|e| HeaderValue::from_str(e).ok()) {
        h.insert(header::ETAG, v);
    }
    resp
}

/// MIME type for a file extension, or `application/octet-stream`.
fn mime_for(path: &Path) -> HeaderValue {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    let s: &'static str = match ext.as_deref() {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js" | "mjs") => "application/javascript; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("xml") => "application/xml; charset=utf-8",
        Some("txt" | "md") => "text/plain; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("avif") => "image/avif",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("otf") => "font/otf",
        Some("wasm") => "application/wasm",
        Some("pdf") => "application/pdf",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        Some("mp3") => "audio/mpeg",
        Some("ogg") => "audio/ogg",
        Some("zip") => "application/zip",
        Some("map") => "application/json; charset=utf-8",
        _ => "application/octet-stream",
    };
    HeaderValue::from_static(s)
}

/// A type a browser renders with script when opened directly.
fn runs_script(mime: &HeaderValue) -> bool {
    let m = mime.to_str().unwrap_or_default();
    ["text/html", "image/svg+xml", "application/xml"]
        .iter()
        .any(|t| m.starts_with(t))
}

/// Format unix seconds as an IMF-fixdate (RFC 7231), the HTTP date
/// format.
fn format_http_date(secs: u64) -> String {
    let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(i64::try_from(secs).unwrap_or(0), 0)
        .unwrap_or_else(chrono::Utc::now);
    // chrono's RFC 2822 output ends in "+0000"; HTTP wants "GMT". Same
    // shape otherwise, so write the format out by hand.
    dt.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

/// Parse an IMF-fixdate back into unix seconds. Any other format
/// gives `None`.
fn parse_http_date(s: &str) -> Option<u64> {
    let dt = chrono::DateTime::parse_from_rfc2822(s)
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(s, "%a, %d %b %Y %H:%M:%S GMT").map(|n| {
                chrono::DateTime::from_naive_utc_and_offset(
                    n,
                    chrono::FixedOffset::east_opt(0).unwrap(),
                )
            })
        })
        .ok()?;
    let ts = dt.timestamp();
    if ts < 0 {
        None
    } else {
        Some(u64::try_from(ts).ok()?)
    }
}

// Keeps the `SystemTime` import used, so `-D warnings` stays happy.
const _: fn() = || {
    let _ = SystemTime::UNIX_EPOCH;
};

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use std::io::Write;
    use tempfile::TempDir;
    use tower::ServiceExt;

    fn write_file(dir: &TempDir, rel: &str, body: &[u8]) -> PathBuf {
        let p = dir.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body).unwrap();
        p
    }

    fn server(dir: &TempDir) -> Router {
        static_router(StaticFiles::new(dir.path().to_path_buf()))
    }

    #[tokio::test]
    async fn serves_existing_file_with_correct_content_type() {
        let dir = TempDir::new().unwrap();
        write_file(&dir, "style.css", b"body{}");

        let resp = server(&dir)
            .oneshot(
                Request::builder()
                    .uri("/style.css")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers()
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap(),
            "text/css; charset=utf-8"
        );
        assert!(resp.headers().get(header::CACHE_CONTROL).is_some());
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"body{}");
    }

    #[tokio::test]
    async fn returns_404_for_missing_file() {
        let dir = TempDir::new().unwrap();
        let resp = server(&dir)
            .oneshot(
                Request::builder()
                    .uri("/missing.txt")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn rejects_parent_dir_traversal() {
        let dir = TempDir::new().unwrap();
        let resp = server(&dir)
            .oneshot(
                Request::builder()
                    .uri("/../etc/passwd")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn rejects_hidden_files_by_default() {
        let dir = TempDir::new().unwrap();
        write_file(&dir, ".env", b"SECRET=hunter2");
        let resp = server(&dir)
            .oneshot(Request::builder().uri("/.env").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn serves_hidden_when_opted_in() {
        let dir = TempDir::new().unwrap();
        write_file(&dir, ".well-known/x.txt", b"hi");
        let app = static_router(StaticFiles::new(dir.path().to_path_buf()).serve_hidden(true));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/.well-known/x.txt")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn directory_path_returns_404() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        let resp = server(&dir)
            .oneshot(Request::builder().uri("/sub").body(Body::empty()).unwrap())
            .await
            .unwrap();
        // Directory listing is not supported, so 404.
        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn if_modified_since_returns_304_when_unchanged() {
        let dir = TempDir::new().unwrap();
        write_file(&dir, "robots.txt", b"User-agent: *\n");

        // First request: 200 + Last-Modified
        let r1 = server(&dir)
            .oneshot(
                Request::builder()
                    .uri("/robots.txt")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r1.status(), 200);
        let last_modified = r1
            .headers()
            .get(header::LAST_MODIFIED)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();

        // Sending that value back as If-Modified-Since gives a 304.
        let r2 = server(&dir)
            .oneshot(
                Request::builder()
                    .uri("/robots.txt")
                    .header(header::IF_MODIFIED_SINCE, &last_modified)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r2.status(), StatusCode::NOT_MODIFIED);
        let bytes = axum::body::to_bytes(r2.into_body(), 1 << 16).await.unwrap();
        assert!(bytes.is_empty());
    }

    #[tokio::test]
    async fn head_request_returns_headers_no_body() {
        let dir = TempDir::new().unwrap();
        write_file(&dir, "x.html", b"<html></html>");
        let resp = server(&dir)
            .oneshot(
                Request::builder()
                    .method(Method::HEAD)
                    .uri("/x.html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert!(resp.headers().get(header::CONTENT_LENGTH).is_some());
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
            .await
            .unwrap();
        assert!(bytes.is_empty(), "HEAD must not return a body");
    }

    #[tokio::test]
    async fn immutable_helper_sets_correct_cache_control() {
        let dir = TempDir::new().unwrap();
        write_file(&dir, "app.a3f8b2.js", b"console.log(1)");
        let app = static_router(
            StaticFiles::new(dir.path().to_path_buf())
                .immutable(Duration::from_secs(60 * 60 * 24 * 365)),
        );
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/app.a3f8b2.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let cc = resp
            .headers()
            .get(header::CACHE_CONTROL)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(cc.contains("immutable"));
        assert!(cc.contains("max-age=31536000"));
    }

    #[test]
    fn mime_for_known_extensions() {
        assert_eq!(
            mime_for(Path::new("a.css")).to_str().unwrap(),
            "text/css; charset=utf-8"
        );
        assert_eq!(
            mime_for(Path::new("a.svg")).to_str().unwrap(),
            "image/svg+xml"
        );
        assert_eq!(
            mime_for(Path::new("a.unknown")).to_str().unwrap(),
            "application/octet-stream"
        );
    }

    async fn get(app: Router, uri: &str) -> Response {
        let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
        app.oneshot(req).await.unwrap()
    }

    /// Uploaded HTML/SVG/XML must not run on the app origin (#1849).
    #[tokio::test]
    async fn user_content_downloads_active_types() {
        let dir = TempDir::new().unwrap();
        for f in ["x.html", "x.HTM", "x.svg", "x.xml", "x.png"] {
            write_file(&dir, f, b"<script>alert(1)</script>");
        }
        let app = static_router(StaticFiles::new(dir.path()).user_content());
        for f in ["x.html", "x.HTM", "x.svg", "x.xml"] {
            let r = get(app.clone(), &format!("/{f}")).await;
            assert_eq!(
                r.headers()[header::CONTENT_DISPOSITION],
                "attachment",
                "{f}"
            );
            assert_eq!(r.headers()[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        }
        let r = get(app, "/x.png").await;
        assert!(r.headers().get(header::CONTENT_DISPOSITION).is_none());
        assert_eq!(r.headers()[header::X_CONTENT_TYPE_OPTIONS], "nosniff");

        // Plain static mounts keep serving HTML inline.
        let r = get(server(&dir), "/x.html").await;
        assert!(r.headers().get(header::CONTENT_DISPOSITION).is_none());
    }

    /// What `warn_if_uploads_prefix(prefix)` logs.
    #[cfg(feature = "runtime")]
    fn prefix_logs(prefix: &str) -> String {
        let out = crate::testkit::CaptureWriter::default();
        let sub = tracing_subscriber::fmt()
            .with_writer(out.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(sub, || warn_if_uploads_prefix(prefix));
        out.contents()
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn upload_like_static_prefixes_warn() {
        for p in ["/uploads", "/Media", "/user-uploads/"] {
            assert!(prefix_logs(p).contains("with_uploads"), "{p}");
        }
        assert_eq!(prefix_logs("/static"), "");
    }

    /// #1531 — the body streams in chunks instead of one whole-file buffer.
    #[tokio::test]
    async fn large_file_streams_in_chunks() {
        use http_body_util::BodyExt as _;
        let dir = TempDir::new().unwrap();
        let big = vec![7u8; 1 << 20];
        write_file(&dir, "big.bin", &big);
        let mut body = get(server(&dir), "/big.bin").await.into_body();
        // Exact, so `CompressionLayer` still compresses a streamed asset.
        assert_eq!(http_body::Body::size_hint(&body).exact(), Some(1 << 20));
        let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
        assert!(first.len() < big.len(), "first frame is the whole file");
        let rest = body.collect().await.unwrap().to_bytes();
        assert_eq!(first.len() + rest.len(), big.len());
    }

    /// The cached root follows a re-pointed root symlink (release swaps).
    #[cfg(unix)]
    #[tokio::test]
    async fn root_symlink_swap_keeps_serving() {
        let dir = TempDir::new().unwrap();
        write_file(&dir, "a/x.txt", b"a");
        write_file(&dir, "b/x.txt", b"b");
        let cur = dir.path().join("current");
        std::os::unix::fs::symlink(dir.path().join("a"), &cur).unwrap();
        let app = static_router(StaticFiles::new(&cur));
        assert_eq!(get(app.clone(), "/x.txt").await.status(), 200);
        std::fs::remove_file(&cur).unwrap();
        std::os::unix::fs::symlink(dir.path().join("b"), &cur).unwrap();
        let r = get(app, "/x.txt").await;
        assert_eq!(r.status(), 200);
        assert_eq!(
            &axum::body::to_bytes(r.into_body(), 8).await.unwrap()[..],
            b"b"
        );
    }

    async fn ranged(app: Router, range: &str) -> Response {
        let req = Request::builder()
            .uri("/r.txt")
            .header(header::RANGE, range)
            .body(Body::empty())
            .unwrap();
        app.oneshot(req).await.unwrap()
    }

    /// #1531 — single byte ranges get a 206; bad ones a 416.
    #[tokio::test]
    async fn range_requests_are_honoured() {
        let dir = TempDir::new().unwrap();
        write_file(&dir, "r.txt", b"0123456789");
        let full = get(server(&dir), "/r.txt").await;
        assert_eq!(full.headers()[header::ACCEPT_RANGES], "bytes");
        for (spec, want, cr) in [
            ("bytes=2-4", &b"234"[..], "bytes 2-4/10"),
            ("bytes=7-", b"789", "bytes 7-9/10"),
            ("bytes=-3", b"789", "bytes 7-9/10"),
            ("bytes=8-99", b"89", "bytes 8-9/10"),
        ] {
            let r = ranged(server(&dir), spec).await;
            assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT, "{spec}");
            assert_eq!(r.headers()[header::CONTENT_RANGE], cr, "{spec}");
            assert_eq!(r.headers()[header::CONTENT_LENGTH], want.len().to_string());
            let b = axum::body::to_bytes(r.into_body(), 64).await.unwrap();
            assert_eq!(&b[..], want, "{spec}");
        }
        let r = ranged(server(&dir), "bytes=10-").await;
        assert_eq!(r.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(r.headers()[header::CONTENT_RANGE], "bytes */10");
        // Several ranges: the whole file is a valid answer.
        assert_eq!(ranged(server(&dir), "bytes=0-1,4-5").await.status(), 200);
        // Malformed specs are ignored, not refused (RFC 9110 §14.2).
        for spec in ["bytes=abc-", "bytes=5-3", "bytes=-x", "bytes=1-y"] {
            assert_eq!(ranged(server(&dir), spec).await.status(), 200, "{spec}");
        }
    }

    async fn if_range(app: Router, validator: &str) -> Response {
        let req = Request::builder()
            .uri("/r.txt")
            .header(header::RANGE, "bytes=2-4")
            .header(header::IF_RANGE, validator)
            .body(Body::empty())
            .unwrap();
        app.oneshot(req).await.unwrap()
    }

    /// `If-Range` resumes on the file's strong ETag or its Last-Modified,
    /// also behind `EtagLayer`, and a changed validator gets the whole file.
    #[tokio::test]
    async fn if_range_matches_etag_or_last_modified() {
        use crate::etag::{EtagLayer, EtagRouterExt as _};
        let dir = TempDir::new().unwrap();
        write_file(&dir, "r.txt", b"0123456789");
        let app = server(&dir).etag(EtagLayer::new());
        let full = get(app.clone(), "/r.txt").await;
        let etag = full.headers()[header::ETAG].to_str().unwrap().to_owned();
        let lm = full.headers()[header::LAST_MODIFIED]
            .to_str()
            .unwrap()
            .to_owned();

        let r = if_range(app.clone(), &etag).await;
        assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
        // The 206 names the whole file, not a hash of the slice.
        assert_eq!(r.headers()[header::ETAG], etag.as_str());
        assert_eq!(if_range(app.clone(), &lm).await.status(), 206);

        let weak = format!("W/{etag}");
        for stale in ["\"other\"", weak.as_str(), "Thu, 01 Jan 1970 00:00:00 GMT"] {
            let r = if_range(app.clone(), stale).await;
            assert_eq!(r.status(), StatusCode::OK, "{stale}");
            assert_eq!(r.headers()[header::CONTENT_LENGTH], "10");
        }

        let req = Request::builder()
            .uri("/r.txt")
            .header(header::IF_NONE_MATCH, &etag)
            .body(Body::empty())
            .unwrap();
        assert_eq!(app.oneshot(req).await.unwrap().status(), 304);
    }

    /// The cached root expires, so a root re-pointed to a subdirectory
    /// stops serving symlinks into the old target.
    #[cfg(unix)]
    #[tokio::test]
    async fn repointed_root_drops_the_old_tree() {
        let dir = TempDir::new().unwrap();
        write_file(&dir, "a/secret.txt", b"s");
        write_file(&dir, "a/pub/x.txt", b"x");
        std::os::unix::fs::symlink(
            dir.path().join("a/secret.txt"),
            dir.path().join("a/pub/leak"),
        )
        .unwrap();
        let cur = dir.path().join("current");
        std::os::unix::fs::symlink(dir.path().join("a"), &cur).unwrap();
        let app = static_router(StaticFiles::new(&cur));
        assert_eq!(get(app.clone(), "/secret.txt").await.status(), 200);
        std::fs::remove_file(&cur).unwrap();
        std::os::unix::fs::symlink(dir.path().join("a/pub"), &cur).unwrap();
        tokio::time::sleep(ROOT_TTL * 2).await;
        assert_eq!(get(app.clone(), "/x.txt").await.status(), 200);
        assert_eq!(get(app, "/leak").await.status(), 404);
    }

    #[test]
    fn http_date_round_trips() {
        let secs = 1_714_651_200u64; // 2024-05-02T12:00:00Z
        let s = format_http_date(secs);
        let parsed = parse_http_date(&s).unwrap();
        assert_eq!(parsed, secs);
    }

    #[test]
    fn http_date_parses_rfc2822_form() {
        // chrono RFC 2822 → numeric offset, not "GMT".
        let parsed = parse_http_date("Thu, 02 May 2024 12:00:00 +0000").unwrap();
        assert_eq!(parsed, 1_714_651_200);
    }
}
