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
//! use rustango::media::{Media, MediaManager};
//! use rustango::storage::StorageRegistry;
//! use std::sync::Arc;
//!
//! // Once at startup:
//! Media::ensure_table(&pool).await?;
//!
//! let registry = StorageRegistry::new()
//!     .set("avatars", Arc::new(s3_storage))
//!     .with_default("avatars");
//!
//! let manager = MediaManager::new(pool.clone(), registry);
//!
//! // Server-side save (small files):
//! let media = manager.save_bytes(SaveOpts {
//!     disk: "avatars".into(),
//!     key_prefix: "users/".into(),
//!     bytes: png_bytes.clone(),
//!     mime: "image/png".into(),
//!     original_filename: "alice.png".into(),
//!     uploaded_by_id: Some(42),
//!     metadata: serde_json::json!({}),
//! }).await?;
//!
//! // Read:
//! let url = manager.url(&media);                  // CDN-aware
//! let download = manager.presigned_get(&media, Duration::from_secs(3600)).await;
//!
//! // Delete row + storage object via post_delete signal:
//! manager.delete(&media).await?;
//! ```
//!
//! ## Schema
//!
//! ```sql
//! CREATE TABLE rustango_media (
//!     id                BIGSERIAL PRIMARY KEY,
//!     disk              TEXT        NOT NULL,
//!     storage_key       TEXT        NOT NULL,
//!     mime              TEXT        NOT NULL,
//!     size_bytes        BIGINT      NOT NULL,
//!     original_filename TEXT        NOT NULL,
//!     status            TEXT        NOT NULL,            -- pending/ready/failed
//!     uploaded_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
//!     uploaded_by_id    BIGINT,                          -- soft FK to your User table
//!     derived_from_id   BIGINT,                          -- self-FK for variants/thumbnails
//!     metadata          JSONB       NOT NULL DEFAULT '{}'::JSONB,
//!     deleted_at        TIMESTAMPTZ                      -- soft delete
//! );
//! CREATE INDEX rustango_media_disk_key_idx ON rustango_media (disk, storage_key);
//! CREATE INDEX rustango_media_status_idx   ON rustango_media (status)
//!     WHERE deleted_at IS NULL;
//! ```

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(feature = "postgres")]
use sqlx::PgPool;

use crate::sql::Auto;
use crate::storage::{StorageError, StorageRegistry};

const CREATE_MEDIA_TABLE_SQL_PG: &str = "\
CREATE TABLE IF NOT EXISTS rustango_media (
    id                BIGSERIAL PRIMARY KEY,
    disk              TEXT        NOT NULL,
    storage_key       TEXT        NOT NULL,
    mime              TEXT        NOT NULL,
    size_bytes        BIGINT      NOT NULL,
    original_filename TEXT        NOT NULL,
    status            TEXT        NOT NULL,
    uploaded_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    uploaded_by_id    BIGINT,
    derived_from_id   BIGINT,
    collection_id     BIGINT,
    metadata          JSONB       NOT NULL DEFAULT '{}'::JSONB,
    deleted_at        TIMESTAMPTZ
);
ALTER TABLE rustango_media ADD COLUMN IF NOT EXISTS collection_id BIGINT;
CREATE INDEX IF NOT EXISTS rustango_media_disk_key_idx
    ON rustango_media (disk, storage_key);
CREATE INDEX IF NOT EXISTS rustango_media_status_idx
    ON rustango_media (status)
    WHERE deleted_at IS NULL;
CREATE INDEX IF NOT EXISTS rustango_media_collection_idx
    ON rustango_media (collection_id)
    WHERE deleted_at IS NULL";

const CREATE_MEDIA_TABLE_SQL_MYSQL: &str = "\
CREATE TABLE IF NOT EXISTS `rustango_media` (
    `id`                BIGINT      NOT NULL AUTO_INCREMENT PRIMARY KEY,
    `disk`              VARCHAR(255) NOT NULL,
    `storage_key`       VARCHAR(512) NOT NULL,
    `mime`              VARCHAR(255) NOT NULL,
    `size_bytes`        BIGINT      NOT NULL,
    `original_filename` VARCHAR(512) NOT NULL,
    `status`            VARCHAR(32)  NOT NULL,
    `uploaded_at`       DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    `uploaded_by_id`    BIGINT,
    `derived_from_id`   BIGINT,
    `collection_id`     BIGINT,
    `metadata`          JSON         NOT NULL,
    `deleted_at`        DATETIME(6)
);
CREATE INDEX `rustango_media_disk_key_idx`
    ON `rustango_media` (`disk`, `storage_key`);
CREATE INDEX `rustango_media_status_idx`
    ON `rustango_media` (`status`);
CREATE INDEX `rustango_media_collection_idx`
    ON `rustango_media` (`collection_id`)";

