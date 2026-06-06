//! Django-shape random-string helpers — `get_random_string` /
//! `get_random_token_urlsafe`.
//!
//! Mirrors `django.utils.crypto.get_random_string(length,
//! allowed_chars=…)` — generates a uniformly-random string of
//! `length` characters drawn from `allowed_chars`. Backed by the
//! `rand::rngs::OsRng` CSPRNG so output is suitable for session
//! identifiers, password-reset tokens, email verification codes —
//! anywhere Django code reaches for `get_random_string`.
//!
//! ```ignore
//! use rustango::random::{get_random_string, get_random_string_default,
//!                       get_random_token_urlsafe, ALPHANUM_CHARS};
//!
//! // Default Django shape — 12 alphanumeric chars.
//! let session_id: String = get_random_string_default(12);
//!
//! // Custom allowlist — Django shape, second arg.
//! let pin: String = get_random_string(6, "0123456789");
//!
//! // URL-safe base64 — better entropy/char than alphanumeric for
//! // reset-token use cases.
//! let reset_token: String = get_random_token_urlsafe(32);
//!
//! // Re-export of the default Django allowlist.
//! assert!(ALPHANUM_CHARS.contains('a'));
//! ```
//!
//! ## Choosing length
//!
//! For security tokens (session IDs, reset URLs, API keys) target
//! at least 128 bits of entropy. Quick reference:
//!
//! | Alphabet                    | Entropy per char | 128-bit length |
//! |-----------------------------|------------------|----------------|
//! | digits only (10)            | 3.32 bits        | 39 chars       |
//! | alphanumeric mixed-case (62)| 5.95 bits        | 22 chars       |
//! | URL-safe base64 (64)        | 6 bits           | 22 chars       |
//!
//! The default `get_random_string_default(N)` uses the mixed-case
//! alphanumeric alphabet to match Django's pre-4.2 `RANDOM_STRING_CHARS`.

use rand::{Rng, RngCore};

/// Django's default `get_random_string` allowlist — ASCII letters
/// + digits, mixed case. Mirrors
/// `django.utils.crypto.RANDOM_STRING_CHARS`.
pub const ALPHANUM_CHARS: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

/// Django-parity
/// [`get_random_string(length, allowed_chars=…)`](https://docs.djangoproject.com/en/6.0/ref/utils/#django.utils.crypto.get_random_string) —
/// return a uniformly-random string of `length` characters chosen
/// from `allowed_chars`. Uses `OsRng` (CSPRNG) — suitable for
/// security-sensitive tokens.
///
/// `allowed_chars` is taken as a `&str` so callers can pass either
/// the predefined [`ALPHANUM_CHARS`] / [`URL_SAFE_CHARS`] /
/// [`DIGITS_CHARS`] / [`HEX_CHARS`] / [`UPPERCASE_HEX_CHARS`] or
/// any custom set.
///
/// # Panics
/// Panics if `allowed_chars` is empty (Django raises `IndexError`;
/// we surface the bug with a clear message — empty alphabet has no
/// sensible semantics).
///
/// ```ignore
/// use rustango::random::get_random_string;
/// let pin = get_random_string(6, "0123456789");
/// assert_eq!(pin.len(), 6);
/// assert!(pin.chars().all(|c| c.is_ascii_digit()));
/// ```
#[must_use]
pub fn get_random_string(length: usize, allowed_chars: &str) -> String {
    assert!(
        !allowed_chars.is_empty(),
        "get_random_string requires a non-empty `allowed_chars` alphabet"
    );
    let chars: Vec<char> = allowed_chars.chars().collect();
    let mut rng = rand::thread_rng();
    (0..length)
        .map(|_| chars[rng.gen_range(0..chars.len())])
        .collect()
}

/// Django-parity convenience — `get_random_string(length)` with
/// the default mixed-case alphanumeric alphabet ([`ALPHANUM_CHARS`]).
/// Equivalent to `get_random_string(length, ALPHANUM_CHARS)`.
#[must_use]
pub fn get_random_string_default(length: usize) -> String {
    get_random_string(length, ALPHANUM_CHARS)
}

