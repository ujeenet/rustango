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
    /// The env var isn't set at all. Returned only by the strict
    /// [`SessionSecret::require_from_env`] (the lenient loaders treat an
    /// unset var as "generate a random key").
    Missing,
    /// The env var didn't decode as base64.
    BadBase64 { cause: String },
    /// Decoded successfully but the resulting key is fewer than 32
    /// bytes — too short for HMAC-SHA256.
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
            // One definition of "valid secret", shared with build_jwt and
            // `check --deploy` since #1396.
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
            match Self::from_b64(&raw) {
                Ok(secret) => return secret,
                // Set but unusable. This used to fall through in silence,
                // and the "generated new session secret … set
                // RUSTANGO_SESSION_SECRET to override" line that followed
                // read as though the variable were unset — so a malformed
                // real secret, or a deliberate rotation, looked applied and
                // was not (#1359).
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

    /// **Strict** variant for production boot: requires
    /// `RUSTANGO_SESSION_SECRET` to be present, valid base64, and ≥ 32
    /// bytes. Unlike [`Self::try_from_env`], an *unset* var is an error
    /// ([`SessionSecretError::Missing`]) rather than a silent random
    /// key — an ephemeral key breaks multi-instance deployments and
    /// masks a missing-secret misconfiguration. Used by
    /// [`load_session_secret_for_tier`] on the prod tier.
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

    /// Decode + validate a base64 secret string — **the** definition of
    /// what `RUSTANGO_SESSION_SECRET` means.
    ///
    /// Public since #1396, because it was not, and two other readers
    /// grew their own answers: `auth_routes::Config::build_jwt` signed
    /// JWTs with the raw string bytes, and `manage check --deploy`
    /// measured the raw string's length. A 32-character base64 secret
    /// decodes to 24 bytes, so it passed both 32-byte floors while this
    /// one — the runtime's — rejected it. One variable, three answers.
    ///
    /// Every reader now calls this, so a value `check --deploy` accepts
    /// is a value the runtime accepts.
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

    /// Construct from raw bytes — useful for tests + callers that
    /// load the key from a custom source.
    ///
    /// # Panics
    /// If `bytes` is shorter than 32. HMAC accepts any key length —
    /// `sign` below says so inline, right before relying on it — but a
    /// short key is guessable, and guessing this one forges the session
    /// cookie that authenticates the admin, the operator console and
    /// every tenant member session. Fail closed rather than sign with
    /// it.
    ///
    /// This floor already existed on [`Self::from_b64`] and
    /// `from_env_or_disk`; it was missing on exactly the constructor
    /// whose doc points at production — "callers that load the key from
    /// a custom source" is Vault, KMS, a secrets manager. So the casual
    /// path (paste base64 into an env var) was checked and the
    /// deliberate one was not.
    ///
    /// Same reasoning, and the same 32-byte floor, as
    /// [`crate::tenancy::jwt_lifecycle::JwtLifecycle::new`] (audit
    /// A-06). That audit hardened the JWT signing family and left this
    /// one alone, though a forged session cookie on the operator
    /// console is a strictly larger blast radius than a forged JWT.
    ///
    /// Use [`Self::from_b64`] when you want a `Result` instead.
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

/// Tier-aware session-secret loader (audit M2).
///
/// * **prod** tier → [`SessionSecret::require_from_env`]: the secret
///   MUST be a valid `RUSTANGO_SESSION_SECRET`. On any error (missing,
///   bad base64, too short) this **panics** — a production server
///   refuses to start rather than silently signing cookies + JWTs with
///   an ephemeral per-process random key (which breaks multi-instance
///   deployments and masks the misconfiguration).
/// * **dev / staging / anything else** → [`SessionSecret::from_env_or_disk`]:
///   restart-stable local development without requiring the env var.
///
/// `tier` is typically `RUSTANGO_ENV`; callers read it via
/// [`tier_from_env`].
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

/// Explicit override for the console-cookie `Secure` policy. Set once
/// at boot by [`set_secure_cookies`] from `security.secure_cookies`; when
/// present it wins over the tier default below (audit N2).
static SECURE_COOKIES_OVERRIDE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Install the explicit console-cookie `Secure` policy (first call wins).
/// The `manage` runner calls this from `security.secure_cookies` (which
/// defaults to `true`) when settings are applied, so the standard boot
/// path is **fail-closed**: cookies are `Secure` unless an operator
/// explicitly sets `security.secure_cookies = false` (e.g. in
/// `dev_settings.toml` for local plain-HTTP development). Returns `false`
/// if the policy was already set.
pub fn set_secure_cookies(secure: bool) -> bool {
    SECURE_COOKIES_OVERRIDE.set(secure).is_ok()
}

/// Resolve the console-cookie `Secure` policy: an explicit override
/// (from `security.secure_cookies`) wins; otherwise fall back to "secure
/// on the prod tier." Pure helper so the precedence is unit-testable
/// without touching the process-global / environment.
fn resolve_secure_cookies(override_flag: Option<bool>, tier: &str) -> bool {
    override_flag.unwrap_or_else(|| is_prod_tier(tier))
}

/// Whether the tenancy operator + tenant console cookies should carry the
/// `Secure` attribute (audit H2/N2). Precedence:
/// 1. the explicit policy set at boot via [`set_secure_cookies`] (from
///    `security.secure_cookies`, default `true` on the `manage` path —
///    fail-closed), else
/// 2. "secure on the prod tier" (`RUSTANGO_ENV`) as a fallback for
///    direct/non-`manage` use, so HTTPS prod still gets `Secure` cookies
///    without config while local plain-HTTP dev keeps working.
#[must_use]
pub fn secure_cookies() -> bool {
    resolve_secure_cookies(SECURE_COOKIES_OVERRIDE.get().copied(), &tier_from_env())
}

/// Say that `RUSTANGO_SESSION_SECRET` was set and could not be used.
///
/// Both surfaces, because the two audiences differ: `tracing` for whoever
/// reads the deployment's logs, stderr for whoever is watching the boot.
/// A silent fall-through here makes a failed key rotation look successful
/// (#1359).
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

    /// A short key is refused, not signed with.
    ///
    /// The same floor and the same reasoning as `JwtLifecycle::new`
    /// (audit A-06). `sign` builds its HMAC with
    /// `expect("HMAC accepts any key length")` — true, and precisely
    /// why the caller has to be the one that refuses.
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
        // Audit N2 — explicit policy (from security.secure_cookies) wins
        // over the tier; fall back to "secure on prod tier" only when no
        // override is set.
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