const CREATE_MEDIA_TABLE_SQL_SQLITE: &str = "\
CREATE TABLE IF NOT EXISTS rustango_media (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    disk              TEXT     NOT NULL,
    storage_key       TEXT     NOT NULL,
    mime              TEXT     NOT NULL,
    size_bytes        INTEGER  NOT NULL,
    original_filename TEXT     NOT NULL,
    status            TEXT     NOT NULL,
    uploaded_at       TEXT     NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    uploaded_by_id    INTEGER,
    derived_from_id   INTEGER,
    collection_id     INTEGER,
    metadata          TEXT     NOT NULL DEFAULT '{}',
    deleted_at        TEXT
);
CREATE INDEX IF NOT EXISTS rustango_media_disk_key_idx
    ON rustango_media (disk, storage_key);
CREATE INDEX IF NOT EXISTS rustango_media_status_idx
    ON rustango_media (status) WHERE deleted_at IS NULL;
CREATE INDEX IF NOT EXISTS rustango_media_collection_idx
    ON rustango_media (collection_id) WHERE deleted_at IS NULL";

pub mod collection;
pub mod tag;
pub use collection::MediaCollection;
pub use tag::MediaTag;

#[cfg(feature = "admin")]
pub mod router;

const DEFAULT_DISK_NAME: &str = "default";

/// One-call bootstrap for every table the `media` module needs:
/// `rustango_media`, `rustango_media_collections`,
/// `rustango_media_tags`, and the `rustango_media_tag_links`
/// junction. Also runs the idempotent ALTER that adds
/// `collection_id` to `rustango_media` for deployments that ran
/// the v0.21.51 ensure_table before this column existed.
///
/// Safe to call on every boot. v0.38 — PG back-compat shim around
/// [`ensure_all_tables_pool`].
///
/// # Errors
/// Surfaces the underlying sqlx error if any DDL fails.
#[cfg(feature = "postgres")]
pub async fn ensure_all_tables(pool: &sqlx::PgPool) -> Result<(), sqlx::Error> {
    ensure_all_tables_pool(&crate::sql::Pool::Postgres(pool.clone())).await
}

/// Tri-dialect counterpart of [`ensure_all_tables`] (v0.38). Routes
/// each `ensure_table_pool` through the unified [`crate::sql::Pool`]
/// enum so the media bootstrap works on PG / SQLite / MySQL.
///
/// # Errors
/// Surfaces the underlying sqlx error if any DDL fails.
pub async fn ensure_all_tables_pool(pool: &crate::sql::Pool) -> Result<(), sqlx::Error> {
    Media::ensure_table_pool(pool).await?;
    MediaCollection::ensure_table_pool(pool).await?;
    MediaTag::ensure_table_pool(pool).await?;
    Ok(())
}

/// Lifecycle state of a Media row.
///
/// - `Pending` — row exists but the storage object hasn't been
///   confirmed (typical for direct-browser-upload flows: row created
///   when the presigned URL was issued; storage object lands later).
/// - `Ready` — the storage object has been confirmed to exist.
/// - `Failed` — finalize attempted but the object wasn't there.
///   (Useful for purge sweeps + audit.)
///
/// Stored on the database as a single TEXT column so callers can
/// filter / order without bespoke type handling.
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
#[derive(Debug, Clone)]
pub struct Media {
    pub id: Auto<i64>,
    pub disk: String,
    pub storage_key: String,
    pub mime: String,
    pub size_bytes: i64,
    pub original_filename: String,
    pub status: String, // MediaStatus serialized as &str
    pub uploaded_at: DateTime<Utc>,
    pub uploaded_by_id: Option<i64>,
    pub derived_from_id: Option<i64>,
    /// Optional FK to `rustango_media_collections.id` — the
    /// "where it lives" folder. NULL means "loose" (in the root).
    pub collection_id: Option<i64>,
    pub metadata: Value,
    pub deleted_at: Option<DateTime<Utc>>,
}

impl Media {
    /// Create the `rustango_media` table + supporting indexes if
    /// they don't exist. Idempotent. Safe to call on every boot.
    ///
    /// We use raw DDL (matching the `PgJobQueue::ensure_table`
    /// pattern shipped in v0.21.14) rather than threading through
    /// the migration system, because Media is a framework-shipped
    /// table rather than user-authored.
    ///
    /// # Errors
    /// Underlying sqlx error if the DDL fails (insufficient
    /// privileges, connection issue, etc.).
    #[cfg(feature = "postgres")]
    pub async fn ensure_table(pool: &PgPool) -> Result<(), sqlx::Error> {
        Self::ensure_table_pool(&crate::sql::Pool::Postgres(pool.clone())).await
    }

