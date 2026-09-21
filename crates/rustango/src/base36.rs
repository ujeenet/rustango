//! Base36 integer encoding: [`int_to_base36`] / [`base36_to_int`].
//!
//! Django uses base36 in password-reset URLs
//! (`/reset/<uidb36>/<token>/`) to write the user PK as a short,
//! URL-safe string. The alphabet is `[0-9a-z]`, so `100000000`
//! becomes `1njchs`.
//!
//! ```ignore
//! use rustango::base36::{int_to_base36, base36_to_int};
//!
//! assert_eq!(int_to_base36(0), "0");
//! assert_eq!(int_to_base36(35), "z");
//! assert_eq!(int_to_base36(36), "10");
//! assert_eq!(int_to_base36(100_000_000), "1njchs");
//!
//! // Round-trip.
//! for n in [0u64, 1, 35, 36, 1000, 1_000_000_000_000] {
//!     assert_eq!(base36_to_int(&int_to_base36(n)).unwrap(), n);
//! }
//!
//! // Reject negative-shaped or out-of-alphabet input.
//! assert!(base36_to_int("-1").is_err());
//! assert!(base36_to_int("FOO").is_err()); // uppercase rejected (Django shape)
//! ```
//!
//! Only non-negative integers encode. Decoding accepts lowercase
//! only, so `1A` and `1a` can never map to the same number.
//!
//! [`int_to_base36`]: crate::base36::int_to_base36
//! [`base36_to_int`]: crate::base36::base36_to_int

/// Base36 digits, `[0-9a-z]`.
const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

/// Errors from [`base36_to_int`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Base36Error {
    /// Empty string.
    #[error("base36_to_int: empty input")]
    Empty,

    /// Character outside `[0-9a-z]`. Uppercase is also rejected.
    #[error("base36_to_int: invalid character `{0}` (allowed: 0-9, a-z)")]
    InvalidChar(char),

    /// The value does not fit in `u64`.
    #[error("base36_to_int: value overflows u64")]
    Overflow,
}

/// Encode a non-negative integer as a lowercase base36 string.
/// Output matches Django's `int_to_base36(n)`.
///
/// ```ignore
/// use rustango::base36::int_to_base36;
/// assert_eq!(int_to_base36(0), "0");
/// assert_eq!(int_to_base36(10), "a");
/// assert_eq!(int_to_base36(35), "z");
/// assert_eq!(int_to_base36(36), "10");
/// assert_eq!(int_to_base36(1295), "zz");
/// assert_eq!(int_to_base36(1296), "100");
/// ```
#[must_use]
pub fn int_to_base36(mut n: u64) -> String {
    if n == 0 {
        return "0".to_owned();
    }
    let mut buf: Vec<u8> = Vec::with_capacity(13); // ⌈log36(u64::MAX)⌉ = 13
    while n > 0 {
        buf.push(ALPHABET[(n % 36) as usize]);
        n /= 36;
    }
    buf.reverse();
    // SAFETY: ALPHABET is ASCII-only, so `buf` is valid UTF-8.
    String::from_utf8(buf).expect("base36 alphabet is ASCII")
}

