//! WebAuthn registration + authentication ceremonies (#392).
//!
//! These orchestrate the [`super::verify`] primitives into the two
//! server-side verification steps, plus the challenge + options helpers
//! the browser's `navigator.credentials.create()/get()` need. The crypto
//! is pure-Rust (`p256` / `ciborium`); see the module docs.
//!
//! Scope: **ES256** keys, **`none`/self attestation** (no cert-chain
//! verification). The caller is responsible for storing the per-ceremony
//! `challenge` (e.g. in the session) between the `*_options` and `verify_*`
//! calls, and for persisting the credential via [`super::register`] /
//! advancing the counter via [`super::update_sign_count`].

use super::error::PasskeyError;
use super::verify;

/// A fresh 32-byte ceremony challenge. Stash it (session) between issuing
/// the options and verifying the client's response.
#[must_use]
pub fn generate_challenge() -> Vec<u8> {
    use rand::RngCore as _;
    let mut buf = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    buf.to_vec()
}

/// What a verified registration yields — store these via
/// [`super::register`].
#[derive(Debug, Clone)]
pub struct RegistrationOutcome {
    /// base64url (no pad) of the raw credential id.
    pub credential_id: String,
    /// COSE-encoded public key bytes.
    pub cose_public_key: Vec<u8>,
    /// The authenticator's initial signature counter.
    pub sign_count: i64,
}

/// Verify a registration response (WebAuthn §7.1, `none`/self attestation).
///
/// `challenge` is the value handed to [`registration_options_json`];
/// `client_data_json` + `attestation_object` are the decoded
/// `response.clientDataJSON` / `response.attestationObject` from the
/// browser.
///
/// # Errors
/// The matching [`PasskeyError`] for whichever check fails.
pub fn verify_registration(
    challenge: &[u8],
    rp_id: &str,
    allowed_origins: &[String],
    client_data_json: &[u8],
    attestation_object: &[u8],
) -> Result<RegistrationOutcome, PasskeyError> {
    // 1. clientDataJSON: type / challenge / origin.
    verify::parse_and_verify_client_data(
        client_data_json,
        "webauthn.create",
        challenge,
        allowed_origins,
    )?;

    // 2. attestationObject CBOR → authData byte string.
    let value: ciborium::value::Value = ciborium::de::from_reader(attestation_object)
        .map_err(|e| PasskeyError::Cbor(e.to_string()))?;
    let auth_data_bytes = extract_auth_data(&value)?;

    // 3. authData: rpIdHash, UP flag, attested credential data.
    let ad = verify::parse_authenticator_data(&auth_data_bytes)?;
    verify::verify_rp_id(&ad, rp_id)?;
    if !ad.user_present() {
        return Err(PasskeyError::UserNotPresent);
    }
    let attested = ad.attested.ok_or_else(|| {
        PasskeyError::AuthData("registration authData has no attested credential data".into())
    })?;
    // `none`/self attestation: trust the embedded key (no cert chain).
    // The COSE key was already validated as a parseable P-256 ES256 key
    // by `parse_authenticator_data` → `canonicalize_cose_key`; confirm it
    // builds a verifying key so we never store an unusable credential.
    verify::cose_es256_key(&attested.cose_public_key)?;

    Ok(RegistrationOutcome {
        credential_id: verify::b64url_encode(&attested.credential_id),
        cose_public_key: attested.cose_public_key,
        sign_count: i64::from(ad.sign_count),
    })
}

/// Verify an authentication (assertion) response (WebAuthn §7.2).
/// Returns the new signature counter to persist via
/// [`super::update_sign_count`].
///
/// # Errors
/// The matching [`PasskeyError`] for whichever check fails (including
/// [`PasskeyError::CounterRegression`] on a non-advancing counter).
#[allow(clippy::too_many_arguments)]
pub fn verify_authentication(
    challenge: &[u8],
    rp_id: &str,
    allowed_origins: &[String],
    stored_cose_public_key: &[u8],
    stored_sign_count: i64,
    client_data_json: &[u8],
    authenticator_data: &[u8],
    signature_der: &[u8],
) -> Result<i64, PasskeyError> {
    verify::parse_and_verify_client_data(
        client_data_json,
        "webauthn.get",
        challenge,
        allowed_origins,
    )?;
    let ad = verify::parse_authenticator_data(authenticator_data)?;
    verify::verify_rp_id(&ad, rp_id)?;
    if !ad.user_present() {
        return Err(PasskeyError::UserNotPresent);
    }
    verify::verify_es256_assertion(
        stored_cose_public_key,
        authenticator_data,
        client_data_json,
        signature_der,
    )?;
    // Clone/replay detection: a non-zero counter must strictly advance.
    let new = i64::from(ad.sign_count);
    if new != 0 && stored_sign_count != 0 && new <= stored_sign_count {
        return Err(PasskeyError::CounterRegression);
    }
    Ok(new)
}

