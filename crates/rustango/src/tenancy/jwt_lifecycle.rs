//! Full JWT lifecycle — access + refresh tokens with revocation.
//!
//! Builds on [`super::auth_backends::JwtBackend`] (which only verifies a
//! single token type) by adding:
//!
//! - **Token pairs**: short-lived access + long-lived refresh
//! - **Token type claim** (`"typ": "access"` or `"typ": "refresh"`)
//! - **JWT ID** (`"jti"`) for individual token revocation
//! - **Sliding refresh**: each `refresh()` issues a new pair (rotates jti)
//! - **In-memory blacklist** for revocations (replace with cache-backed storage in production)
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::tenancy::jwt_lifecycle::JwtLifecycle;
//!
//! let jwt = JwtLifecycle::new(b"my-signing-key".to_vec());
//!
//! // Login: issue both tokens
//! let pair = jwt.issue_pair(user_id);
//! // Send pair.access to the client (short TTL, in Authorization header)
//! // Send pair.refresh to the client (long TTL, in HttpOnly cookie or secure storage)
//!
//! // Authenticated request:
//! match jwt.verify_access(&access_token) {
//!     Some(claims) => { /* claims.sub is the user id */ }
//!     None => { /* 401 */ }
//! }
//!
//! // Refresh endpoint:
//! match jwt.refresh(&refresh_token) {
//!     Some(new_pair) => { /* return new_pair to client */ }
//!     None => { /* 401 — refresh expired or revoked */ }
//! }
//!
//! // Logout:
//! jwt.revoke(&access_token);   // blacklist the access JTI
//! jwt.revoke(&refresh_token);  // blacklist the refresh JTI
//! ```

use std::collections::HashMap;
use std::sync::RwLock;

use base64::Engine;
use rand::Rng;
use subtle::ConstantTimeEq;

/// One issued access+refresh pair.
#[derive(Debug, Clone)]
pub struct JwtTokenPair {
    pub access: String,
    pub refresh: String,
}

/// Claims extracted from a verified token.
#[derive(Debug, Clone)]
pub struct JwtClaims {
    /// Subject — the user id.
    pub sub: i64,
    /// Expiry — unix seconds.
    pub exp: i64,
    /// JWT ID — unique per-token identifier (used for revocation).
    pub jti: String,
    /// Token type — `"access"` or `"refresh"`.
    pub typ: String,
}

const ACCESS_TYP: &str = "access";
const REFRESH_TYP: &str = "refresh";

/// Default access token TTL — 15 minutes.
pub const DEFAULT_ACCESS_TTL_SECS: i64 = 900;
/// Default refresh token TTL — 7 days.
pub const DEFAULT_REFRESH_TTL_SECS: i64 = 7 * 24 * 3600;

/// JWT manager with access + refresh tokens and an in-memory blacklist.
pub struct JwtLifecycle {
    secret: Vec<u8>,
    pub access_ttl_secs: i64,
    pub refresh_ttl_secs: i64,
    blacklist: RwLock<HashMap<String, i64>>, // jti -> expiry timestamp (auto-pruned)
}

impl JwtLifecycle {
    /// Build a new lifecycle with default TTLs (15 min access / 7 day refresh).
    #[must_use]
    pub fn new(secret: Vec<u8>) -> Self {
        Self {
            secret,
            access_ttl_secs: DEFAULT_ACCESS_TTL_SECS,
            refresh_ttl_secs: DEFAULT_REFRESH_TTL_SECS,
            blacklist: RwLock::new(HashMap::new()),
        }
    }

    /// Override the access token TTL (in seconds).
    #[must_use]
    pub fn with_access_ttl(mut self, secs: i64) -> Self {
        self.access_ttl_secs = secs;
        self
    }

    /// Override the refresh token TTL (in seconds).
    #[must_use]
    pub fn with_refresh_ttl(mut self, secs: i64) -> Self {
        self.refresh_ttl_secs = secs;
        self
    }

    /// Issue an access+refresh pair for `user_id`.
    pub fn issue_pair(&self, user_id: i64) -> JwtTokenPair {
        let access = self.issue_token(user_id, ACCESS_TYP, self.access_ttl_secs);
        let refresh = self.issue_token(user_id, REFRESH_TYP, self.refresh_ttl_secs);
        JwtTokenPair { access, refresh }
    }

