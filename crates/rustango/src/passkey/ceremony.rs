//! WebAuthn registration + authentication ceremonies (#392).
//!
//! These orchestrate the [`super::verify`] primitives into the two
//! server-side verification steps, plus the challenge + options helpers
//! the browser's `navigator.credentials.create()/get()` need. The crypto
//! uses `ciborium` and `ring`; see the module docs.
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
#[non_exhaustive]
pub struct RegistrationOutcome {
    /// base64url (no pad) of the raw credential id.
    pub credential_id: String,
    /// COSE-encoded public key bytes.
    pub cose_public_key: Vec<u8>,
    /// The authenticator's initial signature counter.
    pub sign_count: i64,
    /// The UV flag: the user passed a PIN or biometric, not just a touch.
    pub user_verified: bool,
}

impl RegistrationOutcome {
    /// Refuse the registration when `policy` is [`UserVerification::Required`]
    /// and the authenticator did not verify the user.
    ///
    /// # Errors
    /// [`PasskeyError::UserNotVerified`].
    pub fn require(self, policy: UserVerification) -> Result<Self, PasskeyError> {
        policy.check(self.user_verified).map(|()| self)
    }
}

/// Whether the authenticator must verify the user (PIN or biometric).
/// Send it in the options, then check it on the outcome with `require`:
/// the browser's answer is not proof.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum UserVerification {
    /// Fail unless the user is verified. Needed to trust PRF output.
    Required,
    /// Verify when the authenticator can.
    #[default]
    Preferred,
    /// Skip verification when possible.
    Discouraged,
}

impl UserVerification {
    fn as_str(self) -> &'static str {
        match self {
            Self::Required => "required",
            Self::Preferred => "preferred",
            Self::Discouraged => "discouraged",
        }
    }

    fn check(self, user_verified: bool) -> Result<(), PasskeyError> {
        if self == Self::Required && !user_verified {
            return Err(PasskeyError::UserNotVerified);
        }
        Ok(())
    }
}

/// Verify a registration response (WebAuthn §7.1, `none`/self attestation).
/// It checks presence, not user verification: when the options asked for
/// [`UserVerification::Required`], call `.require(UserVerification::Required)`
/// on the outcome, or a touch-only key is accepted.
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
    let user_verified = ad.user_verified();
    let attested = ad.attested.ok_or_else(|| {
        PasskeyError::AuthData("registration authData has no attested credential data".into())
    })?;
    // `none`/self attestation: trust the embedded key (no cert chain).
    // This checks the COSE shape (EC2, P-256, ES256, 32-byte x and y); a point
    // off the curve is only refused when an assertion is verified.
    verify::cose_es256_key(&attested.cose_public_key)?;

    Ok(RegistrationOutcome {
        credential_id: verify::b64url_encode(&attested.credential_id),
        cose_public_key: attested.cose_public_key,
        sign_count: i64::from(ad.sign_count),
        user_verified,
    })
}

/// Verify an authentication (assertion) response (WebAuthn §7.2).
/// Returns the new signature counter and the UV flag. As with
/// [`verify_registration`], enforce a `Required` policy with `.require(..)`.
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
) -> Result<AuthenticationOutcome, PasskeyError> {
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
    // WebAuthn §7.2 step 21: once either count is non-zero it must advance,
    // so a clone can't reset a stored counter to 0.
    let new = i64::from(ad.sign_count);
    if (new != 0 || stored_sign_count != 0) && new <= stored_sign_count {
        return Err(PasskeyError::CounterRegression);
    }
    Ok(AuthenticationOutcome {
        sign_count: new,
        user_verified: ad.user_verified(),
    })
}

/// What a verified assertion yields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct AuthenticationOutcome {
    /// New signature counter; persist via [`super::update_sign_count`].
    pub sign_count: i64,
    /// The UV flag: the user passed a PIN or biometric, not just a touch.
    /// Refuse when your policy needs user verification.
    pub user_verified: bool,
}

impl AuthenticationOutcome {
    /// Refuse the sign-in when `policy` is [`UserVerification::Required`]
    /// and the authenticator did not verify the user.
    ///
    /// # Errors
    /// [`PasskeyError::UserNotVerified`].
    pub fn require(self, policy: UserVerification) -> Result<Self, PasskeyError> {
        policy.check(self.user_verified).map(|()| self)
    }
}

