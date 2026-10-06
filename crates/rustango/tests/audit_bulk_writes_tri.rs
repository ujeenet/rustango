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
        large_bulk_update_is_chunked,
        bulk_insert_and_upsert_audit_on_postgres,
        large_audit_batch_is_chunked,
        bulk_writes_page_past_one_chunk,
        bounded_update_writes_only_its_rows,
        failed_audit_insert_rolls_back_the_write,
        large_bulk_insert_audits_on_postgres,
    ],
}
