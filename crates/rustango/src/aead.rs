//! XChaCha20-Poly1305 under a key derived from `RUSTANGO_SECRET_KEY`.
//!
//! Shared by the `EncryptedString` cast and sealed tenant secrets, so the
//! two cannot drift. Stored form: base64(`nonce(24) ‖ ciphertext+tag`).

use base64::Engine as _;
use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{Key, KeyInit, XChaCha20Poly1305, XNonce};
use sha2::{Digest, Sha256};

/// 192-bit XChaCha nonce: random per write, so reuse is not a concern.
const NONCE_LEN: usize = 24;

/// The environment variable the key is derived from.
pub(crate) const KEY_VAR: &str = "RUSTANGO_SECRET_KEY";

/// Shorter keys are guessable; 32 random bytes is the documented floor.
pub(crate) const MIN_KEY_LEN: usize = 32;

/// The current key as the console and `check --deploy` report it.
#[cfg_attr(not(feature = "tenancy"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyStatus {
    /// Unset: nothing can be sealed.
    Missing,
    /// Set, but below [`MIN_KEY_LEN`].
    Short,
    Ok,
}

#[cfg_attr(not(feature = "tenancy"), allow(dead_code))]
pub(crate) fn key_status() -> KeyStatus {
    match env_key() {
        None => KeyStatus::Missing,
        Some(k) if k.len() < MIN_KEY_LEN => KeyStatus::Short,
        Some(_) => KeyStatus::Ok,
    }
}

/// The one key every in-crate unit test uses: they share a process
/// under `cargo test`, so differing values would race.
#[cfg(test)]
pub(crate) fn set_test_key() {
    std::env::set_var(KEY_VAR, "unit-test-secret-key");
}

/// Why sealing or opening failed. Callers word their own message.
#[derive(Debug)]
pub(crate) enum AeadError {
    MissingKey,
    Encrypt(String),
    Decode(String),
    Short,
    Decrypt,
}

/// The current key: the env var, read per call so rotation and tests see
/// the live value (blank counts as unset), else the dev key file.
pub(crate) fn env_key() -> Option<String> {
    resolve_key(std::env::var(KEY_VAR).ok(), dev_key)
}

fn resolve_key(env: Option<String>, dev: impl FnOnce() -> Option<String>) -> Option<String> {
    env.filter(|k| !k.trim().is_empty()).or_else(dev)
}

/// Where the dev and test tiers keep a generated key, under the project root.
#[cfg(any(feature = "admin", feature = "tenancy", feature = "csrf"))]
const DEV_KEY_FILE: &str = "var/.rustango_secret.key";

/// The dev key, read (or generated) once per process.
#[cfg(any(feature = "admin", feature = "tenancy", feature = "csrf"))]
static DEV_KEY: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// On the dev and test tiers an unset key falls back to the saved one, so
/// every binary of the project (server, worker, `manage`, tests) agrees.
/// Any other tier keeps requiring the env var.
#[cfg(any(feature = "admin", feature = "tenancy", feature = "csrf"))]
fn dev_key() -> Option<String> {
    if !dev_key_tier(&crate::env::tier_from_env()) {
        return None;
    }
    DEV_KEY
        .get_or_init(|| {
            let cwd = std::env::current_dir().ok()?;
            load_dev_key(&dev_key_path(&cwd))
        })
        .clone()
}

#[cfg(not(any(feature = "admin", feature = "tenancy", feature = "csrf")))]
fn dev_key() -> Option<String> {
    None
}

/// The first saved [`DEV_KEY_FILE`] from `cwd` up to the Cargo workspace root
/// (the nearest dir with `Cargo.lock`), else a new one there, else in `cwd`.
#[cfg(any(feature = "admin", feature = "tenancy", feature = "csrf"))]
fn dev_key_path(cwd: &std::path::Path) -> std::path::PathBuf {
    let root = cwd.ancestors().find(|d| d.join("Cargo.lock").is_file());
    let searched: Vec<&std::path::Path> = match root {
        Some(root) => cwd
            .ancestors()
            .take_while(|d| *d != root)
            .chain([root])
            .collect(),
        None => vec![cwd],
    };
    searched
        .iter()
        .map(|d| d.join(DEV_KEY_FILE))
        .find(|p| p.is_file())
        .unwrap_or_else(|| root.unwrap_or(cwd).join(DEV_KEY_FILE))
}

/// Only these tiers may seal with a key nobody set; staging and prod may not.
#[cfg(any(feature = "admin", feature = "tenancy", feature = "csrf"))]
fn dev_key_tier(tier: &str) -> bool {
    matches!(crate::env::canonical_tier(tier).as_str(), "dev" | "test")
}

/// The saved key as text, generated on first use; `None` when it cannot be saved.
#[cfg(any(feature = "admin", feature = "tenancy", feature = "csrf"))]
fn load_dev_key(path: &std::path::Path) -> Option<String> {
    use base64::Engine as _;
    match crate::session::load_or_create_key_file(path, "secret key", KEY_VAR) {
        Ok(bytes) => Some(base64::engine::general_purpose::STANDARD.encode(bytes)),
        Err(e) => {
            tracing::warn!(
                target: "rustango::aead",
                path = %path.display(),
                error = %e,
                "could not save a dev {KEY_VAR}; sealing stays off until it is set"
            );
            None
        }
    }
}

