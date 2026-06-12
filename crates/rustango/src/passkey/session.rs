//! Stateless challenge round-trip for the passkey ceremonies (#392).
//!
//! A WebAuthn ceremony spans two requests — the `*_options` call issues a
//! challenge, and the `verify_*` call checks the client echoed it back.
//! The server must remember that challenge in between. Rather than mandate
//! a server-side session store, this seals the challenge (+ an optional
//! caller context string, e.g. the registering user id) with HMAC-SHA256
//! into an opaque token you can stuff in a cookie — the same
//! transport-agnostic pattern as `oauth2::seal_flow`. Tampering or a wrong
//! signing key fails the open; the challenge can't be forged or replayed
//! across a key rotation.
//!
//! ```ignore
//! // /passkey/register/start
//! let challenge = passkey::generate_challenge();
//! let opts = passkey::registration_options_json(rp_id, rp_name, &uid, name, &challenge, &[]);
//! let cookie = passkey::seal_challenge(&challenge, current_user_id.to_string().as_bytes(), secret);
//! // → set `cookie` as an HttpOnly Secure cookie, return `opts` JSON.
//!
//! // /passkey/register/finish
//! let (challenge, ctx) = passkey::open_challenge(&cookie, secret)?;       // ctx = user id bytes
//! let outcome = passkey::verify_registration(&challenge, rp_id, &origins, &client_data, &att_obj)?;
//! passkey::register(pool, user_id, &outcome.credential_id, outcome.cose_public_key, outcome.sign_count, "").await?;
//! ```

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq as _;

/// Seal `challenge` (+ free-form `context` bytes — e.g. the registering
/// user id, or `b""` for authentication) into an opaque, HMAC-signed
/// token. Put it in an HttpOnly+Secure cookie for the duration of the
/// ceremony.
#[must_use]
pub fn seal_challenge(challenge: &[u8], context: &[u8], secret: &[u8]) -> String {
    let challenge_b64 = URL_SAFE_NO_PAD.encode(challenge);
    let context_b64 = URL_SAFE_NO_PAD.encode(context);
    let payload = format!("{challenge_b64}.{context_b64}");
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret).expect("HMAC key of any size");
    mac.update(payload.as_bytes());
    let sig = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    format!("{payload}.{sig}")
}

/// Verify + open a token sealed by [`seal_challenge`]. Returns the
/// `(challenge, context)` bytes, or `None` if the token is malformed,
/// tampered, or signed with a different `secret`. The signature check is
/// constant-time.
#[must_use]
pub fn open_challenge(sealed: &str, secret: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    // payload = "<challenge_b64>.<context_b64>"; token = "payload.sig".
    let last_dot = sealed.rfind('.')?;
    let (payload, sig_b64) = (&sealed[..last_dot], &sealed[last_dot + 1..]);
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret).expect("HMAC key of any size");
    mac.update(payload.as_bytes());
    let expected = mac.finalize().into_bytes();
    let provided = URL_SAFE_NO_PAD.decode(sig_b64).ok()?;
    if expected.ct_eq(&provided).unwrap_u8() == 0 {
        return None;
    }
    let (challenge_b64, context_b64) = payload.split_once('.')?;
    let challenge = URL_SAFE_NO_PAD.decode(challenge_b64).ok()?;
    let context = URL_SAFE_NO_PAD.decode(context_b64).ok()?;
    Some((challenge, context))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_round_trip_with_context() {
        let secret = b"signing-secret";
        let challenge = b"\x00\x01\x02 random-32-byte-ish challenge";
        let token = seal_challenge(challenge, b"user-42", secret);
        let (got_challenge, got_ctx) = open_challenge(&token, secret).expect("opens");
        assert_eq!(got_challenge, challenge);
        assert_eq!(got_ctx, b"user-42");
    }

    #[test]
    fn empty_context_round_trips() {
        let secret = b"k";
        let token = seal_challenge(b"chal", b"", secret);
        let (c, ctx) = open_challenge(&token, secret).unwrap();
        assert_eq!(c, b"chal");
        assert!(ctx.is_empty());
    }

    #[test]
    fn wrong_secret_is_rejected() {
        let token = seal_challenge(b"chal", b"ctx", b"secret-A");
        assert!(open_challenge(&token, b"secret-B").is_none());
    }

    #[test]
    fn tampered_token_is_rejected() {
        let token = seal_challenge(b"chal", b"ctx", b"secret");
        // Flip a payload char.
        let mut bytes = token.into_bytes();
        bytes[0] ^= 0x01;
        let tampered = String::from_utf8(bytes).unwrap();
        assert!(open_challenge(&tampered, b"secret").is_none());
    }

    #[test]
    fn malformed_token_is_none() {
        assert!(open_challenge("nodothere", b"s").is_none());
        assert!(open_challenge("only.onedot", b"s").is_none());
    }
}
