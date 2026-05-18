//! Signed-cookie session primitives — HMAC-SHA256 key wrapper, sign,
//! and verify helpers shared across the framework.
//!
//! This module deliberately holds **only the crypto primitive + key
//! management**, never payload shape. Layers above (`tenancy::session`
//! for operator/tenant cookies, `admin::session` for the bare-admin
//! session cookie, …) define their own payload structs and call into
//! [`sign`] to produce the MAC. That way two layers can share one
//! signing key safely — they just need distinct cookie names + payload
//! shapes so neither layer accidentally decodes the other's cookie.
//!
//! Lives at the crate root (not under any feature flag) so the bare
//! `admin` module can use the same primitives even when the `tenancy`
//! feature is off — closes the duplication concern raised in #253.

use base64::Engine;
use hmac::{Hmac, Mac};
use rand::{rngs::OsRng, RngCore};
use sha2::Sha256;

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

/// Server-held signing key. Wrap `Vec<u8>` so callers can't
/// accidentally print it. `Clone` is opt-in so the same secret can
/// be shared across layers that use distinct cookie names + payload
/// shapes (e.g. tenancy operator + tenancy tenant + bare admin —
/// three layers, one key, three independent cookies).
#[derive(Clone)]
pub struct SessionSecret(Vec<u8>);

impl SessionSecret {
    /// Read the secret from `RUSTANGO_SESSION_SECRET` (base64-encoded
    /// 32+ bytes). Falls back to a randomly generated secret with a
    /// `tracing::warn` when the var is *unset* — sessions are then
    /// invalidated on every server restart.
    ///
    /// When the var IS set but unparseable (bad base64, fewer than
    /// 32 bytes), we ALSO print a loud `eprintln!` to stderr in
    /// addition to the tracing::warn (history: operators who set
    /// the var and forgot to run it through `base64` quietly lost
    /// session persistence on every redeploy).
    #[must_use]
    pub fn from_env_or_random() -> Self {
        if let Ok(raw) = std::env::var("RUSTANGO_SESSION_SECRET") {
            match base64::engine::general_purpose::STANDARD.decode(raw.trim()) {
                Ok(bytes) if bytes.len() >= 32 => return Self(bytes),
                Ok(bytes) => {
                    tracing::warn!(
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
                        error = %e,
                        "RUSTANGO_SESSION_SECRET is not valid base64 — falling back to random",
                    );
                    eprintln!(
                        "\x1b[33;1mwarning:\x1b[0m RUSTANGO_SESSION_SECRET is set but \
                         is not valid base64 ({e}). Using a random key. \
                         Sessions will NOT survive a server restart. \
                         Generate one with: \
                         openssl rand -base64 32",
                    );
                }
            }
        } else {
            tracing::warn!(
                "RUSTANGO_SESSION_SECRET not set — generating random key (sessions \
                 will not survive server restarts; set the env var for production)",
            );
        }
        let mut buf = vec![0u8; 32];
        OsRng.fill_bytes(&mut buf);
        Self(buf)
    }

    /// Dev-friendly variant of [`Self::from_env_or_random`] that
    /// persists the generated key to disk so sessions survive
    /// server restarts even without `RUSTANGO_SESSION_SECRET` set.
    ///
    /// Resolution order:
    /// 1. `RUSTANGO_SESSION_SECRET` env var — production path.
    /// 2. Read `disk_path` if it exists and contains ≥ 32 bytes.
    /// 3. Generate a random key, atomically write it to `disk_path`
    ///    (creating parent directories as needed), and return it.
    /// 4. If the write fails, fall back to ephemeral random + a
    ///    `tracing::warn!`.
    ///
    /// Used by the runserver boot path so dev `cargo run` cycles
    /// don't sign every operator out on every reload (#69).
    /// Production deployments should still set
    /// `RUSTANGO_SESSION_SECRET` so the secret lives in env / a
    /// secret-manager rather than the filesystem.
    #[must_use]
    pub fn from_env_or_disk(disk_path: &std::path::Path) -> Self {
        if let Ok(raw) = std::env::var("RUSTANGO_SESSION_SECRET") {
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(raw.trim()) {
                if bytes.len() >= 32 {
                    return Self(bytes);
                }
            }
            // Bad env var — fall through to disk/random. The loud
            // warnings live on `from_env_or_random` for callers that
            // want them.
        }
        if let Ok(bytes) = std::fs::read(disk_path) {
            if bytes.len() >= 32 {
                tracing::info!(
                    path = %disk_path.display(),
                    "loaded persisted session secret from disk",
                );
                return Self(bytes);
            }
        }
        let mut buf = vec![0u8; 32];
        OsRng.fill_bytes(&mut buf);
        if let Some(parent) = disk_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp_path = disk_path.with_extension("tmp");
        match std::fs::write(&tmp_path, &buf).and_then(|_| std::fs::rename(&tmp_path, disk_path)) {
            Ok(()) => {
                restrict_session_secret_perms(disk_path);
                tracing::info!(
                    path = %disk_path.display(),
                    "generated new session secret and persisted to disk \
                     (set RUSTANGO_SESSION_SECRET to override; this message \
                     only fires on first boot)",
                );
            }
            Err(e) => {
                tracing::warn!(
                    path = %disk_path.display(),
                    error = %e,
                    "could not persist session secret to disk — using ephemeral random key",
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
        tracing::warn!(
            "RUSTANGO_SESSION_SECRET not set — generating random key (sessions \
             will not survive server restarts; set the env var for production)",
        );
        let mut buf = vec![0u8; 32];
        OsRng.fill_bytes(&mut buf);
        Ok(Self(buf))
    }

    /// Construct from raw bytes — useful for tests + callers that
    /// load the key from a custom source.
    #[must_use]
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// Raw key material. `pub(crate)` so framework modules can sign
    /// or verify payloads, but external callers go through
    /// [`sign`] / their layer's own encode/decode helpers.
    pub(crate) fn key(&self) -> &[u8] {
        &self.0
    }
}

/// Restrict the persisted session-secret file to 0600 on Unix so
/// other users on the host can't read the signing key. Windows ACL
/// hardening is separate (DPAPI / restricted DACL).
#[cfg(unix)]
fn restrict_session_secret_perms(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(mut perms) = std::fs::metadata(path).map(|m| m.permissions()) {
        perms.set_mode(0o600);
        let _ = std::fs::set_permissions(path, perms);
    }
}

#[cfg(not(unix))]
fn restrict_session_secret_perms(_path: &std::path::Path) {
    // No portable equivalent.
}

/// HMAC-SHA256(secret, msg), truncated to 32 bytes. The shared MAC
/// primitive every signed-cookie layer in the framework calls into.
#[must_use]
pub fn sign(secret: &SessionSecret, msg: &[u8]) -> [u8; 32] {
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
    fn sign_is_deterministic_per_key() {
        let secret = SessionSecret::from_bytes(b"a-test-secret-thirty-two-bytes-x".to_vec());
        let a = sign(&secret, b"hello");
        let b = sign(&secret, b"hello");
        assert_eq!(a, b);
    }

    #[test]
    fn sign_differs_across_keys() {
        let s1 = SessionSecret::from_bytes(vec![1u8; 32]);
        let s2 = SessionSecret::from_bytes(vec![2u8; 32]);
        assert_ne!(sign(&s1, b"x"), sign(&s2, b"x"));
    }

    #[test]
    fn from_bytes_round_trip() {
        let secret = SessionSecret::from_bytes(vec![0xab; 40]);
        assert_eq!(secret.key().len(), 40);
    }
}
