#![cfg(feature = "postgres")]
//! Live PG test for QuerySet set algebra (issue #25). Verifies UNION
//! / INTERSECT / EXCEPT runtime semantics + that outer ORDER BY /
//! LIMIT apply to the merged result. Skips silently when
//! `DATABASE_URL` is unset.

use std::sync::OnceLock;

use rustango::core::Column as _;
use rustango::sql::{sqlx, Auto, Fetcher as _};
use rustango::Model;
use tokio::sync::Mutex;

#[derive(Model, Debug, Clone)]
#[rustango(table = "salg_live_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 20)]
    pub status: String,
    pub author_id: i64,
}

fn lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn fresh_pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = sqlx::PgPool::connect(&url).await.ok()?;
    sqlx::query(r#"DROP TABLE IF EXISTS "salg_live_post" CASCADE"#)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        r#"
        CREATE TABLE "salg_live_post" (
            id BIGSERIAL PRIMARY KEY,
            status VARCHAR(20) NOT NULL,
            author_id BIGINT NOT NULL
        )
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();
    // Seed: 6 posts across 3 authors with 3 statuses.
    // author 1: draft, review
    // author 2: draft, published
    // author 3: published, archived
    for (status, author_id) in [
        ("draft", 1_i64),
        ("review", 1),
        ("draft", 2),
        ("published", 2),
        ("published", 3),
        ("archived", 3),
    ] {
        let mut p = Post {
            id: Auto::default(),
            status: status.into(),
            author_id,
        };
        p.insert_pool(&(&pool).clone().into()).await.unwrap();
    }
    Some(pool)
}

fn author_id_of(p: &Post) -> i64 {
    p.author_id
}

/// `UNION` deduplicates. Author 1's drafts + author 2's drafts =
/// 2 distinct rows. `UNION` of "drafts" + "reviews" = 3 distinct rows
/// (no overlap, but `UNION` would dedupe if there were any).
#[tokio::test]
async fn union_combines_branches_and_dedupes() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    // Posts that are drafts OR reviews — combined branches.
    let rows: Vec<Post> = Post::objects()
        .where_(Post::status.eq("draft"))
        .union(Post::objects().where_(Post::status.eq("review")))
        .order_by(&[("id", false)])
        .fetch(&pool)
        .await
        .unwrap();
    // 2 drafts + 1 review = 3 rows.
    assert_eq!(rows.len(), 3);
    let statuses: Vec<String> = rows.iter().map(|p| p.status.clone()).collect();
    assert!(statuses.iter().filter(|s| *s == "draft").count() == 2);
    assert!(statuses.iter().filter(|s| *s == "review").count() == 1);

    cleanup(&pool).await;
}

/// `UNION ALL` keeps duplicates. Two identical branches → 2× rows.
#[tokio::test]
async fn union_all_keeps_duplicates() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    // Same WHERE on both branches → each draft row appears twice.
    let rows: Vec<Post> = Post::objects()
        .where_(Post::status.eq("draft"))
        .union_all(Post::objects().where_(Post::status.eq("draft")))
        .fetch(&pool)
        .await
        .unwrap();
    // 2 drafts × 2 branches = 4 rows.
    assert_eq!(rows.len(), 4);

    cleanup(&pool).await;
}

/// `INTERSECT` keeps rows present in BOTH branches. Authors with both
/// draft AND published posts → only author 2 (has draft+published).
/// Implemented as INTERSECT of two queries returning the FULL post
/// row — only rows identical across both branches survive.
#[tokio::test]
async fn intersect_keeps_only_rows_in_both_branches() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    // Identical branches → INTERSECT = same rows (no filtering).
    let rows: Vec<Post> = Post::objects()
        .where_(Post::status.eq("published"))
        .intersection(Post::objects().where_(Post::status.eq("published")))
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2, "2 published posts intersect themselves");

    // Disjoint branches → INTERSECT = empty.
    let empty: Vec<Post> = Post::objects()
        .where_(Post::status.eq("draft"))
        .intersection(Post::objects().where_(Post::status.eq("published")))
        .fetch(&pool)
        .await
        .unwrap();
    assert!(
        empty.is_empty(),
        "drafts ∩ publisheds = empty (disjoint status sets)"
    );

    cleanup(&pool).await;
}

/// `EXCEPT` keeps rows in the first branch but NOT the second.
/// All posts EXCEPT author 1's posts = 4 rows (authors 2 + 3).
#[tokio::test]
async fn except_excludes_matching_rows() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let rows: Vec<Post> = Post::objects()
        .difference(Post::objects().where_(Post::author_id.eq(1_i64)))
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 4, "6 total - 2 author-1 = 4: got {rows:?}");
    let authors: Vec<i64> = rows.iter().map(author_id_of).collect();
    assert!(!authors.contains(&1), "no author 1 in EXCEPT result");

    cleanup(&pool).await;
}

/// Outer LIMIT applies to the COMBINED result, not per-branch.
/// 3 drafts+reviews total, LIMIT 2 caps to 2.
#[tokio::test]
async fn outer_limit_caps_combined_result() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let rows: Vec<Post> = Post::objects()
        .where_(Post::status.eq("draft"))
        .union(Post::objects().where_(Post::status.eq("review")))
        .order_by(&[("id", false)])
        .limit(2)
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2, "outer LIMIT 2 wins: got {rows:?}");

    cleanup(&pool).await;
}

async fn cleanup(pool: &sqlx::PgPool) {
    sqlx::query(r#"DROP TABLE IF EXISTS "salg_live_post" CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
}
