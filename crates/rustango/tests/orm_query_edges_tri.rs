//! ORM edges on every backend: FK `get` under `on`, join alias clashes, long
//! `select_related` chains, `in_bulk_on` past the bind cap, soft / generic
//! prefetch through a renamed column, ISO weeks and `f32` in JSON, and the
//! model shortcuts on small or renamed keys and past the bind cap.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use chrono::{NaiveDate, TimeZone as _, Utc};
use rustango::core::{
    joins::aliased, Column as _, Join, JoinKind, Model as _, Op, QueryError, WhereExpr,
};
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

/// Holds a bucket: deleting it is refused.
#[derive(Model, Debug, Clone)]
#[rustango(table = "oqe_bucket_ref", app = "oqe")]
pub struct BucketRef {
    #[rustango(primary_key)]
    pub id: i64,
    pub bucket: ForeignKey<Bucket>,
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

fn shown_only() -> WhereExpr {
    WhereExpr::Predicate(rustango::core::Filter::new(
        "shown",
        Op::Eq,
        rustango::core::SqlValue::Bool(true),
    ))
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "oqe_doc", app = "oqe", global_scope(name = "shown", apply = shown_only))]
pub struct Doc {
    #[rustango(primary_key)]
    pub id: i64,
    pub shown: bool,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "oqe_remark", app = "oqe", global_scope(name = "shown", apply = shown_only))]
#[rustango(generic_fk(name = "target", ct_column = "ct_id", pk_column = "object_pk"))]
pub struct Remark {
    #[rustango(primary_key)]
    pub id: i64,
    pub ct_id: i64,
    pub object_pk: i64,
    pub shown: bool,
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

#[derive(Model, Debug, Clone)]
#[rustango(table = "oqe_item", app = "oqe")]
pub struct Item {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 20)]
    pub category: String,
    #[rustango(max_length = 40)]
    pub title: String,
    #[rustango(max_length = 20)]
    pub middle: Option<String>,
    #[rustango(max_length = 40)]
    pub out: Option<String>,
    pub score: f64,
    pub created_at: chrono::DateTime<Utc>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "oqe_ev", app = "oqe")]
pub struct Ev {
    #[rustango(primary_key)]
    pub id: i64,
    pub at: Option<chrono::DateTime<Utc>>,
    pub d: Option<NaiveDate>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "oqe_small", app = "oqe")]
pub struct Small {
    #[rustango(primary_key)]
    pub id: rustango::sql::Auto<i32>,
    pub n: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "oqe_renamed", app = "oqe")]
pub struct Renamed {
    #[rustango(primary_key, column = "renamed_id")]
    pub id: rustango::sql::Auto<i64>,
    pub n: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "oqe_code", app = "oqe")]
pub struct Code {
    #[rustango(primary_key, max_length = 20)]
    pub code: String,
}

