//! ORM edges on every backend: FK `get` under `on`, join alias clashes, long
//! `select_related` chains, `in_bulk_on` past the bind cap, soft / generic
//! prefetch through a renamed column, ISO weeks and `f32` in JSON.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use chrono::{NaiveDate, TimeZone as _, Utc};
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

#[derive(Model, Debug, Clone)]
#[rustango(table = "oqe_day", app = "oqe")]
pub struct Day {
    #[rustango(primary_key)]
    pub id: i64,
    pub d: NaiveDate,
    pub at: chrono::DateTime<Utc>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "oqe_gauge", app = "oqe", audit(track = "ratio"))]
pub struct Gauge {
    #[rustango(primary_key)]
    pub id: i64,
    pub ratio: f32,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "oqe_owner", app = "oqe")]
pub struct Owner {
    #[rustango(primary_key)]
    pub id: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "oqe_pet", app = "oqe")]
pub struct Pet {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(column = "owner_ref")]
    pub owner: ForeignKey<Owner>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "oqe_note", app = "oqe")]
#[rustango(generic_fk(name = "target", ct_column = "ct_ref", pk_column = "obj_ref"))]
pub struct Note {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(column = "ct_ref")]
    pub ct_id: i64,
    #[rustango(column = "obj_ref")]
    pub object_pk: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "oqe_token", app = "oqe")]
pub struct Token {
    #[rustango(primary_key)]
    pub id: uuid::Uuid,
    #[rustango(max_length = 20)]
    pub label: Option<String>,
    pub rank: i32,
}

async fn setup(pool: &Pool) {
    use rustango::testkit::matrix::{drop_table, fresh_table};
    for t in [City::SCHEMA, Top::SCHEMA, Mid::SCHEMA, Pet::SCHEMA] {
        drop_table(pool, t.table).await;
    }
    fresh_table::<Country>(pool).await;
    fresh_table::<City>(pool).await;
    fresh_table::<Leaf>(pool).await;
    fresh_table::<Mid>(pool).await;
    fresh_table::<Top>(pool).await;
    fresh_table::<Bucket>(pool).await;
    fresh_table::<Owner>(pool).await;
    fresh_table::<Pet>(pool).await;
    fresh_table::<Note>(pool).await;
    fresh_table::<Day>(pool).await;
    fresh_table::<Gauge>(pool).await;
    fresh_table::<Token>(pool).await;
    rustango::audit::ensure_table_pool(pool)
        .await
        .expect("audit table");
}

