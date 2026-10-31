//! [`Storage`] over Azure Blob Storage. Server calls are signed with
//! the account's Shared Key; presigned URLs are service SAS tokens.
//!
//! Plain HTTP over `reqwest`, signed with `hmac` and `sha2`, like the S3
//! backend — no Azure SDK.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::storage::azure::{AzureBlobStorage, AzureConfig};
//! use std::sync::Arc;
//!
//! // https://myaccount.blob.core.windows.net unless `.endpoint(..)` is set
//! let key = std::env::var("AZURE_STORAGE_KEY").unwrap();
//! let blobs = AzureBlobStorage::new(AzureConfig::new("myaccount", key, "uploads"))?;
//! let storage: rustango::storage::BoxedStorage = Arc::new(blobs);
//! storage.save("avatars/alice.png", &png_bytes).await?;
//! ```
//!
//! For Azurite set `endpoint` to `http://127.0.0.1:10000/devstoreaccount1`.
//!
//! ## What it does
//!
//! - Put Blob (block blobs up to 5000 MiB), Get Blob, Delete Blob and
//!   Get Blob Properties, signed with Shared Key.
//! - Presigned GET and PUT URLs as service SAS. A create-only PUT is
//!   granted `c` (create) without `w` (write), so the URL cannot
//!   replace an existing blob.
//! - [`AzureBlobStorage::create_container`] for setup scripts and tests.
//! - Streamed `save_stream` and `load_stream`. Above 5000 MiB the body
//!   goes as staged blocks, committed by one Put Block List.
//!
//! ## What it does not do
//!
//! - Sign headers into a SAS: Azure has no way to. The type and size in
//!   [`PutConditions`] are not enforced by the URL, so confirm a direct
//!   upload with [`Storage::metadata`]. `media` gives no direct-upload
//!   ticket on an Azure disk; use `save_bytes`.
//! - Microsoft Entra ID (user delegation SAS), listing.

use std::fmt::Write as _;

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;

use super::transport::{
    canonical_header_value, send_error, split_endpoint, stream_body, Http, Parts, Split,
    MAX_PRESIGN_SECS,
};
use super::{
    already_exists, check_declared_len, validate_key, ByteStream, ObjectMeta, PutConditions,
    Storage, StorageError,
};
use super::{encode_key, encode_query};
use crate::crypto::hmac_sha256;

/// REST version sent on every request and signed into every SAS.
/// 2019-12-12 raised Put Blob to 5000 MiB; 2020-12-06 set the SAS
/// string-to-sign layout used below.
const API_VERSION: &str = "2021-08-06";

/// The most one Put Blob takes: 5000 MiB.
const MAX_PUT_BLOB: u64 = 5000 << 20;

/// The most one block takes: 4000 MiB.
const MAX_BLOCK: u64 = 4000 << 20;

/// Above one Put Blob, blocks of at least 64 MiB; a blob holds 50,000.
const BLOCKS: Split = Split {
    max_single: MAX_PUT_BLOB,
    min_part: 64 << 20,
    max_parts: 50_000,
};

/// A request body.
enum Payload<'a> {
    /// No body at all.
    None,
    Bytes(&'a [u8]),
    /// Exactly this many bytes, sent as they arrive.
    Stream(ByteStream, u64),
}

/// How to reach the container. Build it with [`AzureConfig::new`].
#[derive(Clone)]
#[non_exhaustive]
pub struct AzureConfig {
    /// Storage account name.
    pub account: String,
    /// The account key, base64, as the portal shows it.
    pub account_key: String,
    pub container: String,
    /// `None` means `https://<account>.blob.core.windows.net`. Set it for
    /// Azurite, a sovereign cloud or a custom domain; a path is kept.
    pub endpoint: Option<String>,
}

impl AzureConfig {
    /// The account's default endpoint; see [`Self::endpoint`].
    #[must_use]
    pub fn new(
        account: impl Into<String>,
        account_key: impl Into<String>,
        container: impl Into<String>,
    ) -> Self {
        Self {
            account: account.into(),
            account_key: account_key.into(),
            container: container.into(),
            endpoint: None,
        }
    }

    /// Azurite, a sovereign cloud or a custom domain; a path is kept.
    #[must_use]
    pub fn endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }
}

impl std::fmt::Debug for AzureConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AzureConfig")
            .field("account", &self.account)
            .field("account_key", &"<redacted>")
            .field("container", &self.container)
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

/// Storage backed by an Azure Blob Storage container.
pub struct AzureBlobStorage {
    account: String,
    key: Vec<u8>,
    container: String,
    /// `scheme://host[:port]`.
    origin: String,
    /// The endpoint's path, such as `/devstoreaccount1` on Azurite.
    prefix: String,
    http: Http,
    blocks: Split,
}

impl AzureBlobStorage {
    /// # Errors
    /// [`StorageError::Io`] when the account or container is empty, or
    /// the key is not base64.
    pub fn new(cfg: AzureConfig) -> Result<Self, StorageError> {
        if cfg.account.is_empty() || cfg.container.is_empty() {
            return Err(StorageError::Io(
                "Azure storage needs an account and a container".into(),
            ));
        }
        let key = B64
            .decode(cfg.account_key.trim())
            .map_err(|e| StorageError::Io(format!("Azure account key is not base64: {e}")))?;
        let endpoint = cfg
            .endpoint
            .unwrap_or_else(|| format!("https://{}.blob.core.windows.net", cfg.account));
        let (origin, prefix) = split_endpoint(&endpoint);
        Ok(Self {
            account: cfg.account,
            key,
            container: cfg.container,
            origin,
            prefix,
            http: Http::defaults(),
            blocks: BLOCKS,
        })
    }

    /// Use your own reqwest client. Turn its redirects off. The default
    /// timeouts are those of `S3Storage::with_http`.
    #[must_use]
    pub fn with_http(mut self, http: reqwest::Client) -> Self {
        self.http = Http::Custom(http);
        self
    }

    /// Create the container. `Ok(false)` when it already exists.
    ///
    /// # Errors
    /// [`StorageError::Io`] on any other answer.
    pub async fn create_container(&self) -> Result<bool, StorageError> {
        let path = format!("{}/{}", self.prefix, self.container);
        let resp = self
            .send(
                reqwest::Method::PUT,
                &path,
                &[("restype", "container")],
                Payload::Bytes(b""),
                &PutConditions::new(),
                &[],
            )
            .await?;
        match resp.status() {
            reqwest::StatusCode::CREATED => Ok(true),
            reqwest::StatusCode::CONFLICT => Ok(false),
            status => Err(StorageError::Io(format!(
                "Azure create container {} -> {status}: {}",
                self.container,
                resp.text().await.unwrap_or_default()
            ))),
        }
    }

    fn path(&self, key: &str) -> String {
        format!("{}/{}/{}", self.prefix, self.container, encode_key(key))
    }

