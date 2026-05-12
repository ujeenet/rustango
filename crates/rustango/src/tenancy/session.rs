//! HMAC-SHA256 signed session cookies for the operator console.
//!
//! Stateless — the cookie carries the principal (`operator_id` + `exp`)
//! and a signature. Server validates the signature on every request;
//! no server-side session table for v1. Trade-offs:
//!
//! * **No revocation** — once issued, a cookie is valid until `exp`.
//!   Operator deletion / password change doesn't invalidate live
//!   cookies. Acceptable for v1; v2 can add a short-lived cookie +
//!   server-side revocation list.
//! * **Secret rotation invalidates all cookies** — a server restart
//!   with auto-generated secret signs everyone out. With
//!   `RUSTANGO_SESSION_SECRET` set in env, sessions survive restarts.
//!
//! Cookie format:
//!
//! ```text
//! Cookie: rustango_op_session=<base64(payload)>.<base64(hmac_sha256)>
//! payload  = JSON {"oid": <i64>, "exp": <unix_seconds>}
//! signature = HMAC-SHA256(secret, payload_base64) [first 32 bytes]
//! ```

use base64::Engine;
use hmac::{Hmac, Mac};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;

/// Default cookie name. Visible in browser devtools — namespaced so
/// it doesn't collide with tenant cookies.
pub const COOKIE_NAME: &str = "rustango_op_session";

/// Default session lifetime (7 days). Configurable later; locked
/// for v1.
pub const SESSION_TTL_SECS: i64 = 7 * 24 * 60 * 60;

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("session cookie is malformed")]
    Malformed,
    #[error("session signature mismatch")]
    BadSignature,
    #[error("session expired")]
    Expired,
    /// Cookie's tenant slug doesn't match the resolved tenant — used
    /// by the tenant console to defend against cross-tenant cookie
    /// replay (cookie issued for `acme` should not authenticate at
    /// `globex`'s subdomain).
    #[error("session is bound to a different tenant")]
    WrongTenant,
}

/// Principal payload carried inside the cookie. Compact field names
/// to keep the cookie short (browsers truncate aggressively).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionPayload {
    /// Operator id (from `rustango_operators.id`).
    pub oid: i64,
    /// Expiry as Unix seconds.
    pub exp: i64,
    /// Issued-at as Unix seconds (v0.28.4, #77 follow-up).
    /// Compared to `rustango_operators.password_changed_at` —
    /// sessions issued before the latest password rotation are
    /// rejected. `0` for cookies minted on pre-0.28.4 servers
    /// (`#[serde(default)]` keeps them parseable; the comparison
    /// treats `0` as "issued at the dawn of time" so they're
    /// invalidated by any password change).
    #[serde(default)]
    pub iat: i64,
}

impl SessionPayload {
    #[must_use]
    pub fn new(operator_id: i64, ttl_secs: i64) -> Self {
        let now = chrono::Utc::now().timestamp();
        Self {
            oid: operator_id,
            exp: now + ttl_secs,
            iat: now,
        }
    }

    fn is_expired(&self) -> bool {
        chrono::Utc::now().timestamp() >= self.exp
    }
}

/// Server-held signing key. Wrap `Vec<u8>` so callers can't
/// accidentally print it. `Clone` is opt-in so the same secret can
/// be handed to both the operator console and the tenant admin —
/// they use different cookie names + payload shapes, so sharing
/// the key is safe.
#[derive(Clone)]
pub struct SessionSecret(Vec<u8>);

/// Error returned by [`SessionSecret::try_from_env`] when the
/// `RUSTANGO_SESSION_SECRET` env var is set but the value isn't a
/// valid signing key. Used by production boot paths that prefer to
/// fail loudly over silently downgrading to an ephemeral random key.
#[derive(Debug)]
pub enum SessionSecretError {
    /// The env var didn't decode as base64.
    BadBase64 { cause: String },
    /// Decoded successfully but the resulting key is fewer than 32
    /// bytes — too short for HMAC-SHA256.
    TooShort { actual: usize },
}

impl core::fmt::Display for SessionSecretError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BadBase64 { cause } => write!(
                f,
                "RUSTANGO_SESSION_SECRET is not valid base64: {cause} \
                 (generate one with: openssl rand -base64 32)"
            ),
            Self::TooShort { actual } => write!(
                f,
                "RUSTANGO_SESSION_SECRET decoded to {actual} bytes; need at least 32 \
                 (generate one with: openssl rand -base64 32)"
            ),
        }
    }
}

