//! TOTP — Time-based One-Time Passwords (RFC 6238) for 2FA.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::totp::{TotpSecret, generate, verify, otpauth_url};
//!
//! // Enrollment: generate a fresh secret, store it on the user row
//! let secret = TotpSecret::generate();
//! user.totp_secret = secret.to_base32();
//!
//! // Show user the QR code (encode this URL as a QR):
//! let url = otpauth_url("MyApp", &user.email, &secret);
//!
//! // Verify a 6-digit code from the user's authenticator app:
//! if verify(&secret, "123456", 30, 6, 1) {
//!     // accepted
//! }
//! ```
//!
//! ## Configuration
//!
//! All functions accept `step_secs` (default 30 seconds), `digits`
//! (default 6), and `window` (number of past/future steps to accept,
//! default 1 → tolerates ±30 seconds of clock drift).

use std::time::{SystemTime, UNIX_EPOCH};

/// A TOTP shared secret (raw bytes). Store base32-encoded on the user row.
#[derive(Debug, Clone)]
pub struct TotpSecret(pub Vec<u8>);

impl TotpSecret {
    /// Generate a 20-byte random secret (RFC 4226 minimum).
    ///
    /// Uses [`rand::rngs::OsRng`] — the OS CSPRNG — rather than
    /// `thread_rng`. `thread_rng` is seeded from `OsRng` but is a
    /// userspace ChaCha PRNG that doesn't satisfy "cryptographic key
    /// material" guarantees on every platform; for 2FA shared secrets
    /// the OS CSPRNG is the right default. Matches the rest of the
    /// framework's crypto sites (sessions, CSP nonce, CSRF token,
    /// signed URLs).
    #[must_use]
    pub fn generate() -> Self {
        use rand::rngs::OsRng;
        use rand::RngCore;
        let mut bytes = vec![0u8; 20];
        OsRng.fill_bytes(&mut bytes);
        Self(bytes)
    }

    /// Encode the secret as base32 (no padding) — the standard format
    /// understood by Google Authenticator, Authy, 1Password, etc.
    #[must_use]
    pub fn to_base32(&self) -> String {
        base32_encode(&self.0)
    }

    /// Decode a base32-encoded secret string (with or without padding).
    /// Returns `None` for invalid base32.
    #[must_use]
    pub fn from_base32(s: &str) -> Option<Self> {
        base32_decode(s).map(Self)
    }
}

/// Generate the TOTP code for `secret` at the current time.
#[must_use]
pub fn generate(secret: &TotpSecret, step_secs: u64, digits: u32) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    generate_at(secret, now, step_secs, digits)
}

/// Generate the TOTP code for `secret` at a specific unix timestamp.
/// Useful for testing.
#[must_use]
pub fn generate_at(secret: &TotpSecret, unix_secs: u64, step_secs: u64, digits: u32) -> String {
    let counter = unix_secs / step_secs.max(1);
    hotp(&secret.0, counter, digits)
}

/// Verify a user-supplied code against `secret`. Accepts codes from
/// `[now - window, now + window]` to tolerate clock drift.
///
/// `window = 1` (default) tolerates ±1 step (±30 seconds at default step).
#[must_use]
pub fn verify(secret: &TotpSecret, code: &str, step_secs: u64, digits: u32, window: i64) -> bool {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    verify_at(secret, code, now, step_secs, digits, window)
}

/// Verify at a specific unix timestamp — useful for tests.
#[must_use]
pub fn verify_at(
    secret: &TotpSecret,
    code: &str,
    unix_secs: u64,
    step_secs: u64,
    digits: u32,
    window: i64,
) -> bool {
    let step = step_secs.max(1) as i64;
    let center = (unix_secs as i64) / step;
    for offset in -window..=window {
        let counter = match (center + offset).try_into() {
            Ok(c) => c,
            Err(_) => continue,
        };
        if constant_time_eq(&hotp(&secret.0, counter, digits), code) {
            return true;
        }
    }
    false
}

