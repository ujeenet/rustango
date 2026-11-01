//! `m2m_changed` fires only when `add` / `remove` / `set` / `clear` changed a junction row (#2221),
//! and carries the generic owner's content type and the `set` diff.

#![cfg(all(
    feature = "signals",
    any(feature = "postgres", feature = "mysql", feature = "sqlite")
))]

use std::sync::{Arc, Mutex};

use rustango::core::{FieldSchema, FieldType, IndexSchema, Model as _, ModelSchema, SqlValue};
use rustango::signals::m2m::{clear_all, connect_m2m_changed, M2mAction, M2mChangedContext};
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

/// A junction whose only unique key is `post_id`: one tag per post.
const ONE_TAG: &ModelSchema = &{
    const FIELDS: &[FieldSchema] = &[
        FieldSchema::new("post_id", "post_id", FieldType::I64),
        FieldSchema::new("tag_id", "tag_id", FieldType::I64),
    ];
    const INDEXES: &[IndexSchema] = &[{
        let mut i = IndexSchema::new("m2m_noop_one_tag_post", &["post_id"]);
        i.unique = true;
        i
    }];
    let mut s = ModelSchema::new("OneTag", "m2m_noop_one_tag");
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

/// A source whose auto junction has a unique `(post_id, tag_id)` key.
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "m2m_lock_post",
    app = "m2mnoop",
    m2m(
        name = "tags",
        to = "m2m_lock_tag",
        through = "m2m_lock_post_tags",
        src = "post_id",
        dst = "tag_id"
    )
)]
pub struct LockPost {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub n: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "m2m_lock_tag", app = "m2mnoop")]
pub struct LockTag {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub n: i64,
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
        LockPost::SCHEMA,
        LockTag::SCHEMA,
        JUNCTION,
        ONE_TAG,
        PostTagI32::SCHEMA,
        Doc::SCHEMA,
        GENERIC_INT,
        GENERIC_I32,
        GENERIC_STR,
        GENERIC_UUID,
    ];
    rustango::testkit::matrix::drop_table(pool, "m2m_lock_post_tags").await;
    for t in tables {
        rustango::testkit::matrix::drop_table(pool, t.table).await;
    }
    // Also makes the LockPost <-> LockTag junction: both ends are listed.
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
    let (_signals, fired) = record_contexts().await;
    let p = M2MManager {
        src_pk: SqlValue::I64(1),
        through: PostTagI32::SCHEMA.table,
        src_col: "post_id",
        dst_col: "tag_id",
    };
    p.set(&[5_i64, 6], pool).await.expect("set");
    p.set(&[6_i64, 5], pool).await.expect("same set");
    // The keyless through model can hold one link twice: `removed` lists it once.
    for _ in 0..2 {
        PostTagI32 {
            id: Auto::default(),
            post_id: 1,
            tag_id: 7,
        }
        .insert_pool(pool)
        .await
        .expect("duplicate link");
    }
    p.set(&[5_i64, 6], pool).await.expect("drop the duplicate");
    let i = SqlValue::I64;
    assert_eq!(
        diffs(&fired),
        vec![(None, vec![i(5), i(6)], vec![]), (None, vec![], vec![i(7)])]
    );
}

/// `add` skips a present link even when the through model has no unique key.
async fn add_twice_on_a_keyless_through_model(pool: &Pool) {
    let (_signals, fired) = record().await;
    let p = M2MManager {
        src_pk: SqlValue::I64(1),
        through: PostTagI32::SCHEMA.table,
        src_col: "post_id",
        dst_col: "tag_id",
    };
    p.add(5_i64, pool).await.expect("add");
    p.add(5_i64, pool).await.expect("duplicate add");
    assert_eq!(actions(&fired), [M2mAction::Add]);
    assert_eq!(p.all::<i32>(pool).await.expect("all"), [5]);
}

/// A unique key the link check does not cover skips the insert: no `Add`.
async fn add_skipped_by_another_unique_key(pool: &Pool) {
    let (_signals, fired) = record().await;
    let p = M2MManager {
        src_pk: SqlValue::I64(1),
        through: ONE_TAG.table,
        src_col: "post_id",
        dst_col: "tag_id",
    };
    p.add(5_i64, pool).await.expect("add");
    p.add(6_i64, pool)
        .await
        .expect("add the unique post_id skips");
    assert_eq!(actions(&fired), [M2mAction::Add]);
    assert_eq!(p.all::<i64>(pool).await.expect("all"), [5]);
}

