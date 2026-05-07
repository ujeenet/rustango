//! Per-tenant branding — logo / favicon storage, CSS variable
//! overrides derived from `Org.primary_color`, validation helpers
//! for `theme_mode` and hex colors.
//!
//! ## What this module owns
//!
//! * **Storage layout** — assets live under
//!   `{root}/{slug}/{logo|favicon}.{ext}`, where `root` comes from
//!   `RUSTANGO_BRAND_STORAGE_DIR` (default `./var/brand`).
//! * **CSS variable derivation** — [`build_brand_css`] takes an
//!   [`Org`] and emits a single line of `--color-* : <hex>;`
//!   assignments suitable for inlining inside `<style>:root{ ... }`.
//!   No raw CSS from operator input ever reaches the page.
//! * **Validation** — [`validate_hex_color`] guards `primary_color`,
//!   [`validate_theme_mode`] guards `theme_mode`. Anything that
//!   doesn't match is dropped (returns `None`).
//!
//! ## What this module does NOT own
//!
//! * Multipart parsing — the operator console handler peels bytes
//!   out of `axum::extract::Multipart` and passes them in.
//! * The HTTP route — both the operator console
//!   (`/__brand__/{slug}/{filename}`) and the tenant admin (same
//!   path under each tenant host) wire their own handlers; this
//!   module just supplies the storage + content-type lookups.

use std::path::PathBuf;
use std::sync::Arc;

use crate::storage::{BoxedStorage, LocalStorage, StorageError};

use super::org::Org;

/// Env var for the brand storage root. Default: `./var/brand`.
pub const BRAND_STORAGE_ROOT_ENV: &str = "RUSTANGO_BRAND_STORAGE_DIR";
const DEFAULT_BRAND_STORAGE_ROOT: &str = "./var/brand";

/// Env var for the per-asset size cap. Default: 1 MiB.
pub const MAX_BRAND_BYTES_ENV: &str = "RUSTANGO_BRAND_MAX_BYTES";
/// Default ceiling on a single brand asset. 1 MiB is plenty for a
/// logo / favicon and small enough to keep memory bounded under
/// concurrent uploads.
pub const DEFAULT_MAX_BRAND_BYTES: usize = 1_048_576;

/// Read the configured storage root. Each call hits the env once —
/// callers should cache the resulting [`BoxedStorage`] if they're
/// going to reach for it on every request.
#[must_use]
pub fn brand_storage_root() -> PathBuf {
    PathBuf::from(
        std::env::var(BRAND_STORAGE_ROOT_ENV)
            .unwrap_or_else(|_| DEFAULT_BRAND_STORAGE_ROOT.to_owned()),
    )
}

/// Read the configured per-asset byte cap.
#[must_use]
pub fn max_brand_bytes() -> usize {
    std::env::var(MAX_BRAND_BYTES_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MAX_BRAND_BYTES)
}

/// Default `BoxedStorage` for brand assets — `LocalStorage` rooted
/// at [`brand_storage_root`]. Used as the fallback when an admin /
/// operator console builder didn't get an explicit
/// `brand_storage(BoxedStorage)`.
///
/// Apps that want S3 / R2 / B2 / MinIO / a CDN-fronted bucket should
/// build their own [`BoxedStorage`] (any `Arc<dyn Storage>`) and
/// pass it via the builder setter — this is the one-liner default.
#[must_use]
pub fn default_brand_storage() -> BoxedStorage {
    Arc::new(LocalStorage::new(brand_storage_root()))
}

#[derive(Debug, thiserror::Error)]
pub enum BrandError {
    #[error("file too large: {actual} bytes (max {max})")]
    TooLarge { actual: usize, max: usize },
    #[error("unsupported content type `{0}`")]
    UnsupportedContentType(String),
    #[error("invalid slug")]
    InvalidSlug,
    #[error("invalid filename")]
    InvalidFilename,
    #[error("asset not found")]
    NotFound,
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
}

/// Which brand asset is being saved. Used to pick the stable on-disk
/// filename so a re-upload overwrites the previous one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrandAssetKind {
    Logo,
    Favicon,
}

impl BrandAssetKind {
    /// Stem the saved filename will use (e.g. `"logo"` → `"logo.png"`).
    #[must_use]
    pub const fn stem(self) -> &'static str {
        match self {
            Self::Logo => "logo",
            Self::Favicon => "favicon",
        }
    }
}

