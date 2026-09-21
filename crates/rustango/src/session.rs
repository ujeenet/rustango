//! Signed-cookie session primitives: the HMAC-SHA256 key wrapper plus
//! the shared sign and verify helpers.
//!
//! This module holds the key and the MAC, never the payload shape.
//! Layers above it, such as `tenancy::session` and `admin::session`,
//! define their own payload struct and call [`sign`]. Several layers
//! can then share one key, as long as each uses its own cookie name
//! and payload so it cannot decode another layer's cookie.
//!
//! It sits at the crate root with no feature gate, so `admin` gets the
//! same primitives when `tenancy` is off.
//!
//! [`sign`]: crate::session::sign

use base64::Engine;
use hmac::{Hmac, Mac};
use rand::{rngs::OsRng, RngCore};
use sha2::Sha256;

/// Why `RUSTANGO_SESSION_SECRET` could not be used as a signing key.
/// Production boot paths return this instead of quietly falling back
/// to a random key.
#[derive(Debug)]
pub enum SessionSecretError {
    /// The env var is not set. Only the strict
    /// [`SessionSecret::require_from_env`] reports this; the other
    /// loaders generate a random key instead.
    Missing,
    /// The value is not valid base64.
    BadBase64 { cause: String },
    /// It decoded, but to fewer than 32 bytes.
    TooShort { actual: usize },
}

impl core::fmt::Display for SessionSecretError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Missing => write!(
                f,
                "RUSTANGO_SESSION_SECRET is not set \
                 (generate one with: openssl rand -base64 32)"
            ),
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

/// Server-held signing key. It wraps the bytes so they cannot be
/// printed by accident. It is `Clone` so several cookie layers can
/// share one key, each with its own cookie name and payload.
#[derive(Clone)]
pub struct SessionSecret(Vec<u8>);