/// Decode a base36 string into a `u64`. Only `[0-9a-z]` is allowed.
/// Uppercase, whitespace and a leading `-` are all rejected, like
/// Django's `base36_to_int(s)`.
///
/// # Errors
/// * [`Base36Error::Empty`] — empty string.
/// * [`Base36Error::InvalidChar`] — character outside `[0-9a-z]`.
/// * [`Base36Error::Overflow`] — value exceeds `u64::MAX`.
pub fn base36_to_int(s: &str) -> Result<u64, Base36Error> {
    if s.is_empty() {
        return Err(Base36Error::Empty);
    }
    let mut out: u64 = 0;
    for c in s.chars() {
        let digit = match c {
            '0'..='9' => (c as u32) - ('0' as u32),
            'a'..='z' => (c as u32) - ('a' as u32) + 10,
            _ => return Err(Base36Error::InvalidChar(c)),
        };
        out = out
            .checked_mul(36)
            .and_then(|n| n.checked_add(u64::from(digit)))
            .ok_or(Base36Error::Overflow)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------- int_to_base36 --------

    #[test]
    fn encode_zero() {
        assert_eq!(int_to_base36(0), "0");
    }

    #[test]
    fn encode_single_digit_boundary() {
        assert_eq!(int_to_base36(9), "9");
        assert_eq!(int_to_base36(10), "a"); // crossover digits→letters
        assert_eq!(int_to_base36(35), "z");
    }

    #[test]
    fn encode_two_digit_boundary() {
        assert_eq!(int_to_base36(36), "10");
        assert_eq!(int_to_base36(37), "11");
        assert_eq!(int_to_base36(71), "1z"); // 36 + 35
        assert_eq!(int_to_base36(72), "20");
    }

    #[test]
    fn encode_large_canonical_values() {
        // Django docstring example.
        assert_eq!(int_to_base36(1295), "zz");
        assert_eq!(int_to_base36(1296), "100");
        // 36^6 = 2_176_782_336
        assert_eq!(int_to_base36(2_176_782_336), "1000000");
    }

    #[test]
    fn encode_u64_max_doesnt_panic() {
        // u64::MAX needs 13 base36 digits.
        let s = int_to_base36(u64::MAX);
        assert_eq!(s.len(), 13);
        assert!(s
            .chars()
            .all(|c| c.is_ascii_alphanumeric() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn encode_is_always_lowercase() {
        // Decoders reject uppercase, so the encoder must never emit it.
        for n in [10u64, 100, 1000, 12345, 999_999_999_999] {
            let s = int_to_base36(n);
            assert!(
                s.chars().all(|c| !c.is_ascii_uppercase()),
                "value {n} encoded as `{s}` with uppercase letters"
            );
        }
    }

    // -------- base36_to_int --------

    #[test]
    fn decode_zero() {
        assert_eq!(base36_to_int("0").unwrap(), 0);
    }

    #[test]
    fn decode_simple_digits() {
        assert_eq!(base36_to_int("9").unwrap(), 9);
        assert_eq!(base36_to_int("a").unwrap(), 10);
        assert_eq!(base36_to_int("z").unwrap(), 35);
    }

    #[test]
    fn decode_two_digit() {
        assert_eq!(base36_to_int("10").unwrap(), 36);
        assert_eq!(base36_to_int("zz").unwrap(), 1295);
        assert_eq!(base36_to_int("100").unwrap(), 1296);
    }

    #[test]
    fn decode_empty_is_error() {
        assert_eq!(base36_to_int(""), Err(Base36Error::Empty));
    }

    #[test]
    fn decode_rejects_uppercase() {
        // Accepting it would alias `1a` and `1A`.
        let err = base36_to_int("FOO").unwrap_err();
        assert!(matches!(err, Base36Error::InvalidChar('F')));
    }

    #[test]
    fn decode_rejects_leading_dash() {
        // base36 here is unsigned.
        let err = base36_to_int("-1").unwrap_err();
        assert!(matches!(err, Base36Error::InvalidChar('-')));
    }

    #[test]
    fn decode_rejects_whitespace() {
        assert!(matches!(
            base36_to_int(" 0").unwrap_err(),
            Base36Error::InvalidChar(' ')
        ));
        assert!(matches!(
            base36_to_int("0 ").unwrap_err(),
            Base36Error::InvalidChar(' ')
        ));
    }

    #[test]
    fn decode_rejects_non_ascii() {
        assert!(matches!(
            base36_to_int("ω").unwrap_err(),
            Base36Error::InvalidChar('ω')
        ));
    }

    #[test]
    fn decode_overflow_surfaces_as_error() {
        // 13 'z's = 36^13 - 1 = 170,581,728,179,578,208,255 > u64::MAX
        let err = base36_to_int("zzzzzzzzzzzzzz").unwrap_err(); // 14 z's
        assert_eq!(err, Base36Error::Overflow);
    }

    // -------- round-trip --------

    #[test]
    fn round_trip_canonical_values() {
        for n in [
            0u64,
            1,
            9,
            10,
            35,
            36,
            71,
            72,
            1000,
            12345,
            1_000_000,
            1_000_000_000_000,
            u64::MAX - 1,
            u64::MAX,
        ] {
            assert_eq!(base36_to_int(&int_to_base36(n)).unwrap(), n, "n = {n}");
        }
    }
}
