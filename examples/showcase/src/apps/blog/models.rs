//! Blog domain models. Each attribute exercises one rustango model
//! feature so the E2E playwright suite can assert it round-trips
//! through the API.

use rustango::sql::Auto;
use rustango::Model;

/// One blog post — minimal model exercising `Auto<i64>` primary key,
/// `max_length` (DDL VARCHAR), `default` (DDL DEFAULT clause), and
/// `auto_now_add` (DB-side `NOW()`-like default).
#[derive(Model, Debug, Clone)]
#[rustango(table = "showcase_blog_post", display = "title")]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(max_length = 200)]
    pub title: String,

    /// Long-form body. `Option<String>` → nullable TEXT column.
    pub body: Option<String>,

    /// Visibility flag — defaults to `false` (drafts).
    #[rustango(default = "false")]
    pub published: bool,

    /// Set by the DB on insert. `Auto<DateTime<Utc>>` with
    /// `auto_now_add` round-trips through every backend.
    #[rustango(auto_now_add)]
    pub created_at: Auto<chrono::DateTime<chrono::Utc>>,
}
