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
//!     s3::{S3Storage, S3Config, S3Credentials},
//! };
//! use std::sync::Arc;
//!
//! let avatars: BoxedStorage = Arc::new(S3Storage::new(S3Config::new("avatars", "us-east-1"), creds.clone()));
//! let docs:    BoxedStorage = Arc::new(S3Storage::new(S3Config::new("docs", "us-east-1"), creds));
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
//!
//! Or declare the disks in `[storage]` and call
//! [`StorageRegistry::from_settings`]; see
//! [`StorageSettings`](crate::config::StorageSettings).

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
            let key = super::encode_key(key.trim_start_matches('/'));
            return Some(format!("{base}/{key}"));
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

#[cfg(feature = "config")]
impl StorageRegistry {
    /// One disk per `[storage.disks.<name>]`, with its CDN, and
    /// `[storage] default` as the default disk.
    ///
    /// # Errors
    /// [`StorageError::Io`] naming the disk when its driver is unknown or
    /// not compiled in, a required key is missing, its name is not
    /// `[a-z0-9_]` (env overrides arrive lowercased), or `default` names
    /// no disk.
    pub fn from_settings(
        settings: &crate::config::StorageSettings,
    ) -> Result<Self, super::StorageError> {
        let mut registry = Self::new();
        for (name, disk) in &settings.disks {
            let err = |e: super::StorageError| {
                super::StorageError::Io(format!("storage disk `{name}`: {e}"))
            };
            let env_safe = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_';
            if name.is_empty() || !name.chars().all(env_safe) {
                return Err(err(super::StorageError::Io(
                    "a disk name must be lowercase [a-z0-9_], so env overrides can reach it".into(),
                )));
            }
            let store = build_disk(disk, disk.endpoint.as_deref()).map_err(err)?;
            let store = match &disk.public_endpoint {
                Some(public) => Arc::new(PresignVia {
                    presign: build_disk(disk, Some(public)).map_err(err)?,
                    store,
                }),
                None => store,
            };
            registry = registry.set(name, store);
            if let Some(cdn) = &disk.cdn {
                registry = registry.cdn(name, cdn);
            }
        }
        if let Some(default) = &settings.default {
            if !registry.has(default) {
                return Err(super::StorageError::Io(format!(
                    "[storage] default `{default}` is not a disk"
                )));
            }
            registry = registry.with_default(default);
        }
        Ok(registry)
    }
}

/// The backend `disk.driver` names, reached at `endpoint`.
#[cfg(feature = "config")]
#[cfg_attr(
    not(any(feature = "storage-s3", feature = "storage-azure")),
    allow(unused_variables)
)]
fn build_disk(
    disk: &crate::config::DiskSettings,
    endpoint: Option<&str>,
) -> Result<BoxedStorage, super::StorageError> {
    use super::StorageError;
    let need = |v: &Option<String>, key: &str| {
        v.clone()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| StorageError::Io(format!("driver `{}` needs `{key}`", disk.driver)))
    };
    #[cfg(not(all(feature = "storage-s3", feature = "storage-azure")))]
    let missing = |feature: &str| {
        StorageError::Io(format!(
            "driver `{}` needs the `{feature}` cargo feature",
            disk.driver
        ))
    };
    match disk.driver.as_str() {
        "local" => {
            let mut s = super::LocalStorage::new(need(&disk.root, "root")?.into());
            if let Some(base) = &disk.base_url {
                s = s.with_base_url(base.clone());
            }
            Ok(Arc::new(s))
        }
        #[cfg(feature = "storage-s3")]
        "s3" => {
            use super::s3::{S3Config, S3Credentials, S3Storage};
            let mut cfg = S3Config::new(
                need(&disk.bucket, "bucket")?,
                disk.region.clone().unwrap_or_else(|| "us-east-1".into()),
            )
            .with_path_style(disk.path_style.unwrap_or(false));
            cfg.endpoint = endpoint.map(str::to_owned);
            let creds = S3Credentials::new(
                disk.access_key_id.clone().unwrap_or_default(),
                disk.secret_access_key.clone().unwrap_or_default(),
            );
            Ok(Arc::new(S3Storage::new(cfg, creds)))
        }
        #[cfg(not(feature = "storage-s3"))]
        "s3" => Err(missing("storage-s3")),
        #[cfg(feature = "storage-azure")]
        "azure" => Ok(Arc::new(super::azure::AzureBlobStorage::new(
            super::azure::AzureConfig {
                account: need(&disk.account, "account")?,
                account_key: need(&disk.account_key, "account_key")?,
                container: need(&disk.container, "container")?,
                endpoint: endpoint.map(str::to_owned),
            },
        )?)),
        #[cfg(not(feature = "storage-azure"))]
        "azure" => Err(missing("storage-azure")),
        other => Err(StorageError::Io(format!(
            "unknown driver `{other}` (local, s3, azure)"
        ))),
    }
}

