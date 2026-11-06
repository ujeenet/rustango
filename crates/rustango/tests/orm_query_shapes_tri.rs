//! Query shapes on every backend: `.aggregate()` returns one row,
//! `last()` under DISTINCT ON, counts over DISTINCT + relation order +
//! LIMIT, empty IN lists, DISTINCT with an expression order, SQLite LIKE
//! against a column, `where_raw` / `Q` checks, and `refresh_from_db`.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::core::aggregates::count_all;
use rustango::core::funcs::{lower, replace};
use rustango::core::{Column as _, Filter, Model as _, Op, QueryError, SqlValue, WhereExpr, F};
use rustango::query::Q;
use rustango::sql::{
    CounterPool as _, ExecError, ExistsPool as _, FetcherPool as _, ForeignKey, Pool, SqlError,
};
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(table = "oqs_author", app = "oqs")]
pub struct Author {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 20)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "oqs_post", app = "oqs")]
pub struct Post {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 20)]
    pub title: String,
    #[rustango(max_length = 20)]
    pub pattern: String,
    pub grp: i64,
    pub rank: i64,
    pub author: ForeignKey<Author>,
}

fn shown_only() -> WhereExpr {
    WhereExpr::Predicate(Filter::new("shown", Op::Eq, SqlValue::Bool(true)))
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "oqs_doc", app = "oqs", global_scope(name = "shown", apply = shown_only))]
pub struct Doc {
    #[rustango(primary_key)]
    pub id: i64,
    pub shown: bool,
    pub n: i64,
}

async fn setup(pool: &Pool) {
    use rustango::testkit::matrix::{drop_table, fresh_table};
    drop_table(pool, Post::SCHEMA.table).await;
    fresh_table::<Author>(pool).await;
    fresh_table::<Post>(pool).await;
    fresh_table::<Doc>(pool).await;
}

/// `(id, title, pattern, grp, rank, author)`.
async fn seed(pool: &Pool) {
    for (id, name) in [(1, "zed"), (2, "amy")] {
        Author {
            id,
            name: name.into(),
        }
        .insert(pool)
        .await
        .expect("author");
    }
    for (id, title, pattern, grp, rank, author) in [
        (1, "xd", "XD", 1, 1, 1),
        (2, "c", "c", 1, 5, 2),
        (3, "b", "b%", 2, 2, 1),
        (4, "a", "A", 2, 9, 2),
    ] {
        Post {
            id,
            title: title.into(),
            pattern: pattern.into(),
            grp,
            rank,
            author: ForeignKey::unloaded(author),
        }
        .insert(pool)
        .await
        .expect("post");
    }
}

fn ids(rows: &[Post]) -> Vec<i64> {
    rows.iter().map(|p| p.id).collect()
}

/// `.aggregate()` with no `.values()` sums the whole set into one row.
async fn aggregate_alone_is_one_row(pool: &Pool) {
    let summary = || {
        Post::objects().aggregate().annotate(
            "high",
            count_all()
                .filter(Post::rank.gt(1_i64))
                .default(0_i64)
                .into(),
        )
    };
    let empty = summary().fetch(pool).await.expect("empty table");
    assert_eq!(empty.len(), 1, "one summary row on an empty table");
    seed(pool).await;
    let rows = summary().fetch(pool).await.expect("aggregate");
    assert_eq!(rows.len(), 1, "one summary row, not one per post: {rows:?}");
    assert_eq!(rows[0]["high"], SqlValue::I64(3));
    // The `.annotate()` shortcut still adds the row's own columns.
    let per_row = Post::objects()
        .annotate("n", count_all().into())
        .fetch(pool)
        .await
        .expect("per row");
    assert_eq!(per_row.len(), 4);
}

/// `last()` keeps the row each DISTINCT ON group picked.
async fn last_under_distinct_on_keeps_the_groups_row(pool: &Pool) {
    seed(pool).await;
    let qs = || {
        Post::objects()
            .distinct_on(&["grp"])
            .order_by(&[("grp", false), ("rank", true)])
    };
    let all = qs().fetch(pool).await.expect("fetch");
    assert_eq!(ids(&all), [2, 4], "highest rank per group");
    let last = qs().last(pool).await.expect("last").expect("a row");
    assert_eq!(last.id, 4, "the last row of the result");
    let first = qs().first(pool).await.expect("first").expect("a row");
    assert_eq!(first.id, 2);
}

/// A count over DISTINCT + relation order + LIMIT selects the order column.
async fn count_over_distinct_relation_order_limit(pool: &Pool) {
    seed(pool).await;
    let qs = || {
        Post::objects()
            .distinct()
            .order_by(&[("author__name", false)])
            .limit(3)
    };
    assert_eq!(qs().fetch(pool).await.expect("fetch").len(), 3);
    assert_eq!(qs().count(pool).await.expect("count"), 3);
    assert!(qs().exists(pool).await.expect("exists"));
}

