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

use rustango::core::joins::aliased;
use rustango::core::{
    AggregateExpr, Column as _, Join, JoinKind, Model as _, Op, SqlValue, WhereExpr,
};
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
    pub score: Option<i64>,
    pub meta: Option<serde_json::Value>,
}

/// Joined to `Post` by id, so a join reads another model's columns.
#[derive(Model, Debug, Clone)]
#[rustango(table = "values_tri_author")]
#[rustango(app = "values_tri")]
#[allow(dead_code)]
pub struct Author {
    #[rustango(primary_key)]
    pub id: i64,
    pub active: bool,
}

/// Rebuild the table and seed the four rows every scenario reads.
///
/// Row order is load-bearing: the flat-column assertions below compare
/// whole vectors, so the seed order *is* the expectation.
async fn seeded(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Post>(pool).await;

    for (title, view_count, published, score) in [
        ("Intro to Rust", 100_i64, true, Some(7_i64)),
        ("Advanced Lifetimes", 50, true, None),
        ("Draft Post", 0, false, None),
        ("Performance Tips", 200, true, Some(9)),
    ] {
        let mut p = Post {
            id: Auto::default(),
            title: title.into(),
            view_count,
            published,
            score,
            meta: Some(serde_json::json!({ "views": view_count })),
        };
        p.insert_pool(pool).await.expect("seed row");
    }
    rustango::testkit::matrix::fresh_table::<Author>(pool).await;
    for (id, active) in [(1, false), (2, false), (3, true), (4, true)] {
        Author { id, active }
            .insert_pool(pool)
            .await
            .expect("seed author");
    }
}

/// `Post` joined to its same-id `Author` as `a`, projecting `project`.
fn author_join(project: Vec<&'static str>) -> Join {
    Join {
        target: Author::SCHEMA,
        alias: "a",
        kind: JoinKind::Inner,
        on: WhereExpr::ExprCompare {
            lhs: aliased("a", "id"),
            op: Op::Eq,
            rhs: aliased("values_tri_post", "id"),
        },
        project,
    }
}

