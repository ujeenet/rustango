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
//! Stored as a regular Postgres table, soft-deleted via
//! `deleted_at`. `slug` is unique and path-friendly.
//!
//! [`Media`]: crate::media::Media

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};

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

impl MediaCollection {
    /// Create the `rustango_media_collections` table + supporting
    /// index if absent. Idempotent. Same pattern as
    /// [`crate::media::Media::ensure_table`].
    ///
    /// # Errors
    /// Underlying sqlx DDL error.
    pub async fn ensure_table(pool: &PgPool) -> Result<(), sqlx::Error> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS rustango_media_collections (
                id          BIGSERIAL PRIMARY KEY,
                name        TEXT        NOT NULL,
                slug        TEXT        NOT NULL UNIQUE,
                parent_id   BIGINT,
                description TEXT        NOT NULL DEFAULT '',
                created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                deleted_at  TIMESTAMPTZ
             )",
        )
        .execute(pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS rustango_media_collections_parent_idx
                ON rustango_media_collections (parent_id)
                WHERE deleted_at IS NULL",
        )
        .execute(pool)
        .await?;
        Ok(())
    }

    pub(super) fn from_row(row: &sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
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
}
