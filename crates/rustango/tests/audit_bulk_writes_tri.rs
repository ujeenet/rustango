//! Bulk writes on audited models write their audit rows in the write's
//! transaction: one per affected row, or one bulk entry for `truncate`
//! (#1747).

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::audit::{self, AuditLog};
use rustango::core::{BulkInsertQuery, SqlValue};
use rustango::sql::{Auto, CounterPool as _, ExecError, FetcherPool as _, Pool, UpdaterPool as _};
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "audit1747_item",
    app = "audit1747",
    audit(track = "name, score")
)]
#[allow(dead_code)]
pub struct Item {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub name: String,
    pub score: i64,
}

/// Natural-PK model: exercises the non-`Auto` bulk insert.
#[derive(Model, Debug, Clone)]
#[rustango(table = "audit1747_tag", app = "audit1747", audit(track = "label"))]
#[allow(dead_code)]
pub struct Tag {
    #[rustango(primary_key, max_length = 32)]
    pub slug: String,
    #[rustango(max_length = 64, unique)]
    pub label: String,
}

/// `Auto` PK with a composite unique: `upsert` targets `(shop, code)`.
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "audit1795_sku",
    app = "audit1747",
    unique_together = "shop, code",
    audit(track = "qty")
)]
#[allow(dead_code)]
pub struct Sku {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 16)]
    pub shop: String,
    #[rustango(max_length = 16)]
    pub code: String,
    pub qty: i64,
}

impl rustango::prunable::Prunable for Item {
    fn prune_queryset() -> rustango::query::QuerySet<Self> {
        Item::objects().filter("name", "b")
    }
}
rustango::register_prunable!(Item);

const ITEM: &str = "audit1747_item";
const TAG: &str = "audit1747_tag";
const SKU: &str = "audit1795_sku";

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Item>(pool).await;
    rustango::testkit::matrix::fresh_table::<Tag>(pool).await;
    rustango::testkit::matrix::fresh_table::<Sku>(pool).await;
    audit::ensure_table_pool(pool).await.expect("audit table");
    for table in [ITEM, TAG, SKU] {
        AuditLog::delete_where("entity_table", table, pool)
            .await
            .expect("clear audit rows");
    }
}

async fn ops(pool: &Pool, table: &str, operation: &str) -> i64 {
    AuditLog::objects()
        .filter("entity_table", table)
        .filter("operation", operation)
        .count(pool)
        .await
        .expect("count audit rows")
}

/// The newest entry for one row.
async fn latest(pool: &Pool, table: &str, pk: &str) -> audit::AuditEntry {
    audit::fetch_for_entity_pool(pool, table, pk)
        .await
        .expect("fetch audit rows")
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("no audit row for {table}/{pk}"))
}

/// Three items: `a` (1), `b` (2), `b` (3). Returns their PKs.
async fn seed(pool: &Pool) -> Vec<i64> {
    let mut pks = Vec::new();
    for (name, score) in [("a", 1), ("b", 2), ("b", 3)] {
        let mut item = Item {
            id: Auto::default(),
            name: name.into(),
            score,
        };
        item.insert_pool(pool).await.expect("insert");
        pks.push(*item.id.get().expect("pk assigned"));
    }
    assert_eq!(ops(pool, ITEM, "create").await, 3);
    pks
}

async fn destroy_audits_each_deleted_row(pool: &Pool) {
    let pks = seed(pool).await;
    assert_eq!(Item::destroy([pks[0], pks[2]], pool).await.unwrap(), 2);
    assert_eq!(ops(pool, ITEM, "delete").await, 2);
    let entry = latest(pool, ITEM, &pks[2].to_string()).await;
    assert_eq!(entry.operation, "delete");
    assert_eq!(entry.changes["score"], 3);
}

async fn delete_where_audits_each_deleted_row(pool: &Pool) {
    let pks = seed(pool).await;
    assert_eq!(Item::delete_where("name", "b", pool).await.unwrap(), 2);
    assert_eq!(ops(pool, ITEM, "delete").await, 2);
    assert_eq!(
        latest(pool, ITEM, &pks[1].to_string()).await.operation,
        "delete"
    );
    assert_eq!(
        latest(pool, ITEM, &pks[0].to_string()).await.operation,
        "create"
    );
}

