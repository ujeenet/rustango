//! Signed URL helpers — tamper-evident URLs with optional expiry.
//!
//! Common uses:
//! - Magic-link login (emailed to the user)
//! - Password reset confirmation links
//! - Time-limited file download URLs
//! - "Click here to verify your email" links
//!
//! ## These links are not single-use
//!
//! [`verify`] checks the signature and the expiry, nothing else. A
//! link that leaks through a Referer header, browser history, a proxy
//! log or a forwarded message keeps working until it expires.
//!
//! For a link that logs someone in or changes data, enforce single
//! use yourself: delete or rotate the record after the first click,
//! or use the `verify_single_use` variants in [`crate::auth_flows`].
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::signed_url::{sign, verify};
//! use std::time::Duration;
//!
//! let secret = b"my-app-secret";
//!
//! // Issue a one-hour magic-link
//! let url = sign("https://app.example.com/auth/login?email=alice@x.com",
//!                secret,
//!                Some(Duration::from_secs(3600)));
//!
//! // On the callback handler:
//! match verify(&incoming_url, secret) {
//!     Ok(()) => { /* identity confirmed */ }
//!     Err(e) => { /* expired or tampered */ }
//! }
//! ```
//!
//! ## How it works
//!
//! It appends `?signature=<base64>&expires=<unix_secs>`. The
//! signature is HMAC-SHA256 over the scheme, host, path and the
//! sorted query without the signature itself. Sorting first means the
//! same URL always signs the same, whatever order the params arrive
//! in.
//!
//! [`verify`]: crate::signed_url::verify

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SignedUrlError {
    #[error("missing `signature` query parameter")]
    MissingSignature,
    #[error("signature is malformed")]
    MalformedSignature,
    #[error("signature does not match")]
    InvalidSignature,
    #[error("URL has expired")]
    Expired,
}

const SIGNATURE_PARAM: &str = "signature";
const EXPIRES_PARAM: &str = "expires";

/// Sign `url` with `secret`, with an optional lifetime from now.
///
/// It returns the URL with `signature` and `expires` added to the
/// query string.
#[must_use]
pub fn sign(url: &str, secret: &[u8], ttl: Option<Duration>) -> String {
    // The opposite fallback to `current_unix_secs`, on purpose: both
    // fail closed. A broken clock here stamps the URL in 1970, so it
    // is already expired. `u64::MAX` would never expire.
    let expires = ttl.map(|d| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |t| t.as_secs().saturating_add(d.as_secs()))
    });
    sign_at(url, secret, expires)
}

/// Like [`sign`], but with the expiry given as unix seconds.
#[must_use]
pub fn sign_at(url: &str, secret: &[u8], expires_at: Option<u64>) -> String {
    let (base, mut params) = parse_url(url);
    params.retain(|(k, _)| k != SIGNATURE_PARAM && k != EXPIRES_PARAM);
    if let Some(exp) = expires_at {
        params.push((EXPIRES_PARAM.to_owned(), exp.to_string()));
    }
    let canonical = canonicalize(&base, &params);
    let signature = compute_signature(&canonical, secret);
    params.push((SIGNATURE_PARAM.to_owned(), signature));
    rebuild_url(&base, &params)
}

/// Check the signature on `url`. It returns `Ok(())` when the URL is
/// genuine and, if it carries `expires`, still in date.
///
/// # Errors
/// [`SignedUrlError`] variants describe the specific failure mode.
pub fn verify(url: &str, secret: &[u8]) -> Result<(), SignedUrlError> {
    verify_at(url, secret, current_unix_secs())
}

/// Like [`verify`], but with the current time given in unix seconds.
/// Useful in tests.
///
/// # Errors
/// See [`SignedUrlError`].
pub fn verify_at(url: &str, secret: &[u8], now_secs: u64) -> Result<(), SignedUrlError> {
    let (base, mut params) = parse_url(url);

    // Extract the signature
    let sig_idx = params
        .iter()
        .position(|(k, _)| k == SIGNATURE_PARAM)
        .ok_or(SignedUrlError::MissingSignature)?;
    let (_, provided_sig) = params.remove(sig_idx);

    // Check expiry
    if let Some((_, exp_str)) = params.iter().find(|(k, _)| k == EXPIRES_PARAM) {
        let exp: u64 = exp_str
            .parse()
            .map_err(|_| SignedUrlError::MalformedSignature)?;
        if now_secs > exp {
            return Err(SignedUrlError::Expired);
        }
    }

    // Recompute over the remaining params
    let canonical = canonicalize(&base, &params);
    let expected = compute_signature(&canonical, secret);

    if expected
        .as_bytes()
        .ct_eq(provided_sig.as_bytes())
        .unwrap_u8()
        != 1
    {
        return Err(SignedUrlError::InvalidSignature);
    }
    Ok(())
}