/// Build the `otpauth://` URI for a TOTP secret. Encode this as a QR code
/// to make enrollment one-tap for the user.
#[must_use]
pub fn otpauth_url(issuer: &str, account: &str, secret: &TotpSecret) -> String {
    let label = format!("{}:{}", url_encode(issuer), url_encode(account));
    format!(
        "otpauth://totp/{label}?secret={}&issuer={}&algorithm=SHA1&digits=6&period=30",
        secret.to_base32(),
        url_encode(issuer),
    )
}

// ------------------------------------------------------------------ HOTP (RFC 4226)

/// HMAC-Based One-Time Password — the building block for TOTP.
fn hotp(secret: &[u8], counter: u64, digits: u32) -> String {
    use hmac::{Hmac, Mac};
    use sha1::Sha1;

    let mut mac = <Hmac<Sha1>>::new_from_slice(secret).expect("HMAC accepts any key");
    mac.update(&counter.to_be_bytes());
    let hash = mac.finalize().into_bytes();

    // Dynamic truncation per RFC 4226
    let offset = (hash[hash.len() - 1] & 0x0f) as usize;
    let bin_code = ((hash[offset] & 0x7f) as u32) << 24
        | (hash[offset + 1] as u32) << 16
        | (hash[offset + 2] as u32) << 8
        | (hash[offset + 3] as u32);

    let modulo = 10u32.pow(digits.min(10));
    let value = bin_code % modulo;
    format!("{:0width$}", value, width = digits as usize)
}

// ------------------------------------------------------------------ Base32

const BASE32_ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

fn base32_encode(input: &[u8]) -> String {
    let mut out = String::new();
    let mut buffer = 0u64;
    let mut bits_left = 0;
    for &b in input {
        buffer = (buffer << 8) | u64::from(b);
        bits_left += 8;
        while bits_left >= 5 {
            bits_left -= 5;
            let idx = ((buffer >> bits_left) & 0x1f) as usize;
            out.push(BASE32_ALPHABET[idx] as char);
        }
    }
    if bits_left > 0 {
        let idx = ((buffer << (5 - bits_left)) & 0x1f) as usize;
        out.push(BASE32_ALPHABET[idx] as char);
    }
    out
}

