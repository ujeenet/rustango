//! `update()` / `delete()` honour `limit`, `offset` and `order_by` on
//! every backend (#1666). They used to drop them and touch every row.
//! A set operation is refused outright, with or without a bound (#2452).

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::core::{BoundedDmlReason, QueryError};
use rustango::sql::{delete_pool, FetcherPool as _, ForeignKey, Pool, UpdaterPool as _};
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(table = "bdml_item")]
#[allow(dead_code)]
pub struct Item {
    #[rustango(primary_key)]
    pub id: i64,
    pub rank: i64,
    #[rustango(max_length = 10)]
    pub tag: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "bdml_child")]
#[allow(dead_code)]
pub struct Child {
    #[rustango(primary_key)]
    pub id: i64,
    pub item: ForeignKey<Item>,
}

/// Points at `Item` with no foreign key, like `rustango_media_tag_links`.
#[derive(Model, Debug, Clone)]
#[rustango(table = "bdml_link")]
#[allow(dead_code)]
pub struct Link {
    #[rustango(primary_key)]
    pub id: i64,
    pub item_id: i64,
}

/// Its children are rows of its own table.
#[derive(Model, Debug, Clone)]
#[rustango(table = "bdml_node")]
#[allow(dead_code)]
pub struct Node {
    #[rustango(primary_key)]
    pub id: i64,
    pub parent_id: Option<i64>,
    #[rustango(max_length = 10)]
    pub tag: String,
}

/// No primary key at all: a bounded write has nothing to bound by.
#[derive(Model, Debug, Clone)]
#[rustango(table = "bdml_log")]
#[allow(dead_code)]
pub struct Log {
    pub line: i64,
}

async fn seeded(pool: &Pool) {
    // A run that panicked mid-scenario leaves `bdml_child`'s FK behind.
    rustango::testkit::matrix::drop_table(pool, "bdml_child").await;
    rustango::testkit::matrix::fresh_table::<Item>(pool).await;
    for (id, rank) in [(1, 50), (2, 10), (3, 40), (4, 20), (5, 30)] {
        Item {
            id,
            rank,
            tag: "new".into(),
        }
        .insert_pool(pool)
        .await
        .expect("seed");
    }
}

async fn ids(pool: &Pool) -> Vec<i64> {
    let rows: Vec<Item> = Item::objects()
        .order_by(&[("id", false)])
        .fetch(pool)
        .await
        .unwrap();
    rows.into_iter().map(|r| r.id).collect()
}

async fn ids_tagged(pool: &Pool, tag: &str) -> Vec<i64> {
    let rows: Vec<Item> = Item::objects()
        .filter("tag", tag)
        .order_by(&[("id", false)])
        .fetch(pool)
        .await
        .unwrap();
    rows.into_iter().map(|r| r.id).collect()
}

async fn limit_delete_removes_one(pool: &Pool) {
    let q = Item::objects().limit(1).compile_delete().unwrap();
    assert_eq!(delete_pool(pool, &q).await.expect("limited delete"), 1);
    assert_eq!(
        ids(pool).await,
        vec![2, 3, 4, 5],
        "one row goes; the smallest pk on every backend"
    );
}

async fn ordered_limited_delete(pool: &Pool) {
    // Two highest ranks are ids 1 (50) and 3 (40).
    let q = Item::objects()
        .order_by(&[("rank", true)])
        .limit(2)
        .compile_delete()
        .unwrap();
    assert_eq!(delete_pool(pool, &q).await.unwrap(), 2);
    assert_eq!(ids(pool).await, vec![2, 4, 5]);
}

async fn filtered_offset_delete(pool: &Pool) {
    // rank > 15 by rank asc: 4, 5, 3, 1; skip 2, take 1 -> id 3.
    let q = Item::objects()
        .filter("rank__gt", 15_i64)
        .order_by(&[("rank", false)])
        .limit(1)
        .offset(2)
        .compile_delete()
        .unwrap();
    assert_eq!(delete_pool(pool, &q).await.unwrap(), 1);
    assert_eq!(ids(pool).await, vec![1, 2, 4, 5]);
}

async fn offset_only_delete(pool: &Pool) {
    // Keep the two lowest ranks (ids 2, 4), delete the rest.
    let q = Item::objects()
        .order_by(&[("rank", false)])
        .offset(2)
        .compile_delete()
        .unwrap();
    assert_eq!(delete_pool(pool, &q).await.unwrap(), 3);
    assert_eq!(ids(pool).await, vec![2, 4]);
}