/// `add` on a keyed junction is one statement and takes no source lock, so
/// it does not wait on a caller's transaction that updated the source.
async fn add_inside_atomic_that_updated_the_source(pool: &Pool) {
    let _signals = signals_lock().await;
    LockPost {
        id: Auto::Set(1),
        n: 0,
    }
    .insert_pool(pool)
    .await
    .expect("post");
    for id in [7, 8] {
        LockTag {
            id: Auto::Set(id),
            n: 0,
        }
        .insert_pool(pool)
        .await
        .expect("tag");
    }
    let tags = || M2MManager {
        src_pk: SqlValue::I64(1),
        through: "m2m_lock_post_tags",
        src_col: "post_id",
        dst_col: "tag_id",
    };
    let statements = rustango::test_assertions::QueryCounter::scope(async {
        tags().add(7_i64, pool).await.expect("add");
        rustango::test_assertions::QueryCounter::current()
    })
    .await;
    assert_eq!(statements, 1, "add on a keyed junction is one insert");
    // MySQL's FK check and SQLite's write lock wait on any second
    // connection, so only PG can show the self-lock.
    if pool.dialect().name() != "postgres" {
        return;
    }
    let (p, late) = (pool.clone(), tags());
    rustango::atomic!(pool, |tx| {
        LockPost::objects()
            .filter("id", 1_i64)
            .update()
            .set("n", 1_i64)
            .execute_on(&mut *tx.lock().await?)
            .await?;
        tokio::time::timeout(std::time::Duration::from_secs(5), late.add(8_i64, &p))
            .await
            .expect("add waited on the caller's row")?;
        Ok(())
    })
    .await
    .expect("atomic");
    assert_eq!(tags().all::<i64>(pool).await.expect("all"), vec![7, 8]);
}

async fn generic_noop_set_and_clear_fire_nothing(pool: &Pool) {
    let (_signals, fired) = record().await;
    let t = tags(GENERIC_INT);
    t.set(&[5_i64, 6], pool).await.expect("set");
    t.set(&[6_i64, 5], pool).await.expect("same set");
    t.clear(pool).await.expect("clear");
    t.clear(pool).await.expect("clear again");
    assert_eq!(actions(&fired), [M2mAction::Set, M2mAction::Clear]);
}

async fn generic_set_with_string_keys(pool: &Pool) {
    let (_signals, fired) = record().await;
    let t = tags(GENERIC_STR);
    t.set(&["a", "b"], pool).await.expect("set");
    t.set(&["b", "a"], pool).await.expect("same set");
    t.set(&["b"], pool).await.expect("smaller set");
    assert_eq!(actions(&fired), [M2mAction::Set, M2mAction::Set]);
    assert_eq!(t.all::<String>(pool).await.expect("all"), ["b"]);
}

/// The guessed `i64` read fails on PG's `INT`; `set` still applies.
async fn generic_set_on_an_int_column(pool: &Pool) {
    let (_signals, fired) = record_contexts().await;
    let t = tags(GENERIC_I32);
    t.set(&[5_i64, 6], pool).await.expect("set");
    t.set(&[6_i64], pool).await.expect("smaller set");
    assert_eq!(t.all::<i32>(pool).await.expect("all"), [6]);
    // Where the old keys are unreadable (PG) the diff is empty, never a re-added key.
    let got = diffs(&fired);
    assert_eq!(got.len(), 2);
    assert!(got[1].1.is_empty(), "nothing was added: {got:?}");
    assert!(
        got[1].2.is_empty() || got[1].2 == [SqlValue::I64(5)],
        "{got:?}"
    );
}

async fn generic_set_with_uuid_keys(pool: &Pool) {
    let (_signals, fired) = record_contexts().await;
    let t = tags(GENERIC_UUID);
    let (a, b) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    t.set(&[a, b], pool).await.expect("set");
    t.set(&[b, a], pool).await.expect("same set");
    t.set(&[b], pool).await.expect("smaller set");
    assert_eq!(t.all::<uuid::Uuid>(pool).await.expect("all"), [b]);
    let got: Vec<_> = diffs(&fired).into_iter().map(|(_, a, r)| (a, r)).collect();
    let u = SqlValue::Uuid;
    assert_eq!(got, vec![(vec![u(a), u(b)], vec![]), (vec![], vec![u(a)])]);
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

/// Serializes the global signal registry: the SQLite arm takes no harness
/// lock, and `cargo test` runs scenarios on threads of one process.
async fn signals_lock() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    LOCK.lock().await
}

