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

/// Each values-family read is one counted query.
async fn values_reads_are_counted(pool: &Pool) {
    use rustango::test_assertions::QueryCounter;
    QueryCounter::scope(async {
        let qs = || Post::objects().order_by(&[("id", false)]);
        qs().values_dict(&["id"]).fetch(pool).await.expect("dict");
        assert_eq!(QueryCounter::take(), 1, "values_dict");
        qs().values_list(&["id"]).fetch(pool).await.expect("list");
        assert_eq!(QueryCounter::take(), 1, "values_list");
        qs().pluck::<i64>("id", pool).await.expect("pluck");
        assert_eq!(QueryCounter::take(), 1, "pluck");
        qs().value::<i64>("id", pool).await.expect("value");
        assert_eq!(QueryCounter::take(), 1, "value");
        qs().pluck_pairs::<i64, String>("id", "title", pool)
            .await
            .expect("pairs");
        assert_eq!(QueryCounter::take(), 1, "pluck_pairs");
        Post::objects()
            .aggregate()
            .annotate("n", AggregateExpr::Count(None))
            .fetch(pool)
            .await
            .expect("aggregate");
        assert_eq!(QueryCounter::take(), 1, "aggregate");
    })
    .await;
}

/// One nullable column per portable type, for the decode scenarios.
#[derive(Model, Debug, Clone)]
#[rustango(table = "values_tri_typed")]
#[rustango(app = "values_tri")]
#[allow(dead_code)]
pub struct Typed {
    #[rustango(primary_key)]
    pub id: i64,
    pub small_n: Option<i16>,
    pub mid_n: Option<i32>,
    pub big_n: Option<i64>,
    pub real_n: Option<f32>,
    pub double_n: Option<f64>,
    pub flag: Option<bool>,
    pub body: Option<String>,
    #[rustango(max_length = 16)]
    pub label: Option<String>,
    pub day: Option<chrono::NaiveDate>,
    pub clock: Option<chrono::NaiveTime>,
    pub at_ts: Option<chrono::DateTime<chrono::Utc>>,
    pub token: Option<uuid::Uuid>,
    pub doc: Option<serde_json::Value>,
    pub blob: Option<Vec<u8>>,
}

const TYPED_COLS: &[&str] = &[
    "id", "small_n", "mid_n", "big_n", "real_n", "double_n", "flag", "body", "label", "day",
    "clock", "at_ts", "token", "doc", "blob",
];

/// Row 1 holds a value in every column, row 2 NULL in every nullable one.
async fn seed_typed(pool: &Pool) -> Vec<SqlValue> {
    rustango::testkit::matrix::fresh_table::<Typed>(pool).await;
    let day = chrono::NaiveDate::from_ymd_opt(2024, 1, 2).unwrap();
    let clock = chrono::NaiveTime::from_hms_micro_opt(3, 4, 5, 123_456).unwrap();
    let at = day.and_time(clock).and_utc();
    let token = uuid::uuid!("6f1c2a4e-9b7d-4c3a-8e21-0d5f4b6a7c89");
    let doc = serde_json::json!({ "k": [1, 2] });
    let blob = vec![0_u8, 1, 2, 255];
    Typed {
        id: 1,
        small_n: Some(-7),
        mid_n: Some(70_000),
        big_n: Some(1 << 40),
        real_n: Some(1.5),
        double_n: Some(2.25),
        flag: Some(true),
        body: Some("body".into()),
        label: Some("label".into()),
        day: Some(day),
        clock: Some(clock),
        at_ts: Some(at),
        token: Some(token),
        doc: Some(doc.clone()),
        blob: Some(blob.clone()),
    }
    .insert_pool(pool)
    .await
    .expect("seed typed");
    Typed {
        id: 2,
        small_n: None,
        mid_n: None,
        big_n: None,
        real_n: None,
        double_n: None,
        flag: None,
        body: None,
        label: None,
        day: None,
        clock: None,
        at_ts: None,
        token: None,
        doc: None,
        blob: None,
    }
    .insert_pool(pool)
    .await
    .expect("seed nulls");
    vec![
        SqlValue::I64(1),
        SqlValue::I16(-7),
        SqlValue::I32(70_000),
        SqlValue::I64(1 << 40),
        SqlValue::F32(1.5),
        SqlValue::F64(2.25),
        SqlValue::Bool(true),
        SqlValue::String("body".into()),
        SqlValue::String("label".into()),
        SqlValue::Date(day),
        SqlValue::Time(clock),
        SqlValue::DateTime(at),
        SqlValue::Uuid(token),
        SqlValue::Json(doc),
        SqlValue::Binary(blob),
    ]
}

