//! `default_uuid_v7` PKs on audited inserts and bulk writes (#1934).

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::sql::{Auto, FetcherPool as _, Pool};
use rustango::{tri_dialect_test, Model};
use uuid::Uuid;

#[derive(Model, Debug, Clone)]
#[rustango(table = "v71934_item", app = "v71934")]
pub struct Item {
    #[rustango(primary_key, default_uuid_v7)]
    pub id: Auto<Uuid>,
    #[rustango(max_length = 32, unique)]
    pub name: String,
    pub score: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "v71934_note", app = "v71934", audit(track = "name"))]
pub struct Note {
    #[rustango(primary_key, default_uuid_v7)]
    pub id: Auto<Uuid>,
    #[rustango(max_length = 32)]
    pub name: String,
}

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Item>(pool).await;
    rustango::testkit::matrix::fresh_table::<Note>(pool).await;
    rustango::audit::ensure_table_pool(pool)
        .await
        .expect("audit table");
}

fn item(name: &str, score: i64) -> Item {
    Item {
        id: Auto::Unset,
        name: name.into(),
        score,
    }
}

async fn distinct_ids(pool: &Pool) -> usize {
    let mut ids: Vec<Uuid> = Item::objects()
        .fetch(pool)
        .await
        .expect("fetch")
        .into_iter()
        .map(|i| *i.id.get().expect("stored id"))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids.len()
}

async fn audited_insert_fills_the_pk(pool: &Pool) {
    let mut n = Note {
        id: Auto::Unset,
        name: "a".into(),
    };
    n.insert_pool(pool).await.expect("audited insert_pool");
    let id = *n.id.get().expect("id filled");
    let mut m = Note {
        id: Auto::Unset,
        name: "b".into(),
    };
    m.save_pool(pool).await.expect("audited save_pool");
    let entries = rustango::audit::fetch_for_entity_pool(pool, "v71934_note", &id.to_string())
        .await
        .expect("audit rows");
    assert_eq!(entries.len(), 1, "one audit row for the insert");
    assert_eq!(Note::objects().fetch(pool).await.expect("fetch").len(), 2);
}

async fn bulk_writes_fill_the_pk(pool: &Pool) {
    Item::bulk_insert_or_ignore_pool(&[item("a", 1), item("b", 2)], pool)
        .await
        .expect("bulk_insert_or_ignore_pool");
    Item::bulk_upsert_pool(&[item("b", 3), item("c", 4)], &["name"], &["score"], pool)
        .await
        .expect("bulk_upsert_pool");
    assert_eq!(distinct_ids(pool).await, 3);
}

/// The PG-only `bulk_insert_on` also writes the ids back into the rows.
async fn pg_bulk_insert_writes_ids_back(pool: &Pool) {
    #[cfg(feature = "postgres")]
    if let Pool::Postgres(pg) = pool {
        let mut rows = vec![item("x", 1), item("y", 2)];
        Item::bulk_insert(&mut rows, pg).await.expect("bulk_insert");
        let mut stored: Vec<Uuid> = Item::objects()
            .fetch(pool)
            .await
            .expect("fetch")
            .into_iter()
            .map(|i| *i.id.get().expect("id"))
            .collect();
        let mut local: Vec<Uuid> = rows.iter().map(|r| *r.id.get().expect("filled")).collect();
        stored.sort_unstable();
        local.sort_unstable();
        assert_eq!(stored, local);

        let mut notes = vec![Note {
            id: Auto::Unset,
            name: "n".into(),
        }];
        Note::bulk_insert(&mut notes, pg)
            .await
            .expect("audited bulk_insert");
        assert!(notes[0].id.get().is_some());
    }
    let _ = pool;
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        audited_insert_fills_the_pk,
        bulk_writes_fill_the_pk,
        pg_bulk_insert_writes_ids_back,
    ],
}