/// Record every `m2m_changed`, holding [`signals_lock`] while the guard lives.
async fn record() -> (tokio::sync::MutexGuard<'static, ()>, Fired) {
    let guard = signals_lock().await;
    clear_all();
    let fired: Fired = Arc::default();
    let sink = fired.clone();
    connect_m2m_changed(move |ctx| {
        sink.lock().unwrap().push((ctx.action, ctx.dst_pks.clone()));
        async {}
    });
    (guard, fired)
}

async fn noop_changes_fire_nothing(pool: &Pool) {
    let (_signals, fired) = record().await;
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
    assert_eq!(p.all::<i64>(pool).await.expect("all"), vec![6]);
}

async fn noop_set_and_clear_fire_nothing(pool: &Pool) {
    let (_signals, fired) = record().await;
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

/// Record every full `m2m_changed` context, like [`record`].
async fn record_contexts() -> (
    tokio::sync::MutexGuard<'static, ()>,
    Arc<Mutex<Vec<M2mChangedContext>>>,
) {
    let guard = signals_lock().await;
    clear_all();
    let fired: Arc<Mutex<Vec<M2mChangedContext>>> = Arc::default();
    let sink = fired.clone();
    connect_m2m_changed(move |ctx| {
        sink.lock().unwrap().push(ctx);
        async {}
    });
    (guard, fired)
}

/// `(content_type_id, added, removed)` of each recorded context.
fn diffs(
    fired: &Mutex<Vec<M2mChangedContext>>,
) -> Vec<(Option<i64>, Vec<SqlValue>, Vec<SqlValue>)> {
    let got = fired
        .lock()
        .unwrap()
        .iter()
        .map(|c| (c.content_type_id, c.added.clone(), c.removed.clone()))
        .collect();
    clear_all();
    got
}

/// `set` reports which keys it linked and unlinked; a plain M2M has no content type.
async fn set_reports_added_and_removed(pool: &Pool) {
    let (_signals, fired) = record_contexts().await;
    let p = post(3);
    p.set(&[5_i64, 6], pool).await.expect("set");
    p.set(&[6_i64, 7], pool).await.expect("swap one");
    p.add(8_i64, pool).await.expect("add");
    p.remove(6_i64, pool).await.expect("remove");
    p.clear(pool).await.expect("clear");
    let i = SqlValue::I64;
    assert_eq!(
        diffs(&fired),
        vec![
            (None, vec![i(5), i(6)], vec![]),
            (None, vec![i(7)], vec![i(5)]),
            (None, vec![i(8)], vec![]),
            (None, vec![], vec![i(6)]),
            (None, vec![], vec![]),
        ]
    );
}

/// A generic M2M carries its owner's content type, and `set` its diff.
async fn generic_signal_carries_content_type_and_diff(pool: &Pool) {
    let ct = rustango::contenttypes::ContentType::get_for_schema(pool, Doc::SCHEMA)
        .await
        .expect("content type")
        .and_then(|c| c.id.get().copied());
    assert!(ct.is_some());
    let (_signals, fired) = record_contexts().await;
    let t = tags(GENERIC_STR);
    t.set(&["a", "b"], pool).await.expect("set");
    t.set(&["b", "c"], pool).await.expect("swap one");
    t.clear(pool).await.expect("clear");
    let s = |v: &str| SqlValue::String(v.to_owned());
    assert_eq!(
        diffs(&fired),
        vec![
            (ct, vec![s("a"), s("b")], vec![]),
            (ct, vec![s("c")], vec![s("a")]),
            (ct, vec![], vec![]),
        ]
    );
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        noop_changes_fire_nothing,
        noop_set_and_clear_fire_nothing,
        set_on_an_i32_through_model,
        add_twice_on_a_keyless_through_model,
        add_skipped_by_another_unique_key,
        add_inside_atomic_that_updated_the_source,
        generic_noop_set_and_clear_fire_nothing,
        generic_set_with_string_keys,
        generic_set_on_an_int_column,
        generic_set_with_uuid_keys,
        set_reports_added_and_removed,
        generic_signal_carries_content_type_and_diff,
    ],
}
