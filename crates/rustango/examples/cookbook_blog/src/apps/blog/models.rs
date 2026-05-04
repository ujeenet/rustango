//! Blog domain models — the cookbook's exercise of the model surface.
//!
//! Each attribute below is referenced by a recipe in COOKBOOK Chapter 2.
//! Any rustango behaviour an app would actually rely on must show up
//! here AND have a test in `tests/cookbook_chapter02_models.rs`.

use rustango::sql::Auto;
use rustango::Model;
use uuid::Uuid;

/// Chapter 2 §2.11 (basics) + §2.12 (Auto<i64>) + §2.13 (Option) +
/// §2.14 (default) + §2.15 (unique) + §2.17 (max_length) +
/// §2.29 (auto_now_add).
#[derive(Model, Debug, Clone)]
#[rustango(table = "cookbook_author", display = "name")]
pub struct Author {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub name: String,
    /// Demonstrates `unique`: the database rejects a duplicate insert.
    #[rustango(unique, max_length = 200)]
    pub email: String,
    /// Demonstrates `Option<T>` → nullable column.
    #[rustango(max_length = 500)]
    pub bio: Option<String>,
    /// Demonstrates `default = "..."` raw SQL fragment + `auto_now_add`.
    #[rustango(auto_now_add)]
    pub joined_at: Auto<chrono::DateTime<chrono::Utc>>,
}

/// Chapter 2 §2.16 (min/max → CHECK).
#[derive(Model, Debug, Clone)]
#[rustango(table = "cookbook_rating", display = "score")]
pub struct Rating {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    /// Demonstrates `min` + `max` → DDL CHECK constraint.
    #[rustango(min = 1, max = 5)]
    pub score: i64,
}

/// Chapter 2 §2.27 — `Auto<Uuid>` PK + `auto_uuid` mixin.
#[derive(Model, Debug, Clone)]
#[rustango(table = "cookbook_session")]
pub struct Session {
    /// Mixin sugar for `primary_key + auto + DEFAULT gen_random_uuid()`.
    #[rustango(auto_uuid)]
    pub id: Auto<Uuid>,
    #[rustango(max_length = 80)]
    pub user_token: String,
}

/// Chapter 2 §2.21 — one-to-one (O2O column with UNIQUE on the FK).
#[derive(Model, Debug, Clone)]
#[rustango(table = "cookbook_author_profile")]
pub struct AuthorProfile {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(o2o = "cookbook_author")]
    pub author_id: i64,
    pub avatar_url: String,
}

/// Chapter 2 §2.22 — many-to-many through an auto-created junction.
#[derive(Model, Debug, Clone)]
#[rustango(table = "cookbook_tag")]
pub struct Tag {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(unique, max_length = 40)]
    pub name: String,
}

/// Chapter 2 §2.30 — `#[rustango(soft_delete)]` marks a `deleted_at`
/// column the ORM treats as "alive when NULL". `delete()` becomes a
/// timestamp update; `objects()` filters by NULL by default.
#[derive(Model, Debug, Clone)]
#[rustango(table = "cookbook_archive_note")]
pub struct ArchiveNote {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub note: String,
    #[rustango(soft_delete)]
    pub deleted_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Chapter 2 §2.20 (basic FK) + §2.18 (field-level index) + §2.22
/// (M2M to Tag through `cookbook_post_tag`).
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "cookbook_post",
    display = "title",
    m2m(name = "tags", to = "cookbook_tag", through = "cookbook_post_tag", src = "post_id", dst = "tag_id"),
)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    #[rustango(unique, max_length = 200)]
    pub slug: String,
    pub body: String,
    #[rustango(fk = "cookbook_author", index)]
    pub author_id: i64,
    /// Plain bool default.
    #[rustango(default = "false")]
    pub published: bool,
    pub view_count: i64,
    /// Chapter 2 §2.26: `serde_json::Value` → JSONB column.
    pub metadata: serde_json::Value,
    /// Chapter 2 §2.28: `chrono::DateTime<Utc>` → TIMESTAMPTZ.
    pub published_at: Option<chrono::DateTime<chrono::Utc>>,
}
