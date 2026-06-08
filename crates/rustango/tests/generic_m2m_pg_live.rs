#![cfg(feature = "postgres")]
//! Live PostgreSQL round-trip for polymorphic M2M (`morphToMany`, issue
//! #818) — the same coverage as the SQLite live test against PG, so the
//! tri-dialect `INSERT … ON CONFLICT` / placeholder / quote-ident paths
//! are exercised on a second backend.
//!
//! Skips silently when `DATABASE_URL` is unset (runs in CI's
//! `postgres_test` job).

use std::sync::OnceLock;

use rustango::contenttypes;
use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;
use tokio::sync::Mutex;

fn suite_lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "gm2mpg_post", app = "gm2mpg")]
#[rustango(generic_m2m(
    name = "tags",
    through = "gm2mpg_taggables",
    pk_column = "taggable_id",
    ct_column = "taggable_type",
    related_column = "tag_id"
))]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 120)]
    pub title: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "gm2mpg_video", app = "gm2mpg")]
#[rustango(generic_m2m(
    name = "tags",
    through = "gm2mpg_taggables",
    pk_column = "taggable_id",
    ct_column = "taggable_type",
    related_column = "tag_id"
))]
#[allow(dead_code)]
pub struct Video {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 120)]
    pub url: String,
}

async fn pool() -> Option<Pool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool: Pool = sqlx::PgPool::connect(&url).await.ok()?.into();
    contenttypes::ensure_seeded(&pool).await.ok()?;
    contenttypes::clear_cache();
    let pg = pool.as_postgres().unwrap();
    for ddl in [
        r#"DROP TABLE IF EXISTS "gm2mpg_taggables" CASCADE"#,
        r#"CREATE TABLE IF NOT EXISTS "gm2mpg_post" ("id" BIGSERIAL PRIMARY KEY, "title" VARCHAR(120) NOT NULL)"#,
        r#"CREATE TABLE IF NOT EXISTS "gm2mpg_video" ("id" BIGSERIAL PRIMARY KEY, "url" VARCHAR(120) NOT NULL)"#,
        r#"CREATE TABLE "gm2mpg_taggables" (
            "taggable_id"   BIGINT NOT NULL,
            "taggable_type" BIGINT NOT NULL,
            "tag_id"        BIGINT NOT NULL,
            UNIQUE("taggable_id", "taggable_type", "tag_id"))"#,
    ] {
        sqlx::query(ddl).execute(pg).await.unwrap();
    }
    Some(pool)
}

#[tokio::test]
async fn polymorphic_m2m_round_trips_and_isolates_by_content_type_on_pg() {
    let _g = suite_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };

    let post = Post {
        id: Auto::from(1),
        title: "P".into(),
    };
    let video = Video {
        id: Auto::from(1),
        url: "v".into(),
    };

    // add + idempotent add + contains + remove
    post.tags_m2m().add(10, &pool).await.unwrap();
    post.tags_m2m().add(10, &pool).await.unwrap(); // ON CONFLICT DO NOTHING
    post.tags_m2m().add(11, &pool).await.unwrap();
    assert!(post.tags_m2m().contains(10, &pool).await.unwrap());
    post.tags_m2m().remove(11, &pool).await.unwrap();

    // set replaces; isolated from Video which shares PK=1 in the pivot.
    post.tags_m2m().set(&[10, 12], &pool).await.unwrap();
    video.tags_m2m().set(&[20], &pool).await.unwrap();

    let mut post_tags = post.tags_m2m().all(&pool).await.unwrap();
    post_tags.sort_unstable();
    assert_eq!(post_tags, vec![10, 12]);
    assert_eq!(video.tags_m2m().all(&pool).await.unwrap(), vec![20]);
    assert!(!post.tags_m2m().contains(20, &pool).await.unwrap());

    post.tags_m2m().clear(&pool).await.unwrap();
    assert!(post.tags_m2m().all(&pool).await.unwrap().is_empty());
    // Video untouched by Post's clear (CT-scoped).
    assert_eq!(video.tags_m2m().all(&pool).await.unwrap(), vec![20]);
}
