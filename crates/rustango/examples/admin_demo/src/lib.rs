//! Models for the admin guide (docs/admin.md). The `admin(...)` block on
//! `Post` exercises the configurable surface of the auto-admin; `Comment` is
//! shown as an inline on the post edit page.

use chrono::{DateTime, TimeZone, Utc};
use rustango::sql::sqlx::{self, PgPool};
use rustango::{Auto, Model};

#[derive(Model, Clone, Debug)]
#[rustango(table = "authors", display = "name")]
pub struct Author {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 120)]
    pub name: String,
    #[rustango(max_length = 200)]
    pub email: String,
}

#[derive(Model, Clone, Debug)]
#[rustango(table = "tags", display = "name")]
pub struct Tag {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 60)]
    pub name: String,
    #[rustango(max_length = 60)]
    pub slug: String,
}

/// The showcase model. Almost every `admin(...)` knob is set here so the
/// generated list + edit pages demonstrate the feature set.
#[derive(Model, Clone, Debug)]
#[rustango(
    table = "posts",
    display = "title",
    admin(
        list_display       = "id, title, author_id, status, view_count, published_at",
        list_display_links = "id, title",
        list_filter        = "status, author_id",
        search_fields      = "title, body",
        search_help_text   = "Search posts by title or body",
        ordering           = "-published_at",
        list_per_page      = 10,
        date_hierarchy     = "published_at",
        fieldsets          = "Content: title, body, status | Publishing: author_id, published_at, view_count",
        actions            = "publish, archive",
    ),
    audit(track = "title, body, status"),
)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    #[rustango(max_length = 200)]
    pub slug: String,
    pub body: String,
    #[rustango(max_length = 20, default = "'draft'")]
    pub status: String, // draft | published | archived
    pub author_id: i64,
    pub view_count: i64,
    pub published_at: DateTime<Utc>,
}

#[derive(Model, Clone, Debug)]
#[rustango(
    table = "comments",
    display = "author_name",
    admin(list_display = "id, post_id, author_name, created_at")
)]
pub struct Comment {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub post_id: i64,
    #[rustango(max_length = 120)]
    pub author_name: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
}

// Show each post's comments as a read-only inline table on the post edit page.
rustango::register_admin_inline!(
    parent = "posts",
    child = "comments",
    fk = "post_id",
    kind = rustango::admin::inlines::InlineKind::Tabular,
    label = "Comments",
    fields = &["author_name", "body", "created_at"],
);

/// Idempotent demo seed: 3 authors, 5 tags, 15 posts (varied status + dates)
/// and comments, so the admin has browsable content. Shared by the binary's
/// boot-time `.seed()` hook and the integration test.
pub async fn seed(pool: &PgPool) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM posts").fetch_one(pool).await?;
    if count > 0 {
        return Ok(());
    }

    let mut author_ids = Vec::new();
    for name in ["Ada Lovelace", "Alan Turing", "Grace Hopper"] {
        let mut a = Author {
            id: Auto::default(),
            name: name.into(),
            email: format!("{}@example.com", name.split(' ').next().unwrap().to_lowercase()),
        };
        a.save(pool).await?;
        author_ids.push(a.id.get().copied().unwrap());
    }

    for t in ["rust", "web", "orm", "admin", "async"] {
        let mut tag = Tag { id: Auto::default(), name: t.into(), slug: t.into() };
        tag.save(pool).await?;
    }

    let statuses = ["draft", "published", "archived"];
    for i in 1..=15i64 {
        let month = (((i - 1) % 6) + 1) as u32;
        let day = ((i % 27) + 1) as u32;
        let mut p = Post {
            id: Auto::default(),
            title: format!("Post {i}: exploring rustango"),
            slug: format!("post-{i}"),
            body: format!("This is the body of post {i}. It exists to fill the admin with browsable content."),
            status: statuses[(i as usize) % 3].into(),
            author_id: author_ids[(i as usize) % author_ids.len()],
            view_count: (i * 37) % 500,
            published_at: Utc.with_ymd_and_hms(2025, month, day, 12, 0, 0).unwrap(),
        };
        p.save(pool).await?;
        let pid = p.id.get().copied().unwrap();

        for c in 1..=3u32 {
            let mut cm = Comment {
                id: Auto::default(),
                post_id: pid,
                author_name: format!("Commenter {c}"),
                body: format!("Comment {c} on post {i}."),
                created_at: Utc.with_ymd_and_hms(2025, month, day, 13, c, 0).unwrap(),
            };
            cm.save(pool).await?;
        }
    }
    Ok(())
}
