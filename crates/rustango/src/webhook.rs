//! Webhook signature checks, using HMAC and constant-time compare.
//!
//! Providers sign the request and send the signature in a header. Verify
//! it before you act on the payload, or you will run your handler on
//! forged data. GitHub signs the body; Stripe and Slack sign a timestamp
//! plus the body, so build that string first (see `docs/security.md`).
//!
//! ## Quick start
//!
//! ```
//! use axum::{body::Bytes, http::HeaderMap, http::StatusCode};
//! use rustango::webhook::{verify_signature, SignatureFormat};
//!
//! // GitHub sends `X-Hub-Signature-256: sha256=<hex>` over the raw body.
//! fn check_github(secret: &[u8], headers: &HeaderMap, body: &Bytes) -> StatusCode {
//!     let signature = headers
//!         .get("X-Hub-Signature-256")
//!         .and_then(|v| v.to_str().ok())
//!         .unwrap_or("");
//!     if !verify_signature(SignatureFormat::HexSha256WithPrefix, secret, body, signature) {
//!         return StatusCode::UNAUTHORIZED;
//!     }
//!     // ... process the verified payload
//!     StatusCode::OK
//! }
//! # let body = Bytes::from_static(b"{}");
//! # let mut h = HeaderMap::new();
//! # let sig = rustango::webhook::sign(SignatureFormat::HexSha256WithPrefix, b"s", &body).unwrap();
//! # h.insert("X-Hub-Signature-256", sig.parse().unwrap());
//! # assert_eq!(check_github(b"s", &h, &body), StatusCode::OK);
//! # assert_eq!(check_github(b"other", &h, &body), StatusCode::UNAUTHORIZED);
//! ```

use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

/// How the provider encodes the signature.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub enum SignatureFormat {
    /// `sha256=<hex>` (GitHub `X-Hub-Signature-256`)
    HexSha256WithPrefix,
    /// `<hex>` (raw lowercase hex digest, no prefix)
    HexSha256,
    /// `<base64>` (standard base64 with padding)
    Base64Sha256,
}

/// Check `signature` against `body` with HMAC-SHA256 and `secret`.
///
/// Returns `true` only on a match. The compare is constant-time, so
/// it does not leak the expected signature through timing. An empty
/// `secret` never verifies: anyone could sign with it (#1850).
///
/// # Example
///
/// ```
/// use rustango::webhook::{verify_signature, SignatureFormat, sign};
/// let secret = b"my-shared-secret";
/// let body = b"{\"event\":\"foo\"}";
/// let sig = sign(SignatureFormat::HexSha256, secret, body).unwrap();
/// assert!(verify_signature(SignatureFormat::HexSha256, secret, body, &sig));
/// ```
#[must_use]
pub fn verify_signature(
    format: SignatureFormat,
    secret: &[u8],
    body: &[u8],
    signature: &str,
) -> bool {
    let Some(expected_bytes) = compute_hmac(secret, body) else {
        return false;
    };
    let provided_bytes = match decode_signature(format, signature) {
        Some(b) => b,
        None => return false,
    };
    if expected_bytes.len() != provided_bytes.len() {
        return false;
    }
    expected_bytes.ct_eq(&provided_bytes).unwrap_u8() == 1
}

/// [`sign`] refuses an empty key: anyone could sign with it (#1850).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the webhook signing key is empty")]
pub struct EmptySigningKey;

/// Sign `body` with `secret` in the given format. Use it when you
/// send webhooks, or in tests.
///
/// # Errors
/// [`EmptySigningKey`] when `secret` is empty.
pub fn sign(
    format: SignatureFormat,
    secret: &[u8],
    body: &[u8],
) -> Result<String, EmptySigningKey> {
    let bytes = compute_hmac(secret, body).ok_or(EmptySigningKey)?;
    Ok(match format {
        SignatureFormat::HexSha256WithPrefix => format!("sha256={}", to_hex(&bytes)),
        SignatureFormat::HexSha256 => to_hex(&bytes),
        SignatureFormat::Base64Sha256 => {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        }
    })
}

/// The signed headers of a delivery from `rustango::webhook_delivery`,
/// as received.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct DeliveryHeaders<'a> {
    /// `X-Webhook-Id`.
    pub id: &'a str,
    /// `X-Webhook-Event`.
    pub event: &'a str,
    /// `X-Webhook-Timestamp`, unix seconds at dispatch.
    pub timestamp: &'a str,
    /// `X-Webhook-Signature-V2`.
    pub signature: &'a str,
}

impl<'a> DeliveryHeaders<'a> {
    /// The four header values, as received.
    #[must_use]
    pub fn new(id: &'a str, event: &'a str, timestamp: &'a str, signature: &'a str) -> Self {
        Self {
            id,
            event,
            timestamp,
            signature,
        }
    }
}

