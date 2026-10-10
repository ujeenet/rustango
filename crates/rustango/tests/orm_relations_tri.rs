//! Relation fetches on every backend: NULL FKs under `select_related` (#2293),
//! shared select_related hops (#2294), bind-cap batching (#2295), M2M `set`
//! with repeated ids (#2297), `prefetch_generic` on an i32 PK (#2298),
//! reverse-generic prefetch batching (#2318), bare columns under joins
//! (#2411) and `None` as `IS NULL` (#2413).
//!
//! The cap tests use 33k rows (x2 binds = 66k) or 70k keys: past SQLite's
//! 32,766 and PG/MySQL's 65,535.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::core::{
    BulkInsertQuery, FieldSchema, FieldType, Model as _, ModelSchema, Op, SqlValue,
};
use rustango::sql::{
    bulk_insert_pool, fetch_with_prefetch_filtered, fetch_with_prefetch_pool, Auto,
    CounterPool as _, ExecError, FetcherPool as _, ForeignKey, Pool,
};
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(table = "rel2293_country", app = "rel2293")]
pub struct Country {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 8)]
    pub code: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "rel2293_agency", app = "rel2293")]
pub struct Agency {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 40)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "rel2293_profile", app = "rel2293")]
pub struct Profile {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 40)]
    pub bio: String,
    pub country: Option<ForeignKey<Country>>,
    pub agency: Option<ForeignKey<Agency>>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "rel2293_editor", app = "rel2293")]
pub struct Editor {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 40)]
    pub name: String,
    pub profile: ForeignKey<Profile>,
    pub agency: Option<ForeignKey<Agency>>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "rel2293_article", app = "rel2293")]
pub struct Article {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 40)]
    pub title: String,
    pub editor: Option<ForeignKey<Editor>>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "rel2293_row", app = "rel2293")]
pub struct Row {
    #[rustango(primary_key)]
    pub id: i64,
    pub n: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "rel2293_child", app = "rel2293")]
pub struct Child {
    #[rustango(primary_key)]
    pub id: i64,
    pub row: ForeignKey<Row>,
}

/// Points at any model through a generic FK.
#[derive(Model, Debug, Clone)]
#[rustango(table = "rel2293_note", app = "rel2293")]
#[rustango(generic_fk(name = "target", ct_column = "ct_id", pk_column = "object_pk"))]
pub struct Note {
    #[rustango(primary_key)]
    pub id: i64,
    pub ct_id: i64,
    pub object_pk: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "rel2293_uniq", app = "rel2293")]
pub struct Uniq {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(unique)]
    pub n: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "rel2293_gpost", app = "rel2293")]
#[rustango(generic_m2m(
    name = "tags",
    through = "rel2293_taggables",
    pk_column = "taggable_id",
    ct_column = "taggable_type",
    related_column = "tag_id"
))]
pub struct Gpost {
    #[rustango(primary_key)]
    pub id: i64,
}

/// Shares `id` and `at` with `Shift`, so a bare column is ambiguous.
#[derive(Model, Debug, Clone)]
#[rustango(table = "rel2293_crew", app = "rel2293")]
pub struct Crew {
    #[rustango(primary_key)]
    pub id: i64,
    pub at: chrono::DateTime<chrono::Utc>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "rel2293_shift", app = "rel2293")]
pub struct Shift {
    #[rustango(primary_key)]
    pub id: i64,
    pub at: chrono::DateTime<chrono::Utc>,
    pub crew: Option<ForeignKey<Crew>>,
}

/// The generic M2M pivot, kept out of the model registry.
const TAGGABLES: &ModelSchema = &{
    const FIELDS: &[FieldSchema] = &[
        FieldSchema::new("taggable_id", "taggable_id", FieldType::I64),
        FieldSchema::new("taggable_type", "taggable_type", FieldType::I64),
        FieldSchema::new("tag_id", "tag_id", FieldType::I64),
    ];
    let mut s = ModelSchema::new("Taggable", "rel2293_taggables");
    s.fields = FIELDS;
    s
};

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "rel2293_post",
    app = "rel2293",
    m2m(
        name = "tags",
        to = "rel2293_tag",
        through = "rel2293_post_tag",
        src = "post_id",
        dst = "tag_id"
    )
)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "rel2293_post_tag", app = "rel2293")]
pub struct PostTag {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub post_id: i64,
    pub tag_id: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "rel2293_badge", app = "rel2293")]
