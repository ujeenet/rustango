//! Relation edges on every backend: a `ForeignKey<T>` to a renamed PK column
//! and concurrent M2M `set`.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::core::{Model as _, Relation};
use rustango::sql::{Auto, FetcherPool as _, ForeignKey, Pool};
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(table = "edg_acct", app = "edg")]
pub struct Acct {
    #[rustango(primary_key, column = "acct_no")]
    pub id: i64,
    #[rustango(max_length = 20)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "edg_txn", app = "edg")]
pub struct Txn {
    #[rustango(primary_key)]
    pub id: i64,
    pub acct: ForeignKey<Acct>,
}

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "edg_album",
    app = "edg",
    m2m(
        name = "tags",
        to = "edg_tag",
        through = "edg_album_tag",
        src = "album_id",
        dst = "tag_id"
    )
)]
pub struct Album {
    #[rustango(primary_key)]
    pub id: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "edg_album_tag", app = "edg")]
pub struct AlbumTag {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub album_id: i64,
    pub tag_id: i64,
}

async fn setup(pool: &Pool) {
    use rustango::testkit::matrix::{drop_table, fresh_table};
    for t in [Txn::SCHEMA, AlbumTag::SCHEMA] {
        drop_table(pool, t.table).await;
    }
    fresh_table::<Album>(pool).await;
    fresh_table::<AlbumTag>(pool).await;
}

/// No `on`: the FK references the target's PK column, not `id`.
async fn fk_targets_a_renamed_pk_column(pool: &Pool) {
    rustango::testkit::matrix::drop_table(pool, Txn::SCHEMA.table).await;
    rustango::testkit::matrix::fresh_table::<Acct>(pool).await;
    let created = rustango::testkit::create_tables(pool, &[Txn::SCHEMA]).await;
    let field = Txn::SCHEMA.field("acct").expect("acct");
    assert!(
        matches!(field.relation, Some(Relation::Fk { on: "acct_no", .. })),
        "{:?} (create: {created:?})",
        field.relation
    );
    created.expect("REFERENCES the renamed PK column");
    Acct {
        id: 7,
        name: "seven".into(),
    }
    .insert_pool(pool)
    .await
    .expect("acct");
    Txn {
        id: 1,
        acct: ForeignKey::unloaded(7),
    }
    .insert_pool(pool)
    .await
    .expect("txn");
    let rows: Vec<Txn> = Txn::objects()
        .select_related("acct")
        .fetch(pool)
        .await
        .expect("select_related joins on the renamed column");
    assert_eq!(
        rows[0].acct.value().map(|a| a.name.as_str()),
        Some("seven")
    );
}

/// Two `set`s racing on one source: the result is one of them, never both.
async fn concurrent_m2m_sets_keep_one(pool: &Pool) {
    Album { id: 1 }.insert_pool(pool).await.expect("album");
    let album = Album { id: 1 };
    for round in 0..40 {
        album.tags_m2m().set(&[9_i64], pool).await.expect("reset");
        let barrier = tokio::sync::Barrier::new(2);
        let run = |ids: &'static [i64]| {
            let barrier = &barrier;
            async move {
                barrier.wait().await;
                Album { id: 1 }.tags_m2m().set(ids, pool).await
            }
        };
        let (a, b) = tokio::join!(run(&[1, 2]), run(&[3, 4]));
        a.unwrap_or_else(|e| panic!("round {round}: first set: {e}"));
        b.unwrap_or_else(|e| panic!("round {round}: second set: {e}"));
        let mut got = album.tags_m2m().all(pool).await.expect("all");
        got.sort_unstable();
        assert!(got == [1, 2] || got == [3, 4], "round {round}: {got:?}");
    }
}

tri_dialect_test! {
    setup: setup,
    scenarios: [fk_targets_a_renamed_pk_column, concurrent_m2m_sets_keep_one],
}