async fn ordered_limited_update(pool: &Pool) {
    // Two lowest ranks are ids 2 (10) and 4 (20).
    let n = Item::objects()
        .order_by(&[("rank", false)])
        .limit(2)
        .update()
        .set("tag", "hit")
        .execute_pool(pool)
        .await
        .expect("limited update");
    assert_eq!(n, 2);
    assert_eq!(ids_tagged(pool, "hit").await, vec![2, 4]);
}

async fn offset_update(pool: &Pool) {
    // By id: skip 3, take the rest -> ids 4, 5.
    let n = Item::objects()
        .offset(3)
        .update()
        .set("tag", "tail")
        .execute_pool(pool)
        .await
        .unwrap();
    assert_eq!(n, 2);
    assert_eq!(ids_tagged(pool, "tail").await, vec![4, 5]);
}

async fn filtered_limited_update_binds(pool: &Pool) {
    // SET bind, outer WHERE bind, inner WHERE bind: order must hold.
    let n = Item::objects()
        .filter("rank__gte", 30_i64)
        .order_by(&[("rank", true)])
        .limit(1)
        .update()
        .set("tag", "top")
        .execute_pool(pool)
        .await
        .unwrap();
    assert_eq!(n, 1);
    assert_eq!(ids_tagged(pool, "top").await, vec![1]);
}

async fn ties_page_by_pk(pool: &Pool) {
    // Equal ranks: pages must not overlap.
    Item::objects()
        .update()
        .set("rank", 7_i64)
        .execute_pool(pool)
        .await
        .unwrap();
    for (page, tag) in [(0, "p0"), (2, "p1")] {
        Item::objects()
            .order_by(&[("rank", false)])
            .limit(2)
            .offset(page)
            .update()
            .set("tag", tag)
            .execute_pool(pool)
            .await
            .unwrap();
    }
    assert_eq!(ids_tagged(pool, "p0").await, vec![1, 2]);
    assert_eq!(ids_tagged(pool, "p1").await, vec![3, 4]);
}

async fn unbounded_paths_unchanged(pool: &Pool) {
    // order_by alone does not bound anything.
    let q = Item::objects()
        .filter("rank__lt", 25_i64)
        .order_by(&[("rank", true)])
        .compile_delete()
        .unwrap();
    assert_eq!(delete_pool(pool, &q).await.unwrap(), 2);
    let q = Item::objects().none().limit(3).compile_delete().unwrap();
    assert_eq!(delete_pool(pool, &q).await.unwrap(), 0);
    assert_eq!(ids(pool).await, vec![1, 3, 5]);
}

async fn anti_join_delete(pool: &Pool) {
    // The media orphan-link sweep (#1578): delete links whose item is gone.
    use rustango::core::{subquery::outer_ref, Column as _};
    rustango::testkit::matrix::fresh_table::<Link>(pool).await;
    for (id, item_id) in [(1, 1), (2, 99), (3, 5), (4, 98)] {
        Link { id, item_id }.insert_pool(pool).await.unwrap();
    }
    let alive = Item::objects()
        .where_(Item::id.eq_expr(outer_ref("item_id")))
        .compile()
        .unwrap();
    let q = Link::objects()
        .where_not_exists(alive)
        .compile_delete()
        .unwrap();
    assert_eq!(delete_pool(pool, &q).await.unwrap(), 2);
    let left: Vec<Link> = Link::objects()
        .order_by(&[("id", false)])
        .fetch(pool)
        .await
        .unwrap();
    assert_eq!(left.iter().map(|l| l.id).collect::<Vec<_>>(), vec![1, 3]);
}

/// `IN (SELECT … FROM <target>)` in a write; MySQL raised 1093.
async fn same_table_in_subquery_dml(pool: &Pool) {
    let high = || {
        Item::objects()
            .filter("rank__gt", 35_i64)
            .values_list_flat("id")
            .compile()
            .unwrap()
    };
    let n = Item::objects()
        .where_in_subquery("id", high())
        .update()
        .set("tag", "hi")
        .execute_pool(pool)
        .await
        .expect("update IN (same table)");
    assert_eq!(n, 2);
    assert_eq!(ids_tagged(pool, "hi").await, vec![1, 3]);
    let q = Item::objects()
        .where_not_in_subquery("id", high())
        .compile_delete()
        .unwrap();
    assert_eq!(delete_pool(pool, &q).await.expect("delete NOT IN"), 3);
    assert_eq!(ids(pool).await, vec![1, 3]);
}

