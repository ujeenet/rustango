//! Relation edges on every backend: a `ForeignKey<T>` to a renamed PK column,
//! the FK key `select_related` stores, join labels past 63 bytes, ad-hoc join
//! aliases, PG prefetch past the bind cap, and concurrent M2M `set`.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::core::{
    joins::aliased, BulkInsertQuery, Join, JoinKind, Model as _, Op, Relation, SqlValue, WhereExpr,
};
use rustango::sql::{bulk_insert_pool, Auto, FetcherPool as _, ForeignKey, Pool};
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

#[derive(Model, Debug, Clone)]
#[rustango(table = "edg_country", app = "edg")]
pub struct Country {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(unique)]
    pub num: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "edg_city", app = "edg")]
pub struct City {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(on = "num")]
    pub country: ForeignKey<Country>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "edg_lp", app = "edg")]
pub struct LongParent {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 20)]
    pub an_extremely_long_column_name_for_labels: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "edg_lc", app = "edg")]
pub struct LongChild {
    #[rustango(primary_key)]
    pub id: i64,
    pub a_rather_long_foreign_key_field_name_here: ForeignKey<LongParent>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "edg_bucket", app = "edg")]
pub struct Bucket {
    #[rustango(primary_key)]
    pub id: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "edg_drop", app = "edg")]
pub struct Drip {
    #[rustango(primary_key)]
    pub id: i64,
    pub bucket: ForeignKey<Bucket>,
}

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "edg_album",
    app = "edg",
    m2m(
        name = "tags",
        to = "edg_tag",
        through = "edg_album_tag",
        src = "album_id",
        dst = "tag_id"
    )
)]
pub struct Album {
    #[rustango(primary_key)]
    pub id: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "edg_album_tag", app = "edg")]
pub struct AlbumTag {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub album_id: i64,
    pub tag_id: i64,
}

async fn setup(pool: &Pool) {
    use rustango::testkit::matrix::{drop_table, fresh_table};
    for t in [
        Txn::SCHEMA,
        City::SCHEMA,
        LongChild::SCHEMA,
        Drip::SCHEMA,
        AlbumTag::SCHEMA,
    ] {
        drop_table(pool, t.table).await;
    }
    fresh_table::<Country>(pool).await;
    fresh_table::<City>(pool).await;
    fresh_table::<LongParent>(pool).await;
    fresh_table::<LongChild>(pool).await;
    fresh_table::<Bucket>(pool).await;
    fresh_table::<Drip>(pool).await;
    fresh_table::<Album>(pool).await;
    fresh_table::<AlbumTag>(pool).await;
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

async fn seed_city(pool: &Pool) {
    Country { id: 1, num: 100 }
        .insert_pool(pool)
        .await
        .expect("country");
    City {
        id: 1,
        country: ForeignKey::unloaded(100),
    }
    .insert_pool(pool)
    .await
    .expect("city");
}

/// With `on = "num"` the loaded FK keeps `num`, not the parent's PK.
async fn select_related_keeps_the_fk_key(pool: &Pool) {
    seed_city(pool).await;
    let rows: Vec<City> = City::objects()
        .select_related("country")
        .fetch(pool)
        .await
        .expect("fetch");
    let fk = &rows[0].country;
    assert_eq!(fk.value().map(|c| c.id), Some(1));
    assert_eq!(fk.pk(), 100, "the FK still holds the referenced key");
}

/// An ad-hoc join named like an FK field is not stitched as `select_related`.
async fn ad_hoc_join_named_like_an_fk(pool: &Pool) {
    seed_city(pool).await;
    let rows: Vec<City> = City::objects()
        .join(Join {
            target: Country::SCHEMA,
            alias: "country",
            kind: JoinKind::Inner,
            on: WhereExpr::ExprCompare {
                lhs: aliased("country", "num"),
                op: Op::Eq,
                rhs: aliased("edg_city", "country"),
            },
            project: vec!["num"],
        })
        .fetch(pool)
        .await
        .expect("an ad-hoc join is not decoded as a relation");
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].country.is_loaded());
}

async fn seed_long(pool: &Pool) {
    LongParent {
        id: 1,
        an_extremely_long_column_name_for_labels: "deep".into(),
    }
    .insert_pool(pool)
    .await
    .expect("parent");
    LongChild {
        id: 1,
        a_rather_long_foreign_key_field_name_here: ForeignKey::unloaded(1),
    }
    .insert_pool(pool)
    .await
    .expect("child");
}

