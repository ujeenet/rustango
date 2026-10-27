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
//! use rustango::storage::s3::{S3Config, S3Credentials, S3Storage};
//! use std::sync::Arc;
//!
//! // AWS S3, virtual-hosted URLs at https://s3.<region>.amazonaws.com
//! let s3 = S3Storage::new(
//!     S3Config::new("my-bucket", "us-east-1"),
//!     S3Credentials::new(
//!         std::env::var("AWS_ACCESS_KEY_ID")?,
//!         std::env::var("AWS_SECRET_ACCESS_KEY")?,
//!     ),
//! );
//!
//! // Cloudflare R2
//! let r2 = S3Storage::new(
//!     S3Config::new("my-bucket", "auto")
//!         .with_endpoint("https://<account>.r2.cloudflarestorage.com")
//!         .with_path_style(true),
//!     S3Credentials::new(std::env::var("R2_KEY")?, std::env::var("R2_SECRET")?),
//! );
//!
//! let storage: rustango::storage::BoxedStorage = Arc::new(s3);
//! storage.save("avatars/alice.png", &png_bytes).await?;
//! ```
//!
//! ## IAM roles and temporary credentials
//!
//! Temporary keys carry a session token:
//! `S3Credentials::new(key, secret).with_session_token(token)`. They
//! expire, so for an IAM role (IRSA, instance profile, ECS task role)
//! pass an [`S3CredentialProvider`] that refreshes them. It is asked
//! once per request, so it should cache. For example, over `aws-config`:
//!
//! ```ignore
//! use aws_credential_types::provider::ProvideCredentials;
//!
//! struct AwsChain(aws_config::default_provider::credentials::DefaultCredentialsChain);
//!
//! #[async_trait::async_trait]
//! impl S3CredentialProvider for AwsChain {
//!     async fn credentials(&self) -> Result<S3Credentials, StorageError> {
//!         let c = self.0.provide_credentials().await
//!             .map_err(|e| StorageError::Io(format!("credentials: {e}")))?;
//!         let creds = S3Credentials::new(c.access_key_id(), c.secret_access_key());
//!         Ok(match c.session_token() {
//!             Some(t) => creds.with_session_token(t),
//!             None => creds,
//!         })
//!     }
//! }
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
//! - Streamed `save_stream` and `load_stream`. The PUT sends
//!   `UNSIGNED-PAYLOAD` over https and signs each chunk over plain http.
//!   Above 5 GiB it is a multipart upload, up to 5 TiB, aborted on any
//!   failure so no partial object shows.
//!
//! ## What it does not do
//!
//! - Creating or listing buckets.
//! - Server-side encryption with your own keys (SSE-C).

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;

use super::transport::{
    canonical_header_value, encode_key, encode_query, send_error, stream_body, Http, Parts, Split,
    MAX_PRESIGN_SECS,
};
#[cfg(test)]
use super::transport::{upload_timeout, DEFAULT_CONNECT_TIMEOUT, DEFAULT_READ_TIMEOUT};
use super::{
    already_exists, check_declared_len, validate_key, ByteStream, ObjectMeta, PutConditions,
    Storage, StorageError,
};

/// The most one PUT takes: 5 GiB.
const MAX_PUT: u64 = 5 << 30;

/// The largest object, which a multipart upload reaches: 5 TiB.
const MAX_OBJECT: u64 = 5 << 40;

/// Above one PUT, parts of at least 64 MiB; 10,000 of them reach 5 TiB.
const MULTIPART: Split = Split {
    max_single: MAX_PUT,
    min_part: 64 << 20,
    max_parts: 10_000,
};

/// Chunk size of a signed streaming upload; S3 wants at least 8 KiB.
const SIGNED_CHUNK: usize = 64 * 1024;

/// A request body.
enum Body<'a> {
    /// Hashed into the signature. Empty sends no body.
    Bytes(&'a [u8]),
    /// `UNSIGNED-PAYLOAD`: TLS guards the bytes.
    Unsigned(ByteStream, u64),
    /// `aws-chunked` with a signature per chunk, for plain-http endpoints.
    Chunked(ByteStream, u64),
}

/// How to reach the bucket. Build it with [`S3Config::new`]; the keys
/// are passed apart, as [`S3Credentials`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct S3Config {
    pub bucket: String,
    pub region: String,
    /// `None` means AWS S3 at `https://s3.<region>.amazonaws.com`.
    /// Set it for any other provider.
    pub endpoint: Option<String>,
    /// `false` puts the bucket in the host:
    /// `https://<bucket>.s3.<region>.amazonaws.com/<key>`, or
    /// `https://<bucket>.<endpoint-host>/<key>` with an `endpoint`. `true`
    /// puts it in the path: `https://<endpoint>/<bucket>/<key>`.
    /// AWS wants the first, R2 and MinIO the second.
    pub path_style: bool,
}

impl S3Config {
    /// AWS S3 in `region`, virtual-hosted URLs.
    #[must_use]
    pub fn new(bucket: impl Into<String>, region: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            region: region.into(),
            endpoint: None,
            path_style: false,
        }
    }

    /// A non-AWS endpoint, such as R2 or MinIO.
    #[must_use]
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    /// Put the bucket in the path instead of the host.
    #[must_use]
    pub fn with_path_style(mut self, path_style: bool) -> Self {
        self.path_style = path_style;
        self
    }
}

/// One set of access keys; temporary ones (STS, IAM roles) carry a
/// session token. `Debug` hides the secret and the token.
#[derive(Clone)]
pub struct S3Credentials {
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
}

impl S3Credentials {
    #[must_use]
    pub fn new(access_key_id: impl Into<String>, secret_access_key: impl Into<String>) -> Self {
        Self {
            access_key_id: access_key_id.into(),
            secret_access_key: secret_access_key.into(),
            session_token: None,
        }
    }

    /// Signed and sent as `X-Amz-Security-Token` on every request and
    /// presigned URL.
    #[must_use]
    pub fn with_session_token(mut self, token: impl Into<String>) -> Self {
        self.session_token = Some(token.into());
        self
    }

    #[must_use]
    pub fn access_key_id(&self) -> &str {
        &self.access_key_id
    }
}

impl std::fmt::Debug for S3Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Credentials")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// Where [`S3Storage`] gets its keys, asked once per request.
/// Implement it to refresh temporary credentials from an IAM role.
#[async_trait]
pub trait S3CredentialProvider: Send + Sync + 'static {
    async fn credentials(&self) -> Result<S3Credentials, StorageError>;
}

/// Fixed keys.
#[async_trait]
impl S3CredentialProvider for S3Credentials {
    async fn credentials(&self) -> Result<S3Credentials, StorageError> {
        Ok(self.clone())
    }
}

/// A cached SigV4 key and what it was derived from.
struct SigningKey {
    date: String,
    secret: String,
    key: Arc<[u8]>,
}

/// Storage backed by an S3-compatible bucket.
pub struct S3Storage {
    cfg: S3Config,
    creds: Arc<dyn S3CredentialProvider>,
    http: Http,
    /// The SigV4 key for one date stamp and secret, so a page of
    /// presigns derives it once and rotated keys derive anew.
    signing_key: std::sync::Mutex<Option<SigningKey>>,
    multipart: Split,
    #[cfg(test)]
    derivations: std::sync::atomic::AtomicUsize,
}

/// The endpoint without its `http(s)://` scheme.
fn strip_scheme(endpoint: &str) -> &str {
    endpoint
        .strip_prefix("https://")
        .or_else(|| endpoint.strip_prefix("http://"))
        .unwrap_or(endpoint)
}

