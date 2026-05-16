#![cfg(feature = "postgres")]
//! Live PG tests for `QuerySet::values_dict` / `values_list` /
//! `values_list_flat` (issue #22). Verifies that pure projection
//! returns the right shape against a real database with mixed-type
//! columns. Skips silently when `DATABASE_URL` is unset.

use std::collections::HashMap;
use std::sync::OnceLock;

use rustango::core::{Column as _, SqlValue};
use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;
use tokio::sync::Mutex;

#[derive(Model, Debug, Clone)]
#[rustango(table = "v_post_live")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub title: String,
    pub view_count: i64,
    pub published: bool,
}

fn lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn fresh_pool() -> Option<Pool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pg = sqlx::PgPool::connect(&url).await.ok()?;
    sqlx::query(r#"DROP TABLE IF EXISTS "v_post_live" CASCADE"#)
        .execute(&pg)
        .await
        .unwrap();
    sqlx::query(
        r#"
        CREATE TABLE "v_post_live" (
            id BIGSERIAL PRIMARY KEY,
            title VARCHAR(64) NOT NULL,
            view_count BIGINT NOT NULL,
            published BOOLEAN NOT NULL
        )
        "#,
    )
    .execute(&pg)
    .await
    .unwrap();
    let pool = Pool::Postgres(pg);
    for (title, vc, pub_) in [
        ("Intro to Rust", 100_i64, true),
        ("Advanced Lifetimes", 50, true),
        ("Draft Post", 0, false),
        ("Performance Tips", 200, true),
    ] {
        let mut p = Post {
            id: Auto::default(),
            title: title.into(),
            view_count: vc,
            published: pub_,
        };
        p.insert_pool(&pool).await.unwrap();
    }
    Some(pool)
}

/// `.values_dict(&["id", "title"])` returns each row as a HashMap
/// keyed by column name. Only the listed cols come back.
#[tokio::test]
async fn values_dict_returns_hashmap_per_row() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let rows: Vec<HashMap<String, SqlValue>> = Post::objects()
        .where_(Post::published.eq(true))
        .order_by(&[("id", false)])
        .values_dict(&["id", "title"])
        .fetch(&pool)
        .await
        .unwrap();

    assert_eq!(rows.len(), 3, "three published posts: {rows:?}");
    for row in &rows {
        assert_eq!(row.len(), 2, "only id + title columns: {row:?}");
        assert!(row.contains_key("id"));
        assert!(row.contains_key("title"));
        // view_count + published were NOT requested — not present
        assert!(!row.contains_key("view_count"));
        assert!(!row.contains_key("published"));
    }
    // First row by ascending id is "Intro to Rust"
    match &rows[0]["title"] {
        SqlValue::String(s) => assert_eq!(s, "Intro to Rust"),
        other => panic!("expected String, got {other:?}"),
    }
}

/// `.values_list(...)` returns rows as Vec<SqlValue> in user-specified
/// column order.
#[tokio::test]
async fn values_list_returns_ordered_vec_per_row() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    // Reverse the natural order — title first, id second.
    let rows: Vec<Vec<SqlValue>> = Post::objects()
        .order_by(&[("id", false)])
        .values_list(&["title", "id"])
        .fetch(&pool)
        .await
        .unwrap();

    assert_eq!(rows.len(), 4);
    for row in &rows {
        assert_eq!(row.len(), 2);
        // [0] = title (String), [1] = id (I64)
        assert!(matches!(row[0], SqlValue::String(_)));
        assert!(matches!(row[1], SqlValue::I64(_)));
    }
}

/// `.values_list_flat::<i64>("id")` returns Vec<i64> directly — no
/// SqlValue wrapping.
#[tokio::test]
async fn values_list_flat_typed_i64_column() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let ids: Vec<i64> = Post::objects()
        .order_by(&[("id", false)])
        .values_list_flat("id")
        .fetch::<i64>(&pool)
        .await
        .unwrap();

    assert_eq!(ids, vec![1, 2, 3, 4], "PKs ascending: {ids:?}");
}

/// `.values_list_flat::<String>("title")` decodes the String column
/// directly.
#[tokio::test]
async fn values_list_flat_typed_string_column() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let titles: Vec<String> = Post::objects()
        .where_(Post::published.eq(true))
        .order_by(&[("id", false)])
        .values_list_flat("title")
        .fetch::<String>(&pool)
        .await
        .unwrap();

    assert_eq!(
        titles,
        vec![
            "Intro to Rust".to_owned(),
            "Advanced Lifetimes".to_owned(),
            "Performance Tips".to_owned(),
        ]
    );
}

/// `.values_list_flat::<bool>("published")` decodes a boolean.
#[tokio::test]
async fn values_list_flat_typed_bool_column() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let flags: Vec<bool> = Post::objects()
        .order_by(&[("id", false)])
        .values_list_flat("published")
        .fetch::<bool>(&pool)
        .await
        .unwrap();

    assert_eq!(flags, vec![true, true, false, true]);
}