/// A key that may seal: set and not blank. A blank one would seal under
/// SHA-256 of nothing, which anyone can open.
#[derive(Clone, Copy)]
struct AeadKey<'a>(&'a str);

impl<'a> AeadKey<'a> {
    fn new(secret: Option<&'a str>) -> Result<Self, AeadError> {
        match secret {
            Some(s) if !s.trim().is_empty() => Ok(Self(s)),
            _ => Err(AeadError::MissingKey),
        }
    }

    /// Opening only: an explicitly set blank previous key, so values sealed
    /// under one before it was refused can still be re-sealed.
    fn previous(secret: Option<&'a str>) -> Result<Self, AeadError> {
        secret.map(Self).ok_or(AeadError::MissingKey)
    }

    fn cipher(self) -> XChaCha20Poly1305 {
        // SHA-256 of the secret, so any length works.
        let digest = Sha256::digest(self.0.as_bytes());
        XChaCha20Poly1305::new(Key::from_slice(&digest))
    }
}

/// Encrypt `plaintext`, binding `aad` (empty for the cast).
pub(crate) fn encrypt(plaintext: &[u8], aad: &[u8]) -> Result<String, AeadError> {
    warn_blank_previous(std::env::var(PREVIOUS_KEY_VAR).ok().as_deref());
    encrypt_with(env_key().as_deref(), plaintext, aad)
}

/// A blank previous key opens what anyone could seal: say so, once, while
/// it is set. It still opens, so old values can be re-sealed.
fn warn_blank_previous(previous: Option<&str>) -> bool {
    static WARNED: std::sync::Once = std::sync::Once::new();
    let blank = previous.is_some_and(|p| p.trim().is_empty());
    if blank {
        WARNED.call_once(|| {
            tracing::warn!(
                target: "rustango::aead",
                "{PREVIOUS_KEY_VAR} is set but blank; run `reseal-tenant-secrets --apply`, then unset it"
            );
        });
    }
    blank
}

/// Set during key rotation to the old key; only ever used to open.
pub(crate) const PREVIOUS_KEY_VAR: &str = "RUSTANGO_SECRET_KEY_PREVIOUS";

/// Open what [`encrypt`] produced under the same `aad`, with the
/// previous key when the current one fails.
#[cfg_attr(not(feature = "casts"), allow(dead_code))]
pub(crate) fn decrypt(stored: &str, aad: &[u8]) -> Result<Vec<u8>, AeadError> {
    let previous = std::env::var(PREVIOUS_KEY_VAR).ok();
    warn_blank_previous(previous.as_deref());
    decrypt_either(env_key().as_deref(), previous.as_deref(), stored, aad)
}

/// [`decrypt_with`] under `current`, then under `previous`.
pub(crate) fn decrypt_either(
    current: Option<&str>,
    previous: Option<&str>,
    stored: &str,
    aad: &[u8],
) -> Result<Vec<u8>, AeadError> {
    match AeadKey::new(current).and_then(|k| open(k, stored, aad)) {
        Err(AeadError::Decrypt | AeadError::MissingKey) if previous.is_some() => {
            open(AeadKey::previous(previous)?, stored, aad)
        }
        other => other,
    }
}

pub(crate) fn encrypt_with(
    secret: Option<&str>,
    plaintext: &[u8],
    aad: &[u8],
) -> Result<String, AeadError> {
    let cipher = AeadKey::new(secret)?.cipher();
    let mut nonce = [0u8; NONCE_LEN];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut nonce);
    let ciphertext = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|e| AeadError::Encrypt(e.to_string()))?;
    let mut framed = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    framed.extend_from_slice(&nonce);
    framed.extend_from_slice(&ciphertext);
    Ok(base64::engine::general_purpose::STANDARD.encode(framed))
}

#[cfg(test)]
pub(crate) fn decrypt_with(
    secret: Option<&str>,
    stored: &str,
    aad: &[u8],
) -> Result<Vec<u8>, AeadError> {
    open(AeadKey::new(secret)?, stored, aad)
}

