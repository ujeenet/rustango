//! Random strings — `get_random_string` /
//! `get_random_token_urlsafe`.
//!
//! A uniformly random string of `length` characters drawn from
//! `allowed_chars`. Use it for session IDs, password-reset tokens
//! and email verification codes.
//!
//! Both generators here are cryptographically secure. Bulk fills use
//! `OsRng`; per-character draws use `rand::thread_rng` (ChaCha12,
//! seeded and reseeded from the OS) to avoid one syscall per
//! character. Never swap either for a fast, non-crypto PRNG.
//!
//! ```ignore
//! use rustango::random::{get_random_string, get_random_string_default,
//!                       get_random_token_urlsafe, ALPHANUM_CHARS};
//!
//! // Default alphabet — 12 alphanumeric chars.
//! let session_id: String = get_random_string_default(12);
//!
//! // Custom allowlist as the second arg.
//! let pin: String = get_random_string(6, "0123456789");
//!
//! // URL-safe base64 — better entropy/char than alphanumeric for
//! // reset-token use cases.
//! let reset_token: String = get_random_token_urlsafe(32);
//!
//! // The default allowlist.
//! assert!(ALPHANUM_CHARS.contains('a'));
//! ```
//!
//! ## Choosing length
//!
//! Security tokens need at least 128 bits of entropy:
//!
//! | Alphabet                    | Entropy per char | 128-bit length |
//! |-----------------------------|------------------|----------------|
//! | digits only (10)            | 3.32 bits        | 39 chars       |
//! | alphanumeric mixed-case (62)| 5.95 bits        | 22 chars       |
//! | URL-safe base64 (64)        | 6 bits           | 22 chars       |
//!
//! `get_random_string_default(N)` uses the mixed-case alphanumeric
//! alphabet, [`ALPHANUM_CHARS`](crate::random::ALPHANUM_CHARS).

use rand::{Rng, RngCore};

/// Mixed-case ASCII letters and digits — the default
/// `get_random_string` alphabet.
pub const ALPHANUM_CHARS: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

/// Return a uniformly random string of `length` characters chosen
/// from `allowed_chars`. Backed by `rand::thread_rng`, a CSPRNG
/// seeded from the OS, so it is safe for security tokens.
///
/// Pass any of [`ALPHANUM_CHARS`], [`URL_SAFE_CHARS`],
/// [`DIGITS_CHARS`], [`HEX_CHARS`], [`UPPERCASE_HEX_CHARS`], or your
/// own alphabet.
///
/// # Panics
/// Panics if `allowed_chars` is empty. An empty alphabet has no
/// meaning, so this is always a caller bug.
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

/// `get_random_string(length, ALPHANUM_CHARS)` — the default
/// alphabet.
#[must_use]
pub fn get_random_string_default(length: usize) -> String {
    get_random_string(length, ALPHANUM_CHARS)
}

/// URL-safe base64 alphabet (RFC 4648 §5), 64 chars, no padding.
/// 6 bits of entropy per char. Matches Python's
/// `secrets.token_urlsafe`.
pub const URL_SAFE_CHARS: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// `[0-9]`, for PINs and verification codes.
pub const DIGITS_CHARS: &str = "0123456789";

/// Lowercase hex `[0-9a-f]`, for compact opaque identifiers.
pub const HEX_CHARS: &str = "0123456789abcdef";

/// Uppercase hex `[0-9A-F]`.
pub const UPPERCASE_HEX_CHARS: &str = "0123456789ABCDEF";

/// Lowercase letters `[a-z]`, for case-insensitive reference or
/// coupon codes.
pub const LOWERCASE_CHARS: &str = "abcdefghijklmnopqrstuvwxyz";

/// Uppercase letters `[A-Z]`.
pub const UPPERCASE_CHARS: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";

/// Mixed-case letters `[a-zA-Z]` (no digits).
pub const LETTERS_CHARS: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";

/// URL-safe random token of `length` characters, 6 bits of entropy
/// each. Same result as `get_random_string(length, URL_SAFE_CHARS)`,
/// but faster: it masks raw `OsRng` bytes instead of indexing chars.
///
/// 32 chars gives 192 bits, well past the 128-bit target for session
/// and reset URLs.
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