    /// Verify an access token. Returns the claims on success, `None` if
    /// invalid, expired, blacklisted, or wrong type.
    #[must_use]
    pub fn verify_access(&self, token: &str) -> Option<JwtClaims> {
        let claims = self.verify_token(token)?;
        if claims.typ != ACCESS_TYP {
            return None;
        }
        Some(claims)
    }

    /// Verify a refresh token. Returns the claims on success, `None` if
    /// invalid, expired, blacklisted, or wrong type.
    #[must_use]
    pub fn verify_refresh(&self, token: &str) -> Option<JwtClaims> {
        let claims = self.verify_token(token)?;
        if claims.typ != REFRESH_TYP {
            return None;
        }
        Some(claims)
    }

    /// Exchange a refresh token for a new access+refresh pair (sliding
    /// expiry). The old refresh token's JTI is blacklisted to prevent reuse.
    ///
    /// Returns `None` if the refresh token is invalid, expired, or already
    /// blacklisted.
    pub fn refresh(&self, refresh_token: &str) -> Option<JwtTokenPair> {
        let claims = self.verify_refresh(refresh_token)?;
        // Rotate: blacklist the old refresh, issue a new pair.
        self.blacklist_jti(&claims.jti, claims.exp);
        Some(self.issue_pair(claims.sub))
    }

    /// Revoke a token by adding its JTI to the blacklist. Subsequent
    /// `verify_*` calls for this token will return `None` until the
    /// token's natural expiry passes.
    pub fn revoke(&self, token: &str) -> bool {
        let Some(claims) = self.decode_unchecked(token) else {
            return false;
        };
        self.blacklist_jti(&claims.jti, claims.exp);
        true
    }

    /// Number of currently blacklisted JTIs (for tests / monitoring).
    #[must_use]
    pub fn blacklist_size(&self) -> usize {
        self.prune_blacklist();
        self.blacklist.read().expect("blacklist poisoned").len()
    }

    // ------------------------------------------------------------------ internals

    fn issue_token(&self, user_id: i64, typ: &str, ttl_secs: i64) -> String {
        let exp = chrono::Utc::now().timestamp() + ttl_secs;
        let jti = random_jti();
        let payload = serde_json::json!({
            "sub": user_id,
            "exp": exp,
            "jti": jti,
            "typ": typ,
        });
        let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&payload).unwrap_or_default());
        let sig = self.sign(payload_b64.as_bytes());
        let sig_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig);
        format!("{payload_b64}.{sig_b64}")
    }

    fn verify_token(&self, token: &str) -> Option<JwtClaims> {
        let claims = self.decode_unchecked(token)?;
        // Expiry
        if chrono::Utc::now().timestamp() >= claims.exp {
            return None;
        }
        // Blacklist
        if self.is_blacklisted(&claims.jti) {
            return None;
        }
        Some(claims)
    }

    /// Decode + verify signature only — does NOT check expiry or blacklist.
    /// Used for `revoke` so we can blacklist even an already-expired token's JTI.
    fn decode_unchecked(&self, token: &str) -> Option<JwtClaims> {
        let (payload_b64, sig_b64) = token.split_once('.')?;
        let expected = self.sign(payload_b64.as_bytes());
        let provided = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(sig_b64)
            .ok()?;
        if expected.ct_eq(&provided[..]).unwrap_u8() == 0 {
            return None;
        }
        let payload_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload_b64)
            .ok()?;
        let payload: serde_json::Value = serde_json::from_slice(&payload_bytes).ok()?;
        Some(JwtClaims {
            sub: payload.get("sub")?.as_i64()?,
            exp: payload.get("exp")?.as_i64()?,
            jti: payload.get("jti")?.as_str()?.to_owned(),
            typ: payload.get("typ")?.as_str()?.to_owned(),
        })
    }

    fn sign(&self, msg: &[u8]) -> Vec<u8> {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        let mut mac = <Hmac<Sha256>>::new_from_slice(&self.secret).expect("HMAC accepts any key");
        mac.update(msg);
        mac.finalize().into_bytes().to_vec()
    }

    fn blacklist_jti(&self, jti: &str, expires_at: i64) {
        self.blacklist
            .write()
            .expect("blacklist poisoned")
            .insert(jti.to_owned(), expires_at);
        self.prune_blacklist();
    }

    fn is_blacklisted(&self, jti: &str) -> bool {
        let now = chrono::Utc::now().timestamp();
        let bl = self.blacklist.read().expect("blacklist poisoned");
        bl.get(jti).map_or(false, |&exp| exp > now)
    }

    /// Remove blacklist entries past their expiry — keeps the map bounded.
    fn prune_blacklist(&self) {
        let now = chrono::Utc::now().timestamp();
        let mut bl = self.blacklist.write().expect("blacklist poisoned");
        bl.retain(|_, &mut exp| exp > now);
    }
}

