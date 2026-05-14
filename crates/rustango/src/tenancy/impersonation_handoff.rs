//! URL-token handoff for operator-as-superuser impersonation (#88).
//!
//! ## The problem this solves
//!
//! The original `/orgs/{slug}/impersonate` flow (#78, v0.27.8) mints
//! a tenant session cookie on the operator-console origin
//! (`<apex>:<port>`) with `Domain=.<apex>` so subdomains receive it,
//! then 302s to the tenant admin (`<slug>.<apex>:<port>/admin/`).
//!
//! On real DNS (`example.com`), subdomain cookies cross the origin
//! boundary and the flow Just Works. **On Chromium-family browsers
//! against `localhost`** — and Chromium treats `localhost` as a
//! public-suffix-list TLD — `Domain=localhost` is rejected on
//! subdomains, so the cookie never reaches `acme.localhost`. The
//! operator lands on the tenant login page instead of an
//! impersonated admin. Firefox is more lenient.
//!
//! ## How the handoff works
//!
//! 1. Operator console mints a short-lived signed token (HMAC-SHA256
//!    over the same secret used for tenant session cookies) with
//!    `op_id`, `slug`, `exp`, and a single-use `jti`.
//! 2. Redirects the browser to
//!    `<scheme>://<sub>.<apex>:<port><handoff_url>?token=<signed>`.
//!    No cookie is set on the operator-console origin.
//! 3. The tenant admin handles the handoff URL: validates signature,
//!    expiry, slug binding, and that the `jti` hasn't been redeemed
//!    before. On success it mints the regular impersonation
//!    `rustango_tenant_session` cookie HOST-SCOPED (no `Domain=`)
//!    and 302s the browser to the admin index. Host-scoped cookies
//!    are accepted even on the public-suffix `localhost` TLD.
//!
//! ## Security properties
//!
//! Equivalent to the cookie-domain path:
//! - **Tampering rejected** — HMAC-SHA256 over the payload.
//! - **Cross-tenant replay rejected** — `slug` is part of the signed
//!   payload AND verified against the resolved tenant on redemption.
//! - **Single-use** — `jti` is recorded in [`JtiBlacklist`] on
//!   redemption; second use returns [`HandoffError::AlreadyUsed`].
//! - **Short-TTL** — handoff tokens default to 60 seconds (the
//!   resulting cookie has the regular impersonation TTL, default
//!   1 hour).
//!
//! Browser-history risk (the URL appears in history with the token)
//! is bounded by single-use enforcement + the 60s TTL. To prevent
//! the token from leaking via `Referer` headers to third-party
//! resources loaded by the admin page, the handoff response sets
//! `Referrer-Policy: no-referrer`.
//!
//! ## Multi-instance deployments
//!
//! [`JtiBlacklist`] is process-local. In a horizontally-scaled
//! deployment behind a load balancer, a redeemed token COULD be
//! replayed against a different process within the TTL window. The
//! mitigation today is the short TTL (one process likely catches
//! both within 60s if the LB hashes consistently); proper fix is a
//! shared store (Redis SETNX or a `rustango_used_jti` table). For
//! the dev-localhost scenario this addresses, single-instance is
//! the universal case.

use std::sync::{Arc, OnceLock};

use base64::Engine;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

pub use super::session::SessionSecret;
use super::session::{sign, SessionError};

/// Default lifetime — 60 seconds. The handoff is "click button →
/// browser redirects → tenant admin redeems"; nothing legitimate
/// takes longer than that, and a longer window only widens the
/// browser-history-leak attack surface.
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
/// Compact field names keep the URL short (operator console mints
/// these into 302 Location headers; some intermediaries cap header
/// size around 8KB). With i64 ids the encoded length is ~150 bytes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HandoffPayload {
    /// Operator id (`rustango_operators.id` from the registry pool).
    /// Threaded into the resulting impersonation cookie's `imp` field.
    pub op: i64,
    /// Tenant slug the handoff was minted for. Verified against the
    /// resolved org on redemption — defense against replay against
    /// a different tenant even if the URL leaks.
    pub slug: String,
    /// Expiry as Unix seconds.
    pub exp: i64,
    /// Single-use token identifier. Random 16-byte value, base64url
    /// encoded. Recorded in [`JtiBlacklist`] on redemption; second
    /// use rejects with [`HandoffError::AlreadyUsed`].
    pub jti: String,
}

