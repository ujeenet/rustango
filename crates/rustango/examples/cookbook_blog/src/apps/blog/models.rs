//! Blog domain models — the cookbook's exercise of the model surface.
//!
//! Each attribute below is referenced by a recipe in COOKBOOK Chapter 2.
//! Any rustango behaviour an app would actually rely on must show up
//! here AND have a test in `tests/cookbook_chapter02_models.rs`.

use rustango::sql::Auto;
use rustango::Model;

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

/// Chapter 2 §2.20 (basic FK) + §2.18 (field-level index).
#[derive(Model, Debug, Clone)]
#[rustango(table = "cookbook_post", display = "title")]
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