fn random_jti() -> String {
    let bytes: [u8; 16] = rand::thread_rng().gen();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt() -> JwtLifecycle {
        JwtLifecycle::new(b"test-secret".to_vec())
    }

    #[test]
    fn issue_and_verify_access() {
        let j = jwt();
        let pair = j.issue_pair(42);
        let claims = j.verify_access(&pair.access).expect("access verifies");
        assert_eq!(claims.sub, 42);
        assert_eq!(claims.typ, "access");
        assert!(!claims.jti.is_empty());
    }

    #[test]
    fn issue_and_verify_refresh() {
        let j = jwt();
        let pair = j.issue_pair(42);
        let claims = j.verify_refresh(&pair.refresh).expect("refresh verifies");
        assert_eq!(claims.sub, 42);
        assert_eq!(claims.typ, "refresh");
    }

    #[test]
    fn access_token_rejected_as_refresh() {
        let j = jwt();
        let pair = j.issue_pair(1);
        assert!(j.verify_refresh(&pair.access).is_none());
    }

    #[test]
    fn refresh_token_rejected_as_access() {
        let j = jwt();
        let pair = j.issue_pair(1);
        assert!(j.verify_access(&pair.refresh).is_none());
    }

    #[test]
    fn refresh_returns_new_pair() {
        let j = jwt();
        let pair = j.issue_pair(7);
        let new_pair = j.refresh(&pair.refresh).expect("refresh succeeds");
        assert_ne!(pair.access, new_pair.access);
        assert_ne!(pair.refresh, new_pair.refresh);

        let claims = j.verify_access(&new_pair.access).unwrap();
        assert_eq!(claims.sub, 7);
    }

    #[test]
    fn refresh_blacklists_old_refresh_token() {
        let j = jwt();
        let pair = j.issue_pair(7);
        let _new = j.refresh(&pair.refresh).unwrap();
        // The old refresh token can no longer be used.
        assert!(j.refresh(&pair.refresh).is_none());
        assert!(j.verify_refresh(&pair.refresh).is_none());
    }

    #[test]
    fn revoke_invalidates_access_token() {
        let j = jwt();
        let pair = j.issue_pair(1);
        assert!(j.verify_access(&pair.access).is_some());
        assert!(j.revoke(&pair.access));
        assert!(j.verify_access(&pair.access).is_none());
    }

    #[test]
    fn revoke_invalid_token_returns_false() {
        let j = jwt();
        assert!(!j.revoke("not-a-valid-token"));
    }

    #[test]
    fn tampered_signature_fails_verification() {
        let j = jwt();
        let pair = j.issue_pair(1);
        let mut bytes = pair.access.into_bytes();
        // Flip a byte in the signature
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        let tampered = String::from_utf8(bytes).unwrap();
        assert!(j.verify_access(&tampered).is_none());
    }

    #[test]
    fn wrong_secret_fails_verification() {
        let j1 = jwt();
        let j2 = JwtLifecycle::new(b"different-secret".to_vec());
        let pair = j1.issue_pair(5);
        assert!(j2.verify_access(&pair.access).is_none());
    }

    #[test]
    fn unique_jti_per_issuance() {
        let j = jwt();
        let pair1 = j.issue_pair(1);
        let pair2 = j.issue_pair(1);
        let c1 = j.verify_access(&pair1.access).unwrap();
        let c2 = j.verify_access(&pair2.access).unwrap();
        assert_ne!(c1.jti, c2.jti);
    }

    #[test]
    fn custom_ttls() {
        let j = JwtLifecycle::new(b"k".to_vec())
            .with_access_ttl(60)
            .with_refresh_ttl(3600);
        assert_eq!(j.access_ttl_secs, 60);
        assert_eq!(j.refresh_ttl_secs, 3600);
    }
}
