//! Base62 integer encoding: [`int_to_base62`] / [`base62_to_int`].
//!
//! The alphabet is `[0-9A-Za-z]` and encoding is case-sensitive.
//! Use it for short public IDs, such as URL-shortener slugs. It
//! packs about 17% more per character than base36.
//!
//! ```ignore
//! use rustango::base62::{int_to_base62, base62_to_int};
//!
//! assert_eq!(int_to_base62(0), "0");
//! assert_eq!(int_to_base62(61), "z");
//! assert_eq!(int_to_base62(62), "10");
//! assert_eq!(int_to_base62(125), "21");
//!
//! // Round-trip.
//! for n in [0u64, 1, 61, 62, 1000, 1_000_000_000_000, u64::MAX] {
//!     assert_eq!(base62_to_int(&int_to_base62(n)).unwrap(), n);
//! }
//!
//! // Strict case-sensitive — "Z" (35) ≠ "z" (61).
//! assert_eq!(base62_to_int("Z").unwrap(), 35);
//! assert_eq!(base62_to_int("z").unwrap(), 61);
//! ```
//!
//! [`int_to_base62`]: crate::base62::int_to_base62
//! [`base62_to_int`]: crate::base62::base62_to_int

/// Base62 digits, `[0-9A-Za-z]`: `'0'` is 0, `'Z'` is 35, `'a'` is
/// 36, `'z'` is 61.
const ALPHABET: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// Errors from [`base62_to_int`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Base62Error {
    /// Empty string.
    #[error("base62_to_int: empty input")]
    Empty,

    /// Character outside `[0-9A-Za-z]`.
    #[error("base62_to_int: invalid character `{0}` (allowed: 0-9, A-Z, a-z)")]
    InvalidChar(char),

    /// The value does not fit in `u64`.
    #[error("base62_to_int: value overflows u64")]
    Overflow,
}

/// Encode a non-negative integer as a base62 string. Unlike base36,
/// the output is case-sensitive.
///
/// ```ignore
/// use rustango::base62::int_to_base62;
/// assert_eq!(int_to_base62(0), "0");
/// assert_eq!(int_to_base62(10), "A");
/// assert_eq!(int_to_base62(35), "Z");
/// assert_eq!(int_to_base62(36), "a");
/// assert_eq!(int_to_base62(61), "z");
/// assert_eq!(int_to_base62(62), "10");
/// ```
#[must_use]
pub fn int_to_base62(mut n: u64) -> String {
    if n == 0 {
        return "0".to_owned();
    }
    // u64::MAX needs 11 base62 digits.
    let mut buf: Vec<u8> = Vec::with_capacity(11);
    while n > 0 {
        buf.push(ALPHABET[(n % 62) as usize]);
        n /= 62;
    }
    buf.reverse();
    // SAFETY: ALPHABET is ASCII-only.
    String::from_utf8(buf).expect("base62 alphabet is ASCII")
}

