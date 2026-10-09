//! [`Storage`] over any S3-compatible API: AWS S3, Cloudflare R2,
//! Backblaze B2, MinIO and the rest. Requests are plain HTTP, signed
//! with AWS Signature V4.
//!
//! The signing is written here rather than pulled from `aws-sdk-s3`,
//! whose dependency tree is large. It uses `reqwest`, `hmac`, `sha2`
//! and `base64`, which other features already bring in.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::storage::s3::{S3Storage, S3Config};
//! use std::sync::Arc;
//!
//! // AWS S3
//! let s3 = S3Storage::new(S3Config {
//!     bucket: "my-bucket".into(),
//!     region: "us-east-1".into(),
//!     endpoint: None,                 // None -> https://s3.<region>.amazonaws.com
//!     access_key_id: std::env::var("AWS_ACCESS_KEY_ID").unwrap(),
//!     secret_access_key: std::env::var("AWS_SECRET_ACCESS_KEY").unwrap(),
//!     path_style: false,              // virtual-hosted (default for AWS)
//! });
//!
//! // Cloudflare R2
//! let r2 = S3Storage::new(S3Config {
//!     bucket: "my-bucket".into(),
//!     region: "auto".into(),
//!     endpoint: Some("https://<account>.r2.cloudflarestorage.com".into()),
//!     access_key_id: std::env::var("R2_KEY").unwrap(),
//!     secret_access_key: std::env::var("R2_SECRET").unwrap(),
//!     path_style: true,
//! });
//!
//! let storage: rustango::storage::BoxedStorage = Arc::new(s3);
//! storage.save("avatars/alice.png", &png_bytes).await?;
//! ```
//!
//! ## What it does
//!
//! - PUT, GET, DELETE and HEAD, signed with SigV4.
//! - Both URL styles: virtual-hosted, which AWS uses, and path style,
//!   which R2 and MinIO use.
//! - `url(key)` gives a stable public URL when `endpoint` is set, or
//!   when the bucket is public.
//! - Presigned GET and PUT URLs, so a browser can download a private
//!   file or upload straight to the bucket.
//!
//! ## What it does not do
//!
//! - Multipart upload. A single PUT covers files up to about 5 GiB.
//! - Creating or listing buckets.
//! - Server-side encryption with your own keys (SSE-C).

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;

use super::{validate_key, ObjectMeta, PutConditions, Storage, StorageError};

/// How to reach the bucket.
#[derive(Clone, Debug)]
pub struct S3Config {
    pub bucket: String,
    pub region: String,
    /// `None` means AWS S3 at `https://s3.<region>.amazonaws.com`.
    /// Set it for any other provider.
    pub endpoint: Option<String>,
    pub access_key_id: String,
    pub secret_access_key: String,
    /// `false` puts the bucket in the host:
    /// `https://<bucket>.s3.<region>.amazonaws.com/<key>`, or
    /// `https://<bucket>.<endpoint-host>/<key>` with an `endpoint`. `true`
    /// puts it in the path: `https://<endpoint>/<bucket>/<key>`.
    /// AWS wants the first, R2 and MinIO the second.
    pub path_style: bool,
}

/// Storage backed by an S3-compatible bucket.
pub struct S3Storage {
    cfg: S3Config,
    http: Http,
    /// The SigV4 key for one date stamp. It depends only on the secret,
    /// date, region and service, so a page of presigns derives it once (#1570).
    signing_key: std::sync::Mutex<Option<(String, Arc<[u8]>)>>,
    #[cfg(test)]
    derivations: std::sync::atomic::AtomicUsize,
}

/// Default client timeouts; [`S3Storage::with_http`] replaces them.
const DEFAULT_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// Waiting for a reply or the next body chunk; a total cap would fail big objects.
const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
/// The slowest upload rate the default PUT timeout allows, in bytes per second.
const MIN_UPLOAD_RATE: u64 = 256 * 1024;

/// The HTTP clients. Only the defaults get a size-based upload timeout.
enum Http {
    /// reqwest's read timeout runs through an upload, so PUT uses a client without one.
    Default {
        http: reqwest::Client,
        upload: reqwest::Client,
        read_timeout: std::time::Duration,
    },
    /// From [`S3Storage::with_http`]: its own timeouts apply to every request.
    Custom(reqwest::Client),
}

impl Http {
    fn defaults() -> Self {
        Self::with_read_timeout(DEFAULT_READ_TIMEOUT)
    }

    fn with_read_timeout(read_timeout: std::time::Duration) -> Self {
        // A redirect could take signed requests to a host the config never named (#1780).
        let base = || {
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(DEFAULT_CONNECT_TIMEOUT)
        };
        // A stalled endpoint must fail, not hang the request holding it (#2220).
        Self::Default {
            http: base()
                .read_timeout(read_timeout)
                .build()
                .expect("reqwest client builds"),
            upload: base().build().expect("reqwest client builds"),
            read_timeout,
        }
    }

    fn request(&self, method: reqwest::Method, url: &str, len: usize) -> reqwest::RequestBuilder {
        match self {
            Self::Default {
                upload,
                read_timeout,
                ..
            } if method == reqwest::Method::PUT => upload
                .request(method, url)
                .timeout(upload_timeout(*read_timeout, len)),
            Self::Default { http, .. } | Self::Custom(http) => http.request(method, url),
        }
    }
}