async fn prune_audits_each_deleted_row(pool: &Pool) {
    let pks = seed(pool).await;
    let opts = rustango::prunable::PruneOptions {
        only: vec![ITEM.to_owned()],
        ..Default::default()
    };
    let report = rustango::prunable::prune_all(pool, &opts).await.unwrap();
    assert_eq!(report[0].rows, 2);
    assert_eq!(ops(pool, ITEM, "delete").await, 2);
    assert_eq!(
        latest(pool, ITEM, &pks[2].to_string()).await.operation,
        "delete"
    );
}

async fn update_where_audits_the_written_values(pool: &Pool) {
    let pks = seed(pool).await;
    assert_eq!(
        Item::update_where("name", "b", "score", 7_i64, pool)
            .await
            .unwrap(),
        2
    );
    assert_eq!(ops(pool, ITEM, "update").await, 2);
    let entry = latest(pool, ITEM, &pks[1].to_string()).await;
    assert_eq!(entry.operation, "update");
    assert_eq!(entry.changes["score"], 7);
}

async fn update_all_audits_every_row(pool: &Pool) {
    seed(pool).await;
    assert_eq!(Item::update_all("name", "z", pool).await.unwrap(), 3);
    assert_eq!(ops(pool, ITEM, "update").await, 3);
}

async fn increment_each_audits_the_new_values(pool: &Pool) {
    let pks = seed(pool).await;
    assert_eq!(Item::increment_each("score", 5, pool).await.unwrap(), 3);
    assert_eq!(ops(pool, ITEM, "update").await, 3);
    assert_eq!(
        latest(pool, ITEM, &pks[0].to_string()).await.changes["score"],
        6
    );
}

async fn truncate_writes_one_bulk_entry(pool: &Pool) {
    seed(pool).await;
    Item::truncate(pool).await.unwrap();
    assert_eq!(ops(pool, ITEM, "delete").await, 1);
    let entry = latest(pool, ITEM, "").await;
    assert_eq!(entry.changes["bulk"], "truncate");
}

/// A failed bulk write commits neither the data nor any audit row.
async fn failed_bulk_write_writes_no_audit_row(pool: &Pool) {
    for slug in ["a", "b"] {
        Tag {
            slug: slug.into(),
            label: slug.into(),
        }
        .insert_pool(pool)
        .await
        .expect("insert");
    }
    // Both rows to one unique label: a violation on every backend.
    let err = Tag::update_all("label", "dup", pool).await.unwrap_err();
    assert!(matches!(err, ExecError::Driver(_)), "{err}");
    assert_eq!(ops(pool, TAG, "update").await, 0);
    assert_eq!(
        Tag::objects()
            .filter("label", "a")
            .count(pool)
            .await
            .unwrap(),
        1
    );
}

async fn bulk_update_audits_each_row(pool: &Pool) {
    let pks = seed(pool).await;
    let mut items = Item::objects().fetch(pool).await.unwrap();
    for item in &mut items {
        item.score = 40;
    }
    assert_eq!(
        Item::bulk_update(&items, &["score"], pool).await.unwrap(),
        3
    );
    assert_eq!(ops(pool, ITEM, "update").await, 3);
    assert_eq!(
        latest(pool, ITEM, &pks[2].to_string()).await.changes["score"],
        40
    );
}

async fn queryset_update_audits_each_row(pool: &Pool) {
    let pks = seed(pool).await;
    let n = Item::objects()
        .filter("name", "b")
        .update()
        .set("score", 11_i64)
        .execute_pool(pool)
        .await
        .unwrap();
    assert_eq!(n, 2);
    assert_eq!(ops(pool, ITEM, "update").await, 2);
    assert_eq!(
        latest(pool, ITEM, &pks[1].to_string()).await.changes["score"],
        11
    );
}

