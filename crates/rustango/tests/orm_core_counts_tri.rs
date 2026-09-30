//! `count` / `exists` / `sum` honour every queryset clause (#1885),
//! `Sum` keeps the column's type (#1886) and a multi-batch
//! `bulk_insert_pool` is atomic (#1891), on every backend; join aliases
//! are not leaked per `compile()` (#1889).

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::core::{BulkInsertQuery, CountQuery, Model as _, SqlValue};
use rustango::sql::{
    atomic, bulk_insert_pool, CounterPool as _, ExecError, ExistsPool as _, ForeignKey, Pool,
};
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(table = "occ_author")]
#[allow(dead_code)]
pub struct Author {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 40)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "occ_book")]
#[allow(dead_code)]
pub struct Book {
    #[rustango(primary_key)]
    pub id: i64,
    pub author: ForeignKey<Author>,
    pub price: f64,
    pub pages: i64,
}

/// Ten columns, so a bind-limited batch is a few thousand rows.
#[derive(Model, Debug, Clone)]
#[rustango(table = "occ_wide")]
#[allow(dead_code)]
pub struct Wide {
    #[rustango(primary_key)]
    pub id: i64,
    pub c1: i64,
    pub c2: i64,
    pub c3: i64,
    pub c4: i64,
    pub c5: i64,
    pub c6: i64,
    pub c7: i64,
    pub c8: i64,
    pub c9: i64,
}

/// Ada: books 1 (10.75, 100 pages) and 2 (0.5, 200). Bob: book 3 (3.0, 300).
async fn seeded(pool: &Pool) {
    rustango::testkit::matrix::drop_table(pool, Book::SCHEMA.table).await;
    rustango::testkit::matrix::fresh_table::<Author>(pool).await;
    rustango::testkit::matrix::fresh_table::<Book>(pool).await;
    rustango::testkit::matrix::fresh_table::<Wide>(pool).await;
    for (id, name) in [(1, "Ada"), (2, "Bob")] {
        Author {
            id,
            name: name.into(),
        }
        .insert_pool(pool)
        .await
        .expect("seed author");
    }
    for (id, author, price, pages) in [(1, 1, 10.75, 100), (2, 1, 0.5, 200), (3, 2, 3.0, 300)] {
        Book {
            id,
            author: ForeignKey::unloaded(author),
            price,
            pages,
        }
        .insert_pool(pool)
        .await
        .expect("seed book");
    }
}

async fn none_counts_nothing(pool: &Pool) {
    assert_eq!(Book::objects().none().count(pool).await.unwrap(), 0);
    assert!(!Book::objects().none().exists(pool).await.unwrap());
    assert!(Book::objects().none().is_empty(pool).await.unwrap());
    let s: Option<i64> = Book::objects().none().sum("pages", pool).await.unwrap();
    assert_eq!(s, None, "sum over no rows");
}

async fn count_honours_limit_and_offset(pool: &Pool) {
    let qs = || Book::objects().order_by(&[("id", false)]);
    assert_eq!(qs().limit(2).count(pool).await.unwrap(), 2);
    assert_eq!(qs().offset(2).count(pool).await.unwrap(), 1);
    assert_eq!(qs().limit(1).offset(1).count(pool).await.unwrap(), 1);
    let s: Option<i64> = qs().limit(2).sum("pages", pool).await.unwrap();
    assert_eq!(s, Some(300), "sum over the first two rows only");
}

async fn count_honours_compound(pool: &Pool) {
    let one = || Book::objects().filter("id", 1_i64);
    let two = || Book::objects().filter("id__lte", 2_i64);
    assert_eq!(one().union(two()).count(pool).await.unwrap(), 2);
    assert_eq!(one().union_all(two()).count(pool).await.unwrap(), 3);
}