/// The read timeout plus the time to send `len` bytes at [`MIN_UPLOAD_RATE`].
fn upload_timeout(read_timeout: std::time::Duration, len: usize) -> std::time::Duration {
    let len = u64::try_from(len).unwrap_or(u64::MAX);
    read_timeout + std::time::Duration::from_millis(len.saturating_mul(1000) / MIN_UPLOAD_RATE)
}

/// The endpoint without its `http(s)://` scheme.
fn strip_scheme(endpoint: &str) -> &str {
    endpoint
        .strip_prefix("https://")
        .or_else(|| endpoint.strip_prefix("http://"))
        .unwrap_or(endpoint)
}

impl S3Storage {
    #[must_use]
    pub fn new(cfg: S3Config) -> Self {
        Self {
            cfg,
            http: Http::defaults(),
            signing_key: std::sync::Mutex::new(None),
            #[cfg(test)]
            derivations: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// The signing key for `date_stamp`, derived once per date.
    fn signing_key(&self, date_stamp: &str) -> Arc<[u8]> {
        let mut slot = self
            .signing_key
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((date, key)) = slot.as_ref() {
            if date == date_stamp {
                return key.clone();
            }
        }
        #[cfg(test)]
        self.derivations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let key: Arc<[u8]> = derive_signing_key(
            &self.cfg.secret_access_key,
            date_stamp,
            &self.cfg.region,
            "s3",
        )
        .into();
        *slot = Some((date_stamp.to_owned(), key.clone()));
        key
    }

    /// Use your own reqwest client, for custom timeouts, proxies or
    /// TLS roots. Turn its redirects off, as the default client does.
    ///
    /// The default gives up after 10 s to connect, and on GET, HEAD and
    /// DELETE after 60 s without a response or a body chunk. An upload
    /// gets 60 s + size / 256 KiB/s in total. Your client's own timeouts
    /// replace all of these.
    #[must_use]
    pub fn with_http(mut self, http: reqwest::Client) -> Self {
        self.http = Http::Custom(http);
        self
    }

    /// Just `host[:port]`, never a path.
    ///
    /// This goes into the `Host` header and into the signed `host:`
    /// canonical header, so a path here would make both wrong. Some
    /// endpoints do carry one, such as Supabase Storage at
    /// `https://<ref>.storage.supabase.co/storage/v1/s3`; a proxy
    /// rejects such a `Host` with a 400 before S3 ever sees it.
    fn host(&self) -> String {
        if let Some(ep) = &self.cfg.endpoint {
            let no_scheme = strip_scheme(ep);
            let authority = no_scheme.split('/').next().unwrap_or(no_scheme);
            let bucket = &self.cfg.bucket;
            // Virtual-hosted on a custom endpoint: the bucket goes in the
            // host, unless the endpoint already names it (#1904).
            let named =
                authority == bucket.as_str() || authority.starts_with(&format!("{bucket}."));
            if self.cfg.path_style || named {
                authority.to_owned()
            } else {
                format!("{bucket}.{authority}")
            }
        } else if self.cfg.path_style {
            format!("s3.{}.amazonaws.com", self.cfg.region)
        } else {
            format!("{}.s3.{}.amazonaws.com", self.cfg.bucket, self.cfg.region)
        }
    }

    fn scheme(&self) -> &'static str {
        if let Some(ep) = &self.cfg.endpoint {
            if ep.starts_with("http://") {
                return "http";
            }
        }
        "https"
    }

    /// The path prefix a custom endpoint carries, such as
    /// `/storage/v1/s3` for Supabase Storage. Empty for AWS, R2 and
    /// MinIO, which are bare hosts. It must go into both the request
    /// target and the signed canonical path, or the signature would
    /// cover a different resource than the one asked for.
    fn endpoint_prefix(&self) -> String {
        let Some(ep) = &self.cfg.endpoint else {
            return String::new();
        };
        let no_scheme = strip_scheme(ep).trim_end_matches('/');
        match no_scheme.find('/') {
            Some(i) => no_scheme[i..].to_owned(),
            None => String::new(),
        }
    }

    /// The URL path for a key: `<prefix>/key` when the bucket is in
    /// the host, `<prefix>/bucket/key` when it is in the path.
    fn key_path(&self, key: &str) -> String {
        let prefix = self.endpoint_prefix();
        if self.cfg.path_style {
            format!("{prefix}/{}/{}", self.cfg.bucket, encode_key(key))
        } else {
            format!("{prefix}/{}", encode_key(key))
        }
    }

    fn full_url(&self, key: &str) -> String {
        format!("{}://{}{}", self.scheme(), self.host(), self.key_path(key))
    }

    async fn signed_request(
        &self,
        method: &str,
        key: &str,
        body: &[u8],
        content_type: Option<&str>,
    ) -> Result<reqwest::Response, StorageError> {
        validate_key(key)?;
        let url = self.full_url(key);
        let path = self.key_path(key);
        let host = self.host();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| StorageError::Io(format!("clock: {e}")))?
            .as_secs();
        let amz_date = format_amz_date(now);
        let date_stamp = &amz_date[..8];
        let payload_hash = sha256_hex(body);

        // Canonical headers: lowercase, sorted by name. A content type
        // is signed too, so S3 stores the one we sent (#1904).
        let content_type = canonical_header_value(content_type)?;
        let content_type = content_type.as_deref();
        let ct_line = content_type.map_or(String::new(), |c| format!("content-type:{c}\n"));
        let canonical_headers = format!(
            "{ct_line}host:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{amz_date}\n"
        );
        let signed_headers = if content_type.is_some() {
            "content-type;host;x-amz-content-sha256;x-amz-date"
        } else {
            "host;x-amz-content-sha256;x-amz-date"
        };

        let canonical_request =
            format!("{method}\n{path}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}");
        let cr_hash = sha256_hex(canonical_request.as_bytes());

        let scope = format!("{date_stamp}/{}/s3/aws4_request", self.cfg.region);
        let string_to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{cr_hash}");

        let signing_key = self.signing_key(date_stamp);
        let signature = hex_encode(&hmac_sha256(&signing_key, string_to_sign.as_bytes()));

        let auth = format!(
            "AWS4-HMAC-SHA256 Credential={key_id}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            key_id = self.cfg.access_key_id,
        );

        let mut req = self
            .http
            .request(
                reqwest::Method::from_bytes(method.as_bytes())
                    .map_err(|e| StorageError::Io(format!("method: {e}")))?,
                &url,
                body.len(),
            )
            .header("host", &host)
            .header("x-amz-content-sha256", &payload_hash)
            .header("x-amz-date", &amz_date)
            .header("authorization", auth);
        if let Some(ct) = content_type {
            req = req.header("content-type", ct);
        }
        if !body.is_empty() {
            req = req.body(body.to_vec());
        }
        req.send()
            .await
            .map_err(|e| StorageError::Io(format!("http: {e}")))
    }

