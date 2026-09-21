//! URL-token handoff for operator-as-superuser impersonation.
//!
//! The operator console and the tenant admin sit on different origins,
//! so the console cannot set a cookie the tenant admin will read. A
//! `Domain=.<apex>` cookie works on real DNS but not on `localhost`,
//! which Chromium treats as a public-suffix TLD. A token in the URL
//! works everywhere.
//!
//! ## The flow
//!
//! 1. The console mints a short-lived token, signed with HMAC-SHA256
//!    over the tenant session secret, holding `op_id`, `slug`, `exp`
//!    and a single-use `jti`.
//! 2. It redirects to
//!    `<scheme>://<sub>.<apex>:<port><handoff_url>?token=<signed>`,
//!    setting no cookie of its own.
//! 3. The tenant admin checks the signature, the expiry, the slug and
//!    that the `jti` is unused. It then sets the usual
//!    `rustango_tenant_session` cookie host-scoped, with no `Domain=`,
//!    and redirects to the admin index.
//!
//! ## Security
//!
//! The signature stops tampering. The signed `slug` is checked against
//! the resolved tenant, so a token cannot be replayed on another
//! tenant. The `jti` goes into [`JtiBlacklist`] on redemption, so a
//! second use gives [`HandoffError::AlreadyUsed`]. The token lives 60
//! seconds; the cookie it produces keeps the normal impersonation TTL.
//!
//! The token does land in browser history. Single use and the short
//! TTL bound that, and the handoff response sends
//! `Referrer-Policy: no-referrer` so it does not leak through
//! `Referer`.
//!
//! [`JtiBlacklist`] is per process. Behind a load balancer, a redeemed
//! token could be replayed against another process inside the TTL. A
//! shared store (Redis `SETNX`, or a `rustango_used_jti` table) would
//! close that.

use std::sync::{Arc, OnceLock};

use base64::Engine;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

pub use super::session::SessionSecret;
use super::session::{sign, SessionError};

/// Token lifetime, 60 seconds. Click, redirect, redeem: nothing real
/// takes longer, and a wider window only helps an attacker.
pub const HANDOFF_TTL_SECS: i64 = 60;

/// Errors decoding or validating a handoff token.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HandoffError {
    /// Token shape is wrong (no `.` separator, bad base64, bad JSON).
    #[error("handoff token malformed")]
    Malformed,
    /// HMAC signature didn't verify.
    #[error("handoff token signature invalid")]
    BadSignature,
    /// `exp` is in the past.
    #[error("handoff token expired")]
    Expired,
    /// The `slug` field doesn't match the resolved tenant slug.
    #[error("handoff token bound to a different tenant")]
    WrongTenant,
    /// The `jti` has already been redeemed.
    #[error("handoff token already used")]
    AlreadyUsed,
}

impl From<SessionError> for HandoffError {
    fn from(e: SessionError) -> Self {
        match e {
            SessionError::Malformed => Self::Malformed,
            SessionError::BadSignature => Self::BadSignature,
            SessionError::Expired => Self::Expired,
            SessionError::WrongTenant => Self::WrongTenant,
        }
    }
}

/// Signed payload carried in the `?token=` query parameter.
///
/// The field names are short so the token fits comfortably in a
/// `Location` header: about 150 bytes encoded.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HandoffPayload {
    /// Operator id from `rustango_operators`. Ends up in the
    /// impersonation cookie's `imp` field.
    pub op: i64,
    /// Tenant slug this token was minted for. Checked against the
    /// resolved org, so a leaked URL cannot be used on another tenant.
    pub slug: String,
    /// Expiry as Unix seconds.
    pub exp: i64,
    /// Single-use id: 16 random bytes, base64url. Recorded in
    /// [`JtiBlacklist`] on redemption; a second use gives
    /// [`HandoffError::AlreadyUsed`].
    pub jti: String,
}

impl HandoffPayload {
    /// Build a fresh payload with a random `jti` and `exp = now + ttl`.
    #[must_use]
    pub fn new(op_id: i64, slug: impl Into<String>, ttl_secs: i64) -> Self {
        let now = chrono::Utc::now().timestamp();
        // From the OS CSPRNG. A guessable jti would let someone who saw
        // one handoff URL predict the next and slip past the
        // single-use check.
        use rand::{rngs::OsRng, RngCore};
        let mut bytes = [0u8; 16];
        OsRng.fill_bytes(&mut bytes[..]);
        let jti = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        Self {
            op: op_id,
            slug: slug.into(),
            exp: now + ttl_secs,
            jti,
        }
    }