impl S3Storage {
    /// `creds` is an [`S3Credentials`] for fixed keys, or any
    /// [`S3CredentialProvider`] for ones that refresh.
    #[must_use]
    pub fn new(cfg: S3Config, creds: impl S3CredentialProvider) -> Self {
        Self {
            cfg,
            creds: Arc::new(creds),
            http: Http::defaults(),
            signing_key: std::sync::Mutex::new(None),
            multipart: MULTIPART,
            #[cfg(test)]
            derivations: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// The same bucket and keys, owned, for a request that outlives `&self`.
    fn detached(&self) -> Self {
        Self {
            cfg: self.cfg.clone(),
            creds: self.creds.clone(),
            http: self.http.clone(),
            signing_key: std::sync::Mutex::new(None),
            multipart: self.multipart,
            #[cfg(test)]
            derivations: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// The signing key for `date_stamp` and `secret`, derived once per pair.
    fn signing_key(&self, date_stamp: &str, secret: &str) -> Arc<[u8]> {
        let mut slot = self
            .signing_key
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(k) = slot.as_ref() {
            if k.date == date_stamp && k.secret == secret {
                return k.key.clone();
            }
        }
        #[cfg(test)]
        self.derivations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let key: Arc<[u8]> = derive_signing_key(secret, date_stamp, &self.cfg.region, "s3").into();
        *slot = Some(SigningKey {
            date: date_stamp.to_owned(),
            secret: secret.to_owned(),
            key: key.clone(),
        });
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
        query: &[(&str, &str)],
        body: Body<'_>,
        put: &PutConditions,
    ) -> Result<reqwest::Response, StorageError> {
        validate_key(key)?;
        let creds = self.creds.credentials().await?;
        let mut query: Vec<(String, String)> = query
            .iter()
            .map(|(k, v)| (encode_query(k), encode_query(v)))
            .collect();
        query.sort();
        let canonical_query = query
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        let mut url = self.full_url(key);
        if !canonical_query.is_empty() {
            url = format!("{url}?{canonical_query}");
        }
        let path = self.key_path(key);
        let amz_date = format_amz_date(unix_now()?);
        let date_stamp = &amz_date[..8];
        let payload_hash = match &body {
            Body::Bytes(b) => sha256_hex(b),
            Body::Unsigned(..) => "UNSIGNED-PAYLOAD".to_owned(),
            Body::Chunked(..) => "STREAMING-AWS4-HMAC-SHA256-PAYLOAD".to_owned(),
        };

        // Canonical headers: lowercase, sorted by name. A content type
        // is signed too, so S3 stores the one we sent (#1904).
        let mut headers: Vec<(&str, String)> = Vec::with_capacity(9);
        if let Some(ct) = canonical_header_value(put.content_type.as_deref())? {
            headers.push(("content-type", ct));
        }
        headers.push(("host", self.host()));
        headers.push(("x-amz-content-sha256", payload_hash.clone()));
        headers.push(("x-amz-date", amz_date.clone()));
        if let Some(token) = &creds.session_token {
            headers.push(("x-amz-security-token", token.clone()));
        }
        // A streamed body signs its length, so S3 refuses any other.
        // No `Content-Encoding: aws-chunked`: SeaweedFS would store it.
        let sent = match &body {
            Body::Bytes(b) => b.len() as u64,
            Body::Unsigned(_, len) => *len,
            Body::Chunked(_, len) => {
                headers.push(("x-amz-decoded-content-length", len.to_string()));
                aws_chunked_len(*len, SIGNED_CHUNK as u64)
            }
        };
        if !matches!(body, Body::Bytes(_)) {
            headers.push(("content-length", sent.to_string()));
        }
        if put.create_only {
            headers.push(("if-none-match", "*".to_owned()));
        }
        headers.sort_by(|a, b| a.0.cmp(b.0));
        let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
        let signed_headers = headers
            .iter()
            .map(|(k, _)| *k)
            .collect::<Vec<_>>()
            .join(";");

        let canonical_request =
            format!("{method}\n{path}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}");
        let cr_hash = sha256_hex(canonical_request.as_bytes());

        let scope = format!("{date_stamp}/{}/s3/aws4_request", self.cfg.region);
        let string_to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{cr_hash}");

        let signing_key = self.signing_key(date_stamp, &creds.secret_access_key);
        let signature = hex_encode(&hmac_sha256(&signing_key, string_to_sign.as_bytes()));

        let auth = format!(
            "AWS4-HMAC-SHA256 Credential={key_id}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            key_id = creds.access_key_id,
        );

        let mut req = self.http.request(
            reqwest::Method::from_bytes(method.as_bytes())
                .map_err(|e| StorageError::Io(format!("method: {e}")))?,
            &url,
            sent,
        );
        for (name, value) in headers {
            req = req.header(name, value);
        }
        req = req.header("authorization", auth);
        match body {
            Body::Bytes([]) => {}
            Body::Bytes(b) => req = req.body(b.to_vec()),
            Body::Unsigned(body, len) => req = req.body(stream_body(body, len)),
            Body::Chunked(body, len) => {
                let signer = ChunkSigner {
                    key: signing_key,
                    amz_date,
                    scope,
                    prev: signature,
                };
                let framed = aws_chunked(body.exact(len), signer, SIGNED_CHUNK);
                req = req.body(stream_body(framed, sent));
            }
        }
        req.send().await.map_err(|e| send_error(&e))
    }

    /// One streamed PUT. `signed` chunk-signs the body instead of
    /// sending it as `UNSIGNED-PAYLOAD`.
    async fn put_stream(
        &self,
        key: &str,
        len: u64,
        body: ByteStream,
        put: &PutConditions,
        signed: bool,
    ) -> Result<(), StorageError> {
        let body = if signed {
            Body::Chunked(body, len)
        } else {
            Body::Unsigned(body, len)
        };
        let resp = self.signed_request("PUT", key, &[], body, put).await?;
        let status = resp.status();
        // A racing conditional write can get 409 ConditionalRequestConflict.
        if put.create_only
            && matches!(
                status,
                reqwest::StatusCode::PRECONDITION_FAILED | reqwest::StatusCode::CONFLICT
            )
        {
            return Err(already_exists(key));
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(StorageError::Io(format!(
                "S3 PUT {key} -> {status}: {body}"
            )));
        }
        Ok(())
    }

    /// A multipart upload, one part in flight. Any failure, or a dropped
    /// future, aborts it, so S3 never shows a partial object.
    async fn put_multipart(
        &self,
        key: &str,
        len: u64,
        body: ByteStream,
        put: &PutConditions,
        signed: bool,
    ) -> Result<(), StorageError> {
        if len > MAX_OBJECT {
            return Err(StorageError::Io(format!(
                "S3 PUT {key}: {len} bytes is over the 5 TiB S3 stores"
            )));
        }
        // Fail fast on a key that is surely taken; Complete still decides.
        if put.create_only && matches!(self.exists(key).await, Ok(true)) {
            return Err(already_exists(key));
        }
        let upload_id = self.create_multipart(key, put).await?;
        let mut guard = AbortOnDrop(Some((self.detached(), key.to_owned(), upload_id.clone())));
        let result = self
            .upload_parts(key, &upload_id, len, body, put, signed)
            .await;
        if result.is_err() {
            // Best effort: the caller wants the upload's own error.
            let _ = self.abort_multipart(key, &upload_id).await;
        }
        guard.0 = None;
        result
    }

    /// CreateMultipartUpload; the content type is set here.
    async fn create_multipart(
        &self,
        key: &str,
        put: &PutConditions,
    ) -> Result<String, StorageError> {
        let create = PutConditions {
            content_type: put.content_type.clone(),
            ..PutConditions::default()
        };
        let resp = self
            .signed_request("POST", key, &[("uploads", "")], Body::Bytes(b""), &create)
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(StorageError::Io(format!(
                "S3 CreateMultipartUpload {key} -> {status}: {text}"
            )));
        }
        xml_text(&text, "UploadId").ok_or_else(|| {
            StorageError::Io(format!(
                "S3 CreateMultipartUpload {key}: no UploadId in {text}"
            ))
        })
    }

    /// Every part, then CompleteMultipartUpload.
    async fn upload_parts(
        &self,
        key: &str,
        upload_id: &str,
        len: u64,
        body: ByteStream,
        put: &PutConditions,
        signed: bool,
    ) -> Result<(), StorageError> {
        let part = self.multipart.part_size(len);
        let mut parts = Parts::new(body, len);
        let mut etags = Vec::new();
        let mut sent = 0;
        while sent < len {
            let size = part.min(len - sent);
            let (stream, back) = parts.take(size);
            let body = if signed {
                Body::Chunked(stream, size)
            } else {
                Body::Unsigned(stream, size)
            };
            let number = (etags.len() + 1).to_string();
            let query = [("partNumber", number.as_str()), ("uploadId", upload_id)];
            let resp = self
                .signed_request("PUT", key, &query, body, &PutConditions::new())
                .await?;
            let status = resp.status();
            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                return Err(StorageError::Io(format!(
                    "S3 UploadPart {key} #{number} -> {status}: {text}"
                )));
            }
            let etag = resp
                .headers()
                .get(reqwest::header::ETAG)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
                .ok_or_else(|| {
                    StorageError::Io(format!("S3 UploadPart {key} #{number}: no ETag"))
                })?;
            parts = back.await.map_err(|_| {
                StorageError::Io(format!(
                    "S3 UploadPart {key} #{number}: part not fully sent"
                ))
            })?;
            etags.push(etag);
            sent += size;
        }
        parts.finish().await?;
        self.complete_multipart(key, upload_id, &etags, put.create_only)
            .await
    }

    /// CompleteMultipartUpload. `create_only` sends `If-None-Match: *`.
    async fn complete_multipart(
        &self,
        key: &str,
        upload_id: &str,
        etags: &[String],
        create_only: bool,
    ) -> Result<(), StorageError> {
        let mut xml = String::from("<CompleteMultipartUpload>");
        for (i, etag) in etags.iter().enumerate() {
            let _ = write!(
                xml,
                "<Part><PartNumber>{}</PartNumber><ETag>{}</ETag></Part>",
                i + 1,
                xml_escape(etag)
            );
        }
        xml.push_str("</CompleteMultipartUpload>");
        let cond = PutConditions {
            create_only,
            ..PutConditions::default()
        };
        let resp = self
            .signed_request(
                "POST",
                key,
                &[("uploadId", upload_id)],
                Body::Bytes(xml.as_bytes()),
                &cond,
            )
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        // S3 may answer 200 and still fail, with an `<Error>` body.
        let code = xml_text(&text, "Code").filter(|_| text.contains("<Error>"));
        let taken = matches!(
            status,
            reqwest::StatusCode::PRECONDITION_FAILED | reqwest::StatusCode::CONFLICT
        ) || matches!(
            code.as_deref(),
            Some("PreconditionFailed" | "ConditionalRequestConflict")
        );
        if create_only && taken {
            return Err(already_exists(key));
        }
        if !status.is_success() || code.is_some() {
            return Err(StorageError::Io(format!(
                "S3 CompleteMultipartUpload {key} -> {status}: {text}"
            )));
        }
        Ok(())
    }

    /// AbortMultipartUpload; a gone upload counts as done.
    async fn abort_multipart(&self, key: &str, upload_id: &str) -> Result<(), StorageError> {
        let resp = self
            .signed_request(
                "DELETE",
                key,
                &[("uploadId", upload_id)],
                Body::Bytes(b""),
                &PutConditions::new(),
            )
            .await?;
        let status = resp.status();
        if status.is_success() || status == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        Err(StorageError::Io(format!(
            "S3 AbortMultipartUpload {key} -> {status}"
        )))
    }

    /// GET, its status checked.
    async fn get(&self, key: &str) -> Result<reqwest::Response, StorageError> {
        let resp = self
            .signed_request("GET", key, &[], Body::Bytes(b""), &PutConditions::new())
            .await?;
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
        Ok(resp)
    }

    /// Build a URL whose SigV4 signature lives in the query string.
    /// Used for both presigned GET and presigned PUT.
    ///
    /// The payload hash is the literal `UNSIGNED-PAYLOAD`, since the
    /// URL exists before the body does. For a PUT with a content
    /// type, that header is signed too, so the browser has to send a
    /// matching value.
    async fn build_presigned_url(
        &self,
        method: &str,
        key: &str,
        ttl_secs: u64,
        put: &PutConditions,
    ) -> Result<String, StorageError> {
        validate_key(key)?;
        let creds = self.creds.credentials().await?;
        let expires = ttl_secs.clamp(1, MAX_PRESIGN_SECS);
        let path = self.key_path(key);
        let host = self.host();
        let amz_date = format_amz_date(unix_now()?);
        let date_stamp = &amz_date[..8];
        let scope = format!("{date_stamp}/{}/s3/aws4_request", self.cfg.region);
        let credential = format!("{}/{scope}", creds.access_key_id);
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
        // S3 signs the token as part of the canonical query string.
        if let Some(token) = &creds.session_token {
            query.push(("X-Amz-Security-Token".into(), token.clone()));
        }
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

        let signing_key = self.signing_key(date_stamp, &creds.secret_access_key);
        let signature = hex_encode(&hmac_sha256(&signing_key, string_to_sign.as_bytes()));

        Ok(format!(
            "{}://{}{}?{canonical_query}&X-Amz-Signature={signature}",
            self.scheme(),
            host,
            path,
        ))
    }
}

fn unix_now() -> Result<u64, StorageError> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| StorageError::Io(format!("clock: {e}")))?
        .as_secs())
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
        let put = PutConditions {
            content_type: content_type.map(str::to_owned),
            ..PutConditions::default()
        };
        let resp = self
            .signed_request("PUT", key, &[], Body::Bytes(data), &put)
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(StorageError::Io(format!(
                "S3 PUT {key} -> {status}: {body}"
            )));
        }
        Ok(())
    }

    /// One PUT up to 5 GiB, a multipart upload above, up to 5 TiB. Over
    /// https the body goes as `UNSIGNED-PAYLOAD`; over plain http each
    /// chunk is signed.
    async fn save_stream(
        &self,
        key: &str,
        len: u64,
        body: ByteStream,
        put: &PutConditions,
    ) -> Result<(), StorageError> {
        validate_key(key)?;
        check_declared_len(key, len, put)?;
        let signed = self.scheme() == "http";
        if len > self.multipart.max_single {
            return self.put_multipart(key, len, body, put, signed).await;
        }
        self.put_stream(key, len, body, put, signed).await
    }

    async fn load(&self, key: &str) -> Result<Vec<u8>, StorageError> {
        let bytes = self
            .get(key)
            .await?
            .bytes()
            .await
            .map_err(|e| StorageError::Io(format!("read body: {e}")))?;
        // Reuses the buffer when the body is uniquely owned.
        Ok(Vec::from(bytes))
    }

    async fn load_stream(&self, key: &str) -> Result<ByteStream, StorageError> {
        Ok(ByteStream::from(self.get(key).await?))
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        let resp = self
            .signed_request("DELETE", key, &[], Body::Bytes(b""), &PutConditions::new())
            .await?;
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
        let resp = self
            .signed_request("HEAD", key, &[], Body::Bytes(b""), &PutConditions::new())
            .await?;
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
            .await
            .ok()
    }

    async fn presigned_put_url(
        &self,
        key: &str,
        ttl: std::time::Duration,
        put: &PutConditions,
    ) -> Option<String> {
        self.build_presigned_url("PUT", key, ttl.as_secs(), put)
            .await
            .ok()
    }

    /// Without `s3:ListBucket`, S3 answers a missing key with 403, not
    /// 404; that stays an error, so a finalize fails closed.
    async fn metadata(&self, key: &str) -> Result<Option<ObjectMeta>, StorageError> {
        let resp = self
            .signed_request("HEAD", key, &[], Body::Bytes(b""), &PutConditions::new())
            .await?;
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

/// Format unix seconds as `YYYYMMDDTHHMMSSZ`, which SigV4 wants.
fn format_amz_date(unix_secs: u64) -> String {
    let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(unix_secs as i64, 0)
        .unwrap_or_else(chrono::Utc::now);
    dt.format("%Y%m%dT%H%M%SZ").to_string()
}

/// Aborts a multipart upload whose future was dropped. Best effort: a
/// spawned DELETE, so it needs a running tokio runtime.
struct AbortOnDrop(Option<(S3Storage, String, String)>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        let Some((s3, key, upload_id)) = self.0.take() else {
            return;
        };
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                let _ = s3.abort_multipart(&key, &upload_id).await;
            });
        }
    }
}

