//! Live test for slice 9.0d — `Post::objects().select_related("author").fetch_on()`
//! emits ONE SQL query with a LEFT JOIN and decodes both the post and
//! the author into `Post { author: ForeignKey::Loaded { ... } }`.
//!
//! Skipped silently when `DATABASE_URL` is unset.

use std::sync::OnceLock;

use rustango::core::Column as _;
use rustango::sql::{sqlx, Auto, ForeignKey};
use rustango::Model;
use tokio::sync::Mutex;

#[derive(Model, Debug, Clone)]
#[rustango(table = "rustango_sr_author", display = "name")]
#[allow(dead_code)]
pub struct Author {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub name: String,
    #[rustango(max_length = 200)]
    pub bio: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "rustango_sr_post", display = "title")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    pub author: ForeignKey<Author>,
}

fn lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    sqlx::PgPool::connect(&url).await.ok()
}

async fn setup(pool: &sqlx::PgPool) {
    sqlx::query(r#"DROP TABLE IF EXISTS "rustango_sr_post" CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(r#"DROP TABLE IF EXISTS "rustango_sr_author" CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        r#"CREATE TABLE "rustango_sr_author" (
            id BIGSERIAL PRIMARY KEY,
            name VARCHAR(80) NOT NULL,
            bio VARCHAR(200) NOT NULL
        )"#,
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        r#"CREATE TABLE "rustango_sr_post" (
            id BIGSERIAL PRIMARY KEY,
            title VARCHAR(200) NOT NULL,
            author BIGINT NOT NULL REFERENCES "rustango_sr_author"(id)
        )"#,
    )
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn select_related_loads_fk_target_in_one_query() {
    let _g = lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    setup(&pool).await;

    // Seed: 2 authors + 3 posts (Ada has 2, Grace has 1).
    let mut ada = Author {
        id: Auto::default(),
        name: "Ada".into(),
        bio: "Pioneer".into(),
    };
    ada.save(&pool).await.unwrap();
    let ada_pk = ada.id.get().copied().unwrap();
    let mut grace = Author {
        id: Auto::default(),
        name: "Grace".into(),
        bio: "Compiler inventor".into(),
    };
    grace.save(&pool).await.unwrap();
    let grace_pk = grace.id.get().copied().unwrap();

    for (title, author_pk) in [
        ("Analytical engine", ada_pk),
        ("Algorithms as poetry", ada_pk),
        ("Birth of the bug", grace_pk),
    ] {
        let mut p = Post {
            id: Auto::default(),
            title: title.into(),
            author: ForeignKey::unloaded(author_pk),
        };
        p.save(&pool).await.unwrap();
    }

    // Fetch with select_related: every post's `author` FK is Loaded
    // — no follow-up query.
    let posts: Vec<Post> = Post::objects()
        .select_related("author")
        .fetch_on(&pool)
        .await
        .unwrap();
    assert_eq!(posts.len(), 3);
    for p in &posts {
        assert!(
            p.author.is_loaded(),
            "post `{}` should have Loaded author; was Unloaded",
            p.title
        );
        let author = p.author.value().expect("just asserted Loaded");
        let expected_name = if p.author.pk() == ada_pk {
            "Ada"
        } else {
            "Grace"
        };
        assert_eq!(author.name, expected_name);
    }
}

#[tokio::test]
async fn select_related_unknown_field_is_a_compile_error_kind() {
    // Schema validation runs in `compile()`; using a non-FK field
    // should yield SelectRelatedInvalid. We test via the typed
    // `Post::title` (a string field, not FK).
    let _g = lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    setup(&pool).await;
    let result = Post::objects()
        .select_related("title")
        .fetch_on(&pool)
        .await;
    assert!(result.is_err(), "expected SelectRelatedInvalid err");
}

#[tokio::test]
async fn fetch_on_without_select_related_keeps_fast_path() {
    let _g = lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    setup(&pool).await;
    let mut a = Author {
        id: Auto::default(),
        name: "Solo".into(),
        bio: "Just here".into(),
    };
    a.save(&pool).await.unwrap();
    let posts: Vec<Post> = Post::objects()
        .where_(Post::title.eq("missing".to_owned()))
        .fetch_on(&pool)
        .await
        .unwrap();
    assert!(posts.is_empty());
}
