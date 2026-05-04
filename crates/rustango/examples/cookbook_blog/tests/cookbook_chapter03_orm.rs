//! Cookbook Chapter 3 — the ORM, exercised live against docker PG.
//!
//! Each test is a recipe in COOKBOOK Chapter 3. Reuses the Chapter 2
//! schema (Author, Post, Tag) so the data feels real (a blog).
//!
//! Run: `DATABASE_URL=... cargo test --test cookbook_chapter03_orm -- --test-threads=1`

use cookbook_blog::apps::blog::models::*;
use rustango::core::Op;
use rustango::sql::{sqlx, Auto, Fetcher};
use rustango::Model;

fn url() -> Option<String> {
    std::env::var("DATABASE_URL").ok()
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = url()?;
    Some(sqlx::PgPool::connect(&url).await.expect("connect"))
}

async fn fresh_blog(pool: &sqlx::PgPool) -> i64 {
    for ddl in [
        "DROP TABLE IF EXISTS cookbook_post CASCADE",
        "DROP TABLE IF EXISTS cookbook_author CASCADE",
    ] {
        sqlx::query(ddl).execute(pool).await.expect(ddl);
    }
    sqlx::query(
        r#"CREATE TABLE cookbook_author (
            id BIGSERIAL PRIMARY KEY,
            name VARCHAR(80) NOT NULL,
            email VARCHAR(200) NOT NULL UNIQUE,
            bio VARCHAR(500) NULL,
            joined_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )"#,
    ).execute(pool).await.unwrap();
    sqlx::query(
        r#"CREATE TABLE cookbook_post (
            id BIGSERIAL PRIMARY KEY,
            title VARCHAR(200) NOT NULL,
            slug VARCHAR(200) NOT NULL UNIQUE,
            body TEXT NOT NULL,
            author_id BIGINT NOT NULL REFERENCES cookbook_author(id),
            published BOOLEAN NOT NULL DEFAULT false,
            view_count BIGINT NOT NULL,
            metadata JSONB NOT NULL,
            published_at TIMESTAMPTZ NULL
        )"#,
    ).execute(pool).await.unwrap();

    let mut a = Author {
        id: Auto::Unset,
        name: "ada".into(), email: "ada@example.com".into(),
        bio: None, joined_at: Auto::Unset,
    };
    a.save(pool).await.unwrap();
    let author_id = match a.id { Auto::Set(v) => v, _ => unreachable!() };

    // 5 posts: 3 published, 2 draft. Mix of view counts for filtering tests.
    for (i, (slug, title, published, views)) in [
        ("rust-orm", "Rust ORM", true, 100),
        ("django-shape", "Django shape", true, 250),
        ("draft-1",     "Draft one",    false, 0),
        ("axum-101",    "Axum 101",     true, 80),
        ("draft-2",     "Draft two",    false, 0),
    ].iter().enumerate() {
        let mut p = Post {
            id: Auto::Unset,
            title: (*title).into(),
            slug: (*slug).into(),
            body: format!("body {i}"),
            author_id,
            published: *published,
            view_count: *views,
            metadata: serde_json::json!({"i": i}),
            published_at: published.then(chrono::Utc::now),
        };
        p.save(pool).await.unwrap();
    }
    author_id
}

// §3.31 — filter + fetch returns matching rows.
#[tokio::test]
async fn filter_eq_fetch_returns_matching_rows() {
    let Some(pool) = pool().await else { return };
    let _ = fresh_blog(&pool).await;
    let published: Vec<Post> = Post::objects()
        .filter("published", Op::Eq, true)
        .fetch(&pool).await.unwrap();
    assert_eq!(published.len(), 3, "3 of 5 posts are published");
    for p in &published { assert!(p.published, "filter must keep only published"); }
}