/// Decode a base62 string into a `u64`. Only `[0-9A-Za-z]` is
/// allowed. Whitespace, a leading `-` and non-ASCII are rejected.
///
/// # Errors
/// * [`Base62Error::Empty`] — empty string.
/// * [`Base62Error::InvalidChar`] — character outside `[0-9A-Za-z]`.
/// * [`Base62Error::Overflow`] — value exceeds `u64::MAX`.
pub fn base62_to_int(s: &str) -> Result<u64, Base62Error> {
    if s.is_empty() {
        return Err(Base62Error::Empty);
    }
    let mut out: u64 = 0;
    for c in s.chars() {
        let digit = match c {
            '0'..='9' => (c as u32) - ('0' as u32),
            'A'..='Z' => (c as u32) - ('A' as u32) + 10,
            'a'..='z' => (c as u32) - ('a' as u32) + 36,
            _ => return Err(Base62Error::InvalidChar(c)),
        };
        out = out
            .checked_mul(62)
            .and_then(|n| n.checked_add(u64::from(digit)))
            .ok_or(Base62Error::Overflow)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------- int_to_base62 --------

    #[test]
    fn encode_zero() {
        assert_eq!(int_to_base62(0), "0");
    }

    #[test]
    fn encode_digit_letter_boundaries() {
        assert_eq!(int_to_base62(9), "9");
        assert_eq!(int_to_base62(10), "A"); // digit→uppercase
        assert_eq!(int_to_base62(35), "Z");
        assert_eq!(int_to_base62(36), "a"); // uppercase→lowercase
        assert_eq!(int_to_base62(61), "z");
    }

    #[test]
    fn encode_two_digit_boundary() {
        assert_eq!(int_to_base62(62), "10");
        assert_eq!(int_to_base62(123), "1z"); // 62 + 61
        assert_eq!(int_to_base62(124), "20");
    }

    #[test]
    fn encode_three_digit_boundary() {
        // 62^2 = 3844
        assert_eq!(int_to_base62(3844), "100");
        // 62^2 - 1 = 3843 = zz
        assert_eq!(int_to_base62(3843), "zz");
    }

    #[test]
    fn encode_u64_max_doesnt_panic() {
        let s = int_to_base62(u64::MAX);
        assert_eq!(s.len(), 11);
        assert!(s.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn encode_youtube_video_id_shape() {
        // Output stays inside the URL-safe alphabet.
        let s = int_to_base62(123_456_789);
        assert!(s.chars().all(|c| c.is_ascii_alphanumeric()));
        assert!(!s.is_empty());
    }

    // -------- base62_to_int --------

    #[test]
    fn decode_canonical() {
        assert_eq!(base62_to_int("0").unwrap(), 0);
        assert_eq!(base62_to_int("9").unwrap(), 9);
        assert_eq!(base62_to_int("A").unwrap(), 10);
        assert_eq!(base62_to_int("Z").unwrap(), 35);
        assert_eq!(base62_to_int("a").unwrap(), 36);
        assert_eq!(base62_to_int("z").unwrap(), 61);
        assert_eq!(base62_to_int("10").unwrap(), 62);
    }

    #[test]
    fn decode_case_sensitive() {
        // `Z` is 35 and `z` is 61; base36 would fold them.
        assert_ne!(base62_to_int("Z").unwrap(), base62_to_int("z").unwrap());
        assert_eq!(base62_to_int("Z").unwrap(), 35);
        assert_eq!(base62_to_int("z").unwrap(), 61);
    }

    #[test]
    fn decode_empty_errors() {
        assert_eq!(base62_to_int(""), Err(Base62Error::Empty));
    }

    #[test]
    fn decode_rejects_non_alphanumeric() {
        let err = base62_to_int("hello world").unwrap_err();
        assert!(matches!(err, Base62Error::InvalidChar(' ')));
    }

    #[test]
    fn decode_rejects_leading_dash() {
        let err = base62_to_int("-1").unwrap_err();
        assert!(matches!(err, Base62Error::InvalidChar('-')));
    }

    #[test]
    fn decode_rejects_non_ascii() {
        let err = base62_to_int("ω").unwrap_err();
        assert!(matches!(err, Base62Error::InvalidChar('ω')));
    }

    #[test]
    fn decode_overflow_surfaces_as_error() {
        // 12 'z's = 62^12 - 1 ≫ u64::MAX
        let err = base62_to_int("zzzzzzzzzzzz").unwrap_err();
        assert_eq!(err, Base62Error::Overflow);
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
            61,
            62,
            123,
            3843,
            3844,
            1_000_000,
            1_000_000_000_000,
            u64::MAX - 1,
            u64::MAX,
        ] {
            assert_eq!(base62_to_int(&int_to_base62(n)).unwrap(), n, "n = {n}");
        }
    }

    #[test]
    fn base62_shorter_than_base36_for_same_value() {
        // Small values tie; larger ones favour base62.
        let v: u64 = 1_000_000_000_000;
        let in_62 = int_to_base62(v);
        let in_36 = crate::base36::int_to_base36(v);
        assert!(
            in_62.len() <= in_36.len(),
            "base62 (`{in_62}`) should be at most as long as base36 (`{in_36}`)"
        );
    }
}
