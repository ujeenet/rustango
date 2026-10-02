//! A registry of named storage disks. Look one up by name, get its
//! URL through a CDN if it has one, or fall back to a default disk.
//!
//! One app can hold several storages at once: `"avatars"` on S3
//! behind a CDN, `"docs"` in a private bucket, `"cache"` on local
//! disk. Laravel calls these filesystem disks.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::storage::{
//!     LocalStorage, BoxedStorage,
//!     registry::StorageRegistry,
//!     s3::{S3Storage, S3Config},
//! };
//! use std::sync::Arc;
//!
//! let avatars: BoxedStorage = Arc::new(S3Storage::new(S3Config { /* ... */ }));
//! let docs:    BoxedStorage = Arc::new(S3Storage::new(S3Config { /* ... */ }));
//! let cache:   BoxedStorage = Arc::new(LocalStorage::new("./cache".into()));
//!
//! let registry = StorageRegistry::new()
//!     .set("avatars", avatars).cdn("avatars", "https://cdn.example.com/avatars")
//!     .set("docs",    docs)
//!     .set("cache",   cache)
//!     .with_default("avatars");
//!
//! // Resolve by name:
//! let s = registry.disk("avatars").unwrap();
//! s.save("alice.png", &png).await?;
//!
//! // Or use the default:
//! registry.default_disk().unwrap().save("k", b"v").await?;
//!
//! // Uses the CDN prefix when the disk has one, else the backend's
//! // own url().
//! let url = registry.cdn_url("avatars", "alice.png");
//! //  -> "https://cdn.example.com/avatars/alice.png"
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use super::BoxedStorage;

/// Named storage disks. Cheap to clone; the state is behind an
/// `Arc`.
#[derive(Clone, Default)]
pub struct StorageRegistry {
    inner: Arc<RegistryInner>,
}

#[derive(Default)]
struct RegistryInner {
    disks: HashMap<String, BoxedStorage>,
    cdns: HashMap<String, String>, // disk -> CDN base URL
    default_name: Option<String>,
}

impl StorageRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `storage` under `name`, replacing any disk already
    /// registered there.
    #[must_use]
    pub fn set(self, name: impl Into<String>, storage: BoxedStorage) -> Self {
        let mut inner = (*self.inner).rebuild();
        inner.disks.insert(name.into(), storage);
        Self {
            inner: Arc::new(inner),
        }
    }

    /// Set the CDN base URL for a disk. `cdn_url(disk, key)` then
    /// joins it with the key. A trailing slash on `base` is fine.
    #[must_use]
    pub fn cdn(self, disk: impl Into<String>, base: impl Into<String>) -> Self {
        let mut inner = (*self.inner).rebuild();
        let base = base.into().trim_end_matches('/').to_owned();
        inner.cdns.insert(disk.into(), base);
        Self {
            inner: Arc::new(inner),
        }
    }

    /// Mark `name` as the default disk, which `default_disk` returns
    /// and helpers use when no disk is named. It is not called
    /// `default`, to avoid clashing with `Default::default`.
    #[must_use]
    pub fn with_default(self, name: impl Into<String>) -> Self {
        let mut inner = (*self.inner).rebuild();
        inner.default_name = Some(name.into());
        Self {
            inner: Arc::new(inner),
        }
    }

    /// Look up a disk by name.
    #[must_use]
    pub fn disk(&self, name: &str) -> Option<BoxedStorage> {
        self.inner.disks.get(name).cloned()
    }

    /// The default disk. `None` when no default was set, or when the
    /// name it points at was never registered.
    #[must_use]
    pub fn default_disk(&self) -> Option<BoxedStorage> {
        self.inner
            .default_name
            .as_deref()
            .and_then(|n| self.disk(n))
    }

    /// The name of the default disk, if any.
    #[must_use]
    pub fn default_name(&self) -> Option<&str> {
        self.inner.default_name.as_deref()
    }

    /// All registered disk names, sorted alphabetically.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        let mut out: Vec<String> = self.inner.disks.keys().cloned().collect();
        out.sort();
        out
    }

    /// `true` when `name` is registered.
    #[must_use]
    pub fn has(&self, name: &str) -> bool {
        self.inner.disks.contains_key(name)
    }

    /// The public URL for a key: the disk's CDN base when it has one,
    /// else the backend's own `url(key)`. `None` when the disk is
    /// unknown or the backend has no URLs.
    #[must_use]
    pub fn cdn_url(&self, disk: &str, key: &str) -> Option<String> {
        if let Some(base) = self.inner.cdns.get(disk) {
            return Some(format!("{base}/{}", key.trim_start_matches('/')));
        }
        self.inner.disks.get(disk).and_then(|s| s.url(key))
    }

    /// Like `cdn_url`, but always the backend's own URL, skipping
    /// any CDN. Use it when the CDN should not be involved, such as
    /// an admin-only download.
    #[must_use]
    pub fn origin_url(&self, disk: &str, key: &str) -> Option<String> {
        self.inner.disks.get(disk).and_then(|s| s.url(key))
    }

    /// The configured CDN base for `disk`, if any.
    #[must_use]
    pub fn cdn_base(&self, disk: &str) -> Option<&str> {
        self.inner.cdns.get(disk).map(String::as_str)
    }
}

