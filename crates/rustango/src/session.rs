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

/// The dev key file for tenant sessions; API keys are MACed with it too,
/// so the CLI that mints one and the server that checks it agree.
#[cfg(feature = "tenancy")]
pub(crate) const TENANT_SESSION_KEY_PATH: &str = "./var/.rustango_tenant_session.key";

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
        match load_or_create_key_file(disk_path, "session secret", "RUSTANGO_SESSION_SECRET") {
            Ok(bytes) => Self(bytes),
            Err(e) => {
                tracing::warn!(
                    path = %disk_path.display(),
                    error = %e,
                    "could not persist session secret to disk — using ephemeral random key",
                );
                let mut buf = vec![0u8; 32];
                OsRng.fill_bytes(&mut buf);
                Self(buf)
            }
        }
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
    crate::env::is_prod_tier(tier)
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
/// when unset. Normalized like `crate::config` (`PRODUCTION` reads `prod`).
#[must_use]
pub fn tier_from_env() -> String {
    crate::env::tier_from_env()
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
pub(crate) fn warn_unusable_secret(reason: &str) {
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

/// The 32+ byte key saved at `path`, else a new random one saved there
/// owner-only. `what` and `var` name it in the one INFO line each logs.
pub(crate) fn load_or_create_key_file(
    path: &std::path::Path,
    what: &str,
    var: &str,
) -> std::io::Result<Vec<u8>> {
    if let Ok(bytes) = std::fs::read(path) {
        if bytes.len() >= 32 {
            restrict_session_secret_perms(path);
            tracing::info!(path = %path.display(), "loaded persisted {what} from disk");
            return Ok(bytes);
        }
    }
    let mut buf = vec![0u8; 32];
    OsRng.fill_bytes(&mut buf);
    if let Some(parent) = path.parent() {
        create_private_dir(parent);
    }
    let on_disk = persist_secret(path, &buf)?;
    tracing::info!(
        path = %path.display(),
        "generated new {what} and persisted to disk (set {var} to override; \
         this message only fires on first boot)",
    );
    // Another first boot may have won; everyone uses its key.
    Ok(on_disk)
}

/// Publish `bytes` at `path` unless another boot got there first, and
/// return the key that is on disk. The temp file has a unique name, is
/// owner-only from creation, and is never opened through a link.
fn persist_secret(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    use std::io::Write as _;
    let mut nonce = [0u8; 8];
    OsRng.fill_bytes(&mut nonce);
    let suffix: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(format!(".{}.{suffix}.tmp", std::process::id()));
    let tmp = path.with_file_name(tmp_name);
    let mut file = create_owner_only(&tmp)?;
    let published = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| publish_first(&tmp, path));
    let _ = std::fs::remove_file(&tmp);
    let won = published?;
    sync_parent(path);
    if won {
        return Ok(bytes.to_vec());
    }
    let on_disk = std::fs::read(path)?;
    if on_disk.len() >= 32 {
        return Ok(on_disk);
    }
    Err(std::io::Error::other("session secret file is too short"))
}

/// Link `tmp` at `path` only if nothing is there: `false` when another
/// writer already published. A filesystem without hard links falls back
/// to a check-then-rename.
fn publish_first(tmp: &std::path::Path, path: &std::path::Path) -> std::io::Result<bool> {
    match std::fs::hard_link(tmp, path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // A stale short file (the loader skipped it) is replaced.
            match std::fs::read(path) {
                Ok(b) if b.len() >= 32 => Ok(false),
                _ => std::fs::rename(tmp, path).map(|()| true),
            }
        }
        Err(_) if std::fs::symlink_metadata(path).is_ok() => Ok(false),
        Err(_) => std::fs::rename(tmp, path).map(|()| true),
    }
}

/// Create `dir` (and missing parents) owner-only on Unix.
fn create_private_dir(dir: &std::path::Path) {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    let _ = b.create(dir);
}