/// Every value read through `values_list`, `values_dict` and a grouped
/// `values().annotate()` keeps its declared type, and NULL stays `Null`.
/// PG lost SMALLINT/REAL/TIME, SQLite returned dates as text (#2486).
async fn values_decode_every_type(pool: &Pool) {
    let want = seed_typed(pool).await;
    let mut nulls = vec![SqlValue::I64(2)];
    nulls.resize(want.len(), SqlValue::Null);
    let by_id = || Typed::objects().order_by(&[("id", false)]);

    let list = by_id()
        .values_list(TYPED_COLS)
        .fetch(pool)
        .await
        .expect("list");
    assert_eq!(list, vec![want.clone(), nulls.clone()], "values_list");

    let as_row = |m: &HashMap<String, SqlValue>| -> Vec<SqlValue> {
        TYPED_COLS.iter().map(|c| m[*c].clone()).collect()
    };
    let dict = by_id()
        .values_dict(TYPED_COLS)
        .fetch(pool)
        .await
        .expect("dict");
    assert_eq!(
        dict.iter().map(as_row).collect::<Vec<_>>(),
        vec![want.clone(), nulls.clone()],
        "values_dict"
    );

    let grouped = Typed::objects()
        .values(TYPED_COLS)
        .annotate("n", AggregateExpr::Count(None))
        .fetch(pool)
        .await
        .expect("annotate");
    let mut grouped: Vec<Vec<SqlValue>> = grouped.iter().map(as_row).collect();
    grouped.sort_by_key(|r| format!("{:?}", r[0]));
    assert_eq!(
        grouped,
        vec![want.clone(), nulls.clone()],
        "values().annotate()"
    );

    // The `_on` reads decode through the same plan.
    #[cfg(feature = "postgres")]
    if let Some(pg) = pool.as_postgres() {
        let on = by_id()
            .values_dict(TYPED_COLS)
            .fetch_on(pg)
            .await
            .expect("fetch_on");
        assert_eq!(
            on.iter().map(as_row).collect::<Vec<_>>(),
            vec![want.clone(), nulls.clone()],
            "fetch_on"
        );
        let agg = Typed::objects()
            .values(TYPED_COLS)
            .annotate("n", AggregateExpr::Count(None))
            .fetch_on(pg)
            .await
            .expect("aggregate fetch_on");
        let mut agg: Vec<Vec<SqlValue>> = agg.iter().map(as_row).collect();
        agg.sort_by_key(|r| format!("{:?}", r[0]));
        assert_eq!(agg, vec![want, nulls], "aggregate fetch_on");
    }
}

/// An aggregate over a typed column keeps the column's type where the
/// backend reports it; SQLite reports none, so it reads the storage class.
async fn aggregate_expression_keeps_column_type(pool: &Pool) {
    let want = seed_typed(pool).await;
    let rows = Typed::objects()
        .where_(Typed::id.eq(1_i64))
        .aggregate()
        .annotate("s", AggregateExpr::Max("small_n"))
        .annotate("r", AggregateExpr::Max("real_n"))
        .annotate("c", AggregateExpr::Max("clock"))
        .annotate("d", AggregateExpr::Min("day"))
        .fetch(pool)
        .await
        .expect("aggregate");
    let got = [
        rows[0]["s"].clone(),
        rows[0]["r"].clone(),
        rows[0]["c"].clone(),
        rows[0]["d"].clone(),
    ];
    // MAX/MIN keep the field's type on every backend (dialects-001).
    let expect = [
        want[1].clone(),
        want[4].clone(),
        want[10].clone(),
        want[9].clone(),
    ];
    assert_eq!(got, expect, "{}", pool.dialect().name());
}

/// `NUMERIC` / `DECIMAL` reads as `Decimal`, a model row included.
mod decimal {
    use super::*;
    use rust_decimal::Decimal;

    #[derive(Model, Debug, Clone)]
    #[rustango(table = "values_tri_money")]
    #[rustango(app = "values_tri")]
    #[allow(dead_code)]
    pub struct Money {
        #[rustango(primary_key)]
        pub id: i64,
        pub amount: Option<Decimal>,
    }