// §3.34 — Op::Gt / Op::Lt on i64 column.
#[tokio::test]
async fn filter_with_gt_lt_op() {
    let Some(pool) = pool().await else { return };
    let _ = fresh_blog(&pool).await;
    let popular: Vec<Post> = Post::objects()
        // Bind as i64 — the column is BIGINT, and `90` would otherwise
        // infer as i32 and trip rustango's TypeMismatch guard.
        .filter("view_count", Op::Gt, 90i64)
        .fetch(&pool).await.unwrap();
    assert_eq!(popular.len(), 2, "view_count>90: rust-orm(100) + django-shape(250)");
    let titles: Vec<&str> = popular.iter().map(|p| p.title.as_str()).collect();
    assert!(titles.contains(&"Rust ORM"));
    assert!(titles.contains(&"Django shape"));
}

// §3.34 — Op::ILike for case-insensitive substring search.
#[tokio::test]
async fn filter_with_ilike_case_insensitive() {
    let Some(pool) = pool().await else { return };
    let _ = fresh_blog(&pool).await;
    let drafts: Vec<Post> = Post::objects()
        .filter("title", Op::ILike, "%draft%")
        .fetch(&pool).await.unwrap();
    assert_eq!(drafts.len(), 2);
}

// §3.34 — Op::In with a list value.
#[tokio::test]
async fn filter_with_in_list() {
    use rustango::core::SqlValue;
    let Some(pool) = pool().await else { return };
    let _ = fresh_blog(&pool).await;
    let picks: Vec<Post> = Post::objects()
        .filter("slug", Op::In, SqlValue::List(vec![
            SqlValue::String("rust-orm".into()),
            SqlValue::String("axum-101".into()),
        ]))
        .fetch(&pool).await.unwrap();
    assert_eq!(picks.len(), 2);
}

// §3.34 — Op::Between for range checks.
#[tokio::test]
async fn filter_with_between_range() {
    use rustango::core::SqlValue;
    let Some(pool) = pool().await else { return };
    let _ = fresh_blog(&pool).await;
    let mid: Vec<Post> = Post::objects()
        .filter("view_count", Op::Between, SqlValue::List(vec![
            SqlValue::I64(50), SqlValue::I64(150),
        ]))
        .fetch(&pool).await.unwrap();
    assert_eq!(mid.len(), 2, "axum-101(80) + rust-orm(100) within [50, 150]");
}

// §3.34 — Op::IsNull for NOT NULL filtering.
#[tokio::test]
async fn filter_with_is_null_unpublished() {
    use rustango::core::SqlValue;
    let Some(pool) = pool().await else { return };
    let _ = fresh_blog(&pool).await;
    let drafts: Vec<Post> = Post::objects()
        .filter("published_at", Op::IsNull, SqlValue::Bool(true))
        .fetch(&pool).await.unwrap();
    assert_eq!(drafts.len(), 2, "draft-1 + draft-2 have NULL published_at");
}

// §3.35 — order_by ascending + descending.
#[tokio::test]
async fn order_by_view_count_desc() {
    let Some(pool) = pool().await else { return };
    let _ = fresh_blog(&pool).await;
    let by_views: Vec<Post> = Post::objects()
        .filter("published", Op::Eq, true)
        // QuerySet::order_by uses (column, desc): true = DESC, false = ASC.
        .order_by(&[("view_count", true)])
        .fetch(&pool).await.unwrap();
    assert_eq!(by_views[0].slug, "django-shape", "250 views first");
    assert_eq!(by_views[1].slug, "rust-orm", "100 views second");
    assert_eq!(by_views[2].slug, "axum-101", "80 views third");
}

// §3.36 — limit + offset for pagination.
#[tokio::test]
async fn limit_offset_paginates() {
    let Some(pool) = pool().await else { return };
    let _ = fresh_blog(&pool).await;
    let page2: Vec<Post> = Post::objects()
        // ASC by id, skip 2, take 2 → posts 3 and 4 (draft-1, axum-101).
        .order_by(&[("id", false)])
        .limit(2)
        .offset(2)
        .fetch(&pool).await.unwrap();
    assert_eq!(page2.len(), 2);
    assert_eq!(page2[0].slug, "draft-1");
    assert_eq!(page2[1].slug, "axum-101");
}

