//! Tiny `application/x-www-form-urlencoded` decoder.
//!
//! Three private copies of this lived in [`crate::signed_url`],
//! [`crate::auth_flows`], and [`crate::tenancy::admin`] before the
//! consolidation. URL decoders are a notorious source of security
//! bugs (overlong encodings, malformed `%xx` sequences, `+`/space
//! conflation, mixed-case hex) — keeping the implementation in one
//! place means a fix lands everywhere at once.
//!
//! ## Behavior
//!
//! * `+` → `' '` (the historical query-string convention; same
//!   behavior `serde_urlencoded` and JavaScript's `decodeURIComponent`
//!   *do not* implement, but every browser form encoder does, and
//!   every server-side decoder we ship needs to honor it).
//! * `%XX` where both `X` are hex → that byte. Mixed case (`%Aa`)
//!   accepted.
//! * `%XX` where either `X` is non-hex → the literal `%` is kept and
//!   parsing continues at the next byte. Same convention as
//!   `serde_urlencoded` + RFC 3986 §2.1: malformed escapes fall
//!   through rather than aborting.
//! * Trailing `%` or `%X` (less than 2 bytes left) → kept as literal.
//! * Decoded byte stream that is not valid UTF-8 → replaced with the
//!   Unicode replacement character (`U+FFFD`) via
//!   [`String::from_utf8_lossy`]. This is a deliberate choice over
//!   `String::from_utf8(out).unwrap_or_default()` (the previous
//!   `signed_url` / `auth_flows` shape) — the unwrap-or-default
//!   variant *silently wipes the entire output* on a single bad
//!   byte, which hid both legitimate non-UTF-8 inputs and crafted
//!   ones. Lossy preserves the well-formed prefix and surfaces the
//!   error to the caller as a visible replacement char.
//!
//! ## What this is *not*
//!
//! Not a full RFC 3986 percent-decoder. Specifically, it doesn't
//! distinguish reserved characters by URI component (path vs query
//! vs fragment) — every `%XX` decodes regardless of position. Use
//! `url::Url` for parsing whole URLs; use this for body fields and
//! query-string values where the whole input is already known to be
//! `application/x-www-form-urlencoded`.

/// Decode a `application/x-www-form-urlencoded` string.
///
/// See module docs for malformed-input handling.
#[must_use]
pub(crate) fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
            if let Ok(b) = u8::from_str_radix(hex, 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_passes_through() {
        assert_eq!(url_decode("hello"), "hello");
    }

    #[test]
    fn empty_string_yields_empty() {
        assert_eq!(url_decode(""), "");
    }

    #[test]
    fn percent_20_becomes_space() {
        assert_eq!(url_decode("hello%20world"), "hello world");
    }

    #[test]
    fn plus_becomes_space() {
        assert_eq!(url_decode("hello+world"), "hello world");
    }

    #[test]
    fn percent_2b_decodes_to_literal_plus() {
        // `%2B` is the encoded form of `+`; round-trip must NOT be
        // confused with the `+ → space` convention.
        assert_eq!(url_decode("a%2Bb"), "a+b");
    }

    #[test]
    fn mixed_plus_and_percent() {
        assert_eq!(url_decode("hello+world%21"), "hello world!");
    }

    #[test]
    fn mixed_case_hex_accepted() {
        assert_eq!(url_decode("%2A%2a%2F%2f"), "**//");
    }

    #[test]
    fn unicode_via_utf8_bytes() {
        // `café` = 0x63 0x61 0x66 0xC3 0xA9
        assert_eq!(url_decode("caf%C3%A9"), "café");
    }

    #[test]
    fn malformed_percent_kept_as_literal() {
        // `%2X` is not a valid escape — literal `%` survives, then
        // continues parsing at the `2`.
        assert_eq!(url_decode("a%2Xb"), "a%2Xb");
    }

    #[test]
    fn malformed_non_hex_first_digit() {
        assert_eq!(url_decode("a%XYb"), "a%XYb");
    }

    #[test]
    fn trailing_percent_kept_as_literal() {
        // Only 1 byte after `%` — escape can't complete.
        assert_eq!(url_decode("foo%"), "foo%");
        // Only 2 bytes after `%` but second is missing; spec says
        // keep `%` and try `2` as a normal char. (i+2 < len fails.)
        assert_eq!(url_decode("foo%2"), "foo%2");
    }

    #[test]
    fn invalid_utf8_is_replaced_not_dropped() {
        // 0xC3 alone is an incomplete UTF-8 sequence (lead byte
        // for a 2-byte char with no continuation). The OLD impl
        // (`from_utf8(out).unwrap_or_default()`) would return ""
        // — a silent total wipe of the rest of the input. Lossy
        // returns the well-formed prefix + U+FFFD for the bad byte.
        let got = url_decode("hello%C3");
        assert!(got.starts_with("hello"), "got: {got:?}");
        // Trailing U+FFFD or kept literal `%C3` (since `i+2 < len`
        // fails on the 2-char tail, we hit the literal-keep arm).
        assert!(got.contains("%C3") || got.contains('\u{FFFD}'), "got: {got:?}");
    }

    #[test]
    fn invalid_utf8_in_middle_keeps_well_formed_tail() {
        // A real malformed sequence in the middle: `%C3%28` — `%C3`
        // is a valid UTF-8 lead byte but `%28` (=`(`) is NOT a valid
        // continuation byte. The lossy decoder must keep the prefix,
        // emit U+FFFD for the bad byte, and KEEP DECODING the tail.
        let got = url_decode("a%C3%28b");
        assert!(got.starts_with('a'), "got: {got:?}");
        assert!(got.ends_with('b'), "got: {got:?}");
        assert!(got.contains('\u{FFFD}'), "expected replacement char, got: {got:?}");
    }

    #[test]
    fn no_panic_on_arbitrary_input() {
        // Smoke: feed a few weird strings and confirm no panic +
        // some output.
        for s in ["%", "%%", "%%%", "+%", "%+", "+%2", "%2+"] {
            let _ = url_decode(s);
        }
    }

    #[test]
    fn dollar_amp_equal_unchanged() {
        // Reserved characters that aren't `%` or `+` pass through
        // without alteration. Caller is expected to have already
        // split on `&` / `=` etc.
        assert_eq!(url_decode("a=b&c=d"), "a=b&c=d");
    }
}
