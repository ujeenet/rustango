//! Static file serving: read files from a directory and send them with
//! `Content-Type`, `Cache-Control` and `Last-Modified`, and answer 304
//! to `If-Modified-Since`.
//!
//! Add [`crate::etag::EtagLayer`] to revalidate on a body hash as well
//! as on the timestamp.
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
//!
//! ## Caching
//!
//! `Cache-Control` defaults to `public, max-age=3600`; change it with
//! [`StaticFiles::cache_control`]. `Last-Modified` comes from the
//! file's mtime, and a matching `If-Modified-Since` gets a 304 with no
//! body.
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
        }
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

/// Router that serves everything under `files.root`. Mount it with
/// `.nest("/static", static_router(...))`.
#[must_use]
pub fn static_router(files: StaticFiles) -> Router {
    Router::new()
        .route("/{*path}", get(serve))
        .with_state(Arc::new(files))
}

async fn serve(
    State(files): State<Arc<StaticFiles>>,
    PathExt(rel): PathExt<String>,
    headers: HeaderMap,
    method: Method,
) -> Response {
    let resolved = match resolve_path(&files, &rel) {
        Some(p) => p,
        None => return not_found(),
    };

    let meta = match std::fs::metadata(&resolved) {
        Ok(m) if m.is_file() => m,
        Ok(_) | Err(_) => return not_found(),
    };

    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs());

    if let Some(secs) = mtime {
        if let Some(ims) = headers.get(header::IF_MODIFIED_SINCE) {
            if let Some(client_secs) = parse_http_date(ims.to_str().unwrap_or("")) {
                if secs <= client_secs {
                    return not_modified(secs, &files.cache_control);
                }
            }
        }
    }

    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        match std::fs::read(&resolved) {
            Ok(bytes) => Body::from(bytes),
            Err(_) => return not_found(),
        }
    };

    let mime = mime_for(&resolved);
    let mut resp = Response::builder()
        .status(StatusCode::OK)
        .body(body)
        .unwrap_or_else(|_| Response::new(Body::empty()));
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, mime);
    if !files.cache_control.is_empty() {
        if let Ok(v) = HeaderValue::from_str(&files.cache_control) {
            h.insert(header::CACHE_CONTROL, v);
        }
    }
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(meta.len()));
    if let Some(secs) = mtime {
        if let Ok(v) = HeaderValue::from_str(&format_http_date(secs)) {
            h.insert(header::LAST_MODIFIED, v);
        }
    }
    resp
}

fn resolve_path(files: &StaticFiles, rel: &str) -> Option<PathBuf> {
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
        let root_canon = std::fs::canonicalize(&files.root).ok()?;
        if !canon.starts_with(&root_canon) {
            return None;
        }
        return Some(canon);
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

fn not_modified(mtime_secs: u64, cache_control: &str) -> Response {
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
    if let Ok(v) = HeaderValue::from_str(&format_http_date(mtime_secs)) {
        h.insert(header::LAST_MODIFIED, v);
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