/// Allowed content types for the upload path. SVG is intentionally
/// missing — serving operator-supplied SVG is a stored-XSS sink and
/// not worth the complication for an admin UI.
const ALLOWED_CONTENT_TYPES: &[(&str, &str)] = &[
    ("image/png", "png"),
    ("image/jpeg", "jpg"),
    ("image/webp", "webp"),
    ("image/x-icon", "ico"),
    ("image/vnd.microsoft.icon", "ico"),
];

/// Build the storage key (`{slug}/{filename}`) where a brand asset
/// lives. Caller-validated slug; the function also rejects path
/// traversal in the filename.
fn storage_key(slug: &str, filename: &str) -> Result<String, BrandError> {
    if !is_safe_slug(slug) {
        return Err(BrandError::InvalidSlug);
    }
    if !is_safe_filename(filename) {
        return Err(BrandError::InvalidFilename);
    }
    Ok(format!("{slug}/{filename}"))
}

fn is_safe_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 64
        && slug
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

fn is_safe_filename(filename: &str) -> bool {
    !filename.is_empty()
        && filename.len() <= 64
        && !filename.contains("..")
        && !filename.contains('/')
        && !filename.contains('\\')
        && filename
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// Pick the content-type that should be served for `filename` based
/// on its extension. Returns `None` for anything outside the
/// allow-list — caller should 404.
#[must_use]
pub fn content_type_for(filename: &str) -> Option<&'static str> {
    let ext = std::path::Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)?;
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        _ => return None,
    })
}

/// Save uploaded bytes as the named brand asset. Returns the
/// relative filename (e.g. `"logo.png"`) — store this on
/// `Org.logo_path` / `Org.favicon_path`.
///
/// The `content_type` argument is the client-supplied one, treated
/// as advisory. The actual content-type at serve time is derived
/// from the saved filename's extension by [`content_type_for`].
///
/// # Errors
/// * [`BrandError::TooLarge`] when `bytes.len() > max_brand_bytes()`.
/// * [`BrandError::UnsupportedContentType`] for anything outside the
///   allow-list.
/// * [`BrandError::InvalidSlug`] / [`BrandError::InvalidFilename`]
///   for path-traversal attempts.
pub async fn save_brand_asset(
    slug: &str,
    kind: BrandAssetKind,
    bytes: &[u8],
    content_type: Option<&str>,
    storage: &BoxedStorage,
) -> Result<String, BrandError> {
    let max = max_brand_bytes();
    if bytes.len() > max {
        return Err(BrandError::TooLarge {
            actual: bytes.len(),
            max,
        });
    }
    let ct = content_type.unwrap_or("application/octet-stream").trim();
    let ext = ALLOWED_CONTENT_TYPES
        .iter()
        .find_map(|(c, e)| (c.eq_ignore_ascii_case(ct)).then_some(*e))
        .ok_or_else(|| BrandError::UnsupportedContentType(ct.to_owned()))?;
    let filename = format!("{}.{ext}", kind.stem());
    let key = storage_key(slug, &filename)?;
    storage.save(&key, bytes).await?;
    Ok(filename)
}

/// Read a previously-saved brand asset. Returns `(bytes, content_type)`.
///
/// # Errors
/// As [`save_brand_asset`], plus [`BrandError::NotFound`] when the
/// underlying storage has no such key.
pub async fn load_brand_asset(
    slug: &str,
    filename: &str,
    storage: &BoxedStorage,
) -> Result<(Vec<u8>, &'static str), BrandError> {
    let key = storage_key(slug, filename)?;
    let bytes = storage.load(&key).await.map_err(|e| match e {
        StorageError::NotFound(_) => BrandError::NotFound,
        other => other.into(),
    })?;
    let ct = content_type_for(filename).ok_or(BrandError::NotFound)?;
    Ok((bytes, ct))
}

// ----- color + theme validators -------------------------------------