async fn exists_reads_one_row_unordered(pool: &Pool) {
    let ordered = || Book::objects().order_by(&[("id", true)]);
    let sql = |q: &CountQuery| pool.dialect().compile_count(q).unwrap().sql;
    let exists = sql(&CountQuery::exists(ordered().compile().unwrap()));
    assert!(exists.contains("LIMIT 1"), "{exists}");
    assert!(!exists.contains("ORDER BY"), "{exists}");
    let distinct = sql(&CountQuery::from_select(
        ordered().distinct().compile().unwrap(),
    ));
    assert!(!distinct.contains("ORDER BY"), "{distinct}");
    let paged = sql(&CountQuery::from_select(
        ordered().limit(2).compile().unwrap(),
    ));
    assert!(
        paged.contains("ORDER BY"),
        "a limit keeps its order: {paged}"
    );
    assert!(ordered()
        .filter("pages", 300_i64)
        .exists(pool)
        .await
        .unwrap());
    assert!(ordered()
        .filter("pages", 1_i64)
        .is_empty(pool)
        .await
        .unwrap());
    assert!(!ordered().offset(3).exists(pool).await.unwrap());
}

async fn count_honours_relation_span(pool: &Pool) {
    let ada = || Book::objects().filter("author__name", "Ada");
    assert_eq!(ada().count(pool).await.unwrap(), 2);
    assert!(ada().exists(pool).await.unwrap());
    let s: Option<i64> = ada().sum("pages", pool).await.unwrap();
    assert_eq!(s, Some(300));
}

async fn sum_keeps_float(pool: &Pool) {
    let s: Option<f64> = Book::objects().sum("price", pool).await.unwrap();
    assert_eq!(
        s,
        Some(14.25),
        "10.75 + 0.5 + 3.0 must not be cast to an integer"
    );
    let i: Option<i64> = Book::objects().sum("pages", pool).await.unwrap();
    assert_eq!(i, Some(600));
}

fn wide_rows(ids: impl Iterator<Item = i64>) -> Vec<Vec<SqlValue>> {
    ids.map(|id| (0..10).map(|_| SqlValue::I64(id)).collect())
        .collect()
}

fn wide_query(rows: Vec<Vec<SqlValue>>) -> BulkInsertQuery {
    BulkInsertQuery::new(
        Wide::SCHEMA,
        vec!["id", "c1", "c2", "c3", "c4", "c5", "c6", "c7", "c8", "c9"],
        rows,
    )
}

/// One row past what a single statement can bind.
fn two_batches(pool: &Pool) -> i64 {
    i64::try_from(pool.dialect().max_bind_params() / 10).unwrap() + 1
}

async fn bulk_insert_rolls_back_every_batch(pool: &Pool) {
    let n = two_batches(pool);
    // The last row repeats id 1, so only the second batch fails.
    let mut rows = wide_rows(1..n);
    rows.extend(wide_rows(std::iter::once(1)));
    let err = bulk_insert_pool(pool, &wide_query(rows)).await;
    assert!(err.is_err(), "duplicate pk must fail");
    assert_eq!(
        Wide::objects().count(pool).await.unwrap(),
        0,
        "the first batch must not stay committed"
    );
    bulk_insert_pool(pool, &wide_query(wide_rows(1..=n)))
        .await
        .expect("clean multi-batch insert");
    assert_eq!(Wide::objects().count(pool).await.unwrap(), n);
}

async fn bulk_insert_joins_outer_atomic(pool: &Pool) {
    for n in [3, two_batches(pool)] {
        bulk_insert_rolled_back_with_outer(pool, n).await;
    }
}

/// `n` rows inside an `atomic()` that then fails: one batch or several.
async fn bulk_insert_rolled_back_with_outer(pool: &Pool, n: i64) {
    let (p, q) = (pool.clone(), wide_query(wide_rows(1..=n)));
    let r: Result<(), ExecError> = atomic(pool, move |_tx| {
        Box::pin(async move {
            bulk_insert_pool(&p, &q).await?;
            Err(ExecError::AtomicAborted)
        })
    })
    .await;
    assert!(r.is_err());
    assert_eq!(
        Wide::objects().count(pool).await.unwrap(),
        0,
        "the outer rollback must undo a {n}-row bulk insert"
    );
}