impl std::error::Error for SessionSecretError {}

impl SessionSecret {
    /// Read the secret from `RUSTANGO_SESSION_SECRET` (base64-encoded
    /// 32+ bytes). Falls back to a randomly generated secret with a
    /// `tracing::warn` when the var is *unset* — sessions are then
    /// invalidated on every server restart.
    ///
    /// v0.13.2 — when the var IS set but unparseable (bad base64,
    /// fewer than 32 bytes), we now ALSO print a loud
    /// `eprintln!` to stderr in addition to the tracing::warn.
    /// Operators who set the var and forget to run it through
    /// `base64` quietly lost session persistence on every redeploy
    /// before this fix, with the only signal being a structured
    /// log line buried in the boot output. The eprintln! makes the
    /// failure mode loud at the boot console.
    #[must_use]
    pub fn from_env_or_random() -> Self {
        if let Ok(raw) = std::env::var("RUSTANGO_SESSION_SECRET") {
            match base64::engine::general_purpose::STANDARD.decode(raw.trim()) {
                Ok(bytes) if bytes.len() >= 32 => return Self(bytes),
                Ok(bytes) => {
                    tracing::warn!(
                        target: "crate::tenancy",
                        actual_len = bytes.len(),
                        "RUSTANGO_SESSION_SECRET decoded to fewer than 32 bytes — falling back to random",
                    );
                    eprintln!(
                        "\x1b[33;1mwarning:\x1b[0m RUSTANGO_SESSION_SECRET is set but \
                         decoded to {} bytes (need ≥ 32). Using a random key. \
                         Sessions will NOT survive a server restart. \
                         Generate one with: \
                         openssl rand -base64 32",
                        bytes.len()
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        target: "crate::tenancy",
                        error = %e,
                        "RUSTANGO_SESSION_SECRET is not valid base64 — falling back to random",
                    );
                    eprintln!(
                        "\x1b[33;1mwarning:\x1b[0m RUSTANGO_SESSION_SECRET is set but \
                         is not valid base64 ({}). Using a random key. \
                         Sessions will NOT survive a server restart. \
                         Generate one with: \
                         openssl rand -base64 32",
                        e
                    );
                }
            }
        } else {
            tracing::warn!(
                target: "crate::tenancy",
                "RUSTANGO_SESSION_SECRET not set — generating random key (sessions \
                 will not survive server restarts; set the env var for production)",
            );
        }
        let mut buf = vec![0u8; 32];
        // v0.30.12 — use OsRng directly (cryptographic secret).
        // Consistent with src/forms/csrf.rs + src/passwords.rs.
        OsRng.fill_bytes(&mut buf);
        Self(buf)
    }

