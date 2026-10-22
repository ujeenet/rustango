//! `m2m_changed` fires only when `add` / `remove` / `set` / `clear` changed a junction row (#2221).

#![cfg(all(
    feature = "signals",
    any(feature = "postgres", feature = "mysql", feature = "sqlite")
))]

use std::sync::{Arc, Mutex};

use rustango::core::{FieldSchema, FieldType, IndexSchema, Model as _, ModelSchema, SqlValue};
use rustango::signals::m2m::{clear_all, connect_m2m_changed, M2mAction};
use rustango::sql::{Auto, GenericM2MManager, M2MManager, Pool};
use rustango::{tri_dialect_test, Model};

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

/// A registered through model with `INT` keys, not the junction's usual `BIGINT`.
#[derive(Model, Debug, Clone)]
#[rustango(table = "m2m_noop_post_tag_i32", app = "m2mnoop")]
#[allow(dead_code)]
pub struct PostTagI32 {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub post_id: i32,
    pub tag_id: i32,
}

/// The owner of the generic junctions below.
#[derive(Model, Debug, Clone)]
#[rustango(table = "m2m_noop_doc", app = "m2mnoop")]
#[allow(dead_code)]
pub struct Doc {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
}

/// An unregistered generic junction whose `tag_id` is `ty`.
macro_rules! generic_junction {
    ($table:literal, $ty:expr, $max:expr) => {
        &{
            const FIELDS: &[FieldSchema] = &[
                FieldSchema::new("taggable_id", "taggable_id", FieldType::I64),
                FieldSchema::new("taggable_type", "taggable_type", FieldType::I64),
                {
                    let mut f = FieldSchema::new("tag_id", "tag_id", $ty);
                    f.max_length = $max;
                    f
                },
            ];
            const INDEXES: &[IndexSchema] = &[{
                let mut i = IndexSchema::new(
                    concat!($table, "_key"),
                    &["taggable_id", "taggable_type", "tag_id"],
                );
                i.unique = true;
                i
            }];
            let mut s = ModelSchema::new($table, $table);
            s.fields = FIELDS;
            s.indexes = INDEXES;
            s
        }
    };
}

const GENERIC_INT: &ModelSchema = generic_junction!("m2m_noop_gen_int", FieldType::I64, None);
const GENERIC_I32: &ModelSchema = generic_junction!("m2m_noop_gen_i32", FieldType::I32, None);
const GENERIC_STR: &ModelSchema =
    generic_junction!("m2m_noop_gen_str", FieldType::String, Some(40));
const GENERIC_UUID: &ModelSchema = generic_junction!("m2m_noop_gen_uuid", FieldType::Uuid, None);

async fn setup(pool: &Pool) {
    let tables = [
        JUNCTION,
        PostTagI32::SCHEMA,
        Doc::SCHEMA,
        GENERIC_INT,
        GENERIC_I32,
        GENERIC_STR,
        GENERIC_UUID,
    ];
    for t in tables {
        rustango::testkit::matrix::drop_table(pool, t.table).await;
    }
    rustango::testkit::create_tables(pool, &tables)
        .await
        .expect("tables");
    // Another suite may have left this table with a different shape.
    let ct = rustango::contenttypes::ContentType::SCHEMA.table;
    rustango::testkit::matrix::drop_table(pool, ct).await;
    rustango::contenttypes::ensure_seeded(pool)
        .await
        .expect("content types");
    rustango::contenttypes::clear_cache();
}

fn tags(through: &'static ModelSchema) -> GenericM2MManager {
    GenericM2MManager {
        src_pk: SqlValue::I64(1),
        src_schema: Doc::SCHEMA,
        through: through.table,
        pk_col: "taggable_id",
        ct_col: "taggable_type",
        dst_col: "tag_id",
    }
}

fn actions(fired: &Fired) -> Vec<M2mAction> {
    let got = fired.lock().unwrap().iter().map(|(a, _)| *a).collect();
    clear_all();
    got
}

async fn set_on_an_i32_through_model(pool: &Pool) {
    let fired = record();
    let p = M2MManager {
        src_pk: SqlValue::I64(1),
        through: PostTagI32::SCHEMA.table,
        src_col: "post_id",
        dst_col: "tag_id",
    };
    p.set(&[5_i64, 6], pool).await.expect("set");
    p.set(&[6_i64, 5], pool).await.expect("same set");
    assert_eq!(actions(&fired), [M2mAction::Set]);
}

async fn generic_noop_set_and_clear_fire_nothing(pool: &Pool) {
    let fired = record();
    let t = tags(GENERIC_INT);
    t.set(&[5_i64, 6], pool).await.expect("set");
    t.set(&[6_i64, 5], pool).await.expect("same set");
    t.clear(pool).await.expect("clear");
    t.clear(pool).await.expect("clear again");
    assert_eq!(actions(&fired), [M2mAction::Set, M2mAction::Clear]);
}

async fn generic_set_with_string_keys(pool: &Pool) {
    let fired = record();
    let t = tags(GENERIC_STR);
    t.set(&["a", "b"], pool).await.expect("set");
    t.set(&["b", "a"], pool).await.expect("same set");
    t.set(&["b"], pool).await.expect("smaller set");
    assert_eq!(actions(&fired), [M2mAction::Set, M2mAction::Set]);
    assert_eq!(t.all_as::<String>(pool).await.expect("all"), ["b"]);
}

/// The guessed `i64` read fails on PG's `INT`; `set` still applies.
async fn generic_set_on_an_int_column(pool: &Pool) {
    let t = tags(GENERIC_I32);
    t.set(&[5_i64, 6], pool).await.expect("set");
    t.set(&[6_i64], pool).await.expect("smaller set");
    assert_eq!(t.all_as::<i32>(pool).await.expect("all"), [6]);
}

async fn generic_set_with_uuid_keys(pool: &Pool) {
    let fired = record();
    let t = tags(GENERIC_UUID);
    let (a, b) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    t.set(&[a, b], pool).await.expect("set");
    t.set(&[b, a], pool).await.expect("same set");
    t.set(&[b], pool).await.expect("smaller set");
    assert_eq!(actions(&fired), [M2mAction::Set, M2mAction::Set]);
    assert_eq!(t.all_as::<uuid::Uuid>(pool).await.expect("all"), [b]);
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

async fn noop_set_and_clear_fire_nothing(pool: &Pool) {
    let fired = record();
    let p = post(2);
    p.set(&[5_i64, 6], pool).await.expect("set");
    p.set(&[6_i64, 5], pool).await.expect("same set");
    p.set(&[6_i64], pool).await.expect("smaller set");
    p.clear(pool).await.expect("clear");
    p.clear(pool).await.expect("clear again");
    p.set::<i64>(&[], pool).await.expect("empty set on empty");
    let got: Vec<M2mAction> = fired.lock().unwrap().iter().map(|(a, _)| *a).collect();
    clear_all();
    assert_eq!(got, [M2mAction::Set, M2mAction::Set, M2mAction::Clear]);
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        noop_changes_fire_nothing,
        noop_set_and_clear_fire_nothing,
        set_on_an_i32_through_model,
        generic_noop_set_and_clear_fire_nothing,
        generic_set_with_string_keys,
        generic_set_on_an_int_column,
        generic_set_with_uuid_keys,
    ],
}
