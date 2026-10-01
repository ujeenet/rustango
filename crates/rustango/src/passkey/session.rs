//! Stateless challenge round-trip for the passkey ceremonies (#392).
//!
//! A WebAuthn ceremony spans two requests — the `*_options` call issues a
//! challenge, and the `verify_*` call checks the client echoed it back.
//! The server must remember that challenge in between. Rather than mandate
//! a server-side session store, this seals the challenge (+ an optional
//! caller context string, e.g. the registering user id) with HMAC-SHA256
//! into an opaque token you can stuff in a cookie — the same
//! transport-agnostic pattern as `oauth2::seal_flow`. Tampering or a wrong
//! signing key fails the open. A token is bound to its ceremony, expires
//! after [`CHALLENGE_TTL`] and opens only once (#1841).
//!
//! ```ignore
//! use rustango::passkey::CeremonyPurpose::Registration;
//! // /passkey/register/start
//! let challenge = passkey::generate_challenge();
//! let opts = passkey::registration_options_json(rp_id, rp_name, &uid, name, &challenge, &[]);
//! let cookie = passkey::seal_challenge(&challenge, Registration, current_user_id.to_string().as_bytes(), secret);
//! // → set `cookie` as an HttpOnly Secure cookie, return `opts` JSON.
//!
//! // /passkey/register/finish
//! let (challenge, ctx) = passkey::open_challenge(&cookie, Registration, secret, &*cache).await?; // ctx = user id bytes
//! let outcome = passkey::verify_registration(&challenge, rp_id, &origins, &client_data, &att_obj)?;
//! passkey::register(pool, user_id, &outcome.credential_id, outcome.cose_public_key, outcome.sign_count, "").await?;
//! ```

use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq as _;

/// How long a sealed challenge stays valid (#1841).
pub const CHALLENGE_TTL: Duration = Duration::from_secs(5 * 60);

/// Clock skew tolerated between the replica that sealed and the one that opens.
const CLOCK_SKEW_SECS: i64 = 30;

/// Which ceremony a sealed challenge was issued for. A registration
/// challenge never opens as an authentication one, and the reverse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CeremonyPurpose {
    /// `navigator.credentials.create()`.
    Registration,
    /// `navigator.credentials.get()`.
    Authentication,
}

impl CeremonyPurpose {
    fn tag(self) -> &'static str {
        match self {
            Self::Registration => "reg",
            Self::Authentication => "auth",
        }
    }
}

/// Seal `challenge` (+ free-form `context` bytes — e.g. the registering
/// user id, or `b""` for authentication) into an opaque, HMAC-signed
/// token bound to `purpose` and the issue time. Put it in an
/// HttpOnly+Secure cookie for the duration of the ceremony.
#[must_use]
pub fn seal_challenge(
    challenge: &[u8],
    purpose: CeremonyPurpose,
    context: &[u8],
    secret: &[u8],
) -> String {
    seal_at(
        challenge,
        purpose,
        context,
        secret,
        chrono::Utc::now().timestamp(),
    )
}

fn seal_at(
    challenge: &[u8],
    purpose: CeremonyPurpose,
    context: &[u8],
    secret: &[u8],
    issued_at: i64,
) -> String {
    let payload = format!(
        "{}.{issued_at}.{}.{}",
        purpose.tag(),
        URL_SAFE_NO_PAD.encode(challenge),
        URL_SAFE_NO_PAD.encode(context)
    );
    format!(
        "{payload}.{}",
        URL_SAFE_NO_PAD.encode(mac(secret, &payload))
    )
}

