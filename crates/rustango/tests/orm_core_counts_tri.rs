//! `count` / `exists` / `sum` honour every queryset clause (#1885), on
//! every backend; join aliases are not leaked per `compile()` (#1889).

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::core::Model as _;
use rustango::sql::{CounterPool as _, ExistsPool as _, ForeignKey, Pool};
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

/// Ada: books 1 (10.75, 100 pages) and 2 (0.5, 200). Bob: book 3 (3.0, 300).
async fn seeded(pool: &Pool) {
    rustango::testkit::matrix::drop_table(pool, Book::SCHEMA.table).await;
    rustango::testkit::matrix::fresh_table::<Author>(pool).await;
    rustango::testkit::matrix::fresh_table::<Book>(pool).await;
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

async fn count_honours_relation_span(pool: &Pool) {
    let ada = || Book::objects().filter("author__name", "Ada");
    assert_eq!(ada().count(pool).await.unwrap(), 2);
    assert!(ada().exists(pool).await.unwrap());
    let s: Option<i64> = ada().sum("pages", pool).await.unwrap();
    assert_eq!(s, Some(300));
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

tri_dialect_test! {
    setup: seeded,
    scenarios: [
        none_counts_nothing,
        count_honours_limit_and_offset,
        count_honours_compound,
        count_honours_relation_span,
    ],
}