// §3.37 — aggregation: count, sum.
#[tokio::test]
async fn aggregate_count_and_sum() {
    use rustango::core::{AggregateExpr, SqlValue};
    use rustango::sql::fetch_aggregate;
    let Some(pool) = pool().await else { return };
    let _ = fresh_blog(&pool).await;

    let q = Post::objects()
        .filter("published", Op::Eq, true)
        .aggregate()
        .annotate("total", AggregateExpr::Count(None))
        .annotate("views", AggregateExpr::Sum("view_count"))
        .compile()
        .unwrap();
    let rows = fetch_aggregate(&q, &pool).await.unwrap();
    assert_eq!(rows.len(), 1, "no GROUP BY → one summary row");
    let row = &rows[0];
    match row.get("total") {
        Some(SqlValue::I64(n)) => assert_eq!(*n, 3),
        other => panic!("expected total: Int(3), got {other:?}"),
    }
    match row.get("views") {
        Some(SqlValue::I64(n)) => assert_eq!(*n, 100 + 250 + 80),
        other => panic!("expected views: Int(430), got {other:?}"),
    }
}

// §3.42 — save() inserts new row, then UPDATE on the same instance.
#[tokio::test]
async fn save_inserts_then_updates_in_place() {
    let Some(pool) = pool().await else { return };
    let author_id = fresh_blog(&pool).await;

    let mut p = Post {
        id: Auto::Unset,
        title: "save-flow".into(), slug: "save-flow".into(),
        body: "v1".into(), author_id,
        published: false, view_count: 0,
        metadata: serde_json::json!({}),
        published_at: None,
    };
    p.save(&pool).await.unwrap();
    let id = match p.id { Auto::Set(v) => v, _ => unreachable!() };

    p.body = "v2".into();
    p.published = true;
    p.save(&pool).await.unwrap();

    let back: Vec<Post> = Post::objects()
        .filter("id", Op::Eq, id)
        .fetch(&pool).await.unwrap();
    assert_eq!(back[0].body, "v2");
    assert!(back[0].published);
}

// §3.46 — raw SQL escape via sqlx::query_as for use cases the QuerySet doesn't cover.
#[tokio::test]
async fn raw_sql_escape_via_sqlx() {
    let Some(pool) = pool().await else { return };
    let _ = fresh_blog(&pool).await;

    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM cookbook_post WHERE view_count > $1"
    ).bind(100i64).fetch_one(&pool).await.unwrap();
    assert_eq!(count, 1, "only django-shape has view_count > 100");
}

// §3.47 — manual transaction via sqlx for atomicity.
#[tokio::test]
async fn manual_transaction_rolls_back_on_error() {
    let Some(pool) = pool().await else { return };
    let author_id = fresh_blog(&pool).await;

    let count_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cookbook_post")
        .fetch_one(&pool).await.unwrap();

    // Open a tx, insert one row, then deliberately violate UNIQUE on slug.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO cookbook_post (title, slug, body, author_id, published, view_count, metadata) VALUES ($1,$2,$3,$4,$5,$6,$7)")
        .bind("ok").bind("tx-ok").bind("b").bind(author_id).bind(false).bind(0i64).bind(serde_json::json!({}))
        .execute(&mut *tx).await.unwrap();
    let dup = sqlx::query("INSERT INTO cookbook_post (title, slug, body, author_id, published, view_count, metadata) VALUES ($1,$2,$3,$4,$5,$6,$7)")
        .bind("dup").bind("rust-orm").bind("b").bind(author_id).bind(false).bind(0i64).bind(serde_json::json!({}))
        .execute(&mut *tx).await;
    assert!(dup.is_err(), "duplicate slug must violate UNIQUE inside the tx");
    tx.rollback().await.unwrap();

    let count_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cookbook_post")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(count_after, count_before, "rollback must undo the in-tx insert");
}

// §3.48 — JSON operators via raw SQL on the JSONB column.
#[tokio::test]
async fn json_operator_on_jsonb_column() {
    let Some(pool) = pool().await else { return };
    let _ = fresh_blog(&pool).await;

    // metadata = {"i": <index>}; index 1 == django-shape.
    let row: (String,) = sqlx::query_as(
        "SELECT slug FROM cookbook_post WHERE metadata @> '{\"i\": 1}'::jsonb"
    ).fetch_one(&pool).await.unwrap();
    assert_eq!(row.0, "django-shape");
}
