//! First-class `Media` model — a real `#[derive(Model)]` row that
//! references a file in the [`crate::storage::Storage`] layer.
//!
//! Other models reference media via `Option<ForeignKey<Media>>`
//! (a normal integer FK column). All metadata — disk, key, MIME,
//! size, original filename, derived-from chain, custom JSONB —
//! lives on the Media row, so deletes are atomic and the admin can
//! browse uploads with no extra wiring.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::media::{MediaManager, SaveOpts};
//! use rustango::storage::StorageRegistry;
//! use std::sync::Arc;
//! use std::time::Duration;
//!
//! // Tables come from the framework's system migrations.
//!
//! let registry = StorageRegistry::new()
//!     .set("avatars", Arc::new(s3_storage))
//!     .with_default("avatars");
//!
//! // `new_pool` works on all three backends. `MediaManager::new` is
//! // Postgres-only and takes a `PgPool`.
//! let manager = MediaManager::new_pool(pool.clone(), registry);
//!
//! // Server-side save (small files):
//! let media = manager.save_bytes(SaveOpts {
//!     disk: "avatars".into(),
//!     key_prefix: "users/".into(),
//!     bytes: png_bytes.clone(),
//!     mime: "image/png".into(),
//!     original_filename: "alice.png".into(),
//!     uploaded_by_id: Some(42),
//!     collection_id: None,
//!     metadata: serde_json::json!({}),
//! }).await?;
//!
//! // Read:
//! let url = manager.url(&media);                  // CDN-aware
//! let download = manager.presigned_get(&media, Duration::from_secs(3600)).await;
//!
//! // Soft-delete the row. The storage object survives, so a presigned
//! // URL minted before this keeps working until its TTL expires —
//! // `purge` is what actually revokes access.
//! manager.delete(&media).await?;
//! ```
//!
//! The REST router over this surface lives in [`router`] and needs the
//! `admin` feature. It requires an authorization policy: `media_router`
//! is deprecated and answers `403` to everything, and
//! `router::media_router_with` takes a [`router::MediaAuthorizer`].
//!
//! ## Schema
//!
//! The `rustango_media` table, and the collection / tag / tag-link
//! tables, are managed `#[derive(Model)]`s, so their schema ships as
//! ordinary system migrations. See [`Media`] for the columns.
//!
//! [`Media`]: crate::media::Media
//! [`router`]: crate::media::router
//! [`router::MediaAuthorizer`]: crate::media::router::MediaAuthorizer

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(feature = "postgres")]
use sqlx::PgPool;

use crate::core::Column as _;
use crate::sql::Auto;
use crate::storage::{StorageError, StorageRegistry};

pub mod collection;
pub mod tag;
pub use collection::MediaCollection;
pub use tag::{MediaTag, MediaTagLink};

// No outer `///` here on purpose. A doc comment at a module's
// declaration site joins the module's own `//!` block and resolves in
// *this* module's scope, which breaks every relative intra-doc link in
// router.rs. Its header states the `admin` requirement instead.
#[cfg(feature = "admin")]
pub mod router;

/// Re-exported so implementing [`router::MediaAuthorizer`] does not
/// need `async-trait` in the integrator's own `Cargo.toml`, and cannot
/// drift from the version the trait was declared with.
#[cfg(feature = "admin")]
pub use async_trait::async_trait;

const DEFAULT_DISK_NAME: &str = "default";

/// Ceiling on any listing's page size. Matches the clamp
/// [`MediaManager::list_with_tag`] and [`MediaManager::popular_tags`]
/// already applied, so the whole surface has one bound.
const MAX_LIST_LIMIT: i64 = 1000;

/// Most distinct slugs one [`MediaManager::tag`] / [`MediaManager::set_tags`]
/// call takes. Its `IN` list and two-bind insert rows fit every dialect's bind cap.
const MAX_TAGS_PER_CALL: usize = MAX_LIST_LIMIT as usize;
const _: () = assert!(MAX_TAGS_PER_CALL * 2 <= 32766);

/// Largest offset a recursive listing takes once its subtree needs more
/// than one `IN` list: each list then loads `offset + limit` rows.
const MAX_WIDE_OFFSET: i64 = 10_000;

/// Rows [`MediaManager::purge_pending`] deletes per call.
///
/// Bounds how much one run does: the sweep reads this many rows, then
/// deletes each with its object.
///
/// A bigger backlog drains over several runs. A sweep that finishes
/// late beats one that blocks every other writer.
const PURGE_PENDING_BATCH: i64 = 10_000;

/// Default cap on a direct upload's declared size: 100 MiB.
pub const DEFAULT_MAX_UPLOAD_BYTES: u64 = 100 << 20;

/// Page size when a caller names none. Below [`MAX_LIST_LIMIT`] on
/// purpose: an unpaged listing should return a reasonable page, not
/// the largest one a caller could ask for.
const DEFAULT_LIST_CAP: i64 = 100;

/// Lifecycle state of a Media row.
///
/// - `Pending` — the row exists but the storage object is not
///   confirmed yet. Usual for direct browser uploads: the row is
///   created when the presigned URL is issued, the object lands later.
/// - `Ready` — the storage object is confirmed to exist.
/// - `Failed` — finalize ran but the object was not there.
///
/// Stored as a single TEXT column so callers can filter and order on
/// it with no special type handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaStatus {
    Pending,
    Ready,
    Failed,
}

impl MediaStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }

    #[must_use]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "ready" => Some(Self::Ready),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// First-class media row. Always referenced from user models via
/// `Option<ForeignKey<Media>>` rather than embedded directly.
///
/// Managed `#[derive(Model)]` on the `rustango_media` table. Its
/// schema and the `(disk, storage_key)` / `status` / `collection_id`
/// indexes ship as system migrations.
#[derive(crate::Model, Debug, Clone)]
// `permissions` so `auto_create_permissions` seeds
// `rustango_media.{add,change,delete,view}`, the codenames
// `router::MediaPerms` checks. Not a column, so no migration.
#[rustango(table = "rustango_media", index("disk, storage_key"), permissions)]
pub struct Media {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 255)]
    pub disk: String,
    #[rustango(max_length = 512)]
    pub storage_key: String,
    #[rustango(max_length = 255)]
    pub mime: String,
    pub size_bytes: i64,
    #[rustango(max_length = 512)]
    pub original_filename: String,
    /// MediaStatus serialized as &str (pending/ready/failed).
    #[rustango(max_length = 32, index)]
    pub status: String,
    /// Set on INSERT via the per-dialect `DEFAULT NOW()`.
    #[rustango(auto_now_add)]
    pub uploaded_at: Auto<DateTime<Utc>>,
    /// Soft FK to your User table.
    pub uploaded_by_id: Option<i64>,
    /// Self-FK for variants / thumbnails.
    pub derived_from_id: Option<i64>,
    /// Optional FK to `rustango_media_collections.id` — the
    /// "where it lives" folder. NULL means "loose" (in the root).
    #[rustango(index)]
    pub collection_id: Option<i64>,
    #[rustango(default = "'{}'")]
    pub metadata: Value,
    /// Soft delete.
    pub deleted_at: Option<DateTime<Utc>>,
}

impl Media {
    /// Typed status accessor.
    #[must_use]
    pub fn status_enum(&self) -> Option<MediaStatus> {
        MediaStatus::from_str(&self.status)
    }

    /// `true` when the storage object is confirmed present.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.status_enum() == Some(MediaStatus::Ready)
    }
}

// =====================================================================
// Errors
// =====================================================================

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("unknown disk: {0} (configure via StorageRegistry::set)")]
    UnknownDisk(String),
    #[error("storage: {0}")]
    Storage(#[from] StorageError),
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
    #[error("{0}")]
    Other(String),
}

// =====================================================================
// Save options
// =====================================================================

/// Arguments to [`MediaManager::save_bytes`].
#[derive(Debug, Clone)]
pub struct SaveOpts {
    /// Disk name to write to. Must be registered in the
    /// [`StorageRegistry`].
    pub disk: String,
    /// Optional path prefix prepended to the generated key
    /// (e.g. `"users/"` -> `"users/{uuid}.png"`). Trailing `/` is
    /// optional.
    pub key_prefix: String,
    /// File body.
    pub bytes: Vec<u8>,
    /// MIME type (caller is responsible for trusting / validating
    /// this; for direct browser uploads the client always lies).
    pub mime: String,
    pub original_filename: String,
    pub uploaded_by_id: Option<i64>,
    /// Optional collection (folder) to drop the new row into. `None`
    /// means "loose" / unfiled. See [`MediaCollection`].
    pub collection_id: Option<i64>,
    /// Free-form JSONB metadata — EXIF, image dimensions, ICC,
    /// whatever the app wants to keep alongside the file.
    pub metadata: Value,
}

/// Arguments to [`MediaManager::begin_upload`] (direct browser flow).
#[derive(Debug, Clone)]
pub struct UploadIntent {
    pub disk: String,
    pub key_prefix: String,
    pub mime: String,
    pub original_filename: String,
    pub size_bytes: i64,
    pub uploaded_by_id: Option<i64>,
    pub collection_id: Option<i64>,
    /// How long the presigned PUT URL stays valid. Default 5 min.
    pub ttl: Duration,
}

impl UploadIntent {
    pub fn new(
        disk: impl Into<String>,
        mime: impl Into<String>,
        original_filename: impl Into<String>,
        size_bytes: i64,
    ) -> Self {
        Self {
            disk: disk.into(),
            key_prefix: String::new(),
            mime: mime.into(),
            original_filename: original_filename.into(),
            size_bytes,
            uploaded_by_id: None,
            collection_id: None,
            ttl: Duration::from_secs(300),
        }
    }
}