fn base32_decode(input: &str) -> Option<Vec<u8>> {
    let mut buffer = 0u64;
    let mut bits = 0;
    let mut out = Vec::new();
    for c in input.chars() {
        if c == '=' || c.is_whitespace() {
            continue;
        }
        let upper = c.to_ascii_uppercase();
        let val = BASE32_ALPHABET.iter().position(|&b| b == upper as u8)? as u64;
        buffer = (buffer << 5) | val;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

// ------------------------------------------------------------------ helpers

fn constant_time_eq(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    if a.len() != b.len() {
        return false;
    }
    a.as_bytes().ct_eq(b.as_bytes()).unwrap_u8() == 1
}

fn url_encode(s: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc6238_test_vector_sha1() {
        // RFC 6238 Appendix B test vectors (SHA1, T=59).
        // Secret: ASCII "12345678901234567890" (20 bytes).
        let secret = TotpSecret(b"12345678901234567890".to_vec());
        let code = generate_at(&secret, 59, 30, 8);
        assert_eq!(code, "94287082");
    }

    #[test]
    fn rfc6238_test_vector_t_1111111109() {
        let secret = TotpSecret(b"12345678901234567890".to_vec());
        let code = generate_at(&secret, 1_111_111_109, 30, 8);
        assert_eq!(code, "07081804");
    }

    #[test]
    fn generate_returns_correct_digit_count() {
        let s = TotpSecret::generate();
        for digits in [6, 7, 8] {
            let c = generate(&s, 30, digits);
            assert_eq!(c.len(), digits as usize);
            assert!(c.chars().all(|c| c.is_ascii_digit()));
        }
    }

    #[test]
    fn verify_accepts_current_step() {
        let s = TotpSecret(b"12345678901234567890".to_vec());
        let code = generate_at(&s, 100, 30, 6);
        assert!(verify_at(&s, &code, 100, 30, 6, 1));
    }

    #[test]
    fn verify_accepts_within_window() {
        let s = TotpSecret(b"12345678901234567890".to_vec());
        // Code from 30 seconds ago (one step) — should be accepted with window=1
        let code = generate_at(&s, 100 - 30, 30, 6);
        assert!(verify_at(&s, &code, 100, 30, 6, 1));
        // One in the future
        let code = generate_at(&s, 100 + 30, 30, 6);
        assert!(verify_at(&s, &code, 100, 30, 6, 1));
    }

    #[test]
    fn verify_rejects_outside_window() {
        let s = TotpSecret(b"12345678901234567890".to_vec());
        // Code from 5 minutes ago — way outside window=1
        let now = 10_000;
        let code = generate_at(&s, now - 300, 30, 6);
        assert!(!verify_at(&s, &code, now, 30, 6, 1));
    }

    #[test]
    fn verify_rejects_wrong_code() {
        let s = TotpSecret(b"12345678901234567890".to_vec());
        assert!(!verify_at(&s, "000000", 100, 30, 6, 1));
    }

    #[test]
    fn base32_roundtrip() {
        let original: Vec<u8> = (0..20).collect();
        let encoded = base32_encode(&original);
        let decoded = base32_decode(&encoded).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn base32_decode_handles_padding_and_whitespace() {
        let original = b"hello world".to_vec();
        let encoded = base32_encode(&original);
        // Add some padding + whitespace to mimic real-world input
        let messy = format!("{} ==", encoded);
        let decoded = base32_decode(&messy).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn base32_decode_invalid_returns_none() {
        assert_eq!(base32_decode("not valid 0189!!"), None);
    }

    #[test]
    fn secret_from_base32_roundtrip() {
        let s1 = TotpSecret::generate();
        let encoded = s1.to_base32();
        let s2 = TotpSecret::from_base32(&encoded).unwrap();
        assert_eq!(s1.0, s2.0);
    }

    #[test]
    fn otpauth_url_format() {
        let secret = TotpSecret(b"12345678901234567890".to_vec());
        let url = otpauth_url("MyApp", "alice@example.com", &secret);
        assert!(url.starts_with("otpauth://totp/MyApp:alice%40example.com?"));
        assert!(url.contains("secret="));
        assert!(url.contains("issuer=MyApp"));
        assert!(url.contains("digits=6"));
        assert!(url.contains("period=30"));
    }

    // ---- v0.42 regressions: lock in OsRng quality ----

    #[test]
    fn generate_produces_full_20_byte_secrets() {
        // Defends against future regressions where someone shortens
        // the secret below RFC 4226's 160-bit minimum.
        for _ in 0..16 {
            let s = TotpSecret::generate();
            assert_eq!(s.0.len(), 20, "TOTP secret must be exactly 20 bytes");
        }
    }

    #[test]
    fn generate_produces_unique_secrets() {
        // 64 secrets * 20 bytes = 160 bits of entropy each. Collisions
        // at this rate would mean ~1 in 2^80 per pair — astronomically
        // impossible with a working CSPRNG. A regression to a constant
        // / counter / weakly-seeded PRNG would surface immediately.
        use std::collections::HashSet;
        let mut seen: HashSet<Vec<u8>> = HashSet::new();
        for _ in 0..64 {
            let s = TotpSecret::generate();
            assert!(seen.insert(s.0), "duplicate TOTP secret — RNG regressed");
        }
    }

    #[test]
    fn generate_does_not_produce_trivial_secrets() {
        // No all-zero, no all-0xff, no fixed-byte secrets — those are
        // the failure modes a broken RNG would exhibit.
        let s = TotpSecret::generate();
        assert!(s.0.iter().any(|&b| b != 0), "all-zero secret");
        assert!(s.0.iter().any(|&b| b != 0xff), "all-ones secret");
        let first = s.0[0];
        assert!(
            s.0.iter().any(|&b| b != first),
            "fixed-byte secret (every byte = {first:#x}) — RNG returns constant"
        );
    }
}