fn open(key: AeadKey<'_>, stored: &str, aad: &[u8]) -> Result<Vec<u8>, AeadError> {
    let framed = base64::engine::general_purpose::STANDARD
        .decode(stored)
        .map_err(|e| AeadError::Decode(e.to_string()))?;
    if framed.len() < NONCE_LEN {
        return Err(AeadError::Short);
    }
    let (nonce, ciphertext) = framed.split_at(NONCE_LEN);
    key.cipher()
        .decrypt(
            XNonce::from_slice(nonce),
            Payload {
                msg: ciphertext,
                aad,
            },
        )
        .map_err(|_| AeadError::Decrypt)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The env var wins; blank counts as unset and falls to the dev key.
    #[test]
    fn the_env_key_beats_the_dev_key() {
        let dev = || Some("dev-key".to_owned());
        assert_eq!(
            resolve_key(Some("env-key".into()), dev).as_deref(),
            Some("env-key")
        );
        assert_eq!(
            resolve_key(Some("  ".into()), dev).as_deref(),
            Some("dev-key")
        );
        assert_eq!(resolve_key(None, dev).as_deref(), Some("dev-key"));
        assert_eq!(resolve_key(None, || None), None);
    }

    /// Dev and test get a saved key that survives a restart; staging and prod never do.
    #[cfg(any(feature = "admin", feature = "tenancy", feature = "csrf"))]
    #[test]
    fn only_dev_and_test_generate_a_key() {
        let dir = std::env::temp_dir().join(format!("rustango_aead_dev_{}", std::process::id()));
        let path = dir.join("var/.rustango_secret.key");
        for tier in ["prod", "Production", "staging"] {
            assert!(!dev_key_tier(tier), "{tier}");
        }
        assert!(dev_key_tier("dev") && dev_key_tier("") && dev_key_tier("TEST"));
        let first = load_dev_key(&path).expect("generated");
        assert!(first.len() >= MIN_KEY_LEN, "short dev key");
        assert_eq!(
            load_dev_key(&path),
            Some(first.clone()),
            "a restart keeps the key"
        );
        let sealed = encrypt_with(Some(&first), b"pw", b"").unwrap();
        assert_eq!(
            decrypt_with(load_dev_key(&path).as_deref(), &sealed, b"").unwrap(),
            b"pw"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every dir of a workspace finds one key: a saved one on the way up, else the root's.
    #[cfg(any(feature = "admin", feature = "tenancy", feature = "csrf"))]
    #[test]
    fn the_dev_key_path_does_not_depend_on_the_cwd() {
        let root = std::env::temp_dir().join(format!("rustango_aead_path_{}", std::process::id()));
        let sub = root.join("crates/app/src");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(root.join("Cargo.lock"), "").unwrap();
        let at_root = root.join(DEV_KEY_FILE);
        assert_eq!(dev_key_path(&sub), at_root);
        assert_eq!(dev_key_path(&root), at_root);
        let crate_key = root.join("crates/app").join(DEV_KEY_FILE);
        std::fs::create_dir_all(crate_key.parent().unwrap()).unwrap();
        std::fs::write(&crate_key, [0u8; 32]).unwrap();
        assert_eq!(dev_key_path(&sub), crate_key, "a saved key wins");
        assert_eq!(
            dev_key_path(&root.join("crates")),
            at_root,
            "never looks down"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    const KEY: Option<&str> = Some("aead-unit-test-key");

    /// Ciphertext written before this module existed (no AAD) still opens.
    #[test]
    fn empty_aad_matches_the_plain_slice_api() {
        let digest = Sha256::digest(KEY.unwrap().as_bytes());
        let cipher = XChaCha20Poly1305::new(Key::from_slice(&digest));
        let nonce = [7u8; NONCE_LEN];
        let ct = cipher
            .encrypt(XNonce::from_slice(&nonce), b"legacy".as_slice())
            .unwrap();
        let framed = [nonce.as_slice(), &ct].concat();
        let stored = base64::engine::general_purpose::STANDARD.encode(framed);
        assert_eq!(decrypt_with(KEY, &stored, b"").unwrap(), b"legacy");
    }

    #[test]
    fn only_a_blank_previous_key_warns() {
        assert!(warn_blank_previous(Some("  ")));
        assert!(!warn_blank_previous(Some("old-key")));
        assert!(!warn_blank_previous(None));
    }

    #[test]
    fn a_different_aad_does_not_open() {
        let stored = encrypt_with(KEY, b"x", b"one").unwrap();
        assert!(matches!(
            decrypt_with(KEY, &stored, b"two"),
            Err(AeadError::Decrypt)
        ));
        assert_eq!(decrypt_with(KEY, &stored, b"one").unwrap(), b"x");
    }

    /// A blank key seals under a hash anyone can compute, so it is unset;
    /// a blank previous key still opens what it sealed, for re-sealing.
    #[test]
    fn a_blank_key_never_seals() {
        for blank in ["", "  "] {
            assert!(matches!(
                encrypt_with(Some(blank), b"x", b""),
                Err(AeadError::MissingKey)
            ));
        }
        let digest = Sha256::digest(b"");
        let nonce = [1u8; NONCE_LEN];
        let ct = XChaCha20Poly1305::new(Key::from_slice(&digest))
            .encrypt(XNonce::from_slice(&nonce), b"old".as_slice())
            .unwrap();
        let stored =
            base64::engine::general_purpose::STANDARD.encode([nonce.as_slice(), &ct].concat());
        assert!(decrypt_either(Some(""), None, &stored, b"").is_err());
        assert_eq!(decrypt_either(KEY, Some(""), &stored, b"").unwrap(), b"old");
    }

    #[test]
    fn a_missing_key_is_its_own_error() {
        assert!(matches!(
            encrypt_with(None, b"x", b""),
            Err(AeadError::MissingKey)
        ));
        let stored = encrypt_with(KEY, b"x", b"").unwrap();
        assert!(matches!(
            decrypt_with(None, &stored, b""),
            Err(AeadError::MissingKey)
        ));
    }
}