    /// Build a URL whose SigV4 signature lives in the query string.
    /// Used for both presigned GET and presigned PUT.
    ///
    /// The payload hash is the literal `UNSIGNED-PAYLOAD`, since the
    /// URL exists before the body does. For a PUT with a content
    /// type, that header is signed too, so the browser has to send a
    /// matching value.
    fn build_presigned_url(
        &self,
        method: &str,
        key: &str,
        ttl_secs: u64,
        put: &PutConditions,
    ) -> Result<String, StorageError> {
        validate_key(key)?;
        // AWS caps the lifetime at 7 days.
        let expires = ttl_secs.min(604_800).max(1);
        let path = self.key_path(key);
        let host = self.host();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| StorageError::Io(format!("clock: {e}")))?
            .as_secs();
        let amz_date = format_amz_date(now);
        let date_stamp = &amz_date[..8];
        let scope = format!("{date_stamp}/{}/s3/aws4_request", self.cfg.region);
        let credential = format!("{}/{scope}", self.cfg.access_key_id);
        let content_type = canonical_header_value(put.content_type.as_deref())?;

        // Always sign `host`. A PUT signs its content type and length
        // too, so S3 refuses a body of another size or type (#1851).
        let mut signed: Vec<(&str, String)> = vec![("host", host.clone())];
        if method == "PUT" {
            if let Some(ct) = content_type {
                signed.push(("content-type", ct));
            }
            if let Some(len) = put.content_length {
                signed.push(("content-length", len.to_string()));
            }
            // S3 and MinIO answer 412 when the key exists, so a replayed
            // URL cannot swap a finalized object.
            if put.create_only {
                signed.push(("if-none-match", "*".to_owned()));
            }
        }
        signed.sort_by(|a, b| a.0.cmp(b.0));
        let signed_headers = signed.iter().map(|(k, _)| *k).collect::<Vec<_>>().join(";");

        // The canonical query string sorts these by name.
        let mut query: Vec<(String, String)> = vec![
            ("X-Amz-Algorithm".into(), "AWS4-HMAC-SHA256".into()),
            ("X-Amz-Credential".into(), credential.clone()),
            ("X-Amz-Date".into(), amz_date.clone()),
            ("X-Amz-Expires".into(), expires.to_string()),
            ("X-Amz-SignedHeaders".into(), signed_headers.clone()),
        ];
        // Sort, then render. Values follow the same escaping rules as
        // `encode_key`, except that slashes are escaped too.
        query.sort_by(|a, b| a.0.cmp(&b.0));
        let canonical_query = query
            .iter()
            .map(|(k, v)| format!("{}={}", encode_query(k), encode_query(v)))
            .collect::<Vec<_>>()
            .join("&");

        let canonical_headers: String = signed.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();

        let payload_hash = "UNSIGNED-PAYLOAD";

        let canonical_request = format!(
            "{method}\n{path}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
        );
        let cr_hash = sha256_hex(canonical_request.as_bytes());
        let string_to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{cr_hash}");

        let signing_key = self.signing_key(date_stamp);
        let signature = hex_encode(&hmac_sha256(&signing_key, string_to_sign.as_bytes()));

        Ok(format!(
            "{}://{}{}?{canonical_query}&X-Amz-Signature={signature}",
            self.scheme(),
            host,
            path,
        ))
    }
}

#[async_trait]
impl Storage for S3Storage {
    async fn save(&self, key: &str, data: &[u8]) -> Result<(), StorageError> {
        self.save_with_content_type(key, data, None).await
    }