    /// Dev-friendly variant of [`Self::from_env_or_random`] that
    /// persists the generated key to disk so sessions survive
    /// server restarts even without `RUSTANGO_SESSION_SECRET` set.
    ///
    /// Resolution order:
    /// 1. `RUSTANGO_SESSION_SECRET` env var (same shape as
    ///    [`Self::from_env_or_random`]) — production path.
    /// 2. Read `disk_path` if it exists and contains ≥ 32 bytes.
    /// 3. Generate a random key, atomically write it to
    ///    `disk_path` (creating parent directories as needed),
    ///    and return it.
    /// 4. If the write fails (read-only filesystem, no perms),
    ///    fall back to ephemeral random + a `tracing::warn!`.
    ///
    /// Used by the runserver boot path so dev `cargo run` cycles
    /// don't sign every operator out on every reload (#69).
    /// Production deployments should still set
    /// `RUSTANGO_SESSION_SECRET` so the secret lives in env/secret-
    /// manager rather than the filesystem.
    #[must_use]
    pub fn from_env_or_disk(disk_path: &std::path::Path) -> Self {
        // Env var path: identical to `from_env_or_random`'s first
        // branch. Duplicating here rather than calling the existing
        // method because that method falls through to ephemeral
        // random — we want disk fallback in between.
        if let Ok(raw) = std::env::var("RUSTANGO_SESSION_SECRET") {
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(raw.trim()) {
                if bytes.len() >= 32 {
                    return Self(bytes);
                }
            }
            // Bad env var — fall through to disk/random with the
            // same loud warnings as `from_env_or_random` would emit.
            // (We don't re-emit them here to keep the boot log
            // tidy — the env-var path is exercised by
            // `from_env_or_random` if anyone wants the warnings.)
        }
        // Disk path: read existing key if present.
        if let Ok(bytes) = std::fs::read(disk_path) {
            if bytes.len() >= 32 {
                // v0.31.1 (#7): bumped from `debug!` to `info!` so the
                // happy-path "your session keys are persistent" message
                // shows up in default-log boots and operators can tell
                // at a glance whether the secret is freshly generated
                // or carried over from a prior run.
                tracing::info!(
                    target: "crate::tenancy::session",
                    path = %disk_path.display(),
                    "loaded persisted session secret from disk",
                );
                return Self(bytes);
            }
        }
        // Generate a fresh key and try to persist it.
        let mut buf = vec![0u8; 32];
        // v0.30.12 — use OsRng directly (cryptographic secret).
        // Consistent with src/forms/csrf.rs + src/passwords.rs.
        OsRng.fill_bytes(&mut buf);
        if let Some(parent) = disk_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Atomic write: tmp file → rename. Avoids half-written
        // keys if the process is killed mid-write.
        let tmp_path = disk_path.with_extension("tmp");
        match std::fs::write(&tmp_path, &buf).and_then(|_| std::fs::rename(&tmp_path, disk_path)) {
            Ok(()) => {
                // v0.31.1 (#7): clarify the "(dev fallback)" wording —
                // the message previously made it look like the secret
                // was being regenerated on every boot. It only fires
                // when no env var AND no on-disk key were found, i.e.
                // the very first boot.
                tracing::info!(
                    target: "crate::tenancy::session",
                    path = %disk_path.display(),
                    "generated new session secret and persisted to disk \
                     (set RUSTANGO_SESSION_SECRET to override; this message \
                     only fires on first boot)",
                );
            }
            Err(e) => {
                tracing::warn!(
                    target: "crate::tenancy::session",
                    path = %disk_path.display(),
                    error = %e,
                    "could not persist session secret to disk — using ephemeral random key (sessions will not survive restart)",
                );
                let _ = std::fs::remove_file(&tmp_path);
            }
        }
        Self(buf)
    }

    /// Strict variant of [`Self::from_env_or_random`]: returns
    /// `Err(...)` when the env var is *set but unparseable* or
    /// *too short*. Use this from production boot paths where a
    /// malformed secret should fail loudly instead of silently
    /// downgrading to a random ephemeral key.
    ///
    /// Behaviour:
    /// * Var set + ≥ 32 bytes after base64 decode → `Ok(SessionSecret)`.
    /// * Var set but bad base64 / too short → `Err(SessionSecretError)`.
    /// * Var unset → `Ok(random key)` with the same warn-and-go path
    ///   as `from_env_or_random` (this is the dev/test default and
    ///   is fine in those contexts).
    ///
    /// # Errors
    /// `SessionSecretError::BadBase64` when decode fails;
    /// `SessionSecretError::TooShort` when the decoded bytes are
    /// fewer than 32.
    pub fn try_from_env() -> Result<Self, SessionSecretError> {
        if let Ok(raw) = std::env::var("RUSTANGO_SESSION_SECRET") {
            return match base64::engine::general_purpose::STANDARD.decode(raw.trim()) {
                Ok(bytes) if bytes.len() >= 32 => Ok(Self(bytes)),
                Ok(bytes) => Err(SessionSecretError::TooShort {
                    actual: bytes.len(),
                }),
                Err(e) => Err(SessionSecretError::BadBase64 {
                    cause: e.to_string(),
                }),
            };
        }
        // Var unset is the dev/test path; fall back to random.
        tracing::warn!(
            target: "crate::tenancy",
            "RUSTANGO_SESSION_SECRET not set — generating random key (sessions \
             will not survive server restarts; set the env var for production)",
        );
        let mut buf = vec![0u8; 32];
        // v0.30.12 — use OsRng directly (cryptographic secret).
        // Consistent with src/forms/csrf.rs + src/passwords.rs.
        OsRng.fill_bytes(&mut buf);
        Ok(Self(buf))
    }