pub struct Badge {
    #[rustango(primary_key)]
    pub id: Auto<i32>,
    #[rustango(max_length = 40)]
    pub label: String,
}

/// `rel2293_article` without its FK constraint, so a row can point nowhere.
const LOOSE_ARTICLE: &ModelSchema = &{
    const FIELDS: &[FieldSchema] = &[
        {
            let mut f = FieldSchema::new("id", "id", FieldType::I64);
            f.primary_key = true;
            f
        },
        {
            let mut f = FieldSchema::new("title", "title", FieldType::String);
            f.max_length = Some(40);
            f
        },
        {
            let mut f = FieldSchema::new("editor", "editor", FieldType::I64);
            f.nullable = true;
            f
        },
    ];
    let mut s = ModelSchema::new("LooseArticle", "rel2293_article");
    s.fields = FIELDS;
    s
};

async fn setup(pool: &Pool) {
    use rustango::testkit::matrix::{drop_table, fresh_table};
    for t in [
        Article::SCHEMA,
        Editor::SCHEMA,
        Profile::SCHEMA,
        Child::SCHEMA,
        Shift::SCHEMA,
        TAGGABLES,
    ] {
        drop_table(pool, t.table).await;
    }
    fresh_table::<Country>(pool).await;
    fresh_table::<Agency>(pool).await;
    fresh_table::<Profile>(pool).await;
    fresh_table::<Editor>(pool).await;
    fresh_table::<Article>(pool).await;
    fresh_table::<Row>(pool).await;
    fresh_table::<Child>(pool).await;
    fresh_table::<Uniq>(pool).await;
    fresh_table::<PostTag>(pool).await;
    fresh_table::<Badge>(pool).await;
    fresh_table::<Note>(pool).await;
    fresh_table::<Crew>(pool).await;
    fresh_table::<Shift>(pool).await;
    rustango::testkit::create_tables(pool, &[TAGGABLES])
        .await
        .expect("taggables");
}

/// Article 1 has no editor; article 2 is edited by "Ada", whose profile
/// and agency are set.
async fn seed_articles(pool: &Pool) {
    Country {
        id: 1,
        code: "US".into(),
    }
    .insert_pool(pool)
    .await
    .expect("country");
    for (id, name) in [(1, "Acme"), (2, "Beta")] {
        Agency {
            id,
            name: name.into(),
        }
        .insert_pool(pool)
        .await
        .expect("agency");
    }
    // Profile and editor point at different agencies, so the joins differ.
    Profile {
        id: 1,
        bio: "bio".into(),
        country: Some(ForeignKey::unloaded(1)),
        agency: Some(ForeignKey::unloaded(2)),
    }
    .insert_pool(pool)
    .await
    .expect("profile");
    Editor {
        id: 1,
        name: "Ada".into(),
        profile: ForeignKey::unloaded(1),
        agency: Some(ForeignKey::unloaded(1)),
    }
    .insert_pool(pool)
    .await
    .expect("editor");
    for (id, editor) in [(1, None), (2, Some(ForeignKey::unloaded(1)))] {
        Article {
            id,
            title: format!("a{id}"),
            editor,
        }
        .insert_pool(pool)
        .await
        .expect("article");
    }
}

fn editor_name(a: &Article) -> Option<&str> {
    a.editor
        .as_ref()
        .and_then(|fk| fk.value())
        .map(|e| e.name.as_str())
}

async fn null_fk_select_related_skips_the_row(pool: &Pool) {
    seed_articles(pool).await;
    let rows: Vec<Article> = Article::objects()
        .select_related("editor")
        .order_by(&[("id", false)])
        .fetch(pool)
        .await
        .expect("a NULL FK must not fail the fetch");
    assert_eq!(rows.len(), 2);
    assert!(rows[0].editor.is_none());
    assert_eq!(editor_name(&rows[1]), Some("Ada"));
}