/// Compile-only chain for the multi-hop alias test; never gets a table.
#[derive(Model, Debug, Clone)]
#[rustango(table = "occ_region")]
#[allow(dead_code)]
pub struct Region {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 40)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "occ_store")]
#[allow(dead_code)]
pub struct Store {
    #[rustango(primary_key)]
    pub id: i64,
    pub region: ForeignKey<Region>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "occ_sale")]
#[allow(dead_code)]
pub struct Sale {
    #[rustango(primary_key)]
    pub id: i64,
    pub store: ForeignKey<Store>,
}

/// `compile()` reuses one alias string per path instead of leaking a new one (#1889).
#[test]
fn span_alias_is_not_reallocated_per_compile() {
    let aliases = || -> Vec<&'static str> {
        let one = Book::objects().filter("author__name", "Ada").compile();
        let span = Sale::objects().filter("store__region__name", "x").compile();
        let related = Sale::objects().select_related("store__region").compile();
        [one, span, related]
            .into_iter()
            .flat_map(|q| q.unwrap().joins.into_iter().map(|j| j.alias))
            .collect()
    };
    let (a, b) = (aliases(), aliases());
    assert_eq!(
        a,
        ["author", "store", "store__region", "store", "store__region"]
    );
    for (x, y) in a.iter().zip(&b) {
        assert!(std::ptr::eq(*x, *y), "`{x}` was allocated again");
    }
}

/// Self-FK, so a caller can build paths of any depth; never gets a table.
#[derive(Model, Debug, Clone)]
#[rustango(table = "occ_node")]
#[allow(dead_code)]
pub struct Node {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(fk = "self", on = "id")]
    pub parent: Option<i64>,
}

/// A too-deep path fails only its own query; new paths keep compiling.
#[test]
fn deep_self_fk_path_is_refused_per_query() {
    let deep = ["parent"; 7].join("__") + "__id";
    let err = Node::objects().filter(&deep, 1_i64).compile().unwrap_err();
    assert!(
        matches!(
            err,
            rustango::core::QueryError::RelationPathTooDeep { max: 6, .. }
        ),
        "{err:?}"
    );
    let ok = ["parent"; 6].join("__") + "__id";
    assert_eq!(
        Node::objects()
            .filter(&ok, 1_i64)
            .compile()
            .unwrap()
            .joins
            .len(),
        6
    );
    Sale::objects()
        .filter("store__region__id", 1_i64)
        .compile()
        .unwrap();
}

tri_dialect_test! {
    setup: seeded,
    scenarios: [
        none_counts_nothing,
        count_honours_limit_and_offset,
        count_honours_compound,
        count_honours_relation_span,
        exists_reads_one_row_unordered,
        sum_keeps_float,
        bulk_insert_rolls_back_every_batch,
        bulk_insert_joins_outer_atomic,
    ],
}

/// Decimal `SUM` is not cast, so PG NUMERIC and MySQL DECIMAL stay exact.
/// A `Decimal` model does not build with the `sqlite` feature (SQLite has
/// no decimal type; a NUMERIC `SUM` there reads back as `f64` / `i64`).
#[cfg(not(feature = "sqlite"))]
mod decimal {
    use super::*;
    use rust_decimal::Decimal;

    #[derive(Model, Debug, Clone)]
    #[rustango(table = "occ_ledger")]
    #[allow(dead_code)]
    pub struct Ledger {
        #[rustango(primary_key)]
        pub id: i64,
        pub amount: Decimal,
    }

    async fn sum_keeps_decimal_exact(pool: &Pool) {
        for (id, amount) in [(1, "0.1"), (2, "0.2")] {
            Ledger {
                id,
                amount: amount.parse().unwrap(),
            }
            .insert_pool(pool)
            .await
            .expect("seed ledger");
        }
        let s: Option<Decimal> = Ledger::objects().sum("amount", pool).await.unwrap();
        assert_eq!(
            s,
            Some("0.3".parse::<Decimal>().unwrap()),
            "exact, not 0.30000000000000004"
        );
    }

    tri_dialect_test! {
        model: Ledger,
        scenarios: [sum_keeps_decimal_exact],
    }
}