/// An empty IN matches nothing and an empty NOT IN matches everything.
async fn empty_in_lists(pool: &Pool) {
    seed(pool).await;
    let none = || SqlValue::List(vec![]);
    let got = Post::objects()
        .filter("id__in", none())
        .fetch(pool)
        .await
        .expect("empty IN");
    assert!(got.is_empty());
    let got = Post::objects()
        .exclude("id__in", none())
        .order_by(&[("id", false)])
        .fetch(pool)
        .await
        .expect("exclude empty IN");
    assert_eq!(ids(&got), [1, 2, 3, 4]);
    let got = Post::objects()
        .filter_op("id", Op::NotIn, none())
        .count(pool)
        .await
        .expect("empty NOT IN");
    assert_eq!(got, 4);
    let got = Post::objects()
        .where_(Q::eq("id", 3_i64) | Q::in_("id", Vec::<i64>::new()))
        .fetch(pool)
        .await
        .expect("empty IN in an OR branch");
    assert_eq!(ids(&got), [3]);
    let got = Post::objects()
        .filter("author__name__in", none())
        .count(pool)
        .await
        .expect("empty IN across a relation");
    assert_eq!(got, 0);
}

/// DISTINCT ordered by an expression selects it, binds and all.
async fn distinct_ordered_by_an_expression(pool: &Pool) {
    seed(pool).await;
    let got = Post::objects()
        .distinct()
        .order_by_expr(lower(F("title")), false)
        .fetch(pool)
        .await
        .expect("lower()");
    assert_eq!(ids(&got), [4, 3, 2, 1]);
    let got: Vec<String> = Post::objects()
        .distinct()
        .order_by_expr(replace(F("title"), "x", "a"), false)
        .values_list_flat("title")
        .fetch::<String>(pool)
        .await
        .expect("an expression with binds");
    assert_eq!(got, ["a", "xd", "b", "c"], "xd sorts as ad");
}

/// LIKE against a column is case-sensitive, or refused on SQLite.
async fn like_against_a_column(pool: &Pool) {
    seed(pool).await;
    let got = Post::objects()
        .where_raw(WhereExpr::ExprCompare {
            lhs: F("title").into(),
            op: Op::Like,
            rhs: F("pattern").into(),
        })
        .order_by(&[("id", false)])
        .fetch(pool)
        .await;
    match pool.dialect().name() {
        "sqlite" => assert!(
            matches!(
                got,
                Err(ExecError::Sql(SqlError::OpNotSupportedInDialect { .. }))
            ),
            "SQLite LIKE ignores case: {got:?}"
        ),
        _ => assert_eq!(ids(&got.expect("like")), [2, 3]),
    }
}

/// `where_raw` and `Q` are checked against the model at `compile()`.
async fn raw_and_q_checked_at_compile(pool: &Pool) {
    let _ = pool;
    // The FK field `author` is stored in column `author`, not `author_id`.
    let r = Post::objects().where_(Q::eq("author_id", 1_i64)).compile();
    assert!(
        matches!(r, Err(QueryError::UnknownField { ref field, .. }) if field == "author_id"),
        "{r:?}"
    );
    let r = Post::objects()
        .where_(Q::eq("id", 1_i64) | Q::eq("nope", 1_i64))
        .compile();
    assert!(
        matches!(r, Err(QueryError::UnknownField { ref field, .. }) if field == "nope"),
        "{r:?}"
    );
    let r = Post::objects()
        .where_raw(WhereExpr::Predicate(Filter::new(
            "title",
            Op::Eq,
            SqlValue::I64(5),
        )))
        .compile();
    assert!(matches!(r, Err(QueryError::TypeMismatch { .. })), "{r:?}");
    Post::objects()
        .where_(Q::eq("author", 1_i64) & Q::in_("id", [1_i64, 2]) & Q::like("rank", "%1%"))
        .compile()
        .expect("valid columns pass");
}

/// `refresh_from_db` and `fresh` reload a row outside the global scopes.
async fn refresh_ignores_global_scopes(pool: &Pool) {
    Doc {
        id: 1,
        shown: false,
        n: 1,
    }
    .insert(pool)
    .await
    .expect("doc");
    let mut doc = Doc {
        id: 1,
        shown: false,
        n: 0,
    };
    doc.refresh_from_db(pool).await.expect("hidden row reloads");
    assert_eq!(doc.n, 1);
    assert!(doc.fresh(pool).await.expect("fresh").is_some());
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        aggregate_alone_is_one_row,
        last_under_distinct_on_keeps_the_groups_row,
        count_over_distinct_relation_order_limit,
        empty_in_lists,
        distinct_ordered_by_an_expression,
        like_against_a_column,
        raw_and_q_checked_at_compile,
        refresh_ignores_global_scopes,
    ],
}