/// Writes that cannot audit are refused, and write nothing. MySQL has no
/// RETURNING, so conflict-handling bulk inserts are refused there.
async fn unauditable_bulk_writes_are_refused(pool: &Pool) {
    let tags = [Tag {
        slug: "r".into(),
        label: "r".into(),
    }];
    let refused = |r: Result<(), ExecError>| {
        assert!(
            matches!(r, Err(ExecError::AuditUnsupported { .. })),
            "{r:?}"
        );
    };
    if pool.dialect().name() == "mysql" {
        refused(Tag::bulk_insert_or_ignore_pool(&tags, pool).await);
        refused(Tag::bulk_upsert_pool(&tags, &["slug"], &["label"], pool).await);
    }
    let pk_change = Tag::update_all("slug", "z", pool).await.map(|_| ());
    refused(pk_change);
    assert_eq!(Tag::objects().count(pool).await.unwrap(), 0);
    assert_eq!(ops(pool, TAG, "create").await, 0);
}

fn tag(slug: &str, label: &str) -> Tag {
    Tag {
        slug: slug.into(),
        label: label.into(),
    }
}

/// `bulk_upsert_pool` / `bulk_insert_or_ignore_pool` audit each row they
/// wrote, as `create` or `update` (#1795).
async fn conflict_bulk_inserts_audit_each_written_row(pool: &Pool) {
    if pool.dialect().name() == "mysql" {
        return;
    }
    tag("a", "one").insert_pool(pool).await.expect("seed");
    let rows = [tag("a", "uno"), tag("b", "two")];
    Tag::bulk_upsert_pool(&rows, &["slug"], &["label"], pool)
        .await
        .expect("upsert");
    assert_eq!(ops(pool, TAG, "create").await, 2);
    assert_eq!(ops(pool, TAG, "update").await, 1);
    let a = latest(pool, TAG, "a").await;
    assert_eq!(
        (a.operation.as_str(), &a.changes["label"]),
        ("update", &"uno".into())
    );
    assert_eq!(latest(pool, TAG, "b").await.operation, "create");

    // `a` is skipped, `c` lands: only `c` is audited.
    let rows = [tag("a", "skipped"), tag("c", "three")];
    Tag::bulk_insert_or_ignore_pool(&rows, pool)
        .await
        .expect("insert or ignore");
    assert_eq!(ops(pool, TAG, "create").await, 3);
    assert_eq!(ops(pool, TAG, "update").await, 1);
    assert_eq!(latest(pool, TAG, "c").await.operation, "create");
    assert_eq!(latest(pool, TAG, "a").await.changes["label"], "uno");
}

/// A target that never matches (an unset `Auto` PK) inserts each row once.
async fn bulk_upsert_on_unmatched_target_inserts_once(pool: &Pool) {
    if pool.dialect().name() == "mysql" {
        return;
    }
    let item = |name: &str| Item {
        id: Auto::default(),
        name: name.into(),
        score: 1,
    };
    Item::bulk_upsert_pool(&[item("x"), item("y")], &["id"], &["score"], pool)
        .await
        .expect("upsert");
    assert_eq!(Item::objects().count(pool).await.unwrap(), 2);
    assert_eq!(ops(pool, ITEM, "create").await, 2);
    assert_eq!(ops(pool, ITEM, "update").await, 0);
}

/// A PG `upsert` on a non-PK target records `create`, then `update` (#1795).
async fn upsert_on_unique_target_records_its_op(pool: &Pool) {
    let _ = pool;
    #[cfg(feature = "postgres")]
    #[allow(irrefutable_let_patterns)]
    if let Pool::Postgres(pg) = pool {
        let sku = |qty| Sku {
            id: Auto::default(),
            shop: "s".into(),
            code: "k".into(),
            qty,
        };
        let mut first = sku(1);
        first.upsert(pg).await.expect("upsert insert");
        let pk = first.id.get().expect("pk assigned").to_string();
        assert_eq!(latest(pool, SKU, &pk).await.operation, "create");
        let mut second = sku(5);
        second.upsert(pg).await.expect("upsert update");
        assert_eq!(second.id.get().map(ToString::to_string), Some(pk.clone()));
        let entry = latest(pool, SKU, &pk).await;
        assert_eq!(entry.operation, "update");
        assert_eq!(entry.changes["qty"], 5);
        assert_eq!(ops(pool, SKU, "create").await, 1);
    }
}

