//! `QuerySet::values_dict` / `values_list` / `values_list_flat` on every
//! backend — one body, three dialects (#22, #1461).
//!
//! Replaces `values_live.rs` (PostgreSQL, 5 scenarios) and
//! `values_sqlite_live.rs` (SQLite, 3). MySQL had none.
//!
//! The split was not a statement about which dialects support
//! projection — it is what happens when the second and third copy are
//! written by hand. SQLite gains the two it was missing and MySQL gains
//! all five, for no new assertions:
//!
//! | scenario | was | now |
//! |---|---|---|
//! | `values_dict` shape | PG, SQLite | all three |
//! | `values_list` column order | PG only | all three |
//! | `values_list_flat::<i64>` | PG, SQLite | all three |
//! | `values_list_flat::<String>` | PG, SQLite | all three |
//! | `values_list_flat::<bool>` | **PG only** | all three |
//!
//! The last row is the one worth watching. `BOOLEAN` is a real type on
//! PostgreSQL, `TINYINT(1)` on MySQL, and an integer on SQLite — so
//! decoding a column to `bool` is exactly where a projection path would
//! diverge, and it was tested on one backend.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use std::collections::HashMap;

use rustango::core::{Column as _, SqlValue};
use rustango::sql::{Auto, Pool};
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(table = "values_tri_post")]
#[rustango(app = "values_tri")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub title: String,
    pub view_count: i64,
    /// `BOOLEAN` on PostgreSQL, `TINYINT(1)` on MySQL, an integer on
    /// SQLite — and the emitter picks per dialect, which is the point of
    /// building the table from `SCHEMA` rather than by hand.
    pub published: bool,
}

/// Rebuild the table and seed the four rows every scenario reads.
///
/// Row order is load-bearing: the flat-column assertions below compare
/// whole vectors, so the seed order *is* the expectation.
async fn seeded(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Post>(pool).await;

    for (title, view_count, published) in [
        ("Intro to Rust", 100_i64, true),
        ("Advanced Lifetimes", 50, true),
        ("Draft Post", 0, false),
        ("Performance Tips", 200, true),
    ] {
        let mut p = Post {
            id: Auto::default(),
            title: title.into(),
            view_count,
            published,
        };
        p.insert_pool(pool).await.expect("seed row");
    }
}

/// `values_dict` returns one map per row, carrying only the listed
/// columns.
async fn values_dict_returns_a_map_per_row(pool: &Pool) {
    let rows: Vec<HashMap<String, SqlValue>> = Post::objects()
        .where_(Post::published.eq(true))
        .order_by(&[("id", false)])
        .values_dict(&["id", "title"])
        .fetch(pool)
        .await
        .expect("values_dict");

    assert_eq!(rows.len(), 3, "three published posts: {rows:?}");
    for row in &rows {
        assert_eq!(row.len(), 2, "only the requested columns: {row:?}");
        assert!(row.contains_key("id"));
        assert!(row.contains_key("title"));
        // The unrequested columns must be absent, not null — projection
        // that quietly returns everything is the bug this guards.
        assert!(!row.contains_key("view_count"));
        assert!(!row.contains_key("published"));
    }
    match &rows[0]["title"] {
        SqlValue::String(s) => assert_eq!(s, "Intro to Rust"),
        other => panic!("expected a String title, got {other:?}"),
    }
}

/// `values_list` honours the caller's column order, not the model's.
async fn values_list_uses_the_requested_column_order(pool: &Pool) {
    // Deliberately reversed against the struct: title first, id second.
    let rows: Vec<Vec<SqlValue>> = Post::objects()
        .order_by(&[("id", false)])
        .values_list(&["title", "id"])
        .fetch(pool)
        .await
        .expect("values_list");

    assert_eq!(rows.len(), 4);
    for row in &rows {
        assert_eq!(row.len(), 2);
        assert!(
            matches!(row[0], SqlValue::String(_)),
            "position 0 should be the title, got {:?}",
            row[0]
        );
        assert!(
            matches!(row[1], SqlValue::I64(_)),
            "position 1 should be the id, got {:?}",
            row[1]
        );
    }
}

async fn values_list_flat_decodes_an_integer_column(pool: &Pool) {
    let ids: Vec<i64> = Post::objects()
        .order_by(&[("id", false)])
        .values_list_flat("id")
        .fetch::<i64>(pool)
        .await
        .expect("values_list_flat i64");

    assert_eq!(
        ids,
        vec![1, 2, 3, 4],
        "the table is dropped and rebuilt per scenario, so the sequence restarts \
         on every dialect"
    );
}

async fn values_list_flat_decodes_a_string_column(pool: &Pool) {
    let titles: Vec<String> = Post::objects()
        .where_(Post::published.eq(true))
        .order_by(&[("id", false)])
        .values_list_flat("title")
        .fetch::<String>(pool)
        .await
        .expect("values_list_flat String");

    assert_eq!(
        titles,
        vec![
            "Intro to Rust".to_owned(),
            "Advanced Lifetimes".to_owned(),
            "Performance Tips".to_owned(),
        ]
    );
}

/// The scenario that was PostgreSQL-only.
///
/// `bool` is the least portable column type in the suite: a real
/// `BOOLEAN` on PostgreSQL, `TINYINT(1)` on MySQL, an integer on SQLite.
/// Projection decoding it to `bool` is where a dialect would diverge,
/// and it was covered on exactly the backend where it is easiest.
async fn values_list_flat_decodes_a_boolean_column(pool: &Pool) {
    let flags: Vec<bool> = Post::objects()
        .order_by(&[("id", false)])
        .values_list_flat("published")
        .fetch::<bool>(pool)
        .await
        .expect("values_list_flat bool");

    assert_eq!(
        flags,
        vec![true, true, false, true],
        "a boolean column must decode to `bool` on every backend, whatever the \
         storage type underneath"
    );
}

tri_dialect_test! {
    setup: seeded,
    scenarios: [
        values_dict_returns_a_map_per_row,
        values_list_uses_the_requested_column_order,
        values_list_flat_decodes_an_integer_column,
        values_list_flat_decodes_a_string_column,
        values_list_flat_decodes_a_boolean_column,
    ],
}
