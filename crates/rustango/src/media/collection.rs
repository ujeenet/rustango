//! `MediaCollection` — hierarchical "where the file lives" folders.
//! One [`Media`] row belongs to at most one collection; collections
//! nest via `parent_id`.
//!
//! Sibling to [`crate::media::tag::MediaTag`]: collections express
//! exclusive location ("/products/2026/launch/"), tags express
//! inclusive labels ("featured", "approved"). Both are
//! orthogonal — Media has at most one collection FK and any number
//! of tag M2M rows.
//!
//! Stored as a regular table, soft-deleted via `deleted_at`. `slug`
//! is unique and path-friendly. v0.38 — tri-dialect via
//! [`Self::ensure_table_pool`] + per-backend row decoders.
//!
//! [`Media`]: crate::media::Media

use chrono::{DateTime, Utc};
#[cfg(feature = "postgres")]
use sqlx::PgPool;

use crate::sql::Auto;

/// One folder. Cheap to clone.
#[derive(Debug, Clone)]
pub struct MediaCollection {
    pub id: Auto<i64>,
    pub name: String,
    /// Path-friendly id, unique across the table.
    pub slug: String,
    pub parent_id: Option<i64>,
    pub description: String,
    pub created_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

const CREATE_TABLE_SQL_PG: &str = "\
CREATE TABLE IF NOT EXISTS rustango_media_collections (
    id          BIGSERIAL PRIMARY KEY,
    name        TEXT        NOT NULL,
    slug        TEXT        NOT NULL UNIQUE,
    parent_id   BIGINT,
    description TEXT        NOT NULL DEFAULT '',
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    deleted_at  TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS rustango_media_collections_parent_idx
    ON rustango_media_collections (parent_id)
    WHERE deleted_at IS NULL";

const CREATE_TABLE_SQL_MYSQL: &str = "\
CREATE TABLE IF NOT EXISTS `rustango_media_collections` (
    `id`          BIGINT      NOT NULL AUTO_INCREMENT PRIMARY KEY,
    `name`        VARCHAR(255) NOT NULL,
    `slug`        VARCHAR(255) NOT NULL UNIQUE,
    `parent_id`   BIGINT,
    `description` TEXT         NOT NULL,
    `created_at`  DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    `deleted_at`  DATETIME(6)
);
CREATE INDEX `rustango_media_collections_parent_idx`
    ON `rustango_media_collections` (`parent_id`)";

const CREATE_TABLE_SQL_SQLITE: &str = "\
CREATE TABLE IF NOT EXISTS rustango_media_collections (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    name        TEXT     NOT NULL,
    slug        TEXT     NOT NULL UNIQUE,
    parent_id   INTEGER,
    description TEXT     NOT NULL DEFAULT '',
    created_at  TEXT     NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    deleted_at  TEXT
);
CREATE INDEX IF NOT EXISTS rustango_media_collections_parent_idx
    ON rustango_media_collections (parent_id) WHERE deleted_at IS NULL";

impl MediaCollection {
    /// PG back-compat shim around [`Self::ensure_table_pool`].
    ///
    /// # Errors
    /// Underlying sqlx DDL error.
    #[cfg(feature = "postgres")]
    pub async fn ensure_table(pool: &PgPool) -> Result<(), sqlx::Error> {
        Self::ensure_table_pool(&crate::sql::Pool::Postgres(pool.clone())).await
    }

    /// Create the `rustango_media_collections` table + supporting
    /// index if absent. Idempotent. Same pattern as
    /// [`crate::media::Media::ensure_table_pool`]. v0.38 — tri-dialect.
    ///
    /// MySQL skips the partial index (`WHERE deleted_at IS NULL`) since
    /// MySQL doesn't support partial indexes; the equivalent
    /// full-column index is created instead. SQLite supports partial
    /// indexes and uses the PG shape.
    ///
    /// # Errors
    /// Underlying sqlx DDL error.
    pub async fn ensure_table_pool(pool: &crate::sql::Pool) -> Result<(), sqlx::Error> {
        let ddl = match pool.dialect().name() {
            "postgres" => CREATE_TABLE_SQL_PG,
            "mysql" => CREATE_TABLE_SQL_MYSQL,
            "sqlite" => CREATE_TABLE_SQL_SQLITE,
            _ => CREATE_TABLE_SQL_PG,
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
                        if !super::tag::is_mysql_dup_index_error(&e) {
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

    /// PG row decoder — kept for in-crate callers using `PgRow`.
    /// Tri-dialect callers use the `sqlx::FromRow` impl below.
    #[cfg(feature = "postgres")]
    pub(super) fn decode_pg(row: &sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        let id: i64 = row.try_get("id")?;
        Ok(Self {
            id: Auto::Set(id),
            name: row.try_get("name")?,
            slug: row.try_get("slug")?,
            parent_id: row.try_get("parent_id")?,
            description: row.try_get("description")?,
            created_at: row.try_get("created_at")?,
            deleted_at: row.try_get("deleted_at")?,
        })
    }

    /// MySQL row decoder.
    #[cfg(feature = "mysql")]
    pub(super) fn decode_my(row: &sqlx::mysql::MySqlRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        let id: i64 = row.try_get("id")?;
        let created_at = super::tag::decode_my_datetime(row, "created_at")?;
        let deleted_at = super::tag::decode_my_datetime_opt(row, "deleted_at")?;
        Ok(Self {
            id: Auto::Set(id),
            name: row.try_get("name")?,
            slug: row.try_get("slug")?,
            parent_id: row.try_get("parent_id")?,
            description: row.try_get("description")?,
            created_at,
            deleted_at,
        })
    }

    /// SQLite row decoder.
    #[cfg(feature = "sqlite")]
    pub(super) fn decode_sq(row: &sqlx::sqlite::SqliteRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        let id: i64 = row.try_get("id")?;
        let created_at = super::tag::decode_sqlite_datetime(row, "created_at")?;
        let deleted_at = super::tag::decode_sqlite_datetime_opt(row, "deleted_at")?;
        Ok(Self {
            id: Auto::Set(id),
            name: row.try_get("name")?,
            slug: row.try_get("slug")?,
            parent_id: row.try_get("parent_id")?,
            description: row.try_get("description")?,
            created_at,
            deleted_at,
        })
    }
}

// v0.38 — FromRow impls so `raw_query_pool::<MediaCollection>` works
// on every backend.
#[cfg(feature = "postgres")]
impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for MediaCollection {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        Self::decode_pg(row)
    }
}
#[cfg(feature = "mysql")]
impl<'r> sqlx::FromRow<'r, sqlx::mysql::MySqlRow> for MediaCollection {
    fn from_row(row: &'r sqlx::mysql::MySqlRow) -> Result<Self, sqlx::Error> {
        Self::decode_my(row)
    }
}
#[cfg(feature = "sqlite")]
impl<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> for MediaCollection {
    fn from_row(row: &'r sqlx::sqlite::SqliteRow) -> Result<Self, sqlx::Error> {
        Self::decode_sq(row)
    }
}
