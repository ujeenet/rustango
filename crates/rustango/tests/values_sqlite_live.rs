#![cfg(feature = "sqlite")]
//! Live SQLite tests for `QuerySet::values_dict` / `values_list` /
//! `values_list_flat` (issue #22). Pure projection round-trip on a
//! sqlite::memory: pool — proves the bi-dialect emission + dynamic
//! decode path works on SQLite as well as PG.

use std::collections::HashMap;

use rustango::core::{Model as _, SqlValue};
use rustango::sql::{Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "v_post_sqlite")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub title: String,
    pub view_count: i64,
    pub published: bool,
}

async fn seeded_pool() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite");
    rustango::sql::raw_execute_pool(
        &pool,
        "CREATE TABLE v_post_sqlite (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            title TEXT NOT NULL,
            view_count INTEGER NOT NULL,
            published INTEGER NOT NULL
        )",
        vec![],
    )
    .await
    .unwrap();
    for (title, vc, pub_) in [
        ("Intro to Rust", 100_i64, 1_i64),
        ("Advanced Lifetimes", 50, 1),
        ("Draft", 0, 0),
        ("Performance Tips", 200, 1),
    ] {
        rustango::sql::raw_execute_pool(
            &pool,
            "INSERT INTO v_post_sqlite(title, view_count, published) VALUES (?, ?, ?)",
            vec![
                SqlValue::String(title.to_owned()),
                SqlValue::I64(vc),
                SqlValue::I64(pub_),
            ],
        )
        .await
        .unwrap();
    }
    let _ = Post::SCHEMA;
    pool
}

#[tokio::test]
async fn values_dict_returns_hashmap_on_sqlite() {
    let pool = seeded_pool().await;

    let rows: Vec<HashMap<String, SqlValue>> = Post::objects()
        .order_by(&[("id", false)])
        .values_dict(&["id", "title"])
        .fetch(&pool)
        .await
        .unwrap();

    assert_eq!(rows.len(), 4);
    for row in &rows {
        assert_eq!(row.len(), 2);
        assert!(row.contains_key("id") && row.contains_key("title"));
        assert!(!row.contains_key("view_count"));
    }
    match &rows[0]["title"] {
        SqlValue::String(s) => assert_eq!(s, "Intro to Rust"),
        other => panic!("expected String, got {other:?}"),
    }
}

#[tokio::test]
async fn values_list_flat_typed_i64_on_sqlite() {
    let pool = seeded_pool().await;

    let view_counts: Vec<i64> = Post::objects()
        .order_by(&[("id", false)])
        .values_list_flat("view_count")
        .fetch::<i64>(&pool)
        .await
        .unwrap();

    assert_eq!(view_counts, vec![100, 50, 0, 200]);
}

#[tokio::test]
async fn values_list_flat_typed_string_on_sqlite() {
    let pool = seeded_pool().await;

    let titles: Vec<String> = Post::objects()
        .order_by(&[("id", false)])
        .values_list_flat("title")
        .fetch::<String>(&pool)
        .await
        .unwrap();

    assert_eq!(titles.len(), 4);
    assert_eq!(titles[0], "Intro to Rust");
    assert_eq!(titles[3], "Performance Tips");
}