/// Flush the directory entry so a crash cannot lose the published name.
fn sync_parent(path: &std::path::Path) {
    #[cfg(unix)]
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        let _ = std::fs::File::open(dir).and_then(|d| d.sync_all());
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// `O_CREAT | O_EXCL`, mode 0600 on Unix. Windows has no mode bits here.
fn create_owner_only(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    opts.open(path)
}

/// Make a saved secret other users can read owner-only, and say so. A
/// link is left alone: its target may be shared on purpose. Windows needs
/// its own ACL work.
#[cfg(unix)]
fn restrict_session_secret_perms(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    if meta.file_type().is_symlink() {
        let loose = std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o077 != 0);
        if loose {
            tracing::warn!(
                path = %path.display(),
                "session secret link points to a file other users can read; not changed. \
                 Restrict it and rotate the key",
            );
        }
        return;
    }
    let mut perms = meta.permissions();
    if perms.mode() & 0o077 == 0 {
        return;
    }
    tracing::warn!(
        path = %path.display(),
        mode = format!("{:o}", perms.mode() & 0o777),
        "session secret file was readable by other users; setting it to 0600. \
         Rotate the key: it may have been read",
    );
    perms.set_mode(0o600);
    let _ = std::fs::set_permissions(path, perms);
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

/// HMAC of a user's `password_hash`, carried in a session cookie. Every
/// password write makes a new salted hash, so the old sessions stop matching.
///
/// `Default` is the empty value that older cookies decode to; it never matches.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct PasswordFingerprint(String);

/// Domain tag, so a fingerprint is never a valid MAC of anything else.
const PASSWORD_FINGERPRINT_TAG: &[u8] = b"rustango-pwf-v1.";

impl PasswordFingerprint {
    /// Fingerprint `password_hash` under `secret`.
    #[must_use]
    pub fn of(secret: &SessionSecret, password_hash: &str) -> Self {
        let msg = [PASSWORD_FINGERPRINT_TAG, password_hash.as_bytes()].concat();
        Self(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sign(secret, &msg)))
    }

    /// `true` when this was minted from `password_hash`. Constant time.
    #[must_use]
    pub fn matches(&self, secret: &SessionSecret, password_hash: &str) -> bool {
        let current = Self::of(secret, password_hash);
        use subtle::ConstantTimeEq as _;
        self.0.as_bytes().ct_eq(current.0.as_bytes()).into()
    }
}

/// Who passed the first factor and still owes a TOTP code. Signed under a
/// per-flow tag ending in `.`, so it never verifies as a session cookie.
#[cfg(all(feature = "totp", any(feature = "admin-sso", feature = "tenancy")))]
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct PendingSecondFactor {
    pub(crate) uid: i64,
    /// Bound to the password hash, so a password change in between ends it.
    pub(crate) pwf: PasswordFingerprint,
    pub(crate) exp: i64,
}

#[cfg(all(feature = "totp", any(feature = "admin-sso", feature = "tenancy")))]
impl PendingSecondFactor {
    /// How long the code step stays open.
    pub(crate) const TTL_SECS: i64 = 300;

    pub(crate) fn new(secret: &SessionSecret, uid: i64, password_hash: &str) -> Self {
        Self {
            uid,
            pwf: PasswordFingerprint::of(secret, password_hash),
            exp: chrono::Utc::now().timestamp() + Self::TTL_SECS,
        }
    }

    fn mac(secret: &SessionSecret, tag: &[u8], body: &str) -> [u8; 32] {
        sign(secret, &[tag, body.as_bytes()].concat())
    }

    pub(crate) fn seal(&self, secret: &SessionSecret, tag: &[u8]) -> String {
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let body = b64.encode(serde_json::to_vec(self).unwrap_or_default());
        let sig = b64.encode(Self::mac(secret, tag, &body));
        format!("{body}.{sig}")
    }

    /// The pending state behind `value`, if signed under `tag` and not expired.
    pub(crate) fn open(secret: &SessionSecret, tag: &[u8], value: &str) -> Option<Self> {
        use subtle::ConstantTimeEq as _;
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let (body, sig) = value.split_once('.')?;
        let sig = b64.decode(sig).ok()?;
        if Self::mac(secret, tag, body).ct_eq(&sig[..]).unwrap_u8() == 0 {
            return None;
        }
        let p: Self = serde_json::from_slice(&b64.decode(body).ok()?).ok()?;
        (chrono::Utc::now().timestamp() < p.exp).then_some(p)
    }
}

/// Column every session principal (tenant user, operator, admin user)
/// stamps on logout; sessions issued at or before it are refused (#1855).
#[cfg(any(feature = "admin", feature = "tenancy"))]
pub(crate) const SESSIONS_REVOKED_AT: &str = "sessions_revoked_at";

/// `true` when a session minted with `pwf` at `iat` is still valid: same
/// password hash, not before `password_changed_at`, after the last logout.
#[cfg(any(feature = "admin", feature = "tenancy"))]
#[must_use]
pub(crate) fn session_survives(
    secret: &SessionSecret,
    pwf: &PasswordFingerprint,
    iat: i64,
    password_hash: &str,
    password_changed_at: Option<chrono::DateTime<chrono::Utc>>,
    sessions_revoked_at: Option<chrono::DateTime<chrono::Utc>>,
) -> bool {
    pwf.matches(secret, password_hash)
        && password_changed_at.is_none_or(|ts| iat >= ts.timestamp())
        && sessions_revoked_at.is_none_or(|ts| iat > ts.timestamp())
}

