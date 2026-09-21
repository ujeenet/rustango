//! URL codec helpers: `application/x-www-form-urlencoded` decoding,
//! RFC 3986 percent encoding, and urlsafe base64.
//!
//! URL decoders are an easy place to hide a security bug, so all of it
//! lives here and a fix lands everywhere at once.
//!
//! ## Decoding rules
//!
//! * `+` becomes a space. This is the form convention, and every
//!   browser form encoder uses it.
//! * `%XX` with two hex digits becomes that byte. Mixed case is fine.
//! * A bad or truncated escape keeps the literal `%` and parsing goes
//!   on, like `serde_urlencoded` and RFC 3986 §2.1.
//! * Bytes that are not valid UTF-8 become `U+FFFD` through
//!   [`String::from_utf8_lossy`]. A single bad byte must not wipe the
//!   whole output.
//!
//! This is not a full RFC 3986 decoder: it does not treat reserved
//! characters differently per URI part. Use `url::Url` to parse a whole
//! URL; use this for form bodies and query-string values.

/// Percent-encode every byte outside the RFC 3986 unreserved set
/// (alphanumeric and `-` `_` `.` `~`).
///
/// A space becomes `%20`, never `+`: `+` is a decoder convention only.
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
    decode_escapes(s, true)
}

/// The byte encoded at `i`, when the two chars after `%` are hex.
///
/// `u8::from_str_radix` alone is not enough: it accepts a leading sign,
/// so `%+5` would decode to `0x05`. A non-hex pair must keep the
/// literal `%`, or a gate can end up guarding a different value than
/// the one the handler acts on.
fn hex_pair_at(bytes: &[u8], i: usize) -> Option<u8> {
    let pair = bytes.get(i + 1..i + 3)?;
    if !pair.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()
}

/// Shared body of the two decoders. `plus_is_space` is the only
/// difference between form semantics and path semantics.
fn decode_escapes(s: &str, plus_is_space: bool) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if let Some(b) = hex_pair_at(bytes, i) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(if plus_is_space && bytes[i] == b'+' {
            b' '
        } else {
            bytes[i]
        });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Decode one path segment: `%XX` only, `+` stays literal.
///
/// [`url_decode`] turns `+` into a space, which is right for forms and
/// wrong for a path. Routers, axum's `Path` included, decode paths this
/// way. So code that compares a decoded segment with what a handler
/// sees must use this. A gate that reads `/tags/a+b` as `a b` while the
/// handler reads `a+b` is guarding the wrong row.
///
/// ```ignore
/// use rustango::url_codec::percent_decode_path;
/// assert_eq!(percent_decode_path("%31"), "1");     // digits survive encoding
/// assert_eq!(percent_decode_path("a+b"), "a+b");   // '+' is literal here
/// assert_eq!(percent_decode_path("a%2Fb"), "a/b");
/// ```
#[must_use]
pub fn percent_decode_path(s: &str) -> String {
    decode_escapes(s, false)
}