/// Server response to [`MediaManager::begin_upload`] — the row id
/// the browser will reference, the URL it should PUT to, and an
/// expiry hint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadTicket {
    pub media_id: i64,
    pub upload_url: String,
    pub expires_at: DateTime<Utc>,
    /// Echoed back so the caller can confirm what they signed for.
    pub disk: String,
    pub storage_key: String,
    /// The `Content-Type` the browser must send. An active type
    /// (HTML, SVG, script) is signed as `application/octet-stream`.
    #[serde(default)]
    pub content_type: String,
    /// Every header the PUT must carry, `content-type` and
    /// `if-none-match` included.
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
}

// =====================================================================
// MediaManager
// =====================================================================

/// Glue between the `Media` model and a [`StorageRegistry`]. Cheap
/// to clone — internal state is `Arc`-shared.
///
/// Every query method dispatches per backend (PG / MySQL 8+ / SQLite)
/// through the [`crate::sql::Pool`] enum, so PG-only idioms like
/// `ANY($1)` or `ON CONFLICT DO UPDATE` are rewritten per dialect.
#[derive(Clone)]
pub struct MediaManager {
    pool: crate::sql::Pool,
    registry: StorageRegistry,
    max_upload_bytes: u64,
}

impl MediaManager {
    /// PG back-compat constructor.
    #[cfg(feature = "postgres")]
    #[must_use]
    pub fn new(pool: PgPool, registry: StorageRegistry) -> Self {
        Self::new_pool(crate::sql::Pool::Postgres(pool), registry)
    }

    /// Constructor that works on all three backends.
    #[must_use]
    pub fn new_pool(pool: impl Into<crate::sql::Pool>, registry: StorageRegistry) -> Self {
        Self {
            pool: pool.into(),
            registry,
            max_upload_bytes: DEFAULT_MAX_UPLOAD_BYTES,
        }
    }

    /// Refuse a [`Self::begin_upload`] that declares more than `bytes`.
    /// Default [`DEFAULT_MAX_UPLOAD_BYTES`].
    #[must_use]
    pub fn with_max_upload_bytes(mut self, bytes: u64) -> Self {
        self.max_upload_bytes = bytes;
        self
    }

    #[must_use]
    pub fn registry(&self) -> &StorageRegistry {
        &self.registry
    }

    /// The pool, on any backend; `.as_postgres()` for the PG one (#2070).
    #[must_use]
    pub fn pool(&self) -> &crate::sql::Pool {
        &self.pool
    }

    /// Same as [`Self::pool`].
    #[deprecated(since = "0.60.1", note = "use `pool()`")]
    #[must_use]
    pub fn pool_dyn(&self) -> &crate::sql::Pool {
        &self.pool
    }

    fn resolve_disk(&self, name: &str) -> Result<crate::storage::BoxedStorage, MediaError> {
        self.registry
            .disk(name)
            .ok_or_else(|| MediaError::UnknownDisk(name.to_owned()))
    }

    // --------- save_bytes (server-side write)

    /// Write `opts.bytes` to the storage backend, then insert a
    /// `Media` row in the `Ready` state. Returns the inserted row.
    ///
    /// # Errors
    /// `UnknownDisk` if the disk isn't registered, `Storage` for any
    /// upload failure, `Db` for the row insert.
    pub async fn save_bytes(&self, opts: SaveOpts) -> Result<Media, MediaError> {
        let storage = self.resolve_disk(&opts.disk)?;
        let key = build_key(&opts.key_prefix, &opts.original_filename);
        let size_bytes = opts.bytes.len() as i64;
        storage
            .save_with_content_type(&key, &opts.bytes, Some(stored_content_type(&opts.mime)))
            .await?;
        let inserted = self
            .insert_row(InsertRow {
                disk: opts.disk,
                storage_key: key.clone(),
                mime: opts.mime,
                size_bytes,
                original_filename: opts.original_filename,
                status: MediaStatus::Ready,
                uploaded_by_id: opts.uploaded_by_id,
                derived_from_id: None,
                collection_id: opts.collection_id,
                metadata: opts.metadata,
            })
            .await;
        if inserted.is_err() {
            // No row means no sweep will ever find this object (#1905).
            let _ = storage.delete(&key).await;
        }
        inserted
    }

    // --------- begin / finalize (direct browser upload)