// ------------------------------------------------------------------ internals

/// Wall clock in unix seconds, failing **closed**.
///
/// `duration_since` fails when the clock is before 1970: a dead RTC
/// battery, a bad NTP step, a container that starts at epoch 0. This
/// answers `u64::MAX` so every URL counts as expired. Answering `0`
/// would make `now > exp` false for every stamp, so a broken clock
/// would accept every expired link.
fn current_unix_secs() -> u64 {
    clock_secs(SystemTime::now().duration_since(UNIX_EPOCH))
}

/// The fallback value, split out so a test can reach it. The clock
/// cannot be broken on demand.
fn clock_secs<E>(elapsed: Result<Duration, E>) -> u64 {
    elapsed.map_or(u64::MAX, |t| t.as_secs())
}

fn parse_url(url: &str) -> (String, Vec<(String, String)>) {
    if let Some((base, query)) = url.split_once('?') {
        let params: Vec<(String, String)> = query
            .split('&')
            .filter(|s| !s.is_empty())
            .map(|pair| match pair.split_once('=') {
                Some((k, v)) => (decode_component(k), decode_component(v)),
                None => (decode_component(pair), String::new()),
            })
            .collect();
        (base.to_owned(), params)
    } else {
        (url.to_owned(), Vec::new())
    }
}

fn canonicalize(base: &str, params: &[(String, String)]) -> String {
    // Sort by key then value, so param order cannot change the sig.
    let mut sorted: Vec<&(String, String)> = params.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let qs: Vec<String> = sorted
        .iter()
        .map(|(k, v)| format!("{}={}", encode_component(k), encode_component(v)))
        .collect();
    if qs.is_empty() {
        base.to_owned()
    } else {
        format!("{base}?{}", qs.join("&"))
    }
}

fn rebuild_url(base: &str, params: &[(String, String)]) -> String {
    if params.is_empty() {
        return base.to_owned();
    }
    let qs: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{}={}", encode_component(k), encode_component(v)))
        .collect();
    format!("{base}?{}", qs.join("&"))
}

