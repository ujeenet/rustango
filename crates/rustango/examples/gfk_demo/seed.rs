//! Idempotent seed for the gfk_demo example. Creates two posts, two
//! articles, and attaches tags + comments to each target — exercising
//! both the typed setter (`#240`) and reverse traversal.

use rustango::admin::AdminUser;
use rustango::contenttypes;
use rustango::sql::{Auto, FetcherPool as _, Pool};

use crate::models::{Article, Comment, Post, Tag};

pub async fn run(pool: &Pool) -> Result<(), Box<dyn std::error::Error>> {
    // Seed the ContentType catalog so the typed setter
    // (`Tag::set_target_for::<Post>`) can resolve the CT id.
    contenttypes::ensure_seeded(pool).await?;

    // #253 — seed the admin user used by the session-login form.
    // Credentials default to `admin / admin`; override via the
    // `RUSTANGO_DEMO_USER` / `RUSTANGO_DEMO_PASS` env vars.
    let existing_admin: Vec<AdminUser> = AdminUser::objects().fetch_pool(pool).await?;
    if existing_admin.is_empty() {
        let username = std::env::var("RUSTANGO_DEMO_USER").unwrap_or_else(|_| "admin".to_owned());
        let password = std::env::var("RUSTANGO_DEMO_PASS").unwrap_or_else(|_| "admin".to_owned());
        let mut admin =
            AdminUser::new_with_password(username, &password, /* superuser */ true)?;
        admin.save_pool(pool).await?;
        println!("→ seed: admin user created");
    }

    // Skip the rest if we've already populated.
    let existing_posts: Vec<Post> = Post::objects().fetch_pool(pool).await?;
    if !existing_posts.is_empty() {
        println!("→ seed: demo data already present, skipping");
        return Ok(());
    }

    let now = chrono::Utc::now();

    // Posts.
    let mut p1 = Post {
        id: Auto::Unset,
        title: "Welcome to the GFK demo".into(),
        body: "This post has tags + comments attached via a generic FK.".into(),
        published_at: now,
    };
    p1.save_pool(pool).await?;
    let p1_pk = *p1.id.get().unwrap();

    let mut p2 = Post {
        id: Auto::Unset,
        title: "Polymorphic relations in practice".into(),
        body: "Same Tag table, different parent type.".into(),
        published_at: now,
    };
    p2.save_pool(pool).await?;
    let p2_pk = *p2.id.get().unwrap();

    // Articles.
    let mut a1 = Article {
        id: Auto::Unset,
        title: "An article that shares the tag table".into(),
        body: "Articles use the same Tag model as Posts.".into(),
        published_at: now,
    };
    a1.save_pool(pool).await?;
    let a1_pk = *a1.id.get().unwrap();

    let mut a2 = Article {
        id: Auto::Unset,
        title: "Generic inlines also work on Articles".into(),
        body: "Click 'Edit' to add tags + comments via inline forms.".into(),
        published_at: now,
    };
    a2.save_pool(pool).await?;
    let a2_pk = *a2.id.get().unwrap();

    // Tags + Comments attached via the typed setter. The macro emits
    // `set_target_for::<T>` from the `generic_fk(name = "target")` arg.
    for (post_pk, names) in [
        (p1_pk, ["rust", "django-parity", "demo"].as_slice()),
        (p2_pk, ["polymorphic", "tags"].as_slice()),
    ] {
        for name in names {
            let mut t = Tag {
                id: Auto::Unset,
                content_type_id: 0,
                object_pk: 0,
                name: (*name).to_owned(),
            };
            t.set_target_for::<Post>(pool, post_pk).await?;
            t.save_pool(pool).await?;
        }
        let mut c = Comment {
            id: Auto::Unset,
            content_type_id: 0,
            object_pk: 0,
            body: format!("First comment on post #{post_pk}"),
            created_at: now,
        };
        c.set_target_for::<Post>(pool, post_pk).await?;
        c.save_pool(pool).await?;
    }

    for (article_pk, names) in [
        (a1_pk, ["articles", "tagging", "cross-model"].as_slice()),
        (a2_pk, ["inlines", "generic-fk"].as_slice()),
    ] {
        for name in names {
            let mut t = Tag {
                id: Auto::Unset,
                content_type_id: 0,
                object_pk: 0,
                name: (*name).to_owned(),
            };
            t.set_target_for::<Article>(pool, article_pk).await?;
            t.save_pool(pool).await?;
        }
        let mut c = Comment {
            id: Auto::Unset,
            content_type_id: 0,
            object_pk: 0,
            body: format!("First comment on article #{article_pk}"),
            created_at: now,
        };
        c.set_target_for::<Article>(pool, article_pk).await?;
        c.save_pool(pool).await?;
    }

    println!(
        "→ seed: 2 posts + 2 articles + {} tags + 4 comments inserted",
        5 + 5
    );
    Ok(())
}
