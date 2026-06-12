//! WebAuthn verification primitives (#392) — pure-Rust, no C dep.
//!
//! These are the security-critical building blocks the ceremonies use:
//! parse `clientDataJSON` / `authenticatorData` / the `attestationObject`
//! CBOR, decode a COSE ES256 public key, and verify an ES256 assertion
//! signature with `p256`. All parsing is bounds-checked; all failures
//! return a [`PasskeyError`] rather than panicking.
//!
//! Scope: **ES256 (ECDSA P-256)** keys and **`none`/self attestation**
//! (the dominant passkey case). Attestation-statement certificate chains
//! are not verified — for `none` attestation there are none, and for
//! platform passkeys RPs typically trust-on-first-use anyway.

use sha2::{Digest, Sha256};

use super::error::PasskeyError;

/// `authenticatorData` flag bits (WebAuthn §6.1).
const FLAG_UP: u8 = 0b0000_0001; // User Present
const FLAG_AT: u8 = 0b0100_0000; // Attested credential data included

/// Parsed `authenticatorData`.
#[derive(Debug, Clone)]
pub struct AuthenticatorData {
    /// SHA-256 of the RP ID.
    pub rp_id_hash: [u8; 32],
    /// Raw flags byte.
    pub flags: u8,
    /// Signature counter.
    pub sign_count: u32,
    /// Present only when the `AT` flag is set (registration).
    pub attested: Option<AttestedCredentialData>,
}

impl AuthenticatorData {
    /// `User Present` bit.
    #[must_use]
    pub fn user_present(&self) -> bool {
        self.flags & FLAG_UP != 0
    }
}

/// The attested credential data embedded in registration `authData`.
#[derive(Debug, Clone)]
pub struct AttestedCredentialData {
    /// Authenticator AAGUID (16 bytes).
    pub aaguid: [u8; 16],
    /// Raw credential id.
    pub credential_id: Vec<u8>,
    /// COSE-encoded public key bytes (stored verbatim for later asserts).
    pub cose_public_key: Vec<u8>,
}

/// Parse `authenticatorData` (WebAuthn §6.1). `expect_attested` controls
/// whether the `AT` block must be present (true for registration).
///
/// # Errors
/// [`PasskeyError::AuthData`] on truncation / structural problems.
pub fn parse_authenticator_data(bytes: &[u8]) -> Result<AuthenticatorData, PasskeyError> {
    if bytes.len() < 37 {
        return Err(PasskeyError::AuthData(format!(
            "too short ({} bytes, need ≥37)",
            bytes.len()
        )));
    }
    let mut rp_id_hash = [0u8; 32];
    rp_id_hash.copy_from_slice(&bytes[0..32]);
    let flags = bytes[32];
    let sign_count = u32::from_be_bytes([bytes[33], bytes[34], bytes[35], bytes[36]]);

    let attested = if flags & FLAG_AT != 0 {
        // aaguid(16) || credIdLen(2 BE) || credId(L) || coseKey(rest, CBOR)
        if bytes.len() < 55 {
            return Err(PasskeyError::AuthData(
                "AT flag set but attested credential data truncated".into(),
            ));
        }
        let mut aaguid = [0u8; 16];
        aaguid.copy_from_slice(&bytes[37..53]);
        let cred_len = u16::from_be_bytes([bytes[53], bytes[54]]) as usize;
        let cred_start = 55;
        let cred_end = cred_start + cred_len;
        if bytes.len() < cred_end {
            return Err(PasskeyError::AuthData(format!(
                "credential id length {cred_len} exceeds buffer"
            )));
        }
        let credential_id = bytes[cred_start..cred_end].to_vec();
        // The COSE key is the remainder — a single CBOR map. We re-encode
        // exactly what we decode so the stored bytes are canonical and
        // self-describing (length-prefixed extensions, if any, are
        // dropped — we don't support extensions in this slice).
        let cose_slice = &bytes[cred_end..];
        let cose_public_key = canonicalize_cose_key(cose_slice)?;
        Some(AttestedCredentialData {
            aaguid,
            credential_id,
            cose_public_key,
        })
    } else {
        None
    };

    Ok(AuthenticatorData {
        rp_id_hash,
        flags,
        sign_count,
        attested,
    })
}

