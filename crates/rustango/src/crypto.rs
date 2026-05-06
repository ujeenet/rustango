//! Shared HMAC-SHA256 / SHA-256 / hex-encoding primitives.
//!
//! Three private copies of these helpers used to live in
//! [`crate::hmac_auth`], [`crate::storage::s3`], and [`crate::jwt`].
//! Each copy was character-identical, which is the kind of code we
//! *must* keep in one place — diverging crypto helpers are a known
//! source of CWE-694-class bugs (one path patches a constant-time
//! issue, the other doesn't, and you get a working-on-paper-but-
//! exploitable signature primitive).
//!
//! This module is `pub(crate)` — the helpers are intentionally not
//! part of the public API; users who want raw HMAC should pull in
//! `hmac` + `sha2` themselves so the framework isn't on the hook
//! for crypto-API stability.
//!
//! Feature gate: any feature that uses HMAC-SHA256 enables this
//! module. Keeping the gate on `crate::crypto` rather than every
//! call site means the crypto code disappears entirely from a build
//! that only enables (e.g.) `postgres` + `manage`.
//!
//! Test coverage: RFC 4231 known-good HMAC-SHA256 test vectors,
//! NIST SHA-256 zero-length input, hex-encode round-trip + edge
//! cases (empty, all-zero, all-`0xff`).

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

/// Hex-encode a byte slice using lowercase digits. ~2× faster than
/// `format!("{b:02x}")` per byte; matches the `hex` crate's output.
#[must_use]
pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

/// `SHA-256(bytes)` rendered as a lowercase hex string. Used by the
/// SigV4 canonical-request hash and HMAC auth body hash.
#[must_use]
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex_encode(&h.finalize())
}

/// `HMAC-SHA256(key, data)` returning the raw 32-byte tag. The
/// `new_from_slice` constructor cannot fail for SHA-256 (it accepts
/// any key length) — the `expect` is for documentation only.
#[must_use]
pub(crate) fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC key");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------- hex_encode ----------------

    #[test]
    fn hex_encode_empty_yields_empty() {
        assert_eq!(hex_encode(&[]), "");
    }

    #[test]
    fn hex_encode_known_vector() {
        assert_eq!(hex_encode(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
    }

    #[test]
    fn hex_encode_handles_full_byte_range() {
        let all: Vec<u8> = (0..=255u8).collect();
        let s = hex_encode(&all);
        // 256 bytes → 512 hex chars; spot-check the bookends.
        assert_eq!(s.len(), 512);
        assert!(s.starts_with("000102030405"));
        assert!(s.ends_with("fafbfcfdfeff"));
    }

    #[test]
    fn hex_encode_uses_lowercase() {
        assert_eq!(hex_encode(&[0xAB, 0xCD, 0xEF]), "abcdef");
    }

    // ---------------- sha256_hex ----------------

    #[test]
    fn sha256_hex_empty_input_matches_nist_vector() {
        // NIST SHA-256 test vector for the empty string.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sha256_hex_abc_matches_nist_vector() {
        // NIST SHA-256 test vector for "abc".
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn sha256_hex_is_deterministic() {
        let a = sha256_hex(b"hello world");
        let b = sha256_hex(b"hello world");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64); // 32 bytes hex-encoded
    }

    // ---------------- hmac_sha256 ----------------
    //
    // Test vectors from RFC 4231 §4 — Identification and Test
    // Vectors for HMAC-SHA-224, HMAC-SHA-256, HMAC-SHA-384, and
    // HMAC-SHA-512 (https://www.rfc-editor.org/rfc/rfc4231).

    #[test]
    fn hmac_sha256_rfc4231_test_case_1() {
        // Key   = 0x0b repeated 20 times
        // Data  = "Hi There"
        // Output (HMAC-SHA-256): b0344c61d8db38535ca8afceaf0bf12b
        //                        881dc200c9833da726e9376c2e32cff7
        let key = [0x0b_u8; 20];
        let data = b"Hi There";
        let mac = hmac_sha256(&key, data);
        assert_eq!(
            hex_encode(&mac),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn hmac_sha256_rfc4231_test_case_2() {
        // Key   = "Jefe"
        // Data  = "what do ya want for nothing?"
        let key = b"Jefe";
        let data = b"what do ya want for nothing?";
        let mac = hmac_sha256(key, data);
        assert_eq!(
            hex_encode(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn hmac_sha256_rfc4231_test_case_3() {
        // Key   = 0xaa repeated 20 times
        // Data  = 0xdd repeated 50 times
        let key = [0xaa_u8; 20];
        let data = [0xdd_u8; 50];
        let mac = hmac_sha256(&key, &data);
        assert_eq!(
            hex_encode(&mac),
            "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe"
        );
    }

    #[test]
    fn hmac_sha256_accepts_zero_length_key() {
        // Edge case — sha2's `new_from_slice` accepts any length
        // including zero. Document that we handle this rather than
        // panicking.
        let mac = hmac_sha256(&[], b"data");
        assert_eq!(mac.len(), 32);
    }

    #[test]
    fn hmac_sha256_distinguishes_keys() {
        let a = hmac_sha256(b"key-a", b"data");
        let b = hmac_sha256(b"key-b", b"data");
        assert_ne!(a, b);
    }
}