/// Sorted, so row order does not matter.
fn sorted(mut v: Vec<SqlValue>) -> Vec<SqlValue> {
    v.sort_by_key(|v| format!("{v:?}"));
    v
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

/// A bool reads as `Bool` and a JSON column as `Json` on every backend;
/// MySQL gave `I64` and `Null` (#2296).
async fn values_keep_bool_and_json_types(pool: &Pool) {
    let qs = || Post::objects().where_(Post::id.eq(1_i64));
    let dict = qs()
        .values_dict(&["published", "meta"])
        .fetch(pool)
        .await
        .expect("values_dict");
    let list = qs()
        .values_list(&["published", "meta"])
        .fetch(pool)
        .await
        .expect("values_list");
    let want = [
        SqlValue::Bool(true),
        SqlValue::Json(serde_json::json!({ "views": 100 })),
    ];
    assert_eq!(
        [dict[0]["published"].clone(), dict[0]["meta"].clone()],
        want
    );
    assert_eq!(list[0], want);
}

/// An aggregate alias named like a bool column is not that column: it keeps
/// its own type, while a real group-by column still reads as `Bool` (#2296).
async fn aggregate_alias_does_not_take_the_column_type(pool: &Pool) {
    let rows = Post::objects()
        .where_(Post::id.eq(4_i64))
        .values(&["title"])
        .annotate("published", AggregateExpr::Max("score"))
        .fetch(pool)
        .await
        .expect("aggregate");
    assert_eq!(rows[0]["published"], SqlValue::I64(9));
    let groups = Post::objects()
        .where_(Post::id.eq(1_i64))
        .values(&["published"])
        .annotate("n", AggregateExpr::Count(None))
        .fetch(pool)
        .await
        .expect("grouped");
    assert_eq!(groups[0]["published"], SqlValue::Bool(true));
}

/// A bool grouped through a join reads as `Bool`, with and without a
/// derived table; MySQL and SQLite gave `I64` (#2322).
async fn joined_group_bool_keeps_its_type(pool: &Pool) {
    let both = [SqlValue::Bool(false), SqlValue::Bool(true)];
    for distinct in [false, true] {
        let qs = || {
            let qs = Post::objects().join(author_join(vec![]));
            if distinct {
                qs.distinct()
            } else {
                qs
            }
        };
        let group = |col: &'static str, key: &'static str| {
            let qs = qs();
            async move {
                let rows = qs
                    .values(&[col])
                    .annotate("n", AggregateExpr::Count(None))
                    .fetch(pool)
                    .await
                    .expect("joined group");
                sorted(rows.into_iter().map(|r| r[key].clone()).collect())
            }
        };
        assert_eq!(
            group("a.active", "a__active").await,
            both,
            "distinct={distinct}"
        );
        // The base table's own dotted name.
        assert_eq!(
            group("values_tri_post.published", "values_tri_post__published").await,
            both,
            "distinct={distinct}"
        );
    }
}

/// A joined column a `values` projection carries reads as `Bool` too (#2322).
async fn joined_projection_bool_keeps_its_type(pool: &Pool) {
    let want = vec![
        SqlValue::Bool(false),
        SqlValue::Bool(false),
        SqlValue::Bool(true),
        SqlValue::Bool(true),
    ];
    let qs = || Post::objects().join(author_join(vec!["active"]));
    let dict = qs()
        .values_dict(&["title"])
        .fetch(pool)
        .await
        .expect("dict");
    assert_eq!(
        sorted(dict.into_iter().map(|r| r["a__active"].clone()).collect()),
        want
    );
    let list = qs()
        .values_list(&["title"])
        .fetch(pool)
        .await
        .expect("list");
    assert_eq!(
        sorted(list.into_iter().map(|r| r[1].clone()).collect()),
        want
    );
}

/// NULL must error into a bare `i64` and read as `None` into
/// `Option<i64>`; SQLite used to hand back `0` (#1773).
async fn values_list_flat_null_needs_an_option(pool: &Pool) {
    let flat = || {
        Post::objects()
            .order_by(&[("id", false)])
            .values_list_flat("score")
    };
    let err = flat().fetch::<i64>(pool).await.expect_err("NULL into i64");
    assert!(err.to_string().contains("score"), "names the column: {err}");
    let scores = flat()
        .fetch::<Option<i64>>(pool)
        .await
        .expect("Option<i64>");
    assert_eq!(scores, vec![Some(7), None, None, Some(9)]);

    let one = || Post::objects().where_(Post::id.eq(2_i64));
    one()
        .value::<i64>("score", pool)
        .await
        .expect_err("value NULL into i64");
    let v = one()
        .value::<Option<i64>>("score", pool)
        .await
        .expect("value Option<i64>");
    assert_eq!(v, Some(None));
}

/// `pluck_pairs` reads NULL like the flat decode: an error into a bare
/// `i64`, `None` into `Option<i64>` (#1808).
async fn pluck_pairs_null_needs_an_option(pool: &Pool) {
    let posts = || Post::objects().order_by(&[("id", false)]);
    let err = posts()
        .pluck_pairs::<String, i64>("title", "score", pool)
        .await
        .expect_err("NULL into i64");
    assert!(err.to_string().contains("score"), "names the column: {err}");
    let pairs = posts()
        .pluck_pairs::<String, Option<i64>>("title", "score", pool)
        .await
        .expect("Option<i64>");
    let scores: Vec<Option<i64>> = pairs.into_iter().map(|(_, s)| s).collect();
    assert_eq!(scores, vec![Some(7), None, None, Some(9)]);
}

tri_dialect_test! {
    setup: seeded,
    scenarios: [
        values_dict_returns_a_map_per_row,
        values_list_uses_the_requested_column_order,
        values_list_flat_decodes_an_integer_column,
        values_list_flat_decodes_a_string_column,
        values_list_flat_decodes_a_boolean_column,
        values_list_flat_null_needs_an_option,
        pluck_pairs_null_needs_an_option,
        values_keep_bool_and_json_types,
        aggregate_alias_does_not_take_the_column_type,
        joined_group_bool_keeps_its_type,
        joined_projection_bool_keeps_its_type,
    ],
}