    /// Issue a presigned PUT URL for a direct browser upload, and
    /// create a `Media` row in `Pending` state. The browser PUTs
    /// straight to S3 (or compatible). The server then calls
    /// [`Self::finalize_upload`] to check the object landed and flip
    /// the row to `Ready`.
    ///
    /// # Errors
    /// `UnknownDisk`, `Db`, or `Storage` if the backend cannot sign
    /// URLs. `Other` for a negative size or one over the upload limit.
    pub async fn begin_upload(&self, intent: UploadIntent) -> Result<UploadTicket, MediaError> {
        let storage = self.resolve_disk(&intent.disk)?;
        let size = u64::try_from(intent.size_bytes).map_err(|_| {
            MediaError::Other(format!("negative size_bytes: {}", intent.size_bytes))
        })?;
        if size > self.max_upload_bytes {
            return Err(MediaError::Other(format!(
                "size_bytes {size} is over the {} byte upload limit",
                self.max_upload_bytes
            )));
        }
        let key = build_key(&intent.key_prefix, &intent.original_filename);
        // Sign a safe type and the declared size: the bucket stores no
        // active MIME and refuses a bigger body (#2057, #1851).
        let content_type = stored_content_type(&intent.mime).to_owned();
        // Create-only: a replayed URL cannot swap a finalized object.
        let put = crate::storage::PutConditions::new()
            .content_type(content_type.clone())
            .content_length(size)
            .create_only();
        let upload_url = storage
            .presigned_put_url(&key, intent.ttl, &put)
            .await
            .ok_or_else(|| {
                MediaError::Other(format!(
                    "disk `{}` doesn't support presigned PUT (use save_bytes instead)",
                    intent.disk
                ))
            })?;
        let row = self
            .insert_row(InsertRow {
                disk: intent.disk.clone(),
                storage_key: key.clone(),
                mime: intent.mime,
                size_bytes: intent.size_bytes,
                original_filename: intent.original_filename,
                status: MediaStatus::Pending,
                uploaded_by_id: intent.uploaded_by_id,
                derived_from_id: None,
                collection_id: intent.collection_id,
                metadata: Value::Object(serde_json::Map::new()),
            })
            .await?;
        let media_id = match row.id {
            Auto::Set(v) => v,
            _ => unreachable!("insert returns Set id"),
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let expires_at = DateTime::<Utc>::from_timestamp(
            i64::try_from(now + intent.ttl.as_secs()).unwrap_or(i64::MAX),
            0,
        )
        .unwrap_or_else(Utc::now);
        Ok(UploadTicket {
            media_id,
            upload_url,
            expires_at,
            disk: intent.disk,
            storage_key: key,
            content_type,
            headers: put.headers(),
        })
    }

    /// Check the storage object for `media_id` and flip the row from
    /// `Pending` to `Ready`. It must have the size the row declared and,
    /// where the backend stores one, the signed type. Otherwise the row
    /// flips to `Failed` and a mismatched object is deleted. Returns the
    /// row either way; a row that is not `Pending` is returned untouched.
    ///
    /// # Errors
    /// `Db` if the row is missing or the update fails. `Storage` for
    /// transport failures, or a backend that cannot report
    /// [`crate::storage::ObjectMeta`]. Without `s3:ListBucket`, S3 answers
    /// a HEAD on a missing key with 403, so this fails closed (router: 502).
    pub async fn finalize_upload(&self, media_id: i64) -> Result<Media, MediaError> {
        let media = self
            .get(media_id)
            .await?
            .ok_or_else(|| MediaError::Other(format!("media {media_id} not found")))?;
        // Only a Pending row is finalized: a Ready or Failed one keeps its
        // object and its status, whoever calls this.
        if media.status_enum() != Some(MediaStatus::Pending) {
            return Ok(media);
        }
        let storage = self.resolve_disk(&media.disk)?;
        // The backend's word, never the client's: the declared size and
        // type are what the PUT was signed for (#1851).
        let (new_status, mismatched) = match storage.metadata(&media.storage_key).await? {
            Some(meta) if upload_matches(&meta, &media) => (MediaStatus::Ready, false),
            Some(_) => (MediaStatus::Failed, true),
            None => (MediaStatus::Failed, false),
        };
        use crate::sql::UpdaterPool as _;
        let changed = Media::objects()
            .where_(Media::id.eq(media_id))
            .where_(Media::status.eq(MediaStatus::Pending.as_str().to_owned()))
            .update()
            .set("status", new_status.as_str())
            .execute_pool(&self.pool)
            .await
            .map_err(media_err_from_exec)?;
        if changed != 1 {
            // A concurrent finalize won; report the row as it now is.
            return self
                .get(media_id)
                .await?
                .ok_or_else(|| MediaError::Other(format!("media {media_id} not found")));
        }
        if mismatched {
            let _ = storage.delete(&media.storage_key).await;
        }
        let mut updated = media;
        updated.status = new_status.as_str().to_owned();
        Ok(updated)
    }

    // --------- read

    /// Fetch by id. Soft-deleted rows are excluded; pass through to
    /// [`Self::get_including_deleted`] when you want them.
    pub async fn get(&self, id: i64) -> Result<Option<Media>, MediaError> {
        let p = self.pool.dialect().placeholder(1);
        let sql = format!(
            "SELECT id, disk, storage_key, mime, size_bytes, original_filename, \
                    status, uploaded_at, uploaded_by_id, derived_from_id, \
                    collection_id, metadata, deleted_at \
               FROM rustango_media WHERE id = {p} AND deleted_at IS NULL"
        );
        let rows: Vec<Media> =
            crate::sql::raw_query_pool(&sql, vec![crate::core::SqlValue::I64(id)], &self.pool)
                .await
                .map_err(media_err_from_exec)?;
        Ok(rows.into_iter().next())
    }

    /// Like [`Self::get`] but returns soft-deleted rows too. Use
    /// from admin / restore flows.
    pub async fn get_including_deleted(&self, id: i64) -> Result<Option<Media>, MediaError> {
        let p = self.pool.dialect().placeholder(1);
        let sql = format!(
            "SELECT id, disk, storage_key, mime, size_bytes, original_filename, \
                    status, uploaded_at, uploaded_by_id, derived_from_id, \
                    collection_id, metadata, deleted_at \
               FROM rustango_media WHERE id = {p}"
        );
        let rows: Vec<Media> =
            crate::sql::raw_query_pool(&sql, vec![crate::core::SqlValue::I64(id)], &self.pool)
                .await
                .map_err(media_err_from_exec)?;
        Ok(rows.into_iter().next())
    }

    /// CDN-aware URL for `m`. Returns `None` when neither the disk's
    /// CDN prefix nor the backend's public URL is available.
    #[must_use]
    pub fn url(&self, m: &Media) -> Option<String> {
        self.registry.cdn_url(&m.disk, &m.storage_key)
    }

    /// CDN-aware public URL for a media **id**, minting no signature.
    ///
    /// This is the supported way to put an uploaded image on a public
    /// page. [`crate::media::router`] is the *internal* management
    /// API and refuses anonymous requests by design. A public page
    /// renders this string into its own template, from its own route.
    ///
    /// `Ok(None)` has two causes: the row is missing or soft-deleted,
    /// or the disk has no CDN prefix and no base URL. Call
    /// [`Self::get`] first if you need to tell them apart.
    ///
    /// **The address is only as public as the bucket.** This says
    /// where the object *would* be served from; it does not make it
    /// readable. On a private bucket the URL is correct and the fetch
    /// is a 403. Use [`Self::presigned_get`] there instead.
    ///
    /// No signing means no `await` in a template, which matters
    /// because Tera filters are sync.
    ///
    /// ```ignore
    /// let ctx = tera::Context::from_serialize(serde_json::json!({
    ///     "hero": manager.public_url(hero_id).await?,
    /// }))?;
    /// ```
    ///
    /// # Errors
    /// Propagates the row lookup's driver error.
    pub async fn public_url(&self, id: i64) -> Result<Option<String>, MediaError> {
        Ok(self.get(id).await?.and_then(|m| self.url(&m)))
    }

    /// Bare backend URL (no CDN). For internal admin / debug.
    #[must_use]
    pub fn origin_url(&self, m: &Media) -> Option<String> {
        self.registry.origin_url(&m.disk, &m.storage_key)
    }

    /// Time-limited download link suitable for `<a href=...>`.
    /// Returns `None` when the disk's backend can't sign.
    pub async fn presigned_get(&self, m: &Media, ttl: Duration) -> Option<String> {
        let storage = self.registry.disk(&m.disk)?;
        storage.presigned_get_url(&m.storage_key, ttl).await
    }

    /// Read the file bytes server-side.
    pub async fn load_bytes(&self, m: &Media) -> Result<Vec<u8>, MediaError> {
        let storage = self.resolve_disk(&m.disk)?;
        Ok(storage.load(&m.storage_key).await?)
    }

    // --------- delete

    /// Soft-delete: set `deleted_at`. The storage object stays. Purge
    /// it later with [`Self::purge`], or let the `purge_orphans`
    /// sweep take it.
    pub async fn delete(&self, m: &Media) -> Result<(), MediaError> {
        let id = match m.id {
            Auto::Set(v) => v,
            _ => return Err(MediaError::Other("Media has no id".into())),
        };
        use crate::sql::UpdaterPool as _;
        Media::objects()
            .where_(Media::id.eq(id))
            .update()
            .set("deleted_at", Utc::now())
            .execute_pool(&self.pool)
            .await
            .map_err(media_err_from_exec)?;
        Ok(())
    }

    /// Hard-delete: remove the storage object, the row's tag links,
    /// and the row. Usually called by the `post_delete` signal after
    /// `delete()` has soft-deleted.
    ///
    /// This is also what **revokes** access. `delete()` only
    /// soft-deletes, so a presigned URL minted before it keeps working
    /// until its TTL expires. The storage object has to go for the
    /// credential to stop resolving.
    ///
    /// # Errors
    /// `UnknownDisk` if `m.disk` is not registered, `Storage` if the
    /// object could not be removed, `Db` for either delete. **The row
    /// is left in place in every error case.**
    pub async fn purge(&self, m: &Media) -> Result<(), MediaError> {
        let id = match m.id {
            Auto::Set(v) => v,
            _ => return Err(MediaError::Other("Media has no id".into())),
        };
        // The storage delete is what revokes access, and this row is
        // the only record that the object exists: `orphans_older_than`
        // finds it by `deleted_at`, and nothing else stores the key.
        // So a failed delete must not be followed by the row delete.
        //
        // `Storage::delete` is a no-op on a missing key, so an `Err`
        // here is a real failure (credentials, network, policy), not a
        // stale row. Leaving the row soft-deleted lets the next sweep
        // retry it.
        let storage = self.resolve_disk(&m.disk)?;
        storage.delete(&m.storage_key).await?;
        // Links first. `rustango_media_tag_links.media_id` has no
        // foreign key, so deleting the media row alone would leave
        // them behind, and `popular_tags` counts links.
        // One transaction, so a failed row delete keeps its links (#1573).
        let unlink = MediaTagLink::objects()
            .where_(MediaTagLink::media_id.eq(id))
            .compile_delete()
            .map_err(media_err_from_query)?;
        let row = Media::objects()
            .where_(Media::id.eq(id))
            .compile_delete()
            .map_err(media_err_from_query)?;
        let mut tx = crate::sql::transaction_pool(&self.pool)
            .await
            .map_err(media_err_from_exec)?;
        crate::sql::delete_tx(&mut tx, &unlink)
            .await
            .map_err(media_err_from_exec)?;
        crate::sql::delete_tx(&mut tx, &row)
            .await
            .map_err(media_err_from_exec)?;
        tx.commit().await?;
        Ok(())
    }

    /// Hard-delete every soft-deleted Media row older than
    /// `older_than`, removing the storage object as we go. Returns
    /// the count of rows purged.
    ///
    /// Run from the [`crate::scheduler`] (e.g. nightly) to keep
    /// orphan storage objects from accumulating.
    ///
    /// **Under tenancy, pass a tenant-scoped pool.** `rustango_media`
    /// is per-tenant, so this sweeps only the tenant `self.pool`
    /// points at (on a registry pool in schema mode, only `public`).
    /// It also deletes storage objects, so fan it out with
    /// [`crate::tenancy::for_each_tenant`], and run
    /// [`Self::purge_orphans_dry_run`] first if you are unsure what a
    /// pool points at.
    ///
    /// # Errors
    /// The first failure, **after trying every row**, so one
    /// unreachable object does not block the rest of the sweep. Each
    /// failure is logged at `warn` with its disk and key, and the run
    /// is summarised at `error`, because the purged count does not
    /// survive the `Err`.
    pub async fn purge_orphans(&self, older_than: Duration) -> Result<u64, MediaError> {
        let rows = self.orphans_older_than(older_than).await?;
        let total = rows.len();
        let mut purged = 0u64;
        let mut first_err: Option<MediaError> = None;
        for m in rows {
            match self.purge(&m).await {
                Ok(()) => purged += 1,
                Err(e) => {
                    tracing::warn!(
                        disk = %m.disk,
                        storage_key = %m.storage_key,
                        error = %e,
                        "media purge_orphans: row left in place, will retry next sweep"
                    );
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
            }
        }
        if let Some(e) = first_err {
            let failed = total - purged as usize;
            tracing::error!(
                purged,
                failed,
                total,
                "media purge_orphans: finished with failures"
            );
            return Err(e);
        }
        Ok(purged)
    }

    /// What [`Self::purge_orphans`] *would* delete. Same query, no
    /// `purge` call, so no rows and no storage objects are removed.
    ///
    /// Run it before wiring the real sweep in a tenancy app: the rows
    /// carry their `disk` and `storage_key`, so you can see whether
    /// the pool points where you think it does.
    ///
    /// # Errors
    /// Driver error reading `rustango_media`.
    pub async fn purge_orphans_dry_run(
        &self,
        older_than: Duration,
    ) -> Result<Vec<Media>, MediaError> {
        self.orphans_older_than(older_than).await
    }

    /// The soft-deleted rows older than `older_than`. Shared by
    /// [`Self::purge_orphans`] and [`Self::purge_orphans_dry_run`] so
    /// the dry run cannot drift from the real sweep.
    async fn orphans_older_than(&self, older_than: Duration) -> Result<Vec<Media>, MediaError> {
        use crate::sql::FetcherPool as _;
        // Cutoff computed in Rust, so no per-dialect `NOW() - INTERVAL`.
        let cutoff = Utc::now()
            - chrono::Duration::from_std(older_than).unwrap_or(chrono::Duration::seconds(0));
        Media::objects()
            .where_(Media::deleted_at.is_not_null())
            .where_(Media::deleted_at.lt(cutoff))
            .fetch(&self.pool)
            .await
            .map_err(media_err_from_exec)
    }

    /// Hard-delete Media rows still `Pending`, or `Failed`, after
    /// `older_than`, with their storage objects, up to
    /// `PURGE_PENDING_BATCH` per call.
    ///
    /// Direct browser uploads leave `Pending` rows behind when the
    /// browser gives up before calling `finalize_upload`, and a PUT can
    /// still land under a `Failed` row. Run this from the
    /// [`crate::scheduler`].
    ///
    /// Returns the number of media rows deleted. A full batch means
    /// there is more work — call again, or let the next run take it.
    ///
    /// Each row is deleted on `id` and the status it was read with, so
    /// one finalized in between is kept. Its object is deleted inside
    /// that transaction: on a storage error the row stays for the next
    /// sweep. Tag links of gone rows are reclaimed at the end.
    ///
    /// # Errors
    /// The first failure, after trying every row.
    pub async fn purge_pending(&self, older_than: Duration) -> Result<u64, MediaError> {
        use crate::sql::FetcherPool as _;
        let cutoff = Utc::now()
            - chrono::Duration::from_std(older_than).unwrap_or(chrono::Duration::seconds(0));
        let unconfirmed =
            [MediaStatus::Pending, MediaStatus::Failed].map(|s| s.as_str().to_owned());
        let rows = Media::objects()
            .where_(Media::status.is_in(unconfirmed))
            .where_(Media::uploaded_at.lt(cutoff))
            .limit(PURGE_PENDING_BATCH)
            .fetch(&self.pool)
            .await
            .map_err(media_err_from_exec)?;
        let mut purged = 0u64;
        let mut first_err: Option<MediaError> = None;
        for m in rows {
            match self.purge_unconfirmed(&m).await {
                Ok(true) => purged += 1,
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(
                        disk = %m.disk,
                        storage_key = %m.storage_key,
                        error = %e,
                        "media purge_pending: row left in place, will retry next sweep"
                    );
                    first_err.get_or_insert(e);
                }
            }
        }
        // Reclaim tag links whose media row is gone.
        // `rustango_media_tag_links.media_id` has no foreign key, so
        // nothing else reclaims them. Keying on "the row does not
        // exist", rather than on the ids just deleted, makes this
        // race-free: a link whose media row is present is never
        // touched, and one whose row is absent is garbage.
        let alive = Media::objects()
            .where_(Media::id.eq_expr(crate::core::subquery::outer_ref("media_id")))
            .compile()
            .map_err(media_err_from_query)?;
        let orphans = MediaTagLink::objects()
            .where_not_exists(alive)
            .compile_delete()
            .map_err(media_err_from_query)?;
        crate::sql::delete_pool(&self.pool, &orphans)
            .await
            .map_err(media_err_from_exec)?;
        first_err.map_or(Ok(purged), Err)
    }

    /// Delete one row read as `m.status`, and its object.
    /// `Ok(false)` when the row changed status or went away meanwhile.
    async fn purge_unconfirmed(&self, m: &Media) -> Result<bool, MediaError> {
        let Auto::Set(id) = m.id else {
            return Err(MediaError::Other("Media has no id".into()));
        };
        let storage = self.resolve_disk(&m.disk)?;
        let row = Media::objects()
            .where_(Media::id.eq(id))
            .where_(Media::status.eq(m.status.clone()))
            .compile_delete()
            .map_err(media_err_from_query)?;
        let mut tx = crate::sql::transaction_pool(&self.pool)
            .await
            .map_err(media_err_from_exec)?;
        let deleted = crate::sql::delete_tx(&mut tx, &row)
            .await
            .map_err(media_err_from_exec)?;
        if deleted != 1 {
            tx.rollback().await?;
            return Ok(false);
        }
        // A missing key is a no-op, so an error here is real.
        storage.delete(&m.storage_key).await?;
        tx.commit().await?;
        Ok(true)
    }

    // =================================================================
    // Collections (folders)
    // =================================================================

    /// Create a new collection. `slug` must be unique among live
    /// collections; a deleted one's slug is reused. `parent` may be
    /// `None` (root) or another collection's id (sub-folder).
    ///
    /// # Errors
    /// `Db` for unique-constraint violations on `slug` or any other
    /// underlying sqlx error. `Other("parent collection N is missing or
    /// deleted")` when `parent` is.
    pub async fn create_collection(
        &self,
        name: impl Into<String>,
        slug: impl Into<String>,
        parent: Option<i64>,
        description: impl Into<String>,
    ) -> Result<MediaCollection, MediaError> {
        let slug = slug.into();
        let mut row = MediaCollection {
            id: Auto::Unset,
            name: name.into(),
            slug,
            parent_id: parent,
            description: description.into(),
            created_at: Auto::Unset,
            deleted_at: None,
        };
        // IMMEDIATE on SQLite: this tx reads before it writes (#2180).
        let mut tx = crate::sql::write_transaction_pool(&self.pool)
            .await
            .map_err(media_err_from_exec)?;
        // Locked until commit, so a concurrent `delete_collection` either
        // sees this child or has already deleted the parent (#1573).
        if let Some(p) = parent {
            use crate::sql::FetcherTx as _;
            let live = MediaCollection::objects()
                .where_(MediaCollection::id.eq(p))
                .where_(MediaCollection::deleted_at.is_null())
                .select_for_update()
                .fetch_tx(&mut tx)
                .await
                .map_err(media_err_from_exec)?;
            if live.is_empty() {
                tx.rollback().await?;
                // Not "not found": the router would answer 404 for a bad body.
                return Err(MediaError::Other(format!(
                    "parent collection {p} is missing or deleted"
                )));
            }
        }
        // A tombstone's `unique` slug would block this one forever (#1677).
        // Dropped in this tx, by id: a DELETE by a slug that matches nothing
        // gap-locks MySQL's slug index, and two creates deadlock (#2182).
        let tombstones = {
            use crate::sql::FetcherTx as _;
            MediaCollection::objects()
                .where_(MediaCollection::slug.eq(row.slug.clone()))
                .where_(MediaCollection::deleted_at.is_not_null())
                .fetch_tx(&mut tx)
                .await
                .map_err(media_err_from_exec)?
        };
        for t in tombstones {
            let Auto::Set(id) = t.id else { continue };
            let tombstone = MediaCollection::objects()
                .where_(MediaCollection::id.eq(id))
                .where_(MediaCollection::deleted_at.is_not_null())
                .compile_delete()
                .map_err(|e| media_err_from_exec(e.into()))?;
            crate::sql::delete_tx(&mut tx, &tombstone)
                .await
                .map_err(media_err_from_exec)?;
        }
        row.insert_tx(&mut tx).await.map_err(media_err_from_exec)?;
        let Auto::Set(id) = row.id else {
            return Err(MediaError::Other("collection INSERT returned no id".into()));
        };
        // Read back for the database-stamped `created_at`, inside the tx:
        // after commit a concurrent subtree delete may already have taken it.
        let created = {
            use crate::sql::FetcherTx as _;
            MediaCollection::objects()
                .where_(MediaCollection::id.eq(id))
                .fetch_tx(&mut tx)
                .await
                .map_err(media_err_from_exec)?
        };
        tx.commit().await?;
        created
            .into_iter()
            .next()
            .ok_or_else(|| MediaError::Other(format!("collection {id} vanished after INSERT")))
    }

    /// Look up by id (excludes soft-deleted).
    pub async fn get_collection(&self, id: i64) -> Result<Option<MediaCollection>, MediaError> {
        let p = self.pool.dialect().placeholder(1);
        let sql = format!(
            "SELECT id, name, slug, parent_id, description, created_at, deleted_at \
               FROM rustango_media_collections WHERE id = {p} AND deleted_at IS NULL"
        );
        let rows: Vec<MediaCollection> =
            crate::sql::raw_query_pool(&sql, vec![crate::core::SqlValue::I64(id)], &self.pool)
                .await
                .map_err(media_err_from_exec)?;
        Ok(rows.into_iter().next())
    }

    /// Look up by slug (excludes soft-deleted).
    pub async fn get_collection_by_slug(
        &self,
        slug: &str,
    ) -> Result<Option<MediaCollection>, MediaError> {
        let p = self.pool.dialect().placeholder(1);
        let sql = format!(
            "SELECT id, name, slug, parent_id, description, created_at, deleted_at \
               FROM rustango_media_collections WHERE slug = {p} AND deleted_at IS NULL"
        );
        let rows: Vec<MediaCollection> = crate::sql::raw_query_pool(
            &sql,
            vec![crate::core::SqlValue::String(slug.to_owned())],
            &self.pool,
        )
        .await
        .map_err(media_err_from_exec)?;
        Ok(rows.into_iter().next())
    }

    /// List every non-deleted collection, roots first, then by
    /// `(parent_id, name)` so siblings group together — handy for tree-renderers.
    ///
    /// Unbounded: one row per collection.
    #[deprecated(note = "use list_collections_paged")]
    pub async fn list_collections(&self) -> Result<Vec<MediaCollection>, MediaError> {
        use crate::sql::FetcherPool as _;
        live_collections()
            .fetch(&self.pool)
            .await
            .map_err(media_err_from_exec)
    }

    /// One page of [`Self::list_collections`]; `limit` is clamped to
    /// `1..=MAX_LIST_LIMIT`.
    pub async fn list_collections_paged(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<MediaCollection>, MediaError> {
        use crate::sql::FetcherPool as _;
        live_collections()
            .limit(limit.clamp(1, MAX_LIST_LIMIT))
            .offset(offset.max(0))
            .fetch(&self.pool)
            .await
            .map_err(media_err_from_exec)
    }

    /// Build the slug-joined path for a collection: `"products/2026/launch"`.
    /// Walks up the parent chain. Cycles raise `Other`.
    pub async fn collection_path(&self, id: i64) -> Result<String, MediaError> {
        let mut parts = Vec::new();
        let mut cur = Some(id);
        let mut depth = 0;
        while let Some(cid) = cur {
            depth += 1;
            if depth > 64 {
                return Err(MediaError::Other(
                    "collection_path: cycle / too-deep parent chain".into(),
                ));
            }
            let c = self
                .get_collection(cid)
                .await?
                .ok_or_else(|| MediaError::Other(format!("collection {cid} not found")))?;
            cur = c.parent_id;
            parts.push(c.slug);
        }
        parts.reverse();
        Ok(parts.join("/"))
    }

    /// Soft-delete a collection **and its descendants**. Media inside
    /// them is not deleted, only orphaned (`collection_id` set to
    /// NULL), so the rows stay queryable and the storage objects
    /// survive.
    ///
    /// The subtree goes too. Leaving it would leave children pointing
    /// at a parent that no longer resolves: still listed by
    /// [`Self::list_collections`], and [`Self::collection_path`] on
    /// any of them a permanent error.
    pub async fn delete_collection(&self, id: i64) -> Result<(), MediaError> {
        // Both statements in one transaction. Run separately, a
        // failure of the soft-delete leaves the orphaning committed:
        // the collection still live, every media row under it
        // orphaned, and the old `collection_id` recorded nowhere.
        // Both are database-only, so a transaction closes it.
        // IMMEDIATE on SQLite: the walk reads before the writes (#2180).
        let mut tx = crate::sql::write_transaction_pool(&self.pool)
            .await
            .map_err(media_err_from_exec)?;
        // One bind spare for `deleted_at`; a wide subtree is many `IN` lists.
        let chunk = self.pool.dialect().max_bind_params() - 1;
        // The **whole subtree**, walked inside the transaction so a
        // child created mid-delete is not left under a dead parent.
        let ids = lock_subtree_tx(&mut tx, id, chunk).await?;

        let now = Utc::now();
        for part in ids.chunks(chunk) {
            // Media survives, as documented — it is orphaned, not deleted.
            let orphan = Media::objects()
                .where_(Media::collection_id.is_in(part.iter().copied()))
                .update()
                .set("collection_id", crate::core::SqlValue::Null)
                .compile()
                .map_err(media_err_from_query)?;
            let soft_delete = MediaCollection::objects()
                .where_(MediaCollection::id.is_in(part.iter().copied()))
                .update()
                .set("deleted_at", now)
                .compile()
                .map_err(media_err_from_query)?;
            crate::sql::update_tx(&mut tx, &orphan)
                .await
                .map_err(media_err_from_exec)?;
            crate::sql::update_tx(&mut tx, &soft_delete)
                .await
                .map_err(media_err_from_exec)?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Move a [`Media`] into a collection (or `None` to set "loose").
    pub async fn move_to_collection(
        &self,
        media_id: i64,
        collection_id: Option<i64>,
    ) -> Result<(), MediaError> {
        let d = self.pool.dialect();
        let sql = format!(
            "UPDATE rustango_media SET collection_id = {p1} WHERE id = {p2}",
            p1 = d.placeholder(1),
            p2 = d.placeholder(2),
        );
        let target = collection_id
            .map(crate::core::SqlValue::I64)
            .unwrap_or(crate::core::SqlValue::Null);
        crate::sql::raw_execute_pool(
            &self.pool,
            &sql,
            vec![target, crate::core::SqlValue::I64(media_id)],
        )
        .await
        .map_err(media_err_from_exec)?;
        Ok(())
    }

    /// List media in `collection_id`. When `recursive`, descends into
    /// every nested collection.
    ///
    /// **Returns at most [`DEFAULT_LIST_CAP`] rows** (100). Use
    /// [`Self::list_in_collection_paged`] to choose the page.
    pub async fn list_in_collection(
        &self,
        collection_id: i64,
        recursive: bool,
    ) -> Result<Vec<Media>, MediaError> {
        self.list_in_collection_paged(collection_id, recursive, DEFAULT_LIST_CAP, 0)
            .await
    }

    /// [`Self::list_in_collection`] with an explicit page.
    ///
    /// `limit` is clamped to `1..=MAX_LIST_LIMIT`, matching
    /// [`Self::list_with_tag`].
    ///
    /// # Errors
    /// `Other` (HTTP 400) for an `offset` above 10 000 when the subtree
    /// has more collections than the backend's bind limit.
    pub async fn list_in_collection_paged(
        &self,
        collection_id: i64,
        recursive: bool,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Media>, MediaError> {
        use crate::sql::FetcherPool as _;
        let ids: Vec<i64> = if recursive {
            self.collect_descendant_ids(collection_id).await?
        } else {
            vec![collection_id]
        };
        // Clamped, not trusted. A negative `LIMIT` means "no limit"
        // on SQLite, and PostgreSQL rejects `LIMIT -1` outright — so
        // an unclamped caller value is an unbounded scan on one
        // backend and a 500 on the others.
        let lim = limit.clamp(1, MAX_LIST_LIMIT);
        let off = offset.max(0);
        // A subtree wider than the bind cap is several `IN` lists. Each
        // returns its own first `off + lim` rows; the page is cut after
        // the merge, so it matches what one list would have returned.
        let parts: Vec<&[i64]> = ids.chunks(self.pool.dialect().max_bind_params()).collect();
        let (lim_each, off_each) = match parts.len() {
            1 => (lim, off),
            // Each chunk loads `off + lim` rows, so the offset is capped.
            _ if off > MAX_WIDE_OFFSET => {
                return Err(MediaError::Other(format!(
                    "offset {off} is above {MAX_WIDE_OFFSET} on a subtree this wide"
                )));
            }
            _ => (off + lim, 0),
        };
        let mut rows: Vec<Media> = Vec::new();
        for part in &parts {
            // `, id DESC` is the tiebreaker, and it is not cosmetic.
            // `uploaded_at DESC` alone is not a total order, and ties are
            // normal: on PostgreSQL `now()` is the transaction timestamp,
            // so a bulk import gives every row the same value, and SQLite
            // stores one-second resolution. Under a small `LIMIT` the
            // planner's top-N sort orders tied keys differently per
            // (limit, offset), so paging the same data twice can return a
            // row twice and skip another entirely.
            let page = Media::objects()
                .where_(Media::collection_id.is_in(part.iter().copied()))
                .where_(Media::deleted_at.is_null())
                .order_by(&[("uploaded_at", true), ("id", true)])
                .limit(lim_each)
                .offset(off_each)
                .fetch(&self.pool)
                .await
                .map_err(media_err_from_exec)?;
            rows.extend(page);
        }
        if parts.len() > 1 {
            rows.sort_by_key(|m| std::cmp::Reverse(media_order_key(m)));
            rows = rows
                .into_iter()
                .skip(usize::try_from(off).unwrap_or(usize::MAX))
                .take(usize::try_from(lim).unwrap_or(0))
                .collect();
        }
        Ok(rows)
    }

    async fn collect_descendant_ids(&self, root: i64) -> Result<Vec<i64>, MediaError> {
        // Recursive CTE over the parent_id chain. The same syntax
        // works on PG, MySQL 8+ and SQLite 3.8+.
        let p = self.pool.dialect().placeholder(1);
        let sql = format!(
            "WITH RECURSIVE sub AS ( \
                 SELECT id FROM rustango_media_collections \
                  WHERE id = {p} AND deleted_at IS NULL \
                 UNION \
                 SELECT c.id FROM rustango_media_collections c \
                   JOIN sub ON c.parent_id = sub.id \
                  WHERE c.deleted_at IS NULL \
             ) SELECT id FROM sub"
        );
        let rows: Vec<(i64,)> =
            crate::sql::raw_query_pool(&sql, vec![crate::core::SqlValue::I64(root)], &self.pool)
                .await
                .map_err(media_err_from_exec)?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    // =================================================================
    // Tags
    // =================================================================

    /// Find or create a tag by `slug` (auto-derives `name` from
    /// `slug` if creating).
    pub async fn ensure_tag(&self, slug: &str) -> Result<MediaTag, MediaError> {
        if let Some(t) = self.get_tag_by_slug(slug).await? {
            return Ok(t);
        }
        // Per-dialect upsert:
        //   PG / SQLite : ON CONFLICT (slug) DO UPDATE … RETURNING
        //   MySQL       : ON DUPLICATE KEY UPDATE, then SELECT
        let d = self.pool.dialect();
        let (p1, p2) = (d.placeholder(1), d.placeholder(2));
        if d.name() == "mysql" {
            #[cfg(feature = "mysql")]
            {
                // Irrefutable in a mysql-only build — see the note above.
                #[allow(irrefutable_let_patterns)]
                let crate::sql::Pool::Mysql(my) = &self.pool
                else {
                    unreachable!()
                };
                let mut tx = my.begin().await?;
                sqlx::query(
                    "INSERT INTO `rustango_media_tags` (`name`, `slug`) \
                     VALUES (?, ?) \
                     ON DUPLICATE KEY UPDATE `name` = VALUES(`name`)",
                )
                .bind(slug)
                .bind(slug)
                .execute(&mut *tx)
                .await?;
                let out: MediaTag = sqlx::query_as(
                    "SELECT id, name, slug, created_at FROM `rustango_media_tags` WHERE slug = ?",
                )
                .bind(slug)
                .fetch_one(&mut *tx)
                .await?;
                tx.commit().await?;
                return Ok(out);
            }
            #[cfg(not(feature = "mysql"))]
            unreachable!();
        }
        let sql = format!(
            "INSERT INTO rustango_media_tags (name, slug) VALUES ({p1}, {p2}) \
             ON CONFLICT (slug) DO UPDATE SET name = EXCLUDED.name \
             RETURNING id, name, slug, created_at"
        );
        let rows: Vec<MediaTag> = crate::sql::raw_query_pool(
            &sql,
            vec![
                crate::core::SqlValue::String(slug.to_owned()),
                crate::core::SqlValue::String(slug.to_owned()),
            ],
            &self.pool,
        )
        .await
        .map_err(media_err_from_exec)?;
        rows.into_iter()
            .next()
            .ok_or_else(|| MediaError::Other("ensure_tag INSERT returned no rows".into()))
    }

    /// Look up a tag by slug.
    pub async fn get_tag_by_slug(&self, slug: &str) -> Result<Option<MediaTag>, MediaError> {
        let p = self.pool.dialect().placeholder(1);
        let sql =
            format!("SELECT id, name, slug, created_at FROM rustango_media_tags WHERE slug = {p}");
        let rows: Vec<MediaTag> = crate::sql::raw_query_pool(
            &sql,
            vec![crate::core::SqlValue::String(slug.to_owned())],
            &self.pool,
        )
        .await
        .map_err(media_err_from_exec)?;
        Ok(rows.into_iter().next())
    }

    /// Apply tags to a media row. Auto-creates missing tags.
    /// Idempotent — duplicates ignored.
    ///
    /// # Errors
    /// `Other` for more than 1000 distinct slugs; `Db` otherwise.
    pub async fn tag(&self, media_id: i64, slugs: &[&str]) -> Result<(), MediaError> {
        let tag_ids = self.ensure_tag_ids(slugs).await?;
        crate::sql::bulk_insert_pool(&self.pool, &tag_links(media_id, &tag_ids))
            .await
            .map_err(media_err_from_exec)
    }

    /// Ids of the tags named by `slugs`, creating missing ones, in
    /// three statements however many slugs there are.
    ///
    /// On MySQL a slug that differs from a stored one only in case
    /// (`Mixed` vs `mixed`) costs 2 more: an insert that skips, and a re-read.
    async fn ensure_tag_ids(&self, slugs: &[&str]) -> Result<Vec<i64>, MediaError> {
        let mut slugs = slugs.to_vec();
        slugs.sort_unstable();
        slugs.dedup();
        if slugs.len() > MAX_TAGS_PER_CALL {
            return Err(MediaError::Other(format!(
                "at most {MAX_TAGS_PER_CALL} tags per call, got {}",
                slugs.len()
            )));
        }
        if slugs.is_empty() {
            return Ok(Vec::new());
        }
        let found = self.tags_by_slug(&slugs).await?;
        let have: std::collections::HashSet<&str> = found.iter().map(|t| t.slug.as_str()).collect();
        let missing: Vec<&str> = slugs
            .iter()
            .copied()
            .filter(|s| !have.contains(s))
            .collect();
        if missing.is_empty() {
            return Ok(tag_ids_of(&found));
        }
        let rows = missing
            .iter()
            .map(|s| {
                vec![
                    crate::core::SqlValue::String((*s).to_owned()),
                    crate::core::SqlValue::String((*s).to_owned()),
                ]
            })
            .collect();
        // Skips a tag a concurrent call created meanwhile.
        let insert = crate::core::BulkInsertQuery::new(
            <MediaTag as crate::core::Model>::SCHEMA,
            vec!["name", "slug"],
            rows,
        )
        .on_conflict_do_nothing();
        if let Err(e) = crate::sql::bulk_insert_pool(&self.pool, &insert).await {
            if !is_mysql_batch_retryable(&e) {
                return Err(media_err_from_exec(e));
            }
            // Two new slugs equal under MySQL's collation, or a deadlock: one at a time.
            for s in &missing {
                self.ensure_tag(s).await?;
            }
        }
        Ok(tag_ids_of(&self.tags_by_slug(&slugs).await?))
    }

    /// Tags whose slug is in `slugs` (at most [`MAX_TAGS_PER_CALL`]).
    async fn tags_by_slug(&self, slugs: &[&str]) -> Result<Vec<MediaTag>, MediaError> {
        use crate::sql::FetcherPool as _;
        MediaTag::objects()
            .where_(MediaTag::slug.is_in(slugs.iter().map(|s| (*s).to_owned())))
            .fetch(&self.pool)
            .await
            .map_err(media_err_from_exec)
    }

    /// Remove a single tag from a media row.
    pub async fn untag(&self, media_id: i64, slug: &str) -> Result<(), MediaError> {
        // `DELETE … USING` is PG-only. A subquery works everywhere.
        let d = self.pool.dialect();
        let (p1, p2) = (d.placeholder(1), d.placeholder(2));
        let sql = format!(
            "DELETE FROM rustango_media_tag_links \
              WHERE tag_id IN (SELECT id FROM rustango_media_tags WHERE slug = {p1}) \
                AND media_id = {p2}"
        );
        crate::sql::raw_execute_pool(
            &self.pool,
            &sql,
            vec![
                crate::core::SqlValue::String(slug.to_owned()),
                crate::core::SqlValue::I64(media_id),
            ],
        )
        .await
        .map_err(media_err_from_exec)?;
        Ok(())
    }

    /// Replace the entire tag set for a media row. Tags not in
    /// `slugs` are removed; tags in `slugs` are added (auto-created
    /// if needed).
    ///
    /// **Atomic.** The delete and the inserts are one transaction, so
    /// a failure part-way leaves the row's tags as they were.
    ///
    /// # Errors
    /// `Db` for the delete, either insert, or the commit.
    pub async fn set_tags(&self, media_id: i64, slugs: &[&str]) -> Result<(), MediaError> {
        // Resolve every tag id **before** opening the transaction.
        // Get-or-create writes and is the most likely step to fail, so
        // a failure leaves the row's old tags intact. A tag row left
        // over from a failed set is harmless: `popular_tags` counts
        // links, so it reports zero uses.
        let tag_ids = self.ensure_tag_ids(slugs).await?;
        // MySQL's empty-range DELETE takes a gap lock that a concurrent
        // set on the next media row deadlocks with; the loser is retried.
        let mut attempt = 0;
        loop {
            match self.replace_links(media_id, &tag_ids).await {
                Err(MediaError::Db(e)) if is_deadlock(&e) && attempt < 2 => attempt += 1,
                r => return r,
            }
        }
    }

    /// The delete + insert half of [`Self::set_tags`], in one transaction.
    async fn replace_links(&self, media_id: i64, tag_ids: &[i64]) -> Result<(), MediaError> {
        let p1 = self.pool.dialect().placeholder(1);
        let delete_sql = format!("DELETE FROM rustango_media_tag_links WHERE media_id = {p1}");

        let mut tx = crate::sql::transaction_pool(&self.pool)
            .await
            .map_err(media_err_from_exec)?;
        crate::sql::raw_execute_tx(
            &mut tx,
            &delete_sql,
            vec![crate::core::SqlValue::I64(media_id)],
        )
        .await
        .map_err(media_err_from_exec)?;
        crate::sql::bulk_insert_tx(&mut tx, &tag_links(media_id, tag_ids))
            .await
            .map_err(media_err_from_exec)?;
        tx.commit().await?;
        Ok(())
    }

    /// List tags applied to a media row, alphabetically by slug.
    pub async fn tags_for(&self, media_id: i64) -> Result<Vec<MediaTag>, MediaError> {
        let p = self.pool.dialect().placeholder(1);
        let sql = format!(
            "SELECT t.id, t.name, t.slug, t.created_at \
               FROM rustango_media_tags t \
               JOIN rustango_media_tag_links l ON l.tag_id = t.id \
              WHERE l.media_id = {p} \
              ORDER BY t.slug"
        );
        let rows: Vec<MediaTag> = crate::sql::raw_query_pool(
            &sql,
            vec![crate::core::SqlValue::I64(media_id)],
            &self.pool,
        )
        .await
        .map_err(media_err_from_exec)?;
        Ok(rows)
    }

    /// Tag slugs for many media rows, in **one** query.
    ///
    /// Use this instead of calling [`Self::tags_for`] in a loop, which
    /// costs a round trip per row. Returns a map so a caller can drain
    /// it row by row; a media id with no tags is absent.
    pub async fn tags_for_many(
        &self,
        media_ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, Vec<String>>, MediaError> {
        let mut out: std::collections::HashMap<i64, Vec<String>> = std::collections::HashMap::new();
        if media_ids.is_empty() {
            return Ok(out);
        }
        let d = self.pool.dialect();
        let placeholders: Vec<String> = (1..=media_ids.len()).map(|i| d.placeholder(i)).collect();
        let in_list = placeholders.join(", ");
        let sql = format!(
            "SELECT l.media_id, t.slug \
               FROM rustango_media_tags t \
               JOIN rustango_media_tag_links l ON l.tag_id = t.id \
              WHERE l.media_id IN ({in_list}) \
              ORDER BY l.media_id, t.slug"
        );
        let binds: Vec<crate::core::SqlValue> = media_ids
            .iter()
            .copied()
            .map(crate::core::SqlValue::I64)
            .collect();
        let rows: Vec<(i64, String)> = crate::sql::raw_query_pool(&sql, binds, &self.pool)
            .await
            .map_err(media_err_from_exec)?;
        for (media_id, slug) in rows {
            out.entry(media_id).or_default().push(slug);
        }
        Ok(out)
    }

    /// List media that carry `slug`. Soft-deleted media excluded.
    pub async fn list_with_tag(
        &self,
        slug: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Media>, MediaError> {
        let d = self.pool.dialect();
        let (p1, p2, p3) = (d.placeholder(1), d.placeholder(2), d.placeholder(3));
        let sql = format!(
            "SELECT m.id, m.disk, m.storage_key, m.mime, m.size_bytes, m.original_filename, \
                    m.status, m.uploaded_at, m.uploaded_by_id, m.derived_from_id, \
                    m.collection_id, m.metadata, m.deleted_at \
               FROM rustango_media m \
               JOIN rustango_media_tag_links l ON l.media_id = m.id \
               JOIN rustango_media_tags t ON t.id = l.tag_id \
              WHERE t.slug = {p1} AND m.deleted_at IS NULL \
              ORDER BY m.uploaded_at DESC, m.id DESC \
              LIMIT {p2} OFFSET {p3}"
        );
        let rows: Vec<Media> = crate::sql::raw_query_pool(
            &sql,
            vec![
                crate::core::SqlValue::String(slug.to_owned()),
                crate::core::SqlValue::I64(limit.clamp(1, MAX_LIST_LIMIT)),
                crate::core::SqlValue::I64(offset.max(0)),
            ],
            &self.pool,
        )
        .await
        .map_err(media_err_from_exec)?;
        Ok(rows)
    }

    /// Tags by slug, one page. `limit` is clamped to `1..=MAX_LIST_LIMIT`.
    ///
    /// Reads only the tag table, so a page costs the same however many
    /// links exist — unlike [`Self::popular_tags`].
    pub async fn list_tags(&self, limit: i64, offset: i64) -> Result<Vec<MediaTag>, MediaError> {
        use crate::sql::FetcherPool as _;
        MediaTag::objects()
            .order_by(&[("slug", false)])
            .limit(limit.clamp(1, MAX_LIST_LIMIT))
            .offset(offset.max(0))
            .fetch(&self.pool)
            .await
            .map_err(media_err_from_exec)
    }

    /// Top tags by usage count, descending. Limit clamped to
    /// `1..=`[`MAX_LIST_LIMIT`].
    ///
    /// The count aggregates every link before the limit applies, so the
    /// cost grows with the link table, not the page.
    pub async fn popular_tags(&self, limit: i64) -> Result<Vec<(MediaTag, i64)>, MediaError> {
        let p = self.pool.dialect().placeholder(1);
        let sql = format!(
            // The join to `rustango_media` keeps this in step with
            // `list_with_tag`. Without it the count includes links to
            // soft-deleted rows, which leaks how many were deleted.
            // Still a LEFT JOIN chain, so a tag with no live media
            // stays listed with a count of zero.
            "SELECT t.id, t.name, t.slug, t.created_at, COUNT(m.id) AS use_count \
               FROM rustango_media_tags t \
               LEFT JOIN rustango_media_tag_links l ON l.tag_id = t.id \
               LEFT JOIN rustango_media m \
                      ON m.id = l.media_id AND m.deleted_at IS NULL \
              GROUP BY t.id, t.name, t.slug, t.created_at \
              ORDER BY use_count DESC, t.slug \
              LIMIT {p}"
        );
        // `use_count` is an aggregate, not part of MediaTag's schema,
        // so the per-backend decoders below pull it themselves and
        // leave the tag columns to the derived `sqlx::FromRow`.
        let lim = limit.clamp(1, MAX_LIST_LIMIT);
        match &self.pool {
            #[cfg(feature = "postgres")]
            crate::sql::Pool::Postgres(pg) => {
                let rows = sqlx::query(&sql).bind(lim).fetch_all(pg).await?;
                rows.iter().map(decode_tag_with_count_pg).collect()
            }
            #[cfg(feature = "mysql")]
            crate::sql::Pool::Mysql(my) => {
                let rows = sqlx::query(&sql).bind(lim).fetch_all(my).await?;
                rows.iter().map(decode_tag_with_count_my).collect()
            }
            #[cfg(feature = "sqlite")]
            crate::sql::Pool::Sqlite(sq) => {
                let rows = sqlx::query(&sql).bind(lim).fetch_all(sq).await?;
                rows.iter().map(decode_tag_with_count_sq).collect()
            }
        }
    }

    // --------- internal: row insert

    async fn insert_row(&self, r: InsertRow) -> Result<Media, MediaError> {
        let d = self.pool.dialect();
        let ph: Vec<String> = (1..=10).map(|i| d.placeholder(i)).collect();
        let placeholders = ph.join(", ");
        let cols = "(disk, storage_key, mime, size_bytes, original_filename, \
                     status, uploaded_by_id, derived_from_id, collection_id, metadata)";
        let select_cols = "id, disk, storage_key, mime, size_bytes, original_filename, \
                           status, uploaded_at, uploaded_by_id, derived_from_id, \
                           collection_id, metadata, deleted_at";
        let binds = vec![
            crate::core::SqlValue::String(r.disk.clone()),
            crate::core::SqlValue::String(r.storage_key.clone()),
            crate::core::SqlValue::String(r.mime.clone()),
            crate::core::SqlValue::I64(r.size_bytes),
            crate::core::SqlValue::String(r.original_filename.clone()),
            crate::core::SqlValue::String(r.status.as_str().to_owned()),
            r.uploaded_by_id
                .map(crate::core::SqlValue::I64)
                .unwrap_or(crate::core::SqlValue::Null),
            r.derived_from_id
                .map(crate::core::SqlValue::I64)
                .unwrap_or(crate::core::SqlValue::Null),
            r.collection_id
                .map(crate::core::SqlValue::I64)
                .unwrap_or(crate::core::SqlValue::Null),
            crate::core::SqlValue::Json(r.metadata.clone()),
        ];
        // MySQL has no RETURNING; do INSERT then SELECT LAST_INSERT_ID().
        if d.name() == "mysql" {
            #[cfg(feature = "mysql")]
            {
                // Irrefutable in a mysql-only build — see the note above.
                #[allow(irrefutable_let_patterns)]
                let crate::sql::Pool::Mysql(my) = &self.pool
                else {
                    unreachable!()
                };
                let insert_sql =
                    format!("INSERT INTO `rustango_media` {cols} VALUES ({placeholders})");
                let mut tx = my.begin().await?;
                sqlx::query(&insert_sql)
                    .bind(&r.disk)
                    .bind(&r.storage_key)
                    .bind(&r.mime)
                    .bind(r.size_bytes)
                    .bind(&r.original_filename)
                    .bind(r.status.as_str())
                    .bind(r.uploaded_by_id)
                    .bind(r.derived_from_id)
                    .bind(r.collection_id)
                    .bind(sqlx::types::Json(&r.metadata))
                    .execute(&mut *tx)
                    .await?;
                let out: Media = sqlx::query_as(&format!(
                    "SELECT {select_cols} FROM `rustango_media` WHERE id = LAST_INSERT_ID()"
                ))
                .fetch_one(&mut *tx)
                .await?;
                tx.commit().await?;
                return Ok(out);
            }
            #[cfg(not(feature = "mysql"))]
            unreachable!();
        }
        let sql = format!(
            "INSERT INTO rustango_media {cols} VALUES ({placeholders}) RETURNING {select_cols}"
        );
        let rows: Vec<Media> = crate::sql::raw_query_pool(&sql, binds, &self.pool)
            .await
            .map_err(media_err_from_exec)?;
        rows.into_iter()
            .next()
            .ok_or_else(|| MediaError::Other("media INSERT returned no rows".into()))
    }
}

struct InsertRow {
    disk: String,
    storage_key: String,
    mime: String,
    size_bytes: i64,
    original_filename: String,
    status: MediaStatus,
    uploaded_by_id: Option<i64>,
    derived_from_id: Option<i64>,
    collection_id: Option<i64>,
    metadata: Value,
}

// =====================================================================
// Helpers
// =====================================================================

/// Convert an `ExecError` to a `MediaError`. Pulls the sqlx error out
/// of `Driver` so callers matching on `MediaError::Db` still work;
/// everything else becomes `Other`.
fn media_err_from_exec(e: crate::sql::ExecError) -> MediaError {
    match e {
        crate::sql::ExecError::Driver(e) => MediaError::Db(e),
        other => MediaError::Other(other.to_string()),
    }
}

fn media_err_from_query(e: crate::core::QueryError) -> MediaError {
    media_err_from_exec(e.into())
}

/// Live collections, roots first; `id` makes the order total for paging.
fn live_collections() -> crate::query::QuerySet<MediaCollection> {
    use crate::core::NullsOrder;
    MediaCollection::objects()
        .where_(MediaCollection::deleted_at.is_null())
        .order_by_with_nulls(&[("parent_id", false, NullsOrder::First)])
        .order_by(&[("name", false), ("id", false)])
}

/// The `(uploaded_at, id)` the listings sort on, descending.
fn media_order_key(m: &Media) -> (Option<DateTime<Utc>>, i64) {
    let at = match m.uploaded_at {
        Auto::Set(v) => Some(v),
        _ => None,
    };
    let id = match m.id {
        Auto::Set(v) => v,
        _ => 0,
    };
    (at, id)
}

/// One `(media_id, tag_id)` link row per tag, skipping ones already there.
fn tag_links(media_id: i64, tag_ids: &[i64]) -> crate::core::BulkInsertQuery {
    let rows = tag_ids
        .iter()
        .map(|t| {
            vec![
                crate::core::SqlValue::I64(media_id),
                crate::core::SqlValue::I64(*t),
            ]
        })
        .collect();
    crate::core::BulkInsertQuery::new(
        <MediaTagLink as crate::core::Model>::SCHEMA,
        vec!["media_id", "tag_id"],
        rows,
    )
    .on_conflict_do_nothing()
}

/// Ids of `tags`, deduplicated: MySQL's collation can map two slugs to one tag.
fn tag_ids_of(tags: &[MediaTag]) -> Vec<i64> {
    let mut ids: Vec<i64> = tags
        .iter()
        .filter_map(|t| match t.id {
            Auto::Set(v) => Some(v),
            _ => None,
        })
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// A deadlock the server rolled back: MySQL 1213 (SQLSTATE 40001), PG 40P01.
fn is_deadlock(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(d)
        if matches!(d.code().as_deref(), Some("40001" | "40P01")))
}

/// MySQL 1869 (a skip-on-conflict insert met a row it inserted itself)
/// or 1213 (deadlock with a concurrent batch on the same slugs).
fn is_mysql_batch_retryable(e: &crate::sql::ExecError) -> bool {
    #[cfg(feature = "mysql")]
    if let crate::sql::ExecError::Driver(sqlx::Error::Database(d)) = e {
        return d
            .try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
            .is_some_and(|m| matches!(m.number(), 1869 | 1213));
    }
    let _ = e;
    false
}

/// `root` and its live descendants, each level locked `FOR UPDATE`, so a
/// `create_collection` under any of them waits for `tx` (#1573).
/// `chunk` caps the ids bound per `IN` list.
async fn lock_subtree_tx(
    tx: &mut crate::sql::PoolTx<'_>,
    root: i64,
    chunk: usize,
) -> Result<Vec<i64>, MediaError> {
    use crate::sql::FetcherTx as _;
    // Even a deleted root: a re-delete still orphans its media.
    MediaCollection::objects()
        .where_(MediaCollection::id.eq(root))
        .select_for_update()
        .fetch_tx(tx)
        .await
        .map_err(media_err_from_exec)?;
    let mut ids = vec![root];
    // Guards against a parent_id cycle.
    let mut seen = std::collections::HashSet::from([root]);
    let mut level = vec![root];
    while !level.is_empty() {
        let mut next = Vec::new();
        for part in level.chunks(chunk) {
            let children = MediaCollection::objects()
                .where_(MediaCollection::parent_id.is_in(part.iter().copied()))
                .where_(MediaCollection::deleted_at.is_null())
                .select_for_update()
                .fetch_tx(tx)
                .await
                .map_err(media_err_from_exec)?;
            next.extend(children.into_iter().filter_map(|c| match c.id {
                Auto::Set(id) if seen.insert(id) => Some(id),
                _ => None,
            }));
        }
        ids.extend(&next);
        level = next;
    }
    Ok(ids)
}

/// Decode one `popular_tags` row: a `MediaTag` plus the aggregate
/// `use_count`. `MediaTag`'s derived `FromRow` reads only the model's
/// own columns, so `use_count` is read here. One helper per backend.
#[cfg(feature = "postgres")]
fn decode_tag_with_count_pg(row: &sqlx::postgres::PgRow) -> Result<(MediaTag, i64), MediaError> {
    use sqlx::{FromRow as _, Row as _};
    let count: i64 = row.try_get("use_count").map_err(MediaError::Db)?;
    let tag = MediaTag::from_row(row).map_err(MediaError::Db)?;
    Ok((tag, count))
}

#[cfg(feature = "mysql")]
fn decode_tag_with_count_my(row: &sqlx::mysql::MySqlRow) -> Result<(MediaTag, i64), MediaError> {
    use sqlx::{FromRow as _, Row as _};
    let count: i64 = row.try_get("use_count").map_err(MediaError::Db)?;
    let tag = MediaTag::from_row(row).map_err(MediaError::Db)?;
    Ok((tag, count))
}

#[cfg(feature = "sqlite")]
fn decode_tag_with_count_sq(row: &sqlx::sqlite::SqliteRow) -> Result<(MediaTag, i64), MediaError> {
    use sqlx::{FromRow as _, Row as _};
    let count: i64 = row.try_get("use_count").map_err(MediaError::Db)?;
    let tag = MediaTag::from_row(row).map_err(MediaError::Db)?;
    Ok((tag, count))
}

/// The `Content-Type` stored with an upload. A client-declared type that
/// a browser would render or sniff into script becomes `application/octet-stream`.
fn stored_content_type(mime: &str) -> &str {
    // RFC 9110 `token` characters only: a space or stray byte makes a
    // browser ignore the type and sniff the body (#2570).
    fn is_token(s: &str) -> bool {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
    }
    let (main, params) = mime.split_once(';').unwrap_or((mime, ""));
    let main = main.trim().to_ascii_lowercase();
    let well_formed = main.split_once('/').is_some_and(|(t, s)| {
        is_token(t)
            && is_token(s)
            && t != "*"
            && s != "unknown"
            && main != "multipart/x-mixed-replace"
    });
    // Parameters can't change the type; just keep them header-safe.
    let params_ok = params.bytes().all(|b| (0x20..0x7f).contains(&b));
    let active = ["html", "xml", "javascript", "ecmascript", "xsl"]
        .iter()
        .any(|t| main.contains(t));
    if well_formed && params_ok && !active {
        mime
    } else {
        "application/octet-stream"
    }
}

/// Whether a direct upload landed as signed: the declared size and,
/// when the backend stores a type, the safe type for the row's MIME.
fn upload_matches(meta: &crate::storage::ObjectMeta, media: &Media) -> bool {
    // `type/subtype` only: a backend may rewrite the parameters.
    let norm = |t: &str| {
        t.split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase()
    };
    u64::try_from(media.size_bytes) == Ok(meta.size)
        && meta
            .content_type
            .as_deref()
            .is_none_or(|t| norm(t) == norm(stored_content_type(&media.mime)))
}

/// Build a storage key: `<prefix>/<uuid>-<sanitized filename>`.
fn build_key(prefix: &str, original_filename: &str) -> String {
    let prefix = prefix.trim_end_matches('/');
    let safe = sanitize_filename(original_filename);
    let uuid = uuid::Uuid::new_v4();
    if prefix.is_empty() {
        format!("{uuid}-{safe}")
    } else {
        format!("{prefix}/{uuid}-{safe}")
    }
}

// Basename only, safe ASCII, underscore for the rest.
//
// Identical to `crate::uploads::sanitize_filename`. They sit behind
// different feature gates (`media` vs `uploads`), so neither can call
// the other — change both or neither.
fn sanitize_filename(name: &str) -> String {
    // Split on both separators, not `Path::file_name`, which is
    // per-platform: `\` separates on Windows and is an ordinary
    // character elsewhere, so the same upload would be stored under a
    // different name depending on the server's OS.
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(name);
    let mut out = String::with_capacity(base.len());
    for c in base.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push_str("upload");
    }
    out
}

/// Default disk name when the registry has nothing configured. Use
/// this constant rather than repeating the literal in higher layers.
#[doc(hidden)]
pub const DEFAULT_DISK: &str = DEFAULT_DISK_NAME;

#[cfg(test)]
mod tests {
    use super::*;

    /// #2070 — `pool()` on a non-PG manager returns its pool, no panic.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn pool_works_on_a_sqlite_manager() {
        let pool = crate::sql::Pool::connect("sqlite::memory:").await.unwrap();
        let m = MediaManager::new_pool(pool, StorageRegistry::new());
        assert!(m.pool().as_sqlite().is_some());
    }

    #[test]
    fn media_status_round_trips_through_string() {
        for s in [
            MediaStatus::Pending,
            MediaStatus::Ready,
            MediaStatus::Failed,
        ] {
            let str_form = s.as_str();
            let parsed = MediaStatus::from_str(str_form).unwrap();
            assert_eq!(parsed, s);
        }
        assert!(MediaStatus::from_str("nonsense").is_none());
    }

    #[test]
    fn build_key_uses_uuid_prefix_and_keeps_extension() {
        let k = build_key("avatars", "alice.png");
        assert!(k.starts_with("avatars/"));
        assert!(k.ends_with("-alice.png"));
    }

    #[test]
    fn build_key_strips_trailing_slash_on_prefix() {
        let a = build_key("avatars", "a.png");
        let b = build_key("avatars/", "a.png");
        // Both should produce the same shape — exactly one slash.
        assert_eq!(a.matches('/').count(), 1);
        assert_eq!(b.matches('/').count(), 1);
    }

    #[test]
    fn build_key_handles_empty_prefix() {
        let k = build_key("", "a.png");
        assert!(!k.starts_with('/'));
        assert!(k.ends_with("-a.png"));
        assert_eq!(k.matches('/').count(), 0);
    }

    #[test]
    fn sanitize_strips_directory_and_unsafe_chars() {
        assert_eq!(sanitize_filename("../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("My File.png"), "My_File.png");
        assert_eq!(sanitize_filename("évil.jpg"), "_vil.jpg");
        assert_eq!(sanitize_filename(""), "upload");
        // Windows separators, asserted on every platform.
        assert_eq!(sanitize_filename("C:\\windows\\evil.exe"), "evil.exe");
        assert_eq!(sanitize_filename("C:/Users/me/photo.jpg"), "photo.jpg");
    }

    #[test]
    fn upload_intent_has_sane_defaults() {
        let i = UploadIntent::new("avatars", "image/png", "x.png", 100);
        assert_eq!(i.disk, "avatars");
        assert_eq!(i.ttl, Duration::from_secs(300));
        assert!(i.uploaded_by_id.is_none());
        assert!(i.key_prefix.is_empty());
    }

    #[test]
    fn media_is_ready_reflects_status_string() {
        let mut m = bare_media();
        m.status = "ready".into();
        assert!(m.is_ready());
        m.status = "pending".into();
        assert!(!m.is_ready());
        m.status = "garbage".into();
        assert!(!m.is_ready());
    }

    #[test]
    fn media_status_enum_handles_unknown_string() {
        let mut m = bare_media();
        m.status = "garbage".into();
        assert!(m.status_enum().is_none());
    }

    #[test]
    fn upload_matches_compares_the_main_type_only() {
        let mut m = bare_media();
        m.mime = "text/plain; charset=utf-8".into();
        let meta = |ct: &str| crate::storage::ObjectMeta::new(0, Some(ct.to_owned()));
        assert!(upload_matches(&meta("text/plain;charset=UTF-8"), &m));
        assert!(upload_matches(&meta("Text/Plain"), &m));
        assert!(!upload_matches(&meta("text/csv; charset=utf-8"), &m));
    }

    /// #2570 — malformed or multipart types are stored as octet-stream.
    #[test]
    fn stored_content_type_allows_only_token_types() {
        const OCTET: &str = "application/octet-stream";
        for bad in [
            "image/png x",
            "image /png",
            "image/png\tx",
            "image/png\r\nX-Evil: 1",
            "image/(png)",
            "multipart/x-mixed-replace; boundary=x",
            "Multipart/X-Mixed-Replace",
            "text/html",
            "image/svg+xml",
            "",
            "image/",
        ] {
            assert_eq!(stored_content_type(bad), OCTET, "{bad:?}");
        }
        for ok in [
            "image/png",
            "text/plain; charset=utf-8",
            "application/vnd.ms-excel",
            "audio/x-wav",
        ] {
            assert_eq!(stored_content_type(ok), ok);
        }
    }

    fn bare_media() -> Media {
        Media {
            id: Auto::Set(1),
            disk: "default".into(),
            storage_key: "k".into(),
            mime: "text/plain".into(),
            size_bytes: 0,
            original_filename: "x".into(),
            status: "ready".into(),
            uploaded_at: Auto::Set(Utc::now()),
            uploaded_by_id: None,
            derived_from_id: None,
            collection_id: None,
            metadata: serde_json::json!({}),
            deleted_at: None,
        }
    }
}