/// `PublicKeyCredentialCreationOptions` JSON for
/// `navigator.credentials.create({ publicKey })`. `user_id` is encoded
/// base64url; `exclude` lists the user's existing credential ids (their
/// stored base64url form) so the authenticator won't double-register.
#[must_use]
pub fn registration_options_json(
    rp_id: &str,
    rp_name: &str,
    user_id: &[u8],
    user_name: &str,
    challenge: &[u8],
    exclude_credential_ids: &[String],
) -> serde_json::Value {
    let exclude: Vec<serde_json::Value> = exclude_credential_ids
        .iter()
        .map(|id| serde_json::json!({ "type": "public-key", "id": id }))
        .collect();
    serde_json::json!({
        "challenge": verify::b64url_encode(challenge),
        "rp": { "id": rp_id, "name": rp_name },
        "user": {
            "id": verify::b64url_encode(user_id),
            "name": user_name,
            "displayName": user_name,
        },
        // -7 = ES256 (the only algorithm this slice verifies).
        "pubKeyCredParams": [ { "type": "public-key", "alg": -7 } ],
        "timeout": 60000,
        "attestation": "none",
        "excludeCredentials": exclude,
        "authenticatorSelection": { "userVerification": "preferred", "residentKey": "preferred" },
    })
}

/// `PublicKeyCredentialRequestOptions` JSON for
/// `navigator.credentials.get({ publicKey })`. `allow_credential_ids` are
/// the user's stored base64url credential ids.
#[must_use]
pub fn authentication_options_json(
    rp_id: &str,
    challenge: &[u8],
    allow_credential_ids: &[String],
) -> serde_json::Value {
    let allow: Vec<serde_json::Value> = allow_credential_ids
        .iter()
        .map(|id| serde_json::json!({ "type": "public-key", "id": id }))
        .collect();
    serde_json::json!({
        "challenge": verify::b64url_encode(challenge),
        "rpId": rp_id,
        "timeout": 60000,
        "userVerification": "preferred",
        "allowCredentials": allow,
    })
}

