//! `save()` writes what `self` holds: a soft delete sticks, and a missing
//! row is an error rather than a silent `Ok(0)`.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use chrono::{DateTime, Utc};
use rustango::sql::{Auto, CounterPool as _, ExecError, FetcherPool as _, Pool};
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(table = "oss_entry", app = "orm_save_state")]
pub struct Entry {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub title: String,
    #[rustango(soft_delete)]
    pub deleted_at: Option<DateTime<Utc>>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "oss_slug", app = "orm_save_state")]
pub struct Slug {
    #[rustango(primary_key, max_length = 32)]
    pub code: String,
    #[rustango(max_length = 64)]
    pub title: String,
}

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Entry>(pool).await;
    rustango::testkit::matrix::fresh_table::<Slug>(pool).await;
}

async fn new_entry(pool: &Pool, title: &str) -> Entry {
    let mut e = Entry {
        id: Auto::Unset,
        title: title.into(),
        deleted_at: None,
    };
    e.insert(pool).await.expect("insert entry");
    e
}

async fn stored(pool: &Pool, e: &Entry) -> Entry {
    let id = e.id.get().copied().expect("id");
    let rows: Vec<Entry> = Entry::objects().filter("id", id).fetch(pool).await.unwrap();
    rows.into_iter().next().expect("row")
}

fn not_found(r: Result<u64, ExecError>) {
    assert!(
        matches!(r, Err(ExecError::Driver(rustango::sql::sqlx::Error::RowNotFound))),
        "{r:?}"
    );
}

/// `soft_delete` / `restore` update `self`, so a later `save` keeps them.
async fn soft_delete_survives_a_later_save(pool: &Pool) {
    let mut e = new_entry(pool, "a").await;
    let _other = new_entry(pool, "b").await;
    assert_eq!(e.soft_delete(pool).await.unwrap(), 1);
    assert!(e.trashed(), "self still reads live");
    e.title = "a2".into();
    e.save(pool).await.unwrap();
    let row = stored(pool, &e).await;
    assert_eq!(row.title, "a2");
    assert!(row.deleted_at.is_some(), "save undid the soft delete");

    assert_eq!(e.restore(pool).await.unwrap(), 1);
    assert!(!e.trashed(), "self still reads trashed");
    e.title = "a3".into();
    e.save(pool).await.unwrap();
    assert!(
        stored(pool, &e).await.deleted_at.is_none(),
        "save undid the restore"
    );
}

/// A set PK with no row is an error, not a silent `Ok(0)`.
async fn save_of_a_missing_row_is_not_found(pool: &Pool) {
    Slug {
        code: "kept".into(),
        title: "Kept".into(),
    }
    .insert(pool)
    .await
    .unwrap();
    let mut ghost = Slug {
        code: "ghost".into(),
        title: "Ghost".into(),
    };
    not_found(ghost.save(pool).await);
    let mut tx = rustango::sql::transaction_pool(pool).await.unwrap();
    not_found(ghost.save_tx(&mut tx).await);
    tx.rollback().await.unwrap();
    assert_eq!(Slug::objects().count(pool).await.unwrap(), 1);

    let mut gone = new_entry(pool, "gone").await;
    gone.delete(pool).await.unwrap();
    not_found(gone.save(pool).await);
    not_found(gone.save_on(pool).await);
    assert_eq!(Entry::objects().count(pool).await.unwrap(), 0);
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        soft_delete_survives_a_later_save,
        save_of_a_missing_row_is_not_found,
    ],
}
