//! Relation fetches on every backend: NULL FKs under `select_related` (#2293),
//! shared select_related hops (#2294), bind-cap batching (#2295), M2M `set`
//! with repeated ids (#2297) and `prefetch_generic` on an i32 PK (#2298).

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::core::{BulkInsertQuery, FieldSchema, FieldType, Model as _, ModelSchema, SqlValue};
use rustango::sql::{
    bulk_insert_pool, Auto, CounterPool as _, ExecError, FetcherPool as _, ForeignKey, Pool,
};
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(table = "rel2293_profile", app = "rel2293")]
pub struct Profile {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 40)]
    pub bio: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "rel2293_editor", app = "rel2293")]
pub struct Editor {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 40)]
    pub name: String,
    pub profile: ForeignKey<Profile>,
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
    drop_table(pool, Article::SCHEMA.table).await;
    drop_table(pool, Editor::SCHEMA.table).await;
    fresh_table::<Profile>(pool).await;
    fresh_table::<Editor>(pool).await;
    fresh_table::<Article>(pool).await;
    fresh_table::<Row>(pool).await;
    fresh_table::<PostTag>(pool).await;
    fresh_table::<Badge>(pool).await;
}

/// Article 1 has no editor; article 2 is edited by "Ada".
async fn seed_articles(pool: &Pool) {
    Profile {
        id: 1,
        bio: "bio".into(),
    }
    .insert_pool(pool)
    .await
    .expect("profile");
    Editor {
        id: 1,
        name: "Ada".into(),
        profile: ForeignKey::unloaded(1),
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
    let rows: Vec<Vec<SqlValue>> = (0..10).map(|i| vec![i.into(), i.into()]).collect();
    bulk_insert_pool(
        pool,
        &BulkInsertQuery::new(Row::SCHEMA, vec!["id", "n"], rows),
    )
    .await
    .expect("seed");
    let got = Row::objects()
        .in_bulk(Row::id, 0..70_000_i64, |r| r.id, pool)
        .await
        .expect("in_bulk batches its IN list");
    assert_eq!(got.len(), 10);
}

/// Batching would apply the limit per batch, so it is refused instead.
async fn sliced_in_bulk_past_the_bind_cap_is_refused(pool: &Pool) {
    let err = Row::objects()
        .limit(5)
        .in_bulk(Row::id, 0..70_000_i64, |r| r.id, pool)
        .await
        .expect_err("a sliced queryset cannot be split");
    assert!(
        matches!(err, ExecError::InListTooLongForSlice { keys: 70_000, .. }),
        "{err}"
    );
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
    let map = contenttypes::prefetch_generic::<Badge>(pool, &[(ct_id, pk)])
        .await
        .expect("prefetch_generic");
    assert_eq!(
        map.get(&(ct_id, pk)).map(|b| b.label.as_str()),
        Some("gold")
    );
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        null_fk_select_related_skips_the_row,
        null_fk_multihop_select_related,
        null_fk_order_by_relation,
        dangling_fk_select_related_fails,
        shared_first_hop_joins_once,
        bulk_update_past_the_bind_cap,
        in_bulk_past_the_bind_cap,
        sliced_in_bulk_past_the_bind_cap_is_refused,
        m2m_set_ignores_repeated_ids,
        prefetch_generic_keeps_i32_pks,
    ],
}