fn compute_signature(canonical: &str, secret: &[u8]) -> String {
    use base64::Engine;
    let mut mac = <Hmac<Sha256>>::new_from_slice(secret).expect("HMAC accepts any key");
    mac.update(canonical.as_bytes());
    let bytes = mac.finalize().into_bytes();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn encode_component(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

// Percent-decoding lives in `crate::url_codec::url_decode`.
use crate::url_codec::url_decode as decode_component;

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"my-test-secret";

    #[test]
    fn sign_and_verify_no_expiry() {
        let signed = sign("https://example.com/path", SECRET, None);
        assert!(signed.contains("signature="));
        assert!(verify(&signed, SECRET).is_ok());
    }

    #[test]
    fn sign_preserves_existing_query_params() {
        let signed = sign("https://example.com/path?a=1&b=2", SECRET, None);
        assert!(signed.contains("a=1"));
        assert!(signed.contains("b=2"));
        assert!(verify(&signed, SECRET).is_ok());
    }

    #[test]
    fn verify_fails_on_wrong_secret() {
        let signed = sign("https://example.com/path", SECRET, None);
        let r = verify(&signed, b"different-secret");
        assert_eq!(r, Err(SignedUrlError::InvalidSignature));
    }

    #[test]
    fn verify_fails_on_tampered_path() {
        let signed = sign("https://example.com/admin", SECRET, None);
        let tampered = signed.replace("/admin", "/superadmin");
        let r = verify(&tampered, SECRET);
        assert_eq!(r, Err(SignedUrlError::InvalidSignature));
    }

    #[test]
    fn verify_fails_on_tampered_query_param() {
        let signed = sign("https://example.com/?user_id=42", SECRET, None);
        let tampered = signed.replace("user_id=42", "user_id=99");
        let r = verify(&tampered, SECRET);
        assert_eq!(r, Err(SignedUrlError::InvalidSignature));
    }

    #[test]
    fn verify_fails_when_no_signature() {
        let r = verify("https://example.com/path", SECRET);
        assert_eq!(r, Err(SignedUrlError::MissingSignature));
    }

    #[test]
    fn expired_url_rejected() {
        let past = 100;
        let signed = sign_at("https://example.com/path", SECRET, Some(past));
        let r = verify_at(&signed, SECRET, 1000);
        assert_eq!(r, Err(SignedUrlError::Expired));
    }

    #[test]
    fn unexpired_url_accepted() {
        let future = 10_000;
        let signed = sign_at("https://example.com/path", SECRET, Some(future));
        let r = verify_at(&signed, SECRET, 1000);
        assert!(r.is_ok());
    }

    #[test]
    fn query_param_order_doesnt_change_signature() {
        let url_a = "https://example.com/path?a=1&b=2";
        let url_b = "https://example.com/path?b=2&a=1";
        let sig_a = sign(url_a, SECRET, None);
        // Strip the actual signature from each and re-extract just the canonical form
        // The signatures themselves should be the same since canonicalization sorts.
        let provided_a = sig_a.split("signature=").nth(1).unwrap();
        let sig_b = sign(url_b, SECRET, None);
        let provided_b = sig_b.split("signature=").nth(1).unwrap();
        assert_eq!(provided_a, provided_b, "sorted canonical form must match");
    }

    #[test]
    fn re_signing_overrides_existing_signature() {
        let signed = sign("https://example.com/path", SECRET, None);
        // Re-sign the already-signed URL — should produce the same output, not stack signatures
        let re_signed = sign(&signed, SECRET, None);
        let r = verify(&re_signed, SECRET);
        assert!(r.is_ok());
        // Should only have ONE signature= in the URL
        assert_eq!(re_signed.matches("signature=").count(), 1);
    }

    #[test]
    fn url_encoded_params_round_trip() {
        let original = "https://example.com/?email=alice%40example.com&q=hello%20world";
        let signed = sign(original, SECRET, None);
        assert!(verify(&signed, SECRET).is_ok());
    }

    #[test]
    fn malformed_expires_is_rejected() {
        // Manually craft a URL with a non-numeric expires
        let url = "https://example.com/?expires=not-a-number&signature=anything";
        let r = verify(url, SECRET);
        assert_eq!(r, Err(SignedUrlError::MalformedSignature));
    }

    #[test]
    fn a_clock_before_1970_expires_everything_instead_of_nothing() {
        // `current_unix_secs` answers `u64::MAX` when the clock is
        // unreadable, so `now > exp` holds for every stamp and all
        // links expire. With `0` the opposite happens: one dead RTC
        // battery and every expired link verifies again.
        //
        // First, the fallback value itself:
        assert_eq!(
            clock_secs::<()>(Err(())),
            u64::MAX,
            "an unreadable clock must read as `expired`, not as 1970",
        );
        assert_eq!(clock_secs::<()>(Ok(Duration::from_secs(42))), 42);

        // …then what it does to a real verification.
        let url = sign_at("https://example.com/f.pdf", SECRET, Some(2_000_000_000));
        assert_eq!(
            verify_at(&url, SECRET, u64::MAX),
            Err(SignedUrlError::Expired),
            "the broken-clock fallback must reject, not accept",
        );
        assert_eq!(
            verify_at(&url, SECRET, 0),
            Ok(()),
            "control: the old `0` fallback is exactly what accepted it",
        );
    }

    #[test]
    fn signing_with_a_broken_clock_mints_an_already_dead_url() {
        // The other direction: `sign` keeps the `0` fallback on
        // purpose. A 1970 stamp is refused by any working clock,
        // where `u64::MAX` would mint a URL that never expires.
        let url = sign_at("https://example.com/f.pdf", SECRET, Some(3_600));
        assert_eq!(
            verify_at(&url, SECRET, 1_700_000_000),
            Err(SignedUrlError::Expired),
        );
    }
}