/// Pull the `authData` byte string out of an attestationObject CBOR map.
fn extract_auth_data(value: &ciborium::value::Value) -> Result<Vec<u8>, PasskeyError> {
    use ciborium::value::Value;
    let Value::Map(entries) = value else {
        return Err(PasskeyError::AuthData(
            "attestationObject is not a CBOR map".into(),
        ));
    };
    for (k, v) in entries {
        if let Value::Text(key) = k {
            if key == "authData" {
                if let Value::Bytes(b) = v {
                    return Ok(b.clone());
                }
                return Err(PasskeyError::AuthData(
                    "authData is not a byte string".into(),
                ));
            }
        }
    }
    Err(PasskeyError::AuthData(
        "attestationObject has no authData".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ciborium::value::{Integer, Value};
    use p256::ecdsa::{signature::Signer as _, Signature, SigningKey};
    use sha2::{Digest, Sha256};

    fn os_rng() -> p256::elliptic_curve::rand_core::OsRng {
        p256::elliptic_curve::rand_core::OsRng
    }

    fn cose_key_of(vk: &p256::ecdsa::VerifyingKey) -> Vec<u8> {
        let pt = vk.to_encoded_point(false);
        let map = Value::Map(vec![
            (
                Value::Integer(Integer::from(1)),
                Value::Integer(Integer::from(2)),
            ),
            (
                Value::Integer(Integer::from(3)),
                Value::Integer(Integer::from(-7)),
            ),
            (
                Value::Integer(Integer::from(-1)),
                Value::Integer(Integer::from(1)),
            ),
            (
                Value::Integer(Integer::from(-2)),
                Value::Bytes(pt.x().unwrap().to_vec()),
            ),
            (
                Value::Integer(Integer::from(-3)),
                Value::Bytes(pt.y().unwrap().to_vec()),
            ),
        ]);
        let mut out = Vec::new();
        ciborium::ser::into_writer(&map, &mut out).unwrap();
        out
    }

    fn rp_hash(rp_id: &str) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(rp_id.as_bytes());
        h.finalize().into()
    }

    /// Build registration authData with attested credential data.
    fn reg_auth_data(rp_id: &str, cred_id: &[u8], cose: &[u8], count: u32) -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(&rp_hash(rp_id));
        d.push(0b0100_0001); // UP | AT
        d.extend_from_slice(&count.to_be_bytes());
        d.extend_from_slice(&[0u8; 16]); // aaguid
        #[allow(clippy::cast_possible_truncation)]
        d.extend_from_slice(&(cred_id.len() as u16).to_be_bytes());
        d.extend_from_slice(cred_id);
        d.extend_from_slice(cose);
        d
    }

    fn attestation_object(auth_data: &[u8]) -> Vec<u8> {
        let map = Value::Map(vec![
            (Value::Text("fmt".into()), Value::Text("none".into())),
            (Value::Text("attStmt".into()), Value::Map(vec![])),
            (
                Value::Text("authData".into()),
                Value::Bytes(auth_data.to_vec()),
            ),
        ]);
        let mut out = Vec::new();
        ciborium::ser::into_writer(&map, &mut out).unwrap();
        out
    }

    fn client_data(ty: &str, challenge: &[u8], origin: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "type": ty,
            "challenge": verify::b64url_encode(challenge),
            "origin": origin,
        }))
        .unwrap()
    }

    #[test]
    fn full_register_then_authenticate_round_trip() {
        let rp_id = "example.com";
        let origins = vec!["https://example.com".to_owned()];
        let sk = SigningKey::random(&mut os_rng());
        let cose = cose_key_of(sk.verifying_key());
        let cred_id = b"credential-handle-1";

        // ---- Registration ----
        let reg_challenge = generate_challenge();
        let reg_ad = reg_auth_data(rp_id, cred_id, &cose, 1);
        let att_obj = attestation_object(&reg_ad);
        let reg_client = client_data("webauthn.create", &reg_challenge, "https://example.com");

        let outcome =
            verify_registration(&reg_challenge, rp_id, &origins, &reg_client, &att_obj).unwrap();
        assert_eq!(outcome.credential_id, verify::b64url_encode(cred_id));
        assert_eq!(outcome.sign_count, 1);
        // The stored key must verify a signature from `sk`.
        verify::cose_es256_key(&outcome.cose_public_key).unwrap();

        // ---- Authentication with the registered key ----
        let auth_challenge = generate_challenge();
        let auth_ad = {
            let mut d = Vec::new();
            d.extend_from_slice(&rp_hash(rp_id));
            d.push(0b0000_0001); // UP only (no AT on assertions)
            d.extend_from_slice(&2u32.to_be_bytes()); // count advanced 1 → 2
            d
        };
        let auth_client = client_data("webauthn.get", &auth_challenge, "https://example.com");
        let mut signed = auth_ad.clone();
        let mut ch = Sha256::new();
        ch.update(&auth_client);
        signed.extend_from_slice(&ch.finalize());
        let sig: Signature = sk.sign(&signed);

        let new_count = verify_authentication(
            &auth_challenge,
            rp_id,
            &origins,
            &outcome.cose_public_key,
            outcome.sign_count,
            &auth_client,
            &auth_ad,
            sig.to_der().as_bytes(),
        )
        .unwrap();
        assert_eq!(new_count, 2);
    }

    #[test]
    fn counter_regression_is_rejected() {
        let rp_id = "example.com";
        let origins = vec!["https://example.com".to_owned()];
        let sk = SigningKey::random(&mut os_rng());
        let cose = cose_key_of(sk.verifying_key());

        let challenge = generate_challenge();
        let mut ad = Vec::new();
        ad.extend_from_slice(&rp_hash(rp_id));
        ad.push(0b0000_0001);
        ad.extend_from_slice(&3u32.to_be_bytes()); // count 3
        let client = client_data("webauthn.get", &challenge, "https://example.com");
        let mut signed = ad.clone();
        let mut h = Sha256::new();
        h.update(&client);
        signed.extend_from_slice(&h.finalize());
        let sig: Signature = sk.sign(&signed);

        // Stored count 5 > presented 3 → regression rejected.
        let r = verify_authentication(
            &challenge,
            rp_id,
            &origins,
            &cose,
            5,
            &client,
            &ad,
            sig.to_der().as_bytes(),
        );
        assert!(matches!(r, Err(PasskeyError::CounterRegression)));
    }

    #[test]
    fn options_json_has_required_shape() {
        let reg =
            registration_options_json("example.com", "Example", b"user1", "alice", b"chal", &[]);
        assert_eq!(reg["rp"]["id"], "example.com");
        assert_eq!(reg["pubKeyCredParams"][0]["alg"], -7);
        let auth = authentication_options_json("example.com", b"chal", &["abc".to_owned()]);
        assert_eq!(auth["rpId"], "example.com");
        assert_eq!(auth["allowCredentials"][0]["id"], "abc");
    }
}