/// `PublicKeyCredentialCreationOptions` JSON for
/// `navigator.credentials.create({ publicKey })`. `user_id` is encoded
/// base64url; `exclude` lists the user's existing credential ids (their
/// stored base64url form) so the authenticator won't double-register.
/// [`RegistrationOptions`] adds user verification and PRF.
#[must_use]
pub fn registration_options_json(
    rp_id: &str,
    rp_name: &str,
    user_id: &[u8],
    user_name: &str,
    challenge: &[u8],
    exclude_credential_ids: &[String],
) -> serde_json::Value {
    RegistrationOptions::new(rp_id, rp_name, user_id, user_name, challenge)
        .exclude(exclude_credential_ids)
        .to_json()
}

/// `PublicKeyCredentialRequestOptions` JSON for
/// `navigator.credentials.get({ publicKey })`. `allow_credential_ids` are
/// the user's stored base64url credential ids.
/// [`AuthenticationOptions`] adds user verification and PRF.
#[must_use]
pub fn authentication_options_json(
    rp_id: &str,
    challenge: &[u8],
    allow_credential_ids: &[String],
) -> serde_json::Value {
    AuthenticationOptions::new(rp_id, challenge)
        .allow(allow_credential_ids)
        .to_json()
}

/// Options for `navigator.credentials.create()`, with user verification
/// and the PRF extension.
///
/// ```
/// use rustango::passkey::{RegistrationOptions, UserVerification};
/// let opts = RegistrationOptions::new("example.com", "Example", b"u1", "alice", b"chal")
///     .user_verification(UserVerification::Required)
///     .prf()
///     .to_json();
/// assert_eq!(opts["authenticatorSelection"]["userVerification"], "required");
/// assert!(opts["extensions"]["prf"].is_object());
/// ```
#[derive(Debug, Clone)]
#[must_use]
pub struct RegistrationOptions<'a> {
    rp_id: &'a str,
    rp_name: &'a str,
    user_id: &'a [u8],
    user_name: &'a str,
    challenge: &'a [u8],
    exclude: &'a [String],
    user_verification: UserVerification,
    prf: bool,
}

impl<'a> RegistrationOptions<'a> {
    /// Options for one user; see [`registration_options_json`] for the fields.
    pub fn new(
        rp_id: &'a str,
        rp_name: &'a str,
        user_id: &'a [u8],
        user_name: &'a str,
        challenge: &'a [u8],
    ) -> Self {
        Self {
            rp_id,
            rp_name,
            user_id,
            user_name,
            challenge,
            exclude: &[],
            user_verification: UserVerification::default(),
            prf: false,
        }
    }

    /// The user's existing credential ids, so none is registered twice.
    pub fn exclude(mut self, credential_ids: &'a [String]) -> Self {
        self.exclude = credential_ids;
        self
    }

    /// Ask for user verification; check it with [`RegistrationOutcome::require`].
    pub fn user_verification(mut self, policy: UserVerification) -> Self {
        self.user_verification = policy;
        self
    }

    /// Ask the authenticator to enable the PRF extension (`prf: {}`). Read
    /// `getClientExtensionResults().prf.enabled` in the browser.
    pub fn prf(mut self) -> Self {
        self.prf = true;
        self
    }

    /// The JSON the browser needs.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let mut v = serde_json::json!({
            "challenge": verify::b64url_encode(self.challenge),
            "rp": { "id": self.rp_id, "name": self.rp_name },
            "user": {
                "id": verify::b64url_encode(self.user_id),
                "name": self.user_name,
                "displayName": self.user_name,
            },
            // -7 = ES256 (the only algorithm this slice verifies).
            "pubKeyCredParams": [ { "type": "public-key", "alg": -7 } ],
            "timeout": 60000,
            "attestation": "none",
            "excludeCredentials": credential_list(self.exclude),
            "authenticatorSelection": {
                "userVerification": self.user_verification.as_str(),
                "residentKey": "preferred",
            },
        });
        if self.prf {
            v["extensions"] = serde_json::json!({ "prf": {} });
        }
        v
    }
}