/// A correlated `EXISTS` over the target in a write; MySQL raised 1093.
async fn same_table_exists_dml(pool: &Pool) {
    use rustango::core::{subquery::outer_ref, Column as _};
    let higher = || {
        Item::objects()
            .where_(Item::rank.gt_expr(outer_ref("rank")))
            .compile()
            .unwrap()
    };
    let n = Item::objects()
        .where_exists(higher())
        .update()
        .set("tag", "lo")
        .execute_pool(pool)
        .await
        .expect("update EXISTS (same table)");
    assert_eq!(n, 4);
    assert_eq!(ids_tagged(pool, "lo").await, vec![2, 3, 4, 5]);
    let q = Item::objects()
        .where_not_exists(higher())
        .compile_delete()
        .unwrap();
    assert_eq!(delete_pool(pool, &q).await.expect("delete NOT EXISTS"), 1);
    assert_eq!(ids(pool).await, vec![2, 3, 4, 5]);
}

/// Two EXISTS levels over the target: each `outer_ref` reads the level above.
/// MySQL may refuse the inner read (1093) but must never write wrong rows.
async fn nested_same_table_exists_dml(pool: &Pool) {
    use rustango::core::{subquery::outer_ref, Column as _};
    let two_higher = || {
        let higher = Item::objects()
            .where_(Item::rank.gt_expr(outer_ref("rank")))
            .compile()
            .unwrap();
        Item::objects()
            .where_(Item::rank.gt_expr(outer_ref("rank")))
            .where_exists(higher)
            .compile()
            .unwrap()
    };
    let refused = |r: Result<u64, rustango::sql::ExecError>, want: u64| match r {
        Ok(n) => {
            assert_eq!(n, want);
            false
        }
        Err(e) if pool.dialect().name() == "mysql" && e.to_string().contains("1093") => true,
        Err(e) => panic!("{e}"),
    };
    // Raises rank: a read that saw its own writes would pick other rows.
    let r = Item::objects()
        .where_exists(two_higher())
        .update()
        .set("rank", 100_i64)
        .execute_pool(pool)
        .await;
    if !refused(r, 3) {
        let rows: Vec<Item> = Item::objects()
            .order_by(&[("id", false)])
            .fetch(pool)
            .await
            .unwrap();
        let ranks: Vec<i64> = rows.iter().map(|r| r.rank).collect();
        assert_eq!(ranks, vec![50, 100, 40, 100, 100]);
    }
    seeded(pool).await;
    let q = Item::objects()
        .where_not_exists(two_higher())
        .compile_delete()
        .unwrap();
    if !refused(delete_pool(pool, &q).await, 2) {
        assert_eq!(ids(pool).await, vec![2, 4, 5]);
    }
}

/// A correlated IN over the target, in a write that changes the column it
/// filters on. As a derived table MySQL re-read it per row and saw the
/// earlier writes; it now refuses it (MySQL error 1093) and writes nothing.
/// MariaDB reads the target in a plain subquery and runs it correctly.
async fn correlated_same_table_in_dml(pool: &Pool) {
    use rustango::core::{subquery::outer_ref, Column as _};
    // Tags of higher rows ranked over 25; only id 1 (rank 50) has none.
    let higher = Item::objects()
        .where_(Item::rank.gt_expr(outer_ref("rank")))
        .filter("rank__gt", 25_i64)
        .values_list_flat("tag")
        .compile()
        .unwrap();
    let r = Item::objects()
        .where_not_in_subquery("tag", higher)
        .update()
        .set("rank", 0_i64)
        .execute_pool(pool)
        .await;
    let ranks = || async {
        let rows: Vec<Item> = Item::objects()
            .order_by(&[("id", false)])
            .fetch(pool)
            .await
            .unwrap();
        rows.iter().map(|r| r.rank).collect::<Vec<i64>>()
    };
    let mariadb = pool.dialect().name() == "mysql" && {
        let v: Vec<(String,)> = rustango::sql::raw_query_pool("SELECT VERSION()", Vec::new(), pool)
            .await
            .unwrap();
        v[0].0.contains("MariaDB")
    };
    if pool.dialect().name() == "mysql" && !mariadb {
        let e = r.expect_err("MySQL refuses a correlated IN over the target");
        assert!(e.to_string().contains("1093"), "{e}");
        assert_eq!(ranks().await, vec![50, 10, 40, 20, 30]);
    } else {
        assert_eq!(r.expect("update"), 1);
        assert_eq!(ranks().await, vec![0, 10, 40, 20, 30]);
    }
}