    fn sign(&self, string_to_sign: &str) -> String {
        B64.encode(hmac_sha256(&self.key, string_to_sign.as_bytes()))
    }

    /// One Shared Key request. `put` gives the content type and
    /// `If-None-Match: *`; its length is the body's own.
    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &[(&str, &str)],
        body: Payload<'_>,
        put: &PutConditions,
        extra: &[(&'static str, &str)],
    ) -> Result<reqwest::Response, StorageError> {
        let content_type = canonical_header_value(put.content_type.as_deref())?;
        let if_none_match = put.create_only.then_some("*");
        let len = match &body {
            Payload::None => 0,
            Payload::Bytes(b) => b.len() as u64,
            Payload::Stream(_, len) => *len,
        };
        let date = chrono::Utc::now()
            .format("%a, %d %b %Y %H:%M:%S GMT")
            .to_string();
        let mut ms: Vec<(&str, String)> = vec![
            ("x-ms-date", date),
            ("x-ms-version", API_VERSION.to_owned()),
        ];
        ms.extend(extra.iter().map(|(k, v)| (*k, (*v).to_owned())));
        ms.sort_by(|a, b| a.0.cmp(b.0));

        let mut query = query.to_vec();
        query.sort_unstable();
        let mut resource = format!("/{}{path}", self.account);
        for (k, v) in &query {
            let _ = write!(resource, "\n{k}:{v}");
        }
        let string_to_sign = shared_key_string(
            method.as_str(),
            len,
            content_type.as_deref(),
            if_none_match,
            &ms,
            &resource,
        );
        let auth = format!("SharedKey {}:{}", self.account, self.sign(&string_to_sign));

        let mut url = format!("{}{path}", self.origin);
        if !query.is_empty() {
            let q: Vec<String> = query
                .iter()
                .map(|(k, v)| format!("{}={}", encode_query(k), encode_query(v)))
                .collect();
            url = format!("{url}?{}", q.join("&"));
        }
        let mut req = self
            .http
            .request(method, &url, len)
            .header("authorization", auth);
        for (k, v) in &ms {
            req = req.header(*k, v);
        }
        if let Some(ct) = content_type {
            req = req.header("content-type", ct);
        }
        if let Some(v) = if_none_match {
            req = req.header("if-none-match", v);
        }
        match body {
            Payload::None => {}
            Payload::Bytes(b) => req = req.body(b.to_vec()),
            Payload::Stream(body, len) => {
                req = req
                    .header("content-length", len)
                    .body(stream_body(body, len));
            }
        }
        req.send().await.map_err(|e| send_error(&e))
    }

    /// A service SAS URL for one blob with `permissions`.
    fn sas_url(
        &self,
        key: &str,
        permissions: &str,
        ttl: std::time::Duration,
    ) -> Result<String, StorageError> {
        validate_key(key)?;
        let secs = ttl.as_secs().clamp(1, MAX_PRESIGN_SECS);
        let secs = i64::try_from(secs).unwrap_or(i64::MAX);
        let expiry = (chrono::Utc::now() + chrono::Duration::seconds(secs))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        Ok(self.sas_url_until(key, permissions, &expiry))
    }

    /// A read SAS URL; a type a browser would run comes back as a download.
    fn read_sas_url(&self, key: &str, ttl: std::time::Duration) -> Result<String, StorageError> {
        validate_key(key)?;
        let secs = ttl.as_secs().clamp(1, MAX_PRESIGN_SECS);
        let secs = i64::try_from(secs).unwrap_or(i64::MAX);
        let expiry = (chrono::Utc::now() + chrono::Duration::seconds(secs))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        Ok(self.sas_url_with(key, "r", &expiry, download_override(key)))
    }

    /// [`Self::sas_url`] expiring at `expiry` (`%Y-%m-%dT%H:%M:%SZ`).
    fn sas_url_until(&self, key: &str, permissions: &str, expiry: &str) -> String {
        self.sas_url_with(key, permissions, expiry, None)
    }

    /// [`Self::sas_url_until`] with optional `(rscd, rsct)` response overrides.
    fn sas_url_with(
        &self,
        key: &str,
        permissions: &str,
        expiry: &str,
        overrides: Option<(&str, &str)>,
    ) -> String {
        // Refuse plain http when the endpoint is https.
        let protocol = if self.origin.starts_with("https://") {
            "https"
        } else {
            ""
        };
        let resource = format!("/blob/{}/{}/{key}", self.account, self.container);
        let sig = self.sign(&sas_string_with(
            permissions,
            expiry,
            &resource,
            protocol,
            overrides,
        ));
        let mut query = vec![
            ("sv", API_VERSION),
            ("sr", "b"),
            ("sp", permissions),
            ("se", expiry),
        ];
        if !protocol.is_empty() {
            query.push(("spr", protocol));
        }
        if let Some((rscd, rsct)) = overrides {
            query.push(("rscd", rscd));
            query.push(("rsct", rsct));
        }
        query.push(("sig", sig.as_str()));
        let query: Vec<String> = query
            .iter()
            .map(|(k, v)| format!("{k}={}", encode_query(v)))
            .collect();
        format!("{}{}?{}", self.origin, self.path(key), query.join("&"))
    }

    /// Put Block for each part, then Put Block List. Uncommitted blocks
    /// are never visible and Azure discards them, so a failure stores nothing.
    async fn put_blocks(
        &self,
        key: &str,
        len: u64,
        body: ByteStream,
        put: &PutConditions,
    ) -> Result<(), StorageError> {
        if len > MAX_BLOCK.saturating_mul(self.blocks.max_parts) {
            return Err(StorageError::Io(format!(
                "Azure PUT {key}: {len} bytes is over what a block blob holds"
            )));
        }
        // Fail fast on a blob that is surely there; the list still decides.
        if put.create_only && matches!(self.exists(key).await, Ok(true)) {
            return Err(already_exists(key));
        }
        self.commit_blocks(key, len, body, put).await
    }

    /// [`Self::put_blocks`] past its checks.
    async fn commit_blocks(
        &self,
        key: &str,
        len: u64,
        body: ByteStream,
        put: &PutConditions,
    ) -> Result<(), StorageError> {
        let path = self.path(key);
        let block = self.blocks.part_size(len);
        // A fresh prefix, so a concurrent upload to the key never commits ours.
        let upload = uuid::Uuid::new_v4().simple().to_string();
        let mut parts = Parts::new(body, len);
        let mut ids = Vec::new();
        let mut sent = 0;
        while sent < len {
            let size = block.min(len - sent);
            // The same length for every id, as Azure wants.
            let id = B64.encode(format!("{upload}-{:05}", ids.len()));
            let (stream, back) = parts.take(size);
            let resp = self
                .send(
                    reqwest::Method::PUT,
                    &path,
                    &[("comp", "block"), ("blockid", &id)],
                    Payload::Stream(stream, size),
                    &PutConditions::new(),
                    &[],
                )
                .await?;
            let status = resp.status();
            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                return Err(StorageError::Io(format!(
                    "Azure Put Block {key} -> {status}: {text}"
                )));
            }
            parts = back.await.map_err(|_| {
                StorageError::Io(format!("Azure Put Block {key}: block not fully sent"))
            })?;
            ids.push(id);
            sent += size;
        }
        parts.finish().await?;

        let mut xml = String::from(r#"<?xml version="1.0" encoding="utf-8"?><BlockList>"#);
        for id in &ids {
            let _ = write!(xml, "<Latest>{id}</Latest>");
        }
        xml.push_str("</BlockList>");
        let content_type = canonical_header_value(put.content_type.as_deref())?;
        let mut extra = Vec::new();
        if let Some(ct) = content_type.as_deref() {
            extra.push(("x-ms-blob-content-type", ct));
        }
        let cond = PutConditions {
            create_only: put.create_only,
            ..PutConditions::default()
        };
        let resp = self
            .send(
                reqwest::Method::PUT,
                &path,
                &[("comp", "blocklist")],
                Payload::Bytes(xml.as_bytes()),
                &cond,
                &extra,
            )
            .await?;
        check_put(resp, key, put.create_only, "Put Block List").await
    }

    /// Get Blob, its status checked.
    async fn get(&self, key: &str) -> Result<reqwest::Response, StorageError> {
        let resp = self.blob(reqwest::Method::GET, key).await?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(StorageError::NotFound(key.to_owned()));
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(StorageError::Io(format!(
                "Azure GET {key} -> {status}: {body}"
            )));
        }
        Ok(resp)
    }