/// An audited `bulk_update` past the bind cap is split, not rejected (#1795).
async fn large_bulk_update_is_chunked(pool: &Pool) {
    // Three binds per row: the PK, `name` and `score`.
    let rows = (pool.dialect().max_bind_params() / 3 + 1) as i64;
    seed_many(pool, rows).await;
    let mut items = Item::objects().fetch(pool).await.unwrap();
    for item in &mut items {
        item.score += 1;
    }
    let n = Item::bulk_update(&items, &["name", "score"], pool)
        .await
        .unwrap();
    assert_eq!(n, rows as u64);
    assert_eq!(ops(pool, ITEM, "update").await, rows);
}

/// `bulk_insert` and `upsert` are PostgreSQL-only methods.
async fn bulk_insert_and_upsert_audit_on_postgres(pool: &Pool) {
    let _ = pool;
    #[cfg(feature = "postgres")]
    #[allow(irrefutable_let_patterns)]
    if let Pool::Postgres(pg) = pool {
        let tags = [
            Tag {
                slug: "p".into(),
                label: "one".into(),
            },
            Tag {
                slug: "q".into(),
                label: "two".into(),
            },
        ];
        Tag::bulk_insert(&tags, pg).await.expect("bulk insert");
        assert_eq!(ops(pool, TAG, "create").await, 2);
        assert_eq!(latest(pool, TAG, "q").await.changes["label"], "two");

        let mut item = Item {
            id: Auto::default(),
            name: "u".into(),
            score: 1,
        };
        item.upsert(pg).await.expect("upsert insert");
        let pk = item.id.get().expect("pk assigned").to_string();
        assert_eq!(latest(pool, ITEM, &pk).await.operation, "create");
        item.score = 9;
        item.upsert(pg).await.expect("upsert update");
        let entry = latest(pool, ITEM, &pk).await;
        assert_eq!(entry.operation, "update");
        assert_eq!(entry.changes["score"], 9);
    }
}

/// Past every backend's bind cap as one 6-column audit INSERT.
const OVER_BIND_CAP: usize = 11_000;

/// `n` unaudited items named `b`, scores `0..n`, PKs in score order.
async fn seed_many(pool: &Pool, n: i64) {
    let rows = (0..n)
        .map(|i| vec![SqlValue::from("b"), SqlValue::from(i)])
        .collect();
    let query = BulkInsertQuery::new(
        <Item as rustango::core::Model>::SCHEMA,
        vec!["name", "score"],
        rows,
    );
    rustango::sql::bulk_insert_pool(pool, &query)
        .await
        .expect("seed");
}

/// An audit batch past the bind cap is split, not rejected.
async fn large_audit_batch_is_chunked(pool: &Pool) {
    let entries: Vec<_> = (0..OVER_BIND_CAP)
        .map(|i| audit::PendingEntry {
            entity_table: ITEM,
            entity_pk: i.to_string(),
            operation: audit::AuditOp::Create,
            source: audit::AuditSource::System,
            changes: serde_json::json!({}),
        })
        .collect();
    audit::emit_many_pool(pool, &entries).await.expect("emit");
    assert_eq!(ops(pool, ITEM, "create").await, OVER_BIND_CAP as i64);
}

/// More rows than one page: every page is written and audited.
async fn bulk_writes_page_past_one_chunk(pool: &Pool) {
    seed_many(pool, 1_200).await;
    let n = Item::update_where("name", "b", "score", -1_i64, pool)
        .await
        .unwrap();
    assert_eq!(n, 1_200);
    assert_eq!(ops(pool, ITEM, "update").await, 1_200);
    assert_eq!(Item::delete_where("name", "b", pool).await.unwrap(), 1_200);
    assert_eq!(ops(pool, ITEM, "delete").await, 1_200);
}

/// A `limit()` bound is read once: updating the first page must not let
/// the next page pick up rows past the limit.
async fn bounded_update_writes_only_its_rows(pool: &Pool) {
    seed_many(pool, 1_200).await;
    let n = Item::objects()
        .filter("name", "b")
        .order_by(&[("score", false)])
        .limit(700)
        .update()
        .set("score", 100_000_i64)
        .execute_pool(pool)
        .await
        .unwrap();
    assert_eq!(n, 700);
    assert_eq!(ops(pool, ITEM, "update").await, 700);
    let moved = Item::objects()
        .filter("score", 100_000_i64)
        .count(pool)
        .await
        .unwrap();
    assert_eq!(moved, 700);
}