/// Random lowercase-hex string. Wraps
/// `get_random_string(length, HEX_CHARS)`.
///
/// 4 bits per char, so `length = 32` is about 128 bits — enough for
/// opaque IDs, idempotency keys and short state tokens.
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

/// Random `[a-zA-Z0-9]` string. Wraps
/// `get_random_string(length, ALPHANUM_CHARS)`.
///
/// About 5.95 bits per char, so `length = 22` is roughly 128 bits.
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

/// Random `[a-zA-Z]` string, no digits. Wraps
/// `get_random_string(length, LETTERS_CHARS)`.
///
/// About 5.7 bits per char, so `length = 23` is roughly 128 bits.
///
/// ```ignore
/// use rustango::random::random_letters;
/// let s = random_letters(8);
/// assert_eq!(s.len(), 8);
/// assert!(s.chars().all(|c| c.is_ascii_alphabetic()));
/// ```
#[must_use]
pub fn random_letters(length: usize) -> String {
    get_random_string(length, LETTERS_CHARS)
}

/// Random `[a-z]` string. Wraps
/// `get_random_string(length, LOWERCASE_CHARS)`.
///
/// About 4.7 bits per char, so `length = 28` is roughly 128 bits.
///
/// ```ignore
/// use rustango::random::random_lowercase;
/// let s = random_lowercase(8);
/// assert!(s.chars().all(|c| c.is_ascii_lowercase()));
/// ```
#[must_use]
pub fn random_lowercase(length: usize) -> String {
    get_random_string(length, LOWERCASE_CHARS)
}

/// Random `[A-Z]` string. Wraps
/// `get_random_string(length, UPPERCASE_CHARS)`.
///
/// ```ignore
/// use rustango::random::random_uppercase;
/// let s = random_uppercase(8);
/// assert!(s.chars().all(|c| c.is_ascii_uppercase()));
/// ```
#[must_use]
pub fn random_uppercase(length: usize) -> String {
    get_random_string(length, UPPERCASE_CHARS)
}

/// Random `[0-9]` string. Wraps
/// `get_random_string(length, DIGITS_CHARS)`. Good for OTP codes,
/// PINs and short SMS verification tokens.
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
        // Collision odds are ~1 / 62^32. A failure means the RNG is
        // broken or seeded with a constant.
        let a = get_random_string(32, ALPHANUM_CHARS);
        let b = get_random_string(32, ALPHANUM_CHARS);
        assert_ne!(a, b, "two consecutive calls returned identical strings");
    }

    #[test]
    fn get_random_string_distributes_across_alphabet() {
        // 1024 draws from a 4-char alphabet: every char must show up.
        // P(missing one) is (3/4)^1024, so this cannot flake.
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
        // The alphabet is a sequence of chars, not bytes, so
        // multi-byte UTF-8 entries must work.
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
        // 64 = 2^6, so each char is exactly 6 bits. The masking
        // shortcut in get_random_token_urlsafe depends on it.
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
        assert!(random_letters(0).is_empty());
        assert!(random_lowercase(0).is_empty());
        assert!(random_uppercase(0).is_empty());
    }

    #[test]
    fn random_letters_mixed_case_only() {
        let s = random_letters(40);
        assert_eq!(s.chars().count(), 40);
        assert!(s.chars().all(|c| c.is_ascii_alphabetic()));
    }

    #[test]
    fn random_lowercase_only() {
        let s = random_lowercase(40);
        assert_eq!(s.chars().count(), 40);
        assert!(s.chars().all(|c| c.is_ascii_lowercase()));
    }

    #[test]
    fn random_uppercase_only() {
        let s = random_uppercase(40);
        assert_eq!(s.chars().count(), 40);
        assert!(s.chars().all(|c| c.is_ascii_uppercase()));
    }

    #[test]
    fn letter_alphabet_counts() {
        assert_eq!(LOWERCASE_CHARS.chars().count(), 26);
        assert_eq!(UPPERCASE_CHARS.chars().count(), 26);
        assert_eq!(LETTERS_CHARS.chars().count(), 52);
    }
}
