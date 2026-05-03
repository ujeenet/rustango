//! `MediaTag` — flat, free-form labels on [`Media`] rows.
//!
//! Sibling to [`crate::media::collection::MediaCollection`]: tags
//! express inclusive labels ("featured", "approved",
//! "homepage-hero"); collections express exclusive location.
//! M2M between Media and Tag via `rustango_media_tag_links`.
//!
//! Tags are cheap to recreate, so deletion is hard (not soft) — the
//! junction rows cascade away with the FK.
//!
//! [`Media`]: crate::media::Media

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};

use crate::sql::Auto;

#[derive(Debug, Clone)]
pub struct MediaTag {
    pub id: Auto<i64>,
    pub name: String,
    /// Path-friendly id, unique across the table.
    pub slug: String,
    pub created_at: DateTime<Utc>,
}

impl MediaTag {
    /// Create the `rustango_media_tags` + `rustango_media_tag_links`
    /// junction tables if absent. Idempotent.
    ///
    /// # Errors
    /// Underlying sqlx DDL error.
    pub async fn ensure_table(pool: &PgPool) -> Result<(), sqlx::Error> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS rustango_media_tags (
                id         BIGSERIAL PRIMARY KEY,
                name       TEXT        NOT NULL,
                slug       TEXT        NOT NULL UNIQUE,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
             )",
        )
        .execute(pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS rustango_media_tag_links (
                media_id BIGINT NOT NULL,
                tag_id   BIGINT NOT NULL,
                PRIMARY KEY (media_id, tag_id),
                FOREIGN KEY (tag_id)
                    REFERENCES rustango_media_tags (id)
                    ON DELETE CASCADE
             )",
        )
        .execute(pool)
        .await?;
        // Reverse-lookup index for `list_with_tag` queries.
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS rustango_media_tag_links_tag_idx
                ON rustango_media_tag_links (tag_id)",
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
            created_at: row.try_get("created_at")?,
        })
    }
}
