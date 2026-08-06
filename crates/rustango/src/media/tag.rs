//! `MediaTag` — flat, free-form labels on [`Media`] rows.
//!
//! Sibling to [`crate::media::collection::MediaCollection`]: tags
//! express inclusive labels ("featured", "approved",
//! "homepage-hero"); collections express exclusive location.
//! M2M between Media and Tag via [`MediaTagLink`]
//! (`rustango_media_tag_links`).
//!
//! Tags are cheap to recreate, so deletion is hard (not soft) — the
//! junction rows cascade away with the FK.
//!
//! Both models are managed `#[derive(Model)]`s on `rustango_*` tables, so
//! their schema is emitted as ordinary **system migrations** (and, in
//! tests, materialized by [`crate::testkit::migrate_framework`]). There is
//! no lazy `ensure_*` creation layer — the tables exist because migrations
//! ran, exactly like the rest of the framework's own tables.
//!
//! [`Media`]: crate::media::Media

use crate::sql::Auto;

/// One free-form label. Cheap to clone.
#[derive(crate::Model, Debug, Clone)]
#[rustango(table = "rustango_media_tags")]
pub struct MediaTag {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 255)]
    pub name: String,
    /// Path-friendly id, unique across the table.
    #[rustango(max_length = 255, unique)]
    pub slug: String,
    /// Set on INSERT via the per-dialect `DEFAULT NOW()`.
    #[rustango(auto_now_add)]
    pub created_at: Auto<chrono::DateTime<chrono::Utc>>,
}

/// Junction row linking a [`Media`] to a [`MediaTag`] (the M2M table).
///
/// Carries a surrogate `Auto<i64>` PK so it's an ordinary managed model;
/// the logical key is the composite `UNIQUE(media_id, tag_id)`.
/// `MediaManager` raw-inserts only `(media_id, tag_id)` (relying on the
/// unique index for idempotency, never on a returned id), so the surrogate
/// PK is harmless.
///
/// [`Media`]: crate::media::Media
#[derive(crate::Model, Debug, Clone)]
#[rustango(
    table = "rustango_media_tag_links",
    unique_together = "media_id, tag_id"
)]
pub struct MediaTagLink {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub media_id: i64,
    #[rustango(fk = "rustango_media_tags", on = "id", on_delete = "cascade")]
    #[rustango(index)]
    pub tag_id: i64,
}