/// A raw-table relation EXISTS (M2M / generic `where_has`) over the write's
/// own table; MySQL raised 1093.
async fn self_relation_where_has_dml(pool: &Pool) {
    use rustango::core::{RelCorrelation, WhereExpr};
    let children = |negated| WhereExpr::RelExists {
        table: "bdml_node",
        correlation: RelCorrelation::Fk {
            fk_column: "parent_id",
            outer_column: "id",
            ct: None,
        },
        negated,
    };
    rustango::testkit::matrix::fresh_table::<Node>(pool).await;
    for (id, parent_id) in [(1, None), (2, Some(1)), (3, Some(2)), (4, None)] {
        Node {
            id,
            parent_id,
            tag: "new".into(),
        }
        .insert_pool(pool)
        .await
        .unwrap();
    }
    let n = Node::objects()
        .where_raw(children(false))
        .update()
        .set("tag", "parent")
        .execute_pool(pool)
        .await
        .expect("update where_has (same table)");
    assert_eq!(n, 2);
    let q = Node::objects()
        .where_raw(children(true))
        .compile_delete()
        .unwrap();
    assert_eq!(
        delete_pool(pool, &q)
            .await
            .expect("delete where_doesnt_have"),
        2
    );
    let left: Vec<Node> = Node::objects()
        .order_by(&[("id", false)])
        .fetch(pool)
        .await
        .unwrap();
    let left: Vec<_> = left.iter().map(|r| (r.id, r.tag.as_str())).collect();
    assert_eq!(left, vec![(1, "parent"), (2, "parent")]);
}

/// The target read only by a nested IN or EXISTS still raises 1093 on MySQL;
/// a correlated subquery over another table runs as a plain IN.
async fn nested_and_correlated_in_subquery_dml(pool: &Pool) {
    use rustango::core::{subquery::outer_ref, Column as _};
    rustango::testkit::matrix::fresh_table::<Link>(pool).await;
    for (id, item_id) in [(1, 1), (2, 99), (3, 5), (4, 3)] {
        Link { id, item_id }.insert_pool(pool).await.unwrap();
    }
    let high = || {
        Item::objects()
            .filter("rank__gt", 35_i64)
            .values_list_flat("id")
            .compile()
            .unwrap()
    };
    let linked_high = Link::objects()
        .where_in_subquery("item_id", high())
        .values_list_flat("item_id")
        .compile()
        .unwrap();
    let n = Item::objects()
        .where_in_subquery("id", linked_high)
        .update()
        .set("tag", "d2")
        .execute_pool(pool)
        .await
        .expect("update IN (IN (same table))");
    assert_eq!(n, 2);
    assert_eq!(ids_tagged(pool, "d2").await, vec![1, 3]);

    let correlated = Link::objects()
        .where_(Link::item_id.eq_expr(outer_ref("id")))
        .values_list_flat("item_id")
        .compile()
        .unwrap();
    let n = Item::objects()
        .where_in_subquery("id", correlated)
        .update()
        .set("tag", "lk")
        .execute_pool(pool)
        .await
        .expect("update IN (correlated)");
    assert_eq!(n, 3);
    assert_eq!(ids_tagged(pool, "lk").await, vec![1, 3, 5]);

    let alive_high = Link::objects()
        .where_exists(
            Item::objects()
                .where_(Item::id.eq_expr(outer_ref("item_id")))
                .filter("rank__gt", 35_i64)
                .compile()
                .unwrap(),
        )
        .values_list_flat("item_id")
        .compile()
        .unwrap();
    let q = Item::objects()
        .where_in_subquery("id", alive_high)
        .compile_delete()
        .unwrap();
    assert_eq!(delete_pool(pool, &q).await.expect("delete IN (EXISTS)"), 2);
    assert_eq!(ids(pool).await, vec![2, 4, 5]);
}