    fn is_expired(&self) -> bool {
        chrono::Utc::now().timestamp() >= self.exp
    }
}

/// Sign and encode a payload as `<b64(json)>.<b64(hmac)>`, the same
/// wire format as the tenant session cookie, so both share one key.
#[must_use]
pub fn mint(secret: &SessionSecret, payload: &HandoffPayload) -> String {
    let json = serde_json::to_vec(payload).expect("payload serializes");
    let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json);
    let sig = sign(secret, payload_b64.as_bytes());
    let sig_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig);
    format!("{payload_b64}.{sig_b64}")
}

/// Verify a handoff token, decode it, and check it is bound to
/// `expected_slug`.
///
/// It does not check the `jti`. The caller does that, so it can
/// `mark_used` in the same step that mints the cookie.
///
/// # Errors
/// See [`HandoffError`].
pub fn decode(
    secret: &SessionSecret,
    expected_slug: &str,
    value: &str,
) -> Result<HandoffPayload, HandoffError> {
    let (payload_b64, sig_b64) = value.split_once('.').ok_or(HandoffError::Malformed)?;
    let expected = sign(secret, payload_b64.as_bytes());
    let provided = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(sig_b64)
        .map_err(|_| HandoffError::Malformed)?;
    if expected.ct_eq(&provided[..]).unwrap_u8() == 0 {
        return Err(HandoffError::BadSignature);
    }
    let payload_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|_| HandoffError::Malformed)?;
    let payload: HandoffPayload =
        serde_json::from_slice(&payload_bytes).map_err(|_| HandoffError::Malformed)?;
    if payload.is_expired() {
        return Err(HandoffError::Expired);
    }
    if payload.slug != expected_slug {
        return Err(HandoffError::WrongTenant);
    }
    Ok(payload)
}

/// Tracks which handoff tokens have been redeemed.
///
/// Storage comes from [`crate::jti_store::JtiStore`]. The default is
/// in-process; pass a shared store to [`Self::with_store`] to make it
/// work across processes.
pub struct JtiBlacklist {
    store: Arc<dyn crate::jti_store::JtiStore>,
}

impl JtiBlacklist {
    /// An in-memory store, fine for one process. Use
    /// [`Self::with_store`] when you run more than one.
    fn new() -> Self {
        Self {
            store: Arc::new(crate::jti_store::InMemoryJtiStore::new()),
        }
    }

    /// Back the blacklist with your own store, e.g. a shared one for a
    /// multi-process deployment.
    #[must_use]
    pub fn with_store(store: Arc<dyn crate::jti_store::JtiStore>) -> Self {
        Self { store }
    }

