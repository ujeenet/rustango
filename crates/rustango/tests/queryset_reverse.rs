//! Django-parity #325 — `QuerySet::reverse()`. Flip the direction of
//! every `ORDER BY` entry pending on the queryset.

#![cfg(all(feature = "sqlite", feature = "tenancy"))]

use rustango::sql::{sqlx, Auto, FetcherPool as _, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "qs_rev_widget")]
#[rustango(app = "qs_rev_app")]
#[allow(dead_code)]
pub struct Widget {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 50)]
    pub label: String,
    pub rating: i32,
}

async fn fresh_pool() -> Pool {
    let sq = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite pool");
    sqlx::query(
        "CREATE TABLE qs_rev_widget (\
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            label TEXT NOT NULL, \
            rating INTEGER NOT NULL)",
    )
    .execute(&sq)
    .await
    .expect("create");
    Pool::Sqlite(sq)
}

async fn seed(pool: &Pool) {
    for (label, rating) in [("a", 3), ("b", 1), ("c", 2)] {
        let mut w = Widget {
            id: Auto::default(),
            label: label.into(),
            rating,
        };
        w.insert_pool(pool).await.unwrap();
    }
}

#[tokio::test]
async fn reverse_flips_single_column_ascending_to_descending() {
    let pool = fresh_pool().await;
    seed(&pool).await;

    // ASC by rating → [b(1), c(2), a(3)]
    let asc: Vec<Widget> = Widget::objects()
        .order_by(&[("rating", false)])
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(
        asc.iter().map(|w| w.label.as_str()).collect::<Vec<_>>(),
        vec!["b", "c", "a"]
    );

    // .reverse() should give the same shape with DESC → [a(3), c(2), b(1)]
    let rev: Vec<Widget> = Widget::objects()
        .order_by(&[("rating", false)])
        .reverse()
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(
        rev.iter().map(|w| w.label.as_str()).collect::<Vec<_>>(),
        vec!["a", "c", "b"]
    );
}

#[tokio::test]
async fn reverse_flips_descending_to_ascending() {
    let pool = fresh_pool().await;
    seed(&pool).await;

    // DESC → [a, c, b]; .reverse() → [b, c, a]
    let rev: Vec<Widget> = Widget::objects()
        .order_by(&[("rating", true)])
        .reverse()
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(
        rev.iter().map(|w| w.label.as_str()).collect::<Vec<_>>(),
        vec!["b", "c", "a"]
    );
}

#[tokio::test]
async fn reverse_flips_each_field_of_a_multi_column_sort() {
    let pool = fresh_pool().await;
    // Two rows with the same primary sort, different tiebreaker.
    let mut w1 = Widget {
        id: Auto::default(),
        label: "a".into(),
        rating: 1,
    };
    w1.insert_pool(&pool).await.unwrap();
    let mut w2 = Widget {
        id: Auto::default(),
        label: "b".into(),
        rating: 1,
    };
    w2.insert_pool(&pool).await.unwrap();
    let mut w3 = Widget {
        id: Auto::default(),
        label: "c".into(),
        rating: 2,
    };
    w3.insert_pool(&pool).await.unwrap();

    // ORDER BY rating ASC, label ASC → [(1,a), (1,b), (2,c)]
    let asc: Vec<Widget> = Widget::objects()
        .order_by(&[("rating", false), ("label", false)])
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(
        asc.iter().map(|w| w.label.as_str()).collect::<Vec<_>>(),
        vec!["a", "b", "c"]
    );

    // .reverse() → ORDER BY rating DESC, label DESC → [(2,c), (1,b), (1,a)]
    let rev: Vec<Widget> = Widget::objects()
        .order_by(&[("rating", false), ("label", false)])
        .reverse()
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(
        rev.iter().map(|w| w.label.as_str()).collect::<Vec<_>>(),
        vec!["c", "b", "a"]
    );
}

#[tokio::test]
async fn reverse_no_op_when_no_ordering_set() {
    let pool = fresh_pool().await;
    seed(&pool).await;

    // No order_by — .reverse() must not error and must return all rows.
    let rows: Vec<Widget> = Widget::objects().reverse().fetch(&pool).await.unwrap();
    assert_eq!(rows.len(), 3);
}

#[tokio::test]
async fn double_reverse_returns_to_original() {
    let pool = fresh_pool().await;
    seed(&pool).await;

    let original: Vec<Widget> = Widget::objects()
        .order_by(&[("rating", false)])
        .fetch(&pool)
        .await
        .unwrap();
    let twice_reversed: Vec<Widget> = Widget::objects()
        .order_by(&[("rating", false)])
        .reverse()
        .reverse()
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(
        original.iter().map(|w| &w.label).collect::<Vec<_>>(),
        twice_reversed.iter().map(|w| &w.label).collect::<Vec<_>>(),
    );
}
