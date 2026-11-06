//! The one codec for signed, expiring tokens: `b64(json).b64(mac)`, where
//! `mac = HMAC-SHA256(key, tag "." b64(json))`.
//!
//! Each token kind passes its own `tag`, so a token sealed for one purpose
//! never opens as another under the same key. Callers keep only their own
//! extra checks (tenant binding, audience, kind).

use std::marker::PhantomData;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde::de::DeserializeOwned;
use serde::Serialize;

/// A payload that knows when it stops being valid.
pub trait Expiring {
    /// Expired at `now`, Unix seconds.
    fn is_expired_at(&self, now: i64) -> bool;

    fn is_expired(&self) -> bool {
        self.is_expired_at(crate::clock::unix_now_for_expiry())
    }
}

/// Why [`SealedToken::open`] refused a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SealError {
    #[error("sealed token is malformed")]
    Malformed,
    #[error("sealed token signature mismatch")]
    BadSignature,
    #[error("sealed token expired")]
    Expired,
}

/// Seal and open `T` under a key and a tag.
pub struct SealedToken<T>(PhantomData<fn() -> T>);

impl<T: Serialize + DeserializeOwned + Expiring> SealedToken<T> {
    /// `payload` as a signed token for `tag`.
    #[must_use]
    pub fn seal(key: &[u8], tag: &[u8], payload: &T) -> String {
        let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(payload).unwrap_or_default());
        let sig = URL_SAFE_NO_PAD.encode(mac(key, tag, &body));
        format!("{body}.{sig}")
    }

    /// The payload behind `value`, if signed under `key` for `tag` and not expired.
    ///
    /// # Errors
    /// [`SealError`]: a bad shape, a signature mismatch, or an expired payload.
    pub fn open(key: &[u8], tag: &[u8], value: &str) -> Result<T, SealError> {
        Self::open_at(key, tag, value, crate::clock::unix_now_for_expiry())
    }

    /// [`Self::open`] with expiry judged at `now`, Unix seconds.
    ///
    /// # Errors
    /// As [`Self::open`].
    pub fn open_at(key: &[u8], tag: &[u8], value: &str, now: i64) -> Result<T, SealError> {
        let (body, sig) = value.split_once('.').ok_or(SealError::Malformed)?;
        let sig = URL_SAFE_NO_PAD
            .decode(sig)
            .map_err(|_| SealError::Malformed)?;
        if !crate::crypto::constant_time_compare(&mac(key, tag, body), &sig) {
            return Err(SealError::BadSignature);
        }
        let json = URL_SAFE_NO_PAD
            .decode(body)
            .map_err(|_| SealError::Malformed)?;
        let payload: T = serde_json::from_slice(&json).map_err(|_| SealError::Malformed)?;
        if payload.is_expired_at(now) {
            return Err(SealError::Expired);
        }
        Ok(payload)
    }
}

/// `HMAC(key, tag "." body)`: the tag and body cannot run into each other.
pub(crate) fn mac(key: &[u8], tag: &[u8], body: &str) -> Vec<u8> {
    crate::crypto::hmac_sha256(key, &[tag, b".", body.as_bytes()].concat())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    struct P {
        v: i64,
        exp: i64,
    }

    impl Expiring for P {
        fn is_expired_at(&self, now: i64) -> bool {
            now >= self.exp
        }
    }

    fn live() -> P {
        P {
            v: 7,
            exp: chrono::Utc::now().timestamp() + 60,
        }
    }

    /// Sessions and handoff tokens open through here: a broken clock refuses them.
    #[test]
    fn a_broken_clock_refuses_a_live_token() {
        let t = SealedToken::<P>::seal(b"k", b"a", &live());
        let refused = crate::clock::with_broken_clock(|| SealedToken::<P>::open(b"k", b"a", &t));
        assert_eq!(refused, Err(SealError::Expired));
        assert!(crate::clock::with_broken_clock(|| live().is_expired()));
    }

    #[test]
    fn round_trips_and_refuses_another_tag_key_or_tamper() {
        let t = SealedToken::<P>::seal(b"k", b"a", &live());
        assert_eq!(SealedToken::<P>::open(b"k", b"a", &t), Ok(live()));
        assert_eq!(
            SealedToken::<P>::open(b"k", b"b", &t),
            Err(SealError::BadSignature)
        );
        assert_eq!(
            SealedToken::<P>::open(b"j", b"a", &t),
            Err(SealError::BadSignature)
        );
        assert_eq!(
            SealedToken::<P>::open(b"k", b"a", "x"),
            Err(SealError::Malformed)
        );
        let old = P { v: 7, exp: 0 };
        let t = SealedToken::<P>::seal(b"k", b"a", &old);
        assert_eq!(
            SealedToken::<P>::open(b"k", b"a", &t),
            Err(SealError::Expired)
        );
    }
}
