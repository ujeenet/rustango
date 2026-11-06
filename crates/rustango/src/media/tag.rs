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
#[rustango(table = "rustango_media_tags", access = "open", permissions)]
pub struct MediaTag {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 255)]
    #[rustango(help_text = "Tag name people see, e.g. `Summer 2026`.")]
    #[rustango(verbose_name = "Name")]
    pub name: String,
    /// Path-friendly id, unique across the table.
    #[rustango(max_length = 255, unique)]
    #[rustango(help_text = "Path-friendly id, e.g. `summer-2026`. Unique.")]
    #[rustango(verbose_name = "Slug")]
    pub slug: String,
    /// Set on INSERT via the per-dialect `DEFAULT NOW()`.
    #[rustango(auto_now_add)]
    #[rustango(help_text = "When the row was created. Set by the database.")]
    #[rustango(verbose_name = "Created at")]
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
    access = "open",
    unique_together = "media_id, tag_id"
)]
pub struct MediaTagLink {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(help_text = "The tagged file.")]
    #[rustango(verbose_name = "Media")]
    pub media_id: i64,
    #[rustango(fk = "rustango_media_tags", on = "id", on_delete = "cascade")]
    #[rustango(index)]
    #[rustango(help_text = "The tag on it.")]
    #[rustango(verbose_name = "Tag")]
    pub tag_id: i64,
}