/// Validate a `#RRGGBB` or `#RGB` hex color. Returns the lowercase
/// 6-digit form on success, or `None` for anything else.
#[must_use]
pub fn validate_hex_color(s: &str) -> Option<String> {
    let s = s.trim();
    let rest = s.strip_prefix('#')?;
    let valid = rest.chars().all(|c| c.is_ascii_hexdigit());
    if !valid {
        return None;
    }
    match rest.len() {
        6 => Some(format!("#{}", rest.to_ascii_lowercase())),
        3 => {
            let mut out = String::with_capacity(7);
            out.push('#');
            for c in rest.chars() {
                let lc = c.to_ascii_lowercase();
                out.push(lc);
                out.push(lc);
            }
            Some(out)
        }
        _ => None,
    }
}

/// Validate a `theme_mode` string. Returns one of `"light"`,
/// `"dark"`, `"auto"` — anything else is `None`.
#[must_use]
pub fn validate_theme_mode(s: &str) -> Option<&'static str> {
    match s.trim() {
        "light" => Some("light"),
        "dark" => Some("dark"),
        "auto" | "" => Some("auto"),
        _ => None,
    }
}

// ----- CSS variable builders ----------------------------------------

/// Build the per-tenant CSS variable override block from the org's
/// brand fields. Returns `None` when no overrides are set.
///
/// The returned string is a single line of `--name: value;` pairs,
/// safe to embed inside `<style>:root{ ... }` because every value
/// either comes from a server-controlled palette transformation or
/// is matched against the `^#[0-9a-fA-F]{3,6}$` shape.
#[must_use]
pub fn build_brand_css(org: &Org) -> Option<String> {
    build_brand_css_from_color(org.primary_color.as_deref())
}

/// Mirror of [`build_brand_css`] for the operator console (which
/// reads its primary color from env, not from `Org`).
#[must_use]
pub fn build_op_brand_css(primary_color: Option<&str>) -> Option<String> {
    build_brand_css_from_color(primary_color)
}

fn build_brand_css_from_color(primary_color: Option<&str>) -> Option<String> {
    let raw = primary_color?;
    let hex = validate_hex_color(raw)?;
    let hover = shade(&hex, 0.85);
    let soft = tint(&hex, 0.88);
    let mut out = String::with_capacity(160);
    out.push_str("--color-accent: ");
    out.push_str(&hex);
    out.push_str("; --color-accent-hover: ");
    out.push_str(&hover);
    out.push_str("; --color-accent-bg-soft: ");
    out.push_str(&soft);
    out.push(';');
    Some(out)
}

/// Multiply each channel of a `#rrggbb` color by `factor` (in 0..=1
/// to darken, >1 to lighten). Result is clamped to `[0, 255]` and
/// formatted lowercase.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn shade(hex: &str, factor: f64) -> String {
    let (r, g, b) = parse_rgb(hex).unwrap_or((176, 74, 44));
    let mul = |c: u8| ((f64::from(c) * factor).round().clamp(0.0, 255.0)) as u8;
    format!("#{:02x}{:02x}{:02x}", mul(r), mul(g), mul(b))
}

/// Mix `hex` with white by `factor` (`0.0` = pure original,
/// `1.0` = pure white). Used to derive the soft-bg accent.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn tint(hex: &str, factor: f64) -> String {
    let (r, g, b) = parse_rgb(hex).unwrap_or((176, 74, 44));
    let mix = |c: u8| {
        let v = f64::from(c) + (255.0 - f64::from(c)) * factor;
        v.round().clamp(0.0, 255.0) as u8
    };
    format!("#{:02x}{:02x}{:02x}", mix(r), mix(g), mix(b))
}

fn parse_rgb(hex: &str) -> Option<(u8, u8, u8)> {
    let rest = hex.strip_prefix('#')?;
    if rest.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&rest[0..2], 16).ok()?;
    let g = u8::from_str_radix(&rest[2..4], 16).ok()?;
    let b = u8::from_str_radix(&rest[4..6], 16).ok()?;
    Some((r, g, b))
}

