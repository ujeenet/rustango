//! HTTP plumbing the S3 and Azure backends share: clients with the
//! timeouts object storage needs, and the encodings both signers use.

use std::fmt::Write as _;

use super::StorageError;

/// The longest a presigned URL lives: 7 days, S3's ceiling, used for Azure too.
pub(super) const MAX_PRESIGN_SECS: u64 = 604_800;

/// Default client timeouts; `with_http` on a backend replaces them.
pub(super) const DEFAULT_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// Waiting for a reply or the next body chunk; a total cap would fail big objects.
pub(super) const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
/// The slowest upload rate the default PUT timeout allows, in bytes per second.
const MIN_UPLOAD_RATE: u64 = 256 * 1024;

/// The HTTP clients. Only the defaults get a size-based upload timeout.
pub(super) enum Http {
    /// reqwest's read timeout runs through an upload, so PUT uses a client without one.
    Default {
        http: reqwest::Client,
        upload: reqwest::Client,
        read_timeout: std::time::Duration,
    },
    /// From a backend's `with_http`: its own timeouts apply to every request.
    Custom(reqwest::Client),
}

impl Http {
    pub(super) fn defaults() -> Self {
        Self::with_read_timeout(DEFAULT_READ_TIMEOUT)
    }

    pub(super) fn with_read_timeout(read_timeout: std::time::Duration) -> Self {
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

    pub(super) fn request(
        &self,
        method: reqwest::Method,
        url: &str,
        len: usize,
    ) -> reqwest::RequestBuilder {
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
pub(super) fn upload_timeout(read_timeout: std::time::Duration, len: usize) -> std::time::Duration {
    let len = u64::try_from(len).unwrap_or(u64::MAX);
    read_timeout + std::time::Duration::from_millis(len.saturating_mul(1000) / MIN_UPLOAD_RATE)
}

/// A header value as the signers sign it: trimmed, inner runs of spaces
/// collapsed to one. `None` when empty; control characters are refused.
pub(super) fn canonical_header_value(v: Option<&str>) -> Result<Option<String>, StorageError> {
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

/// Percent-encode a key for a URL path. `/` stays as it is, because
/// keys use it as a separator; everything outside the unreserved set
/// is escaped.
pub(super) fn encode_key(key: &str) -> String {
    percent_encode(key, true)
}

/// Like [`encode_key`], but it escapes `/` as well, for query values.
pub(super) fn encode_query(s: &str) -> String {
    percent_encode(s, false)
}

fn percent_encode(s: &str, keep_slash: bool) -> String {
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

#[cfg(feature = "storage-azure")]
/// `(scheme://authority, path)` of an endpoint URL, the path without a
/// trailing `/`. A bare host is taken as https.
pub(super) fn split_endpoint(endpoint: &str) -> (String, String) {
    let (scheme, rest) = if let Some(r) = endpoint.strip_prefix("http://") {
        ("http", r)
    } else {
        (
            "https",
            endpoint.strip_prefix("https://").unwrap_or(endpoint),
        )
    };
    let rest = rest.trim_end_matches('/');
    match rest.find('/') {
        Some(i) => (format!("{scheme}://{}", &rest[..i]), rest[i..].to_owned()),
        None => (format!("{scheme}://{rest}"), String::new()),
    }
}

#[cfg(all(test, feature = "storage-azure"))]
mod tests {
    use super::*;

    #[test]
    fn split_endpoint_separates_origin_and_path() {
        assert_eq!(
            split_endpoint("http://127.0.0.1:10000/devstoreaccount1/"),
            ("http://127.0.0.1:10000".into(), "/devstoreaccount1".into())
        );
        assert_eq!(
            split_endpoint("https://acct.blob.core.windows.net"),
            ("https://acct.blob.core.windows.net".into(), String::new())
        );
        assert_eq!(
            split_endpoint("cdn.example.com/files"),
            ("https://cdn.example.com".into(), "/files".into())
        );
    }
}