/// Turn an IRI (RFC 3987) into a plain URI (RFC 3986) by
/// percent-encoding every byte outside the URI-safe set. Reserved
/// syntax characters such as `/`, `?`, `#` and `%` are kept, so a URI
/// the caller built stays parseable.
///
/// Same rules as the Tera `|iriencode` filter, for handler code that
/// does not go through a template.
///
/// ```ignore
/// use rustango::url_codec::iri_to_uri;
/// // Non-ASCII gets percent-encoded.
/// assert_eq!(iri_to_uri("/café"), "/caf%C3%A9");
/// // Reserved URI syntax chars pass through.
/// assert_eq!(iri_to_uri("/path?q=hello#frag"), "/path?q=hello#frag");
/// // Already percent-encoded input survives (the `%` is in the safe set).
/// assert_eq!(iri_to_uri("/already%20encoded"), "/already%20encoded");
/// ```
#[must_use]
pub fn iri_to_uri(iri: &str) -> String {
    let mut out = String::with_capacity(iri.len());
    for byte in iri.bytes() {
        // The safe set: the RFC 3986 unreserved chars, the
        // reserved syntax chars, and `%` so already-encoded input
        // round-trips.
        let safe = matches!(
            byte,
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9'
                | b'-' | b'_' | b'.' | b'~'
                | b'/' | b':' | b'?' | b'#' | b'[' | b']' | b'@'
                | b'!' | b'$' | b'&' | b'\'' | b'(' | b')'
                | b'*' | b'+' | b',' | b';' | b'=' | b'%'
        );
        if safe {
            out.push(byte as char);
        } else {
            use std::fmt::Write as _;
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// Turn a URI back into IRI form: decode the escapes that make valid
/// Unicode, and keep the URI's structure.
///
/// Inverse of [`iri_to_uri`]. Encoded reserved characters such as `/`,
/// `?`, `#`, `&` and `=` stay encoded, because decoding them would
/// change what the URI means: `%2F` inside a path segment must not
/// become a separator.
///
/// An escape run that is not valid UTF-8 stays encoded as it was, with
/// no replacement char. A `%` with no hex pair after it passes through.
///
/// ```ignore
/// use rustango::url_codec::uri_to_iri;
///
/// // Non-ASCII UTF-8 decodes back.
/// assert_eq!(uri_to_iri("/caf%C3%A9"), "/café");
///
/// // Reserved chars stay encoded (slash inside a segment).
/// assert_eq!(uri_to_iri("/a%2Fb"), "/a%2Fb");
///
/// // Space (non-reserved) decodes.
/// assert_eq!(uri_to_iri("/with%20space"), "/with space");
///
/// // Already-decoded input passes through.
/// assert_eq!(uri_to_iri("/plain/path"), "/plain/path");
///
/// // Mixed: reserved stays, unreserved decodes.
/// assert_eq!(uri_to_iri("/caf%C3%A9/a%2Fb"), "/café/a%2Fb");
/// ```
#[must_use]
pub fn uri_to_iri(uri: &str) -> String {
    let bytes = uri.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'%' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        // Collect a run of escapes and decode it as one UTF-8
        // sequence: a char like é is two bytes, so two escapes.
        let start = i;
        let mut run: Vec<u8> = Vec::with_capacity(4);
        while i + 2 < bytes.len() + 1 && i < bytes.len() && bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                break;
            }
            let h1 = (bytes[i + 1] as char).to_digit(16);
            let h2 = (bytes[i + 2] as char).to_digit(16);
            match (h1, h2) {
                (Some(a), Some(b)) => {
                    run.push((a * 16 + b) as u8);
                    i += 3;
                }
                _ => break,
            }
        }
        if run.is_empty() {
            // `%` with no hex pair after it: pass it through.
            out.push(bytes[start]);
            i = start + 1;
            continue;
        }
        match std::str::from_utf8(&run) {
            Ok(decoded) => {
                // Keep the decoded chars, except reserved ones: those
                // go back to their encoded form.
                let mut run_idx = 0;
                for ch in decoded.chars() {
                    let utf8_len = ch.len_utf8();
                    if is_uri_reserved(ch) {
                        for &byte in &run[run_idx..run_idx + utf8_len] {
                            use std::fmt::Write as _;
                            let mut buf = String::with_capacity(3);
                            let _ = write!(buf, "%{byte:02X}");
                            out.extend_from_slice(buf.as_bytes());
                        }
                    } else {
                        let mut buf = [0u8; 4];
                        let encoded = ch.encode_utf8(&mut buf);
                        out.extend_from_slice(encoded.as_bytes());
                    }
                    run_idx += utf8_len;
                }
            }
            Err(_) => {
                // Not UTF-8: keep the escapes exactly as written.
                out.extend_from_slice(&bytes[start..i]);
            }
        }
    }
    // Always valid UTF-8: every byte came either from the input or
    // from a char we decoded successfully.
    String::from_utf8(out).unwrap_or_default()
}

fn is_uri_reserved(ch: char) -> bool {
    matches!(
        ch,
        ':' | '/'
            | '?'
            | '#'
            | '['
            | ']'
            | '@'
            | '!'
            | '$'
            | '&'
            | '\''
            | '('
            | ')'
            | '*'
            | '+'
            | ','
            | ';'
            | '='
    )
}