impl RegistryInner {
    /// Copy the inner state, so a builder method can run on a shared
    /// `Arc` without every caller cloning the maps first.
    fn rebuild(&self) -> Self {
        Self {
            disks: self.disks.clone(),
            cdns: self.cdns.clone(),
            default_name: self.default_name.clone(),
        }
    }
}

impl std::fmt::Debug for StorageRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageRegistry")
            .field("disks", &self.names())
            .field("default", &self.default_name())
            .field("cdns", &self.inner.cdns.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{InMemoryStorage, LocalStorage};
    use std::path::PathBuf;
    use std::sync::Arc as StdArc;

    fn mem() -> BoxedStorage {
        StdArc::new(InMemoryStorage::new())
    }

    fn local() -> BoxedStorage {
        StdArc::new(LocalStorage::new(PathBuf::from("/tmp")))
    }

    #[test]
    fn empty_registry_resolves_nothing() {
        let r = StorageRegistry::new();
        assert!(r.disk("avatars").is_none());
        assert!(r.default_disk().is_none());
        assert!(r.names().is_empty());
        assert!(!r.has("anything"));
    }

    #[test]
    fn set_then_resolve_by_name() {
        let r = StorageRegistry::new()
            .set("avatars", mem())
            .set("docs", mem());
        assert!(r.disk("avatars").is_some());
        assert!(r.disk("docs").is_some());
        assert!(r.disk("missing").is_none());
        assert!(r.has("avatars"));
        assert_eq!(r.names(), vec!["avatars".to_owned(), "docs".to_owned()]);
    }

    #[test]
    fn set_replaces_existing_disk() {
        let s1 = mem();
        let s2 = mem();
        let r = StorageRegistry::new()
            .set("k", s1.clone())
            .set("k", s2.clone());
        // Same name, different backend: the second one wins.
        let resolved = r.disk("k").unwrap();
        assert!(StdArc::ptr_eq(&resolved, &s2));
        assert!(!StdArc::ptr_eq(&resolved, &s1));
    }

    #[test]
    fn default_disk_resolves_when_set() {
        let r = StorageRegistry::new()
            .set("avatars", mem())
            .set("docs", mem())
            .with_default("docs");
        assert_eq!(r.default_name(), Some("docs"));
        assert!(r.default_disk().is_some());
    }

    #[test]
    fn default_disk_returns_none_when_target_unregistered() {
        // Naming a default that does not exist is not an error when
        // building; the lookup just returns None.
        let r = StorageRegistry::new().with_default("missing");
        assert_eq!(r.default_name(), Some("missing"));
        assert!(r.default_disk().is_none());
    }

    #[test]
    fn cdn_url_uses_configured_prefix() {
        let r = StorageRegistry::new()
            .set("avatars", mem())
            .cdn("avatars", "https://cdn.example.com/avatars");
        assert_eq!(
            r.cdn_url("avatars", "alice.png").as_deref(),
            Some("https://cdn.example.com/avatars/alice.png")
        );
    }

    #[test]
    fn cdn_url_strips_leading_slash_on_key() {
        let r = StorageRegistry::new()
            .set("a", mem())
            .cdn("a", "https://cdn.example.com");
        assert_eq!(
            r.cdn_url("a", "/foo.png").as_deref(),
            Some("https://cdn.example.com/foo.png")
        );
    }

    #[test]
    fn cdn_url_strips_trailing_slash_from_base() {
        let r = StorageRegistry::new()
            .set("a", mem())
            .cdn("a", "https://cdn.example.com/");
        assert_eq!(
            r.cdn_url("a", "k.png").as_deref(),
            Some("https://cdn.example.com/k.png")
        );
    }

    #[test]
    fn cdn_url_falls_back_to_backend_url_when_no_cdn() {
        // LocalStorage has no URL by default, so with no CDN base
        // cdn_url has nothing to return.
        let r = StorageRegistry::new().set("local", local());
        assert!(r.cdn_url("local", "k.txt").is_none());
    }

    #[test]
    fn cdn_url_unknown_disk_returns_none() {
        let r = StorageRegistry::new();
        assert!(r.cdn_url("missing", "k").is_none());
    }

    #[test]
    fn origin_url_bypasses_cdn() {
        // A CDN is configured, but the caller wants the plain
        // backend URL, as an internal admin tool would.
        let local_with_url: BoxedStorage = StdArc::new(
            LocalStorage::new(PathBuf::from("/tmp"))
                .with_base_url("https://internal.example.com/files"),
        );
        let r = StorageRegistry::new()
            .set("a", local_with_url)
            .cdn("a", "https://cdn.example.com");
        // CDN URL goes through the CDN.
        assert_eq!(
            r.cdn_url("a", "k.txt").as_deref(),
            Some("https://cdn.example.com/k.txt")
        );
        // Origin URL goes straight to the backend.
        assert_eq!(
            r.origin_url("a", "k.txt").as_deref(),
            Some("https://internal.example.com/files/k.txt")
        );
    }

    #[test]
    fn cdn_base_returns_configured_value() {
        let r = StorageRegistry::new()
            .set("a", mem())
            .cdn("a", "https://cdn.example.com");
        assert_eq!(r.cdn_base("a"), Some("https://cdn.example.com"));
        assert!(r.cdn_base("b").is_none());
    }

    #[test]
    fn names_are_sorted() {
        let r = StorageRegistry::new()
            .set("z", mem())
            .set("a", mem())
            .set("m", mem());
        assert_eq!(
            r.names(),
            vec!["a".to_owned(), "m".to_owned(), "z".to_owned()]
        );
    }

    #[test]
    fn debug_renders_disk_names_and_default() {
        let r = StorageRegistry::new()
            .set("a", mem())
            .set("b", mem())
            .cdn("a", "https://cdn")
            .with_default("a");
        let s = format!("{r:?}");
        assert!(s.contains("\"a\""));
        assert!(s.contains("\"b\""));
        assert!(s.contains("Some(\"a\")"));
    }

    #[test]
    fn registry_clone_shares_arc_state() {
        let r = StorageRegistry::new().set("a", mem());
        let r2 = r.clone();
        // Both clones point at the same disk.
        assert!(StdArc::ptr_eq(
            &r.disk("a").unwrap(),
            &r2.disk("a").unwrap()
        ));
    }
}
