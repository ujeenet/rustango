#![cfg(all(feature = "sqlite", feature = "signals"))]
//! Live SQLite test for polymorphic many-to-many — Eloquent
//! `morphToMany` (issue #818). Two unrelated models (`Post`, `Video`)
//! share one `taggables` pivot + one `Tag` set, isolated by the
//! ContentType discriminator. Exercises add / remove / set / clear /
//! contains / all + `m2m_changed`.

use std::sync::Arc;
use std::sync::OnceLock;

use rustango::contenttypes;
use rustango::signals::m2m::{clear_all, connect_m2m_changed, M2mAction, M2mChangedContext};
use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;
use tokio::sync::Mutex;

/// One suite-wide lock — every test mutates two process-global registries
/// (the contenttypes cache + the m2m_changed signal registry).
fn suite_lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "gm2m_post", app = "gm2m")]
#[rustango(generic_m2m(
    name = "tags",
    through = "gm2m_taggables",
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
#[rustango(table = "gm2m_video", app = "gm2m")]
#[rustango(generic_m2m(
    name = "tags",
    through = "gm2m_taggables",
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

#[derive(Model, Debug, Clone)]
#[rustango(table = "gm2m_tag", app = "gm2m")]
#[allow(dead_code)]
pub struct Tag {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 40)]
    pub label: String,
}

async fn fresh_pool() -> Pool {
    let sq = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    let pool = Pool::Sqlite(sq);
    contenttypes::ensure_seeded(&pool)
        .await
        .expect("ensure_seeded");
    // Cache may hold (app, model) → id from a sibling test's pool; this
    // fresh in-memory DB re-seeds with its own ids, so clear it.
    contenttypes::clear_cache();
    let Pool::Sqlite(ref s) = pool else {
        unreachable!()
    };
    for ddl in [
        "CREATE TABLE gm2m_post (id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT NOT NULL)",
        "CREATE TABLE gm2m_video (id INTEGER PRIMARY KEY AUTOINCREMENT, url TEXT NOT NULL)",
        "CREATE TABLE gm2m_tag (id INTEGER PRIMARY KEY AUTOINCREMENT, label TEXT NOT NULL)",
        "CREATE TABLE gm2m_taggables (\
            taggable_id   INTEGER NOT NULL, \
            taggable_type INTEGER NOT NULL, \
            tag_id        INTEGER NOT NULL, \
            UNIQUE(taggable_id, taggable_type, tag_id))",
    ] {
        sqlx::query(ddl).execute(s).await.unwrap();
    }
    pool
}

#[tokio::test]
async fn add_all_contains_remove() {
    let _g = suite_lock().lock().await;
    clear_all();
    let pool = fresh_pool().await;

    let post = Post {
        id: Auto::from(1),
        title: "Hello".into(),
    };
    post.tags_m2m().add(10, &pool).await.unwrap();
    post.tags_m2m().add(20, &pool).await.unwrap();
    // idempotent
    post.tags_m2m().add(10, &pool).await.unwrap();

    let mut tags = post.tags_m2m().all(&pool).await.unwrap();
    tags.sort_unstable();
    assert_eq!(tags, vec![10, 20]);
    assert!(post.tags_m2m().contains(10, &pool).await.unwrap());
    assert!(!post.tags_m2m().contains(99, &pool).await.unwrap());

    post.tags_m2m().remove(10, &pool).await.unwrap();
    assert_eq!(post.tags_m2m().all(&pool).await.unwrap(), vec![20]);
}

#[tokio::test]
async fn set_then_clear() {
    let _g = suite_lock().lock().await;
    clear_all();
    let pool = fresh_pool().await;

    let post = Post {
        id: Auto::from(7),
        title: "Set".into(),
    };
    post.tags_m2m().add(1, &pool).await.unwrap();
    post.tags_m2m().set(&[2, 3, 4], &pool).await.unwrap();
    let mut tags = post.tags_m2m().all(&pool).await.unwrap();
    tags.sort_unstable();
    assert_eq!(tags, vec![2, 3, 4]);

    post.tags_m2m().clear(&pool).await.unwrap();
    assert!(post.tags_m2m().all(&pool).await.unwrap().is_empty());
}

#[tokio::test]
async fn two_unrelated_models_share_pivot_isolated_by_content_type() {
    let _g = suite_lock().lock().await;
    clear_all();
    let pool = fresh_pool().await;

    // Same PK (1) on both models — only the ContentType discriminator
    // keeps their tag sets apart in the shared `gm2m_taggables` pivot.
    let post = Post {
        id: Auto::from(1),
        title: "P".into(),
    };
    let video = Video {
        id: Auto::from(1),
        url: "v".into(),
    };

    post.tags_m2m().set(&[10, 11], &pool).await.unwrap();
    video.tags_m2m().set(&[20, 21], &pool).await.unwrap();

    let mut post_tags = post.tags_m2m().all(&pool).await.unwrap();
    post_tags.sort_unstable();
    let mut video_tags = video.tags_m2m().all(&pool).await.unwrap();
    video_tags.sort_unstable();

    assert_eq!(post_tags, vec![10, 11], "post tags leaked video's");
    assert_eq!(video_tags, vec![20, 21], "video tags leaked post's");
    // Cross-checks: post doesn't see video's tags and vice-versa.
    assert!(!post.tags_m2m().contains(20, &pool).await.unwrap());
    assert!(!video.tags_m2m().contains(10, &pool).await.unwrap());
}

#[tokio::test]
async fn m2m_changed_fires_on_add() {
    let _g = suite_lock().lock().await;
    clear_all();

    let captured: Arc<Mutex<Vec<M2mChangedContext>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = captured.clone();
    connect_m2m_changed(move |ctx| {
        let sink = sink.clone();
        async move {
            sink.lock().await.push(ctx);
        }
    });

    let pool = fresh_pool().await;
    let post = Post {
        id: Auto::from(5),
        title: "Sig".into(),
    };
    post.tags_m2m().add(42, &pool).await.unwrap();

    let events = captured.lock().await;
    assert_eq!(events.len(), 1, "expected one m2m_changed event");
    assert!(matches!(events[0].action, M2mAction::Add));
    assert_eq!(events[0].through, "gm2m_taggables");
    assert_eq!(events[0].dst_pks, vec![42]);
}