    /// v0.38 — tri-dialect `rustango_media` table bootstrap. Dispatches
    /// per-dialect DDL (BIGSERIAL/SERIAL/AUTOINCREMENT, TIMESTAMPTZ/
    /// DATETIME(6)/TEXT-ISO-8601, JSONB/JSON/TEXT-as-JSON). PG + SQLite
    /// keep the partial indexes (`WHERE deleted_at IS NULL`); MySQL
    /// uses full-column indexes since it doesn't support partial
    /// indexes.
    ///
    /// # Errors
    /// Underlying sqlx DDL error.
    pub async fn ensure_table_pool(pool: &crate::sql::Pool) -> Result<(), sqlx::Error> {
        let ddl = match pool.dialect().name() {
            "postgres" => CREATE_MEDIA_TABLE_SQL_PG,
            "mysql" => CREATE_MEDIA_TABLE_SQL_MYSQL,
            "sqlite" => CREATE_MEDIA_TABLE_SQL_SQLITE,
            _ => CREATE_MEDIA_TABLE_SQL_PG,
        };
        for stmt in ddl.split(';') {
            let trimmed = stmt.trim();
            if trimmed.is_empty() {
                continue;
            }
            match pool {
                #[cfg(feature = "postgres")]
                crate::sql::Pool::Postgres(pg) => {
                    sqlx::query(trimmed).execute(pg).await?;
                }
                #[cfg(feature = "mysql")]
                crate::sql::Pool::Mysql(my) => {
                    if let Err(e) = sqlx::query(trimmed).execute(my).await {
                        if !crate::media::tag::is_mysql_dup_index_error(&e) {
                            return Err(e);
                        }
                    }
                }
                #[cfg(feature = "sqlite")]
                crate::sql::Pool::Sqlite(sq) => {
                    sqlx::query(trimmed).execute(sq).await?;
                }
            }
        }
        Ok(())
    }

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

    #[cfg(feature = "postgres")]
    fn decode_pg(row: &sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        let id: i64 = row.try_get("id")?;
        Ok(Self {
            id: Auto::Set(id),
            disk: row.try_get("disk")?,
            storage_key: row.try_get("storage_key")?,
            mime: row.try_get("mime")?,
            size_bytes: row.try_get("size_bytes")?,
            original_filename: row.try_get("original_filename")?,
            status: row.try_get("status")?,
            uploaded_at: row.try_get("uploaded_at")?,
            uploaded_by_id: row.try_get("uploaded_by_id")?,
            derived_from_id: row.try_get("derived_from_id")?,
            collection_id: row.try_get("collection_id")?,
            metadata: row.try_get("metadata")?,
            deleted_at: row.try_get("deleted_at")?,
        })
    }

    /// MySQL row decoder (v0.38).
    #[cfg(feature = "mysql")]
    fn decode_my(row: &sqlx::mysql::MySqlRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        let id: i64 = row.try_get("id")?;
        let uploaded_at = crate::media::tag::decode_my_datetime(row, "uploaded_at")?;
        let deleted_at = crate::media::tag::decode_my_datetime_opt(row, "deleted_at")?;
        let metadata: sqlx::types::Json<Value> = row.try_get("metadata")?;
        Ok(Self {
            id: Auto::Set(id),
            disk: row.try_get("disk")?,
            storage_key: row.try_get("storage_key")?,
            mime: row.try_get("mime")?,
            size_bytes: row.try_get("size_bytes")?,
            original_filename: row.try_get("original_filename")?,
            status: row.try_get("status")?,
            uploaded_at,
            uploaded_by_id: row.try_get("uploaded_by_id")?,
            derived_from_id: row.try_get("derived_from_id")?,
            collection_id: row.try_get("collection_id")?,
            metadata: metadata.0,
            deleted_at,
        })
    }

    /// SQLite row decoder (v0.38). `metadata` is stored as TEXT-as-JSON.
    #[cfg(feature = "sqlite")]
    fn decode_sq(row: &sqlx::sqlite::SqliteRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        let id: i64 = row.try_get("id")?;
        let uploaded_at = crate::media::tag::decode_sqlite_datetime(row, "uploaded_at")?;
        let deleted_at = crate::media::tag::decode_sqlite_datetime_opt(row, "deleted_at")?;
        let meta_text: String = row.try_get("metadata")?;
        let metadata: Value = serde_json::from_str(&meta_text).unwrap_or(Value::Null);
        Ok(Self {
            id: Auto::Set(id),
            disk: row.try_get("disk")?,
            storage_key: row.try_get("storage_key")?,
            mime: row.try_get("mime")?,
            size_bytes: row.try_get("size_bytes")?,
            original_filename: row.try_get("original_filename")?,
            status: row.try_get("status")?,
            uploaded_at,
            uploaded_by_id: row.try_get("uploaded_by_id")?,
            derived_from_id: row.try_get("derived_from_id")?,
            collection_id: row.try_get("collection_id")?,
            metadata,
            deleted_at,
        })
    }
}