async fn setup(pool: &Pool) {
    use rustango::testkit::matrix::{drop_table, fresh_table};
    for t in [
        City::SCHEMA,
        Top::SCHEMA,
        Mid::SCHEMA,
        Pet::SCHEMA,
        BucketRef::SCHEMA,
    ] {
        drop_table(pool, t.table).await;
    }
    fresh_table::<Country>(pool).await;
    fresh_table::<City>(pool).await;
    fresh_table::<Leaf>(pool).await;
    fresh_table::<Mid>(pool).await;
    fresh_table::<Top>(pool).await;
    fresh_table::<Bucket>(pool).await;
    fresh_table::<BucketRef>(pool).await;
    fresh_table::<Owner>(pool).await;
    fresh_table::<Pet>(pool).await;
    fresh_table::<Note>(pool).await;
    fresh_table::<Day>(pool).await;
    fresh_table::<Gauge>(pool).await;
    fresh_table::<Token>(pool).await;
    fresh_table::<Doc>(pool).await;
    fresh_table::<Remark>(pool).await;
    fresh_table::<Item>(pool).await;
    fresh_table::<Small>(pool).await;
    fresh_table::<Ev>(pool).await;
    fresh_table::<Renamed>(pool).await;
    fresh_table::<Code>(pool).await;
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

/// `in_bulk_on` refuses one `IN` list past the bind cap instead of sending it;
/// PG binds the keys as one array.
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
    let got = Bucket::objects()
        .in_bulk_on(Bucket::id, 0..70_000_i64, |b| b.id, &mut conn)
        .await;
    if pool.dialect().name() == "postgres" {
        assert_eq!(got.expect("one array bind on PG").len(), 2);
        return;
    }
    let err = got.expect_err("past the bind cap");
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

/// Every generic-relation read applies the target's global scopes, as
/// `prefetch_generic` does: a row a `QuerySet` hides is hidden by each path.
async fn generic_reads_apply_global_scopes(pool: &Pool) {
    use rustango::contenttypes::{self, ContentType, GenericForeignKey};
    contenttypes::ensure_seeded(pool)
        .await
        .expect("seed content types");
    let ct = ContentType::for_model::<Doc>(pool)
        .await
        .unwrap()
        .expect("doc ct");
    let ct_id = *ct.id.get().expect("ct id");
    for (id, shown) in [(1, true), (2, false)] {
        Doc { id, shown }.insert_pool(pool).await.expect("doc");
        Remark {
            id,
            ct_id,
            object_pk: 1,
            shown,
        }
        .insert_pool(pool)
        .await
        .expect("remark");
    }
    let get = |pk| async move {
        GenericForeignKey::new(ct_id, pk)
            .get_object(pool)
            .await
            .expect("get_object")
    };
    assert!(get(1).await.is_some());
    assert_eq!(get(2).await, None, "get_object read a scoped-out row");
    let prefetched = contenttypes::prefetch_generic::<Doc>(pool, &[(ct_id, 1), (ct_id, 2)])
        .await
        .expect("prefetch_generic");
    assert_eq!(prefetched.len(), 1);

    let ids = |rows: &[serde_json::Value]| -> Vec<i64> {
        rows.iter().filter_map(|r| r["id"].as_i64()).collect()
    };
    let reverse = contenttypes::fetch_reverse_generic(pool, Remark::SCHEMA, ct_id, 1, None)
        .await
        .expect("reverse");
    assert_eq!(ids(&reverse), [1], "{reverse:?}");
    let grouped =
        contenttypes::prefetch_reverse_generic_for::<Doc>(pool, Remark::SCHEMA, &[1], None)
            .await
            .expect("reverse prefetch");
    assert_eq!(ids(&grouped[&1]), [1], "{grouped:?}");
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

/// Rows `(id, category, title, middle)`, one second apart, score 1.23456.
async fn items(pool: &Pool, rows: &[(i64, &str, &str, Option<&str>)]) {
    let base = Utc.with_ymd_and_hms(2026, 2, 28, 23, 30, 0).unwrap();
    for &(id, category, title, middle) in rows {
        Item {
            id,
            category: category.into(),
            title: title.into(),
            middle: middle.map(Into::into),
            out: None,
            score: 1.23456,
            created_at: base + chrono::Duration::seconds(id),
        }
        .insert_pool(pool)
        .await
        .expect("item");
    }
}

/// A DISTINCT `values` ordered by a column outside the projection.
async fn distinct_values_ordered_by_another_column(pool: &Pool) {
    items(
        pool,
        &[
            (1, "a", "t", None),
            (2, "b", "t", None),
            (3, "a", "t", None),
        ],
    )
    .await;
    let got: Vec<String> = Item::objects()
        .distinct()
        .order_by(&[("created_at", true)])
        .values_list_flat("category")
        .fetch::<String>(pool)
        .await
        .expect("PG and MySQL want the ORDER BY column selected");
    assert_eq!(got, ["a", "b", "a"]);
}

/// A page past the last row still reports the total.
async fn page_past_the_end_keeps_its_total(pool: &Pool) {
    items(
        pool,
        &[
            (1, "a", "t", None),
            (2, "a", "t", None),
            (3, "a", "t", None),
        ],
    )
    .await;
    let page = Item::objects()
        .order_by(&[("id", false)])
        .limit(2)
        .offset(10)
        .fetch_paginated_on(pool)
        .await
        .expect("page");
    assert!(page.rows.is_empty());
    assert_eq!(page.total, 3);
    let mut conn = pool.acquire().await.expect("conn");
    let page = Item::objects()
        .limit(2)
        .offset(10)
        .fetch_paginated_on(&mut conn)
        .await
        .expect("page on a connection");
    assert_eq!(page.total, 3);
}

/// `dates()` / `datetimes()` bucket in UTC whatever the PG session zone.
async fn date_buckets_are_utc(pool: &Pool) {
    use rustango::query::{DateKind, DateTimeKind};
    items(pool, &[(0, "a", "t", None)]).await;
    #[cfg(feature = "postgres")]
    let kyiv;
    #[cfg(feature = "postgres")]
    let pool = match pool.as_postgres() {
        // One connection, so the session zone holds for every query.
        Some(_) => {
            let url = std::env::var("DATABASE_URL").expect("the PG arm runs on DATABASE_URL");
            let one = rustango::sql::sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .connect(&url)
                .await
                .expect("one connection");
            rustango::sql::sqlx::query("SET TIME ZONE 'Europe/Kyiv'")
                .execute(&one)
                .await
                .expect("session zone");
            kyiv = Pool::from(one);
            &kyiv
        }
        None => pool,
    };
    let months =
        rustango::sql::fetch_dates_pool(pool, Item::objects().dates("created_at", DateKind::Month))
            .await
            .expect("dates");
    assert_eq!(months, [NaiveDate::from_ymd_opt(2026, 2, 1).unwrap()]);
    let days = rustango::sql::fetch_datetimes_pool(
        pool,
        Item::objects().datetimes("created_at", DateTimeKind::Day),
    )
    .await
    .expect("datetimes");
    assert_eq!(days, [Utc.with_ymd_and_hms(2026, 2, 28, 0, 0, 0).unwrap()]);
}

/// `dates()` / `datetimes()` skip NULL values of a nullable column.
async fn date_buckets_skip_nulls(pool: &Pool) {
    use rustango::query::{DateKind, DateTimeKind};
    let at = Utc.with_ymd_and_hms(2026, 3, 14, 9, 30, 0).unwrap();
    let d = NaiveDate::from_ymd_opt(2026, 3, 14).unwrap();
    for ev in [
        Ev {
            id: 1,
            at: Some(at),
            d: Some(d),
        },
        Ev {
            id: 2,
            at: None,
            d: None,
        },
    ] {
        ev.insert_pool(pool).await.expect("ev");
    }
    let months = rustango::sql::fetch_dates_pool(pool, Ev::objects().dates("at", DateKind::Month))
        .await
        .expect("dates on a nullable datetime");
    assert_eq!(months, [NaiveDate::from_ymd_opt(2026, 3, 1).unwrap()]);
    let days = rustango::sql::fetch_dates_pool(pool, Ev::objects().dates("d", DateKind::Day))
        .await
        .expect("dates on a nullable date");
    assert_eq!(days, [d]);
    let hours = rustango::sql::fetch_datetimes_pool(
        pool,
        Ev::objects().datetimes("at", DateTimeKind::Hour),
    )
    .await
    .expect("datetimes on a nullable datetime");
    assert_eq!(hours, [Utc.with_ymd_and_hms(2026, 3, 14, 9, 0, 0).unwrap()]);
}

/// PARTITION BY / ORDER BY of a window take the base table beside a join.
async fn window_columns_beside_a_join(pool: &Pool) {
    Owner { id: 1 }.insert_pool(pool).await.expect("owner");
    for id in [1, 2] {
        Pet {
            id,
            owner: ForeignKey::unloaded(1),
        }
        .insert_pool(pool)
        .await
        .expect("pet");
    }
    let rows = Pet::objects()
        .join(Join {
            target: Owner::SCHEMA,
            alias: "owner",
            kind: JoinKind::Inner,
            on: WhereExpr::ExprCompare {
                lhs: aliased("owner", "id"),
                op: Op::Eq,
                rhs: aliased("oqe_pet", "owner_ref"),
            },
            project: vec![],
        })
        .aggregate()
        .group_by("id")
        .group_by("owner_ref")
        .annotate(
            "rn",
            rustango::core::window::row_number()
                .partition_by("owner_ref")
                .order_by(&[("id", true)])
                .into(),
        )
        .fetch(pool)
        .await
        .expect("`id` is not ambiguous");
    assert_eq!(rows.len(), 2);
}

/// `contains` / `startswith` / `endswith` and `like` match case.
async fn like_lookups_match_case(pool: &Pool) {
    items(
        pool,
        &[
            (1, "a", "Rust", None),
            (2, "a", "rust", None),
            (3, "a", "RUST", None),
            (4, "a", "a_b*?[x]", None),
        ],
    )
    .await;
    let ids = |rows: Vec<Item>| {
        let mut v: Vec<i64> = rows.iter().map(|i| i.id).collect();
        v.sort_unstable();
        v
    };
    for (lookup, value, want) in [
        ("title__contains", "Rust", vec![1]),
        ("title__startswith", "ru", vec![2]),
        ("title__endswith", "ST", vec![3]),
        ("title__contains", "*?[", vec![4]),
        ("title__contains", "_", vec![4]),
        ("title__icontains", "rust", vec![1, 2, 3]),
        ("title__like", "R%t", vec![1]),
    ] {
        let rows = Item::objects()
            .filter(lookup, value)
            .fetch(pool)
            .await
            .expect(lookup);
        assert_eq!(ids(rows), want, "{lookup} {value}");
    }
}

/// `UPDATE … SET out = expr` for row `id`, then read `out` back.
async fn set_out(pool: &Pool, id: i64, expr: rustango::core::Expr) -> Option<String> {
    Item::objects()
        .filter("id", id)
        .update()
        .set_expr("out", expr)
        .execute_on(pool)
        .await
        .expect("update");
    Item::objects()
        .filter("id", id)
        .fetch(pool)
        .await
        .expect("read")[0]
        .out
        .clone()
}

/// `concat` with a NULL operand is NULL on every backend.
async fn concat_with_null_is_null(pool: &Pool) {
    use rustango::core::{funcs::concat, F};
    items(pool, &[(1, "a", "Ann", None), (2, "a", "Bo", Some("B"))]).await;
    let full = || concat([F("title").into(), " ".into(), F("middle").into()]);
    assert_eq!(set_out(pool, 1, full()).await, None);
    assert_eq!(set_out(pool, 2, full()).await.as_deref(), Some("Bo B"));
}

/// `round_to` on a float column runs on PG too.
async fn round_to_on_a_float(pool: &Pool) {
    use rustango::core::{funcs::round_to, F};
    items(pool, &[(1, "a", "t", None)]).await;
    Item::objects()
        .update()
        .set_expr("score", round_to(F("score"), 2_i32))
        .execute_on(pool)
        .await
        .expect("round(double precision, int)");
    let score = Item::objects().fetch(pool).await.expect("read")[0].score;
    assert!((score - 1.23).abs() < 1e-9, "{score}");
}

/// `lpad` cuts a long input on the right and repeats a fill from its start.
async fn lpad_matches_pg(pool: &Pool) {
    use rustango::core::{funcs::lpad, F};
    items(pool, &[(1, "a", "hello", None), (2, "a", "x", None)]).await;
    let pad = |len: i32, fill: &str| lpad(F("title"), len, fill.to_owned());
    assert_eq!(set_out(pool, 1, pad(3, "x")).await.as_deref(), Some("hel"));
    assert_eq!(
        set_out(pool, 2, pad(5, "ab")).await.as_deref(),
        Some("ababx")
    );
}

/// XOR counts a NULL operand as false, with two operands or more.
async fn xor_counts_null_as_false(pool: &Pool) {
    items(pool, &[(1, "a", "t", None), (2, "a", "t", Some("m"))]).await;
    let a = || Item::middle.eq(Some("m".to_owned()));
    let two: Vec<i64> = Item::objects()
        .where_(a().xor(Item::id.eq(1)))
        .fetch(pool)
        .await
        .expect("two")
        .iter()
        .map(|i| i.id)
        .collect();
    let three: Vec<i64> = Item::objects()
        .where_(a().xor(Item::id.eq(1)).xor(Item::id.eq(-1)))
        .fetch(pool)
        .await
        .expect("three")
        .iter()
        .map(|i| i.id)
        .collect();
    let mut both = [two, three];
    for v in &mut both {
        v.sort_unstable();
    }
    assert_eq!(both[0], [1, 2], "NULL xor TRUE keeps the row");
    assert_eq!(both[0], both[1]);
}

/// A field set twice keeps the last value; Postgres refuses two assignments.
async fn update_set_twice_keeps_the_last(pool: &Pool) {
    use rustango::core::F;
    items(pool, &[(1, "a", "t", None), (2, "a", "t", None)]).await;
    Item::objects()
        .update()
        .set("score", 7.0_f64)
        .set("score", 9.0_f64)
        .execute_on(pool)
        .await
        .expect("set twice");
    Item::objects()
        .filter("id", 1_i64)
        .update()
        .set("score", 1.0_f64)
        .set_expr("score", F("score") + 2.0_f64)
        .execute_on(pool)
        .await
        .expect("set then set_expr");
    let mut scores: Vec<(i64, f64)> = Item::objects()
        .fetch(pool)
        .await
        .expect("read")
        .iter()
        .map(|i| (i.id, i.score))
        .collect();
    scores.sort_by_key(|(id, _)| *id);
    assert_eq!(scores, [(1, 11.0), (2, 9.0)]);
}

/// `chunk`, `chunk_by_id` and `each` on an `i32` key and on a renamed key.
async fn chunk_shortcuts_on_small_and_renamed_keys(pool: &Pool) {
    use rustango::sql::Auto;
    for n in 0..5 {
        Small { id: Auto::Unset, n }
            .insert_pool(pool)
            .await
            .expect("small");
        Renamed { id: Auto::Unset, n }
            .insert_pool(pool)
            .await
            .expect("renamed");
    }
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let take = || std::mem::take(&mut *seen.lock().unwrap());
    let rows = |s: &std::sync::Arc<std::sync::Mutex<Vec<i64>>>| {
        let s = s.clone();
        move |batch: Vec<i64>| {
            s.lock().unwrap().extend(batch);
            async { Ok::<(), ExecError>(()) }
        }
    };
    let ns = |v: Vec<Small>| v.into_iter().map(|r| r.n).collect::<Vec<_>>();
    let rn = |v: Vec<Renamed>| v.into_iter().map(|r| r.n).collect::<Vec<_>>();
    let all = vec![0, 1, 2, 3, 4];

    let add = rows(&seen);
    Small::chunk_by_id(2, pool, |b| add(ns(b)))
        .await
        .expect("i32 chunk_by_id");
    assert_eq!(take(), all, "i32 chunk_by_id");
    let add = rows(&seen);
    Small::each(2, pool, |r| add(vec![r.n]))
        .await
        .expect("i32 each");
    assert_eq!(take(), all, "i32 each");

    let add = rows(&seen);
    Renamed::chunk(2, pool, |b| add(rn(b)))
        .await
        .expect("renamed chunk");
    assert_eq!(take(), all, "renamed chunk");
    let add = rows(&seen);
    Renamed::chunk_by_id(2, pool, |b| add(rn(b)))
        .await
        .expect("renamed chunk_by_id");
    assert_eq!(take(), all, "renamed chunk_by_id");
    let add = rows(&seen);
    Renamed::each(2, pool, |r| add(vec![r.n]))
        .await
        .expect("renamed each");
    assert_eq!(take(), all, "renamed each");
}

/// The key-list shortcuts split a list past the bind cap; PG binds one array.
async fn key_list_shortcuts_past_the_bind_cap(pool: &Pool) {
    for id in [1, 2, 3] {
        Bucket { id }.insert_pool(pool).await.expect("bucket");
    }
    let many = || (1..=70_000_i64).collect::<Vec<_>>();
    let ids = |rows: Vec<Bucket>| {
        let mut v: Vec<i64> = rows.iter().map(|b| b.id).collect();
        v.sort_unstable();
        v
    };
    let found = Bucket::find_many(many(), pool).await.expect("find_many");
    assert_eq!(ids(found), [1, 2, 3]);
    let found = Bucket::where_in("id", many(), pool)
        .await
        .expect("where_in");
    assert_eq!(ids(found), [1, 2, 3]);
    let missing = Bucket::find_many_or_fail(many(), pool).await;
    assert!(
        matches!(
            missing,
            Err(ExecError::Driver(rustango::sql::sqlx::Error::RowNotFound))
        ),
        "{missing:?}"
    );
    let gone = Bucket::destroy(many().into_iter().filter(|id| *id != 2), pool)
        .await
        .expect("destroy");
    assert_eq!(gone, 2);
    assert_eq!(ids(Bucket::objects().fetch(pool).await.expect("left")), [2]);
}

/// Keys a collation folds together, split into two batches, still return
/// the row once: MySQL matches `ABC` to `abc`.
async fn split_key_lists_return_each_row_once(pool: &Pool) {
    Code { code: "abc".into() }
        .insert_pool(pool)
        .await
        .expect("code");
    let keys = || {
        let mut v = vec!["abc".to_owned()];
        v.extend((0..70_000).map(|i| format!("k{i}")));
        v.push("ABC".to_owned());
        v
    };
    let found = Code::find_many(keys(), pool).await.expect("find_many");
    assert_eq!(found.len(), 1, "find_many: {found:?}");
    let found = Code::where_in("code", keys(), pool)
        .await
        .expect("where_in");
    assert_eq!(found.len(), 1, "where_in: {found:?}");
}

/// `where_not_in` cannot split its list: past the bind cap it is refused,
/// unless dropping duplicates makes it fit. PG binds one array.
async fn where_not_in_past_the_bind_cap(pool: &Pool) {
    for id in [1, 2, 3] {
        Bucket { id }.insert_pool(pool).await.expect("bucket");
    }
    let ids = |rows: Vec<Bucket>| {
        let mut v: Vec<i64> = rows.iter().map(|b| b.id).collect();
        v.sort_unstable();
        v
    };
    let repeated = vec![2_i64; 70_000];
    let left = Bucket::where_not_in("id", repeated, pool)
        .await
        .expect("duplicates fit");
    assert_eq!(ids(left), [1, 3]);
    let got = Bucket::where_not_in("id", 3..70_003_i64, pool).await;
    if pool.dialect().name() == "postgres" {
        assert_eq!(ids(got.expect("one array bind on PG")), [1, 2]);
        return;
    }
    let err = got.expect_err("past the bind cap");
    assert!(
        matches!(err, ExecError::InListUnsplittable { .. }),
        "{err:?}"
    );
}

/// `destroy` past the bind cap is all or none: a refused last batch keeps
/// the rows the first batch matched.
async fn destroy_past_the_bind_cap_is_all_or_none(pool: &Pool) {
    for id in [1, 70_000] {
        Bucket { id }.insert_pool(pool).await.expect("bucket");
    }
    BucketRef {
        id: 1,
        bucket: ForeignKey::Unloaded(70_000),
    }
    .insert_pool(pool)
    .await
    .expect("ref");
    Bucket::destroy(1..=70_000_i64, pool)
        .await
        .expect_err("the referenced bucket");
    let left: Vec<i64> = Bucket::objects()
        .fetch(pool)
        .await
        .expect("left")
        .iter()
        .map(|b| b.id)
        .collect();
    assert_eq!(left.len(), 2, "{left:?}");
}

/// `latest` / `earliest` skip NULLs on every backend.
async fn latest_and_earliest_skip_nulls(pool: &Pool) {
    let nothing: Option<Item> = Item::objects().latest("middle", pool).await.expect("empty");
    assert!(nothing.is_none());
    items(
        pool,
        &[
            (1, "a", "t", Some("a")),
            (2, "a", "t", None),
            (3, "a", "t", Some("b")),
            (4, "a", "t", None),
            (5, "a", "t", Some("c")),
        ],
    )
    .await;
    let middle = |row: Option<Item>| row.expect("a row").middle;
    let latest = Item::objects()
        .latest("middle", pool)
        .await
        .expect("latest");
    assert_eq!(middle(latest).as_deref(), Some("c"));
    let earliest = Item::objects()
        .earliest("middle", pool)
        .await
        .expect("earliest");
    assert_eq!(middle(earliest).as_deref(), Some("a"));
    let only_nulls = Item::objects()
        .filter("middle__isnull", true)
        .latest("middle", pool)
        .await
        .expect("only NULLs");
    assert!(only_nulls.is_none());
}

/// `last` on a sliced queryset returns the slice's last row.
async fn last_of_a_slice(pool: &Pool) {
    items(
        pool,
        &[
            (1, "a", "t", None),
            (2, "a", "t", None),
            (3, "a", "t", None),
            (4, "a", "t", None),
            (5, "a", "t", None),
        ],
    )
    .await;
    let by_id = || Item::objects().order_by(&[("id", false)]);
    let id = |row: Option<Item>| row.map(|i| i.id);
    assert_eq!(
        id(by_id().limit(2).last(pool).await.expect("limit")),
        Some(2)
    );
    assert_eq!(
        id(by_id().offset(1).last(pool).await.expect("offset")),
        Some(5)
    );
    assert_eq!(
        id(by_id().limit(2).offset(1).last(pool).await.expect("both")),
        Some(3)
    );
    assert_eq!(id(by_id().offset(9).last(pool).await.expect("past")), None);
    assert_eq!(id(by_id().last(pool).await.expect("whole")), Some(5));
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
        generic_reads_apply_global_scopes,
        in_lists_match_alike_and_share_a_statement,
        distinct_values_ordered_by_another_column,
        page_past_the_end_keeps_its_total,
        date_buckets_are_utc,
        date_buckets_skip_nulls,
        window_columns_beside_a_join,
        like_lookups_match_case,
        concat_with_null_is_null,
        round_to_on_a_float,
        lpad_matches_pg,
        xor_counts_null_as_false,
        update_set_twice_keeps_the_last,
        chunk_shortcuts_on_small_and_renamed_keys,
        key_list_shortcuts_past_the_bind_cap,
        destroy_past_the_bind_cap_is_all_or_none,
        split_key_lists_return_each_row_once,
        where_not_in_past_the_bind_cap,
        latest_and_earliest_skip_nulls,
        last_of_a_slice,
    ],
}