/// A disk whose links point at `public_endpoint`: reads and writes go to
/// `store`, URLs come from `presign`, a copy built for the address
/// browsers reach.
#[cfg(feature = "config")]
struct PresignVia {
    store: BoxedStorage,
    presign: BoxedStorage,
}

#[cfg(feature = "config")]
#[async_trait::async_trait]
impl super::Storage for PresignVia {
    async fn save(&self, key: &str, data: &[u8]) -> Result<(), super::StorageError> {
        self.store.save(key, data).await
    }

    async fn save_with_content_type(
        &self,
        key: &str,
        data: &[u8],
        content_type: Option<&str>,
    ) -> Result<(), super::StorageError> {
        self.store
            .save_with_content_type(key, data, content_type)
            .await
    }

    async fn load(&self, key: &str) -> Result<Vec<u8>, super::StorageError> {
        self.store.load(key).await
    }

    async fn delete(&self, key: &str) -> Result<(), super::StorageError> {
        self.store.delete(key).await
    }

    async fn exists(&self, key: &str) -> Result<bool, super::StorageError> {
        self.store.exists(key).await
    }

    fn url(&self, key: &str) -> Option<String> {
        self.presign.url(key)
    }

    async fn presigned_get_url(&self, key: &str, ttl: std::time::Duration) -> Option<String> {
        self.presign.presigned_get_url(key, ttl).await
    }

    async fn presigned_put_url(
        &self,
        key: &str,
        ttl: std::time::Duration,
        put: &super::PutConditions,
    ) -> Option<String> {
        self.presign.presigned_put_url(key, ttl, put).await
    }

    fn presigned_put_enforces_conditions(&self) -> bool {
        self.presign.presigned_put_enforces_conditions()
    }

    fn presigned_put_headers(
        &self,
        put: &super::PutConditions,
    ) -> std::collections::BTreeMap<String, String> {
        self.presign.presigned_put_headers(put)
    }

    async fn metadata(&self, key: &str) -> Result<Option<super::ObjectMeta>, super::StorageError> {
        self.store.metadata(key).await
    }

    async fn save_stream(
        &self,
        key: &str,
        len: u64,
        body: super::ByteStream,
        put: &super::PutConditions,
    ) -> Result<(), super::StorageError> {
        self.store.save_stream(key, len, body, put).await
    }