impl SessionSecret {
    /// Read the secret from `RUSTANGO_SESSION_SECRET`, which must be
    /// base64 for at least 32 bytes. If the var is unset, generate a
    /// random key and warn; sessions then end on every restart.
    ///
    /// If the var is set but unusable, also print to stderr, so a
    /// mistyped secret is visible at boot and not only in the logs.
    #[must_use]
    pub fn from_env_or_random() -> Self {
        if let Ok(raw) = std::env::var("RUSTANGO_SESSION_SECRET") {
            // One definition of "valid secret", shared with build_jwt
            // and `check --deploy`.
            match Self::from_b64(&raw) {
                Ok(secret) => return secret,
                Err(e) => {
                    tracing::warn!(error = %e, "RUSTANGO_SESSION_SECRET unusable — falling back to random");
                    eprintln!(
                        "\x1b[33;1mwarning:\x1b[0m {e}. Using a random key. \
                         Sessions will NOT survive a server restart.",
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

    /// Like [`Self::from_env_or_random`], but saves a generated key to
    /// disk so dev sessions survive a restart. Order:
    ///
    /// 1. `RUSTANGO_SESSION_SECRET`.
    /// 2. `disk_path`, if it holds at least 32 bytes.
    /// 3. A new random key, written to `disk_path`.
    /// 4. If that write fails, a random key for this process only.
    ///
    /// `runserver` uses this so a rebuild does not log everyone out.
    /// In production still set `RUSTANGO_SESSION_SECRET`, so the key
    /// comes from the environment or a secret manager, not a file.
    #[must_use]
    pub fn from_env_or_disk(disk_path: &std::path::Path) -> Self {
        if let Ok(raw) = std::env::var("RUSTANGO_SESSION_SECRET") {
            match Self::from_b64(&raw) {
                Ok(secret) => return secret,
                // Set but unusable. Say so loudly: otherwise a failed
                // key rotation looks like it worked.
                Err(e) => warn_unusable_secret(&e.to_string()),
            }
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
            return Self::from_b64(&raw);
        }
        tracing::warn!(
            "RUSTANGO_SESSION_SECRET not set — generating random key (sessions \
             will not survive server restarts; set the env var for production)",
        );
        let mut buf = vec![0u8; 32];
        OsRng.fill_bytes(&mut buf);
        Ok(Self(buf))
    }

    /// Strict variant for production boot. `RUSTANGO_SESSION_SECRET`
    /// must be set, valid base64, and at least 32 bytes. Unlike
    /// [`Self::try_from_env`], an unset var is
    /// [`SessionSecretError::Missing`] and not a silent random key: a
    /// per-process key breaks multi-instance deployments and hides the
    /// mistake. [`load_session_secret_for_tier`] uses it on prod.
    ///
    /// # Errors
    /// [`SessionSecretError::Missing`] when unset; `BadBase64` /
    /// `TooShort` per [`Self::try_from_env`].
    pub fn require_from_env() -> Result<Self, SessionSecretError> {
        match std::env::var("RUSTANGO_SESSION_SECRET") {
            Ok(raw) => Self::from_b64(&raw),
            Err(_) => Err(SessionSecretError::Missing),
        }
    }

    /// Decode and check a base64 secret. This is the one definition of
    /// what `RUSTANGO_SESSION_SECRET` means, and every reader in the
    /// crate calls it, so a value `check --deploy` accepts is a value
    /// the runtime accepts.
    ///
    /// The 32-byte floor is on the **decoded** bytes. Measuring the
    /// base64 text instead accepts a 32-character string that decodes
    /// to only 24 bytes.
    ///
    /// # Errors
    /// [`SessionSecretError::BadBase64`] or `TooShort` (measured on the
    /// **decoded** bytes, which is the key that actually signs).
    pub fn from_b64(raw: &str) -> Result<Self, SessionSecretError> {
        match base64::engine::general_purpose::STANDARD.decode(raw.trim()) {
            Ok(bytes) if bytes.len() >= 32 => Ok(Self(bytes)),
            Ok(bytes) => Err(SessionSecretError::TooShort {
                actual: bytes.len(),
            }),
            Err(e) => Err(SessionSecretError::BadBase64 {
                cause: e.to_string(),
            }),
        }
    }

    /// Build a key from raw bytes. Use it in tests, or when the key
    /// comes from Vault, KMS or another secret store.
    ///
    /// # Panics
    /// If `bytes` is shorter than 32. HMAC itself takes a key of any
    /// length, so nothing below this point will stop a short one. But
    /// a short key can be guessed, and guessing it forges the session
    /// cookie for the admin, the operator console and every tenant
    /// member. Refuse it here instead of signing with it.
    ///
    /// Use [`Self::from_b64`] if you want a `Result` instead.
    #[must_use]
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        assert!(
            bytes.len() >= 32,
            "SessionSecret is too short; need >= 32 bytes (a shorter key is forgeable)"
        );
        Self(bytes)
    }

    /// Raw key material. `pub(crate)` so framework modules can sign
    /// or verify payloads, but external callers go through
    /// [`sign`] / their layer's own encode/decode helpers.
    pub(crate) fn key(&self) -> &[u8] {
        &self.0
    }
}

/// `true` for tier strings that mean "production" (case-insensitive
/// `prod` / `production`). Anything else (dev, staging, test, unset) is
/// treated as non-production.
#[must_use]
pub fn is_prod_tier(tier: &str) -> bool {
    matches!(
        tier.trim().to_ascii_lowercase().as_str(),
        "prod" | "production"
    )
}

/// Load the session secret for a deployment tier.
///
/// * **prod** uses [`SessionSecret::require_from_env`] and panics on
///   any problem. The server refuses to start rather than sign
///   cookies with a random per-process key.
/// * **anything else** uses [`SessionSecret::from_env_or_disk`], so
///   local sessions survive a restart with no env var.
///
/// `tier` is usually `RUSTANGO_ENV`, read via [`tier_from_env`].
///
/// # Panics
/// On the prod tier when `RUSTANGO_SESSION_SECRET` is missing/invalid.
#[must_use]
pub fn load_session_secret_for_tier(tier: &str, disk_path: &std::path::Path) -> SessionSecret {
    if is_prod_tier(tier) {
        match SessionSecret::require_from_env() {
            Ok(secret) => secret,
            Err(e) => panic!(
                "refusing to start on the prod tier (RUSTANGO_ENV={tier}): {e}. \
                 Set RUSTANGO_SESSION_SECRET to a stable base64-encoded 32+ byte \
                 key shared across all instances."
            ),
        }
    } else {
        SessionSecret::from_env_or_disk(disk_path)
    }
}

/// Read the deployment tier from `RUSTANGO_ENV`, defaulting to `"dev"`
/// when unset (matches `crate::config` tier resolution).
#[must_use]
pub fn tier_from_env() -> String {
    std::env::var("RUSTANGO_ENV").unwrap_or_else(|_| "dev".to_owned())
}

/// Explicit override for the console-cookie `Secure` policy, set once
/// at boot by [`set_secure_cookies`]. It beats the tier default.
static SECURE_COOKIES_OVERRIDE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Set the console-cookie `Secure` policy. The first call wins, and
/// later calls return `false`.
///
/// The `manage` runner calls this with `security.secure_cookies`,
/// which defaults to `true`. So the normal boot path is fail-closed:
/// cookies are `Secure` unless someone turns that off, as a local
/// plain-HTTP dev setup would.
pub fn set_secure_cookies(secure: bool) -> bool {
    SECURE_COOKIES_OVERRIDE.set(secure).is_ok()
}

/// Pick the `Secure` policy: the override wins, else "secure on the
/// prod tier". A pure helper, so tests can check the order without
/// touching globals or the environment.
fn resolve_secure_cookies(override_flag: Option<bool>, tier: &str) -> bool {
    override_flag.unwrap_or_else(|| is_prod_tier(tier))
}

/// Whether the operator and tenant console cookies get the `Secure`
/// attribute. In order:
///
/// 1. the policy set at boot by [`set_secure_cookies`], else
/// 2. secure on the prod tier, read from `RUSTANGO_ENV`. This covers
///    boots that skip `manage`: HTTPS prod still gets `Secure`, and
///    plain-HTTP dev still works.
#[must_use]
pub fn secure_cookies() -> bool {
    resolve_secure_cookies(SECURE_COOKIES_OVERRIDE.get().copied(), &tier_from_env())
}

/// Report that `RUSTANGO_SESSION_SECRET` was set but unusable.
///
/// Goes to both `tracing` (for the logs) and stderr (for whoever is
/// watching the boot). Staying quiet here makes a failed key rotation
/// look like it worked.
fn warn_unusable_secret(reason: &str) {
    tracing::warn!(
        reason,
        "RUSTANGO_SESSION_SECRET is set but unusable — falling back to the persisted key",
    );
    eprintln!(
        "\x1b[33;1mwarning:\x1b[0m RUSTANGO_SESSION_SECRET is set but cannot be used: \
         {reason}. Falling back to the key on disk. Generate a valid one with: \
         openssl rand -base64 32"
    );
}

/// Set the saved secret file to 0600 on Unix, so other users on the
/// host cannot read the signing key. Windows needs its own ACL work.
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

    /// A short key is refused, not signed with. `sign` would accept
    /// it: HMAC takes any key length, so the check must be here.
    #[test]
    #[should_panic(expected = "need >= 32")]
    fn a_short_session_secret_is_refused() {
        let _ = SessionSecret::from_bytes(b"too-short".to_vec());
    }

    /// And the boundary is inclusive — exactly 32 is accepted.
    #[test]
    fn a_thirty_two_byte_secret_is_accepted() {
        let s = SessionSecret::from_bytes(vec![0u8; 32]);
        assert_eq!(s.key().len(), 32);
    }

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

    #[test]
    fn from_b64_accepts_valid_32_byte_key() {
        let raw = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
        assert!(SessionSecret::from_b64(&raw).is_ok());
    }

    #[test]
    fn from_b64_rejects_short_key() {
        let raw = base64::engine::general_purpose::STANDARD.encode([7u8; 16]);
        assert!(matches!(
            SessionSecret::from_b64(&raw),
            Err(SessionSecretError::TooShort { actual: 16 })
        ));
    }

    #[test]
    fn from_b64_rejects_bad_base64() {
        assert!(matches!(
            SessionSecret::from_b64("!!! not base64 !!!"),
            Err(SessionSecretError::BadBase64 { .. })
        ));
    }

    #[test]
    fn is_prod_tier_matches_prod_and_production_case_insensitively() {
        assert!(is_prod_tier("prod"));
        assert!(is_prod_tier("production"));
        assert!(is_prod_tier("PROD"));
        assert!(is_prod_tier("  Production  "));
        assert!(!is_prod_tier("dev"));
        assert!(!is_prod_tier("staging"));
        assert!(!is_prod_tier(""));
    }

    #[test]
    fn resolve_secure_cookies_override_wins_else_tier() {
        // The explicit policy wins over the tier. Fall back to
        // "secure on the prod tier" only when nothing set it.
        assert!(resolve_secure_cookies(Some(true), "dev")); // override on, even in dev
        assert!(!resolve_secure_cookies(Some(false), "prod")); // override off, even in prod
        assert!(resolve_secure_cookies(None, "prod")); // no override → tier
        assert!(!resolve_secure_cookies(None, "dev")); // no override → tier
        assert!(!resolve_secure_cookies(None, "")); // unset tier behaves as dev
    }

    #[test]
    fn dev_tier_loads_without_requiring_env() {
        // The dev tier must never require RUSTANGO_SESSION_SECRET — it
        // falls back to a disk-persisted (or ephemeral) >=32-byte key.
        let dir = std::env::temp_dir().join(format!("rustango_sess_test_{}", std::process::id()));
        let path = dir.join("k.key");
        let s = load_session_secret_for_tier("dev", &path);
        assert!(s.key().len() >= 32);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