    async fn save_with_content_type(
        &self,
        key: &str,
        data: &[u8],
        content_type: Option<&str>,
    ) -> Result<(), StorageError> {
        let resp = self.signed_request("PUT", key, data, content_type).await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(StorageError::Io(format!(
                "S3 PUT {key} -> {status}: {body}"
            )));
        }
        Ok(())
    }

    async fn load(&self, key: &str) -> Result<Vec<u8>, StorageError> {
        let resp = self.signed_request("GET", key, b"", None).await?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(StorageError::NotFound(key.to_owned()));
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(StorageError::Io(format!(
                "S3 GET {key} -> {status}: {body}"
            )));
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| StorageError::Io(format!("read body: {e}")))?;
        Ok(bytes.to_vec())
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        let resp = self.signed_request("DELETE", key, b"", None).await?;
        let status = resp.status();
        // S3 returns 204 on success. Treat 404 as done, as
        // LocalStorage does.
        if status.is_success() || status == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        let body = resp.text().await.unwrap_or_default();
        Err(StorageError::Io(format!(
            "S3 DELETE {key} -> {status}: {body}"
        )))
    }

    async fn exists(&self, key: &str) -> Result<bool, StorageError> {
        let resp = self.signed_request("HEAD", key, b"", None).await?;
        let status = resp.status();
        // Only a 404 means absent; a 403 or 503 must not read as "gone" (#1904).
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(false);
        }
        if status.is_success() {
            return Ok(true);
        }
        Err(StorageError::Io(format!("S3 HEAD {key} -> {status}")))
    }

    fn url(&self, key: &str) -> Option<String> {
        Some(self.full_url(key))
    }

    async fn presigned_get_url(&self, key: &str, ttl: std::time::Duration) -> Option<String> {
        self.build_presigned_url("GET", key, ttl.as_secs(), &PutConditions::default())
            .ok()
    }

    async fn presigned_put_url(
        &self,
        key: &str,
        ttl: std::time::Duration,
        put: &PutConditions,
    ) -> Option<String> {
        self.build_presigned_url("PUT", key, ttl.as_secs(), put)
            .ok()
    }

    /// Without `s3:ListBucket`, S3 answers a missing key with 403, not
    /// 404; that stays an error, so a finalize fails closed.
    async fn metadata(&self, key: &str) -> Result<Option<ObjectMeta>, StorageError> {
        let resp = self.signed_request("HEAD", key, b"", None).await?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(StorageError::Io(format!("S3 HEAD {key} -> {status}")));
        }
        // The header, not `content_length()`: a HEAD body is empty.
        let header = |name| {
            resp.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        let size = header(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.trim().parse::<u64>().ok())
            .ok_or_else(|| StorageError::Io(format!("S3 HEAD {key}: no Content-Length")))?;
        Ok(Some(ObjectMeta::new(
            size,
            header(reqwest::header::CONTENT_TYPE),
        )))
    }
}

// =====================================================================
// SigV4 primitives
// =====================================================================

/// A header value as SigV4 signs it: trimmed, inner runs of spaces
/// collapsed to one. `None` when empty; control characters are refused.
fn canonical_header_value(v: Option<&str>) -> Result<Option<String>, StorageError> {
    let Some(v) = v else { return Ok(None) };
    if v.chars().any(|c| c.is_control() && c != '\t') {
        return Err(StorageError::Io(format!(
            "content type {v:?} has a control character"
        )));
    }
    let v = v
        .split([' ', '\t'])
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    Ok((!v.is_empty()).then_some(v))
}

/// Format unix seconds as `YYYYMMDDTHHMMSSZ`, which SigV4 wants.
fn format_amz_date(unix_secs: u64) -> String {
    let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(unix_secs as i64, 0)
        .unwrap_or_else(chrono::Utc::now);
    dt.format("%Y%m%dT%H%M%SZ").to_string()
}

// The SHA-256, HMAC and hex helpers live in `crate::crypto`.
use crate::crypto::{hex_encode, hmac_sha256, sha256_hex};

/// Derive the SigV4 signing key by hashing the secret through the
/// scope chain: date, then region, then service.
fn derive_signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

/// Percent-encode a key the way SigV4 wants. `/` stays as it is,
/// because S3 keys use it as a separator; everything outside the
/// unreserved set is escaped.
fn encode_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for &b in key.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