async fn null_fk_multihop_select_related(pool: &Pool) {
    seed_articles(pool).await;
    let rows: Vec<Article> = Article::objects()
        .select_related("editor__profile")
        .order_by(&[("id", false)])
        .fetch(pool)
        .await
        .expect("a NULL first hop must not fail the fetch");
    assert!(rows[0].editor.is_none());
    let editor = rows[1].editor.as_ref().and_then(|fk| fk.value()).unwrap();
    assert_eq!(editor.profile.value().map(|p| p.bio.as_str()), Some("bio"));
}

async fn null_fk_order_by_relation(pool: &Pool) {
    seed_articles(pool).await;
    let rows: Vec<Article> = Article::objects()
        .order_by(&[("editor__name", false), ("id", false)])
        .fetch(pool)
        .await
        .expect("ordering across a NULL FK must not fail the fetch");
    assert_eq!(rows.len(), 2);
}

/// A set FK whose row is missing stays an error; only a NULL FK is skipped.
async fn dangling_fk_select_related_fails(pool: &Pool) {
    rustango::testkit::matrix::drop_table(pool, LOOSE_ARTICLE.table).await;
    rustango::testkit::create_tables(pool, &[LOOSE_ARTICLE])
        .await
        .expect("loose article");
    let row = vec![1_i64.into(), "a".into(), 99_i64.into()];
    let cols = vec!["id", "title", "editor"];
    bulk_insert_pool(pool, &BulkInsertQuery::new(LOOSE_ARTICLE, cols, vec![row]))
        .await
        .expect("dangling row");
    let err = Article::objects()
        .select_related("editor")
        .fetch(pool)
        .await
        .expect_err("a dangling FK must not read as unloaded");
    assert!(err.to_string().contains("missing"), "{err}");
}

async fn shared_first_hop_joins_once(pool: &Pool) {
    seed_articles(pool).await;
    let rows: Vec<Article> = Article::objects()
        .select_related("editor")
        .select_related("editor__profile")
        .order_by(&[("id", false)])
        .fetch(pool)
        .await
        .expect("a hop shared by two select_related names");
    assert_eq!(editor_name(&rows[1]), Some("Ada"));
}

fn editor_of(a: &Article) -> &Editor {
    a.editor
        .as_ref()
        .and_then(|fk| fk.value())
        .expect("editor loaded")
}

fn loaded<T>(fk: &Option<ForeignKey<T>>) -> Option<&T> {
    fk.as_ref().and_then(|fk| fk.value())
}

/// Two chains under one hop both load; the second used to re-decode
/// `editor` and drop what the first stitched on (#2294).
async fn sibling_chains_under_one_hop_both_load(pool: &Pool) {
    seed_articles(pool).await;
    let rows: Vec<Article> = Article::objects()
        .select_related("editor__profile")
        .select_related("editor__agency")
        .order_by(&[("id", false)])
        .fetch(pool)
        .await
        .expect("sibling chains");
    let editor = editor_of(&rows[1]);
    assert_eq!(editor.profile.value().map(|p| p.bio.as_str()), Some("bio"));
    assert_eq!(
        loaded(&editor.agency).map(|a| a.name.as_str()),
        Some("Acme")
    );
}

/// The same three levels deep: both chains share `editor__profile`.
async fn sibling_chains_three_levels_both_load(pool: &Pool) {
    seed_articles(pool).await;
    let rows: Vec<Article> = Article::objects()
        .select_related("editor__profile__country")
        .select_related("editor__profile__agency")
        .select_related("editor__agency")
        .order_by(&[("id", false)])
        .fetch(pool)
        .await
        .expect("three-level sibling chains");
    let editor = editor_of(&rows[1]);
    let profile = editor.profile.value().expect("profile loaded");
    assert_eq!(
        loaded(&profile.country).map(|c| c.code.as_str()),
        Some("US")
    );
    assert_eq!(
        loaded(&profile.agency).map(|a| a.name.as_str()),
        Some("Beta")
    );
    assert_eq!(
        loaded(&editor.agency).map(|a| a.name.as_str()),
        Some("Acme")
    );
}