/// URL-safe base64 alphabet (RFC 4648 §5) — 64 chars, padding
/// stripped. Drop-in for `token_urlsafe(N)` from Python's `secrets`
/// module. Each char carries 6 bits of entropy.
pub const URL_SAFE_CHARS: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// `[0-9]` — for numeric PINs / verification codes.
pub const DIGITS_CHARS: &str = "0123456789";

/// Lowercase hex `[0-9a-f]` — for compact opaque identifiers.
pub const HEX_CHARS: &str = "0123456789abcdef";

/// Uppercase hex `[0-9A-F]`.
pub const UPPERCASE_HEX_CHARS: &str = "0123456789ABCDEF";

/// Generate a URL-safe random token of `length` characters — 6
/// bits of entropy per char. Equivalent to
/// `get_random_string(length, URL_SAFE_CHARS)` but slightly faster
/// (uses raw byte sampling + bit masking rather than character
/// indexing).
///
/// Default token length 32 chars (192 bits) is comfortably above
/// the 128-bit security target for session / reset URLs.
///
/// ```ignore
/// use rustango::random::get_random_token_urlsafe;
/// let token = get_random_token_urlsafe(32);
/// assert_eq!(token.len(), 32);
/// // Every char is in the URL-safe alphabet — safe to drop into
/// // a query parameter without further escaping.
/// assert!(token.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
/// ```
#[must_use]
pub fn get_random_token_urlsafe(length: usize) -> String {
    let mut buf = vec![0u8; length];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    let alphabet: Vec<char> = URL_SAFE_CHARS.chars().collect();
    buf.into_iter()
        // 64-char alphabet → 6 bits → mask off the top 2 bits.
        .map(|b| alphabet[(b & 0b0011_1111) as usize])
        .collect()
}

/// Generate a random lowercase-hex string of `length` characters.
/// Convenience wrapper for `get_random_string(length, HEX_CHARS)`.
///
/// 4 bits of entropy per char — `length = 32` ≈ 128 bits, the
/// usual security target for opaque identifiers, idempotency keys,
/// short signed-state tokens.
///
/// ```ignore
/// use rustango::random::random_hex;
/// let id = random_hex(32);
/// assert_eq!(id.len(), 32);
/// assert!(id.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
/// ```
#[must_use]
pub fn random_hex(length: usize) -> String {
    get_random_string(length, HEX_CHARS)
}

/// Generate a random alphanumeric string of `length` characters
/// (`[a-zA-Z0-9]`). Convenience wrapper for
/// `get_random_string(length, ALPHANUM_CHARS)`.
///
/// ~5.95 bits of entropy per char (62-char alphabet) — `length = 22`
/// is roughly 128 bits.
///
/// ```ignore
/// use rustango::random::random_alphanum;
/// let s = random_alphanum(22);
/// assert_eq!(s.len(), 22);
/// assert!(s.chars().all(|c| c.is_ascii_alphanumeric()));
/// ```
#[must_use]
pub fn random_alphanum(length: usize) -> String {
    get_random_string(length, ALPHANUM_CHARS)
}