/// `alias__column` past 63 bytes: PG truncates the label unless it is hashed.
async fn long_join_labels_decode(pool: &Pool) {
    seed_long(pool).await;
    let rows: Vec<LongChild> = LongChild::objects()
        .select_related("a_rather_long_foreign_key_field_name_here")
        .fetch(pool)
        .await
        .expect("a long label decodes");
    let parent = rows[0].a_rather_long_foreign_key_field_name_here.value();
    assert_eq!(
        parent.map(|p| p.an_extremely_long_column_name_for_labels.as_str()),
        Some("deep")
    );
}

/// Dict and JSON rows report a long joined column under its full key.
async fn long_join_labels_keep_their_key(pool: &Pool) {
    seed_long(pool).await;
    let alias = "a_rather_long_foreign_key_field_name_here";
    let key = "a_rather_long_foreign_key_field_name_here__an_extremely_long_column_name_for_labels";
    let joined = || {
        LongChild::objects().join(Join {
            target: LongParent::SCHEMA,
            alias,
            kind: JoinKind::Inner,
            on: WhereExpr::ExprCompare {
                lhs: aliased(alias, "id"),
                op: Op::Eq,
                rhs: aliased("edg_lc", alias),
            },
            project: vec!["an_extremely_long_column_name_for_labels"],
        })
    };
    let select = joined().compile().expect("compile");
    let fields: Vec<_> = LongChild::SCHEMA.scalar_fields().collect();
    let json = rustango::sql::select_rows_as_json(pool, &select, &fields)
        .await
        .expect("json rows");
    assert_eq!(json[0][key], "deep", "json: {json:?}");
    let dicts = joined().values_dict(&["id"]).fetch(pool).await.expect("dicts");
    assert_eq!(
        dicts[0].get(key),
        Some(&SqlValue::String("deep".into())),
        "dicts: {dicts:?}"
    );
}

/// The PG-typed prefetch batches its `IN` list like the pool one.
async fn pg_prefetch_past_the_bind_cap(pool: &Pool) {
    #[cfg(feature = "postgres")]
    #[allow(irrefutable_let_patterns)]
    if let Pool::Postgres(pg) = pool {
        const N: i64 = 70_000;
        let rows: Vec<Vec<SqlValue>> = (0..N).map(|i| vec![i.into()]).collect();
        bulk_insert_pool(
            pool,
            &BulkInsertQuery::new(Bucket::SCHEMA, vec!["id"], rows),
        )
        .await
        .expect("buckets");
        Drip {
            id: 1,
            bucket: ForeignKey::unloaded(N - 1),
        }
        .insert_pool(pool)
        .await
        .expect("drip");
        let groups = rustango::sql::fetch_with_prefetch::<Bucket, Drip>(Bucket::objects(), "bucket", pg)
            .await
            .expect("prefetch past the bind cap");
        assert_eq!(groups.len(), usize::try_from(N).unwrap());
        let last = groups.iter().find(|(b, _)| b.id == N - 1).expect("last");
        assert_eq!(last.1.len(), 1);
    }
    let _ = (pool, bulk_insert_pool, BulkInsertQuery::new);
}

/// Two `set`s racing on one source: the result is one of them, never both.
async fn concurrent_m2m_sets_keep_one(pool: &Pool) {
    Album { id: 1 }.insert_pool(pool).await.expect("album");
    let album = Album { id: 1 };
    for round in 0..40 {
        album.tags_m2m().set(&[9_i64], pool).await.expect("reset");
        let barrier = tokio::sync::Barrier::new(2);
        let run = |ids: &'static [i64]| {
            let barrier = &barrier;
            async move {
                barrier.wait().await;
                Album { id: 1 }.tags_m2m().set(ids, pool).await
            }
        };
        let (a, b) = tokio::join!(run(&[1, 2]), run(&[3, 4]));
        a.unwrap_or_else(|e| panic!("round {round}: first set: {e}"));
        b.unwrap_or_else(|e| panic!("round {round}: second set: {e}"));
        let mut got = album.tags_m2m().all(pool).await.expect("all");
        got.sort_unstable();
        assert!(got == [1, 2] || got == [3, 4], "round {round}: {got:?}");
    }
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        fk_targets_a_renamed_pk_column,
        select_related_keeps_the_fk_key,
        ad_hoc_join_named_like_an_fk,
        long_join_labels_decode,
        long_join_labels_keep_their_key,
        pg_prefetch_past_the_bind_cap,
        concurrent_m2m_sets_keep_one,
    ],
}
