//! `MediaTag` — flat, free-form labels on [`Media`] rows.
//!
//! Sibling to [`crate::media::collection::MediaCollection`]: tags are
//! inclusive labels ("featured", "approved"), collections are an
//! exclusive location. Media and Tag are M2M through [`MediaTagLink`]
//! (`rustango_media_tag_links`).
//!
//! Tags are cheap to recreate, so deletion is hard, not soft. The
//! junction rows cascade away with the FK.
//!
//! Both models are managed `#[derive(Model)]`s, so their schema ships
//! as system migrations (and in tests via
//! [`crate::testkit::migrate_framework`]).
//!
//! [`Media`]: crate::media::Media

use crate::sql::Auto;

/// One free-form label. Cheap to clone.
#[derive(crate::Model, Debug, Clone)]
// `permissions` so `auto_create_permissions` seeds
// `rustango_media_tags.{add,change,delete,view}`, the codenames
// `router::MediaPerms` checks. Not a column, so no migration.
#[rustango(table = "rustango_media_tags", permissions)]
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
/// It carries a surrogate `Auto<i64>` PK so it is an ordinary managed
/// model. The logical key is the composite `UNIQUE(media_id, tag_id)`,
/// which is what `MediaManager` relies on for idempotency.
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