fn mac(secret: &[u8], payload: &str) -> Vec<u8> {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret).expect("HMAC key of any size");
    mac.update(payload.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// Check signature, purpose and age; does not consume the token.
fn verify_at(
    sealed: &str,
    purpose: CeremonyPurpose,
    secret: &[u8],
    now: i64,
) -> Option<(Vec<u8>, Vec<u8>)> {
    let last_dot = sealed.rfind('.')?;
    let (payload, sig_b64) = (&sealed[..last_dot], &sealed[last_dot + 1..]);
    let provided = URL_SAFE_NO_PAD.decode(sig_b64).ok()?;
    if mac(secret, payload).ct_eq(&provided).unwrap_u8() == 0 {
        return None;
    }
    let mut parts = payload.split('.');
    let (tag, issued_at, challenge_b64, context_b64) =
        (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || tag != purpose.tag() {
        return None;
    }
    let issued_at: i64 = issued_at.parse().ok()?;
    let ttl = i64::try_from(CHALLENGE_TTL.as_secs()).ok()?;
    if issued_at > now + CLOCK_SKEW_SECS || now - issued_at > ttl {
        return None;
    }
    let challenge = URL_SAFE_NO_PAD.decode(challenge_b64).ok()?;
    let context = URL_SAFE_NO_PAD.decode(context_b64).ok()?;
    Some((challenge, context))
}

/// Verify, open and consume a token sealed by [`seal_challenge`].
/// Returns the `(challenge, context)` bytes, or `None` if the token is
/// malformed, tampered, signed with another `secret`, issued for another
/// `purpose`, older than [`CHALLENGE_TTL`], or already opened once.
///
/// One-time use is one atomic `cache.add`; use a cache shared by all
/// replicas. Fails closed on a cache error or a cache that stores nothing.
pub async fn open_challenge(
    sealed: &str,
    purpose: CeremonyPurpose,
    secret: &[u8],
    cache: &dyn crate::cache::Cache,
) -> Option<(Vec<u8>, Vec<u8>)> {
    let opened = verify_at(sealed, purpose, secret, chrono::Utc::now().timestamp())?;
    if cache.stores_nothing() {
        tracing::error!(
            target: "rustango::passkey",
            "passkey challenge refused: the cache keeps nothing (`NullCache`); use a shared cache"
        );
        return None;
    }
    let sig = &sealed[sealed.rfind('.')? + 1..];
    let ttl = CHALLENGE_TTL + Duration::from_secs(CLOCK_SKEW_SECS.unsigned_abs());
    match cache
        .add(&format!("passkey_challenge_used:{sig}"), "1", Some(ttl))
        .await
    {
        Ok(true) => Some(opened),
        Ok(false) => None,
        Err(e) => {
            tracing::error!(target: "rustango::passkey", error = %e, "passkey challenge refused: cache failed");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{InMemoryCache, NullCache};
    use CeremonyPurpose::{Authentication, Registration};

    const NOW: i64 = 1_800_000_000;

    #[test]
    fn seal_open_round_trip_with_context() {
        let secret = b"signing-secret";
        let challenge = b"\x00\x01\x02 random-32-byte-ish challenge";
        let token = seal_at(challenge, Registration, b"user-42", secret, NOW);
        let (got_challenge, got_ctx) = verify_at(&token, Registration, secret, NOW).expect("opens");
        assert_eq!(got_challenge, challenge);
        assert_eq!(got_ctx, b"user-42");
    }

    #[test]
    fn empty_context_round_trips() {
        let token = seal_at(b"chal", Authentication, b"", b"k", NOW);
        let (c, ctx) = verify_at(&token, Authentication, b"k", NOW).unwrap();
        assert_eq!(c, b"chal");
        assert!(ctx.is_empty());
    }

    #[test]
    fn wrong_secret_is_rejected() {
        let token = seal_at(b"chal", Registration, b"ctx", b"secret-A", NOW);
        assert!(verify_at(&token, Registration, b"secret-B", NOW).is_none());
    }

    #[test]
    fn tampered_token_is_rejected() {
        let token = seal_at(b"chal", Registration, b"ctx", b"secret", NOW);
        let mut bytes = token.into_bytes();
        bytes[0] ^= 0x01;
        let tampered = String::from_utf8(bytes).unwrap();
        assert!(verify_at(&tampered, Registration, b"secret", NOW).is_none());
    }

    #[test]
    fn malformed_token_is_none() {
        assert!(verify_at("nodothere", Registration, b"s", NOW).is_none());
        assert!(verify_at("only.onedot", Registration, b"s", NOW).is_none());
    }

    #[test]
    fn expired_challenge_is_rejected() {
        let token = seal_at(b"chal", Authentication, b"", b"s", NOW);
        let ttl = CHALLENGE_TTL.as_secs() as i64;
        assert!(verify_at(&token, Authentication, b"s", NOW + ttl).is_some());
        assert!(verify_at(&token, Authentication, b"s", NOW + ttl + 1).is_none());
        // Far-future issue time is not a way around expiry.
        assert!(verify_at(&token, Authentication, b"s", NOW - 3600).is_none());
    }

    #[test]
    fn purpose_is_bound() {
        let token = seal_at(b"chal", Registration, b"", b"s", NOW);
        assert!(verify_at(&token, Authentication, b"s", NOW).is_none());
    }

    #[tokio::test]
    async fn open_is_one_time() {
        let cache = InMemoryCache::new();
        let token = seal_challenge(b"chal", Authentication, b"", b"s");
        assert!(open_challenge(&token, Authentication, b"s", &cache)
            .await
            .is_some());
        assert!(
            open_challenge(&token, Authentication, b"s", &cache)
                .await
                .is_none(),
            "a replayed challenge must not open twice"
        );
    }

    #[tokio::test]
    async fn null_cache_fails_closed() {
        let token = seal_challenge(b"chal", Authentication, b"", b"s");
        assert!(open_challenge(&token, Authentication, b"s", &NullCache)
            .await
            .is_none());
    }
}