impl HandoffPayload {
    /// Build a fresh payload with a random `jti` and `exp = now + ttl`.
    #[must_use]
    pub fn new(op_id: i64, slug: impl Into<String>, ttl_secs: i64) -> Self {
        let now = chrono::Utc::now().timestamp();
        // 16 random bytes is >2^64 entropy. v0.42 — sources from the
        // OS CSPRNG: a predictable JTI here lets an operator who
        // intercepted one handoff URL pre-mint the JTI for another,
        // bypassing the single-use blacklist.
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

/// Sign and encode a handoff payload as `<b64(json)>.<b64(hmac)>`.
/// Same wire format as the tenant session cookie so we share the
/// same `sign` primitive (no separate key needed).
#[must_use]
pub fn mint(secret: &SessionSecret, payload: &HandoffPayload) -> String {
    let json = serde_json::to_vec(payload).expect("payload serializes");
    let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json);
    let sig = sign(secret, payload_b64.as_bytes());
    let sig_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig);
    format!("{payload_b64}.{sig_b64}")
}

/// Verify, decode, and tenant-bind-check a handoff token. Does NOT
/// check the `jti` against [`JtiBlacklist`] — caller does that
/// separately so it can also `mark_used` atomically with the
/// cookie-mint step.
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

/// In-process single-use jti tracker. v0.47 — delegates to the
/// pluggable [`crate::jti_store::JtiStore`] trait. `JtiBlacklist`
/// itself stays as the consumer-facing type (back-compat with v0.46
/// imports) but the storage is now swappable: pass an
/// `Arc<dyn JtiStore>` to [`Self::with_store`] to share state across
/// processes (Redis / DB).
pub struct JtiBlacklist {
    store: Arc<dyn crate::jti_store::JtiStore>,
}

impl JtiBlacklist {
    /// Build a blacklist backed by an in-memory store. Suitable for
    /// single-instance dev / tests; multi-instance deployments
    /// should call [`Self::with_store`] with a shared store.
    fn new() -> Self {
        Self {
            store: Arc::new(crate::jti_store::InMemoryJtiStore::new()),
        }
    }

    /// Swap the underlying store. v0.47 — pass any
    /// `Arc<dyn JtiStore>` for multi-instance correctness.
    #[must_use]
    pub fn with_store(store: Arc<dyn crate::jti_store::JtiStore>) -> Self {
        Self { store }
    }

    /// Process-wide singleton. The crate has no DI surface for this,
    /// and the practical use case is single-instance dev where a
    /// local map is correct. Multi-instance deployments construct
    /// their own `JtiBlacklist::with_store(...)` and pass it into
    /// the redeem path explicitly.
    pub fn shared() -> &'static Self {
        static INSTANCE: OnceLock<JtiBlacklist> = OnceLock::new();
        INSTANCE.get_or_init(Self::new)
    }

    /// Returns `true` if the jti was previously marked used.
    pub fn is_used(&self, jti: &str) -> bool {
        self.store.is_used(jti)
    }

    /// Atomically check + record. Returns `Err(AlreadyUsed)` if the
    /// jti is in the store; otherwise inserts `(jti, exp)` and
    /// returns `Ok(())`. Pruning of expired entries is the store's
    /// responsibility — the in-memory impl does it on every call.
    pub fn mark_used(&self, jti: &str, exp: i64) -> Result<(), HandoffError> {
        if self.store.mark_used(jti, exp) {
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
        // Flip a byte in the middle of the signature segment (i.e. after
        // the `.` separator), not at `bytes.len() - 1`. The 43-char
        // no-pad URL-safe base64 encoding of a 32-byte HMAC only allows
        // specific trailing characters (the 4 unused bits at the tail
        // must encode as zero), so flipping the very last byte can land
        // on an invalid trailing char and trigger a base64 decode error
        // → `Malformed` instead of the `BadSignature` we want to assert.
        // A mid-signature byte is always-valid base64 and forces the
        // `ct_eq` mismatch path.
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

    #[test]
    fn jti_blacklist_first_use_succeeds_second_fails() {
        let bl = JtiBlacklist::new();
        let jti = "abc123";
        let exp = chrono::Utc::now().timestamp() + 60;
        bl.mark_used(jti, exp).unwrap();
        assert!(bl.is_used(jti));
        assert_eq!(
            bl.mark_used(jti, exp).unwrap_err(),
            HandoffError::AlreadyUsed
        );
    }

    #[test]
    fn jti_blacklist_with_store_delegates_to_swapped_backend() {
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
        bl_a.mark_used(jti, exp).unwrap();
        assert!(
            bl_b.is_used(jti),
            "second handle on the shared store must see the mark"
        );
        assert_eq!(
            bl_b.mark_used(jti, exp).unwrap_err(),
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
