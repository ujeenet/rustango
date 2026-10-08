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

/// A Rust-filled `Auto` declared before the DB-filled PK: MySQL's
/// `LAST_INSERT_ID()` belongs to the PK, not to the first `Auto` field.
#[derive(Model, Debug, Clone)]
#[rustango(table = "v71934_ticket", app = "v71934")]
pub struct Ticket {
    #[rustango(auto_now_add)]
    pub created_at: Auto<chrono::DateTime<chrono::Utc>>,
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub name: String,
}

/// A Rust-filled PK beside another Rust-filled `Auto`: nothing to read back.
#[derive(Model, Debug, Clone)]
#[rustango(table = "v71934_stamp", app = "v71934")]
pub struct Stamp {
    #[rustango(auto_now_add)]
    pub created_at: Auto<chrono::DateTime<chrono::Utc>>,
    #[rustango(primary_key, default_uuid_v7)]
    pub id: Auto<Uuid>,
}

/// DB-filled PK beside a Rust-filled timestamp: the RETURNING `insert_or_ignore`.
#[derive(Model, Debug, Clone)]
#[rustango(table = "v71934_badge", app = "v71934")]
pub struct Badge {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32, unique)]
    pub name: String,
    #[rustango(auto_now_add)]
    pub created_at: Auto<chrono::DateTime<chrono::Utc>>,
    #[rustango(auto_now)]
    pub updated_at: Auto<chrono::DateTime<chrono::Utc>>,
}

/// Never created, so every insert fails.
#[derive(Model, Debug, Clone)]
#[rustango(table = "v71934_missing", app = "v71934")]
pub struct Missing {
    #[rustango(primary_key, default_uuid_v7)]
    pub id: Auto<Uuid>,
    #[rustango(auto_now_add)]
    pub created_at: Auto<chrono::DateTime<chrono::Utc>>,
}

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Badge>(pool).await;
    rustango::testkit::matrix::fresh_table::<Ticket>(pool).await;
    rustango::testkit::matrix::fresh_table::<Stamp>(pool).await;
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

/// A skipped `insert_or_ignore` leaves no unsaved id behind (#1937).
async fn skipped_insert_or_ignore_keeps_the_pk_unset(pool: &Pool) {
    let mut first = item("dup", 1);
    assert!(first.insert_or_ignore(pool).await.expect("first insert"));
    assert!(first.id.get().is_some(), "inserted row keeps its id");
    let mut second = item("dup", 2);
    assert!(!second.insert_or_ignore(pool).await.expect("skip"));
    assert!(matches!(second.id, Auto::Unset), "{:?}", second.id);
}

/// The RETURNING variant and a failed insert reset the Rust-filled fields too (#1937).
async fn unsaved_insert_or_ignore_resets_rust_filled_fields(pool: &Pool) {
    let badge = |name: &str| Badge {
        id: Auto::Unset,
        name: name.into(),
        created_at: Auto::Unset,
        updated_at: Auto::Unset,
    };
    let mut first = badge("dup");
    assert!(first.insert_or_ignore(pool).await.expect("first insert"));
    assert!(
        first.created_at.get().is_some(),
        "inserted row keeps its stamp"
    );
    let mut second = badge("dup");
    assert!(!second.insert_or_ignore(pool).await.expect("skip"));
    assert!(
        matches!(second.created_at, Auto::Unset),
        "{:?}",
        second.created_at
    );
    assert!(
        matches!(second.updated_at, Auto::Unset),
        "{:?}",
        second.updated_at
    );

    let mut ghost = Missing {
        id: Auto::Unset,
        created_at: Auto::Unset,
    };
    assert!(ghost.insert_or_ignore(pool).await.is_err());
    assert!(matches!(ghost.id, Auto::Unset), "{:?}", ghost.id);
    assert!(
        matches!(ghost.created_at, Auto::Unset),
        "{:?}",
        ghost.created_at
    );
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
    if let Some(pg) = pool.as_postgres() {
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

async fn db_pk_is_read_back_beside_a_rust_filled_auto(pool: &Pool) {
    let mut t = Ticket {
        created_at: Auto::Unset,
        id: Auto::Unset,
        name: "a".into(),
    };
    t.insert_pool(pool).await.expect("insert_pool");
    let stored = Ticket::objects().fetch(pool).await.expect("fetch");
    assert_eq!(stored.len(), 1);
    assert_eq!(t.id.get(), stored[0].id.get(), "PK read back");
    assert!(t.created_at.get().is_some());

    let mut s = Stamp {
        created_at: Auto::Unset,
        id: Auto::Unset,
    };
    s.insert_pool(pool).await.expect("insert_pool");
    let id = *s.id.get().expect("id filled");
    let stored = Stamp::objects().fetch(pool).await.expect("fetch");
    assert_eq!(stored[0].id.get(), Some(&id));
}

/// A set PK beside an unset `auto_now_add` stamps the clock, not NULL (#1950).
async fn pg_bulk_insert_set_pk_fills_unset_timestamps(pool: &Pool) {
    #[cfg(feature = "postgres")]
    if let Some(pg) = pool.as_postgres() {
        let ticket = |id: i64| Ticket {
            created_at: Auto::Unset,
            id: Auto::Set(id),
            name: format!("t{id}"),
        };
        // One row sets its own stamp: a mixed set/unset batch keeps it.
        let at = chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("ts");
        let mut rows = vec![ticket(10), ticket(11), ticket(12)];
        rows[2].created_at = Auto::Set(at);
        Ticket::bulk_insert(&mut rows, pg)
            .await
            .expect("bulk_insert");
        let mut stored = Ticket::objects().fetch(pool).await.expect("fetch");
        stored.sort_by_key(|t| *t.id.get().expect("id"));
        let ids: Vec<i64> = stored.iter().map(|t| *t.id.get().unwrap()).collect();
        assert_eq!(ids, [10, 11, 12], "the given ids are stored");
        assert!(stored.iter().all(|t| t.created_at.get().is_some()));
        assert_eq!(stored[2].created_at.get(), Some(&at));
    }
    let _ = pool;
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        audited_insert_fills_the_pk,
        skipped_insert_or_ignore_keeps_the_pk_unset,
        unsaved_insert_or_ignore_resets_rust_filled_fields,
        bulk_writes_fill_the_pk,
        pg_bulk_insert_writes_ids_back,
        db_pk_is_read_back_beside_a_rust_filled_auto,
        pg_bulk_insert_set_pk_fills_unset_timestamps,
    ],
}