/// A NULL FK two hops down leaves only that hop unloaded (#2293).
async fn null_fk_on_a_deeper_hop(pool: &Pool) {
    seed_articles(pool).await;
    Profile {
        id: 2,
        bio: "bare".into(),
        country: None,
        agency: None,
    }
    .insert_pool(pool)
    .await
    .expect("bare profile");
    Editor {
        id: 2,
        name: "Bo".into(),
        profile: ForeignKey::unloaded(2),
        agency: None,
    }
    .insert_pool(pool)
    .await
    .expect("editor 2");
    Article {
        id: 3,
        title: "a3".into(),
        editor: Some(ForeignKey::unloaded(2)),
    }
    .insert_pool(pool)
    .await
    .expect("article 3");
    let rows: Vec<Article> = Article::objects()
        .select_related("editor__profile__country")
        .select_related("editor__agency")
        .order_by(&[("id", false)])
        .fetch(pool)
        .await
        .expect("a NULL deeper hop must not fail the fetch");
    let editor = editor_of(&rows[2]);
    let profile = editor.profile.value().expect("profile loaded");
    assert_eq!(profile.bio, "bare");
    assert!(profile.country.is_none());
    assert!(editor.agency.is_none());
}

/// `__isnull` across a NULL FK joins the relation and must not fail (#2293).
async fn isnull_across_a_null_fk(pool: &Pool) {
    seed_articles(pool).await;
    let rows: Vec<Article> = Article::objects()
        .filter("editor__name__isnull", true)
        .fetch(pool)
        .await
        .expect("__isnull across a NULL FK");
    assert_eq!(rows.iter().map(|a| a.id).collect::<Vec<_>>(), vec![1]);
}

fn article_ids(rows: &[Article]) -> Vec<i64> {
    rows.iter().map(|a| a.id).collect()
}

/// `id` is on both joined tables; bare `F` columns must point at the base (#2411).
async fn bare_columns_qualified_under_joins(pool: &Pool) {
    use rustango::core::{funcs::abs, F};
    seed_articles(pool).await;
    let rows: Vec<Article> = Article::objects()
        .select_related("editor")
        .where_column_op("id", Op::Gt, "editor")
        .order_by_expr(abs(F("id")), true)
        .fetch(pool)
        .await
        .expect("where_column_op and order_by_expr under a join");
    assert_eq!(article_ids(&rows), vec![2]);

    let at = |s: &str| s.parse::<chrono::DateTime<chrono::Utc>>().unwrap();
    Crew {
        id: 1,
        at: at("2025-01-01T00:00:00Z"),
    }
    .insert_pool(pool)
    .await
    .expect("crew");
    for (id, when) in [(1, "2024-05-01T00:00:00Z"), (2, "2025-05-01T00:00:00Z")] {
        Shift {
            id,
            at: at(when),
            crew: Some(ForeignKey::unloaded(1)),
        }
        .insert_pool(pool)
        .await
        .expect("shift");
    }
    let shifts: Vec<Shift> = Shift::objects()
        .select_related("crew")
        .filter("at__year", 2025_i64)
        .fetch(pool)
        .await
        .expect("date transform under a join");
    assert_eq!(shifts.iter().map(|s| s.id).collect::<Vec<_>>(), vec![2]);
}

/// `None` as a filter value means `IS NULL`, as in Django (#2413).
async fn none_filter_is_null(pool: &Pool) {
    seed_articles(pool).await;
    let ids = |qs: rustango::query::QuerySet<Article>| async move {
        let rows: Vec<Article> = qs
            .order_by(&[("id", false)])
            .fetch(pool)
            .await
            .expect("None filter");
        article_ids(&rows)
    };
    let none = None::<i64>;
    assert_eq!(
        ids(Article::objects().filter("editor", none)).await,
        vec![1]
    );
    assert_eq!(
        ids(Article::objects().filter("editor__exact", none)).await,
        vec![1]
    );
    assert_eq!(
        ids(Article::objects().exclude("editor", none)).await,
        vec![2]
    );
    assert_eq!(
        ids(Article::objects().filter("editor__ne", none)).await,
        vec![2]
    );
    assert_eq!(
        ids(Article::objects().filter("editor__name", None::<String>)).await,
        vec![1]
    );
}