    async fn values_decode_decimal(pool: &Pool) {
        let v: Decimal = "12.34".parse().unwrap();
        for (id, amount) in [(1, Some(v)), (2, None)] {
            Money { id, amount }.insert_pool(pool).await.expect("seed");
        }
        let want = vec![
            vec![SqlValue::I64(1), SqlValue::Decimal(v)],
            vec![SqlValue::I64(2), SqlValue::Null],
        ];
        let qs = || Money::objects().order_by(&[("id", false)]);
        let list = qs()
            .values_list(&["id", "amount"])
            .fetch(pool)
            .await
            .expect("list");
        assert_eq!(list, want);
        use rustango::sql::FetcherPool as _;
        let rows = qs().fetch(pool).await.expect("rows");
        let amounts: Vec<_> = rows.iter().map(|m| m.amount).collect();
        assert_eq!(amounts, [Some(v), None]);
        let flat = qs()
            .values_list_flat("amount")
            .fetch::<Option<Decimal>>(pool)
            .await
            .expect("flat");
        assert_eq!(flat, [Some(v), None]);
        let dict = qs()
            .values_dict(&["id", "amount"])
            .fetch(pool)
            .await
            .expect("dict");
        assert_eq!(dict[0]["amount"], SqlValue::Decimal(v));
        assert_eq!(dict[1]["amount"], SqlValue::Null);
        let sum = Money::objects()
            .where_(Money::id.eq(1_i64))
            .aggregate()
            .annotate("s", AggregateExpr::Sum("amount"))
            .fetch(pool)
            .await
            .expect("sum");
        let want_sum = rustango::by_dialect! { pool,
            postgres => SqlValue::Decimal(v), because "SUM over NUMERIC is NUMERIC",
            mysql => SqlValue::Decimal(v), because "SUM over DECIMAL is DECIMAL",
            sqlite => SqlValue::F64(12.34), because "SQLite's SUM over NUMERIC is REAL",
        };
        assert_eq!(sum[0]["s"], want_sum.value, "{}", want_sum.why);

        // The executor path decodes NUMERIC too.
        #[cfg(feature = "postgres")]
        if let Some(pg) = pool.as_postgres() {
            let on = Money::objects()
                .values(&["amount"])
                .annotate("s", AggregateExpr::Sum("amount"))
                .fetch_on(pg)
                .await
                .expect("fetch_on");
            let mut got: Vec<_> = on
                .iter()
                .map(|r| (r["amount"].clone(), r["s"].clone()))
                .collect();
            got.sort_by_key(|r| format!("{r:?}"));
            assert_eq!(
                got,
                vec![
                    (SqlValue::Decimal(v), SqlValue::Decimal(v)),
                    (SqlValue::Null, SqlValue::Null),
                ]
            );
        }
    }

    tri_dialect_test! {
        model: Money,
        scenarios: [values_decode_decimal],
    }
}

tri_dialect_test! {
    setup: seeded,
    scenarios: [
        values_decode_every_type,
        aggregate_expression_keeps_column_type,
        values_dict_returns_a_map_per_row,
        values_list_uses_the_requested_column_order,
        values_list_flat_decodes_an_integer_column,
        values_list_flat_decodes_a_string_column,
        values_list_flat_decodes_a_boolean_column,
        values_list_flat_null_needs_an_option,
        pluck_pairs_null_needs_an_option,
        values_reads_are_counted,
        values_keep_bool_and_json_types,
        aggregate_alias_does_not_take_the_column_type,
        joined_group_bool_keeps_its_type,
        joined_projection_bool_keeps_its_type,
    ],
}

/// Projections that name a related column (`writer__name`) read it through
/// one LEFT JOIN per FK hop, in the caller's column order.
mod related {
    use super::*;
    use rustango::core::QueryError;
    use rustango::sql::ForeignKey;

    #[derive(Model, Debug, Clone)]
    #[rustango(table = "values_tri_press", app = "values_tri")]
    #[allow(dead_code)]
    pub struct Press {
        #[rustango(primary_key)]
        pub id: i64,
        #[rustango(max_length = 32)]
        pub name: String,
    }

    #[derive(Model, Debug, Clone)]
    #[rustango(table = "values_tri_writer", app = "values_tri")]
    #[allow(dead_code)]
    pub struct Writer {
        #[rustango(primary_key)]
        pub id: i64,
        #[rustango(max_length = 32)]
        pub name: String,
        pub press: ForeignKey<Press>,
    }

    #[derive(Model, Debug, Clone)]
    #[rustango(table = "values_tri_book", app = "values_tri")]
    #[allow(dead_code)]
    pub struct Book {
        #[rustango(primary_key)]
        pub id: i64,
        #[rustango(max_length = 32)]
        pub title: String,
        pub body: String,
        pub writer: Option<ForeignKey<Writer>>,
    }

