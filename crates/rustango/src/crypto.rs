//! Shared HMAC-SHA256, SHA-256 and hex helpers.
//!
//! Every crypto helper in the crate lives here, and only here. Copies
//! of crypto code drift: one copy gets a fix, the other does not, and
//! the unfixed path stays exploitable while looking correct.
//!
//! Most helpers are `pub(crate)`. If you want raw HMAC, depend on
//! `hmac` and `sha2` directly, so the framework does not have to keep
//! a crypto API stable for you.
//!
//! The whole module is feature-gated. A build without any HMAC user
//! leaves this code out completely.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

/// Hex-encode bytes with lowercase digits. Re-exports
/// [`crate::hex::hex_encode`] so HMAC callers keep one import path.
// Its callers are feature-gated, so this is dead in some builds. Keep
// the allow: the function is live elsewhere.
#[allow(dead_code)]
#[must_use]
pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    crate::hex::hex_encode(bytes)
}

/// `SHA-256(bytes)` as a lowercase hex string. Used by the SigV4
/// canonical-request hash and the HMAC auth body hash.
#[allow(dead_code)] // see `hex_encode` above
#[must_use]
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex_encode(&h.finalize())
}

/// `HMAC-SHA256(key, data)`, returning the raw 32-byte tag. The
/// `expect` never fires: SHA-256 HMAC accepts a key of any length.
// Some builds enable this module only for `constant_time_compare` and
// so have no HMAC caller. CI runs `-D warnings`, so keep the allow.
#[allow(dead_code)]
#[must_use]
pub(crate) fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC key");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// Django-parity
/// [`django.utils.crypto.constant_time_compare(val1, val2)`](https://docs.djangoproject.com/en/6.0/ref/utils/#django.utils.crypto.constant_time_compare) —
/// compare two byte strings in time that depends on their length but
/// not on their content. Use it for HMAC tags, CSRF tokens, session
/// signatures and TOTP codes. A normal `==` returns early on the
/// first differing byte, which lets an attacker guess a secret one
/// byte at a time.
///
/// Different lengths return `false` right away, like Django. That
/// path is not constant-time, but the length of an HMAC tag or a
/// token is not a secret.
///
/// **This must stay the only such function in the crate.** It calls
/// `subtle::ConstantTimeEq` instead of a hand-written XOR loop,
/// because nothing in safe Rust stops the compiler from vectorising
/// such a loop or adding an early exit. A second copy is how a real
/// timing leak gets introduced: one copy gets the careful treatment
/// and the other quietly does not.
///
/// ```ignore
/// use rustango::crypto::constant_time_compare;
/// let expected = b"abcdef";
/// let supplied = b"abcdef";
/// assert!(constant_time_compare(expected, supplied));
/// assert!(!constant_time_compare(expected, b"abcdez"));
/// assert!(!constant_time_compare(expected, b"abc"));  // length mismatch
/// ```
#[must_use]
pub fn constant_time_compare(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    use subtle::ConstantTimeEq as _;
    a.ct_eq(b).into()
}

