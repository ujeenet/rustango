//! Webhook signature verification — HMAC-based, constant-time.
//!
//! Most webhook providers (Stripe, GitHub, Slack, etc.) sign the request
//! body with HMAC and put the signature in a header. This module verifies
//! those signatures so you don't run handler code on forged payloads.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::webhook::{verify_signature, SignatureFormat};
//!
//! async fn handle_webhook(headers: HeaderMap, body: Bytes) -> impl IntoResponse {
//!     let signature = headers.get("X-Hub-Signature-256")
//!         .and_then(|v| v.to_str().ok())
//!         .unwrap_or("");
//!
//!     if !verify_signature(SignatureFormat::HexSha256, secret, &body, signature) {
//!         return StatusCode::UNAUTHORIZED;
//!     }
//!     // ... process the verified payload
//!     StatusCode::OK
//! }
//! ```

use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

/// Signature encoding format — what the webhook provider sends.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub enum SignatureFormat {
    /// `sha256=<hex>` (GitHub `X-Hub-Signature-256`)
    HexSha256WithPrefix,
    /// `<hex>` (raw lowercase hex digest, no prefix)
    HexSha256,
    /// `<base64>` (standard base64 with padding)
    Base64Sha256,
}

/// Verify `signature` against `body` using HMAC-SHA256 with `secret`.
///
/// Returns `true` only when the signature matches. Comparison is
/// constant-time to prevent timing attacks.
///
/// # Example
///
/// ```
/// use rustango::webhook::{verify_signature, SignatureFormat, sign};
/// let secret = b"my-shared-secret";
/// let body = b"{\"event\":\"foo\"}";
/// let sig = sign(SignatureFormat::HexSha256, secret, body);
/// assert!(verify_signature(SignatureFormat::HexSha256, secret, body, &sig));
/// ```
#[must_use]
pub fn verify_signature(
    format: SignatureFormat,
    secret: &[u8],
    body: &[u8],
    signature: &str,
) -> bool {
    let expected_bytes = compute_hmac(secret, body);
    let provided_bytes = match decode_signature(format, signature) {
        Some(b) => b,
        None => return false,
    };
    if expected_bytes.len() != provided_bytes.len() {
        return false;
    }
    expected_bytes.ct_eq(&provided_bytes).unwrap_u8() == 1
}

/// Sign `body` with `secret`, producing a signature in the given format.
/// Useful for generating webhooks (or in tests).
#[must_use]
pub fn sign(format: SignatureFormat, secret: &[u8], body: &[u8]) -> String {
    let bytes = compute_hmac(secret, body);
    match format {
        SignatureFormat::HexSha256WithPrefix => format!("sha256={}", to_hex(&bytes)),
        SignatureFormat::HexSha256 => to_hex(&bytes),
        SignatureFormat::Base64Sha256 => {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        }
    }
}

fn compute_hmac(secret: &[u8], body: &[u8]) -> Vec<u8> {
    let mut mac = <Hmac<Sha256>>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(body);
    mac.finalize().into_bytes().to_vec()
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

    const SECRET: &[u8] = b"my-test-secret";
    const BODY: &[u8] = b"{\"event\":\"ping\"}";

    #[test]
    fn sign_and_verify_hex_with_prefix() {
        let sig = sign(SignatureFormat::HexSha256WithPrefix, SECRET, BODY);
        assert!(sig.starts_with("sha256="));
        assert!(verify_signature(SignatureFormat::HexSha256WithPrefix, SECRET, BODY, &sig));
    }

    #[test]
    fn sign_and_verify_hex_no_prefix() {
        let sig = sign(SignatureFormat::HexSha256, SECRET, BODY);
        assert_eq!(sig.len(), 64); // 32 bytes hex
        assert!(verify_signature(SignatureFormat::HexSha256, SECRET, BODY, &sig));
    }

    #[test]
    fn sign_and_verify_base64() {
        let sig = sign(SignatureFormat::Base64Sha256, SECRET, BODY);
        assert!(verify_signature(SignatureFormat::Base64Sha256, SECRET, BODY, &sig));
    }

    #[test]
    fn wrong_secret_fails() {
        let sig = sign(SignatureFormat::HexSha256, SECRET, BODY);
        assert!(!verify_signature(
            SignatureFormat::HexSha256,
            b"different-secret",
            BODY,
            &sig
        ));
    }

    #[test]
    fn wrong_body_fails() {
        let sig = sign(SignatureFormat::HexSha256, SECRET, BODY);
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
        assert!(!verify_signature(SignatureFormat::HexSha256, SECRET, BODY, ""));
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

    #[test]
    fn cross_format_does_not_verify() {
        // A hex signature should NOT verify against the base64 format
        let hex_sig = sign(SignatureFormat::HexSha256, SECRET, BODY);
        assert!(!verify_signature(SignatureFormat::Base64Sha256, SECRET, BODY, &hex_sig));
    }
}
