//! Tiny lowercase-hex codec — pure, no external deps. Lives outside
//! [`crate::crypto`] so call sites that don't compile `hmac` / `sha2`
//! (pagination cursor encoding, `row_to_json` binary columns) can
//! still share one implementation.
//!
//! Renames `crate::crypto::hex_encode` from v0.42; the older path is
//! kept as a re-export for the `hmac`-deps codepath.

/// Render `bytes` as a lowercase hex string (no separator).
#[must_use]
pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_yields_empty() {
        assert_eq!(hex_encode(&[]), "");
    }

    #[test]
    fn known_byte_vector() {
        assert_eq!(hex_encode(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
    }

    #[test]
    fn full_byte_range_is_lowercase() {
        let all: Vec<u8> = (0..=255u8).collect();
        let s = hex_encode(&all);
        assert_eq!(s.len(), 512);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!s.chars().any(|c| c.is_ascii_uppercase()));
    }
}