/// MySQL wraps only the IN that reads the target itself: a wrapped inner IN
/// hides its read, a JOIN to the target counts.
async fn nested_in_wraps_once(pool: &Pool) {
    use rustango::core::{subquery::outer_ref, Column as _};
    use rustango::sql::{Dialect as _, MySql};
    rustango::testkit::matrix::fresh_table::<Link>(pool).await;
    for (id, item_id) in [(1, 1), (2, 99), (3, 5), (4, 3)] {
        Link { id, item_id }.insert_pool(pool).await.unwrap();
    }
    rustango::testkit::matrix::fresh_table::<Child>(pool).await;
    for id in [1, 2, 3] {
        Child {
            id,
            item: ForeignKey::unloaded(id),
        }
        .insert_pool(pool)
        .await
        .unwrap();
    }
    let high = || {
        Item::objects()
            .filter("rank__gt", 35_i64)
            .values_list_flat("id")
            .compile()
            .unwrap()
    };
    let run = |inner: rustango::core::SelectQuery, tag: &'static str| async move {
        let b = || Item::objects().where_in_subquery("id", inner.clone());
        let q = b().update().set("tag", tag).compile().unwrap();
        let sql = MySql.compile_update(&q).unwrap().sql;
        assert_eq!(sql.matches("_rustango_in").count(), 1, "{sql}");
        b().update()
            .set("tag", tag)
            .execute_pool(pool)
            .await
            .unwrap_or_else(|e| panic!("{tag}: {e}"))
    };
    // Correlated outer IN over Link holding an IN over the target.
    let correlated = Link::objects()
        .where_(Link::item_id.eq_expr(outer_ref("id")))
        .where_in_subquery("item_id", high())
        .values_list_flat("item_id")
        .compile()
        .unwrap();
    assert_eq!(run(correlated, "co").await, 2);
    assert_eq!(ids_tagged(pool, "co").await, vec![1, 3]);
    // Outer IN over the target holding an IN over Link.
    let linked = Link::objects()
        .values_list_flat("item_id")
        .compile()
        .unwrap();
    let reverse = Item::objects()
        .where_in_subquery("id", linked)
        .values_list_flat("id")
        .compile()
        .unwrap();
    assert_eq!(run(reverse, "rv").await, 3);
    assert_eq!(ids_tagged(pool, "rv").await, vec![1, 3, 5]);
    // The target only in a related-field JOIN.
    let joined = Child::objects()
        .filter("item__rank__gt", 35_i64)
        .values_list_flat("item")
        .compile()
        .unwrap();
    assert_eq!(run(joined, "jn").await, 2);
    assert_eq!(ids_tagged(pool, "jn").await, vec![1, 3]);
    // Its FK would block the next scenario's fresh `bdml_item`.
    rustango::testkit::matrix::drop_table(pool, "bdml_child").await;
}

/// `difference` used to be dropped, so the excluded rows went too (#2452).
async fn set_operation_dml_is_refused(pool: &Pool) {
    let pinned = || Item::objects().filter("id", 1_i64);
    let e = Item::objects()
        .filter("tag", "new")
        .difference(pinned())
        .compile_delete()
        .unwrap_err();
    assert!(matches!(e, QueryError::SetOperationDml { .. }), "{e:?}");
    let e = Item::objects()
        .limit(1)
        .union(pinned())
        .compile_delete()
        .unwrap_err();
    assert!(matches!(e, QueryError::SetOperationDml { .. }), "{e:?}");
    let e = Item::objects()
        .intersection(pinned())
        .update()
        .set("tag", "x")
        .execute_pool(pool)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("set operation"), "{e}");
    assert_eq!(ids(pool).await, vec![1, 2, 3, 4, 5]);
    assert!(ids_tagged(pool, "x").await.is_empty());
}