// v0.38 — FromRow impls so `raw_query_pool::<Media>` dispatches via
// the unified `crate::sql::Pool` enum. Each backend's FromRow forwards
// to the corresponding inherent `decode_*` above.
#[cfg(feature = "postgres")]
impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for Media {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        Self::decode_pg(row)
    }
}
#[cfg(feature = "mysql")]
impl<'r> sqlx::FromRow<'r, sqlx::mysql::MySqlRow> for Media {
    fn from_row(row: &'r sqlx::mysql::MySqlRow) -> Result<Self, sqlx::Error> {
        Self::decode_my(row)
    }
}
#[cfg(feature = "sqlite")]
impl<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> for Media {
    fn from_row(row: &'r sqlx::sqlite::SqliteRow) -> Result<Self, sqlx::Error> {
        Self::decode_sq(row)
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
}

// =====================================================================
// MediaManager
// =====================================================================

/// Glue between the `Media` model and a [`StorageRegistry`]. Cheap
/// to clone — internal state is `Arc`-shared.
///
/// v0.38 — tri-dialect. Every query method dispatches per backend
/// (PG / MySQL 8+ / SQLite) via the unified [`crate::sql::Pool`] enum.
/// PG-specific idioms (`ANY($1)`, `NOW() - INTERVAL`,
/// `DELETE … USING`, `ON CONFLICT DO UPDATE`, `INSERT … RETURNING`)
/// are rewritten as portable equivalents (pre-computed timestamps
/// bound from Rust, `IN (?, ?, …)` expansions, subquery rewrites,
/// `ON DUPLICATE KEY UPDATE` on MySQL, `SELECT … LAST_INSERT_ID()`
/// in a transaction on MySQL).
#[derive(Clone)]
pub struct MediaManager {
    pool: crate::sql::Pool,
    registry: StorageRegistry,
}

impl MediaManager {
    /// PG back-compat constructor.
    #[cfg(feature = "postgres")]
    #[must_use]
    pub fn new(pool: PgPool, registry: StorageRegistry) -> Self {
        Self::new_pool(crate::sql::Pool::Postgres(pool), registry)
    }

    /// Tri-dialect constructor (v0.38).
    #[must_use]
    pub fn new_pool(pool: impl Into<crate::sql::Pool>, registry: StorageRegistry) -> Self {
        Self {
            pool: pool.into(),
            registry,
        }
    }

    #[must_use]
    pub fn registry(&self) -> &StorageRegistry {
        &self.registry
    }

    /// PG back-compat accessor. Panics if the manager wraps a non-PG
    /// pool — prefer [`Self::pool_dyn`] for tri-dialect code.
    #[cfg(feature = "postgres")]
    #[must_use]
    pub fn pool(&self) -> &PgPool {
        match &self.pool {
            crate::sql::Pool::Postgres(pg) => pg,
            #[cfg(any(feature = "mysql", feature = "sqlite"))]
            _ => panic!("MediaManager::pool() called on a non-PG manager; use pool_dyn() instead"),
        }
    }

    /// Tri-dialect accessor — the unified [`crate::sql::Pool`] enum.
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
        storage.save(&key, &opts.bytes).await?;
        self.insert_row(InsertRow {
            disk: opts.disk,
            storage_key: key,
            mime: opts.mime,
            size_bytes,
            original_filename: opts.original_filename,
            status: MediaStatus::Ready,
            uploaded_by_id: opts.uploaded_by_id,
            derived_from_id: None,
            collection_id: opts.collection_id,
            metadata: opts.metadata,
        })
        .await
    }

    // --------- begin / finalize (direct browser upload)