    async fn blob(
        &self,
        method: reqwest::Method,
        key: &str,
    ) -> Result<reqwest::Response, StorageError> {
        validate_key(key)?;
        self.send(
            method,
            &self.path(key),
            &[],
            Payload::None,
            &PutConditions::new(),
            &[],
        )
        .await
    }
}

/// A write's answer. Under `create_only`, 412 or 409 `BlobAlreadyExists`
/// means the blob is there; a lease or immutability 409 stays an error.
async fn check_put(
    resp: reqwest::Response,
    key: &str,
    create_only: bool,
    what: &str,
) -> Result<(), StorageError> {
    let status = resp.status();
    let code = resp
        .headers()
        .get("x-ms-error-code")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let taken = status == reqwest::StatusCode::PRECONDITION_FAILED
        || (status == reqwest::StatusCode::CONFLICT && code == "BlobAlreadyExists");
    if create_only && taken {
        return Err(already_exists(key));
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(StorageError::Io(format!(
            "Azure {what} {key} -> {status}: {body}"
        )));
    }
    Ok(())
}

/// The Shared Key string-to-sign for the Blob service. Content-Length
/// is empty for an empty body (versions 2015-02-21 and later).
fn shared_key_string(
    method: &str,
    len: u64,
    content_type: Option<&str>,
    if_none_match: Option<&str>,
    ms_headers: &[(&str, String)],
    resource: &str,
) -> String {
    let len = if len == 0 {
        String::new()
    } else {
        len.to_string()
    };
    let ct = content_type.unwrap_or("");
    let inm = if_none_match.unwrap_or("");
    let headers = ms_headers.iter().fold(String::new(), |mut out, (k, v)| {
        let _ = writeln!(out, "{k}:{v}");
        out
    });
    // VERB, Content-Encoding, -Language, -Length, -MD5, -Type, Date,
    // If-Modified-Since, If-Match, If-None-Match, If-Unmodified-Since, Range.
    format!("{method}\n\n\n{len}\n\n{ct}\n\n\n\n{inm}\n\n\n{headers}{resource}")
}

/// [`sas_string_with`] without response overrides.
#[cfg(test)]
fn sas_string(permissions: &str, expiry: &str, resource: &str, protocol: &str) -> String {
    sas_string_with(permissions, expiry, resource, protocol, None)
}

/// The service SAS string-to-sign (versions 2020-12-06 and later) with
/// no start, identifier, IP, snapshot or scope; `overrides` sets `(rscd, rsct)`.
fn sas_string_with(
    permissions: &str,
    expiry: &str,
    resource: &str,
    protocol: &str,
    overrides: Option<(&str, &str)>,
) -> String {
    let (rscd, rsct) = overrides.unwrap_or(("", ""));
    [
        permissions,
        "",
        expiry,
        resource,
        "",
        "",
        protocol,
        API_VERSION,
        "b",
        "",
        "",
        "",
        rscd,
        "",
        "",
        rsct,
    ]
    .join("\n")
}

/// `(rscd, rsct)` for a blob a browser would run or load as code when
/// opened from the storage origin: served as an untyped download.
fn download_override(key: &str) -> Option<(&'static str, &'static str)> {
    let ext = key.rsplit_once('.')?.1.to_ascii_lowercase();
    matches!(
        ext.as_str(),
        "html" | "htm" | "xhtml" | "svg" | "xml" | "js" | "mjs" | "css" | "wasm"
    )
    .then_some(("attachment", "application/octet-stream"))
}

#[async_trait]
impl Storage for AzureBlobStorage {
    async fn save(&self, key: &str, data: &[u8]) -> Result<(), StorageError> {
        self.save_with_content_type(key, data, None).await
    }

