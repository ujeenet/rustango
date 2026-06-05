//! URL codec helpers — `application/x-www-form-urlencoded` percent
//! decoder + RFC-3986 percent encoder + Django-shape urlsafe base64.
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

/// Percent-encode bytes outside the RFC 3986 *unreserved* set
/// (alphanumeric + `-` `_` `.` `~`). Used by URL-building
/// helpers that need to safely round-trip user input through a
/// query string. Does NOT encode `+` as space (that's a decoder
/// convention, not an encoder one) — encoders should leave the
/// space character as `%20`, which every browser accepts.
///
/// Mirror of the inline implementations in `template_views`'s
/// pagination URL builder. Centralizing keeps the encoder
/// table consistent across modules.
#[must_use]
pub fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Decode a `application/x-www-form-urlencoded` string.
///
/// See module docs for malformed-input handling.
#[must_use]
pub fn url_decode(s: &str) -> String {
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

// ============================================================ Django urlsafe_base64

/// Django-parity
/// [`urlsafe_base64_encode(bytes)`](https://docs.djangoproject.com/en/6.0/ref/utils/#django.utils.http.urlsafe_base64_encode) —
/// encode `bytes` as URL-safe base64 with padding stripped (per
/// `django.utils.http.urlsafe_base64_encode`). Used in
/// password-reset URL shape `/reset/<uidb64>/<token>/` to encode
/// the user PK as a URL-safe string.
///
/// Drops the standard base64 padding (`=`) so the encoded form
/// drops cleanly into a URL path or query parameter without
/// escaping.
///
/// ```ignore
/// use rustango::url_codec::urlsafe_base64_encode;
/// assert_eq!(urlsafe_base64_encode(b"foo"), "Zm9v");
/// assert_eq!(urlsafe_base64_encode(b""), "");
/// // Encodes characters that would need `%`-escape in standard b64:
/// // raw `+` → `-`, raw `/` → `_`.
/// assert_eq!(urlsafe_base64_encode(&[0xfb, 0xff]), "-_8");
/// ```
#[must_use]
pub fn urlsafe_base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Django-parity
/// [`urlsafe_base64_decode(s)`](https://docs.djangoproject.com/en/6.0/ref/utils/#django.utils.http.urlsafe_base64_decode) —
/// decode a URL-safe base64 string (padding optional) into raw
/// bytes. Both `URL_SAFE` (padded) and `URL_SAFE_NO_PAD` inputs are
/// accepted — Django re-pads internally before decoding so legacy
/// senders that include `=` still work.
///
/// # Errors
/// Returns `None` on any decode failure (invalid alphabet, bad
/// length, etc.). Django raises `binascii.Error`; rustango surfaces
/// the gap as `Option::None` so callers can ergonomically `?` it
/// out with a custom error type per call site.
///
/// ```ignore
/// use rustango::url_codec::urlsafe_base64_decode;
/// assert_eq!(urlsafe_base64_decode("Zm9v").as_deref(), Some(&b"foo"[..]));
/// // Padded input also accepted.
/// assert_eq!(urlsafe_base64_decode("Zm9v====").as_deref(), Some(&b"foo"[..]));
/// // Standard b64 reserved chars rejected.
/// assert!(urlsafe_base64_decode("a+b/c").is_none());
/// ```
#[must_use]
pub fn urlsafe_base64_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    // Strip any padding the caller threaded through — Django shape
    // accepts both forms.
    let trimmed = s.trim_end_matches('=');
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(trimmed)
        .ok()
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
        assert!(
            got.contains("%C3") || got.contains('\u{FFFD}'),
            "got: {got:?}"
        );
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
        assert!(
            got.contains('\u{FFFD}'),
            "expected replacement char, got: {got:?}"
        );
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

    // ---- url_encode ----

    #[test]
    fn url_encode_unreserved_pass_through() {
        assert_eq!(url_encode("plain"), "plain");
        assert_eq!(url_encode("foo-bar.baz_~"), "foo-bar.baz_~");
        assert_eq!(url_encode("AaZz09"), "AaZz09");
    }

    #[test]
    fn url_encode_reserved_chars_percent_encoded() {
        assert_eq!(url_encode("hello world"), "hello%20world");
        assert_eq!(url_encode("a&b=c"), "a%26b%3Dc");
        assert_eq!(url_encode("?#"), "%3F%23");
    }

    /// Round-trip: encode then decode reproduces the input. Confirms
    /// the encoder and decoder agree on the unreserved set.
    #[test]
    fn url_encode_decode_round_trip() {
        for input in [
            "plain",
            "hello world",
            "a&b=c",
            "café",    // multibyte UTF-8
            "100%off", // user input with `%`
            "x_y-z.0", // mostly-unreserved
            "?#&=+/!", // pile of reserved
        ] {
            let encoded = url_encode(input);
            let decoded = url_decode(&encoded);
            assert_eq!(decoded, input, "round-trip failed on `{input}`");
        }
    }

    // ---- urlsafe_base64 (Django parity) ----

    #[test]
    fn urlsafe_b64_encode_matches_django_examples() {
        assert_eq!(urlsafe_base64_encode(b"foo"), "Zm9v");
        assert_eq!(urlsafe_base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(urlsafe_base64_encode(b""), "");
    }

    #[test]
    fn urlsafe_b64_encode_drops_padding() {
        // 1 byte → standard b64 would emit `==` padding; urlsafe-no-pad
        // strips it.
        let encoded = urlsafe_base64_encode(b"f");
        assert_eq!(encoded, "Zg");
        assert!(!encoded.contains('='));
    }

    #[test]
    fn urlsafe_b64_encode_uses_url_safe_alphabet() {
        // 0xfb 0xff in standard b64 is `+/8=`. URL-safe is `-_8`.
        let encoded = urlsafe_base64_encode(&[0xfb, 0xff]);
        assert_eq!(encoded, "-_8");
        assert!(!encoded.contains('+'));
        assert!(!encoded.contains('/'));
    }

    #[test]
    fn urlsafe_b64_decode_simple() {
        assert_eq!(urlsafe_base64_decode("Zm9v").as_deref(), Some(&b"foo"[..]));
    }

    #[test]
    fn urlsafe_b64_decode_accepts_padding_for_django_compat() {
        // Django shape — `=` padding silently stripped so legacy senders
        // that include it still decode cleanly.
        assert_eq!(
            urlsafe_base64_decode("Zm9v====").as_deref(),
            Some(&b"foo"[..])
        );
        assert_eq!(urlsafe_base64_decode("Zg==").as_deref(), Some(&b"f"[..]));
    }

    #[test]
    fn urlsafe_b64_decode_rejects_standard_b64_chars() {
        // Standard b64 reserved chars `+` and `/` must be rejected when
        // they appear in input — URL-safe alphabet uses `-` and `_`.
        assert!(urlsafe_base64_decode("a+b/c").is_none());
    }

    #[test]
    fn urlsafe_b64_decode_rejects_garbage() {
        assert!(urlsafe_base64_decode("!@#$%").is_none());
        assert!(urlsafe_base64_decode("hello\n").is_none()); // embedded LF
    }

    #[test]
    fn urlsafe_b64_decode_empty_is_empty_vec() {
        assert_eq!(urlsafe_base64_decode("").as_deref(), Some(&[][..]));
    }

    #[test]
    fn urlsafe_b64_round_trip_for_random_bytes() {
        // Every byte value through encode → decode lands back unchanged.
        let mut input = Vec::with_capacity(256);
        for b in 0u8..=255 {
            input.push(b);
        }
        let encoded = urlsafe_base64_encode(&input);
        let decoded = urlsafe_base64_decode(&encoded).expect("round-trip");
        assert_eq!(decoded, input);
    }
}