    /// Issue a presigned PUT URL for direct browser upload, and
    /// pre-create a `Media` row in `Pending` state. The browser PUTs
    /// directly to S3 (or compatible); the server later calls
    /// [`Self::finalize_upload`] to verify the object landed and
    /// flip the row to `Ready`.
    ///
    /// # Errors
    /// `UnknownDisk` / `Db` / a `Storage` error if the backend doesn't
    /// support presigned URLs.
    pub async fn begin_upload(&self, intent: UploadIntent) -> Result<UploadTicket, MediaError> {
        let storage = self.resolve_disk(&intent.disk)?;
        let key = build_key(&intent.key_prefix, &intent.original_filename);
        let upload_url = storage
            .presigned_put_url(&key, intent.ttl, Some(&intent.mime))
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
        })
    }

    /// Confirm the storage object exists for `media_id` and flip
    /// the row from `Pending` to `Ready`. If the object isn't there,
    /// flip to `Failed` instead so a purge sweep can clean it up.
    ///
    /// Returns the (possibly-updated) row regardless.
    ///
    /// # Errors
    /// `Db` if the row doesn't exist or the update fails. `Storage`
    /// for transport failures during the `exists` check.
    pub async fn finalize_upload(&self, media_id: i64) -> Result<Media, MediaError> {
        let media = self
            .get(media_id)
            .await?
            .ok_or_else(|| MediaError::Other(format!("media {media_id} not found")))?;
        let storage = self.resolve_disk(&media.disk)?;
        let exists = storage.exists(&media.storage_key).await?;
        let new_status = if exists {
            MediaStatus::Ready
        } else {
            MediaStatus::Failed
        };
        let d = self.pool.dialect();
        let sql = format!(
            "UPDATE rustango_media SET status = {p1} WHERE id = {p2}",
            p1 = d.placeholder(1),
            p2 = d.placeholder(2),
        );
        match &self.pool {
            #[cfg(feature = "postgres")]
            crate::sql::Pool::Postgres(pg) => {
                sqlx::query(&sql)
                    .bind(new_status.as_str())
                    .bind(media_id)
                    .execute(pg)
                    .await?;
            }
            #[cfg(feature = "mysql")]
            crate::sql::Pool::Mysql(my) => {
                sqlx::query(&sql)
                    .bind(new_status.as_str())
                    .bind(media_id)
                    .execute(my)
                    .await?;
            }
            #[cfg(feature = "sqlite")]
            crate::sql::Pool::Sqlite(sq) => {
                sqlx::query(&sql)
                    .bind(new_status.as_str())
                    .bind(media_id)
                    .execute(sq)
                    .await?;
            }
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

    /// Soft-delete: mark `deleted_at = NOW()`. The storage object
    /// stays put — purge it later via [`Self::purge`] or wait for
    /// the `purge_orphans` sweep.
    pub async fn delete(&self, m: &Media) -> Result<(), MediaError> {
        let id = match m.id {
            Auto::Set(v) => v,
            _ => return Err(MediaError::Other("Media has no id".into())),
        };
        let d = self.pool.dialect();
        // v0.38 — bind `Utc::now()` from Rust so the same SQL works
        // on PG / MySQL / SQLite without per-dialect `NOW()` SQL.
        let sql = format!(
            "UPDATE rustango_media SET deleted_at = {now} WHERE id = {p}",
            now = d.placeholder(1),
            p = d.placeholder(2),
        );
        crate::sql::raw_execute_pool(
            &self.pool,
            &sql,
            vec![
                crate::core::SqlValue::DateTime(Utc::now()),
                crate::core::SqlValue::I64(id),
            ],
        )
        .await
        .map_err(media_err_from_exec)?;
        Ok(())
    }

    /// Hard-delete: remove the storage object AND the row. Use for
    /// "I'm sure I want this gone right now" — typically called by
    /// the post_delete signal once `delete()` has soft-deleted.
    pub async fn purge(&self, m: &Media) -> Result<(), MediaError> {
        let id = match m.id {
            Auto::Set(v) => v,
            _ => return Err(MediaError::Other("Media has no id".into())),
        };
        // Best-effort storage delete (matches Storage::delete trait
        // semantics — missing key is fine).
        if let Some(storage) = self.registry.disk(&m.disk) {
            let _ = storage.delete(&m.storage_key).await;
        }
        let p = self.pool.dialect().placeholder(1);
        let sql = format!("DELETE FROM rustango_media WHERE id = {p}");
        crate::sql::raw_execute_pool(&self.pool, &sql, vec![crate::core::SqlValue::I64(id)])
            .await
            .map_err(media_err_from_exec)?;
        Ok(())
    }

    /// Hard-delete every soft-deleted Media row older than
    /// `older_than`, removing the storage object as we go. Returns
    /// the count of rows purged.
    ///
    /// Run from the [`crate::scheduler`] (e.g. nightly) to keep
    /// orphan storage objects from accumulating.
    pub async fn purge_orphans(&self, older_than: Duration) -> Result<u64, MediaError> {
        // v0.38 — cutoff pre-computed in Rust so the same SQL runs
        // on PG / MySQL / SQLite without per-dialect `NOW() - INTERVAL`.
        let cutoff = Utc::now()
            - chrono::Duration::from_std(older_than).unwrap_or(chrono::Duration::seconds(0));
        let p = self.pool.dialect().placeholder(1);
        let sql = format!(
            "SELECT id, disk, storage_key, mime, size_bytes, original_filename, \
                    status, uploaded_at, uploaded_by_id, derived_from_id, \
                    collection_id, metadata, deleted_at \
               FROM rustango_media \
              WHERE deleted_at IS NOT NULL AND deleted_at < {p}"
        );
        let rows: Vec<Media> = crate::sql::raw_query_pool(
            &sql,
            vec![crate::core::SqlValue::DateTime(cutoff)],
            &self.pool,
        )
        .await
        .map_err(media_err_from_exec)?;
        let mut purged = 0u64;
        for m in rows {
            self.purge(&m).await?;
            purged += 1;
        }
        Ok(purged)
    }

    /// Hard-delete every Media row stuck in `Pending` for longer
    /// than `older_than`. Direct-browser-upload flows leave Pending
    /// rows behind when the browser abandons before calling
    /// `finalize_upload`; this sweep cleans them up.
    pub async fn purge_pending(&self, older_than: Duration) -> Result<u64, MediaError> {
        let cutoff = Utc::now()
            - chrono::Duration::from_std(older_than).unwrap_or(chrono::Duration::seconds(0));
        let p = self.pool.dialect().placeholder(1);
        let sql =
            format!("DELETE FROM rustango_media WHERE status = 'pending' AND uploaded_at < {p}");
        crate::sql::raw_execute_pool(
            &self.pool,
            &sql,
            vec![crate::core::SqlValue::DateTime(cutoff)],
        )
        .await
        .map_err(media_err_from_exec)
    }

    // =================================================================
    // Collections (folders)
    // =================================================================

    /// Create a new collection. `slug` must be unique. `parent` may be
    /// `None` (root) or another collection's id (sub-folder).
    ///
    /// # Errors
    /// `Db` for unique-constraint violations on `slug` or any other
    /// underlying sqlx error.
    pub async fn create_collection(
        &self,
        name: impl Into<String>,
        slug: impl Into<String>,
        parent: Option<i64>,
        description: impl Into<String>,
    ) -> Result<MediaCollection, MediaError> {
        let name = name.into();
        let slug = slug.into();
        let description = description.into();
        let parent_val = parent
            .map(crate::core::SqlValue::I64)
            .unwrap_or(crate::core::SqlValue::Null);
        let d = self.pool.dialect();
        let insert_cols = "(name, slug, parent_id, description)";
        let insert_vals = format!(
            "({p1}, {p2}, {p3}, {p4})",
            p1 = d.placeholder(1),
            p2 = d.placeholder(2),
            p3 = d.placeholder(3),
            p4 = d.placeholder(4),
        );
        let select_cols = "id, name, slug, parent_id, description, created_at, deleted_at";
        // MySQL has no `UPDATE … RETURNING`; PG + SQLite (≥3.35) do.
        // Branch on dialect to keep the SQL portable.
        if self.pool.dialect().name() == "mysql" {
            #[cfg(feature = "mysql")]
            {
                let crate::sql::Pool::Mysql(my) = &self.pool else {
                    unreachable!("dialect name matched mysql but variant didn't");
                };
                let insert_sql = format!(
                    "INSERT INTO `rustango_media_collections` {insert_cols} VALUES {insert_vals}"
                );
                let mut tx = my.begin().await?;
                sqlx::query(&insert_sql)
                    .bind(&name)
                    .bind(&slug)
                    .bind(parent)
                    .bind(&description)
                    .execute(&mut *tx)
                    .await?;
                let row = sqlx::query(&format!(
                    "SELECT {select_cols} FROM `rustango_media_collections` \
                     WHERE id = LAST_INSERT_ID()"
                ))
                .fetch_one(&mut *tx)
                .await?;
                let out = MediaCollection::decode_my(&row)?;
                tx.commit().await?;
                return Ok(out);
            }
            #[cfg(not(feature = "mysql"))]
            unreachable!("dialect reports mysql but Cargo feature is disabled");
        }
        let sql = format!(
            "INSERT INTO rustango_media_collections {insert_cols} \
             VALUES {insert_vals} RETURNING {select_cols}"
        );
        let rows: Vec<MediaCollection> = crate::sql::raw_query_pool(
            &sql,
            vec![
                crate::core::SqlValue::String(name),
                crate::core::SqlValue::String(slug),
                parent_val,
                crate::core::SqlValue::String(description),
            ],
            &self.pool,
        )
        .await
        .map_err(media_err_from_exec)?;
        rows.into_iter()
            .next()
            .ok_or_else(|| MediaError::Other("collection INSERT returned no rows".into()))
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

    /// List every non-deleted collection, ordered by `(parent_id, name)`
    /// so siblings group together — handy for tree-renderers.
    pub async fn list_collections(&self) -> Result<Vec<MediaCollection>, MediaError> {
        // v0.38 — `ORDER BY parent_id IS NULL DESC, parent_id, name`
        // is the portable "NULLs first" workaround: MySQL doesn't
        // support `NULLS FIRST` but `IS NULL` is portable across
        // PG / MySQL / SQLite.
        let sql = "SELECT id, name, slug, parent_id, description, created_at, deleted_at \
                   FROM rustango_media_collections \
                   WHERE deleted_at IS NULL \
                   ORDER BY parent_id IS NULL DESC, parent_id, name";
        let rows: Vec<MediaCollection> = crate::sql::raw_query_pool(sql, vec![], &self.pool)
            .await
            .map_err(media_err_from_exec)?;
        Ok(rows)
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

    /// Soft-delete a collection. Media inside it is NOT deleted —
    /// rows are orphaned (`collection_id` set to NULL) so they remain
    /// queryable and the storage objects survive.
    pub async fn delete_collection(&self, id: i64) -> Result<(), MediaError> {
        let d = self.pool.dialect();
        let p1 = d.placeholder(1);
        let p2 = d.placeholder(2);
        let orphan_sql =
            format!("UPDATE rustango_media SET collection_id = NULL WHERE collection_id = {p1}");
        crate::sql::raw_execute_pool(
            &self.pool,
            &orphan_sql,
            vec![crate::core::SqlValue::I64(id)],
        )
        .await
        .map_err(media_err_from_exec)?;
        // Bind `Utc::now()` from Rust so the SQL is portable.
        let soft_delete_sql =
            format!("UPDATE rustango_media_collections SET deleted_at = {p1} WHERE id = {p2}");
        crate::sql::raw_execute_pool(
            &self.pool,
            &soft_delete_sql,
            vec![
                crate::core::SqlValue::DateTime(Utc::now()),
                crate::core::SqlValue::I64(id),
            ],
        )
        .await
        .map_err(media_err_from_exec)?;
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
    pub async fn list_in_collection(
        &self,
        collection_id: i64,
        recursive: bool,
    ) -> Result<Vec<Media>, MediaError> {
        let ids: Vec<i64> = if recursive {
            self.collect_descendant_ids(collection_id).await?
        } else {
            vec![collection_id]
        };
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        // v0.38 — `ANY($1)` is PG-only. Expand to `IN (?, ?, …)`
        // with one placeholder per id; works on every backend.
        let d = self.pool.dialect();
        let placeholders: Vec<String> = (1..=ids.len()).map(|i| d.placeholder(i)).collect();
        let in_list = placeholders.join(", ");
        let sql = format!(
            "SELECT id, disk, storage_key, mime, size_bytes, original_filename, \
                    status, uploaded_at, uploaded_by_id, derived_from_id, \
                    collection_id, metadata, deleted_at \
               FROM rustango_media \
              WHERE collection_id IN ({in_list}) AND deleted_at IS NULL \
              ORDER BY uploaded_at DESC"
        );
        let binds: Vec<crate::core::SqlValue> =
            ids.into_iter().map(crate::core::SqlValue::I64).collect();
        let rows: Vec<Media> = crate::sql::raw_query_pool(&sql, binds, &self.pool)
            .await
            .map_err(media_err_from_exec)?;
        Ok(rows)
    }

    async fn collect_descendant_ids(&self, root: i64) -> Result<Vec<i64>, MediaError> {
        // Recursive CTE walks the parent_id chain. The same syntax
        // works on PG, MySQL 8+, and SQLite 3.8+.
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
        // Per-dialect upsert syntax:
        //   PG     : INSERT … ON CONFLICT (slug) DO UPDATE SET name = EXCLUDED.name RETURNING …
        //   SQLite : same (3.24+ supports `ON CONFLICT`)
        //   MySQL  : INSERT … ON DUPLICATE KEY UPDATE name = VALUES(name); then SELECT
        let d = self.pool.dialect();
        let (p1, p2) = (d.placeholder(1), d.placeholder(2));
        if d.name() == "mysql" {
            #[cfg(feature = "mysql")]
            {
                let crate::sql::Pool::Mysql(my) = &self.pool else {
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
                let row = sqlx::query(
                    "SELECT id, name, slug, created_at FROM `rustango_media_tags` WHERE slug = ?",
                )
                .bind(slug)
                .fetch_one(&mut *tx)
                .await?;
                let out = MediaTag::decode_my(&row)?;
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
    pub async fn tag(&self, media_id: i64, slugs: &[&str]) -> Result<(), MediaError> {
        let d = self.pool.dialect();
        let (p1, p2) = (d.placeholder(1), d.placeholder(2));
        // PG/SQLite: ON CONFLICT DO NOTHING; MySQL: INSERT IGNORE.
        let sql = if d.name() == "mysql" {
            format!(
                "INSERT IGNORE INTO `rustango_media_tag_links` (`media_id`, `tag_id`) \
                 VALUES ({p1}, {p2})"
            )
        } else {
            format!(
                "INSERT INTO rustango_media_tag_links (media_id, tag_id) \
                 VALUES ({p1}, {p2}) ON CONFLICT DO NOTHING"
            )
        };
        for slug in slugs {
            let t = self.ensure_tag(slug).await?;
            let tag_id = match t.id {
                Auto::Set(v) => v,
                _ => continue,
            };
            crate::sql::raw_execute_pool(
                &self.pool,
                &sql,
                vec![
                    crate::core::SqlValue::I64(media_id),
                    crate::core::SqlValue::I64(tag_id),
                ],
            )
            .await
            .map_err(media_err_from_exec)?;
        }
        Ok(())
    }

    /// Remove a single tag from a media row.
    pub async fn untag(&self, media_id: i64, slug: &str) -> Result<(), MediaError> {
        // v0.38 — `DELETE … USING` is PG-only. Rewrite as a subquery,
        // which works on every backend.
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
    pub async fn set_tags(&self, media_id: i64, slugs: &[&str]) -> Result<(), MediaError> {
        let p = self.pool.dialect().placeholder(1);
        let sql = format!("DELETE FROM rustango_media_tag_links WHERE media_id = {p}");
        crate::sql::raw_execute_pool(&self.pool, &sql, vec![crate::core::SqlValue::I64(media_id)])
            .await
            .map_err(media_err_from_exec)?;
        self.tag(media_id, slugs).await
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
              ORDER BY m.uploaded_at DESC \
              LIMIT {p2} OFFSET {p3}"
        );
        let rows: Vec<Media> = crate::sql::raw_query_pool(
            &sql,
            vec![
                crate::core::SqlValue::String(slug.to_owned()),
                crate::core::SqlValue::I64(limit.max(1).min(1000)),
                crate::core::SqlValue::I64(offset.max(0)),
            ],
            &self.pool,
        )
        .await
        .map_err(media_err_from_exec)?;
        Ok(rows)
    }

    /// Top tags by usage count, descending. Limit clamped at 1000.
    pub async fn popular_tags(&self, limit: i64) -> Result<Vec<(MediaTag, i64)>, MediaError> {
        let p = self.pool.dialect().placeholder(1);
        let sql = format!(
            "SELECT t.id, t.name, t.slug, t.created_at, COUNT(l.media_id) AS use_count \
               FROM rustango_media_tags t \
               LEFT JOIN rustango_media_tag_links l ON l.tag_id = t.id \
              GROUP BY t.id, t.name, t.slug, t.created_at \
              ORDER BY use_count DESC, t.slug \
              LIMIT {p}"
        );
        // Per-backend decoding because the `use_count` aggregate
        // column isn't part of the MediaTag schema — we read it
        // separately alongside the tag's columns.
        match &self.pool {
            #[cfg(feature = "postgres")]
            crate::sql::Pool::Postgres(pg) => {
                use sqlx::Row as _;
                let rows = sqlx::query(&sql)
                    .bind(limit.max(1).min(1000))
                    .fetch_all(pg)
                    .await?;
                rows.into_iter()
                    .map(|r| {
                        let count: i64 = r.try_get("use_count").map_err(MediaError::Db)?;
                        let tag = MediaTag::decode_pg(&r).map_err(MediaError::Db)?;
                        Ok((tag, count))
                    })
                    .collect()
            }
            #[cfg(feature = "mysql")]
            crate::sql::Pool::Mysql(my) => {
                use sqlx::Row as _;
                let rows = sqlx::query(&sql)
                    .bind(limit.max(1).min(1000))
                    .fetch_all(my)
                    .await?;
                rows.into_iter()
                    .map(|r| {
                        let count: i64 = r.try_get("use_count").map_err(MediaError::Db)?;
                        let tag = MediaTag::decode_my(&r).map_err(MediaError::Db)?;
                        Ok((tag, count))
                    })
                    .collect()
            }
            #[cfg(feature = "sqlite")]
            crate::sql::Pool::Sqlite(sq) => {
                use sqlx::Row as _;
                let rows = sqlx::query(&sql)
                    .bind(limit.max(1).min(1000))
                    .fetch_all(sq)
                    .await?;
                rows.into_iter()
                    .map(|r| {
                        let count: i64 = r.try_get("use_count").map_err(MediaError::Db)?;
                        let tag = MediaTag::decode_sq(&r).map_err(MediaError::Db)?;
                        Ok((tag, count))
                    })
                    .collect()
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
                let crate::sql::Pool::Mysql(my) = &self.pool else {
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
                let row = sqlx::query(&format!(
                    "SELECT {select_cols} FROM `rustango_media` WHERE id = LAST_INSERT_ID()"
                ))
                .fetch_one(&mut *tx)
                .await?;
                let out = Media::decode_my(&row)?;
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

/// Build a storage key: `<prefix>/<uuid>-<sanitized filename>`.
/// Same sanitization rules as the `uploads` module's
/// `sanitize_filename` (basename only; safe ASCII; underscore for
/// the rest).
/// Convert a framework `ExecError` to a `MediaError` — pulls the
/// underlying sqlx error out of `Driver` so callers that match on
/// `MediaError::Db` still work; everything else lands in `Other`.
fn media_err_from_exec(e: crate::sql::ExecError) -> MediaError {
    match e {
        crate::sql::ExecError::Driver(e) => MediaError::Db(e),
        other => MediaError::Other(other.to_string()),
    }
}

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

fn sanitize_filename(name: &str) -> String {
    let base = std::path::Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
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

/// Default disk name when the registry has nothing configured —
/// prefer matching this constant when constructing default `disk`
/// strings in higher layers (router, manager helpers, etc.).
#[doc(hidden)]
pub const DEFAULT_DISK: &str = DEFAULT_DISK_NAME;

#[cfg(test)]
mod tests {
    use super::*;

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

    fn bare_media() -> Media {
        Media {
            id: Auto::Set(1),
            disk: "default".into(),
            storage_key: "k".into(),
            mime: "text/plain".into(),
            size_bytes: 0,
            original_filename: "x".into(),
            status: "ready".into(),
            uploaded_at: Utc::now(),
            uploaded_by_id: None,
            derived_from_id: None,
            collection_id: None,
            metadata: serde_json::json!({}),
            deleted_at: None,
        }
    }
}
