//! Relation edges on every backend: a `ForeignKey<T>` to a renamed PK column.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::core::{Model as _, Relation};
use rustango::sql::{FetcherPool as _, ForeignKey, Pool};
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

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::drop_table(pool, Txn::SCHEMA.table).await;
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

tri_dialect_test! {
    setup: setup,
    scenarios: [fk_targets_a_renamed_pk_column],
}