/// Like [`encode_key`], but it escapes `/` as well. The canonical
/// query string escapes everything outside the unreserved set.
fn encode_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> S3Config {
        S3Config {
            bucket: "examplebucket".into(),
            region: "us-east-1".into(),
            endpoint: None,
            access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            path_style: false,
        }
    }

    // The known-good values below come from the AWS docs page
    // "Examples of the complete Version 4 signing process".

    #[test]
    fn signing_key_matches_aws_docs_test_vector() {
        let key = derive_signing_key(
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "20130524",
            "us-east-1",
            "s3",
        );
        let hex = hex_encode(&key);
        assert_eq!(
            hex,
            "dbb893acc010964918f1fd433add87c70e8b0db6be30c1fbeafefa5ec6ba8378"
        );
    }

    #[test]
    fn sha256_empty_string_matches_known_value() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn format_amz_date_matches_iso8601_basic() {
        // 1369353600 is 2013-05-24T00:00:00Z.
        assert_eq!(format_amz_date(1369353600), "20130524T000000Z");
    }

    // -------- key encoding --------

    #[test]
    fn encode_key_preserves_safe_chars_and_slashes() {
        assert_eq!(encode_key("a/b/c.txt"), "a/b/c.txt");
        assert_eq!(encode_key("avatars/alice_42.png"), "avatars/alice_42.png");
    }

    #[test]
    fn encode_key_percent_escapes_unsafe_chars() {
        assert_eq!(encode_key("hello world.txt"), "hello%20world.txt");
        assert_eq!(encode_key("a+b.txt"), "a%2Bb.txt");
        assert_eq!(encode_key("café.png"), "caf%C3%A9.png");
    }

    // -------- host and URL composition --------

    #[test]
    fn host_aws_virtual_hosted_default() {
        let s = S3Storage::new(cfg());
        assert_eq!(s.host(), "examplebucket.s3.us-east-1.amazonaws.com");
    }

    #[test]
    fn host_aws_path_style_uses_regional_endpoint() {
        let mut c = cfg();
        c.path_style = true;
        let s = S3Storage::new(c);
        assert_eq!(s.host(), "s3.us-east-1.amazonaws.com");
    }

    #[test]
    fn host_custom_endpoint_strips_scheme() {
        let mut c = cfg();
        c.path_style = true;
        c.endpoint = Some("https://abc.r2.cloudflarestorage.com/".into());
        let s = S3Storage::new(c);
        assert_eq!(s.host(), "abc.r2.cloudflarestorage.com");
    }

    #[test]
    fn host_http_endpoint_recognized() {
        let mut c = cfg();
        c.path_style = true;
        c.endpoint = Some("http://localhost:9000".into());
        let s = S3Storage::new(c);
        assert_eq!(s.host(), "localhost:9000");
        assert_eq!(s.scheme(), "http");
    }

    #[test]
    fn full_url_aws_virtual_hosted() {
        let s = S3Storage::new(cfg());
        assert_eq!(
            s.full_url("avatars/alice.png"),
            "https://examplebucket.s3.us-east-1.amazonaws.com/avatars/alice.png"
        );
    }

    #[test]
    fn full_url_path_style_includes_bucket() {
        let mut c = cfg();
        c.path_style = true;
        c.endpoint = Some("https://minio.example.com".into());
        let s = S3Storage::new(c);
        assert_eq!(
            s.full_url("avatars/alice.png"),
            "https://minio.example.com/examplebucket/avatars/alice.png"
        );
    }

    #[test]
    fn url_returns_stable_public_link() {
        let s = S3Storage::new(cfg());
        assert!(s
            .url("foo.txt")
            .unwrap()
            .ends_with("examplebucket.s3.us-east-1.amazonaws.com/foo.txt"));
    }

    // -------- live test, skipped with no AWS env vars --------

    #[tokio::test]
    async fn live_round_trip_skipped_without_env() {
        let access = std::env::var("RUSTANGO_S3_TEST_KEY").ok();
        let secret = std::env::var("RUSTANGO_S3_TEST_SECRET").ok();
        let bucket = std::env::var("RUSTANGO_S3_TEST_BUCKET").ok();
        let endpoint = std::env::var("RUSTANGO_S3_TEST_ENDPOINT").ok();
        let region =
            std::env::var("RUSTANGO_S3_TEST_REGION").unwrap_or_else(|_| "us-east-1".into());
        let (Some(access), Some(secret), Some(bucket)) = (access, secret, bucket) else {
            eprintln!(
                "skipping live S3 test — set RUSTANGO_S3_TEST_KEY / _SECRET / _BUCKET (and optionally _ENDPOINT / _REGION)"
            );
            return;
        };
        let s = S3Storage::new(S3Config {
            bucket,
            region,
            endpoint: endpoint.clone(),
            access_key_id: access,
            secret_access_key: secret,
            // A custom endpoint usually means R2 or MinIO, which
            // want path style.
            path_style: endpoint.is_some(),
        });
        let key = format!("rustango-test/{}.txt", uuid::Uuid::new_v4());
        let payload = b"hello from rustango integration test";
        s.save(&key, payload).await.expect("save");
        let loaded = s.load(&key).await.expect("load");
        assert_eq!(&loaded, payload);
        assert!(s.exists(&key).await.expect("exists"));
        s.delete(&key).await.expect("delete");
        assert!(!s.exists(&key).await.expect("exists after delete"));
    }

    // -------- presigned URLs, checked offline --------

    #[test]
    fn encode_query_escapes_slashes() {
        // Query encoding escapes slashes; key encoding does not.
        assert_eq!(encode_query("a/b"), "a%2Fb");
        assert_eq!(encode_query("plain-name_1.2~3"), "plain-name_1.2~3");
        assert_eq!(encode_query("a+b"), "a%2Bb");
    }

    #[tokio::test]
    async fn presigned_get_url_carries_required_query_params() {
        let s = S3Storage::new(cfg());
        let url = s
            .presigned_get_url("avatars/alice.png", std::time::Duration::from_secs(60))
            .await
            .unwrap();
        assert!(
            url.starts_with("https://examplebucket.s3.us-east-1.amazonaws.com/avatars/alice.png?")
        );
        for k in [
            "X-Amz-Algorithm=AWS4-HMAC-SHA256",
            "X-Amz-Credential=",
            "X-Amz-Date=",
            "X-Amz-Expires=60",
            "X-Amz-SignedHeaders=host",
            "X-Amz-Signature=",
        ] {
            assert!(url.contains(k), "missing {k} in {url}");
        }
    }

    #[tokio::test]
    async fn presigned_put_url_with_content_type_signs_it() {
        let s = S3Storage::new(cfg());
        let url = s
            .presigned_put_url(
                "uploads/x.png",
                std::time::Duration::from_secs(300),
                &PutConditions::new().content_type("image/png"),
            )
            .await
            .unwrap();
        // SignedHeaders must list both content-type and host, in
        // alphabetical order, so the binding is really in place.
        assert!(
            url.contains("X-Amz-SignedHeaders=content-type%3Bhost"),
            "expected content-type bound in SignedHeaders, got: {url}"
        );
    }

    /// #1851: the declared size is signed, so S3 refuses any other body.
    #[tokio::test]
    async fn presigned_put_url_signs_the_content_length() {
        let s = S3Storage::new(cfg());
        let url = s
            .presigned_put_url(
                "u/x.png",
                std::time::Duration::from_secs(300),
                &PutConditions::new()
                    .content_type("image/png")
                    .content_length(10),
            )
            .await
            .unwrap();
        assert!(
            url.contains("X-Amz-SignedHeaders=content-length%3Bcontent-type%3Bhost"),
            "got: {url}"
        );
    }

    /// A create-only PUT signs `if-none-match`, so the URL cannot overwrite.
    #[tokio::test]
    async fn presigned_put_url_create_only_signs_if_none_match() {
        let s = S3Storage::new(cfg());
        let put = PutConditions::new().content_length(3).create_only();
        let url = s
            .presigned_put_url("u/x", std::time::Duration::from_secs(60), &put)
            .await
            .unwrap();
        assert!(
            url.contains("X-Amz-SignedHeaders=content-length%3Bhost%3Bif-none-match"),
            "got: {url}"
        );
    }

    #[tokio::test]
    async fn presigned_put_url_without_content_type_only_signs_host() {
        let s = S3Storage::new(cfg());
        let url = s
            .presigned_put_url(
                "uploads/x.bin",
                std::time::Duration::from_secs(300),
                &PutConditions::new(),
            )
            .await
            .unwrap();
        assert!(url.contains("X-Amz-SignedHeaders=host"));
        assert!(!url.contains("content-type"));
    }

    /// #1570: a page of presigns derives the SigV4 key once, not per row.
    #[tokio::test]
    async fn presigning_a_page_derives_the_signing_key_once() {
        let s = S3Storage::new(cfg());
        for i in 0..50 {
            let key = format!("k/{i}");
            s.presigned_get_url(&key, std::time::Duration::from_secs(60))
                .await
                .unwrap();
        }
        let n = s.derivations.load(std::sync::atomic::Ordering::Relaxed);
        assert!(n <= 2, "derived {n} times for one page");
    }

    #[tokio::test]
    async fn presigned_url_clamps_expiry_at_7_days() {
        let s = S3Storage::new(cfg());
        let url = s
            .presigned_get_url(
                "k",
                std::time::Duration::from_secs(60 * 60 * 24 * 30), // 30 days
            )
            .await
            .unwrap();
        // AWS caps it at 604800 seconds, which is 7 days.
        assert!(url.contains("X-Amz-Expires=604800"), "got: {url}");
    }

    #[tokio::test]
    async fn presigned_url_uses_path_style_when_configured() {
        let mut c = cfg();
        c.path_style = true;
        c.endpoint = Some("https://minio.example.com".into());
        let s = S3Storage::new(c);
        let url = s
            .presigned_get_url("uploads/x.png", std::time::Duration::from_secs(60))
            .await
            .unwrap();
        assert!(url.starts_with("https://minio.example.com/examplebucket/uploads/x.png?"));
    }

    #[tokio::test]
    async fn local_storage_presigned_get_returns_none() {
        // A backend that cannot sign must not pretend it can.
        use crate::storage::LocalStorage;
        let local = LocalStorage::new(std::path::PathBuf::from("/tmp"));
        assert!(local
            .presigned_get_url("x", std::time::Duration::from_secs(60))
            .await
            .is_none());
        assert!(local
            .presigned_put_url(
                "x",
                std::time::Duration::from_secs(60),
                &PutConditions::new()
            )
            .await
            .is_none());
    }

    // -------- endpoints with a path prefix, like Supabase --------

    fn supabase_cfg() -> S3Config {
        S3Config {
            endpoint: Some("https://abc123.storage.supabase.co/storage/v1/s3".into()),
            bucket: "media".into(),
            region: "us-west-2".into(),
            path_style: true,
            ..cfg()
        }
    }

    #[test]
    fn host_is_the_bare_authority_even_when_the_endpoint_has_a_path() {
        // This value becomes the `Host` header and the signed
        // `host:` header. A path in it is an invalid Host, which a
        // proxy rejects with 400 before S3 sees the request.
        let s = S3Storage::new(supabase_cfg());
        assert_eq!(s.host(), "abc123.storage.supabase.co");
    }

    #[test]
    fn endpoint_path_prefix_is_kept_in_the_request_path() {
        let s = S3Storage::new(supabase_cfg());
        assert_eq!(s.endpoint_prefix(), "/storage/v1/s3");
        assert_eq!(s.key_path("a/b.png"), "/storage/v1/s3/media/a/b.png");
        assert_eq!(
            s.full_url("a/b.png"),
            "https://abc123.storage.supabase.co/storage/v1/s3/media/a/b.png",
        );
    }

    #[test]
    fn bare_host_endpoints_are_unaffected() {
        // MinIO and R2 have no path prefix, so nothing changes.
        let s = S3Storage::new(S3Config {
            endpoint: Some("http://127.0.0.1:9000".into()),
            bucket: "media".into(),
            path_style: true,
            ..cfg()
        });
        assert_eq!(s.host(), "127.0.0.1:9000");
        assert_eq!(s.endpoint_prefix(), "");
        assert_eq!(s.key_path("a.png"), "/media/a.png");
        assert_eq!(s.full_url("a.png"), "http://127.0.0.1:9000/media/a.png");
    }

    #[test]
    fn aws_endpoints_are_unaffected() {
        // With no custom endpoint the bucket sits in the host,
        // unless path style is asked for.
        let virt = S3Storage::new(cfg());
        assert_eq!(virt.host(), "examplebucket.s3.us-east-1.amazonaws.com");
        assert_eq!(virt.endpoint_prefix(), "");
        assert_eq!(virt.key_path("a.png"), "/a.png");

        let path = S3Storage::new(S3Config {
            path_style: true,
            ..cfg()
        });
        assert_eq!(path.host(), "s3.us-east-1.amazonaws.com");
        assert_eq!(path.key_path("a.png"), "/examplebucket/a.png");
    }

    #[test]
    fn signed_canonical_path_matches_the_url_path() {
        // The signature covers `key_path`. If the URL carried the
        // endpoint prefix and the signed path did not, every request
        // would fail with SignatureDoesNotMatch.
        let s = S3Storage::new(supabase_cfg());
        let url = s.full_url("a/b.png");
        let signed_path = s.key_path("a/b.png");
        assert!(
            url.ends_with(&signed_path),
            "url {url} must end with signed path {signed_path}",
        );
    }

    // -------- request shape against a local mock (#1904) --------

    /// Answer one request with `status`; yields the raw request head.
    async fn mock(status: u16) -> (String, tokio::task::JoinHandle<String>) {
        mock_with(status, String::new()).await
    }

    /// [`mock`] with extra response header lines, each ending in `\r\n`.
    async fn mock_with(status: u16, extra: String) -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = sock.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let end = buf
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .unwrap_or(buf.len());
            let head = String::from_utf8_lossy(&buf[..end]).into_owned();
            // Drain the body, so closing does not reset the connection.
            let len: usize = head
                .to_ascii_lowercase()
                .lines()
                .find_map(|l| l.strip_prefix("content-length:").map(str::to_owned))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
            while buf.len() < end + 4 + len {
                let n = sock.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let reply = format!(
                "HTTP/1.1 {status} X\r\n{extra}content-length: 0\r\nconnection: close\r\n\r\n"
            );
            sock.write_all(reply.as_bytes()).await.unwrap();
            head
        });
        (endpoint, task)
    }

    fn mock_storage(endpoint: String) -> S3Storage {
        S3Storage::new(S3Config {
            endpoint: Some(endpoint),
            path_style: true,
            ..cfg()
        })
    }

    /// Check the request's SigV4 signature as S3 would: rebuild the
    /// canonical request from what was sent, re-sign with the test key.
    fn verify_sigv4(head: &str) {
        let mut lines = head.split("\r\n");
        let mut req_line = lines.next().unwrap().split(' ');
        let (method, path) = (req_line.next().unwrap(), req_line.next().unwrap());
        let headers: Vec<(String, String)> = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.to_owned()))
            .collect();
        let get = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.split_whitespace().collect::<Vec<_>>().join(" "))
                .unwrap_or_else(|| panic!("no {name} in {head}"))
        };
        let auth = get("authorization");
        let field = |name: &str| {
            auth.split(|c| c == ' ' || c == ',')
                .find_map(|p| p.strip_prefix(name))
                .unwrap()
                .to_owned()
        };
        let signed = field("SignedHeaders=");
        let canonical_headers: String = signed
            .split(';')
            .map(|h| format!("{h}:{}\n", get(h)))
            .collect();
        let amz_date = get("x-amz-date");
        let payload = get("x-amz-content-sha256");
        let cr = format!("{method}\n{path}\n\n{canonical_headers}\n{signed}\n{payload}");
        let scope = format!("{}/us-east-1/s3/aws4_request", &amz_date[..8]);
        let sts = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
            sha256_hex(cr.as_bytes())
        );
        let key = derive_signing_key(&cfg().secret_access_key, &amz_date[..8], "us-east-1", "s3");
        let want = hex_encode(&hmac_sha256(&key, sts.as_bytes()));
        assert_eq!(
            field("Signature="),
            want,
            "signature does not cover what was sent"
        );
    }

    #[tokio::test]
    async fn save_with_content_type_sends_and_signs_it() {
        // A double space must be signed as S3 normalizes it.
        let (ep, req) = mock(200).await;
        mock_storage(ep)
            .save_with_content_type("a.txt", b"txt", Some(" text/plain;  charset=utf-8 "))
            .await
            .unwrap();
        let head = req.await.unwrap();
        let lower = head.to_ascii_lowercase();
        assert!(
            lower.contains("\r\ncontent-type: text/plain; charset=utf-8"),
            "{head}"
        );
        assert!(
            lower.contains("signedheaders=content-type;host;x-amz-content-sha256;x-amz-date"),
            "{head}"
        );
        verify_sigv4(&head);
    }

    #[tokio::test]
    async fn content_type_with_a_line_break_is_refused() {
        let s = mock_storage("http://127.0.0.1:9".into());
        let err = s
            .save_with_content_type("a.txt", b"x", Some("text/plain\r\nx-evil: 1"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("control character"), "{err}");
    }

    #[tokio::test]
    async fn exists_is_false_only_on_404() {
        let (ep, _req) = mock(404).await;
        assert!(!mock_storage(ep).exists("a.png").await.unwrap());
        let (ep, _req) = mock(200).await;
        assert!(mock_storage(ep).exists("a.png").await.unwrap());
        for status in [403, 503] {
            let (ep, _req) = mock(status).await;
            assert!(
                mock_storage(ep).exists("a.png").await.is_err(),
                "{status} read as a plain answer"
            );
        }
    }

    /// A redirect is an answer, not a hop to the host it names (#1780).
    #[tokio::test]
    async fn redirects_are_not_followed() {
        let elsewhere = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = format!("http://{}/stolen", elsewhere.local_addr().unwrap());
        let (ep, _req) = mock_with(307, format!("location: {target}\r\n")).await;
        let storage = mock_storage(ep);
        let call = tokio::time::timeout(std::time::Duration::from_secs(2), storage.exists("a.png"));
        // A followed redirect hangs on `elsewhere`, which never answers.
        assert!(matches!(call.await, Ok(Err(_))));
        let hop = tokio::time::timeout(std::time::Duration::from_millis(300), elsewhere.accept());
        assert!(hop.await.is_err(), "the redirect was followed");
    }

    /// An endpoint that connects but never answers hits the read timeout (#2220).
    #[tokio::test]
    async fn stalled_endpoint_times_out() {
        let stalled = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let storage = mock_storage(format!("http://{}", stalled.local_addr().unwrap()));
        let start = tokio::time::Instant::now();
        let call = tokio::spawn(async move { storage.exists("a.png").await });
        // Connect on the real clock: a paused one jumps to the connect
        // timeout while the loopback handshake is still in flight (#2358).
        let accept = tokio::time::timeout(DEFAULT_CONNECT_TIMEOUT, stalled.accept());
        let _held = accept.await.expect("the client never connected").unwrap().0;
        tokio::time::pause();
        let call = tokio::time::timeout(DEFAULT_READ_TIMEOUT * 2, call);
        assert!(matches!(call.await, Ok(Ok(Err(_)))), "no timeout fired");
        // Past the connect timeout, so the read timeout fired.
        assert!(
            start.elapsed() >= DEFAULT_READ_TIMEOUT,
            "{:?}",
            start.elapsed()
        );
    }

    /// 1 MiB gets 1 s + 4 s to upload under a 1 s read timeout.
    const UPLOAD_LEN: usize = 1 << 20;
    const SHORT_READ: std::time::Duration = std::time::Duration::from_secs(1);

    fn short_timeout_storage(listener: &tokio::net::TcpListener) -> S3Storage {
        let mut s = mock_storage(format!("http://{}", listener.local_addr().unwrap()));
        s.http = Http::with_read_timeout(SHORT_READ);
        s
    }

    /// Read a request over `ms` milliseconds for the whole body, then answer 200.
    async fn slow_reader(listener: tokio::net::TcpListener, ms: u64) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 16 * 1024];
        let mut head = Vec::new();
        let mut seen = loop {
            let n = sock.read(&mut buf).await.unwrap();
            head.extend_from_slice(&buf[..n]);
            if let Some(end) = head.windows(4).position(|w| w == b"\r\n\r\n") {
                break head.len() - end - 4;
            }
        };
        while seen < UPLOAD_LEN {
            let n = sock.read(&mut buf).await.unwrap();
            assert!(n > 0, "upload cut short");
            seen += n;
            let pause = ms * n as u64 / UPLOAD_LEN as u64;
            tokio::time::sleep(std::time::Duration::from_millis(pause)).await;
        }
        sock.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
            .await
            .unwrap();
    }

    /// An upload slower than the read timeout but inside its size budget succeeds.
    #[tokio::test]
    async fn slow_upload_outlives_the_read_timeout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let storage = short_timeout_storage(&listener);
        let server = tokio::spawn(slow_reader(listener, 2500));
        let start = std::time::Instant::now();
        storage
            .save("big.bin", &vec![0u8; UPLOAD_LEN])
            .await
            .expect("slow upload");
        assert!(start.elapsed() > 2 * SHORT_READ, "{:?}", start.elapsed());
        server.await.unwrap();
    }

    /// An upload the endpoint never answers fails at its size budget.
    #[tokio::test]
    async fn stalled_upload_times_out() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let storage = short_timeout_storage(&listener);
        let held = tokio::spawn(async move { listener.accept().await.unwrap().0 });
        let data = vec![0u8; UPLOAD_LEN];
        let budget = upload_timeout(SHORT_READ, UPLOAD_LEN);
        let start = std::time::Instant::now();
        let call = tokio::time::timeout(budget * 2, storage.save("big.bin", &data));
        assert!(matches!(call.await, Ok(Err(_))), "no timeout fired");
        assert!(held.is_finished());
        assert!(start.elapsed() >= budget, "{:?}", start.elapsed());
    }

    #[test]
    fn upload_timeout_grows_with_size() {
        assert_eq!(
            upload_timeout(DEFAULT_READ_TIMEOUT, 0),
            DEFAULT_READ_TIMEOUT
        );
        let mib_16 = upload_timeout(DEFAULT_READ_TIMEOUT, 16 << 20);
        assert_eq!(mib_16.as_secs(), 124);
    }

    #[test]
    fn virtual_hosted_custom_endpoint_puts_the_bucket_in_the_host() {
        let s = S3Storage::new(S3Config {
            endpoint: Some("https://abc.r2.cloudflarestorage.com".into()),
            ..cfg()
        });
        assert_eq!(
            s.full_url("a.png"),
            "https://examplebucket.abc.r2.cloudflarestorage.com/a.png"
        );
        // An endpoint that already names the bucket is left alone.
        let s = S3Storage::new(S3Config {
            endpoint: Some("https://examplebucket.s3.us-east-1.amazonaws.com".into()),
            ..cfg()
        });
        assert_eq!(s.host(), "examplebucket.s3.us-east-1.amazonaws.com");
    }
}