/// A failed audit insert rolls the data write back. SQLite only:
/// dropping the shared audit table would break other live suites.
async fn failed_audit_insert_rolls_back_the_write(pool: &Pool) {
    if pool.dialect().name() != "sqlite" {
        return;
    }
    seed_many(pool, 3).await;
    rustango::testkit::matrix::drop_table(pool, "rustango_audit_log").await;
    let err = Item::update_where("name", "b", "score", 9_i64, pool)
        .await
        .unwrap_err();
    assert!(matches!(err, ExecError::Driver(_)), "{err}");
    let written = Item::objects().filter("score", 9_i64).count(pool).await;
    assert_eq!(written.unwrap(), 0);
}

/// `bulk_insert` past the audit bind cap still audits every row.
async fn large_bulk_insert_audits_on_postgres(pool: &Pool) {
    let _ = pool;
    #[cfg(feature = "postgres")]
    #[allow(irrefutable_let_patterns)]
    if let Pool::Postgres(pg) = pool {
        let tags: Vec<Tag> = (0..OVER_BIND_CAP)
            .map(|i| Tag {
                slug: format!("s{i}"),
                label: format!("l{i}"),
            })
            .collect();
        Tag::bulk_insert(&tags, pg).await.expect("bulk insert");
        assert_eq!(ops(pool, TAG, "create").await, OVER_BIND_CAP as i64);
    }
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        destroy_audits_each_deleted_row,
        delete_where_audits_each_deleted_row,
        prune_audits_each_deleted_row,
        update_where_audits_the_written_values,
        update_all_audits_every_row,
        increment_each_audits_the_new_values,
        truncate_writes_one_bulk_entry,
        failed_bulk_write_writes_no_audit_row,
        bulk_update_audits_each_row,
        queryset_update_audits_each_row,
        unauditable_bulk_writes_are_refused,
        conflict_bulk_inserts_audit_each_written_row,
        upsert_on_unique_target_records_its_op,
        bulk_upsert_on_unmatched_target_inserts_once,
        large_bulk_update_is_chunked,
        bulk_insert_and_upsert_audit_on_postgres,
        large_audit_batch_is_chunked,
        bulk_writes_page_past_one_chunk,
        bounded_update_writes_only_its_rows,
        failed_audit_insert_rolls_back_the_write,
        large_bulk_insert_audits_on_postgres,
    ],
}

/// Child rows an `ON DELETE CASCADE` removes get a `delete` audit row,
/// through non-audited tables and self-references, each row once.
mod cascade {
    use super::*;

    #[derive(Model, Debug, Clone)]
    #[rustango(table = "audit_casc_shelf", app = "audit1747")]
    #[allow(dead_code)]
    pub struct Shelf {
        #[rustango(primary_key)]
        pub id: i64,
    }

    /// Not audited: the cascade reaches `Crate` through it.
    #[derive(Model, Debug, Clone)]
    #[rustango(table = "audit_casc_rack", app = "audit1747")]
    #[allow(dead_code)]
    pub struct Rack {
        #[rustango(primary_key)]
        pub id: i64,
        #[rustango(fk = "audit_casc_shelf", on = "id", on_delete = "cascade")]
        pub shelf_id: i64,
    }

    #[derive(Model, Debug, Clone)]
    #[rustango(table = "audit_casc_crate", app = "audit1747", audit(track = "label"))]
    #[allow(dead_code)]
    pub struct Crate {
        #[rustango(primary_key)]
        pub id: i64,
        #[rustango(max_length = 16)]
        pub label: String,
        #[rustango(fk = "audit_casc_rack", on = "id", on_delete = "cascade")]
        pub rack_id: i64,
        #[rustango(fk = "audit_casc_crate", on = "id", on_delete = "cascade")]
        pub parent_id: Option<i64>,
    }