/// `iat` for a new session: strictly after the last logout, so a login in
/// the logout's own second still validates.
#[cfg(any(feature = "admin", feature = "tenancy"))]
#[must_use]
pub(crate) fn issued_at(sessions_revoked_at: Option<chrono::DateTime<chrono::Utc>>) -> i64 {
    let now = chrono::Utc::now().timestamp();
    sessions_revoked_at.map_or(now, |ts| now.max(ts.timestamp() + 1))
}

/// Logout cut-off: covers every session issued so far, the logged-out
/// cookie included even when a faster-clock replica minted it.
#[cfg(any(feature = "admin", feature = "tenancy"))]
#[must_use]
pub(crate) fn logout_cutoff(
    sessions_revoked_at: Option<chrono::DateTime<chrono::Utc>>,
    cookie_iat: i64,
) -> i64 {
    issued_at(sessions_revoked_at).max(cookie_iat.saturating_add(1))
}

/// [`logout_cutoff`] as the timestamp stored in `sessions_revoked_at`.
#[cfg(any(feature = "admin", feature = "tenancy"))]
#[must_use]
pub(crate) fn logout_cutoff_at(
    sessions_revoked_at: Option<chrono::DateTime<chrono::Utc>>,
    cookie_iat: i64,
) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(logout_cutoff(sessions_revoked_at, cookie_iat), 0)
        .unwrap_or_else(chrono::Utc::now)
}

/// End every session of row `id` of `T`, on every device, at [`logout_cutoff`].
/// `sessions_revoked_at` is the row's current value; the WHERE never moves it back.
///
/// # Errors
/// [`crate::sql::ExecError`] from the update.
#[cfg(any(feature = "admin", feature = "tenancy"))]
pub(crate) async fn revoke_sessions<T: crate::core::Model + Send>(
    pool: &crate::sql::Pool,
    id: i64,
    sessions_revoked_at: Option<chrono::DateTime<chrono::Utc>>,
    cookie_iat: i64,
) -> Result<u64, crate::sql::ExecError> {
    use crate::query::Q;
    use crate::sql::UpdaterPool as _;
    let cutoff = logout_cutoff_at(sessions_revoked_at, cookie_iat);
    crate::query::QuerySet::<T>::default()
        .filter("id", id)
        .where_(Q::is_null(SESSIONS_REVOKED_AT) | Q::lt(SESSIONS_REVOKED_AT, cutoff))
        .update()
        .set(SESSIONS_REVOKED_AT, cutoff)
        .execute_pool(pool)
        .await
}

