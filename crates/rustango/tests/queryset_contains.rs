//! Django-parity #330 — `QuerySet.contains(obj)` + sibling
//! `.exists()` predicate. Verifies the boolean predicates work on
//! filtered + unfiltered querysets against a real SQLite pool.

#![cfg(all(feature = "sqlite", feature = "tenancy"))]

use rustango::core::Column as _;
use rustango::sql::{sqlx, Auto, ExistsPool as _, FetcherPool as _, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "qs_cont_widget")]
#[rustango(app = "qs_cont_app")]
#[allow(dead_code)]
pub struct Widget {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 50)]
    pub label: String,
    pub published: bool,
}

async fn fresh_pool() -> Pool {
    let sq = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite pool");
    sqlx::query(
        "CREATE TABLE qs_cont_widget (\
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            label TEXT NOT NULL, \
            published INTEGER NOT NULL)",
    )
    .execute(&sq)
    .await
    .expect("create");
    Pool::Sqlite(sq)
}

async fn seed(pool: &Pool) -> Vec<i64> {
    let mut ids = Vec::new();
    for (label, published) in [("a", true), ("b", false), ("c", true)] {
        let mut w = Widget {
            id: Auto::default(),
            label: label.into(),
            published,
        };
        w.insert_pool(pool).await.unwrap();
        ids.push(*w.id.get().expect("PK assigned"));
    }
    ids
}

// ----- exists

#[tokio::test]
async fn exists_true_when_any_match() {
    let pool = fresh_pool().await;
    seed(&pool).await;
    assert!(Widget::objects().exists(&pool).await.unwrap());
}

#[tokio::test]
async fn exists_false_on_empty_table() {
    let pool = fresh_pool().await;
    assert!(!Widget::objects().exists(&pool).await.unwrap());
}

#[tokio::test]
async fn exists_false_when_filter_excludes_all() {
    let pool = fresh_pool().await;
    seed(&pool).await;
    let any_z: bool = Widget::objects()
        .where_(Widget::label.eq("z".to_owned()))
        .exists(&pool)
        .await
        .unwrap();
    assert!(!any_z);
}

#[tokio::test]
async fn exists_honors_chained_filters() {
    let pool = fresh_pool().await;
    seed(&pool).await;
    // Both filters need to match — there ARE published rows, but
    // none labeled "b".
    let q = Widget::objects()
        .where_(Widget::published.eq(true))
        .where_(Widget::label.eq("b".to_owned()))
        .exists(&pool)
        .await
        .unwrap();
    assert!(!q);
}

// ----- contains_pk

#[tokio::test]
async fn contains_pk_true_for_seeded_id() {
    let pool = fresh_pool().await;
    let ids = seed(&pool).await;
    for id in ids {
        assert!(Widget::objects().contains_pk(&pool, id).await.unwrap());
    }
}

#[tokio::test]
async fn contains_pk_false_for_missing_id() {
    let pool = fresh_pool().await;
    seed(&pool).await;
    // 9999 is unallocated.
    let in_set = Widget::objects().contains_pk(&pool, 9999i64).await.unwrap();
    assert!(!in_set);
}

#[tokio::test]
async fn contains_pk_respects_prior_filters() {
    let pool = fresh_pool().await;
    let ids = seed(&pool).await;
    // ids[1] = "b" with published=false. Filter to published=true ⇒
    // contains_pk should be false even though the row exists.
    let in_set = Widget::objects()
        .where_(Widget::published.eq(true))
        .contains_pk(&pool, ids[1])
        .await
        .unwrap();
    assert!(!in_set);
    // ids[0] = "a" with published=true. Same filter ⇒ should match.
    let in_set = Widget::objects()
        .where_(Widget::published.eq(true))
        .contains_pk(&pool, ids[0])
        .await
        .unwrap();
    assert!(in_set);
}

#[tokio::test]
async fn contains_pk_after_delete_returns_false() {
    let pool = fresh_pool().await;
    let ids = seed(&pool).await;
    // Pull the row out, delete it via the typed method, then assert
    // contains_pk flips to false.
    let row: Widget = Widget::objects()
        .where_(Widget::id.eq(ids[0]))
        .fetch(&pool)
        .await
        .unwrap()
        .into_iter()
        .next()
        .expect("seeded row");
    row.delete_pool(&pool).await.unwrap();

    assert!(!Widget::objects().contains_pk(&pool, ids[0]).await.unwrap());
}