/// Past every backend's bind cap: 33k rows x 2 binds, 70k `IN` keys.
const ROWS: i64 = 33_000;

async fn bulk_update_past_the_bind_cap(pool: &Pool) {
    let rows: Vec<Vec<SqlValue>> = (0..ROWS).map(|i| vec![i.into(), 0_i64.into()]).collect();
    bulk_insert_pool(
        pool,
        &BulkInsertQuery::new(Row::SCHEMA, vec!["id", "n"], rows),
    )
    .await
    .expect("seed");
    let objs: Vec<Row> = (0..ROWS).map(|id| Row { id, n: 7 }).collect();
    let n = Row::bulk_update(&objs, &["n"], pool)
        .await
        .expect("bulk_update batches under the bind cap");
    assert_eq!(n, ROWS as u64);
    let sevens = Row::objects().filter("n", 7_i64).count(pool).await.unwrap();
    assert_eq!(sevens, ROWS);
}

async fn in_bulk_past_the_bind_cap(pool: &Pool) {
    seed_rows(pool, (0..10).chain([69_999])).await;
    let got = Row::objects()
        .in_bulk(Row::id, 0..70_000_i64, |r| r.id, pool)
        .await
        .expect("in_bulk batches its IN list");
    assert_eq!(got.len(), 11);
    assert!(got.contains_key(&69_999), "a key in the last batch");
}

/// Rows with `n = id`.
async fn seed_rows(pool: &Pool, ids: impl Iterator<Item = i64>) {
    let rows: Vec<Vec<SqlValue>> = ids.map(|i| vec![i.into(), i.into()]).collect();
    bulk_insert_pool(
        pool,
        &BulkInsertQuery::new(Row::SCHEMA, vec!["id", "n"], rows),
    )
    .await
    .expect("seed rows");
}

/// `n IN (..)` over `count` values, so the queryset carries `count` binds.
fn carrying(count: i64) -> rustango::query::QuerySet<Row> {
    let vals: Vec<SqlValue> = (0..count).map(SqlValue::from).collect();
    Row::objects().filter_op("n", Op::In, SqlValue::List(vals))
}

/// The chunk budget leaves room for the binds the queryset already has.
async fn in_bulk_beside_carried_binds(pool: &Pool) {
    seed_rows(pool, (0..10).chain([49_999])).await;
    let got = carrying(20_000)
        .in_bulk(Row::id, 0..50_000_i64, |r| r.id, pool)
        .await
        .expect("chunks fit beside the queryset's own binds");
    assert_eq!(
        got.len(),
        10,
        "id 49_999 has n = 49_999, outside the filter"
    );
}

/// A queryset whose own binds fill the cap cannot take any keys.
async fn carried_binds_alone_over_the_cap_are_refused(pool: &Pool) {
    let err = carrying(70_000)
        .in_bulk(Row::id, [1_i64], |r| r.id, pool)
        .await
        .expect_err("no room for an IN list");
    assert!(
        matches!(err, ExecError::InListUnsplittable { max: 0, .. }),
        "{err}"
    );
}

/// Batching would apply a limit or offset per batch, so it is refused
/// instead, on the query or on the head of a set operation.
async fn sliced_in_bulk_past_the_bind_cap_is_refused(pool: &Pool) {
    let sliced = [
        ("limit", Row::objects().limit(5)),
        ("offset", Row::objects().offset(5)),
        ("head limit", Row::objects().limit(5).union(Row::objects())),
        (
            "head offset",
            Row::objects().offset(5).union(Row::objects()),
        ),
    ];
    for (what, qs) in sliced {
        let err = qs
            .in_bulk(Row::id, 0..70_000_i64, |r| r.id, pool)
            .await
            .expect_err(what);
        assert!(
            matches!(err, ExecError::InListUnsplittable { keys: 70_000, .. }),
            "{what}: {err}"
        );
    }
}