/// Build the public URL for a tenant's brand asset.
///
/// 1. If `storage` exposes a direct URL for the key (S3/CDN-backed
///    or `LocalStorage::with_base_url`), use that — the browser
///    fetches the asset directly without proxying through the
///    framework.
/// 2. Otherwise fall back to the path-based handler the framework
///    mounts at `GET /__brand__/{slug}/{filename}`.
///
/// Returns `None` when `path` is empty / unset / unsafe.
#[must_use]
pub fn brand_asset_url(slug: &str, path: Option<&str>, storage: &BoxedStorage) -> Option<String> {
    let p = path?;
    if p.is_empty() || !is_safe_filename(p) || !is_safe_slug(slug) {
        return None;
    }
    let key = format!("{slug}/{p}");
    if let Some(direct) = storage.url(&key) {
        return Some(direct);
    }
    Some(format!("/__brand__/{slug}/{p}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn hex_color_accepts_six_digit() {
        assert_eq!(validate_hex_color("#B04A2C").as_deref(), Some("#b04a2c"));
        assert_eq!(validate_hex_color("#abcdef").as_deref(), Some("#abcdef"));
    }

    #[test]
    fn hex_color_accepts_three_digit() {
        assert_eq!(validate_hex_color("#fff").as_deref(), Some("#ffffff"));
        assert_eq!(validate_hex_color("#A1B").as_deref(), Some("#aa11bb"));
    }

    #[test]
    fn hex_color_rejects_garbage() {
        assert_eq!(validate_hex_color(""), None);
        assert_eq!(validate_hex_color("javascript:alert(1)"), None);
        assert_eq!(validate_hex_color("</style><script>"), None);
        assert_eq!(validate_hex_color("#zzzzzz"), None);
        assert_eq!(validate_hex_color("#1234"), None);
        assert_eq!(validate_hex_color("rgb(0,0,0)"), None);
    }

    #[test]
    fn theme_mode_accepts_known_values() {
        assert_eq!(validate_theme_mode("light"), Some("light"));
        assert_eq!(validate_theme_mode("dark"), Some("dark"));
        assert_eq!(validate_theme_mode("auto"), Some("auto"));
        assert_eq!(validate_theme_mode(""), Some("auto"));
        assert_eq!(validate_theme_mode("solarized"), None);
    }

    #[test]
    fn build_brand_css_is_none_when_no_color() {
        assert_eq!(build_brand_css_from_color(None), None);
        assert_eq!(build_brand_css_from_color(Some("")), None);
        assert_eq!(build_brand_css_from_color(Some("not-a-color")), None);
    }

    #[test]
    fn build_brand_css_emits_safe_assignments() {
        let css = build_brand_css_from_color(Some("#b04a2c")).unwrap();
        assert!(css.starts_with("--color-accent: #b04a2c"), "got: {css}");
        assert!(css.contains("--color-accent-hover: #"));
        assert!(css.contains("--color-accent-bg-soft: #"));
        // No path through the function can introduce </style> tokens.
        assert!(!css.contains('<'));
        assert!(!css.contains('>'));
    }

    #[test]
    fn safe_slug_rejects_traversal() {
        assert!(is_safe_slug("acme"));
        assert!(is_safe_slug("acme-corp"));
        assert!(is_safe_slug("a_b_c"));
        assert!(!is_safe_slug(""));
        assert!(!is_safe_slug("../etc/passwd"));
        assert!(!is_safe_slug("/abs"));
        assert!(!is_safe_slug("a b"));
    }

    #[test]
    fn safe_filename_rejects_traversal() {
        assert!(is_safe_filename("logo.png"));
        assert!(is_safe_filename("favicon.ico"));
        assert!(!is_safe_filename(""));
        assert!(!is_safe_filename("../logo.png"));
        assert!(!is_safe_filename("a/b.png"));
        assert!(!is_safe_filename("a\\b.png"));
        assert!(!is_safe_filename("évil.png")); // non-ASCII
    }

    #[test]
    fn content_type_for_known_extensions() {
        assert_eq!(content_type_for("logo.png"), Some("image/png"));
        assert_eq!(content_type_for("logo.PNG"), Some("image/png"));
        assert_eq!(content_type_for("logo.jpg"), Some("image/jpeg"));
        assert_eq!(content_type_for("logo.webp"), Some("image/webp"));
        assert_eq!(content_type_for("favicon.ico"), Some("image/x-icon"));
        assert_eq!(content_type_for("evil.svg"), None);
        assert_eq!(content_type_for("evil.exe"), None);
        assert_eq!(content_type_for("noext"), None);
    }

    #[test]
    fn brand_asset_url_falls_back_to_path_when_storage_has_no_url() {
        // InMemoryStorage's url() returns None — the path-based
        // fallback handler is the only way to reach the asset.
        let storage: BoxedStorage = Arc::new(crate::storage::InMemoryStorage::new());
        assert_eq!(
            brand_asset_url("acme", Some("logo.png"), &storage).as_deref(),
            Some("/__brand__/acme/logo.png"),
        );
    }

    #[test]
    fn brand_asset_url_prefers_direct_url_when_storage_exposes_one() {
        // LocalStorage with a base URL (or any S3-like backend)
        // returns a direct URL — that wins over the path-based form.
        let storage: BoxedStorage = Arc::new(
            LocalStorage::new(PathBuf::from("/tmp/_rustango_brand_test"))
                .with_base_url("https://cdn.example.com/brand"),
        );
        assert_eq!(
            brand_asset_url("acme", Some("logo.png"), &storage).as_deref(),
            Some("https://cdn.example.com/brand/acme/logo.png"),
        );
    }

    #[test]
    fn brand_asset_url_drops_invalid() {
        let storage: BoxedStorage = Arc::new(crate::storage::InMemoryStorage::new());
        assert_eq!(brand_asset_url("acme", None, &storage), None);
        assert_eq!(brand_asset_url("acme", Some(""), &storage), None);
        assert_eq!(brand_asset_url("acme", Some("../etc/passwd"), &storage), None);
        assert_eq!(brand_asset_url("../bad", Some("logo.png"), &storage), None);
    }

    #[test]
    fn shade_darkens_within_range() {
        let dark = shade("#b04a2c", 0.5);
        // Each channel halved (rounded).
        assert_eq!(dark, "#582516");
    }

    #[test]
    fn tint_lightens_toward_white() {
        let soft = tint("#b04a2c", 1.0);
        assert_eq!(soft, "#ffffff");
        let none = tint("#b04a2c", 0.0);
        assert_eq!(none, "#b04a2c");
    }

    /// Lock for tests that mutate `MAX_BRAND_BYTES_ENV` — prevents
    /// the parallel test runner from interleaving env mutations
    /// across different test bodies. Each entrant sets the env to a
    /// known value and resets to the default on exit.
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        static M: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        M.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    #[tokio::test]
    async fn save_and_load_round_trip_in_memory() {
        let _g = env_guard();
        std::env::remove_var(MAX_BRAND_BYTES_ENV);
        use crate::storage::InMemoryStorage;
        let storage: BoxedStorage = Arc::new(InMemoryStorage::new());
        let bytes = b"\x89PNG\r\n\x1a\nfake-png-bytes";
        let saved = save_brand_asset(
            "acme",
            BrandAssetKind::Logo,
            bytes,
            Some("image/png"),
            &storage,
        )
        .await
        .unwrap();
        assert_eq!(saved, "logo.png");

        let (loaded, ct) = load_brand_asset("acme", &saved, &storage).await.unwrap();
        assert_eq!(&loaded, bytes);
        assert_eq!(ct, "image/png");
    }

    #[tokio::test]
    async fn save_rejects_oversize() {
        let _g = env_guard();
        // Force a tiny cap.
        std::env::set_var(MAX_BRAND_BYTES_ENV, "10");
        let storage: BoxedStorage = Arc::new(crate::storage::InMemoryStorage::new());
        let bytes = vec![0_u8; 100];
        let err = save_brand_asset(
            "acme",
            BrandAssetKind::Logo,
            &bytes,
            Some("image/png"),
            &storage,
        )
        .await
        .unwrap_err();
        std::env::remove_var(MAX_BRAND_BYTES_ENV);
        assert!(matches!(err, BrandError::TooLarge { .. }), "got: {err}");
    }

    #[tokio::test]
    async fn save_rejects_unsupported_content_type() {
        use crate::storage::InMemoryStorage;
        let storage: BoxedStorage = Arc::new(InMemoryStorage::new());
        let err = save_brand_asset(
            "acme",
            BrandAssetKind::Logo,
            b"...",
            Some("text/html"),
            &storage,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, BrandError::UnsupportedContentType(_)),
            "got: {err}",
        );
    }

    #[tokio::test]
    async fn save_rejects_path_traversal_slug() {
        use crate::storage::InMemoryStorage;
        let storage: BoxedStorage = Arc::new(InMemoryStorage::new());
        let err = save_brand_asset(
            "../etc",
            BrandAssetKind::Logo,
            b"x",
            Some("image/png"),
            &storage,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, BrandError::InvalidSlug), "got: {err}");
    }
}
