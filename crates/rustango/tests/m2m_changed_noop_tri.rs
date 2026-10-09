//! `m2m_changed` fires only when `add` / `remove` changed a junction row (#2221).

#![cfg(all(
    feature = "signals",
    any(feature = "postgres", feature = "mysql", feature = "sqlite")
))]

use std::sync::{Arc, Mutex};

use rustango::core::{FieldSchema, FieldType, IndexSchema, ModelSchema, SqlValue};
use rustango::signals::m2m::{clear_all, connect_m2m_changed, M2mAction};
use rustango::sql::{M2MManager, Pool};
use rustango::tri_dialect_test;

/// A junction as the migration writer makes it: a unique `(src, dst)` pair, no PK.
const JUNCTION: &ModelSchema = &{
    const FIELDS: &[FieldSchema] = &[
        FieldSchema::new("post_id", "post_id", FieldType::I64),
        FieldSchema::new("tag_id", "tag_id", FieldType::I64),
    ];
    const INDEXES: &[IndexSchema] = &[{
        let mut i = IndexSchema::new("m2m2221_post_tag_pair", &["post_id", "tag_id"]);
        i.unique = true;
        i
    }];
    let mut s = ModelSchema::new("PostTag2221", "m2m2221_post_tag");
    s.fields = FIELDS;
    s.indexes = INDEXES;
    s
};

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::drop_table(pool, JUNCTION.table).await;
    rustango::testkit::create_tables(pool, &[JUNCTION])
        .await
        .expect("junction");
}

fn post(id: i64) -> M2MManager {
    M2MManager {
        src_pk: SqlValue::I64(id),
        through: JUNCTION.table,
        src_col: "post_id",
        dst_col: "tag_id",
    }
}

type Fired = Arc<Mutex<Vec<(M2mAction, Vec<SqlValue>)>>>;

/// Record every `m2m_changed`; the harness lock serializes the global registry.
fn record() -> Fired {
    clear_all();
    let fired: Fired = Arc::default();
    let sink = fired.clone();
    connect_m2m_changed(move |ctx| {
        sink.lock().unwrap().push((ctx.action, ctx.dst_pks.clone()));
        async {}
    });
    fired
}

async fn noop_changes_fire_nothing(pool: &Pool) {
    let fired = record();
    let p = post(1);
    p.add(5, pool).await.expect("add");
    p.add(5, pool).await.expect("duplicate add");
    // A real insert after a skipped one still reports the row (MySQL insert id).
    p.add(6, pool).await.expect("add");
    p.remove(7, pool).await.expect("remove missing");
    p.remove(5, pool).await.expect("remove");
    p.remove(5, pool).await.expect("remove again");
    let got = fired.lock().unwrap().clone();
    clear_all();
    assert_eq!(
        got,
        vec![
            (M2mAction::Add, vec![SqlValue::I64(5)]),
            (M2mAction::Add, vec![SqlValue::I64(6)]),
            (M2mAction::Remove, vec![SqlValue::I64(5)]),
        ]
    );
    assert_eq!(p.all(pool).await.expect("all"), vec![6]);
}

tri_dialect_test! {
    setup: setup,
    scenarios: [noop_changes_fire_nothing],
}