/// Percent-encode the path part of a URI. `/` is kept, so the path
/// structure survives.
///
/// Use it when you build a path from raw segments and do not want to
/// escape the separators yourself.
///
/// Unlike [`iri_to_uri`], it also encodes `?`, `#` and `%`: the input
/// counts as a raw, unencoded path, so a literal `%` becomes `%25`.
///
/// ```ignore
/// use rustango::url_codec::escape_uri_path;
/// // Slashes preserved.
/// assert_eq!(escape_uri_path("/a/b/c"), "/a/b/c");
/// // Spaces and non-ASCII encoded.
/// assert_eq!(escape_uri_path("/a path/café"),
///            "/a%20path/caf%C3%A9");
/// // ? and # encoded (they'd break path-level parsing).
/// assert_eq!(escape_uri_path("/with?query"), "/with%3Fquery");
/// assert_eq!(escape_uri_path("/with#frag"), "/with%23frag");
/// ```
#[must_use]
pub fn escape_uri_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        // RFC 3986 pchar set plus `/` for the separators. `?`, `#`
        // and `%` are left out: they need encoding inside a path.
        let safe = matches!(
            byte,
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9'
                | b'-' | b'_' | b'.' | b'~'
                | b'/' | b':' | b'@'
                | b'!' | b'$' | b'&' | b'\'' | b'(' | b')'
                | b'*' | b'+' | b',' | b';' | b'='
        );
        if safe {
            out.push(byte as char);
        } else {
            use std::fmt::Write as _;
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// Turn a filesystem path into a URI path. Windows `\` separators
/// become `/`.
///
/// Safe set: alphanumeric plus `-` `_` `.` `~` `/` `!` `*` `(` `)` `'`.
/// Everything else is percent-encoded, spaces and non-ASCII included.
///
/// Use this for static-file URLs built from on-disk paths. Use
/// [`escape_uri_path`] to put arbitrary text into a path segment; it
/// keeps `:` and `@`, which this one encodes.
///
/// ```ignore
/// use rustango::url_codec::filepath_to_uri;
///
/// // Plain paths pass through.
/// assert_eq!(filepath_to_uri("/static/css/main.css"), "/static/css/main.css");
///
/// // Spaces encode.
/// assert_eq!(filepath_to_uri("/static/My File.png"), "/static/My%20File.png");
///
/// // Non-ASCII encodes as UTF-8 bytes.
/// assert_eq!(filepath_to_uri("/café/menu.html"), "/caf%C3%A9/menu.html");
///
/// // Windows-style backslash normalizes to forward slash.
/// assert_eq!(filepath_to_uri("C:\\static\\app.js"), "C%3A/static/app.js");
///
/// // Safe-set chars stay verbatim.
/// assert_eq!(filepath_to_uri("/a~b!c(d)e'f*g"), "/a~b!c(d)e'f*g");
///
/// // ? and # encoded (URL-syntactic).
/// assert_eq!(filepath_to_uri("/x?y#z"), "/x%3Fy%23z");
/// ```
#[must_use]
pub fn filepath_to_uri(path: &str) -> String {
    // Windows to POSIX separators.
    let normalized = path.replace('\\', "/");
    let mut out = String::with_capacity(normalized.len());
    for byte in normalized.bytes() {
        // The conventional safe set, plus `/~!*()'`.
        let safe = matches!(
            byte,
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9'
                | b'-' | b'_' | b'.' | b'~'
                | b'/' | b'!' | b'*' | b'(' | b')' | b'\''
        );
        if safe {
            out.push(byte as char);
        } else {
            use std::fmt::Write as _;
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

// ============================================================ urlsafe_base64

/// Base64-encode `bytes` with the URL-safe alphabet and the `=`
/// padding stripped, so
/// the result drops into a URL path or query value with no escaping.
/// Used for the `uidb64` part of `/reset/<uidb64>/<token>/`.
///
/// ```ignore
/// use rustango::url_codec::urlsafe_base64_encode;
/// assert_eq!(urlsafe_base64_encode(b"foo"), "Zm9v");
/// assert_eq!(urlsafe_base64_encode(b""), "");
/// // Encodes characters that would need `%`-escape in standard b64:
/// // raw `+` → `-`, raw `/` → `_`.
/// assert_eq!(urlsafe_base64_encode(&[0xfb, 0xff]), "-_8");
/// ```
///
/// Gated on `_base64`: only the two base64 helpers need an optional crate, so
/// the rest of the module stays available in every feature set.
#[cfg(feature = "_base64")]
#[must_use]
pub fn urlsafe_base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Decode a URL-safe base64 string into raw bytes. Padding is
/// optional, so senders that include `=` still work.
///
/// # Errors
/// Returns `None` on any decode failure, such as a character outside
/// the URL-safe alphabet or a bad length. Callers map that to their
/// own error type.
///
/// ```ignore
/// use rustango::url_codec::urlsafe_base64_decode;
/// assert_eq!(urlsafe_base64_decode("Zm9v").as_deref(), Some(&b"foo"[..]));
/// // Padded input also accepted.
/// assert_eq!(urlsafe_base64_decode("Zm9v====").as_deref(), Some(&b"foo"[..]));
/// // Standard b64 reserved chars rejected.
/// assert!(urlsafe_base64_decode("a+b/c").is_none());
/// ```
#[cfg(feature = "_base64")]
#[must_use]
pub fn urlsafe_base64_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    // Drop any padding the caller sent; both forms are accepted.
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
        // `%2B` is the encoded `+`, not the `+ means space` rule.
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
        // `%2X` is not an escape: keep `%`, go on from the `2`.
        assert_eq!(url_decode("a%2Xb"), "a%2Xb");
    }

    #[test]
    fn malformed_non_hex_first_digit() {
        assert_eq!(url_decode("a%XYb"), "a%XYb");
    }

    /// A signed hex pair is not a valid escape.
    ///
    /// `u8::from_str_radix` accepts a leading sign, so `%+5` once
    /// decoded to `0x05`. The media gate then read `/tags/%+5/media`
    /// as the slug `"\u{5}"` while the handler queried the literal
    /// `"%+5"`, so the gate guarded a row the handler never touched.
    #[test]
    fn a_signed_hex_pair_is_not_an_escape() {
        // Path form: every byte survives, like axum's `Path` gives.
        for raw in ["a%+5b", "a%-5b", "a%+Ab"] {
            assert_eq!(percent_decode_path(raw), raw, "{raw} was decoded");
        }
        // Query form: the `%` is literal and `+` is a space. Either
        // way, no `0x05` byte comes out.
        assert_eq!(url_decode("a%+5b"), "a% 5b");
        assert_eq!(url_decode("a%-5b"), "a%-5b");
        for decoded in [url_decode("a%+5b"), percent_decode_path("a%+5b")] {
            assert!(
                !decoded.contains('\u{5}'),
                "a signed hex pair produced a control byte: {decoded:?}"
            );
        }
        // The encoded `+` still decodes.
        assert_eq!(percent_decode_path("a%2Bb"), "a+b");
    }

    /// Path semantics: `%XX` decodes, `+` stays literal.
    #[test]
    fn percent_decode_path_leaves_plus_alone() {
        assert_eq!(percent_decode_path("a+b"), "a+b");
        assert_eq!(url_decode("a+b"), "a b");
        assert_eq!(percent_decode_path("%31"), "1");
        assert_eq!(percent_decode_path("a%2Fb"), "a/b");
        // Short escapes stay literal in both.
        assert_eq!(percent_decode_path("foo%"), "foo%");
        assert_eq!(percent_decode_path("foo%4"), "foo%4");
    }

    #[test]
    fn trailing_percent_kept_as_literal() {
        // Only one byte after `%`, so the escape cannot complete.
        assert_eq!(url_decode("foo%"), "foo%");
        // Second hex digit missing: keep `%`, read `2` as a char.
        assert_eq!(url_decode("foo%2"), "foo%2");
    }

    #[test]
    fn invalid_utf8_is_replaced_not_dropped() {
        // 0xC3 alone is an incomplete UTF-8 sequence. Lossy decoding
        // keeps the good prefix instead of wiping the whole string.
        let got = url_decode("hello%C3");
        assert!(got.starts_with("hello"), "got: {got:?}");
        // Either a trailing U+FFFD or the literal `%C3`.
        assert!(
            got.contains("%C3") || got.contains('\u{FFFD}'),
            "got: {got:?}"
        );
    }

    #[test]
    fn invalid_utf8_in_middle_keeps_well_formed_tail() {
        // `%C3` is a UTF-8 lead byte but `%28` is not a valid
        // continuation. Keep the prefix, emit U+FFFD, decode the rest.
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
        // Smoke test: odd input must not panic.
        for s in ["%", "%%", "%%%", "+%", "%+", "+%2", "%2+"] {
            let _ = url_decode(s);
        }
    }

    #[test]
    fn dollar_amp_equal_unchanged() {
        // Reserved chars other than `%` and `+` pass through; the
        // caller splits on `&` and `=` first.
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

    /// Encode then decode gives back the input, so both sides agree
    /// on the unreserved set.
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

    // ---- urlsafe_base64 ----
    //
    // Gated like the functions they cover; the rest of the suite runs
    // in every feature set.

    #[cfg(feature = "_base64")]
    #[test]
    fn urlsafe_b64_encode_known_vectors() {
        assert_eq!(urlsafe_base64_encode(b"foo"), "Zm9v");
        assert_eq!(urlsafe_base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(urlsafe_base64_encode(b""), "");
    }

    #[cfg(feature = "_base64")]
    #[test]
    fn urlsafe_b64_encode_drops_padding() {
        // One byte would need `==` padding in standard base64.
        let encoded = urlsafe_base64_encode(b"f");
        assert_eq!(encoded, "Zg");
        assert!(!encoded.contains('='));
    }

    #[cfg(feature = "_base64")]
    #[test]
    fn urlsafe_b64_encode_uses_url_safe_alphabet() {
        // 0xfb 0xff in standard b64 is `+/8=`. URL-safe is `-_8`.
        let encoded = urlsafe_base64_encode(&[0xfb, 0xff]);
        assert_eq!(encoded, "-_8");
        assert!(!encoded.contains('+'));
        assert!(!encoded.contains('/'));
    }

    #[cfg(feature = "_base64")]
    #[test]
    fn urlsafe_b64_decode_simple() {
        assert_eq!(urlsafe_base64_decode("Zm9v").as_deref(), Some(&b"foo"[..]));
    }

    #[cfg(feature = "_base64")]
    #[test]
    fn urlsafe_b64_decode_accepts_optional_padding() {
        // `=` padding is stripped, so senders that add it still work.
        assert_eq!(
            urlsafe_base64_decode("Zm9v====").as_deref(),
            Some(&b"foo"[..])
        );
        assert_eq!(urlsafe_base64_decode("Zg==").as_deref(), Some(&b"f"[..]));
    }

    #[cfg(feature = "_base64")]
    #[test]
    fn urlsafe_b64_decode_rejects_standard_b64_chars() {
        // The URL-safe alphabet uses `-` and `_`, so `+` and `/` are
        // rejected.
        assert!(urlsafe_base64_decode("a+b/c").is_none());
    }

    #[cfg(feature = "_base64")]
    #[test]
    fn urlsafe_b64_decode_rejects_garbage() {
        assert!(urlsafe_base64_decode("!@#$%").is_none());
        assert!(urlsafe_base64_decode("hello\n").is_none()); // embedded LF
    }

    #[cfg(feature = "_base64")]
    #[test]
    fn urlsafe_b64_decode_empty_is_empty_vec() {
        assert_eq!(urlsafe_base64_decode("").as_deref(), Some(&[][..]));
    }

    // ---- iri_to_uri ----

    #[test]
    fn iri_to_uri_ascii_passes_through() {
        assert_eq!(iri_to_uri("/path/here"), "/path/here");
        assert_eq!(iri_to_uri("plain-text_value.1~"), "plain-text_value.1~");
    }

    #[test]
    fn iri_to_uri_encodes_non_ascii_utf8() {
        // The `é` is two UTF-8 bytes, encoded one at a time.
        assert_eq!(iri_to_uri("/café"), "/caf%C3%A9");
    }

    #[test]
    fn iri_to_uri_preserves_reserved_syntax_chars() {
        // A URI with a query and a fragment must survive: these
        // chars carry syntax.
        assert_eq!(
            iri_to_uri("/path?q=hello&page=1#frag"),
            "/path?q=hello&page=1#frag"
        );
        assert_eq!(
            iri_to_uri("scheme://user@host:8080/p"),
            "scheme://user@host:8080/p"
        );
    }

    #[test]
    fn iri_to_uri_preserves_existing_percent_encoded() {
        // `%` is in the safe set so already-encoded input round-trips.
        assert_eq!(iri_to_uri("/already%20encoded"), "/already%20encoded");
    }

    #[test]
    fn iri_to_uri_encodes_space_and_control_chars() {
        // Spaces and control chars both encode.
        assert_eq!(iri_to_uri("a b"), "a%20b");
        assert_eq!(iri_to_uri("a\nb"), "a%0Ab");
    }

    #[test]
    fn iri_to_uri_handles_full_unicode_range() {
        // U+1F600 is four UTF-8 bytes.
        let out = iri_to_uri("/😀");
        assert_eq!(out, "/%F0%9F%98%80");
    }

    #[test]
    fn iri_to_uri_empty_is_empty() {
        assert_eq!(iri_to_uri(""), "");
    }

    // ---- escape_uri_path ----

    #[test]
    fn escape_uri_path_preserves_slashes() {
        assert_eq!(escape_uri_path("/a/b/c"), "/a/b/c");
    }

    #[test]
    fn escape_uri_path_encodes_spaces() {
        assert_eq!(escape_uri_path("/a path"), "/a%20path");
    }

    #[test]
    fn escape_uri_path_encodes_non_ascii() {
        assert_eq!(escape_uri_path("/café"), "/caf%C3%A9");
    }

    #[test]
    fn escape_uri_path_encodes_query_and_fragment_chars() {
        // `?` and `#` would break path parsing, so they encode.
        assert_eq!(escape_uri_path("/with?query"), "/with%3Fquery");
        assert_eq!(escape_uri_path("/with#frag"), "/with%23frag");
    }

    #[test]
    fn escape_uri_path_encodes_percent_sign() {
        // Unlike iri_to_uri, a raw `%` is data, not an escape marker.
        assert_eq!(escape_uri_path("/100%"), "/100%25");
    }

    #[test]
    fn escape_uri_path_preserves_sub_delims_and_colon_at() {
        // RFC 3986 pchar set: legal inside a path segment.
        assert_eq!(escape_uri_path("/a:b@c"), "/a:b@c");
        assert_eq!(escape_uri_path("/a!b$c&d'e(f)g"), "/a!b$c&d'e(f)g");
    }

    #[test]
    fn escape_uri_path_empty() {
        assert_eq!(escape_uri_path(""), "");
    }

    #[cfg(feature = "_base64")]
    #[test]
    fn urlsafe_b64_round_trip_for_random_bytes() {
        // Every byte value survives encode then decode.
        let mut input = Vec::with_capacity(256);
        for b in 0u8..=255 {
            input.push(b);
        }
        let encoded = urlsafe_base64_encode(&input);
        let decoded = urlsafe_base64_decode(&encoded).expect("round-trip");
        assert_eq!(decoded, input);
    }

    // ---- uri_to_iri ----

    #[test]
    fn uri_to_iri_decodes_non_ascii_utf8() {
        assert_eq!(uri_to_iri("/caf%C3%A9"), "/café");
        assert_eq!(uri_to_iri("/%E4%B8%AD%E6%96%87"), "/中文");
    }

    #[test]
    fn uri_to_iri_keeps_reserved_chars_encoded() {
        // Decoding a slash inside a segment would change the path.
        assert_eq!(uri_to_iri("/a%2Fb"), "/a%2Fb");
        // `?`, `#`, `&` and `=` are reserved too.
        assert_eq!(uri_to_iri("/q%3Fk%3Dv%26"), "/q%3Fk%3Dv%26");
    }

    #[test]
    fn uri_to_iri_decodes_non_reserved_ascii() {
        // A space is not reserved, so it decodes.
        assert_eq!(uri_to_iri("/with%20space"), "/with space");
        // Same for an underscore.
        assert_eq!(uri_to_iri("/foo%5Fbar"), "/foo_bar");
    }

    #[test]
    fn uri_to_iri_passes_already_decoded_through() {
        assert_eq!(uri_to_iri("/plain/path"), "/plain/path");
        assert_eq!(uri_to_iri(""), "");
    }

    #[test]
    fn uri_to_iri_mixed_reserved_and_unicode() {
        assert_eq!(uri_to_iri("/caf%C3%A9/a%2Fb"), "/café/a%2Fb");
    }

    #[test]
    fn uri_to_iri_invalid_utf8_stays_encoded() {
        // 0xFF alone is not valid UTF-8, so it stays encoded.
        assert_eq!(uri_to_iri("/x%FFy"), "/x%FFy");
    }

    #[test]
    fn uri_to_iri_malformed_percent_passes_through() {
        // `%` followed by non-hex.
        assert_eq!(uri_to_iri("100%off"), "100%off");
        // Bare `%` at end.
        assert_eq!(uri_to_iri("x%"), "x%");
    }

    #[test]
    fn iri_to_uri_then_uri_to_iri_round_trip_for_unicode() {
        // A pure Unicode path round-trips with no loss.
        let original = "/café";
        let encoded = iri_to_uri(original);
        let decoded = uri_to_iri(&encoded);
        assert_eq!(decoded, original);
    }

    // ---- filepath_to_uri ----

    #[test]
    fn filepath_to_uri_plain_path_passes_through() {
        assert_eq!(
            filepath_to_uri("/static/css/main.css"),
            "/static/css/main.css"
        );
        assert_eq!(filepath_to_uri(""), "");
    }

    #[test]
    fn filepath_to_uri_encodes_spaces() {
        assert_eq!(
            filepath_to_uri("/static/My File.png"),
            "/static/My%20File.png"
        );
    }

    #[test]
    fn filepath_to_uri_encodes_non_ascii() {
        assert_eq!(filepath_to_uri("/café/menu.html"), "/caf%C3%A9/menu.html");
    }

    #[test]
    fn filepath_to_uri_normalizes_backslash_to_forward_slash() {
        assert_eq!(filepath_to_uri("C:\\static\\app.js"), "C%3A/static/app.js");
        assert_eq!(filepath_to_uri("a\\b\\c"), "a/b/c");
    }

    #[test]
    fn filepath_to_uri_keeps_safe_set_chars() {
        // The safe set: alphanumeric, `-_.~` and `/~!*()'`.
        assert_eq!(filepath_to_uri("/a~b!c(d)e'f*g"), "/a~b!c(d)e'f*g");
        assert_eq!(filepath_to_uri("a-b_c.d"), "a-b_c.d");
    }

    #[test]
    fn filepath_to_uri_encodes_url_syntactic_chars() {
        // `?`, `#`, `:`, `[` and `]` are not in the safe set.
        assert_eq!(filepath_to_uri("/x?y#z"), "/x%3Fy%23z");
        assert_eq!(filepath_to_uri("/[bracket]"), "/%5Bbracket%5D");
        assert_eq!(filepath_to_uri("a:b"), "a%3Ab");
        assert_eq!(filepath_to_uri("a&b"), "a%26b");
    }

    #[test]
    fn filepath_to_uri_distinct_from_escape_uri_path_on_colon() {
        // `escape_uri_path` keeps `:` and `@`; `filepath_to_uri`
        // encodes them, because a Windows drive letter must encode.
        assert_eq!(filepath_to_uri("a:b"), "a%3Ab");
        assert_eq!(escape_uri_path("a:b"), "a:b");
        // The same split applies to `&` and `=`.
        assert_eq!(filepath_to_uri("a&b=c"), "a%26b%3Dc");
        assert_eq!(escape_uri_path("a&b=c"), "a&b=c");
    }
}
