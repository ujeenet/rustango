//! `MediaCollection` — hierarchical "where the file lives" folders.
//! One [`Media`] row belongs to at most one collection; collections
//! nest via `parent_id`.
//!
//! Sibling to [`crate::media::tag::MediaTag`]: collections are an
//! exclusive location ("/products/2026/launch/"), tags are inclusive
//! labels. The two are independent — Media has at most one collection
//! FK and any number of tag links.
//!
//! Soft-deleted via `deleted_at`. `slug` is unique and path-friendly.
//! A managed `#[derive(Model)]`, so its schema ships as a system
//! migration.
//!
//! [`Media`]: crate::media::Media

use crate::sql::Auto;

/// One folder. Cheap to clone.
#[derive(crate::Model, Debug, Clone)]
// `permissions` so `auto_create_permissions` seeds
// `rustango_media_collections.{add,change,delete,view}`, the codenames
// `router::MediaPerms` checks. Not a column, so no migration.
#[rustango(table = "rustango_media_collections", permissions)]
pub struct MediaCollection {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 255)]
    pub name: String,
    /// Path-friendly id, unique across the table.
    #[rustango(max_length = 255, unique)]
    pub slug: String,
    #[rustango(index)]
    pub parent_id: Option<i64>,
    #[rustango(default = "")]
    pub description: String,
    /// Set on INSERT via the per-dialect `DEFAULT NOW()`.
    #[rustango(auto_now_add)]
    pub created_at: Auto<chrono::DateTime<chrono::Utc>>,
    pub deleted_at: Option<chrono::DateTime<chrono::Utc>>,
}