    async fn save_with_content_type(
        &self,
        key: &str,
        data: &[u8],
        content_type: Option<&str>,
    ) -> Result<(), StorageError> {
        validate_key(key)?;
        let put = PutConditions {
            content_type: content_type.map(str::to_owned),
            ..PutConditions::default()
        };
        let resp = self
            .send(
                reqwest::Method::PUT,
                &self.path(key),
                &[],
                Payload::Bytes(data),
                &put,
                &[("x-ms-blob-type", "BlockBlob")],
            )
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(StorageError::Io(format!(
                "Azure PUT {key} -> {status}: {body}"
            )));
        }
        Ok(())
    }

    /// One Put Blob up to 5000 MiB, staged blocks above.
    async fn save_stream(
        &self,
        key: &str,
        len: u64,
        body: ByteStream,
        put: &PutConditions,
    ) -> Result<(), StorageError> {
        validate_key(key)?;
        check_declared_len(key, len, put)?;
        if len > self.blocks.max_single {
            return self.put_blocks(key, len, body, put).await;
        }
        let resp = self
            .send(
                reqwest::Method::PUT,
                &self.path(key),
                &[],
                Payload::Stream(body, len),
                put,
                &[("x-ms-blob-type", "BlockBlob")],
            )
            .await?;
        check_put(resp, key, put.create_only, "PUT").await
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
        let resp = self.blob(reqwest::Method::DELETE, key).await?;
        let status = resp.status();
        // 202 on success; a missing blob is already gone.
        if status.is_success() || status == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        let body = resp.text().await.unwrap_or_default();
        Err(StorageError::Io(format!(
            "Azure DELETE {key} -> {status}: {body}"
        )))
    }

    async fn exists(&self, key: &str) -> Result<bool, StorageError> {
        Ok(self.metadata(key).await?.is_some())
    }

    fn url(&self, key: &str) -> Option<String> {
        Some(format!("{}{}", self.origin, self.path(key)))
    }

    async fn presigned_get_url(&self, key: &str, ttl: std::time::Duration) -> Option<String> {
        self.read_sas_url(key, ttl).ok()
    }

    async fn presigned_put_url(
        &self,
        key: &str,
        ttl: std::time::Duration,
        put: &PutConditions,
    ) -> Option<String> {
        let permissions = if put.create_only { "c" } else { "cw" };
        self.sas_url(key, permissions, ttl).ok()
    }

    /// A SAS cannot sign headers, so the size and type are not held.
    fn presigned_put_enforces_conditions(&self) -> bool {
        false
    }

    fn presigned_put_headers(
        &self,
        put: &PutConditions,
    ) -> std::collections::BTreeMap<String, String> {
        let mut headers = put.headers();
        headers.insert("x-ms-blob-type".to_owned(), "BlockBlob".to_owned());
        headers
    }

    async fn metadata(&self, key: &str) -> Result<Option<ObjectMeta>, StorageError> {
        let resp = self.blob(reqwest::Method::HEAD, key).await?;
        let status = resp.status();
        // Only a 404 means absent; a 403 or 503 must not read as "gone".
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(StorageError::Io(format!("Azure HEAD {key} -> {status}")));
        }
        let header = |name| {
            resp.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        let size = header(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.trim().parse::<u64>().ok())
            .ok_or_else(|| StorageError::Io(format!("Azure HEAD {key}: no Content-Length")))?;
        Ok(Some(ObjectMeta::new(
            size,
            header(reqwest::header::CONTENT_TYPE),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::super::transport::testing::{
        self, paced_body, serve, serve_counted, serve_many, Reply, Served,
    };
    use super::*;

    /// Azurite's well-known development key.
    const DEV_KEY: &str =
        "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";

    fn cfg() -> AzureConfig {
        AzureConfig {
            account: "acct".into(),
            account_key: DEV_KEY.into(),
            container: "media".into(),
            endpoint: None,
        }
    }

    fn query(url: &str, name: &str) -> Option<String> {
        url.split_once('?')?
            .1
            .split('&')
            .find_map(|p| p.strip_prefix(&format!("{name}=")))
            .map(|v| urlencoding::decode(v).unwrap().into_owned())
    }

    #[test]
    fn default_endpoint_is_the_account_blob_host() {
        let s = AzureBlobStorage::new(cfg()).unwrap();
        assert_eq!(
            s.url("a/b c.png").as_deref(),
            Some("https://acct.blob.core.windows.net/media/a/b%20c.png")
        );
    }

    #[test]
    fn azurite_endpoint_keeps_its_account_path() {
        let s = AzureBlobStorage::new(AzureConfig {
            endpoint: Some("http://127.0.0.1:10000/devstoreaccount1/".into()),
            ..cfg()
        })
        .unwrap();
        assert_eq!(
            s.url("k").as_deref(),
            Some("http://127.0.0.1:10000/devstoreaccount1/media/k")
        );
    }

    #[test]
    fn a_key_that_is_not_base64_is_refused() {
        let err = AzureBlobStorage::new(AzureConfig {
            account_key: "not base64!".into(),
            ..cfg()
        })
        .err()
        .unwrap();
        assert!(err.to_string().contains("base64"), "{err}");
    }

    #[test]
    fn config_debug_hides_the_key() {
        assert!(!format!("{:?}", cfg()).contains(DEV_KEY));
    }

    #[test]
    fn shared_key_string_has_twelve_standard_lines() {
        let s = shared_key_string(
            "PUT",
            3,
            Some("text/plain"),
            None,
            &[
                ("x-ms-date", "D".into()),
                ("x-ms-version", API_VERSION.into()),
            ],
            "/acct/media/k",
        );
        assert_eq!(
            s,
            format!(
                "PUT\n\n\n3\n\ntext/plain\n\n\n\n\n\n\nx-ms-date:D\nx-ms-version:{API_VERSION}\n/acct/media/k"
            )
        );
        // An empty body signs an empty Content-Length.
        assert!(shared_key_string("GET", 0, None, None, &[], "/r").starts_with("GET\n\n\n\n"));
    }

    // tests-003 — known answers. The strings follow Microsoft's documented
    // layouts; the signatures were computed apart from this code (Python hmac).

    #[test]
    fn shared_key_matches_a_known_answer() {
        let date = "Fri, 26 Jun 2015 23:39:12 GMT";
        let sts = "GET\n\n\n\n\n\n\n\n\n\n\n\nx-ms-date:Fri, 26 Jun 2015 23:39:12 GMT\n\
                   x-ms-version:2021-08-06\n/acct/media/a%20b.txt";
        let ms = [
            ("x-ms-date", date.to_owned()),
            ("x-ms-version", API_VERSION.to_owned()),
        ];
        let built = shared_key_string("GET", 0, None, None, &ms, "/acct/media/a%20b.txt");
        assert_eq!(built, sts);
        let s = AzureBlobStorage::new(cfg()).unwrap();
        assert_eq!(s.sign(sts), "5RsQE6wn+6G2f4efZ9R7ckXr5AoqTeph5BS8GkEBBnI=");
    }

    #[test]
    fn service_sas_matches_a_known_answer() {
        let se = "2026-01-02T03:04:05Z";
        assert_eq!(
            sas_string("r", se, "/blob/acct/media/dir/a b.csv", "https"),
            "r\n\n2026-01-02T03:04:05Z\n/blob/acct/media/dir/a b.csv\n\n\nhttps\n2021-08-06\nb\n\n\n\n\n\n\n"
        );
        let s = AzureBlobStorage::new(cfg()).unwrap();
        assert_eq!(
            s.sas_url_until("dir/a b.csv", "r", se),
            "https://acct.blob.core.windows.net/media/dir/a%20b.csv?sv=2021-08-06&sr=b&sp=r\
             &se=2026-01-02T03%3A04%3A05Z&spr=https&sig=wjB5n7%2FJ0gN22ZlUCg2NcaXwJITF3yiDnxLRMAb6lsY%3D"
        );
        let azurite = AzureBlobStorage::new(
            AzureConfig::new("devstoreaccount1", DEV_KEY, "media")
                .endpoint("http://127.0.0.1:10000/devstoreaccount1"),
        )
        .unwrap();
        assert_eq!(
            azurite.sas_url_until("k", "c", se),
            "http://127.0.0.1:10000/devstoreaccount1/media/k?sv=2021-08-06&sr=b&sp=c\
             &se=2026-01-02T03%3A04%3A05Z&sig=fLbuL%2BiUNswC%2Bjzrqbn%2BgSf0V2sD7P8iaSX6vSdyJCY%3D"
        );
    }

    /// A query parameter joins the canonicalized resource as `\nname:value`.
    #[tokio::test]
    async fn create_container_signs_the_documented_layout() {
        let (ep, req) = mock(201).await;
        assert!(mock_storage(ep).create_container().await.unwrap());
        let head = req.await.unwrap();
        assert!(
            head.starts_with("PUT /acct/media?restype=container "),
            "{head}"
        );
        let sts = format!(
            "PUT\n\n\n\n\n\n\n\n\n\n\n\nx-ms-date:{}\nx-ms-version:2021-08-06\n\
             /acct/acct/media\nrestype:container",
            header(&head, "x-ms-date")
        );
        let key = B64.decode(DEV_KEY).unwrap();
        let sig = B64.encode(hmac_sha256(&key, sts.as_bytes()));
        assert_eq!(
            header(&head, "authorization"),
            format!("SharedKey acct:{sig}")
        );
    }

    fn header(head: &str, name: &str) -> String {
        testing::header(head, name).unwrap_or_else(|| panic!("no {name} in {head}"))
    }

    #[tokio::test]
    async fn sas_get_is_read_only_and_verifies() {
        let s = AzureBlobStorage::new(cfg()).unwrap();
        let url = s
            .presigned_get_url("dir/a b.csv", std::time::Duration::from_secs(60))
            .await
            .unwrap();
        assert!(url.starts_with("https://acct.blob.core.windows.net/media/dir/a%20b.csv?"));
        assert_eq!(query(&url, "sp").as_deref(), Some("r"));
        assert_eq!(query(&url, "sr").as_deref(), Some("b"));
        assert_eq!(query(&url, "spr").as_deref(), Some("https"));
        assert_eq!(query(&url, "sv").as_deref(), Some(API_VERSION));
        // Re-sign what the URL claims, over the unencoded blob name.
        let se = query(&url, "se").unwrap();
        let want = s.sign(&sas_string(
            "r",
            &se,
            "/blob/acct/media/dir/a b.csv",
            "https",
        ));
        assert_eq!(query(&url, "sig").unwrap(), want);
    }

    /// A blob a browser would run downloads from the storage origin; others
    /// keep their stored type.
    #[tokio::test]
    async fn sas_get_downloads_active_types() {
        let s = AzureBlobStorage::new(cfg()).unwrap();
        let url = s
            .presigned_get_url("up/page.HTML", std::time::Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(query(&url, "rscd").as_deref(), Some("attachment"));
        assert_eq!(
            query(&url, "rsct").as_deref(),
            Some("application/octet-stream")
        );
        let se = query(&url, "se").unwrap();
        let want = s.sign(&sas_string_with(
            "r",
            &se,
            "/blob/acct/media/up/page.HTML",
            "https",
            Some(("attachment", "application/octet-stream")),
        ));
        assert_eq!(query(&url, "sig").unwrap(), want);
        let csv = s
            .presigned_get_url("up/a.csv", std::time::Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(query(&csv, "rscd"), None);
        assert_eq!(query(&csv, "rsct"), None);
    }

    #[tokio::test]
    async fn create_only_put_cannot_overwrite() {
        let s = AzureBlobStorage::new(cfg()).unwrap();
        let ttl = std::time::Duration::from_secs(60);
        let once = PutConditions::new().content_length(3).create_only();
        let url = s.presigned_put_url("k", ttl, &once).await.unwrap();
        assert_eq!(query(&url, "sp").as_deref(), Some("c"));
        let url = s
            .presigned_put_url("k", ttl, &PutConditions::new())
            .await
            .unwrap();
        assert_eq!(query(&url, "sp").as_deref(), Some("cw"));
    }

    #[tokio::test]
    async fn sas_expiry_is_capped_at_seven_days() {
        let s = AzureBlobStorage::new(cfg()).unwrap();
        let url = s
            .presigned_get_url("k", std::time::Duration::from_secs(30 * 86_400))
            .await
            .unwrap();
        let se = chrono::DateTime::parse_from_rfc3339(&query(&url, "se").unwrap()).unwrap();
        let days = (se.with_timezone(&chrono::Utc) - chrono::Utc::now()).num_hours();
        assert!((167..=168).contains(&days), "{days}h");
    }

    #[test]
    fn put_headers_name_the_blob_type() {
        let s = AzureBlobStorage::new(cfg()).unwrap();
        let h =
            s.presigned_put_headers(&PutConditions::new().content_type("text/csv").create_only());
        assert_eq!(
            h.get("x-ms-blob-type").map(String::as_str),
            Some("BlockBlob")
        );
        assert_eq!(h.get("if-none-match").map(String::as_str), Some("*"));
        assert_eq!(h.get("content-type").map(String::as_str), Some("text/csv"));
        assert!(!s.presigned_put_enforces_conditions());
    }

    /// Answer one request with `status`; yields the raw request head.
    async fn mock(status: u16) -> (String, tokio::task::JoinHandle<String>) {
        let (endpoint, task) = serve(status, String::new(), Vec::new()).await;
        let head = tokio::spawn(async move { task.await.unwrap().head });
        (format!("{endpoint}/acct"), head)
    }

    fn mock_storage(endpoint: String) -> AzureBlobStorage {
        AzureBlobStorage::new(AzureConfig {
            endpoint: Some(endpoint),
            ..cfg()
        })
        .unwrap()
    }

    #[tokio::test]
    async fn put_signs_what_it_sends() {
        let (ep, req) = mock(201).await;
        let s = mock_storage(ep);
        s.save_with_content_type("a b.txt", b"txt", Some("text/plain"))
            .await
            .unwrap();
        let head = req.await.unwrap();
        let get = |name: &str| header(&head, name);
        assert!(head.starts_with("PUT /acct/media/a%20b.txt "), "{head}");
        assert_eq!(get("x-ms-blob-type"), "BlockBlob");
        let ms = vec![
            ("x-ms-blob-type", get("x-ms-blob-type")),
            ("x-ms-date", get("x-ms-date")),
            ("x-ms-version", get("x-ms-version")),
        ];
        let sts = shared_key_string(
            "PUT",
            get("content-length").parse().unwrap(),
            Some(&get("content-type")),
            None,
            &ms,
            "/acct/acct/media/a%20b.txt",
        );
        assert_eq!(
            get("authorization"),
            format!("SharedKey acct:{}", s.sign(&sts))
        );
    }

    #[tokio::test]
    async fn exists_is_false_only_on_404() {
        let (ep, _req) = mock(404).await;
        assert!(!mock_storage(ep).exists("a").await.unwrap());
        let (ep, _req) = mock(200).await;
        assert!(mock_storage(ep).exists("a").await.unwrap());
        for status in [403, 503] {
            let (ep, _req) = mock(status).await;
            assert!(mock_storage(ep).exists("a").await.is_err(), "{status}");
        }
    }

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

    /// Shared Key signs the length and `If-None-Match`, not the body.
    #[tokio::test]
    async fn save_stream_signs_its_length_and_if_none_match() {
        let (ep, req) = serve(201, String::new(), Vec::new()).await;
        let s = mock_storage(format!("{ep}/acct"));
        let data = sample(150_000);
        let put = PutConditions::new()
            .content_type("text/csv")
            .content_length(150_000)
            .create_only();
        s.save_stream("a b.csv", 150_000, pieces(&data, 9_999), &put)
            .await
            .unwrap();
        let got = req.await.unwrap();
        let head = &got.head;
        let get = |name: &str| header(head, name);
        assert!(head.starts_with("PUT /acct/media/a%20b.csv "), "{head}");
        assert_eq!(get("content-length"), "150000");
        assert_eq!(get("if-none-match"), "*");
        assert_eq!(get("x-ms-blob-type"), "BlockBlob");
        assert!(
            !head.to_ascii_lowercase().contains("transfer-encoding"),
            "{head}"
        );
        let ms = vec![
            ("x-ms-blob-type", get("x-ms-blob-type")),
            ("x-ms-date", get("x-ms-date")),
            ("x-ms-version", get("x-ms-version")),
        ];
        let sts = shared_key_string(
            "PUT",
            150_000,
            Some("text/csv"),
            Some("*"),
            &ms,
            "/acct/acct/media/a%20b.csv",
        );
        assert_eq!(
            get("authorization"),
            format!("SharedKey acct:{}", s.sign(&sts))
        );
        assert_eq!(got.body, data);
    }

    /// Only 412 or 409 `BlobAlreadyExists` mean the blob is there; a
    /// lease 409 is another error.
    #[tokio::test]
    async fn create_only_save_stream_on_an_existing_blob_is_refused() {
        for (status, code, taken) in [
            (409, "BlobAlreadyExists", true),
            (412, "ConditionNotMet", true),
            (409, "LeaseIdMissing", false),
        ] {
            let extra = format!("x-ms-error-code: {code}\r\n");
            let (ep, _req) = serve(status, extra, Vec::new()).await;
            let err = mock_storage(format!("{ep}/acct"))
                .save_stream(
                    "k",
                    3,
                    pieces(b"abc", 3),
                    &PutConditions::new().create_only(),
                )
                .await
                .unwrap_err();
            assert_eq!(
                err.to_string().contains("already exists"),
                taken,
                "{code}: {err}"
            );
        }
    }

    /// Refused before any request: the endpoint is never reached.
    #[tokio::test]
    async fn save_stream_refuses_what_a_block_blob_cannot_hold() {
        let s = mock_storage("http://127.0.0.1:9/acct".into());
        let err = s
            .save_stream(
                "k",
                MAX_BLOCK * 50_000 + 1,
                pieces(b"x", 1),
                &PutConditions::new(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("block blob holds"), "{err}");
        let err = s
            .save_stream(
                "k",
                3,
                pieces(b"abc", 3),
                &PutConditions::new().content_length(2),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("does not match"), "{err}");
    }

    /// A body that fails or ends short never completes the request, so
    /// Azure stores nothing.
    #[tokio::test]
    async fn a_failed_or_short_stream_never_completes_the_upload() {
        let data = sample(300_000);
        for fail in [true, false] {
            let (ep, req) = serve(201, String::new(), Vec::new()).await;
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
            let err = mock_storage(format!("{ep}/acct"))
                .save_stream("k", 300_000, body, &PutConditions::new())
                .await
                .unwrap_err();
            let want = if fail {
                "client went away"
            } else {
                "bytes declared"
            };
            assert!(err.to_string().contains(want), "{err}");
            let got = tokio::time::timeout(std::time::Duration::from_secs(5), req);
            let got = got.await.expect("no request was sent").unwrap();
            assert!(
                !got.head.is_empty() && got.body_len < got.declared,
                "fail {fail}: the server got the whole body"
            );
        }
    }

    #[tokio::test]
    async fn load_stream_streams_the_body_and_maps_404() {
        let data = sample(200_000);
        let (ep, _req) = serve(200, String::new(), data.clone()).await;
        let mut stream = mock_storage(format!("{ep}/acct"))
            .load_stream("a.bin")
            .await
            .unwrap();
        let mut got = Vec::new();
        while let Some(c) = futures_util::StreamExt::next(&mut stream).await {
            got.extend_from_slice(&c.unwrap());
        }
        assert_eq!(got, data);
        let (ep, _req) = serve(404, String::new(), Vec::new()).await;
        let err = mock_storage(format!("{ep}/acct"))
            .load_stream("a.bin")
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::NotFound(_)), "{err}");
    }

    /// The body is pulled only as fast as the server takes it.
    #[tokio::test]
    async fn save_stream_never_buffers_the_object() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc;
        let received = Arc::new(AtomicU64::new(0));
        let ahead = Arc::new(AtomicU64::new(0));
        let (ep, req) = serve_counted(201, String::new(), Vec::new(), Some(received.clone())).await;
        let body = paced_body(256, 1 << 20, received, ahead.clone());
        mock_storage(format!("{ep}/acct"))
            .save_stream("big.bin", 256 << 20, body, &PutConditions::new())
            .await
            .unwrap();
        let got = req.await.unwrap();
        assert_eq!(got.body_len, got.declared);
        let ahead = ahead.load(Ordering::SeqCst);
        assert!(ahead <= 16 << 20, "{ahead} bytes held");
    }

    /// Against a real Azure account or Azurite. Skipped unless
    /// `RUSTANGO_AZURE_TEST_ACCOUNT` / `_KEY` / `_CONTAINER` are set
    /// (and `_ENDPOINT` for Azurite).
    #[tokio::test]
    async fn live_round_trip_skipped_without_env() {
        let var = |k: &str| std::env::var(format!("RUSTANGO_AZURE_TEST_{k}")).ok();
        let (Some(account), Some(account_key), Some(container)) =
            (var("ACCOUNT"), var("KEY"), var("CONTAINER"))
        else {
            eprintln!(
                "skipping live Azure test — set RUSTANGO_AZURE_TEST_ACCOUNT / _KEY / _CONTAINER"
            );
            return;
        };
        let s = AzureBlobStorage::new(AzureConfig {
            account,
            account_key,
            container,
            endpoint: var("ENDPOINT"),
        })
        .unwrap();
        s.create_container().await.expect("create container");
        assert!(!s.create_container().await.unwrap(), "second create");

        let key = format!("rustango-test/{} x.txt", uuid::Uuid::new_v4());
        s.save_with_content_type(&key, b"hello", Some("text/plain"))
            .await
            .expect("save");
        assert_eq!(s.load(&key).await.expect("load"), b"hello");
        let meta = s.metadata(&key).await.unwrap().unwrap();
        assert_eq!(meta.size, 5);
        assert_eq!(meta.content_type.as_deref(), Some("text/plain"));

        // The browser path: presigned PUT with the advertised headers, then GET.
        let http = reqwest::Client::new();
        let direct = format!("{key}.direct");
        let put = PutConditions::new()
            .content_type("text/csv")
            .content_length(3)
            .create_only();
        let url = s
            .presigned_put_url(&direct, std::time::Duration::from_secs(60), &put)
            .await
            .unwrap();
        let send = || {
            let mut req = http.put(&url).body("a,b");
            for (k, v) in s.presigned_put_headers(&put) {
                req = req.header(k, v);
            }
            req.send()
        };
        assert!(send().await.unwrap().status().is_success(), "presigned PUT");
        assert!(
            !send().await.unwrap().status().is_success(),
            "a create-only URL replaced the blob"
        );
        let get = s
            .presigned_get_url(&direct, std::time::Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(
            http.get(&get).send().await.unwrap().text().await.unwrap(),
            "a,b"
        );

        // Streamed: create-only holds, a cut-short body stores nothing.
        let streamed = format!("{key}.streamed");
        let data = sample(3 << 20);
        let once = PutConditions::new()
            .content_type("application/octet-stream")
            .create_only();
        s.save_stream(&streamed, data.len() as u64, pieces(&data, 100_000), &once)
            .await
            .expect("save_stream");
        let err = s
            .save_stream(&streamed, 3, pieces(b"new", 3), &once)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
        let mut got = Vec::new();
        let mut stream = s.load_stream(&streamed).await.expect("load_stream");
        while let Some(c) = futures_util::StreamExt::next(&mut stream).await {
            got.extend_from_slice(&c.unwrap());
        }
        assert_eq!(got, data);
        let cut = format!("{key}.cut");
        let short = pieces(&data[..1 << 20], 100_000);
        assert!(s
            .save_stream(&cut, data.len() as u64, short, &PutConditions::new())
            .await
            .is_err());
        assert!(
            !s.exists(&cut).await.unwrap(),
            "a cut-short upload was stored"
        );

        for k in [&key, &direct, &streamed] {
            s.delete(k).await.expect("delete");
            assert!(!s.exists(k).await.unwrap());
        }
        s.delete(&key).await.expect("delete is idempotent");
    }

    // -------- block uploads --------

    /// Blocks of 50,000 bytes: 5 cover anything up to 250,000.
    const SMALL: Split = Split {
        max_single: 100_000,
        min_part: 10_000,
        max_parts: 5,
    };

    fn request_line(s: &Served) -> String {
        s.head.lines().next().unwrap_or_default().to_owned()
    }

    /// A container that takes blocks; `list` answers Put Block List and
    /// `block` overrides the answer to a block by index.
    fn container(
        list: Reply,
        block: impl Fn(usize) -> Option<Reply> + Send + Sync + 'static,
    ) -> impl Fn(&str) -> Reply + Send + Sync + 'static {
        let n = std::sync::atomic::AtomicUsize::new(0);
        move |head: &str| {
            let line = head.lines().next().unwrap_or_default();
            if line.contains("comp=blocklist") {
                list.clone()
            } else if line.contains("comp=block") {
                let i = n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                block(i).unwrap_or((201, String::new(), Vec::new()))
            } else if line.starts_with("PUT ") {
                (201, String::new(), Vec::new())
            } else {
                (404, String::new(), Vec::new())
            }
        }
    }

    fn created() -> Reply {
        (201, String::new(), Vec::new())
    }

    fn block_storage(ep: &str, split: Split) -> AzureBlobStorage {
        let mut s = mock_storage(format!("{ep}/acct"));
        s.blocks = split;
        s
    }

    fn served(log: &std::sync::Mutex<Vec<Served>>) -> Vec<Served> {
        std::mem::take(&mut *log.lock().unwrap())
    }

    fn committed(log: &[Served]) -> bool {
        log.iter()
            .any(|s| request_line(s).contains("comp=blocklist"))
    }

    #[test]
    fn fifty_thousand_blocks_cover_the_largest_blob() {
        let most = MAX_BLOCK * 50_000;
        let block = BLOCKS.part_size(most);
        assert!(
            most.div_ceil(block) <= 50_000 && block <= MAX_BLOCK,
            "{block}"
        );
        assert_eq!(BLOCKS.part_size(MAX_PUT_BLOB + 1), 64 << 20);
        assert_eq!(BLOCKS.max_single, MAX_PUT_BLOB);
    }

    /// Above one Put Blob: blocks under one fresh id prefix, then a signed
    /// list with the type and `If-None-Match`.
    #[tokio::test]
    async fn block_upload_stages_blocks_then_commits_the_list() {
        let (ep, log) = serve_many(container(created(), |_| None), None).await;
        let s = block_storage(&ep, SMALL);
        let data = sample(250_000);
        let put = PutConditions::new()
            .content_type("text/csv")
            .content_length(250_000)
            .create_only();
        s.save_stream("a b.csv", 250_000, pieces(&data, 9_999), &put)
            .await
            .unwrap();
        let log = served(&log);
        assert!(
            request_line(&log[0]).starts_with("HEAD "),
            "{}",
            log[0].head
        );
        let (blocks, list) = (&log[1..6], &log[6]);
        assert_eq!(log.len(), 7);
        let mut ids = Vec::new();
        let mut sent = Vec::new();
        for b in blocks {
            let line = request_line(b);
            let id = line
                .split(['?', '&', ' '])
                .find_map(|p| p.strip_prefix("blockid="))
                .unwrap();
            let id = urlencoding::decode(id).unwrap().into_owned();
            assert!(
                testing::header(&b.head, "if-none-match").is_none(),
                "{}",
                b.head
            );
            assert_eq!(b.body.len(), 50_000);
            sent.extend_from_slice(&b.body);
            ids.push(id);
        }
        assert_eq!(sent, data);
        let names: Vec<_> = ids
            .iter()
            .map(|id| String::from_utf8(B64.decode(id).unwrap()).unwrap())
            .collect();
        let prefix = names[0].split_once('-').unwrap().0;
        for (i, name) in names.iter().enumerate() {
            assert_eq!(name, &format!("{prefix}-{i:05}"));
        }
        assert!(ids.iter().all(|id| id.len() == ids[0].len()));

        let head = &list.head;
        let get = |name: &str| header(head, name);
        assert!(
            head.starts_with("PUT /acct/media/a%20b.csv?comp=blocklist "),
            "{head}"
        );
        assert_eq!(get("if-none-match"), "*");
        assert_eq!(get("x-ms-blob-content-type"), "text/csv");
        let latest: String = ids
            .iter()
            .map(|id| format!("<Latest>{id}</Latest>"))
            .collect();
        assert_eq!(
            String::from_utf8(list.body.clone()).unwrap(),
            format!(r#"<?xml version="1.0" encoding="utf-8"?><BlockList>{latest}</BlockList>"#)
        );
        let ms = vec![
            ("x-ms-blob-content-type", get("x-ms-blob-content-type")),
            ("x-ms-date", get("x-ms-date")),
            ("x-ms-version", get("x-ms-version")),
        ];
        let sts = shared_key_string(
            "PUT",
            list.body.len() as u64,
            None,
            Some("*"),
            &ms,
            "/acct/acct/media/a%20b.csv\ncomp:blocklist",
        );
        assert_eq!(
            get("authorization"),
            format!("SharedKey acct:{}", s.sign(&sts))
        );
    }

    /// `save_stream` keeps one Put Blob up to the limit, blocks above.
    #[tokio::test]
    async fn save_stream_stages_blocks_only_above_one_put_blob() {
        for (len, blocks) in [(100_000usize, false), (100_001, true)] {
            let (ep, log) = serve_many(container(created(), |_| None), None).await;
            block_storage(&ep, SMALL)
                .save_stream(
                    "k",
                    len as u64,
                    pieces(&sample(len), 7_000),
                    &PutConditions::new(),
                )
                .await
                .unwrap();
            assert_eq!(committed(&served(&log)), blocks, "{len}");
        }
    }

    /// A failed block, or a body of another size, never commits a list.
    #[tokio::test]
    async fn a_failed_block_or_odd_body_never_commits() {
        let fail = |i| (i == 1).then(|| (500, String::new(), Vec::new()));
        let (ep, log) = serve_many(container(created(), fail), None).await;
        let err = block_storage(&ep, SMALL)
            .save_stream(
                "k",
                250_000,
                pieces(&sample(250_000), 9_999),
                &PutConditions::new(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("500"), "{err}");
        assert!(!committed(&served(&log)), "committed after a failed block");
        for (real, want) in [(180_000usize, "ended after"), (260_000, "longer than")] {
            let (ep, log) = serve_many(container(created(), |_| None), None).await;
            let err = block_storage(&ep, SMALL)
                .save_stream(
                    "k",
                    250_000,
                    pieces(&sample(real), 9_999),
                    &PutConditions::new(),
                )
                .await
                .unwrap_err();
            assert!(err.to_string().contains(want), "{real}: {err}");
            assert!(!committed(&served(&log)), "{real}: committed");
        }
    }

    #[tokio::test]
    async fn create_only_block_list_on_a_taken_blob_is_refused() {
        for (status, code, taken) in [
            (409, "BlobAlreadyExists", true),
            (412, "ConditionNotMet", true),
            (409, "LeaseIdMissing", false),
        ] {
            let list = (status, format!("x-ms-error-code: {code}\r\n"), Vec::new());
            let (ep, _log) = serve_many(container(list, |_| None), None).await;
            let err = block_storage(&ep, SMALL)
                .save_stream(
                    "k",
                    150_000,
                    pieces(&sample(150_000), 9_999),
                    &PutConditions::new().create_only(),
                )
                .await
                .unwrap_err();
            assert_eq!(
                err.to_string().contains("already exists"),
                taken,
                "{code}: {err}"
            );
        }
    }

    /// Blocks stream as they arrive: memory stays well under one block.
    #[tokio::test]
    async fn block_upload_never_buffers_a_block() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc;
        let received = Arc::new(AtomicU64::new(0));
        let ahead = Arc::new(AtomicU64::new(0));
        let (ep, log) = serve_many(container(created(), |_| None), Some(received.clone())).await;
        let split = Split {
            max_single: 8 << 20,
            min_part: 32 << 20,
            max_parts: 50_000,
        };
        let body = paced_body(128, 1 << 20, received, ahead.clone());
        block_storage(&ep, split)
            .save_stream("k", 128 << 20, body, &PutConditions::new())
            .await
            .unwrap();
        let blocks: Vec<_> = served(&log)
            .into_iter()
            .filter(|s| request_line(s).contains("comp=block "))
            .map(|s| s.body_len)
            .collect();
        assert_eq!(blocks, [32 << 20; 4]);
        let ahead = ahead.load(Ordering::SeqCst);
        assert!(ahead <= 16 << 20, "{ahead} bytes held");
    }

    /// Against Azurite or Azure, blocks of 1 MiB. Skipped unless
    /// `RUSTANGO_AZURE_TEST_*` is set.
    #[tokio::test]
    async fn live_block_upload_skipped_without_env() {
        let var = |k: &str| std::env::var(format!("RUSTANGO_AZURE_TEST_{k}")).ok();
        let (Some(account), Some(account_key), Some(container)) =
            (var("ACCOUNT"), var("KEY"), var("CONTAINER"))
        else {
            eprintln!("skipping live Azure block test — set RUSTANGO_AZURE_TEST_ACCOUNT / _KEY / _CONTAINER");
            return;
        };
        let mut s = AzureBlobStorage::new(AzureConfig {
            account,
            account_key,
            container,
            endpoint: var("ENDPOINT"),
        })
        .unwrap();
        s.blocks = Split {
            max_single: 2 << 20,
            min_part: 1 << 20,
            max_parts: 50_000,
        };
        s.create_container().await.expect("create container");
        let key = format!("rustango-test/{} blocks.bin", uuid::Uuid::new_v4());
        let data = sample((3 << 20) + 1234);
        let once = PutConditions::new().content_type("text/csv").create_only();
        s.save_stream(&key, data.len() as u64, pieces(&data, 100_000), &once)
            .await
            .expect("block save_stream");
        assert_eq!(s.load(&key).await.unwrap(), data);
        let meta = s.metadata(&key).await.unwrap().unwrap();
        assert_eq!(meta.size, data.len() as u64);
        assert_eq!(meta.content_type.as_deref(), Some("text/csv"));
        // Without `create_only` the blob is replaced.
        let new = sample(3 << 20);
        s.save_stream(
            &key,
            new.len() as u64,
            pieces(&new, 100_000),
            &PutConditions::new(),
        )
        .await
        .expect("overwrite");
        // Past the pre-check, so the list's `If-None-Match` decides.
        let newer = sample((3 << 20) + 7);
        let err = s
            .commit_blocks(&key, newer.len() as u64, pieces(&newer, 100_000), &once)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
        assert_eq!(
            s.load(&key).await.unwrap(),
            new,
            "the list replaced the blob"
        );
        let cut = format!("{key}.cut");
        let short = pieces(&data[..(2 << 20) + 10], 100_000);
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