/// Django-parity
/// [`django.utils.crypto.salted_hmac(key_salt, value, secret=None,
/// algorithm='sha1')`](https://docs.djangoproject.com/en/6.0/ref/utils/#django.utils.crypto.salted_hmac) —
/// compute `HMAC(SHA256(secret || key_salt), value)` and return the
/// raw 32-byte tag. Django uses SHA-1 by default; this uses SHA-256.
///
/// The salt gives each purpose its own derived key from one shared
/// secret. A value signed for sessions then cannot be replayed as a
/// value signed for, say, password reset.
///
/// To put the tag on the wire, use
/// [`hex_encode`](crate::hex::hex_encode) or
/// [`crate::url_codec::urlsafe_base64_encode`].
///
/// ```ignore
/// use rustango::crypto::salted_hmac;
/// let tag = salted_hmac(b"my-purpose", b"user-id=42", b"app-secret-key");
/// assert_eq!(tag.len(), 32);
/// // Same inputs → same tag (deterministic).
/// let tag2 = salted_hmac(b"my-purpose", b"user-id=42", b"app-secret-key");
/// assert_eq!(tag, tag2);
/// // Different purpose → different tag.
/// let tag3 = salted_hmac(b"other-purpose", b"user-id=42", b"app-secret-key");
/// assert_ne!(tag, tag3);
/// ```
// see `hmac_sha256` above
#[allow(dead_code)]
#[must_use]
pub fn salted_hmac(key_salt: &[u8], value: &[u8], secret: &[u8]) -> Vec<u8> {
    // Per-purpose key: SHA256(secret || key_salt).
    let mut hasher = Sha256::new();
    hasher.update(secret);
    hasher.update(key_salt);
    let derived = hasher.finalize();
    hmac_sha256(&derived, value)
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

    // ---------------- constant_time_compare (Django parity) ----------------

    #[test]
    fn ct_compare_equal_inputs_match() {
        assert!(constant_time_compare(b"hello", b"hello"));
        assert!(constant_time_compare(b"", b""));
    }

    #[test]
    fn ct_compare_different_content_fails() {
        assert!(!constant_time_compare(b"hello", b"hellx"));
        assert!(!constant_time_compare(b"abcdef", b"abcdez"));
    }

    #[test]
    fn ct_compare_length_mismatch_fails() {
        // Django shape: length mismatch → False, no panic.
        assert!(!constant_time_compare(b"abc", b"abcd"));
        assert!(!constant_time_compare(b"abcd", b"abc"));
        assert!(!constant_time_compare(b"x", b""));
        assert!(!constant_time_compare(b"", b"x"));
    }

    #[test]
    fn ct_compare_handles_full_byte_range() {
        // Compare against all-bytes input to spot-check no UTF-8
        // assumption hides in the loop.
        let a: Vec<u8> = (0..=255).collect();
        let b: Vec<u8> = (0..=255).collect();
        assert!(constant_time_compare(&a, &b));
        let mut c = b.clone();
        c[200] ^= 0x01;
        assert!(!constant_time_compare(&a, &c));
    }

    #[test]
    fn ct_compare_first_byte_difference_returns_false() {
        // Regression: the loop must keep going past the first mismatch
        // (constant time) but still report failure.
        assert!(!constant_time_compare(b"Xbcdef", b"abcdef"));
    }

    #[test]
    fn ct_compare_last_byte_difference_returns_false() {
        assert!(!constant_time_compare(b"abcdeX", b"abcdef"));
    }

    #[test]
    fn ct_compare_real_hmac_outputs() {
        // Sanity smoke: two HMAC tags differ in exactly one position;
        // compare must catch it.
        let a = hmac_sha256(b"key", b"msg-1");
        let b = hmac_sha256(b"key", b"msg-2");
        assert!(!constant_time_compare(&a, &b));
        // And identical inputs match.
        let c = hmac_sha256(b"key", b"msg-1");
        assert!(constant_time_compare(&a, &c));
    }

    // -------- salted_hmac (Django parity) --------

    #[test]
    fn salted_hmac_returns_32_byte_tag() {
        let tag = salted_hmac(b"purpose-A", b"value", b"secret");
        assert_eq!(tag.len(), 32);
    }

    #[test]
    fn salted_hmac_is_deterministic() {
        let a = salted_hmac(b"purpose", b"value", b"secret");
        let b = salted_hmac(b"purpose", b"value", b"secret");
        assert_eq!(a, b);
    }

    #[test]
    fn salted_hmac_distinguishes_purposes() {
        // Same secret + value, different key_salt → different tag.
        // This is the whole point of the salt — purposes are isolated.
        let a = salted_hmac(b"purpose-A", b"value", b"secret");
        let b = salted_hmac(b"purpose-B", b"value", b"secret");
        assert_ne!(a, b);
    }

    #[test]
    fn salted_hmac_distinguishes_secrets() {
        let a = salted_hmac(b"purpose", b"value", b"secret-1");
        let b = salted_hmac(b"purpose", b"value", b"secret-2");
        assert_ne!(a, b);
    }

    #[test]
    fn salted_hmac_distinguishes_values() {
        let a = salted_hmac(b"purpose", b"value-1", b"secret");
        let b = salted_hmac(b"purpose", b"value-2", b"secret");
        assert_ne!(a, b);
    }

    #[test]
    fn salted_hmac_empty_inputs_dont_panic() {
        // Zero-length salt / value / secret all work — HMAC + SHA256
        // accept empty input.
        let _ = salted_hmac(b"", b"", b"");
        let _ = salted_hmac(b"salt", b"", b"secret");
        let _ = salted_hmac(b"", b"value", b"secret");
    }

    #[test]
    fn salted_hmac_handles_binary_inputs() {
        // Non-UTF8 bytes in salt / value / secret all work (it's a byte
        // primitive, not a string primitive).
        let salt: Vec<u8> = (0u8..=255).collect();
        let tag = salted_hmac(&salt, &salt, &salt);
        assert_eq!(tag.len(), 32);
    }
}