/// Decode the leading CBOR map from `bytes` and re-encode just that map,
/// so trailing extension bytes don't bloat the stored key.
fn canonicalize_cose_key(bytes: &[u8]) -> Result<Vec<u8>, PasskeyError> {
    let value: ciborium::value::Value =
        ciborium::de::from_reader(bytes).map_err(|e| PasskeyError::Cbor(e.to_string()))?;
    if !value.is_map() {
        return Err(PasskeyError::CoseKey("not a CBOR map".into()));
    }
    let mut out = Vec::new();
    ciborium::ser::into_writer(&value, &mut out).map_err(|e| PasskeyError::Cbor(e.to_string()))?;
    Ok(out)
}

/// Parsed + validated `clientDataJSON`.
pub struct ClientData {
    pub ty: String,
    /// base64url-decoded challenge bytes.
    pub challenge: Vec<u8>,
    pub origin: String,
}

/// Parse and validate `clientDataJSON` (WebAuthn §5.8.1). Verifies the
/// ceremony `type`, that the echoed `challenge` decodes to `expected_challenge`,
/// and that `origin` is in `allowed_origins`.
///
/// # Errors
/// The matching [`PasskeyError`] variant for whichever check fails.
pub fn parse_and_verify_client_data(
    json: &[u8],
    expected_type: &'static str,
    expected_challenge: &[u8],
    allowed_origins: &[String],
) -> Result<ClientData, PasskeyError> {
    let v: serde_json::Value =
        serde_json::from_slice(json).map_err(|e| PasskeyError::ClientData(e.to_string()))?;
    let ty = v
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| PasskeyError::ClientData("missing `type`".into()))?
        .to_owned();
    if ty != expected_type {
        return Err(PasskeyError::WrongType {
            expected: expected_type,
            got: ty,
        });
    }
    let challenge_b64 = v
        .get("challenge")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| PasskeyError::ClientData("missing `challenge`".into()))?;
    let challenge = b64url_decode(challenge_b64)
        .ok_or_else(|| PasskeyError::ClientData("challenge is not valid base64url".into()))?;
    // Constant-time compare — the challenge is a secret nonce.
    use subtle::ConstantTimeEq as _;
    if challenge.len() != expected_challenge.len()
        || challenge.ct_eq(expected_challenge).unwrap_u8() == 0
    {
        return Err(PasskeyError::ChallengeMismatch);
    }
    let origin = v
        .get("origin")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| PasskeyError::ClientData("missing `origin`".into()))?
        .to_owned();
    if !allowed_origins.iter().any(|o| o == &origin) {
        return Err(PasskeyError::BadOrigin(origin));
    }
    Ok(ClientData {
        ty,
        challenge,
        origin,
    })
}

/// Verify that `authenticatorData.rpIdHash == SHA-256(rp_id)`.
///
/// # Errors
/// [`PasskeyError::RpIdMismatch`].
pub fn verify_rp_id(auth_data: &AuthenticatorData, rp_id: &str) -> Result<(), PasskeyError> {
    let mut h = Sha256::new();
    h.update(rp_id.as_bytes());
    let expected: [u8; 32] = h.finalize().into();
    if auth_data.rp_id_hash == expected {
        Ok(())
    } else {
        Err(PasskeyError::RpIdMismatch)
    }
}