    async fn load_stream(&self, key: &str) -> Result<super::ByteStream, super::StorageError> {
        self.store.load_stream(key).await
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
    fn urls_percent_encode_the_key() {
        let r = StorageRegistry::new()
            .set(
                "a",
                StdArc::new(LocalStorage::new(PathBuf::from("/tmp")).with_base_url("/m")),
            )
            .cdn("a", "https://cdn.x");
        assert_eq!(
            r.cdn_url("a", "docs/Q3 report #2.pdf").as_deref(),
            Some("https://cdn.x/docs/Q3%20report%20%232.pdf")
        );
        assert_eq!(
            r.origin_url("a", "a?b.png").as_deref(),
            Some("/m/a%3Fb.png")
        );
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

    #[cfg(feature = "config")]
    fn settings(toml: &str) -> crate::config::StorageSettings {
        toml::from_str(toml).unwrap()
    }

    /// Several S3 disks, and other drivers, side by side.
    #[cfg(all(feature = "config", feature = "storage-s3"))]
    #[tokio::test]
    async fn from_settings_builds_every_disk() {
        let r = StorageRegistry::from_settings(&settings(
            r#"
            default = "uploads"
            [disks.uploads]
            driver = "s3"
            bucket = "up"
            endpoint = "http://minio:9000"
            public_endpoint = "https://files.example.com"
            path_style = true
            [disks.archive]
            driver = "s3"
            bucket = "arc"
            cdn = "https://cdn.example.com/arc"
            [disks.cache]
            driver = "local"
            root = "/tmp/cache"
            "#,
        ))
        .unwrap();
        assert_eq!(r.names(), ["archive", "cache", "uploads"]);
        assert_eq!(r.default_name(), Some("uploads"));
        // Links use the public endpoint, not the in-cluster one.
        assert_eq!(
            r.origin_url("uploads", "a.csv").as_deref(),
            Some("https://files.example.com/up/a.csv")
        );
        assert_eq!(
            r.cdn_url("archive", "x").as_deref(),
            Some("https://cdn.example.com/arc/x")
        );
        // Presigned links are signed for the public endpoint too (tests-005).
        let up = r.disk("uploads").unwrap();
        let ttl = std::time::Duration::from_secs(60);
        let public = "https://files.example.com/up/a.csv?";
        let get = up.presigned_get_url("a.csv", ttl).await.unwrap();
        assert!(get.starts_with(public), "{get}");
        let put = crate::storage::PutConditions::new();
        let put = up.presigned_put_url("a.csv", ttl, &put).await.unwrap();
        assert!(put.starts_with(public), "{put}");
        assert!(up.presigned_put_enforces_conditions());
    }

    /// tests-005 — reads and writes go to `store`, links to `presign`.
    #[cfg(all(feature = "config", feature = "storage-s3"))]
    #[tokio::test]
    async fn presign_via_stores_in_one_and_signs_with_the_other() {
        use crate::storage::Storage as _;
        let disk = settings(
            r#"
            [disks.up]
            driver = "s3"
            bucket = "up"
            path_style = true
            "#,
        )
        .disks
        .remove("up")
        .unwrap();
        let via = PresignVia {
            store: Arc::new(crate::storage::InMemoryStorage::new()),
            presign: build_disk(&disk, Some("https://files.example.com")).unwrap(),
        };
        via.save("a.csv", b"abc").await.unwrap();
        assert_eq!(via.load("a.csv").await.unwrap(), b"abc");
        assert_eq!(via.metadata("a.csv").await.unwrap().unwrap().size, 3);
        // Forwarded: the trait default would refuse create-only.
        let once = crate::storage::PutConditions::new().create_only();
        let body = crate::storage::ByteStream::from(b"xyz".to_vec());
        via.save_stream("b.csv", 3, body, &once).await.unwrap();
        let mut got = via.load_stream("b.csv").await.unwrap();
        let chunk = futures_util::StreamExt::next(&mut got).await.unwrap();
        assert_eq!(&chunk.unwrap()[..], b"xyz");
        let ttl = std::time::Duration::from_secs(60);
        let get = via.presigned_get_url("a.csv", ttl).await.unwrap();
        assert!(
            get.starts_with("https://files.example.com/up/a.csv?"),
            "{get}"
        );
    }

    /// correctness-002 — a name env overrides cannot reach is refused.
    #[cfg(feature = "config")]
    #[test]
    fn from_settings_refuses_a_name_env_cannot_reach() {
        for name in ["Uploads", "user-media"] {
            let toml = format!("[disks.\"{name}\"]\ndriver = \"local\"\nroot = \"/tmp/x\"\n");
            let err = StorageRegistry::from_settings(&settings(&toml))
                .err()
                .unwrap();
            assert!(err.to_string().contains("lowercase"), "{err}");
        }
    }

    #[cfg(all(feature = "config", feature = "storage-azure"))]
    #[tokio::test]
    async fn from_settings_builds_an_azure_disk() {
        let r = StorageRegistry::from_settings(&settings(
            r#"
            [disks.blobs]
            driver = "azure"
            account = "acct"
            account_key = "AAAA"
            container = "media"
            "#,
        ))
        .unwrap();
        let disk = r.disk("blobs").unwrap();
        assert_eq!(
            disk.url("k").as_deref(),
            Some("https://acct.blob.core.windows.net/media/k")
        );
        let put = crate::storage::PutConditions::new();
        assert!(disk
            .presigned_put_headers(&put)
            .contains_key("x-ms-blob-type"));
    }

    #[cfg(feature = "config")]
    #[test]
    fn from_settings_names_the_disk_that_is_wrong() {
        let err = |toml: &str| {
            StorageRegistry::from_settings(&settings(toml))
                .unwrap_err()
                .to_string()
        };
        let e = err("[disks.x]\ndriver = \"ftp\"");
        assert!(
            e.contains("disk `x`") && e.contains("unknown driver"),
            "{e}"
        );
        let e = err("[disks.x]\ndriver = \"local\"");
        assert!(e.contains("needs `root`"), "{e}");
        let e = err("default = \"nope\"");
        assert!(e.contains("`nope` is not a disk"), "{e}");
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