    /// Process-wide singleton over the in-memory store. Run more than
    /// one process and you want your own [`Self::with_store`] instead,
    /// passed into the redeem path.
    pub fn shared() -> &'static Self {
        static INSTANCE: OnceLock<JtiBlacklist> = OnceLock::new();
        INSTANCE.get_or_init(Self::new)
    }

    /// `true` if the jti was already marked used.
    pub async fn is_used(&self, jti: &str) -> bool {
        self.store.is_used(jti).await
    }

    /// Check and record in one step. Gives `Err(AlreadyUsed)` if the
    /// jti is already there, otherwise stores `(jti, exp)`. The store
    /// prunes expired entries.
    ///
    /// # Errors
    /// [`HandoffError::AlreadyUsed`] when the jti was already redeemed.
    pub async fn mark_used(&self, jti: &str, exp: i64) -> Result<(), HandoffError> {
        if self.store.mark_used(jti, exp).await {
            Ok(())
        } else {
            Err(HandoffError::AlreadyUsed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> SessionSecret {
        SessionSecret::from_bytes(b"a-test-secret-thirty-two-bytes-x".to_vec())
    }

    #[test]
    fn round_trip_valid_payload() {
        let secret = key();
        let payload = HandoffPayload::new(7, "acme", 60);
        let token = mint(&secret, &payload);
        let back = decode(&secret, "acme", &token).unwrap();
        assert_eq!(back, payload);
    }

    #[test]
    fn rejects_token_minted_for_a_different_tenant() {
        let secret = key();
        let payload = HandoffPayload::new(7, "acme", 60);
        let token = mint(&secret, &payload);
        assert_eq!(
            decode(&secret, "globex", &token).unwrap_err(),
            HandoffError::WrongTenant,
        );
    }

    #[test]
    fn rejects_tampered_signature() {
        let secret = key();
        let payload = HandoffPayload::new(7, "acme", 60);
        let token = mint(&secret, &payload);
        // Flip a byte in the middle of the signature, not the last one.
        // The last base64 char of a 32-byte HMAC has only some valid
        // values, so flipping it can give `Malformed` instead of the
        // `BadSignature` this test is about.
        let mut bytes = token.into_bytes();
        let dot = bytes
            .iter()
            .rposition(|&b| b == b'.')
            .expect("token has `payload.sig` shape");
        let mid_sig = dot + 1 + (bytes.len() - dot - 1) / 2;
        bytes[mid_sig] = if bytes[mid_sig] == b'A' { b'B' } else { b'A' };
        let tampered = String::from_utf8(bytes).unwrap();
        assert_eq!(
            decode(&secret, "acme", &tampered).unwrap_err(),
            HandoffError::BadSignature,
        );
    }

    #[test]
    fn rejects_token_signed_with_a_different_secret() {
        let s1 = key();
        let s2 = SessionSecret::from_bytes(b"b-other-secret-thirty-two-bytes-x".to_vec());
        let token = mint(&s1, &HandoffPayload::new(7, "acme", 60));
        assert_eq!(
            decode(&s2, "acme", &token).unwrap_err(),
            HandoffError::BadSignature,
        );
    }

    #[test]
    fn rejects_expired_token() {
        let secret = key();
        let token = mint(&secret, &HandoffPayload::new(7, "acme", -10));
        assert_eq!(
            decode(&secret, "acme", &token).unwrap_err(),
            HandoffError::Expired,
        );
    }

    #[test]
    fn rejects_malformed_token() {
        let secret = key();
        assert_eq!(
            decode(&secret, "acme", "not-a-token").unwrap_err(),
            HandoffError::Malformed,
        );
        // A malformed sig segment (non-base64) reports Malformed
        // before any constant-time compare runs.
        assert_eq!(
            decode(&secret, "acme", "abc.!!!").unwrap_err(),
            HandoffError::Malformed,
        );
    }

    #[test]
    fn jtis_are_unique_across_mints() {
        let p1 = HandoffPayload::new(7, "acme", 60);
        let p2 = HandoffPayload::new(7, "acme", 60);
        assert_ne!(p1.jti, p2.jti, "random jti collision is unacceptable");
    }

    #[tokio::test]
    async fn jti_blacklist_first_use_succeeds_second_fails() {
        let bl = JtiBlacklist::new();
        let jti = "abc123";
        let exp = chrono::Utc::now().timestamp() + 60;
        bl.mark_used(jti, exp).await.unwrap();
        assert!(bl.is_used(jti).await);
        assert_eq!(
            bl.mark_used(jti, exp).await.unwrap_err(),
            HandoffError::AlreadyUsed
        );
    }

    #[tokio::test]
    async fn jti_blacklist_with_store_delegates_to_swapped_backend() {
        // v0.47 — proves the multi-instance hook: an Arc<dyn JtiStore>
        // passed via `with_store` is the source of truth, not the
        // default in-memory map. Two JtiBlacklist handles built from
        // the same store share state — that's exactly what a Redis-
        // backed store would give a multi-process deployment.
        use crate::jti_store::{InMemoryJtiStore, JtiStore};
        use std::sync::Arc;
        let shared: Arc<dyn JtiStore> = Arc::new(InMemoryJtiStore::new());
        let bl_a = JtiBlacklist::with_store(Arc::clone(&shared));
        let bl_b = JtiBlacklist::with_store(Arc::clone(&shared));
        let jti = "shared-token";
        let exp = chrono::Utc::now().timestamp() + 60;
        bl_a.mark_used(jti, exp).await.unwrap();
        assert!(
            bl_b.is_used(jti).await,
            "second handle on the shared store must see the mark"
        );
        assert_eq!(
            bl_b.mark_used(jti, exp).await.unwrap_err(),
            HandoffError::AlreadyUsed,
            "single-use guard must hold across handles on the shared store"
        );
    }

    // v0.47 — JTI pruning behaviour moved to the `JtiStore` trait
    // and is covered by `jti_store::tests::expired_entries_are_pruned_on_next_mark`.
    // The duplicate JtiBlacklist-level test reached into `bl.inner`
    // directly which is no longer a field (storage is now an
    // `Arc<dyn JtiStore>`).

    /// `HANDOFF_TTL_SECS` is short by design — long enough for the
    /// browser to redirect, short enough that browser-history leak
    /// is bounded. Pin the value so a future caller can't quietly
    /// bump it to "5 minutes" without thinking about the trade-off.
    #[test]
    fn handoff_ttl_default_is_one_minute() {
        assert_eq!(HANDOFF_TTL_SECS, 60);
    }
}