/// Build a `p256` verifying key from a COSE EC2/ES256 public key.
///
/// # Errors
/// [`PasskeyError::CoseKey`] if the key isn't a P-256 ES256 EC2 key or is
/// malformed.
pub fn cose_es256_key(cose: &[u8]) -> Result<p256::ecdsa::VerifyingKey, PasskeyError> {
    use ciborium::value::Value;
    let v: Value =
        ciborium::de::from_reader(cose).map_err(|e| PasskeyError::Cbor(e.to_string()))?;
    let Value::Map(entries) = v else {
        return Err(PasskeyError::CoseKey("not a CBOR map".into()));
    };
    // COSE labels: 1=kty, 3=alg, -1=crv, -2=x, -3=y.
    let int_of = |val: &Value| -> Option<i128> {
        if let Value::Integer(i) = val {
            Some((*i).into())
        } else {
            None
        }
    };
    let mut kty = None;
    let mut alg = None;
    let mut crv = None;
    let mut x: Option<Vec<u8>> = None;
    let mut y: Option<Vec<u8>> = None;
    for (k, val) in &entries {
        let Some(key) = int_of(k) else { continue };
        match key {
            1 => kty = int_of(val),
            3 => alg = int_of(val),
            -1 => crv = int_of(val),
            -2 => {
                if let Value::Bytes(b) = val {
                    x = Some(b.clone());
                }
            }
            -3 => {
                if let Value::Bytes(b) = val {
                    y = Some(b.clone());
                }
            }
            _ => {}
        }
    }
    // kty 2 = EC2, alg -7 = ES256, crv 1 = P-256.
    if kty != Some(2) {
        return Err(PasskeyError::CoseKey(format!("kty {kty:?} != EC2 (2)")));
    }
    if alg != Some(-7) {
        return Err(PasskeyError::CoseKey(format!("alg {alg:?} != ES256 (-7)")));
    }
    if crv != Some(1) {
        return Err(PasskeyError::CoseKey(format!("crv {crv:?} != P-256 (1)")));
    }
    let (x, y) = match (x, y) {
        (Some(x), Some(y)) if x.len() == 32 && y.len() == 32 => (x, y),
        _ => {
            return Err(PasskeyError::CoseKey(
                "missing/short x or y coordinate".into(),
            ))
        }
    };
    // Uncompressed SEC1 point: 0x04 || X || Y.
    let mut sec1 = Vec::with_capacity(65);
    sec1.push(0x04);
    sec1.extend_from_slice(&x);
    sec1.extend_from_slice(&y);
    p256::ecdsa::VerifyingKey::from_sec1_bytes(&sec1)
        .map_err(|e| PasskeyError::CoseKey(format!("invalid P-256 point: {e}")))
}

/// Verify an ES256 assertion signature: the authenticator signs
/// `authenticatorData || SHA-256(clientDataJSON)` (WebAuthn §6.3.3 step
/// 19). `signature` is ASN.1 DER ECDSA.
///
/// # Errors
/// [`PasskeyError::BadSignature`] on any verification failure.
pub fn verify_es256_assertion(
    cose_public_key: &[u8],
    authenticator_data: &[u8],
    client_data_json: &[u8],
    signature_der: &[u8],
) -> Result<(), PasskeyError> {
    use p256::ecdsa::signature::Verifier as _;
    let key = cose_es256_key(cose_public_key)?;
    let mut client_hash = Sha256::new();
    client_hash.update(client_data_json);
    let client_hash: [u8; 32] = client_hash.finalize().into();

    let mut signed = Vec::with_capacity(authenticator_data.len() + 32);
    signed.extend_from_slice(authenticator_data);
    signed.extend_from_slice(&client_hash);

    let sig =
        p256::ecdsa::Signature::from_der(signature_der).map_err(|_| PasskeyError::BadSignature)?;
    key.verify(&signed, &sig)
        .map_err(|_| PasskeyError::BadSignature)
}

/// Decode base64url (with or without padding). Returns `None` on invalid
/// input.
pub(super) fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.trim_end_matches('='))
        .ok()
}

