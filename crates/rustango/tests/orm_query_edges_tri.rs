//! ORM edges on every backend: FK `get` under `on`, join alias clashes, long
//! `select_related` chains and `in_bulk_on` past the bind cap.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::core::{joins::aliased, Join, JoinKind, Model as _, Op, QueryError, WhereExpr};
use rustango::sql::{ExecError, FetcherPool as _, ForeignKey, Pool};
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(table = "oqe_country", app = "oqe")]
pub struct Country {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(unique)]
    pub num: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "oqe_city", app = "oqe")]
pub struct City {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(on = "num")]
    pub country: ForeignKey<Country>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "oqe_leaf", app = "oqe")]
pub struct Leaf {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 20)]
    pub label: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "oqe_mid", app = "oqe")]
pub struct Mid {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(related_name = "+")]
    pub second_hop_relation_one: ForeignKey<Leaf>,
    #[rustango(related_name = "+")]
    pub second_hop_relation_two: ForeignKey<Leaf>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "oqe_top", app = "oqe")]
pub struct Top {
    #[rustango(primary_key)]
    pub id: i64,
    pub the_middle_relation_with_a_much_longer_field_name: ForeignKey<Mid>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "oqe_bucket", app = "oqe")]
pub struct Bucket {
    #[rustango(primary_key)]
    pub id: i64,
}

async fn setup(pool: &Pool) {
    use rustango::testkit::matrix::{drop_table, fresh_table};
    for t in [City::SCHEMA, Top::SCHEMA, Mid::SCHEMA] {
        drop_table(pool, t.table).await;
    }
    fresh_table::<Country>(pool).await;
    fresh_table::<City>(pool).await;
    fresh_table::<Leaf>(pool).await;
    fresh_table::<Mid>(pool).await;
    fresh_table::<Top>(pool).await;
    fresh_table::<Bucket>(pool).await;
}

/// `get` loads the parent by the `on` column, not by a PK equal to the key.
async fn fk_get_follows_the_on_column(pool: &Pool) {
    for (id, num) in [(1, 100), (100, 7)] {
        Country { id, num }.insert_pool(pool).await.expect("country");
    }
    City {
        id: 1,
        country: ForeignKey::unloaded(100),
    }
    .insert_pool(pool)
    .await
    .expect("city");
    let mut city = City::objects().fetch(pool).await.expect("fetch").remove(0);
    let parent = city.country.get_on(pool).await.expect("get");
    assert_eq!(parent.id, 1, "loaded by num = 100, not id = 100");
}

/// An ad-hoc join may not take the alias a relation filter needs.
async fn ad_hoc_alias_clashing_a_relation_filter(pool: &Pool) {
    let _ = pool;
    let compiled = City::objects()
        .filter("country__num", 100_i64)
        .join(Join {
            target: Leaf::SCHEMA,
            alias: "country",
            kind: JoinKind::Left,
            on: WhereExpr::ExprCompare {
                lhs: aliased("country", "id"),
                op: Op::Eq,
                rhs: aliased("oqe_city", "id"),
            },
            project: vec![],
        })
        .compile();
    assert!(
        matches!(compiled, Err(QueryError::DuplicateJoinAlias { ref alias }) if alias == "country"),
        "{compiled:?}"
    );
}

/// Two chains whose aliases share 63 bytes: PG would cut both to one name.
async fn long_select_related_chains(pool: &Pool) {
    for (id, label) in [(1, "one"), (2, "two")] {
        Leaf {
            id,
            label: label.into(),
        }
        .insert_pool(pool)
        .await
        .expect("leaf");
    }
    Mid {
        id: 1,
        second_hop_relation_one: ForeignKey::unloaded(1),
        second_hop_relation_two: ForeignKey::unloaded(2),
    }
    .insert_pool(pool)
    .await
    .expect("mid");
    Top {
        id: 1,
        the_middle_relation_with_a_much_longer_field_name: ForeignKey::unloaded(1),
    }
    .insert_pool(pool)
    .await
    .expect("top");
    let rows: Vec<Top> = Top::objects()
        .select_related("the_middle_relation_with_a_much_longer_field_name__second_hop_relation_one")
        .select_related("the_middle_relation_with_a_much_longer_field_name__second_hop_relation_two")
        .fetch(pool)
        .await
        .expect("two long chains");
    let mid = rows[0]
        .the_middle_relation_with_a_much_longer_field_name
        .value()
        .expect("mid loaded");
    let label = |fk: &ForeignKey<Leaf>| fk.value().map(|l| l.label.clone());
    assert_eq!(label(&mid.second_hop_relation_one).as_deref(), Some("one"));
    assert_eq!(label(&mid.second_hop_relation_two).as_deref(), Some("two"));
}

/// `in_bulk_on` refuses one `IN` list past the bind cap instead of sending it.
async fn in_bulk_on_past_the_bind_cap(pool: &Pool) {
    for id in [1, 2] {
        Bucket { id }.insert_pool(pool).await.expect("bucket");
    }
    let mut conn = pool.acquire().await.expect("conn");
    let small = Bucket::objects()
        .in_bulk_on(Bucket::id, [1_i64, 2], |b| b.id, &mut conn)
        .await
        .expect("small in_bulk_on");
    assert_eq!(small.len(), 2);
    let err = Bucket::objects()
        .in_bulk_on(Bucket::id, 0..70_000_i64, |b| b.id, &mut conn)
        .await
        .expect_err("past the bind cap");
    assert!(matches!(err, ExecError::InListUnsplittable { .. }), "{err:?}");
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        fk_get_follows_the_on_column,
        ad_hoc_alias_clashing_a_relation_filter,
        long_select_related_chains,
        in_bulk_on_past_the_bind_cap,
    ],
}