/// Options for `navigator.credentials.get()`, with user verification and
/// PRF salts. The PRF output stays in the browser; the app stores the
/// per-credential salt.
///
/// ```
/// use rustango::passkey::{AuthenticationOptions, UserVerification};
/// let ids = ["cred1".to_owned()];
/// let opts = AuthenticationOptions::new("example.com", b"chal")
///     .allow(&ids)
///     .user_verification(UserVerification::Required)
///     .prf_salt(b"salt-for-everyone")
///     .prf_salt_for("cred1", b"salt-for-cred1")
///     .to_json();
/// assert_eq!(opts["userVerification"], "required");
/// assert!(opts["extensions"]["prf"]["eval"]["first"].is_string());
/// assert!(opts["extensions"]["prf"]["evalByCredential"]["cred1"]["first"].is_string());
/// ```
#[derive(Debug, Clone)]
#[must_use]
pub struct AuthenticationOptions<'a> {
    rp_id: &'a str,
    challenge: &'a [u8],
    allow: &'a [String],
    user_verification: UserVerification,
    prf_salt: Option<&'a [u8]>,
    prf_by_credential: Vec<(&'a str, &'a [u8])>,
}

impl<'a> AuthenticationOptions<'a> {
    /// Options for one sign-in; see [`authentication_options_json`].
    pub fn new(rp_id: &'a str, challenge: &'a [u8]) -> Self {
        Self {
            rp_id,
            challenge,
            allow: &[],
            user_verification: UserVerification::default(),
            prf_salt: None,
            prf_by_credential: Vec::new(),
        }
    }

    /// The user's stored credential ids; empty lets any passkey answer.
    pub fn allow(mut self, credential_ids: &'a [String]) -> Self {
        self.allow = credential_ids;
        self
    }

    /// Ask for user verification; check it with [`AuthenticationOutcome::require`].
    pub fn user_verification(mut self, policy: UserVerification) -> Self {
        self.user_verification = policy;
        self
    }

    /// PRF input for any credential (`prf.eval.first`).
    pub fn prf_salt(mut self, salt: &'a [u8]) -> Self {
        self.prf_salt = Some(salt);
        self
    }

    /// PRF input for one credential id (`prf.evalByCredential`), such as
    /// the salt stored with that credential. Needs it in [`Self::allow`].
    pub fn prf_salt_for(mut self, credential_id: &'a str, salt: &'a [u8]) -> Self {
        self.prf_by_credential.push((credential_id, salt));
        self
    }

    /// The JSON the browser needs.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let mut v = serde_json::json!({
            "challenge": verify::b64url_encode(self.challenge),
            "rpId": self.rp_id,
            "timeout": 60000,
            "userVerification": self.user_verification.as_str(),
            "allowCredentials": credential_list(self.allow),
        });
        let mut prf = serde_json::Map::new();
        if let Some(salt) = self.prf_salt {
            prf.insert("eval".into(), prf_values(salt));
        }
        if !self.prf_by_credential.is_empty() {
            let by: serde_json::Map<String, serde_json::Value> = self
                .prf_by_credential
                .iter()
                .map(|(id, salt)| ((*id).to_owned(), prf_values(salt)))
                .collect();
            prf.insert("evalByCredential".into(), by.into());
        }
        if !prf.is_empty() {
            v["extensions"] = serde_json::json!({ "prf": prf });
        }
        v
    }
}

/// `[{ type: "public-key", id }]` for stored base64url ids.
fn credential_list(ids: &[String]) -> Vec<serde_json::Value> {
    ids.iter()
        .map(|id| serde_json::json!({ "type": "public-key", "id": id }))
        .collect()
}