/// `get` loads the parent by the `on` column, not by a PK equal to the key.
async fn fk_get_follows_the_on_column(pool: &Pool) {
    for (id, num) in [(1, 100), (100, 7)] {
        Country { id, num }
            .insert_pool(pool)
            .await
            .expect("country");
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

/// An ad-hoc join equal to the relation join, either side order, is reused.
async fn ad_hoc_join_equal_to_the_relation_join(pool: &Pool) {
    for (id, num) in [(1, 100), (2, 200)] {
        Country { id, num }
            .insert_pool(pool)
            .await
            .expect("country");
    }
    for (id, num) in [(1, 100), (2, 200)] {
        City {
            id,
            country: ForeignKey::unloaded(num),
        }
        .insert_pool(pool)
        .await
        .expect("city");
    }
    let (city, country) = (aliased("oqe_city", "country"), aliased("country", "num"));
    for (lhs, rhs) in [(city.clone(), country.clone()), (country, city)] {
        let rows: Vec<City> = City::objects()
            .filter("country__num", 200_i64)
            .join(Join {
                target: Country::SCHEMA,
                alias: "country",
                kind: JoinKind::Left,
                on: WhereExpr::ExprCompare {
                    lhs,
                    op: Op::Eq,
                    rhs,
                },
                project: vec![],
            })
            .fetch(pool)
            .await
            .expect("the same join is reused");
        assert_eq!(rows.iter().map(|c| c.id).collect::<Vec<_>>(), [2]);
    }
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
        .select_related(
            "the_middle_relation_with_a_much_longer_field_name__second_hop_relation_one",
        )
        .select_related(
            "the_middle_relation_with_a_much_longer_field_name__second_hop_relation_two",
        )
        .filter(
            "the_middle_relation_with_a_much_longer_field_name__second_hop_relation_two__label",
            "two",
        )
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
    assert!(
        matches!(err, ExecError::InListUnsplittable { .. }),
        "{err:?}"
    );
}

/// `prefetch_soft` takes the FK's column, which may differ from its field.
async fn prefetch_soft_by_a_renamed_column(pool: &Pool) {
    Owner { id: 1 }.insert_pool(pool).await.expect("owner");
    Pet {
        id: 1,
        owner: ForeignKey::unloaded(1),
    }
    .insert_pool(pool)
    .await
    .expect("pet");
    let grouped =
        rustango::contenttypes::prefetch_soft::<Pet, _>(pool, &[1], "owner_ref", |p| p.owner.pk())
            .await
            .expect("prefetch_soft by column");
    assert_eq!(grouped.get(&1).map(Vec::len), Some(1));
}

/// `IN` lists match the same rows on every backend, NULLs included; on PG
/// they bind one array, so each list length reuses one statement.
async fn in_lists_match_alike_and_share_a_statement(pool: &Pool) {
    use rustango::core::SqlValue;
    let ids: Vec<uuid::Uuid> = (0..4).map(|_| uuid::Uuid::new_v4()).collect();
    for (i, id) in ids.iter().enumerate() {
        let label = (i != 3).then(|| format!("t{i}"));
        Token {
            id: *id,
            label,
            rank: i as i32,
        }
        .insert_pool(pool)
        .await
        .expect("token");
    }
    let count = |col: &'static str, op: Op, list: Vec<SqlValue>| {
        Token::objects().filter_op(col, op, SqlValue::List(list))
    };
    let uuids = |xs: &[usize]| {
        xs.iter()
            .map(|&i| SqlValue::Uuid(ids[i]))
            .collect::<Vec<_>>()
    };
    let strs = |xs: &[&str]| {
        xs.iter()
            .map(|s| SqlValue::String((*s).into()))
            .collect::<Vec<_>>()
    };
    let n = |qs: rustango::query::QuerySet<Token>| async move {
        qs.fetch(pool).await.expect("fetch").len()
    };
    assert_eq!(n(count("id", Op::In, uuids(&[0, 2]))).await, 2);
    assert_eq!(n(count("id", Op::NotIn, uuids(&[0]))).await, 3);
    assert_eq!(
        n(count(
            "rank",
            Op::In,
            vec![SqlValue::I32(1), SqlValue::I32(3)]
        ))
        .await,
        2
    );
    assert_eq!(
        n(count(
            "rank",
            Op::In,
            vec![SqlValue::I64(1), SqlValue::I64(9)]
        ))
        .await,
        1
    );
    assert_eq!(
        n(count("label", Op::In, strs(&["t0", "t1", "zz"]))).await,
        2
    );
    // NOT IN over a NULL row or a NULL element matches nothing for that row.
    assert_eq!(n(count("label", Op::NotIn, strs(&["t0"]))).await, 2);
    let with_null = vec![SqlValue::String("t0".into()), SqlValue::Null];
    assert_eq!(n(count("label", Op::NotIn, with_null)).await, 0);

    let sql = |len: usize| {
        let q = count("id", Op::In, uuids(&(0..len).collect::<Vec<_>>()))
            .compile()
            .expect("compile");
        pool.dialect().compile_select(&q).expect("sql")
    };
    let (two, four) = (sql(2), sql(4));
    if pool.dialect().name() == "postgres" {
        assert_eq!(two.sql, four.sql);
        assert_eq!(four.params.len(), 1);
    } else {
        assert_eq!(four.params.len(), 4);
    }
}

/// Reverse generic prefetch groups by the PK column's row key.
async fn prefetch_reverse_generic_by_a_renamed_column(pool: &Pool) {
    use rustango::contenttypes::{self, ContentType};
    contenttypes::ensure_seeded(pool)
        .await
        .expect("seed content types");
    let ct = ContentType::for_model::<Owner>(pool)
        .await
        .unwrap()
        .expect("owner ct");
    Note {
        id: 1,
        ct_id: *ct.id.get().expect("ct id"),
        object_pk: 1,
    }
    .insert_pool(pool)
    .await
    .expect("note");
    let grouped =
        contenttypes::prefetch_reverse_generic_for::<Owner>(pool, Note::SCHEMA, &[1], None)
            .await
            .expect("reverse generic prefetch");
    assert_eq!(grouped.get(&1).map(Vec::len), Some(1), "{grouped:?}");
}

/// `__week` is the ISO 8601 week on every backend.
async fn week_is_iso(pool: &Pool) {
    let days = [
        (1, NaiveDate::from_ymd_opt(2023, 1, 1).unwrap()),
        (2, NaiveDate::from_ymd_opt(2021, 1, 3).unwrap()),
        (3, NaiveDate::from_ymd_opt(2024, 12, 30).unwrap()),
    ];
    for (id, d) in days {
        let at = Utc.from_utc_datetime(&d.and_hms_opt(12, 0, 0).unwrap());
        Day { id, d, at }.insert_pool(pool).await.expect("day");
    }
    for (week, id) in [(52_i64, 1), (53, 2), (1, 3)] {
        for field in ["d__week", "at__week"] {
            let ids: Vec<i64> = Day::objects()
                .filter(field, week)
                .fetch(pool)
                .await
                .expect("week filter")
                .into_iter()
                .map(|d| d.id)
                .collect();
            assert_eq!(ids, [id], "{field} = {week}");
        }
    }
}

/// An `f32` reads as its own digits in JSON rows and audit changes.
async fn f32_json_is_not_widened(pool: &Pool) {
    Gauge { id: 1, ratio: 0.1 }
        .insert_pool(pool)
        .await
        .expect("gauge");
    let select = Gauge::objects().compile().expect("compile");
    let fields: Vec<_> = Gauge::SCHEMA.scalar_fields().collect();
    let rows = rustango::sql::select_rows_as_json(pool, &select, &fields)
        .await
        .expect("json rows");
    assert_eq!(rows[0]["ratio"], serde_json::json!(0.1), "{rows:?}");
    let audit = rustango::audit::fetch_for_entity_pool(pool, Gauge::SCHEMA.table, "1")
        .await
        .expect("audit");
    assert_eq!(
        audit[0].changes["ratio"],
        serde_json::json!(0.1),
        "{audit:?}"
    );
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        week_is_iso,
        f32_json_is_not_widened,
        fk_get_follows_the_on_column,
        ad_hoc_alias_clashing_a_relation_filter,
        ad_hoc_join_equal_to_the_relation_join,
        long_select_related_chains,
        in_bulk_on_past_the_bind_cap,
        prefetch_soft_by_a_renamed_column,
        prefetch_reverse_generic_by_a_renamed_column,
        in_lists_match_alike_and_share_a_statement,
    ],
}
