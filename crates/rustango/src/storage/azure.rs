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
//!
//! ## What it does not do
//!
//! - Sign headers into a SAS: Azure has no way to. The type and size in
//!   [`PutConditions`] are not enforced by the URL, so confirm a direct
//!   upload with [`Storage::metadata`]. `media` gives no direct-upload
//!   ticket on an Azure disk; use `save_bytes`.
//! - Microsoft Entra ID (user delegation SAS), staged block uploads,
//!   listing.

use std::fmt::Write as _;

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;

use super::transport::{
    canonical_header_value, encode_key, encode_query, split_endpoint, Http, MAX_PRESIGN_SECS,
};
use super::{validate_key, ObjectMeta, PutConditions, Storage, StorageError};
use crate::crypto::hmac_sha256;

/// REST version sent on every request and signed into every SAS.
/// 2019-12-12 raised Put Blob to 5000 MiB; 2020-12-06 set the SAS
/// string-to-sign layout used below.
const API_VERSION: &str = "2021-08-06";

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
                Some(b""),
                None,
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

    /// One Shared Key request. `body` is `None` for a request without one.
    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&[u8]>,
        content_type: Option<&str>,
        extra: &[(&'static str, &str)],
    ) -> Result<reqwest::Response, StorageError> {
        let content_type = canonical_header_value(content_type)?;
        let len = body.map_or(0, <[u8]>::len);
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
        if let Some(body) = body {
            req = req.body(body.to_vec());
        }
        req.send()
            .await
            .map_err(|e| StorageError::Io(format!("http: {e}")))
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

    /// [`Self::sas_url`] expiring at `expiry` (`%Y-%m-%dT%H:%M:%SZ`).
    fn sas_url_until(&self, key: &str, permissions: &str, expiry: &str) -> String {
        // Refuse plain http when the endpoint is https.
        let protocol = if self.origin.starts_with("https://") {
            "https"
        } else {
            ""
        };
        let resource = format!("/blob/{}/{}/{key}", self.account, self.container);
        let sig = self.sign(&sas_string(permissions, expiry, &resource, protocol));
        let mut query = vec![
            ("sv", API_VERSION),
            ("sr", "b"),
            ("sp", permissions),
            ("se", expiry),
        ];
        if !protocol.is_empty() {
            query.push(("spr", protocol));
        }
        query.push(("sig", sig.as_str()));
        let query: Vec<String> = query
            .iter()
            .map(|(k, v)| format!("{k}={}", encode_query(v)))
            .collect();
        format!("{}{}?{}", self.origin, self.path(key), query.join("&"))
    }

    async fn blob(
        &self,
        method: reqwest::Method,
        key: &str,
    ) -> Result<reqwest::Response, StorageError> {
        validate_key(key)?;
        self.send(method, &self.path(key), &[], None, None, &[])
            .await
    }
}

/// The Shared Key string-to-sign for the Blob service. Content-Length
/// is empty for an empty body (versions 2015-02-21 and later).
fn shared_key_string(
    method: &str,
    len: usize,
    content_type: Option<&str>,
    ms_headers: &[(&str, String)],
    resource: &str,
) -> String {
    let len = if len == 0 {
        String::new()
    } else {
        len.to_string()
    };
    let ct = content_type.unwrap_or("");
    let headers = ms_headers.iter().fold(String::new(), |mut out, (k, v)| {
        let _ = writeln!(out, "{k}:{v}");
        out
    });
    // VERB, Content-Encoding, -Language, -Length, -MD5, -Type, Date,
    // If-Modified-Since, If-Match, If-None-Match, If-Unmodified-Since, Range.
    format!("{method}\n\n\n{len}\n\n{ct}\n\n\n\n\n\n\n{headers}{resource}")
}

/// The service SAS string-to-sign (versions 2020-12-06 and later) with
/// no start, identifier, IP, snapshot, scope or response overrides.
fn sas_string(permissions: &str, expiry: &str, resource: &str, protocol: &str) -> String {
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
        "",
        "",
        "",
        "",
    ]
    .join("\n")
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
        let resp = self
            .send(
                reqwest::Method::PUT,
                &self.path(key),
                &[],
                Some(data),
                content_type,
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

    async fn load(&self, key: &str) -> Result<Vec<u8>, StorageError> {
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
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| StorageError::Io(format!("read body: {e}")))?;
        // Reuses the buffer when the body is uniquely owned.
        Ok(Vec::from(bytes))
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
        self.sas_url(key, "r", ttl).ok()
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
        assert!(shared_key_string("GET", 0, None, &[], "/r").starts_with("GET\n\n\n\n"));
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
        let built = shared_key_string("GET", 0, None, &ms, "/acct/media/a%20b.txt");
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
        head.lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case(name).then(|| v.trim().to_owned())
            })
            .unwrap_or_else(|| panic!("no {name} in {head}"))
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
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/acct", listener.local_addr().unwrap());
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
            let end = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
            let head = String::from_utf8_lossy(&buf[..end]).into_owned();
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
            let reply =
                format!("HTTP/1.1 {status} X\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
            sock.write_all(reply.as_bytes()).await.unwrap();
            head
        });
        (endpoint, task)
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

        for k in [&key, &direct] {
            s.delete(k).await.expect("delete");
            assert!(!s.exists(k).await.unwrap());
        }
        s.delete(&key).await.expect("delete is idempotent");
    }
}