/// Encode bytes as base64url, no padding.
pub(super) fn b64url_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::{signature::Signer as _, Signature, SigningKey};

    /// Build a COSE EC2/ES256 key from a p256 verifying key.
    fn cose_key_of(vk: &p256::ecdsa::VerifyingKey) -> Vec<u8> {
        use ciborium::value::{Integer, Value};
        let pt = vk.to_encoded_point(false); // uncompressed 0x04||x||y
        let x = pt.x().unwrap().to_vec();
        let y = pt.y().unwrap().to_vec();
        let map = Value::Map(vec![
            (
                Value::Integer(Integer::from(1)),
                Value::Integer(Integer::from(2)),
            ), // kty=EC2
            (
                Value::Integer(Integer::from(3)),
                Value::Integer(Integer::from(-7)),
            ), // alg=ES256
            (
                Value::Integer(Integer::from(-1)),
                Value::Integer(Integer::from(1)),
            ), // crv=P-256
            (Value::Integer(Integer::from(-2)), Value::Bytes(x)),
            (Value::Integer(Integer::from(-3)), Value::Bytes(y)),
        ]);
        let mut out = Vec::new();
        ciborium::ser::into_writer(&map, &mut out).unwrap();
        out
    }

    /// Build minimal authenticatorData: rpIdHash || flags || signCount.
    fn auth_data(rp_id: &str, flags: u8, count: u32) -> Vec<u8> {
        let mut h = Sha256::new();
        h.update(rp_id.as_bytes());
        let rp_hash: [u8; 32] = h.finalize().into();
        let mut d = Vec::with_capacity(37);
        d.extend_from_slice(&rp_hash);
        d.push(flags);
        d.extend_from_slice(&count.to_be_bytes());
        d
    }

    #[test]
    fn cose_key_round_trips_to_verifying_key() {
        let sk = SigningKey::random(&mut rand_core_os());
        let vk = *sk.verifying_key();
        let cose = cose_key_of(&vk);
        let parsed = cose_es256_key(&cose).expect("parse COSE key");
        assert_eq!(parsed.to_encoded_point(false), vk.to_encoded_point(false));
    }

    #[test]
    fn assertion_verifies_and_tamper_is_rejected() {
        let sk = SigningKey::random(&mut rand_core_os());
        let cose = cose_key_of(sk.verifying_key());

        let ad = auth_data("example.com", FLAG_UP, 1);
        let client =
            br#"{"type":"webauthn.get","challenge":"AAAA","origin":"https://example.com"}"#;
        let mut client_hash = Sha256::new();
        client_hash.update(client);
        let ch: [u8; 32] = client_hash.finalize().into();
        let mut signed = ad.clone();
        signed.extend_from_slice(&ch);
        let sig: Signature = sk.sign(&signed);
        let der = sig.to_der();

        // Good signature verifies.
        verify_es256_assertion(&cose, &ad, client, der.as_bytes()).expect("valid assertion");

        // Tampered authenticatorData (bumped count) → rejected.
        let mut tampered = ad.clone();
        tampered[36] ^= 0xff;
        assert!(matches!(
            verify_es256_assertion(&cose, &tampered, client, der.as_bytes()),
            Err(PasskeyError::BadSignature)
        ));
    }

    #[test]
    fn parse_authenticator_data_reads_header() {
        let ad = auth_data("example.com", FLAG_UP, 42);
        let parsed = parse_authenticator_data(&ad).unwrap();
        assert!(parsed.user_present());
        assert_eq!(parsed.sign_count, 42);
        assert!(parsed.attested.is_none());
        verify_rp_id(&parsed, "example.com").unwrap();
        assert!(matches!(
            verify_rp_id(&parsed, "evil.com"),
            Err(PasskeyError::RpIdMismatch)
        ));
    }

    #[test]
    fn client_data_validation() {
        let json = br#"{"type":"webauthn.get","challenge":"YWJj","origin":"https://example.com"}"#;
        let origins = vec!["https://example.com".to_owned()];
        // "YWJj" is base64url of "abc".
        let cd = parse_and_verify_client_data(json, "webauthn.get", b"abc", &origins).unwrap();
        assert_eq!(cd.origin, "https://example.com");
        // Wrong challenge.
        assert!(matches!(
            parse_and_verify_client_data(json, "webauthn.get", b"xyz", &origins),
            Err(PasskeyError::ChallengeMismatch)
        ));
        // Wrong type.
        assert!(matches!(
            parse_and_verify_client_data(json, "webauthn.create", b"abc", &origins),
            Err(PasskeyError::WrongType { .. })
        ));
        // Disallowed origin.
        assert!(matches!(
            parse_and_verify_client_data(json, "webauthn.get", b"abc", &[]),
            Err(PasskeyError::BadOrigin(_))
        ));
    }

    // p256's `SigningKey::random` wants an `OsRng`; pull it from `rand`'s
    // re-exported core to avoid a separate `rand_core` dep in tests.
    fn rand_core_os() -> p256::elliptic_curve::rand_core::OsRng {
        p256::elliptic_curve::rand_core::OsRng
    }
}