/// Deactivate row `id` of a session principal (`AdminUser`, `Operator`, a
/// tenant user) and end its sessions, so a later reactivation revives none.
/// Writing `active = false` through the ORM directly does not stamp them.
///
/// Returns `false` when there is no such row. `T` needs the `active` and
/// `sessions_revoked_at` columns.
///
/// # Errors
/// [`crate::sql::ExecError`] from the read or the updates.
#[cfg(any(feature = "admin", feature = "tenancy"))]
pub async fn deactivate<T: crate::core::Model + Send>(
    pool: &crate::sql::Pool,
    id: i64,
) -> Result<bool, crate::sql::ExecError> {
    use crate::core::{SelectQuery, SqlValue};
    use crate::sql::UpdaterPool as _;
    let fields: Vec<&'static crate::core::FieldSchema> = T::SCHEMA
        .fields
        .iter()
        .filter(|f| f.column == SESSIONS_REVOKED_AT)
        .collect();
    let select = SelectQuery::by_pk(T::SCHEMA, "id", SqlValue::I64(id));
    let Some(row) = crate::sql::select_one_row_as_json(pool, &select, &fields).await? else {
        return Ok(false);
    };
    let prev = row
        .get(SESSIONS_REVOKED_AT)
        .and_then(serde_json::Value::as_str)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&chrono::Utc));
    // Stamp first: a failure in between leaves an active, logged-out account.
    revoke_sessions::<T>(pool, id, prev, 0).await?;
    crate::query::QuerySet::<T>::default()
        .filter("id", id)
        .update()
        .set("active", false)
        .execute_pool(pool)
        .await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fingerprint is domain-tagged, not a bare MAC of the hash.
    #[test]
    fn a_password_fingerprint_is_not_a_bare_mac_of_the_hash() {
        let secret = SessionSecret::from_bytes(vec![7u8; 32]);
        let bare =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sign(&secret, b"$argon2id$h"));
        assert_ne!(PasswordFingerprint::of(&secret, "$argon2id$h").0, bare);
    }

    /// A logout ends sessions issued in its own second; the next login's
    /// `iat` lands after the cut-off (#1855).
    #[cfg(any(feature = "admin", feature = "tenancy"))]
    #[test]
    fn a_logout_cutoff_ends_older_sessions_but_not_the_next_login() {
        let secret = SessionSecret::from_bytes(vec![7u8; 32]);
        let pwf = PasswordFingerprint::of(&secret, "$h");
        let now = chrono::Utc::now().timestamp();
        let cut = chrono::DateTime::from_timestamp(now, 0);
        assert!(session_survives(&secret, &pwf, now, "$h", None, None));
        assert!(!session_survives(&secret, &pwf, now, "$h", None, cut));
        let next = issued_at(cut);
        assert!(next > now);
        assert!(session_survives(&secret, &pwf, next, "$h", None, cut));
    }

    /// The cut-off covers a cookie minted ahead of this node's clock (#1855).
    #[cfg(any(feature = "admin", feature = "tenancy"))]
    #[test]
    fn a_logout_cutoff_covers_a_cookie_from_a_faster_clock() {
        let ahead = chrono::Utc::now().timestamp() + 100;
        assert_eq!(logout_cutoff(None, ahead), ahead + 1);
        let later = chrono::DateTime::from_timestamp(ahead + 50, 0);
        assert_eq!(logout_cutoff(later, ahead), ahead + 51);
    }

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

    #[cfg(unix)]
    fn mode(path: &std::path::Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    }

    /// A link planted at the key path must not receive the key.
    #[cfg(unix)]
    #[test]
    fn persisting_a_secret_never_writes_through_a_planted_link() {
        assert!(std::env::var_os("RUSTANGO_SESSION_SECRET").is_none());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.key");
        let victim = dir.path().join("victim");
        std::fs::write(&victim, b"").unwrap();
        std::os::unix::fs::symlink(&victim, &path).unwrap();

        let s = SessionSecret::from_env_or_disk(&path);
        assert!(std::fs::read(&victim).unwrap().is_empty(), "key leaked");
        assert!(std::fs::symlink_metadata(&path).unwrap().is_file());
        assert_eq!(std::fs::read(&path).unwrap(), s.key());
        assert_eq!(mode(&path), 0o600);
    }

    /// A saved key other users can read is made owner-only on load.
    #[cfg(unix)]
    #[test]
    fn a_loose_saved_secret_is_made_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        assert!(std::env::var_os("RUSTANGO_SESSION_SECRET").is_none());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.key");
        std::fs::write(&path, [9u8; 32]).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let s = SessionSecret::from_env_or_disk(&path);
        assert_eq!(s.key(), [9u8; 32]);
        assert_eq!(mode(&path), 0o600);
    }

    /// Concurrent first boots agree on one key, in memory and on disk.
    #[test]
    fn concurrent_first_boots_share_one_key() {
        assert!(std::env::var_os("RUSTANGO_SESSION_SECRET").is_none());
        for _ in 0..20 {
            let dir = tempfile::tempdir().unwrap();
            let path = std::sync::Arc::new(dir.path().join("var").join("s.key"));
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
            let keys: Vec<Vec<u8>> = (0..8)
                .map(|_| {
                    let (path, barrier) = (path.clone(), barrier.clone());
                    std::thread::spawn(move || {
                        barrier.wait();
                        SessionSecret::from_env_or_disk(&path).key().to_vec()
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|h| h.join().unwrap())
                .collect();
            let on_disk = std::fs::read(&*path).unwrap();
            assert!(keys.iter().all(|k| *k == on_disk), "boots diverged");
            let left: Vec<_> = std::fs::read_dir(path.parent().unwrap())
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            assert_eq!(left.len(), 1, "temp files left: {left:?}");
            #[cfg(unix)]
            assert_eq!(mode(path.parent().unwrap()), 0o700);
        }
    }

    /// A link to a shared key is never chmod'ed through.
    #[cfg(unix)]
    #[test]
    fn a_linked_secret_is_not_chmoded() {
        use std::os::unix::fs::PermissionsExt as _;
        assert!(std::env::var_os("RUSTANGO_SESSION_SECRET").is_none());
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("mounted");
        std::fs::write(&target, [5u8; 32]).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        let path = dir.path().join("s.key");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert_eq!(SessionSecret::from_env_or_disk(&path).key(), [5u8; 32]);
        assert_eq!(mode(&target), 0o644);
    }

    /// A new key file is created owner-only, not chmod'ed after the fact.
    #[test]
    fn a_new_secret_file_holds_the_key() {
        assert!(std::env::var_os("RUSTANGO_SESSION_SECRET").is_none());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("s.key");
        let s = SessionSecret::from_env_or_disk(&path);
        assert_eq!(std::fs::read(&path).unwrap(), s.key());
        assert!(!path.with_extension("tmp").exists());
        #[cfg(unix)]
        assert_eq!(mode(&path), 0o600);
        // Read back on the next boot.
        assert_eq!(SessionSecret::from_env_or_disk(&path).key(), s.key());
    }
}