    async fn setup(pool: &Pool) {
        use rustango::testkit::matrix::{drop_table, fresh_table};
        drop_table(pool, Book::SCHEMA.table).await;
        drop_table(pool, Writer::SCHEMA.table).await;
        fresh_table::<Press>(pool).await;
        fresh_table::<Writer>(pool).await;
        fresh_table::<Book>(pool).await;
        Press {
            id: 1,
            name: "Acme".into(),
        }
        .insert_pool(pool)
        .await
        .expect("press");
        Writer {
            id: 1,
            name: "Ann".into(),
            press: ForeignKey::unloaded(1),
        }
        .insert_pool(pool)
        .await
        .expect("writer");
        for (id, title, writer) in [(1, "One", Some(1)), (2, "Two", None)] {
            Book {
                id,
                title: title.into(),
                body: "long text".into(),
                writer: writer.map(ForeignKey::unloaded),
            }
            .insert_pool(pool)
            .await
            .expect("book");
        }
    }

    fn s(v: &str) -> SqlValue {
        SqlValue::String(v.into())
    }

    async fn values_list_reads_related_columns_in_order(pool: &Pool) {
        let rows = Book::objects()
            .select_related("writer")
            .order_by(&[("id", false)])
            .values_list(&["writer__name", "title", "writer__press__name"])
            .fetch(pool)
            .await
            .expect("values_list");
        assert_eq!(
            rows,
            vec![
                vec![s("Ann"), s("One"), s("Acme")],
                vec![SqlValue::Null, s("Two"), SqlValue::Null],
            ]
        );
    }

    async fn values_dict_and_only_key_related_columns_by_name(pool: &Pool) {
        let rows = Book::objects()
            .filter("writer__name", "Ann")
            .only(&["title", "writer__name"])
            .fetch(pool)
            .await
            .expect("only");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].len(), 2, "{rows:?}");
        assert_eq!(rows[0]["writer__name"], s("Ann"));
        assert_eq!(rows[0]["title"], s("One"));
        let names: Vec<Option<String>> = Book::objects()
            .order_by(&[("id", false)])
            .values_list_flat("writer__press__name")
            .fetch::<Option<String>>(pool)
            .await
            .expect("flat");
        assert_eq!(names, vec![Some("Acme".to_owned()), None]);
    }

    /// `distinct_on` (a window rewrite on MySQL / SQLite) keeps related columns.
    async fn related_projection_with_distinct_on(pool: &Pool) {
        let rows = Book::objects()
            .distinct_on(&["title"])
            .order_by(&[("title", false), ("id", false)])
            .values_list(&["title", "writer__name"])
            .fetch(pool)
            .await
            .expect("distinct_on values_list");
        assert_eq!(
            rows,
            vec![vec![s("One"), s("Ann")], vec![s("Two"), SqlValue::Null]]
        );
        let plain = Book::objects()
            .distinct()
            .values_list(&["writer__press__name"])
            .fetch(pool)
            .await
            .expect("distinct values_list");
        assert_eq!(plain.len(), 2, "{plain:?}");
    }

    async fn related_projection_selects_only_what_it_names(pool: &Pool) {
        let q = Book::objects()
            .filter("writer__name", "Ann")
            .values_list(&["title", "writer__name"])
            .compile()
            .expect("compile");
        let sql = pool.dialect().compile_select(&q).expect("sql").sql;
        assert!(!sql.contains("body"), "{sql}");
        assert_eq!(sql.matches("JOIN").count(), 1, "{sql}");
        for bad in ["writer__bogus", "title__name", "writer__press__bogus"] {
            let err = Book::objects().values_list(&[bad]).compile().unwrap_err();
            assert!(
                matches!(err, QueryError::UnknownField { .. }),
                "{bad}: {err:?}"
            );
        }
        // `defer` names columns to leave out; a related one is not one of them.
        let err = Book::objects()
            .defer(&["writer__name"])
            .compile()
            .unwrap_err();
        assert!(matches!(err, QueryError::UnknownField { .. }), "{err:?}");
    }

    tri_dialect_test! {
        setup: setup,
        scenarios: [
            values_list_reads_related_columns_in_order,
            values_dict_and_only_key_related_columns_by_name,
            related_projection_selects_only_what_it_names,
            related_projection_with_distinct_on,
        ],
    }
}