    /// Audited, with an FK the shelf delete sets to NULL.
    #[derive(Model, Debug, Clone)]
    #[rustango(
        table = "audit_casc_note",
        app = "audit1747",
        audit(track = "body, shelf_id")
    )]
    #[allow(dead_code)]
    pub struct Note {
        #[rustango(primary_key)]
        pub id: i64,
        #[rustango(max_length = 16)]
        pub body: String,
        #[rustango(fk = "audit_casc_shelf", on = "id", on_delete = "set_null")]
        pub shelf_id: Option<i64>,
    }

    const CRATE: &str = "audit_casc_crate";
    const NOTE: &str = "audit_casc_note";

    async fn setup(pool: &Pool) {
        use rustango::testkit::matrix::{drop_table, fresh_table};
        for t in [NOTE, CRATE, "audit_casc_rack", "audit_casc_shelf"] {
            drop_table(pool, t).await;
        }
        fresh_table::<Shelf>(pool).await;
        fresh_table::<Rack>(pool).await;
        fresh_table::<Crate>(pool).await;
        fresh_table::<Note>(pool).await;
        audit::ensure_table_pool(pool).await.expect("audit table");
        for t in [CRATE, NOTE] {
            AuditLog::delete_where("entity_table", t, pool)
                .await
                .expect("clear audit rows");
        }
        for id in [1, 2] {
            Shelf { id }.insert_pool(pool).await.expect("shelf");
            Rack {
                id: id * 10,
                shelf_id: id,
            }
            .insert_pool(pool)
            .await
            .expect("rack");
        }
        // 1 <- 2 <- 3 on rack 10; 4 on rack 20.
        for (id, label, rack_id, parent_id) in [
            (1, "x", 10, None),
            (2, "x", 10, Some(1)),
            (3, "y", 10, Some(2)),
            (4, "z", 20, None),
        ] {
            Crate {
                id,
                label: label.into(),
                rack_id,
                parent_id,
            }
            .insert_pool(pool)
            .await
            .expect("crate");
        }
    }

    /// `operation` entries for `table` row `pk` (`""` for a bulk one).
    async fn entries(pool: &Pool, table: &str, pk: &str, operation: &str) -> i64 {
        AuditLog::objects()
            .filter("entity_table", table)
            .filter("entity_pk", pk)
            .filter("operation", operation)
            .count(pool)
            .await
            .expect("count")
    }

    /// `ON DELETE SET NULL` writes an `update` entry with the nulled FK,
    /// only for the rows it changed.
    async fn fk_set_null_audits_each_changed_child(pool: &Pool) {
        for (id, shelf_id) in [(1, Some(1)), (2, Some(2)), (3, None)] {
            Note {
                id,
                body: format!("n{id}"),
                shelf_id,
            }
            .insert_pool(pool)
            .await
            .expect("note");
        }
        AuditLog::delete_where("entity_table", NOTE, pool)
            .await
            .expect("clear");
        assert_eq!(Shelf::delete_where("id", 1_i64, pool).await.unwrap(), 1);
        assert_eq!(entries(pool, NOTE, "1", "update").await, 1);
        assert_eq!(entries(pool, NOTE, "2", "update").await, 0);
        assert_eq!(entries(pool, NOTE, "3", "update").await, 0);
        let entry = latest(pool, NOTE, "1").await;
        assert_eq!(entry.changes["shelf_id"], serde_json::Value::Null);
        assert_eq!(entry.changes["body"], "n1");
    }

    /// A truncate's FK cascade gets a bulk entry on each audited table it
    /// empties: every FK on PG (`TRUNCATE … CASCADE`), `ON DELETE CASCADE`
    /// elsewhere (a `DELETE`).
    async fn truncate_cascade_audits_the_tables_it_empties(pool: &Pool) {
        Note {
            id: 1,
            body: "n".into(),
            shelf_id: Some(1),
        }
        .insert_pool(pool)
        .await
        .expect("note");
        Shelf::truncate(pool).await.expect("truncate");
        assert_eq!(Crate::objects().count(pool).await.unwrap(), 0);
        assert_eq!(entries(pool, CRATE, "", "delete").await, 1);
        let pg = pool.dialect().name() == "postgres";
        assert_eq!(entries(pool, NOTE, "", "delete").await, i64::from(pg));
    }

    /// Audited tables the truncate reaches but takes no rows from get no entry.
    async fn truncate_skips_reached_tables_without_rows(pool: &Pool) {
        Crate::truncate(pool).await.expect("empty crates");
        AuditLog::delete_where("entity_table", CRATE, pool)
            .await
            .expect("clear");
        Shelf::truncate(pool).await.expect("truncate");
        assert_eq!(entries(pool, CRATE, "", "delete").await, 0);
        assert_eq!(entries(pool, NOTE, "", "delete").await, 0);
    }

    /// `delete` entries per crate pk.
    async fn deletes(pool: &Pool, pk: i64) -> i64 {
        AuditLog::objects()
            .filter("entity_table", CRATE)
            .filter("entity_pk", pk.to_string())
            .filter("operation", "delete")
            .count(pool)
            .await
            .expect("count")
    }

    async fn fk_cascade_audits_each_removed_child(pool: &Pool) {
        assert_eq!(Shelf::delete_where("id", 1_i64, pool).await.unwrap(), 1);
        for pk in [1, 2, 3] {
            assert_eq!(deletes(pool, pk).await, 1, "crate {pk}");
        }
        assert_eq!(deletes(pool, 4).await, 0);
        let entry = latest(pool, CRATE, "3").await;
        assert_eq!(entry.changes["label"], "y");
        Shelf { id: 2 }.delete_pool(pool).await.unwrap();
        assert_eq!(deletes(pool, 4).await, 1);
        assert_eq!(Crate::objects().count(pool).await.unwrap(), 0);
    }

    async fn audited_delete_audits_each_row_once(pool: &Pool) {
        // MySQL and SQLite do not count a row their cascade took first.
        Crate::delete_where("label", "x", pool).await.unwrap();
        assert_eq!(Crate::objects().count(pool).await.unwrap(), 1);
        for pk in [1, 2, 3] {
            assert_eq!(deletes(pool, pk).await, 1, "crate {pk}");
        }
        assert_eq!(deletes(pool, 4).await, 0);
    }

    /// Inside `atomic` the cascade audit joins the block: a rollback leaves
    /// neither the delete nor its audit rows, a commit keeps both.
    async fn cascade_audit_follows_the_atomic_block(pool: &Pool) {
        let p = pool.clone();
        let rolled: Result<(), ExecError> = rustango::sql::atomic(pool, move |_tx| {
            Box::pin(async move {
                Shelf::delete_where("id", 1_i64, &p).await?;
                Err(ExecError::MissingPrimaryKey { table: "rollback" })
            })
        })
        .await;
        assert!(rolled.is_err());
        assert_eq!(Crate::objects().count(pool).await.unwrap(), 4);
        assert_eq!(deletes(pool, 1).await, 0);
        let p = pool.clone();
        rustango::sql::atomic(pool, move |_tx| {
            Box::pin(async move { Shelf::delete_where("id", 1_i64, &p).await.map(drop) })
        })
        .await
        .expect("commit");
        for pk in [1, 2, 3] {
            assert_eq!(deletes(pool, pk).await, 1, "crate {pk}");
        }
    }

    /// A raw PG executor runs one statement: the delete still works (as in
    /// 0.61.1), the cascade just goes unaudited.
    async fn single_statement_pg_paths_still_delete(pool: &Pool) {
        let _ = pool;
        #[cfg(feature = "postgres")]
        {
            let Some(pg) = pool.as_postgres() else {
                return;
            };
            Shelf { id: 1 }
                .delete(pg)
                .await
                .expect("Model::delete(&PgPool)");
            let n = Shelf::objects()
                .filter("id", 2_i64)
                .delete_on(pg)
                .await
                .expect("QuerySet::delete_on(&PgPool)");
            assert_eq!(n, 1);
            assert_eq!(Crate::objects().count(pool).await.unwrap(), 0);
            assert_eq!(deletes(pool, 1).await, 0);
        }
    }

    tri_dialect_test! {
        setup: setup,
        scenarios: [
            fk_cascade_audits_each_removed_child,
            audited_delete_audits_each_row_once,
            cascade_audit_follows_the_atomic_block,
            single_statement_pg_paths_still_delete,
            fk_set_null_audits_each_changed_child,
            truncate_cascade_audits_the_tables_it_empties,
            truncate_skips_reached_tables_without_rows,
        ],
    }
}