/// 33k parents: past SQLite's budget, which is the smallest.
async fn prefetch_past_the_bind_cap(pool: &Pool) {
    seed_rows(pool, 0..ROWS).await;
    for (id, row) in [(1, 0), (2, ROWS - 1)] {
        Child {
            id,
            row: ForeignKey::unloaded(row),
        }
        .insert_pool(pool)
        .await
        .expect("child");
    }
    let kids_of_last = |groups: Vec<(Row, Vec<Child>)>| {
        assert_eq!(groups.len(), ROWS as usize);
        groups
            .into_iter()
            .find(|(p, _)| p.id == ROWS - 1)
            .map(|(_, kids)| kids.len())
    };
    let all = fetch_with_prefetch_pool::<Row, Child>(Row::objects(), "row", pool)
        .await
        .expect("prefetch batches its IN list");
    assert_eq!(kids_of_last(all), Some(1));
    let ordered = Child::objects().order_by(&[("id", false)]);
    let filtered = fetch_with_prefetch_filtered::<Row, Child>(Row::objects(), "row", ordered, pool)
        .await
        .expect("filtered prefetch batches its IN list");
    assert_eq!(kids_of_last(filtered), Some(1));

    let limited = Child::objects().limit(1);
    let got =
        fetch_with_prefetch_filtered::<Row, Child>(Row::objects(), "row", limited, pool).await;
    if ROWS as usize >= pool.dialect().max_bind_params() {
        assert!(
            matches!(got, Err(ExecError::InListUnsplittable { .. })),
            "a limited child queryset over the cap is refused"
        );
    } else {
        got.expect("under the cap the limit applies once");
    }
}

/// `prefetch_soft` splits its keys too.
async fn prefetch_soft_past_the_bind_cap(pool: &Pool) {
    // Hits in the first and the last batch.
    seed_rows(pool, (0..10).chain([69_999])).await;
    for (id, row) in [(1, 9), (2, 69_999)] {
        Child {
            id,
            row: ForeignKey::unloaded(row),
        }
        .insert_pool(pool)
        .await
        .expect("child");
    }
    let keys: Vec<i64> = (0..70_000).collect();
    let grouped =
        rustango::contenttypes::prefetch_soft::<Child, _>(pool, &keys, "row", |c| c.row.pk())
            .await
            .expect("prefetch_soft batches its IN list");
    assert_eq!(grouped.get(&9).map(Vec::len), Some(1));
    assert_eq!(grouped.get(&69_999).map(Vec::len), Some(1));
}

/// A failing later batch rolls back the earlier ones.
async fn bulk_update_is_all_or_none(pool: &Pool) {
    let rows: Vec<Vec<SqlValue>> = (0..ROWS).map(|i| vec![i.into(), i.into()]).collect();
    bulk_insert_pool(
        pool,
        &BulkInsertQuery::new(Uniq::SCHEMA, vec!["id", "n"], rows),
    )
    .await
    .expect("seed");
    // The last two rows land in the last batch and collide on `n`.
    let objs: Vec<Uniq> = (0..ROWS)
        .map(|id| Uniq {
            id,
            n: if id >= ROWS - 2 { -1 } else { id + 1_000_000 },
        })
        .collect();
    Uniq::bulk_update(&objs, &["n"], pool)
        .await
        .expect_err("the last batch breaks the unique key");
    let moved = Uniq::objects()
        .filter("n__gte", 1_000_000_i64)
        .count(pool)
        .await
        .unwrap();
    assert_eq!(moved, 0, "earlier batches rolled back");
}

async fn m2m_set_ignores_repeated_ids(pool: &Pool) {
    let post = Post { id: 1 };
    post.tags_m2m()
        .set(&[3_i64, 3, 4], pool)
        .await
        .expect("a repeated id is linked once");
    let mut tags = post.tags_m2m().all(pool).await.unwrap();
    tags.sort_unstable();
    assert_eq!(tags, vec![3, 4]);
}

async fn generic_m2m_set_ignores_repeated_ids(pool: &Pool) {
    rustango::contenttypes::ensure_seeded(pool)
        .await
        .expect("seed content types");
    rustango::contenttypes::clear_cache();
    let post = Gpost { id: 1 };
    post.tags_m2m()
        .set(&[3_i64, 3, 4], pool)
        .await
        .expect("a repeated id is linked once");
    let mut tags = post.tags_m2m().all(pool).await.unwrap();
    tags.sort_unstable();
    assert_eq!(tags, vec![3, 4]);
}