/// A join used to be dropped, so the write reached every row.
async fn join_dml_is_refused(pool: &Pool) {
    use rustango::core::joins::aliased;
    use rustango::core::{Join, JoinKind, Model as _, Op, WhereExpr};
    rustango::testkit::matrix::fresh_table::<Child>(pool).await;
    Child {
        id: 1,
        item: ForeignKey::unloaded(1_i64),
    }
    .insert_pool(pool)
    .await
    .unwrap();
    let on = || WhereExpr::ExprCompare {
        lhs: aliased("c", "item"),
        op: Op::Eq,
        rhs: aliased("bdml_item", "id"),
    };
    let joined = || {
        Item::objects().join(Join {
            target: Child::SCHEMA,
            alias: "c",
            kind: JoinKind::Inner,
            on: on(),
            project: vec![],
        })
    };
    let e = joined().compile_delete().unwrap_err();
    assert!(matches!(e, QueryError::JoinDml { .. }), "{e:?}");
    let sub = Child::objects().compile().unwrap();
    let e = Item::objects()
        .join_sub(sub, "c", on())
        .compile_delete()
        .unwrap_err();
    assert!(matches!(e, QueryError::JoinDml { .. }), "{e:?}");
    let e = joined()
        .update()
        .set("tag", "x")
        .execute_pool(pool)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("join"), "{e}");
    assert_eq!(ids(pool).await, vec![1, 2, 3, 4, 5]);
    assert!(ids_tagged(pool, "x").await.is_empty());
    rustango::testkit::matrix::drop_table(pool, "bdml_child").await;
}

tri_dialect_test! {
    setup: seeded,
    scenarios: [
        limit_delete_removes_one,
        ordered_limited_delete,
        filtered_offset_delete,
        offset_only_delete,
        ordered_limited_update,
        offset_update,
        filtered_limited_update_binds,
        ties_page_by_pk,
        unbounded_paths_unchanged,
        anti_join_delete,
        same_table_in_subquery_dml,
        same_table_exists_dml,
        nested_same_table_exists_dml,
        correlated_same_table_in_dml,
        self_relation_where_has_dml,
        nested_and_correlated_in_subquery_dml,
        nested_in_wraps_once,
        set_operation_dml_is_refused,
        join_dml_is_refused,
    ],
}

fn reason(err: QueryError) -> BoundedDmlReason {
    match err {
        QueryError::BoundedDmlUnsupported { reason, .. } => reason,
        other => panic!("expected BoundedDmlUnsupported, got {other:?}"),
    }
}

#[test]
fn negative_limit_or_offset_is_refused() {
    // SQLite reads `LIMIT -1` as no limit: this once deleted every row.
    let e = Item::objects().limit(-1).compile_delete().unwrap_err();
    assert_eq!(reason(e), BoundedDmlReason::Negative);
    let e = Item::objects()
        .offset(-2)
        .update()
        .set("tag", "x")
        .compile()
        .unwrap_err();
    assert_eq!(reason(e), BoundedDmlReason::Negative);
}

#[test]
fn set_operation_with_limit_is_refused() {
    let e = Item::objects()
        .filter("id", 1_i64)
        .union(Item::objects().filter("id", 2_i64))
        .limit(1)
        .compile_delete()
        .unwrap_err();
    assert!(matches!(e, QueryError::SetOperationDml { .. }), "{e:?}");
}

#[test]
fn relation_order_by_with_limit_is_refused() {
    let e = Child::objects()
        .order_by(&[("item__rank", false)])
        .limit(1)
        .compile_delete()
        .unwrap_err();
    assert_eq!(reason(e), BoundedDmlReason::RelationOrderBy);
}

#[test]
fn model_without_single_pk_is_refused() {
    let e = Log::objects().limit(1).compile_delete().unwrap_err();
    assert_eq!(reason(e), BoundedDmlReason::NoSinglePrimaryKey);
}

#[cfg(feature = "postgres")]
#[test]
fn outer_where_is_kept_next_to_the_bound() {
    use rustango::sql::{Dialect as _, Postgres};
    let q = Item::objects()
        .filter("tag", "new")
        .limit(1)
        .compile_delete()
        .unwrap();
    let sql = Postgres.compile_delete(&q).unwrap().sql;
    let (outer, inner) = sql.split_once(" IN (").expect("bounded by IN");
    assert!(outer.contains(r#""tag" = $1"#), "outer WHERE kept: {sql}");
    assert!(inner.contains(r#""tag" = $2"#), "inner WHERE: {sql}");
}

#[test]
fn lateral_join_dml_is_refused() {
    use rustango::core::WhereExpr;
    let sub = Child::objects().compile().unwrap();
    let e = Item::objects()
        .join_lateral(sub, "c", WhereExpr::And(vec![]))
        .update()
        .set("tag", "x")
        .compile()
        .unwrap_err();
    assert!(matches!(e, QueryError::JoinDml { .. }), "{e:?}");
}
