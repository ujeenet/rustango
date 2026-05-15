//! v0.45 — live SQLite coverage for `get_or_create` and
//! `update_or_create` (Django-style atomic-ish helpers).
//!
//! Atomicity caveat: the helpers run SELECT then INSERT/UPDATE in
//! two statements; another writer could race between the two. For
//! race-free behaviour pair with `Pool::begin()` or rely on a
//! UNIQUE constraint. These tests run single-threaded so the race
//! window doesn't matter.

#![cfg(feature = "sqlite")]

use rustango::core::Column as _;
use rustango::sql::{get_or_create, update_or_create, Auto, ExecError, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "v045_widget")]
pub struct Widget {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub slug: String,
    #[rustango(max_length = 200)]
    pub title: String,
}

async fn pool_with_table() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite");
    rustango::sql::raw_execute_pool(
        &pool,
        "CREATE TABLE v045_widget (id INTEGER PRIMARY KEY AUTOINCREMENT, \
         slug TEXT NOT NULL UNIQUE, title TEXT NOT NULL)",
        vec![],
    )
    .await
    .unwrap();
    pool
}

#[tokio::test]
async fn get_or_create_inserts_when_filter_matches_zero() {
    let pool = pool_with_table().await;
    let (w, created) = get_or_create(
        Widget::objects().where_(Widget::slug.eq("alpha".to_owned())),
        |pool| async move {
            let mut w = Widget {
                id: Auto::Unset,
                slug: "alpha".into(),
                title: "Alpha".into(),
            };
            w.insert_pool(&pool).await?;
            Ok(w)
        },
        &pool,
    )
    .await
    .expect("get_or_create");
    assert!(created, "should have inserted a new row");
    assert_eq!(w.slug, "alpha");
    assert_eq!(w.title, "Alpha");
    assert!(matches!(w.id, Auto::Set(_)));
}

#[tokio::test]
async fn get_or_create_returns_existing_when_filter_matches_one() {
    let pool = pool_with_table().await;
    // Seed a row.
    let mut existing = Widget {
        id: Auto::Unset,
        slug: "beta".into(),
        title: "Beta original".into(),
    };
    existing.insert_pool(&pool).await.unwrap();
    let existing_id = existing.id;

    // Second call should find it instead of inserting.
    let (w, created) = get_or_create(
        Widget::objects().where_(Widget::slug.eq("beta".to_owned())),
        |_pool| async move {
            panic!("create_fn must not run when the filter matches");
        },
        &pool,
    )
    .await
    .expect("get_or_create");
    assert!(!created, "should NOT have inserted; row already existed");
    assert_eq!(w.slug, "beta");
    assert_eq!(w.title, "Beta original");
    assert_eq!(w.id, existing_id);
}

#[tokio::test]
async fn get_or_create_errors_when_filter_matches_multiple() {
    let pool = pool_with_table().await;
    // Two rows with the same prefix → filter on `title LIKE 'shared%'`
    // matches both.
    for slug in ["gamma1", "gamma2"] {
        let mut w = Widget {
            id: Auto::Unset,
            slug: slug.into(),
            title: "shared title".into(),
        };
        w.insert_pool(&pool).await.unwrap();
    }
    let err = get_or_create(
        Widget::objects().where_(Widget::title.eq("shared title".to_owned())),
        |_pool| async move {
            panic!("create_fn must not run on multi-match");
        },
        &pool,
    )
    .await
    .expect_err("should error on multiple rows");
    match err {
        ExecError::MultipleRowsReturned { op, table, count } => {
            assert_eq!(op, "get_or_create");
            assert_eq!(table, "v045_widget");
            assert_eq!(count, 2);
        }
        other => panic!("expected MultipleRowsReturned, got {other:?}"),
    }
}

#[tokio::test]
async fn update_or_create_updates_existing() {
    let pool = pool_with_table().await;
    let mut existing = Widget {
        id: Auto::Unset,
        slug: "delta".into(),
        title: "Old".into(),
    };
    existing.insert_pool(&pool).await.unwrap();

    let (w, created) = update_or_create(
        Widget::objects().where_(Widget::slug.eq("delta".to_owned())),
        |pool, mut existing| async move {
            existing.title = "New".into();
            existing.save_pool(&pool).await?;
            Ok(existing)
        },
        |_pool| async move {
            panic!("create_fn must not run when the filter matches");
        },
        &pool,
    )
    .await
    .expect("update_or_create");
    assert!(!created);
    assert_eq!(w.title, "New");
}

#[tokio::test]
async fn update_or_create_creates_when_filter_misses() {
    let pool = pool_with_table().await;
    let (w, created) = update_or_create(
        Widget::objects().where_(Widget::slug.eq("epsilon".to_owned())),
        |_pool, _existing| async move {
            panic!("update_fn must not run when there's no match");
        },
        |pool| async move {
            let mut w = Widget {
                id: Auto::Unset,
                slug: "epsilon".into(),
                title: "Created".into(),
            };
            w.insert_pool(&pool).await?;
            Ok(w)
        },
        &pool,
    )
    .await
    .expect("update_or_create");
    assert!(created);
    assert_eq!(w.slug, "epsilon");
    assert_eq!(w.title, "Created");
}
