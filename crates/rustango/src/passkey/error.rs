//! Passkey / WebAuthn ceremony errors (#392).

/// What went wrong verifying a WebAuthn registration or assertion.
/// Deliberately coarse on the *reason* surfaced to clients (a precise
/// "challenge mismatch" vs "bad signature" can aid an attacker) — log
/// the detail, return a generic "verification failed" to the user.
#[derive(Debug, thiserror::Error)]
pub enum PasskeyError {
    /// `clientDataJSON` wasn't valid JSON, or a required field was missing.
    #[error("malformed clientDataJSON: {0}")]
    ClientData(String),
    /// `clientDataJSON.type` wasn't the expected ceremony type
    /// (`webauthn.create` for registration, `webauthn.get` for assertion).
    #[error("unexpected clientData type: expected `{expected}`, got `{got}`")]
    WrongType { expected: &'static str, got: String },
    /// The challenge echoed by the client didn't match the server's.
    #[error("challenge mismatch")]
    ChallengeMismatch,
    /// The `origin` in `clientDataJSON` isn't an allowed origin.
    #[error("origin `{0}` is not allowed")]
    BadOrigin(String),
    /// The `rpIdHash` in `authenticatorData` ≠ SHA-256(rp_id).
    #[error("rpIdHash does not match the configured rp_id")]
    RpIdMismatch,
    /// The User-Present flag wasn't set — the authenticator reported no
    /// user interaction.
    #[error("user-present flag not set")]
    UserNotPresent,
    /// `authenticatorData` / `attestationObject` was truncated or
    /// otherwise structurally invalid.
    #[error("malformed authenticator data: {0}")]
    AuthData(String),
    /// The COSE public key wasn't a supported algorithm (only ES256 /
    /// ECDSA-P256 is supported in this slice).
    #[error("unsupported or malformed COSE key: {0}")]
    CoseKey(String),
    /// The assertion signature failed ES256 verification.
    #[error("signature verification failed")]
    BadSignature,
    /// The authenticator's signature counter didn't advance — a possible
    /// cloned authenticator (or a buggy one). The WebAuthn spec lets the
    /// RP decide; rustango rejects a regression when both counts are > 0.
    #[error("signature counter did not increase (possible cloned authenticator)")]
    CounterRegression,
    /// CBOR decode failure.
    #[error("CBOR decode error: {0}")]
    Cbor(String),
}