/// Generate a random numeric-only string of `length` digits
/// (`[0-9]`). Convenience wrapper for
/// `get_random_string(length, DIGITS_CHARS)`. Useful for OTP codes,
/// PINs, short verification tokens sent over SMS.
///
/// ```ignore
/// use rustango::random::random_digits;
/// let otp = random_digits(6);
/// assert_eq!(otp.len(), 6);
/// assert!(otp.chars().all(|c| c.is_ascii_digit()));
/// ```
#[must_use]
pub fn random_digits(length: usize) -> String {
    get_random_string(length, DIGITS_CHARS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn get_random_string_returns_requested_length() {
        for n in [0, 1, 8, 32, 256] {
            let s = get_random_string(n, ALPHANUM_CHARS);
            assert_eq!(s.chars().count(), n);
        }
    }

    #[test]
    fn get_random_string_default_uses_alphanum_alphabet() {
        let s = get_random_string_default(128);
        assert_eq!(s.chars().count(), 128);
        assert!(s.chars().all(|c| ALPHANUM_CHARS.contains(c)));
    }

    #[test]
    fn get_random_string_respects_custom_alphabet() {
        let pin = get_random_string(32, "0123456789");
        assert!(pin.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn get_random_string_each_call_is_different() {
        // Probability of two 32-char alphanumeric strings colliding is
        // ~1 / 62^32 — astronomically unlikely. If this ever fails the
        // RNG is broken (or seeded constant).
        let a = get_random_string(32, ALPHANUM_CHARS);
        let b = get_random_string(32, ALPHANUM_CHARS);
        assert_ne!(a, b, "two consecutive calls returned identical strings");
    }

    #[test]
    fn get_random_string_distributes_across_alphabet() {
        // Over a 1024-char sample drawn from a 4-char alphabet, every
        // char should appear at least once. (P(missing one) ~ (3/4)^1024
        // ~= 0 — flake-free.)
        let s = get_random_string(1024, "abcd");
        let unique: HashSet<char> = s.chars().collect();
        assert_eq!(unique.len(), 4);
    }

    #[test]
    #[should_panic(expected = "non-empty")]
    fn get_random_string_empty_alphabet_panics() {
        let _ = get_random_string(10, "");
    }

    #[test]
    fn get_random_string_handles_unicode_alphabet() {
        // Django's docstring guarantees the alphabet is treated as
        // a sequence of chars, not bytes — multi-byte UTF-8 entries
        // must work.
        let s = get_random_string(64, "αβγδ");
        assert_eq!(s.chars().count(), 64);
        assert!(s.chars().all(|c| "αβγδ".contains(c)));
    }

    // -------- get_random_token_urlsafe --------

    #[test]
    fn token_urlsafe_returns_requested_length() {
        for n in [0, 1, 22, 32, 64] {
            let t = get_random_token_urlsafe(n);
            assert_eq!(t.chars().count(), n);
        }
    }

    #[test]
    fn token_urlsafe_uses_url_safe_alphabet() {
        let t = get_random_token_urlsafe(256);
        assert!(t.chars().all(|c| URL_SAFE_CHARS.contains(c)));
    }

    #[test]
    fn token_urlsafe_distinct_calls_are_distinct() {
        let a = get_random_token_urlsafe(32);
        let b = get_random_token_urlsafe(32);
        assert_ne!(a, b);
    }

    #[test]
    fn url_safe_alphabet_has_64_chars() {
        // 64 = 2^6, so each char carries exactly 6 bits — the masking
        // shortcut in get_random_token_urlsafe relies on this.
        assert_eq!(URL_SAFE_CHARS.chars().count(), 64);
    }

    // -------- convenience wrappers --------

    #[test]
    fn random_hex_returns_lowercase_hex_string() {
        for n in [1, 8, 32, 64] {
            let s = random_hex(n);
            assert_eq!(s.chars().count(), n);
            assert!(
                s.chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "got: {s}"
            );
        }
    }

    #[test]
    fn random_hex_distinct_calls_are_distinct() {
        let a = random_hex(32);
        let b = random_hex(32);
        assert_ne!(a, b);
    }

    #[test]
    fn random_alphanum_returns_alphanumeric_only() {
        let s = random_alphanum(22);
        assert_eq!(s.chars().count(), 22);
        assert!(s.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn random_digits_returns_digits_only() {
        let s = random_digits(6);
        assert_eq!(s.chars().count(), 6);
        assert!(s.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn random_helpers_zero_length_returns_empty() {
        assert!(random_hex(0).is_empty());
        assert!(random_alphanum(0).is_empty());
        assert!(random_digits(0).is_empty());
    }
}