async fn prefetch_generic_keeps_i32_pks(pool: &Pool) {
    use rustango::contenttypes::{self, ContentType};
    contenttypes::ensure_seeded(pool)
        .await
        .expect("seed content types");
    let mut badge = Badge {
        id: Auto::default(),
        label: "gold".into(),
    };
    badge.insert_pool(pool).await.expect("badge");
    let pk = i64::from(*badge.id.get().expect("pk"));
    let ct = ContentType::for_model::<Badge>(pool)
        .await
        .unwrap()
        .expect("badge ct");
    let ct_id = *ct.id.get().expect("ct id");
    // A badge past the 70k filler keys, so a hit lands in the last batch.
    Badge {
        id: Auto::from(200_000),
        label: "last".into(),
    }
    .insert_pool(pool)
    .await
    .expect("last badge");
    // 70k pairs: past every backend's bind cap.
    let pairs: Vec<(i64, i64)> = (100_000..170_000)
        .map(|p| (ct_id, p))
        .chain([(ct_id, pk), (ct_id, 200_000)])
        .collect();
    let map = contenttypes::prefetch_generic::<Badge>(pool, &pairs)
        .await
        .expect("prefetch_generic");
    assert_eq!(
        map.get(&(ct_id, pk)).map(|b| b.label.as_str()),
        Some("gold")
    );
    assert_eq!(
        map.get(&(ct_id, 200_000)).map(|b| b.label.as_str()),
        Some("last")
    );
}

/// `prefetch_reverse_generic_for` splits its parent keys too (#2318).
async fn prefetch_reverse_generic_past_the_bind_cap(pool: &Pool) {
    use rustango::contenttypes::{self, ContentType};
    contenttypes::ensure_seeded(pool)
        .await
        .expect("seed content types");
    let ct = ContentType::for_model::<Row>(pool)
        .await
        .unwrap()
        .expect("row ct");
    let ct_id = *ct.id.get().expect("ct id");
    let other = ContentType::for_model::<Badge>(pool)
        .await
        .unwrap()
        .expect("badge ct");
    let other_ct = *other.id.get().expect("other ct id");
    // Hits in the first and the last batch; note 3 is a badge's, not a row's.
    for (id, ct_id, object_pk) in [(1, ct_id, 9), (2, ct_id, 69_999), (3, other_ct, 9)] {
        Note {
            id,
            ct_id,
            object_pk,
        }
        .insert_pool(pool)
        .await
        .expect("note");
    }
    let keys: Vec<i64> = (0..70_000).collect();
    let grouped =
        contenttypes::prefetch_reverse_generic_for::<Row>(pool, Note::SCHEMA, &keys, None)
            .await
            .expect("prefetch_reverse_generic_for batches its IN list");
    assert_eq!(grouped.get(&9).map(Vec::len), Some(1));
    assert_eq!(grouped.get(&69_999).map(Vec::len), Some(1));
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        null_fk_select_related_skips_the_row,
        null_fk_multihop_select_related,
        null_fk_order_by_relation,
        dangling_fk_select_related_fails,
        shared_first_hop_joins_once,
        sibling_chains_under_one_hop_both_load,
        sibling_chains_three_levels_both_load,
        null_fk_on_a_deeper_hop,
        isnull_across_a_null_fk,
        bare_columns_qualified_under_joins,
        none_filter_is_null,
        bulk_update_past_the_bind_cap,
        in_bulk_past_the_bind_cap,
        sliced_in_bulk_past_the_bind_cap_is_refused,
        in_bulk_beside_carried_binds,
        carried_binds_alone_over_the_cap_are_refused,
        prefetch_past_the_bind_cap,
        prefetch_soft_past_the_bind_cap,
        bulk_update_is_all_or_none,
        m2m_set_ignores_repeated_ids,
        generic_m2m_set_ignores_repeated_ids,
        prefetch_generic_keeps_i32_pks,
        prefetch_reverse_generic_past_the_bind_cap,
    ],
}