/// `{ first: <base64url salt> }`; the browser decodes it to a BufferSource.
fn prf_values(salt: &[u8]) -> serde_json::Value {
    serde_json::json!({ "first": verify::b64url_encode(salt) })
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
    use ciborium::value::Value;
    use sha2::{Digest, Sha256};

    use crate::passkey::verify::tests::TestSigner;

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

    /// A registration with the UV flag reports it, and passes `Required`.
    #[test]
    fn a_verified_registration_passes_required() {
        let (rp_id, origins) = ("example.com", vec!["https://example.com".to_owned()]);
        let cose = TestSigner::new().cose();
        let challenge = generate_challenge();
        let mut ad = reg_auth_data(rp_id, b"cred-uv", &cose, 1);
        ad[32] |= 0b0000_0100; // UV, after the 32-byte rpIdHash
        let client = client_data("webauthn.create", &challenge, "https://example.com");
        let outcome = verify_registration(
            &challenge,
            rp_id,
            &origins,
            &client,
            &attestation_object(&ad),
        )
        .unwrap();
        assert!(outcome.user_verified);
        assert!(outcome.require(UserVerification::Required).is_ok());
    }

    #[test]
    fn full_register_then_authenticate_round_trip() {
        let rp_id = "example.com";
        let origins = vec!["https://example.com".to_owned()];
        let sk = TestSigner::new();
        let cose = sk.cose();
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
        // A touch-only registration fails a `Required` policy.
        assert!(!outcome.user_verified);
        assert!(matches!(
            outcome.clone().require(UserVerification::Required),
            Err(PasskeyError::UserNotVerified)
        ));
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
        let sig = sk.sign(&signed);

        let new_count = verify_authentication(
            &auth_challenge,
            rp_id,
            &origins,
            &outcome.cose_public_key,
            outcome.sign_count,
            &auth_client,
            &auth_ad,
            &sig,
        )
        .unwrap();
        assert_eq!(new_count.sign_count, 2);
        assert!(!new_count.user_verified);
    }

    /// Sign an assertion with `flags` and `count`, verify it against `stored`.
    fn assert_with(
        flags: u8,
        count: u32,
        stored: i64,
    ) -> Result<AuthenticationOutcome, PasskeyError> {
        let rp_id = "example.com";
        let origins = vec!["https://example.com".to_owned()];
        let sk = TestSigner::new();
        let cose = sk.cose();

        let challenge = generate_challenge();
        let mut ad = Vec::new();
        ad.extend_from_slice(&rp_hash(rp_id));
        ad.push(flags);
        ad.extend_from_slice(&count.to_be_bytes());
        let client = client_data("webauthn.get", &challenge, "https://example.com");
        let mut signed = ad.clone();
        let mut h = Sha256::new();
        h.update(&client);
        signed.extend_from_slice(&h.finalize());
        let sig = sk.sign(&signed);

        verify_authentication(
            &challenge, rp_id, &origins, &cose, stored, &client, &ad, &sig,
        )
    }

    #[test]
    fn counter_regression_is_rejected() {
        // Stored count 5 > presented 3 → regression rejected.
        let r = assert_with(0b0000_0001, 3, 5);
        assert!(matches!(r, Err(PasskeyError::CounterRegression)));
    }

    #[test]
    fn clone_resetting_counter_to_zero_is_rejected() {
        let r = assert_with(0b0000_0001, 0, 5);
        assert!(matches!(r, Err(PasskeyError::CounterRegression)));
        // An authenticator with no counter (0 → 0) still works.
        assert_eq!(assert_with(0b0000_0001, 0, 0).unwrap().sign_count, 0);
    }

    #[test]
    fn user_verified_flag_is_returned() {
        assert!(assert_with(0b0000_0101, 1, 0).unwrap().user_verified);
        assert!(!assert_with(0b0000_0001, 1, 0).unwrap().user_verified);
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
        // The plain helpers keep `preferred` and send no extensions.
        assert_eq!(
            reg["authenticatorSelection"]["userVerification"],
            "preferred"
        );
        assert_eq!(auth["userVerification"], "preferred");
        assert!(reg.get("extensions").is_none() && auth.get("extensions").is_none());
    }

    #[test]
    fn prf_and_required_uv_reach_the_options() {
        let reg = RegistrationOptions::new("example.com", "Example", b"u", "alice", b"chal")
            .user_verification(UserVerification::Required)
            .prf()
            .to_json();
        assert_eq!(
            reg["authenticatorSelection"]["userVerification"],
            "required"
        );
        assert_eq!(reg["extensions"], serde_json::json!({ "prf": {} }));
        let ids = ["c1".to_owned()];
        let auth = AuthenticationOptions::new("example.com", b"chal")
            .allow(&ids)
            .user_verification(UserVerification::Required)
            .prf_salt(b"s0")
            .prf_salt_for("c1", b"s1")
            .to_json();
        assert_eq!(auth["userVerification"], "required");
        assert_eq!(
            auth["extensions"]["prf"],
            serde_json::json!({
                "eval": { "first": verify::b64url_encode(b"s0") },
                "evalByCredential": { "c1": { "first": verify::b64url_encode(b"s1") } },
            })
        );
    }

    /// `Required` refuses an assertion without the UV flag.
    #[test]
    fn required_user_verification_refuses_a_touch_only_assertion() {
        let touch = assert_with(0b0000_0001, 1, 0).unwrap();
        assert!(matches!(
            touch.require(UserVerification::Required),
            Err(PasskeyError::UserNotVerified)
        ));
        assert!(touch.require(UserVerification::Preferred).is_ok());
        let verified = assert_with(0b0000_0101, 1, 0).unwrap();
        assert!(verified.require(UserVerification::Required).is_ok());
    }
}