    /// Construct from raw bytes — useful for tests.
    #[must_use]
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// Raw key material — only callers inside the tenancy crate should
    /// reach into this; external callers go through encode/decode.
    pub(crate) fn key(&self) -> &[u8] {
        &self.0
    }
}

/// Serialize and sign a payload into a cookie value.
#[must_use]
pub fn encode(secret: &SessionSecret, payload: &SessionPayload) -> String {
    let json = serde_json::to_vec(payload).expect("payload serializes");
    let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json);
    let sig = sign(secret, payload_b64.as_bytes());
    let sig_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig);
    format!("{payload_b64}.{sig_b64}")
}

/// Verify and deserialize a cookie value.
///
/// # Errors
/// Returns [`SessionError::Malformed`] for missing/bad-base64 parts,
/// [`SessionError::BadSignature`] when HMAC doesn't match (covers
/// secret rotation + tampering), [`SessionError::Expired`] when the
/// payload's `exp` is in the past.
pub fn decode(secret: &SessionSecret, value: &str) -> Result<SessionPayload, SessionError> {
    let (payload_b64, sig_b64) = value.split_once('.').ok_or(SessionError::Malformed)?;
    let expected = sign(secret, payload_b64.as_bytes());
    let provided = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(sig_b64)
        .map_err(|_| SessionError::Malformed)?;
    if expected.ct_eq(&provided[..]).unwrap_u8() == 0 {
        return Err(SessionError::BadSignature);
    }
    let payload_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|_| SessionError::Malformed)?;
    let payload: SessionPayload =
        serde_json::from_slice(&payload_bytes).map_err(|_| SessionError::Malformed)?;
    if payload.is_expired() {
        return Err(SessionError::Expired);
    }
    Ok(payload)
}

/// HMAC-SHA256(secret, msg), truncated to 32 bytes (the full SHA256
/// length). Crate-visible so the tenant console can share the same
/// MAC primitive without duplicating crypto.
pub(crate) fn sign(secret: &SessionSecret, msg: &[u8]) -> [u8; 32] {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.key()).expect("HMAC accepts any key length");
    mac.update(msg);
    let bytes = mac.finalize().into_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes[..32]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_valid_payload() {
        let secret = SessionSecret::from_bytes(b"a-test-secret-thirty-two-bytes-x".to_vec());
        let payload = SessionPayload::new(42, 3600);
        let cookie = encode(&secret, &payload);
        let back = decode(&secret, &cookie).unwrap();
        assert_eq!(back, payload);
    }

    #[test]
    fn rejects_tampered_payload() {
        let secret = SessionSecret::from_bytes(b"a-test-secret-thirty-two-bytes-x".to_vec());
        let payload = SessionPayload::new(42, 3600);
        let cookie = encode(&secret, &payload);
        // Replace payload but keep signature → BadSignature.
        let (_, sig) = cookie.split_once('.').unwrap();
        let evil_payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"oid":999,"exp":9999999999}"#);
        let tampered = format!("{evil_payload}.{sig}");
        let err = decode(&secret, &tampered).unwrap_err();
        assert!(matches!(err, SessionError::BadSignature));
    }

    #[test]
    fn rejects_wrong_secret() {
        let s1 = SessionSecret::from_bytes(b"first-test-secret-thirty-2-bytes".to_vec());
        let s2 = SessionSecret::from_bytes(b"second-test-secret-thirty2-bytes".to_vec());
        let cookie = encode(&s1, &SessionPayload::new(1, 3600));
        let err = decode(&s2, &cookie).unwrap_err();
        assert!(matches!(err, SessionError::BadSignature));
    }

    #[test]
    fn rejects_expired() {
        let secret = SessionSecret::from_bytes(b"a-test-secret-thirty-two-bytes-x".to_vec());
        let payload = SessionPayload::new(1, -10);
        let cookie = encode(&secret, &payload);
        let err = decode(&secret, &cookie).unwrap_err();
        assert!(matches!(err, SessionError::Expired));
    }

    #[test]
    fn rejects_malformed_no_dot() {
        let secret = SessionSecret::from_bytes(b"a-test-secret-thirty-two-bytes-x".to_vec());
        let err = decode(&secret, "not-a-cookie").unwrap_err();
        assert!(matches!(err, SessionError::Malformed));
    }
}