/// The text of the first `<tag>` in `xml`, unescaped.
fn xml_text(xml: &str, tag: &str) -> Option<String> {
    let start = xml.find(&format!("<{tag}>"))? + tag.len() + 2;
    let end = start + xml[start..].find(&format!("</{tag}>"))?;
    Some(
        xml[start..end]
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&"),
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// The SHA-256, HMAC and hex helpers live in `crate::crypto`.
use crate::crypto::{hex_encode, hmac_sha256, sha256_hex};

/// Signs each chunk of an `aws-chunked` body, chained from the
/// request's own signature.
struct ChunkSigner {
    key: Arc<[u8]>,
    amz_date: String,
    scope: String,
    /// The last signature: the request's, then each chunk's.
    prev: String,
}

impl ChunkSigner {
    /// `data` framed as `<hex len>;chunk-signature=<sig>\r\n<data>\r\n`.
    fn frame(&mut self, data: &[u8]) -> bytes::Bytes {
        let sts = format!(
            "AWS4-HMAC-SHA256-PAYLOAD\n{}\n{}\n{}\n{}\n{}",
            self.amz_date,
            self.scope,
            self.prev,
            sha256_hex(b""),
            sha256_hex(data)
        );
        self.prev = hex_encode(&hmac_sha256(&self.key, sts.as_bytes()));
        let head = format!("{:x};chunk-signature={}\r\n", data.len(), self.prev);
        let mut out = bytes::BytesMut::with_capacity(head.len() + data.len() + 2);
        out.extend_from_slice(head.as_bytes());
        out.extend_from_slice(data);
        out.extend_from_slice(b"\r\n");
        out.freeze()
    }
}

/// The length of `len` bytes framed in `chunk`-sized `aws-chunked`
/// pieces, the empty final one included.
fn aws_chunked_len(len: u64, chunk: u64) -> u64 {
    // `<hex>;chunk-signature=` + 64 hex + CRLF, the data, CRLF.
    let frame = |n: u64| format!("{n:x}").len() as u64 + 17 + 64 + 2 + n + 2;
    let rest = len % chunk;
    (len / chunk) * frame(chunk) + if rest > 0 { frame(rest) } else { 0 } + frame(0)
}

/// Re-chunk `body` into `chunk`-sized signed frames, then the empty final
/// one. Holds at most one chunk plus one incoming piece.
fn aws_chunked(body: ByteStream, signer: ChunkSigner, chunk: usize) -> ByteStream {
    struct State {
        body: ByteStream,
        signer: ChunkSigner,
        buf: bytes::BytesMut,
        ended: bool,
        done: bool,
    }
    let state = State {
        body,
        signer,
        buf: bytes::BytesMut::new(),
        ended: false,
        done: false,
    };
    ByteStream::new(futures_util::stream::unfold(
        state,
        move |mut st| async move {
            use futures_util::StreamExt as _;
            if st.done {
                return None;
            }
            while !st.ended && st.buf.len() < chunk {
                match st.body.next().await {
                    Some(Ok(b)) => st.buf.extend_from_slice(&b),
                    Some(Err(e)) => {
                        st.done = true;
                        return Some((Err(e), st));
                    }
                    None => st.ended = true,
                }
            }
            let data = st.buf.split_to(st.buf.len().min(chunk));
            // An empty frame only comes once the body has ended: the last one.
            st.done = data.is_empty();
            let frame = st.signer.frame(&data);
            Some((Ok(frame), st))
        },
    ))
}

/// Derive the SigV4 signing key by hashing the secret through the
/// scope chain: date, then region, then service.
fn derive_signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

#[cfg(test)]
mod tests {
    use super::super::transport::testing::{
        header, paced_body, serve, serve_counted, serve_many, Reply, Served,
    };
    use super::*;

    fn cfg() -> S3Config {
        S3Config::new("examplebucket", "us-east-1")
    }

    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
    const TOKEN: &str = "FwoGZXIvYXdzEXAMPLE/token+with=chars";

    fn creds() -> S3Credentials {
        S3Credentials::new("AKIAIOSFODNN7EXAMPLE", SECRET)
    }

    fn store(c: S3Config) -> S3Storage {
        S3Storage::new(c, creds())
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
        let s = store(cfg());
        assert_eq!(s.host(), "examplebucket.s3.us-east-1.amazonaws.com");
    }

    #[test]
    fn host_aws_path_style_uses_regional_endpoint() {
        let mut c = cfg();
        c.path_style = true;
        let s = store(c);
        assert_eq!(s.host(), "s3.us-east-1.amazonaws.com");
    }

    #[test]
    fn host_custom_endpoint_strips_scheme() {
        let mut c = cfg();
        c.path_style = true;
        c.endpoint = Some("https://abc.r2.cloudflarestorage.com/".into());
        let s = store(c);
        assert_eq!(s.host(), "abc.r2.cloudflarestorage.com");
    }

    #[test]
    fn host_http_endpoint_recognized() {
        let mut c = cfg();
        c.path_style = true;
        c.endpoint = Some("http://localhost:9000".into());
        let s = store(c);
        assert_eq!(s.host(), "localhost:9000");
        assert_eq!(s.scheme(), "http");
    }

    #[test]
    fn full_url_aws_virtual_hosted() {
        let s = store(cfg());
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
        let s = store(c);
        assert_eq!(
            s.full_url("avatars/alice.png"),
            "https://minio.example.com/examplebucket/avatars/alice.png"
        );
    }

    #[test]
    fn url_returns_stable_public_link() {
        let s = store(cfg());
        assert!(s
            .url("foo.txt")
            .unwrap()
            .ends_with("examplebucket.s3.us-east-1.amazonaws.com/foo.txt"));
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
        let s = store(cfg());
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
        let s = store(cfg());
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
        let s = store(cfg());
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
        let s = store(cfg());
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
        let s = store(cfg());
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
        let s = store(cfg());
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
        let s = store(cfg());
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
        let s = store(c);
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
        let s = store(supabase_cfg());
        assert_eq!(s.host(), "abc123.storage.supabase.co");
    }

    #[test]
    fn endpoint_path_prefix_is_kept_in_the_request_path() {
        let s = store(supabase_cfg());
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
        let s = store(S3Config {
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
        let virt = store(cfg());
        assert_eq!(virt.host(), "examplebucket.s3.us-east-1.amazonaws.com");
        assert_eq!(virt.endpoint_prefix(), "");
        assert_eq!(virt.key_path("a.png"), "/a.png");

        let path = store(S3Config {
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
        let s = store(supabase_cfg());
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
        let (endpoint, task) = serve(status, extra, Vec::new()).await;
        (
            endpoint,
            tokio::spawn(async move { task.await.unwrap().head }),
        )
    }

    fn mock_storage(endpoint: String) -> S3Storage {
        store(S3Config {
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
        let (method, target) = (req_line.next().unwrap(), req_line.next().unwrap());
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
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
        let cr = format!("{method}\n{path}\n{query}\n{canonical_headers}\n{signed}\n{payload}");
        let scope = format!("{}/us-east-1/s3/aws4_request", &amz_date[..8]);
        let sts = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
            sha256_hex(cr.as_bytes())
        );
        let key = derive_signing_key(SECRET, &amz_date[..8], "us-east-1", "s3");
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

    /// Temporary keys send their session token, and sign it.
    #[tokio::test]
    async fn session_token_is_sent_and_signed() {
        let (ep, req) = mock(200).await;
        let mut c = cfg();
        c.endpoint = Some(ep);
        c.path_style = true;
        S3Storage::new(c, creds().with_session_token(TOKEN))
            .save("a.txt", b"txt")
            .await
            .unwrap();
        let head = req.await.unwrap();
        let lower = head.to_ascii_lowercase();
        assert!(
            head.contains(&format!("x-amz-security-token: {TOKEN}")),
            "{head}"
        );
        assert!(
            lower.contains(
                "signedheaders=host;x-amz-content-sha256;x-amz-date;x-amz-security-token"
            ),
            "{head}"
        );
        verify_sigv4(&head);
    }

    /// A presigned URL carries the token inside the signed query.
    #[tokio::test]
    async fn presigned_url_carries_the_session_token() {
        let s = S3Storage::new(cfg(), creds().with_session_token(TOKEN));
        let url = s
            .presigned_get_url("a.png", std::time::Duration::from_secs(60))
            .await
            .unwrap();
        let (_, query) = url.split_once('?').unwrap();
        let (signed, _) = query.split_once("&X-Amz-Signature=").unwrap();
        assert!(
            signed.contains("X-Amz-Security-Token=FwoGZXIvYXdzEXAMPLE%2Ftoken%2Bwith%3Dchars"),
            "{url}"
        );
    }

    /// Keys that rotate mid-day are signed with the new secret.
    #[tokio::test]
    async fn refreshed_credentials_are_used_on_the_next_request() {
        struct Rotating(std::sync::atomic::AtomicUsize);
        #[async_trait]
        impl S3CredentialProvider for Rotating {
            async fn credentials(&self) -> Result<S3Credentials, StorageError> {
                let n = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(S3Credentials::new(format!("KEY{n}"), format!("secret{n}")))
            }
        }
        let s = S3Storage::new(cfg(), Rotating(0.into()));
        let a = s
            .presigned_get_url("a", std::time::Duration::from_secs(60))
            .await
            .unwrap();
        let b = s
            .presigned_get_url("a", std::time::Duration::from_secs(60))
            .await
            .unwrap();
        assert!(a.contains("X-Amz-Credential=KEY0%2F"), "{a}");
        assert!(b.contains("X-Amz-Credential=KEY1%2F"), "{b}");
        assert_eq!(s.derivations.load(std::sync::atomic::Ordering::Relaxed), 2);
    }

    #[test]
    fn credentials_debug_hides_the_secret_and_token() {
        let s = format!("{:?}", creds().with_session_token(TOKEN));
        assert!(s.contains("AKIAIOSFODNN7EXAMPLE"), "{s}");
        assert!(!s.contains(SECRET) && !s.contains(TOKEN), "{s}");
        let s = format!("{:?}", cfg());
        assert!(!s.contains(SECRET), "{s}");
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
        let budget = upload_timeout(SHORT_READ, UPLOAD_LEN as u64);
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
        let s = store(S3Config {
            endpoint: Some("https://abc.r2.cloudflarestorage.com".into()),
            ..cfg()
        });
        assert_eq!(
            s.full_url("a.png"),
            "https://examplebucket.abc.r2.cloudflarestorage.com/a.png"
        );
        // An endpoint that already names the bucket is left alone.
        let s = store(S3Config {
            endpoint: Some("https://examplebucket.s3.us-east-1.amazonaws.com".into()),
            ..cfg()
        });
        assert_eq!(s.host(), "examplebucket.s3.us-east-1.amazonaws.com");
    }

    // -------- streamed uploads --------

    fn pieces(data: &[u8], size: usize) -> ByteStream {
        let parts: Vec<_> = data
            .chunks(size)
            .map(|c| Ok(bytes::Bytes::copy_from_slice(c)))
            .collect();
        ByteStream::new(futures_util::stream::iter(parts))
    }

    fn sample(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    /// AWS's chunked-upload example: 66560 bytes of `a` in 64 KiB chunks.
    /// The seed comes from the documented canonical request.
    #[tokio::test]
    async fn chunk_signatures_match_the_aws_example() {
        let amz_date = "20130524T000000Z";
        let scope = "20130524/us-east-1/s3/aws4_request";
        let key: Arc<[u8]> = derive_signing_key(SECRET, "20130524", "us-east-1", "s3").into();
        let cr = "PUT\n/examplebucket/chunkObject.txt\n\n\
                  content-encoding:aws-chunked\ncontent-length:66824\nhost:s3.amazonaws.com\n\
                  x-amz-content-sha256:STREAMING-AWS4-HMAC-SHA256-PAYLOAD\n\
                  x-amz-date:20130524T000000Z\nx-amz-decoded-content-length:66560\n\
                  x-amz-storage-class:REDUCED_REDUNDANCY\n\n\
                  content-encoding;content-length;host;x-amz-content-sha256;x-amz-date;\
                  x-amz-decoded-content-length;x-amz-storage-class\n\
                  STREAMING-AWS4-HMAC-SHA256-PAYLOAD";
        let sts = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
            sha256_hex(cr.as_bytes())
        );
        let seed = hex_encode(&hmac_sha256(&key, sts.as_bytes()));
        assert_eq!(
            seed,
            "4f232c4386841ef735655705268965c44a0e4690baa4adea153f7db9fa80a0a9"
        );
        let signer = ChunkSigner {
            key,
            amz_date: amz_date.into(),
            scope: scope.into(),
            prev: seed,
        };
        let mut framed = aws_chunked(pieces(&[b'a'; 66560], 10_000), signer, 65536);
        let mut body = Vec::new();
        while let Some(f) = futures_util::StreamExt::next(&mut framed).await {
            body.extend_from_slice(&f.unwrap());
        }
        assert_eq!(body.len(), 66824);
        assert_eq!(aws_chunked_len(66560, 65536), 66824);
        let frames = decode_aws_chunked(&body);
        let sigs: Vec<_> = frames
            .iter()
            .map(|(len, sig, _)| (*len, sig.as_str()))
            .collect();
        assert_eq!(
            sigs,
            [
                (
                    65536,
                    "ad80c730a21e5b8d04586a2213dd63b9a0e99e0e2307b0ade35a65485a288648"
                ),
                (
                    1024,
                    "0055627c9e194cb4542bae2aa5492e3c1575bbb81b612b7d234b86a503ef5497"
                ),
                (
                    0,
                    "b6c6ea8a5354eaf15b3cb7646744f4275b71ea724fed81ceb9323e279d449df9"
                ),
            ]
        );
    }

    /// `(len, signature, data)` per frame of an `aws-chunked` body.
    fn decode_aws_chunked(mut body: &[u8]) -> Vec<(usize, String, Vec<u8>)> {
        let mut out = Vec::new();
        while !body.is_empty() {
            let eol = body.windows(2).position(|w| w == b"\r\n").unwrap();
            let line = std::str::from_utf8(&body[..eol]).unwrap();
            let (hex, sig) = line.split_once(";chunk-signature=").unwrap();
            let len = usize::from_str_radix(hex, 16).unwrap();
            let data = body[eol + 2..eol + 2 + len].to_vec();
            assert_eq!(&body[eol + 2 + len..eol + 4 + len], b"\r\n");
            body = &body[eol + 4 + len..];
            out.push((len, sig.to_owned(), data));
        }
        out
    }

    /// Plain http: each chunk is signed, chained from the request's
    /// signature, and the length and `If-None-Match` are signed.
    #[tokio::test]
    async fn save_stream_over_http_signs_every_chunk() {
        let (ep, req) = serve(200, String::new(), Vec::new()).await;
        let data = sample(150_000);
        let put = PutConditions::new()
            .content_type("text/csv")
            .content_length(150_000)
            .create_only();
        mock_storage(ep)
            .save_stream("big.csv", 150_000, pieces(&data, 9_999), &put)
            .await
            .unwrap();
        let got = req.await.unwrap();
        let head = &got.head;
        let h = |name| header(head, name).unwrap_or_default();
        assert_eq!(
            h("x-amz-content-sha256"),
            "STREAMING-AWS4-HMAC-SHA256-PAYLOAD"
        );
        assert_eq!(h("x-amz-decoded-content-length"), "150000");
        assert_eq!(
            h("content-length"),
            aws_chunked_len(150_000, 65536).to_string()
        );
        assert_eq!(h("if-none-match"), "*");
        assert_eq!(h("content-type"), "text/csv");
        assert!(header(head, "transfer-encoding").is_none(), "{head}");
        assert!(header(head, "content-encoding").is_none(), "{head}");
        assert!(
            h("authorization").contains(
                "SignedHeaders=content-length;content-type;host;if-none-match;\
                 x-amz-content-sha256;x-amz-date;x-amz-decoded-content-length,"
            ),
            "{head}"
        );
        verify_sigv4(head);

        let amz_date = h("x-amz-date");
        let scope = format!("{}/us-east-1/s3/aws4_request", &amz_date[..8]);
        let key = derive_signing_key(SECRET, &amz_date[..8], "us-east-1", "s3");
        let auth = h("authorization");
        let mut prev = auth.rsplit("Signature=").next().unwrap().to_owned();
        let frames = decode_aws_chunked(&got.body);
        let lens: Vec<_> = frames.iter().map(|f| f.0).collect();
        assert_eq!(lens, [65536, 65536, 150_000 - 2 * 65536, 0]);
        let mut sent = Vec::new();
        for (_, sig, chunk) in &frames {
            let sts = format!(
                "AWS4-HMAC-SHA256-PAYLOAD\n{amz_date}\n{scope}\n{prev}\n{}\n{}",
                sha256_hex(b""),
                sha256_hex(chunk)
            );
            prev = hex_encode(&hmac_sha256(&key, sts.as_bytes()));
            assert_eq!(sig, &prev, "a chunk signature does not chain");
            sent.extend_from_slice(chunk);
        }
        assert_eq!(sent, data);
    }

    /// https sends the body as `UNSIGNED-PAYLOAD`, its length signed.
    #[tokio::test]
    async fn https_save_stream_sends_an_unsigned_payload() {
        let (ep, req) = serve(200, String::new(), Vec::new()).await;
        let data = sample(100_000);
        mock_storage(ep)
            .put_stream(
                "a.bin",
                100_000,
                pieces(&data, 7_000),
                &PutConditions::new(),
                false,
            )
            .await
            .unwrap();
        let got = req.await.unwrap();
        let head = &got.head;
        let h = |name| header(head, name).unwrap_or_default();
        assert_eq!(h("x-amz-content-sha256"), "UNSIGNED-PAYLOAD");
        assert_eq!(h("content-length"), "100000");
        assert!(header(head, "transfer-encoding").is_none(), "{head}");
        assert!(header(head, "if-none-match").is_none(), "{head}");
        assert!(
            h("authorization")
                .contains("SignedHeaders=content-length;host;x-amz-content-sha256;x-amz-date,"),
            "{head}"
        );
        verify_sigv4(head);
        assert_eq!(got.body, data);
    }

    #[tokio::test]
    async fn create_only_save_stream_on_an_existing_key_is_refused() {
        for (status, signed) in [(412, true), (412, false), (409, true)] {
            let (ep, _req) = serve(status, String::new(), Vec::new()).await;
            let err = mock_storage(ep)
                .put_stream(
                    "k",
                    3,
                    pieces(b"abc", 3),
                    &PutConditions::new().create_only(),
                    signed,
                )
                .await
                .unwrap_err();
            assert!(err.to_string().contains("already exists"), "{err}");
        }
    }

    /// Refused before any request: the endpoint is never reached.
    #[tokio::test]
    async fn save_stream_refuses_what_s3_cannot_store() {
        let s = mock_storage("http://127.0.0.1:9".into());
        let err = s
            .save_stream("k", MAX_OBJECT + 1, pieces(b"x", 1), &PutConditions::new())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("5 TiB"), "{err}");
        let err = s
            .save_stream(
                "k",
                3,
                pieces(b"abc", 3),
                &PutConditions::new().content_length(4),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("does not match"), "{err}");
    }

    /// A body that fails or ends short never completes the request, so
    /// S3 stores nothing.
    #[tokio::test]
    async fn a_failed_or_short_stream_never_completes_the_upload() {
        let data = sample(300_000);
        for signed in [true, false] {
            for fail in [true, false] {
                let (ep, req) = serve(200, String::new(), Vec::new()).await;
                let first = bytes::Bytes::copy_from_slice(&data[..100_000]);
                let tail = if fail {
                    Err(StorageError::Io("client went away".into()))
                } else {
                    Ok(bytes::Bytes::new())
                };
                // Later, so the head and the first chunk are on the wire.
                let tail = futures_util::stream::once(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    tail
                });
                let body = ByteStream::new(futures_util::StreamExt::chain(
                    futures_util::stream::iter([Ok(first)]),
                    tail,
                ));
                let err = mock_storage(ep)
                    .put_stream("k", 300_000, body, &PutConditions::new(), signed)
                    .await
                    .unwrap_err();
                let want = if fail {
                    "client went away"
                } else {
                    "bytes declared"
                };
                assert!(err.to_string().contains(want), "signed {signed}: {err}");
                let got = tokio::time::timeout(std::time::Duration::from_secs(5), req);
                let got = got.await.expect("no request was sent").unwrap();
                assert!(
                    !got.head.is_empty() && got.body_len < got.declared,
                    "signed {signed}, fail {fail}: the server got the whole body"
                );
            }
        }
    }

    #[tokio::test]
    async fn load_stream_streams_the_body_and_maps_404() {
        let data = sample(200_000);
        let (ep, _req) = serve(200, String::new(), data.clone()).await;
        let mut stream = mock_storage(ep).load_stream("a.bin").await.unwrap();
        let mut got = Vec::new();
        while let Some(c) = futures_util::StreamExt::next(&mut stream).await {
            got.extend_from_slice(&c.unwrap());
        }
        assert_eq!(got, data);
        let (ep, _req) = serve(404, String::new(), Vec::new()).await;
        let err = mock_storage(ep).load_stream("a.bin").await.unwrap_err();
        assert!(matches!(err, StorageError::NotFound(_)), "{err}");
        let (ep, _req) = serve(403, String::new(), Vec::new()).await;
        let err = mock_storage(ep).load_stream("a.bin").await.unwrap_err();
        assert!(matches!(err, StorageError::Io(_)), "{err}");
    }

    /// The body is pulled only as fast as the server takes it, so memory
    /// stays flat however large the object.
    #[tokio::test]
    async fn save_stream_never_buffers_the_object() {
        use std::sync::atomic::{AtomicU64, Ordering};
        // Chunk signing hashes in an unoptimized build, so less data there.
        for (signed, mib) in [(false, 256usize), (true, 32)] {
            let received = Arc::new(AtomicU64::new(0));
            let ahead = Arc::new(AtomicU64::new(0));
            let (ep, req) =
                serve_counted(200, String::new(), Vec::new(), Some(received.clone())).await;
            let body = paced_body(mib, 1 << 20, received, ahead.clone());
            let len = (mib as u64) << 20;
            mock_storage(ep)
                .put_stream("big.bin", len, body, &PutConditions::new(), signed)
                .await
                .unwrap();
            let got = req.await.unwrap();
            assert_eq!(got.body_len, got.declared);
            let ahead = ahead.load(Ordering::SeqCst);
            assert!(ahead <= 16 << 20, "signed {signed}: {ahead} bytes held");
        }
    }

    // -------- multipart uploads --------

    /// Parts of 50,000 bytes: 5 parts cover anything up to 250,000.
    const SMALL: Split = Split {
        max_single: 100_000,
        min_part: 10_000,
        max_parts: 5,
    };

    const UPLOAD_ID: &str = "up/1+x";

    fn request_line(s: &Served) -> String {
        s.head.lines().next().unwrap_or_default().to_owned()
    }

    /// A bucket that takes a multipart upload; `complete` answers Complete
    /// and `part` overrides the answer to a part by number.
    fn s3_bucket(
        complete: Reply,
        part: impl Fn(u32) -> Option<Reply> + Send + Sync + 'static,
    ) -> impl Fn(&str) -> Reply + Send + Sync + 'static {
        move |head: &str| {
            let line = head.lines().next().unwrap_or_default();
            let (method, target) = line.split_once(' ').unwrap();
            if method == "POST" && target.contains("?uploads=") {
                let xml = format!(
                    "<InitiateMultipartUploadResult><UploadId>{}</UploadId></InitiateMultipartUploadResult>",
                    xml_escape(UPLOAD_ID)
                );
                return (200, String::new(), xml.into_bytes());
            }
            match method {
                "PUT" => {
                    let n: Option<u32> = target
                        .split(['?', '&'])
                        .find_map(|p| p.strip_prefix("partNumber="))
                        .and_then(|v| v.split(' ').next()?.parse().ok());
                    // No part number: a single PUT.
                    let Some(n) = n else {
                        return (200, String::new(), Vec::new());
                    };
                    part(n).unwrap_or((200, format!("etag: \"e{n}\"\r\n"), Vec::new()))
                }
                "POST" => complete.clone(),
                "DELETE" => (204, String::new(), Vec::new()),
                _ => (404, String::new(), Vec::new()),
            }
        }
    }

    fn ok_complete() -> Reply {
        (
            200,
            String::new(),
            b"<CompleteMultipartUploadResult><ETag>\"x-5\"</ETag></CompleteMultipartUploadResult>"
                .to_vec(),
        )
    }

    fn multipart_storage(ep: String, split: Split) -> S3Storage {
        let mut s = mock_storage(ep);
        s.multipart = split;
        s
    }

    fn served(log: &std::sync::Mutex<Vec<Served>>) -> Vec<Served> {
        std::mem::take(&mut *log.lock().unwrap())
    }

    fn aborted(log: &[Served]) -> bool {
        log.iter().any(|s| {
            request_line(s).starts_with(&format!(
                "DELETE /examplebucket/k?uploadId={} ",
                encode_query(UPLOAD_ID)
            ))
        })
    }

    fn completed(log: &[Served]) -> bool {
        log.iter()
            .any(|s| request_line(s).starts_with("POST /examplebucket/k?uploadId="))
    }

    #[test]
    fn ten_thousand_parts_cover_the_largest_object() {
        let part = MULTIPART.part_size(MAX_OBJECT);
        assert!(MAX_OBJECT.div_ceil(part) <= 10_000, "{part}");
        assert!(part <= MAX_PUT, "{part}");
        assert_eq!(MULTIPART.part_size(MAX_PUT + 1), 64 << 20);
        assert_eq!(MULTIPART.max_single, MAX_PUT);
    }

    /// Above one PUT: create, parts in order, Complete with every ETag.
    /// The type goes on create, `If-None-Match` on Complete only.
    #[tokio::test]
    async fn multipart_upload_sends_every_part_then_completes() {
        let data = sample(250_000);
        for signed in [true, false] {
            let (ep, log) = serve_many(s3_bucket(ok_complete(), |_| None), None).await;
            let put = PutConditions::new()
                .content_type("text/csv")
                .content_length(250_000)
                .create_only();
            multipart_storage(ep, SMALL)
                .put_multipart("k", 250_000, pieces(&data, 9_999), &put, signed)
                .await
                .unwrap();
            let log = served(&log);
            let lines: Vec<_> = log.iter().map(request_line).collect();
            let id = encode_query(UPLOAD_ID);
            let mut want = vec![
                "HEAD /examplebucket/k HTTP/1.1".to_owned(),
                "POST /examplebucket/k?uploads= HTTP/1.1".to_owned(),
            ];
            want.extend(
                (1..=5)
                    .map(|n| format!("PUT /examplebucket/k?partNumber={n}&uploadId={id} HTTP/1.1")),
            );
            want.push(format!("POST /examplebucket/k?uploadId={id} HTTP/1.1"));
            assert_eq!(lines, want, "signed {signed}");
            let mut sent = Vec::new();
            for s in &log {
                verify_sigv4(&s.head);
                let h = |name| header(&s.head, name);
                let create = request_line(s).contains("?uploads=");
                let complete = request_line(s).starts_with("POST") && !create;
                assert_eq!(h("content-type").is_some(), create, "{}", s.head);
                assert_eq!(h("if-none-match").is_some(), complete, "{}", s.head);
                if request_line(s).starts_with("PUT") {
                    let part = if signed {
                        let frames = decode_aws_chunked(&s.body);
                        frames.into_iter().flat_map(|f| f.2).collect()
                    } else {
                        s.body.clone()
                    };
                    assert_eq!(part.len(), 50_000);
                    sent.extend(part);
                }
            }
            assert_eq!(sent, data, "signed {signed}");
            let xml = String::from_utf8(log.last().unwrap().body.clone()).unwrap();
            let want: String = (1..=5)
                .map(|n| format!("<Part><PartNumber>{n}</PartNumber><ETag>\"e{n}\"</ETag></Part>"))
                .collect();
            assert_eq!(
                xml,
                format!("<CompleteMultipartUpload>{want}</CompleteMultipartUpload>")
            );
        }
    }

    /// `save_stream` keeps one PUT up to the limit and goes multipart above.
    #[tokio::test]
    async fn save_stream_goes_multipart_only_above_one_put() {
        for (len, multipart) in [(100_000usize, false), (100_001, true)] {
            let (ep, log) = serve_many(s3_bucket(ok_complete(), |_| None), None).await;
            multipart_storage(ep, SMALL)
                .save_stream(
                    "k",
                    len as u64,
                    pieces(&sample(len), 7_000),
                    &PutConditions::new(),
                )
                .await
                .unwrap();
            let lines: Vec<_> = served(&log).iter().map(request_line).collect();
            let single = vec!["PUT /examplebucket/k HTTP/1.1".to_owned()];
            assert_eq!(lines != single, multipart, "{len}: {lines:?}");
        }
    }

    #[tokio::test]
    async fn a_failed_part_aborts_the_upload() {
        let fail = |n| (n == 2).then(|| (500, String::new(), b"<Error/>".to_vec()));
        let (ep, log) = serve_many(s3_bucket(ok_complete(), fail), None).await;
        let err = multipart_storage(ep, SMALL)
            .put_multipart(
                "k",
                250_000,
                pieces(&sample(250_000), 9_999),
                &PutConditions::new(),
                true,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("#2 -> 500"), "{err}");
        let log = served(&log);
        assert!(aborted(&log), "no abort");
        assert!(!completed(&log), "completed after a failed part");
    }

    /// A body shorter or longer than declared never completes.
    #[tokio::test]
    async fn a_body_of_another_size_aborts_the_upload() {
        for (real, want) in [(180_000usize, "ended after"), (260_000, "longer than")] {
            let (ep, log) = serve_many(s3_bucket(ok_complete(), |_| None), None).await;
            let err = multipart_storage(ep, SMALL)
                .put_multipart(
                    "k",
                    250_000,
                    pieces(&sample(real), 9_999),
                    &PutConditions::new(),
                    false,
                )
                .await
                .unwrap_err();
            assert!(err.to_string().contains(want), "{real}: {err}");
            let log = served(&log);
            assert!(aborted(&log), "{real}: no abort");
            assert!(!completed(&log), "{real}: completed");
        }
    }

    /// A key taken before Complete lands: the upload is refused and aborted.
    #[tokio::test]
    async fn create_only_complete_on_a_taken_key_is_refused() {
        let error = |code: &str| format!("<Error><Code>{code}</Code></Error>").into_bytes();
        for (reply, taken) in [
            ((412, String::new(), error("PreconditionFailed")), true),
            (
                (409, String::new(), error("ConditionalRequestConflict")),
                true,
            ),
            ((200, String::new(), error("PreconditionFailed")), true),
            ((200, String::new(), error("InternalError")), false),
        ] {
            let status = reply.0;
            let (ep, log) = serve_many(s3_bucket(reply, |_| None), None).await;
            let err = multipart_storage(ep, SMALL)
                .put_multipart(
                    "k",
                    150_000,
                    pieces(&sample(150_000), 9_999),
                    &PutConditions::new().create_only(),
                    true,
                )
                .await
                .unwrap_err();
            assert_eq!(
                err.to_string().contains("already exists"),
                taken,
                "{status}: {err}"
            );
            assert!(aborted(&served(&log)), "{status}: no abort");
        }
    }

    /// A key that is surely there is refused before any part is sent.
    #[tokio::test]
    async fn create_only_multipart_checks_the_key_first() {
        let bucket = s3_bucket(ok_complete(), |_| None);
        let (ep, log) = serve_many(
            move |head: &str| {
                if head.starts_with("HEAD ") {
                    (200, String::new(), Vec::new())
                } else {
                    bucket(head)
                }
            },
            None,
        )
        .await;
        let err = multipart_storage(ep, SMALL)
            .save_stream(
                "k",
                150_000,
                pieces(&sample(150_000), 9_999),
                &PutConditions::new().create_only(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
        assert_eq!(served(&log).len(), 1);
    }

    /// A caller that stops waiting still gets the upload aborted.
    #[tokio::test]
    async fn a_dropped_upload_is_aborted() {
        let (ep, log) = serve_many(s3_bucket(ok_complete(), |_| None), None).await;
        let s = multipart_storage(ep, SMALL);
        let first = bytes::Bytes::from(sample(60_000));
        let body = ByteStream::new(futures_util::StreamExt::chain(
            futures_util::stream::iter([Ok(first)]),
            futures_util::stream::pending(),
        ));
        let none = PutConditions::new();
        let upload = s.put_multipart("k", 250_000, body, &none, true);
        let cut = tokio::time::timeout(std::time::Duration::from_millis(500), upload);
        assert!(cut.await.is_err(), "the upload finished");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while !aborted(&log.lock().unwrap()) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "no abort after a drop"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(!completed(&log.lock().unwrap()));
    }

    /// Parts stream as they arrive: memory stays well under one part.
    #[tokio::test]
    async fn multipart_never_buffers_a_part() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let received = Arc::new(AtomicU64::new(0));
        let ahead = Arc::new(AtomicU64::new(0));
        let (ep, log) =
            serve_many(s3_bucket(ok_complete(), |_| None), Some(received.clone())).await;
        let split = Split {
            max_single: 8 << 20,
            min_part: 32 << 20,
            max_parts: 10_000,
        };
        let body = paced_body(128, 1 << 20, received, ahead.clone());
        multipart_storage(ep, split)
            .put_multipart("k", 128 << 20, body, &PutConditions::new(), false)
            .await
            .unwrap();
        let parts: Vec<_> = served(&log)
            .into_iter()
            .filter(|s| s.head.starts_with("PUT "))
            .map(|s| s.body_len)
            .collect();
        assert_eq!(parts, [32 << 20; 4]);
        let ahead = ahead.load(Ordering::SeqCst);
        assert!(ahead <= 16 << 20, "{ahead} bytes held");
    }

    /// Against MinIO: parts of 5 MiB, the least S3 takes. Skipped unless
    /// `RUSTANGO_S3_TEST_*` is set.
    #[tokio::test]
    async fn live_multipart_skipped_without_env() {
        let var = |k: &str| std::env::var(format!("RUSTANGO_S3_TEST_{k}")).ok();
        let (Some(access), Some(secret), Some(bucket)) = (var("KEY"), var("SECRET"), var("BUCKET"))
        else {
            eprintln!(
                "skipping live S3 multipart test — set RUSTANGO_S3_TEST_KEY / _SECRET / _BUCKET"
            );
            return;
        };
        let endpoint = var("ENDPOINT");
        let mut c = S3Config::new(bucket, var("REGION").unwrap_or_else(|| "us-east-1".into()));
        c.path_style = endpoint.is_some();
        c.endpoint = endpoint;
        let mut creds = S3Credentials::new(access, secret);
        if let Some(token) = var("SESSION_TOKEN") {
            creds = creds.with_session_token(token);
        }
        let mut s = S3Storage::new(c, creds);
        s.multipart = Split {
            max_single: 6 << 20,
            min_part: 5 << 20,
            max_parts: 10_000,
        };
        let key = format!("rustango-test/{}-multipart.bin", uuid::Uuid::new_v4());
        let data = sample(12 << 20);
        let once = PutConditions::new().content_type("text/csv").create_only();
        s.save_stream(&key, data.len() as u64, pieces(&data, 100_000), &once)
            .await
            .expect("multipart save_stream");
        assert_eq!(s.load(&key).await.unwrap(), data);
        let meta = s.metadata(&key).await.unwrap().unwrap();
        assert_eq!(meta.size, data.len() as u64);
        assert_eq!(meta.content_type.as_deref(), Some("text/csv"));
        // Past the pre-check, so Complete's `If-None-Match` decides.
        let upload_id = s.create_multipart(&key, &once).await.unwrap();
        let err = s
            .upload_parts(&key, &upload_id, 3, pieces(b"new", 3), &once, true)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
        s.abort_multipart(&key, &upload_id).await.unwrap();
        assert_eq!(
            s.load(&key).await.unwrap(),
            data,
            "Complete replaced the object"
        );
        let cut = format!("{key}.cut");
        let short = pieces(&data[..(7 << 20)], 100_000);
        assert!(s
            .save_stream(&cut, data.len() as u64, short, &PutConditions::new())
            .await
            .is_err());
        assert!(
            !s.exists(&cut).await.unwrap(),
            "a cut-short upload was stored"
        );
        s.delete(&key).await.unwrap();
    }
}