/// The v2 signed string. Header values hold no newline, so no two
/// deliveries share one.
fn delivery_message(id: &str, event: &str, timestamp: &str, body: &[u8]) -> Vec<u8> {
    let mut msg = format!("v2\n{id}\n{event}\n{timestamp}\n").into_bytes();
    msg.extend_from_slice(body);
    msg
}

/// The `X-Webhook-Signature-V2` of a delivery: it covers the id, event,
/// timestamp and body, so none can be swapped or replayed under a new id.
///
/// # Errors
/// [`EmptySigningKey`] when `secret` is empty.
pub fn sign_delivery(
    format: SignatureFormat,
    secret: &[u8],
    id: &str,
    event: &str,
    timestamp: i64,
    body: &[u8],
) -> Result<String, EmptySigningKey> {
    let msg = delivery_message(id, event, &timestamp.to_string(), body);
    sign(format, secret, &msg)
}

/// Check a delivery's v2 signature, and that its timestamp is within
/// `max_age` of now. Drop repeated `X-Webhook-Id`s seen within `max_age`
/// to refuse replays. Retries reuse the dispatch timestamp, so `max_age`
/// must cover queue backlog plus the retry window (about two minutes).
///
/// ```
/// use rustango::webhook::{sign_delivery, verify_delivery, DeliveryHeaders, SignatureFormat};
/// let now = std::time::SystemTime::now()
///     .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
/// let f = SignatureFormat::HexSha256WithPrefix;
/// let sig = sign_delivery(f, b"s", "id-1", "order.created", now, b"{}").unwrap();
/// let ts = now.to_string();
/// let h = DeliveryHeaders::new("id-1", "order.created", &ts, &sig);
/// assert!(verify_delivery(f, b"s", &h, b"{}", std::time::Duration::from_secs(300)));
/// let replayed = DeliveryHeaders::new("id-2", "order.created", &ts, &sig);
/// assert!(!verify_delivery(f, b"s", &replayed, b"{}", std::time::Duration::from_secs(300)));
/// ```
#[must_use]
pub fn verify_delivery(
    format: SignatureFormat,
    secret: &[u8],
    headers: &DeliveryHeaders<'_>,
    body: &[u8],
    max_age: std::time::Duration,
) -> bool {
    let Ok(at) = headers.timestamp.parse::<i64>() else {
        return false;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    let max_age = i64::try_from(max_age.as_secs()).unwrap_or(i64::MAX);
    if now.abs_diff(at) > max_age.unsigned_abs() {
        return false;
    }
    let msg = delivery_message(headers.id, headers.event, headers.timestamp, body);
    verify_signature(format, secret, &msg, headers.signature)
}

/// `None` for an empty key.
fn compute_hmac(secret: &[u8], body: &[u8]) -> Option<Vec<u8>> {
    if secret.is_empty() {
        return None;
    }
    let mut mac = <Hmac<Sha256>>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(body);
    Some(mac.finalize().into_bytes().to_vec())
}

fn decode_signature(format: SignatureFormat, signature: &str) -> Option<Vec<u8>> {
    match format {
        SignatureFormat::HexSha256WithPrefix => {
            let s = signature.strip_prefix("sha256=")?;
            from_hex(s)
        }
        SignatureFormat::HexSha256 => from_hex(signature),
        SignatureFormat::Base64Sha256 => {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(signature.as_bytes())
                .ok()
        }
    }
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn from_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for chunk in s.as_bytes().chunks(2) {
        let h = std::str::from_utf8(chunk).ok()?;
        out.push(u8::from_str_radix(h, 16).ok()?);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> i64 {
        chrono::Utc::now().timestamp()
    }

    #[test]
    fn a_delivery_verifies_only_as_signed_and_fresh() {
        let f = SignatureFormat::HexSha256;
        let age = std::time::Duration::from_secs(300);
        let at = now();
        let sig = sign_delivery(f, SECRET, "id-1", "ping", at, BODY).unwrap();
        let ts = at.to_string();
        let h = DeliveryHeaders {
            id: "id-1",
            event: "ping",
            timestamp: &ts,
            signature: &sig,
        };
        assert!(verify_delivery(f, SECRET, &h, BODY, age));
        assert!(!verify_delivery(
            f,
            SECRET,
            &DeliveryHeaders { id: "id-2", ..h },
            BODY,
            age
        ));
        assert!(!verify_delivery(
            f,
            SECRET,
            &DeliveryHeaders { event: "pong", ..h },
            BODY,
            age
        ));
        assert!(!verify_delivery(f, SECRET, &h, b"{}", age));
        assert!(!verify_delivery(f, b"other", &h, BODY, age));
        // The v1 body-only signature is not a v2 one.
        let v1 = sign(f, SECRET, BODY).unwrap();
        assert!(!verify_delivery(
            f,
            SECRET,
            &DeliveryHeaders {
                signature: &v1,
                ..h
            },
            BODY,
            age
        ));
        // A stale or future timestamp fails even with its own signature.
        for off in [-301, 301] {
            let at = now() + off;
            let sig = sign_delivery(f, SECRET, "id-1", "ping", at, BODY).unwrap();
            let ts = at.to_string();
            let old = DeliveryHeaders {
                timestamp: &ts,
                signature: &sig,
                ..h
            };
            assert!(!verify_delivery(f, SECRET, &old, BODY, age), "{off}");
        }
        assert!(!verify_delivery(
            f,
            SECRET,
            &DeliveryHeaders {
                timestamp: "x",
                ..h
            },
            BODY,
            age
        ));
    }

    const SECRET: &[u8] = b"my-test-secret";
    const BODY: &[u8] = b"{\"event\":\"ping\"}";

    #[test]
    fn sign_and_verify_hex_with_prefix() {
        let sig = sign(SignatureFormat::HexSha256WithPrefix, SECRET, BODY).unwrap();
        assert!(sig.starts_with("sha256="));
        assert!(verify_signature(
            SignatureFormat::HexSha256WithPrefix,
            SECRET,
            BODY,
            &sig
        ));
    }

    #[test]
    fn sign_and_verify_hex_no_prefix() {
        let sig = sign(SignatureFormat::HexSha256, SECRET, BODY).unwrap();
        assert_eq!(sig.len(), 64); // 32 bytes hex
        assert!(verify_signature(
            SignatureFormat::HexSha256,
            SECRET,
            BODY,
            &sig
        ));
    }

    #[test]
    fn sign_and_verify_base64() {
        let sig = sign(SignatureFormat::Base64Sha256, SECRET, BODY).unwrap();
        assert!(verify_signature(
            SignatureFormat::Base64Sha256,
            SECRET,
            BODY,
            &sig
        ));
    }

    #[test]
    fn wrong_secret_fails() {
        let sig = sign(SignatureFormat::HexSha256, SECRET, BODY).unwrap();
        assert!(!verify_signature(
            SignatureFormat::HexSha256,
            b"different-secret",
            BODY,
            &sig
        ));
    }

    #[test]
    fn wrong_body_fails() {
        let sig = sign(SignatureFormat::HexSha256, SECRET, BODY).unwrap();
        assert!(!verify_signature(
            SignatureFormat::HexSha256,
            SECRET,
            b"tampered body",
            &sig
        ));
    }

    #[test]
    fn malformed_hex_signature_fails() {
        assert!(!verify_signature(
            SignatureFormat::HexSha256,
            SECRET,
            BODY,
            "not-hex!"
        ));
    }

    #[test]
    fn malformed_prefix_signature_fails() {
        // Missing `sha256=` prefix
        assert!(!verify_signature(
            SignatureFormat::HexSha256WithPrefix,
            SECRET,
            BODY,
            "abcdef0123456789",
        ));
    }

    #[test]
    fn empty_signature_fails() {
        assert!(!verify_signature(
            SignatureFormat::HexSha256,
            SECRET,
            BODY,
            ""
        ));
    }

    #[test]
    fn from_hex_roundtrip() {
        let bytes: Vec<u8> = (0..=255).collect();
        let hex = to_hex(&bytes);
        let decoded = from_hex(&hex).unwrap();
        assert_eq!(decoded, bytes);
    }

    #[test]
    fn from_hex_rejects_odd_length() {
        assert_eq!(from_hex("abc"), None);
    }

    #[test]
    fn from_hex_rejects_invalid_chars() {
        assert_eq!(from_hex("zzzz"), None);
    }

    /// HMAC over an empty key is computable by anyone (#1850).
    #[test]
    fn empty_secret_never_verifies() {
        let forged = {
            use hmac::{Hmac, Mac};
            let mut mac = <Hmac<Sha256>>::new_from_slice(b"").unwrap();
            mac.update(BODY);
            to_hex(&mac.finalize().into_bytes())
        };
        assert!(!verify_signature(
            SignatureFormat::HexSha256,
            b"",
            BODY,
            &forged
        ));
        assert_eq!(
            sign(SignatureFormat::HexSha256, b"", BODY),
            Err(EmptySigningKey)
        );
    }

    #[test]
    fn cross_format_does_not_verify() {
        // A hex signature must not pass as base64.
        let hex_sig = sign(SignatureFormat::HexSha256, SECRET, BODY).unwrap();
        assert!(!verify_signature(
            SignatureFormat::Base64Sha256,
            SECRET,
            BODY,
            &hex_sig
        ));
    }
}
