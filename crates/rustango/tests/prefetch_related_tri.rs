//! Batch `prefetch_related` by relation name on every backend: many-to-many
//! and reverse-FK relations load in one query per relation, not one per parent.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::core::{BulkInsertQuery, Model as _, SqlValue};
use rustango::sql::{
    bulk_insert_pool, fetch_with_prefetch_related, fetch_with_prefetch_related_filtered, Auto,
    ExecError, FetcherPool as _, ForeignKey, Pool,
};
use rustango::test_assertions::QueryCounter;
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(table = "pfr_tag", app = "pfr")]
pub struct Tag {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 40)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "pfr_post",
    app = "pfr",
    m2m(
        name = "tags",
        to = "pfr_tag",
        through = "pfr_post_tag",
        src = "post_id",
        dst = "tag_id"
    )
)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "pfr_post_tag", app = "pfr")]
pub struct PostTag {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub post_id: i64,
    pub tag_id: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "pfr_label", app = "pfr")]
pub struct Label {
    #[rustango(primary_key, max_length = 16)]
    pub code: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "pfr_sku",
    app = "pfr",
    m2m(
        name = "labels",
        to = "pfr_label",
        through = "pfr_sku_label",
        src = "sku_code",
        dst = "label_code"
    )
)]
pub struct Sku {
    #[rustango(primary_key, max_length = 16)]
    pub code: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "pfr_sku_label", app = "pfr")]
pub struct SkuLabel {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 16)]
    pub sku_code: String,
    #[rustango(max_length = 16)]
    pub label_code: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "pfr_reader", app = "pfr")]
pub struct Reader {
    #[rustango(primary_key)]
    pub id: uuid::Uuid,
}

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "pfr_doc",
    app = "pfr",
    m2m(
        name = "readers",
        to = "pfr_reader",
        through = "pfr_doc_reader",
        src = "doc_id",
        dst = "reader_id"
    )
)]
pub struct Doc {
    #[rustango(primary_key)]
    pub id: uuid::Uuid,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "pfr_doc_reader", app = "pfr")]
pub struct DocReader {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub doc_id: uuid::Uuid,
    pub reader_id: uuid::Uuid,
}

/// Children point at the unique `code`, not the pk.
#[derive(Model, Debug, Clone)]
#[rustango(table = "pfr_shop", app = "pfr")]
#[rustango(reverse_has(
    name = "items",
    child = "Item",
    child_fk_column = "shop",
    self_pk_column = "code"
))]
pub struct Shop {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(unique, max_length = 16)]
    pub code: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "pfr_item", app = "pfr")]
pub struct Item {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 16, on = "code")]
    pub shop: ForeignKey<Shop, String>,
}

/// A reverse relation over a plain integer column, not a `ForeignKey`.
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "pfr_cat",
    app = "pfr",
    reverse_has(name = "children", child = "Cat", child_fk_column = "parent_id")
)]
pub struct Cat {
    #[rustango(primary_key)]
    pub id: i64,
    pub parent_id: Option<i64>,
}

/// A reverse relation over a plain, non-unique string column.
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "pfr_bin",
    app = "pfr",
    reverse_has(
        name = "parts",
        child = "Part",
        child_fk_column = "bin_label",
        self_pk_column = "label"
    )
)]
pub struct Bin {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 16)]
    pub label: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "pfr_part", app = "pfr")]
pub struct Part {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 16)]
    pub bin_label: String,
}

async fn setup(pool: &Pool) {
    use rustango::testkit::matrix::{drop_table, fresh_table};
    drop_table(pool, Item::SCHEMA.table).await;
    fresh_table::<Tag>(pool).await;
    fresh_table::<Post>(pool).await;
    fresh_table::<PostTag>(pool).await;
    fresh_table::<Label>(pool).await;
    fresh_table::<Sku>(pool).await;
    fresh_table::<SkuLabel>(pool).await;
    fresh_table::<Reader>(pool).await;
    fresh_table::<Doc>(pool).await;
    fresh_table::<DocReader>(pool).await;
    fresh_table::<Shop>(pool).await;
    fresh_table::<Item>(pool).await;
    fresh_table::<Cat>(pool).await;
    fresh_table::<Bin>(pool).await;
    fresh_table::<Part>(pool).await;
}

async fn insert(
    pool: &Pool,
    schema: &'static rustango::core::ModelSchema,
    cols: Vec<&'static str>,
    rows: Vec<Vec<SqlValue>>,
) {
    bulk_insert_pool(pool, &BulkInsertQuery::new(schema, cols, rows))
        .await
        .unwrap_or_else(|e| panic!("seeding {}: {e}", schema.table));
}

/// `n` posts; post `i` carries tags `1..=i % 4` (post 0 and every 4th has none).
async fn seed_posts(pool: &Pool, n: i64) {
    insert(
        pool,
        Tag::SCHEMA,
        vec!["id", "name"],
        (1..=3)
            .map(|i| vec![i.into(), format!("t{i}").into()])
            .collect(),
    )
    .await;
    insert(
        pool,
        Post::SCHEMA,
        vec!["id"],
        (0..n).map(|i| vec![i.into()]).collect(),
    )
    .await;
    let links = (0..n)
        .flat_map(|p| (1..=p % 4).map(move |t| vec![SqlValue::from(p), t.into()]))
        .collect();
    insert(pool, PostTag::SCHEMA, vec!["post_id", "tag_id"], links).await;
}

fn tag_ids(tags: &[Tag]) -> Vec<i64> {
    let mut ids: Vec<i64> = tags.iter().map(|t| t.id).collect();
    ids.sort_unstable();
    ids
}

async fn m2m_loads_every_parents_targets(pool: &Pool) {
    seed_posts(pool, 8).await;
    let rows: Vec<(Post, Vec<Tag>)> =
        fetch_with_prefetch_related(Post::objects().order_by(&[("id", false)]), "tags", pool)
            .await
            .expect("m2m prefetch");
    assert_eq!(rows.len(), 8);
    for (post, tags) in &rows {
        let want: Vec<i64> = (1..=post.id % 4).collect();
        assert_eq!(tag_ids(tags), want, "post {}", post.id);
    }
    // A tag shared by several posts is a copy under each one.
    assert_eq!(
        rows[3]
            .1
            .iter()
            .find(|t| t.id == 1)
            .map(|t| t.name.as_str()),
        Some("t1")
    );
}

/// The per-instance path costs 1 + 2N queries; the batch costs 2 for any N.
async fn m2m_prefetch_runs_constant_queries(pool: &Pool) {
    seed_posts(pool, 20).await;
    let per_instance = QueryCounter::scope(async {
        let posts = Post::objects().fetch(pool).await.unwrap();
        for p in &posts {
            let ids = p.tags_m2m().all(pool).await.unwrap();
            if ids.is_empty() {
                continue;
            }
            let _tags: Vec<Tag> = Tag::objects()
                .filter_op(
                    "id",
                    rustango::core::Op::In,
                    SqlValue::List(ids.into_iter().map(Into::into).collect()),
                )
                .fetch(pool)
                .await
                .unwrap();
        }
        QueryCounter::current()
    })
    .await;
    assert_eq!(
        per_instance, 36,
        "N+1 baseline: 1 + 20 junction reads + 15 tag reads"
    );
    for n in [5_i64, 20] {
        let (rows, queries) = QueryCounter::scope(async {
            let rows: Vec<(Post, Vec<Tag>)> = fetch_with_prefetch_related(
                Post::objects().filter_op("id", rustango::core::Op::Lt, n),
                "tags",
                pool,
            )
            .await
            .unwrap();
            (rows, QueryCounter::current())
        })
        .await;
        assert_eq!(rows.len() as i64, n);
        assert_eq!(
            queries, 2,
            "{n} parents: one parent query, one per relation"
        );
    }
}

async fn m2m_empty_parents_and_childless_parents(pool: &Pool) {
    seed_posts(pool, 4).await;
    let (rows, queries) = QueryCounter::scope(async {
        let rows: Vec<(Post, Vec<Tag>)> = fetch_with_prefetch_related(
            Post::objects().filter_op("id", rustango::core::Op::Gt, 100_i64),
            "tags",
            pool,
        )
        .await
        .unwrap();
        (rows, QueryCounter::current())
    })
    .await;
    assert!(rows.is_empty());
    assert_eq!(queries, 1, "no parents: no child query");
    // Post 0 has no tags: it still comes back, with an empty list.
    let rows: Vec<(Post, Vec<Tag>)> =
        fetch_with_prefetch_related(Post::objects().filter("id", 0_i64), "tags", pool)
            .await
            .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].1.is_empty());
}

async fn m2m_filtered_targets_keep_their_order(pool: &Pool) {
    seed_posts(pool, 4).await;
    let tags = Tag::objects()
        .filter_op("id", rustango::core::Op::Gt, 1_i64)
        .order_by(&[("name", true)]);
    let rows: Vec<(Post, Vec<Tag>)> = fetch_with_prefetch_related_filtered(
        Post::objects().filter("id", 3_i64),
        "tags",
        tags,
        pool,
    )
    .await
    .unwrap();
    let ids: Vec<i64> = rows[0].1.iter().map(|t| t.id).collect();
    assert_eq!(ids, vec![3, 2], "filter and order of the target queryset");
}

/// 70k parents: past MySQL's 65,535 and SQLite's 32,766 binds.
async fn m2m_prefetch_past_the_bind_cap(pool: &Pool) {
    const N: i64 = 70_000;
    seed_posts(pool, N).await;
    let (rows, queries) = QueryCounter::scope(async {
        let rows: Vec<(Post, Vec<Tag>)> =
            fetch_with_prefetch_related(Post::objects(), "tags", pool)
                .await
                .expect("the IN list is chunked");
        (rows, QueryCounter::current())
    })
    .await;
    assert_eq!(rows.len() as i64, N);
    for (post, tags) in &rows {
        assert_eq!(tags.len() as i64, post.id % 4, "post {}", post.id);
    }
    let want = match pool.dialect().name() {
        "postgres" => 2,
        "mysql" => 3,
        _ => 4,
    };
    assert_eq!(queries, want, "one parent query, then one per IN chunk");
}

async fn m2m_string_keys(pool: &Pool) {
    let codes = ["ab", "cd", "ef"];
    insert(
        pool,
        Label::SCHEMA,
        vec!["code"],
        codes.iter().map(|c| vec![(*c).into()]).collect(),
    )
    .await;
    insert(
        pool,
        Sku::SCHEMA,
        vec!["code"],
        vec![vec!["s1".into()], vec!["s2".into()], vec!["s3".into()]],
    )
    .await;
    insert(
        pool,
        SkuLabel::SCHEMA,
        vec!["sku_code", "label_code"],
        vec![
            vec!["s1".into(), "ab".into()],
            vec!["s1".into(), "cd".into()],
            vec!["s2".into(), "ef".into()],
        ],
    )
    .await;
    let rows: Vec<(Sku, Vec<Label>)> =
        fetch_with_prefetch_related(Sku::objects().order_by(&[("code", false)]), "labels", pool)
            .await
            .unwrap();
    let got: Vec<(String, Vec<String>)> = rows
        .into_iter()
        .map(|(s, ls)| {
            let mut ls: Vec<String> = ls.into_iter().map(|l| l.code).collect();
            ls.sort();
            (s.code, ls)
        })
        .collect();
    assert_eq!(
        got,
        vec![
            ("s1".into(), vec!["ab".into(), "cd".into()]),
            ("s2".into(), vec!["ef".into()]),
            ("s3".into(), vec![]),
        ]
    );
}

async fn m2m_uuid_keys(pool: &Pool) {
    let docs = [uuid::Uuid::new_v4(), uuid::Uuid::new_v4()];
    let readers = [uuid::Uuid::new_v4(), uuid::Uuid::new_v4()];
    insert(
        pool,
        Doc::SCHEMA,
        vec!["id"],
        docs.iter().map(|u| vec![(*u).into()]).collect(),
    )
    .await;
    insert(
        pool,
        Reader::SCHEMA,
        vec!["id"],
        readers.iter().map(|u| vec![(*u).into()]).collect(),
    )
    .await;
    insert(
        pool,
        DocReader::SCHEMA,
        vec!["doc_id", "reader_id"],
        vec![
            vec![docs[0].into(), readers[0].into()],
            vec![docs[0].into(), readers[1].into()],
            vec![docs[1].into(), readers[1].into()],
        ],
    )
    .await;
    let rows: Vec<(Doc, Vec<Reader>)> =
        fetch_with_prefetch_related(Doc::objects(), "readers", pool)
            .await
            .unwrap();
    assert_eq!(rows.len(), 2);
    for (doc, rs) in &rows {
        let mut got: Vec<uuid::Uuid> = rs.iter().map(|r| r.id).collect();
        got.sort();
        let mut want = if doc.id == docs[0] {
            readers.to_vec()
        } else {
            vec![readers[1]]
        };
        want.sort();
        assert_eq!(got, want, "doc {}", doc.id);
    }
}

/// Reverse FK by name, keyed on `self_pk_column = "code"`.
async fn reverse_fk_on_a_unique_column(pool: &Pool) {
    for (id, code) in [(1_i64, "zz"), (2, "aa"), (3, "mm")] {
        Shop {
            id,
            code: code.into(),
        }
        .insert_pool(pool)
        .await
        .unwrap();
    }
    for (id, shop) in [(1_i64, "zz"), (2, "zz"), (3, "aa")] {
        Item {
            id,
            shop: ForeignKey::unloaded(shop.to_owned()),
        }
        .insert_pool(pool)
        .await
        .unwrap();
    }
    let (rows, queries) = QueryCounter::scope(async {
        let rows: Vec<(Shop, Vec<Item>)> =
            fetch_with_prefetch_related(Shop::objects().order_by(&[("id", false)]), "items", pool)
                .await
                .unwrap();
        (rows, QueryCounter::current())
    })
    .await;
    assert_eq!(queries, 2);
    let got: Vec<(i64, Vec<i64>)> = rows
        .into_iter()
        .map(|(s, items)| {
            let mut ids: Vec<i64> = items.iter().map(|i| i.id).collect();
            ids.sort_unstable();
            (s.id, ids)
        })
        .collect();
    assert_eq!(got, vec![(1, vec![1, 2]), (2, vec![3]), (3, vec![])]);
}

async fn reverse_over_a_plain_column(pool: &Pool) {
    for (id, parent) in [(1_i64, None), (2, Some(1_i64)), (3, Some(1)), (4, Some(2))] {
        Cat {
            id,
            parent_id: parent,
        }
        .insert_pool(pool)
        .await
        .unwrap();
    }
    let rows: Vec<(Cat, Vec<Cat>)> =
        fetch_with_prefetch_related(Cat::objects().order_by(&[("id", false)]), "children", pool)
            .await
            .unwrap();
    let got: Vec<(i64, Vec<i64>)> = rows
        .into_iter()
        .map(|(c, kids)| {
            let mut ids: Vec<i64> = kids.iter().map(|k| k.id).collect();
            ids.sort_unstable();
            (c.id, ids)
        })
        .collect();
    assert_eq!(
        got,
        vec![(1, vec![2, 3]), (2, vec![4]), (3, vec![]), (4, vec![])]
    );
}

/// Keys a collation folds together, split into two `IN` lists, still load
/// a child once: MySQL matches `ABC` to `abc`.
async fn reverse_split_keys_load_each_child_once(pool: &Pool) {
    const N: i64 = 70_000;
    let label = |i: i64| match i {
        0 => "abc".to_owned(),
        i if i == N - 1 => "ABC".to_owned(),
        i => format!("k{i}"),
    };
    insert(
        pool,
        Bin::SCHEMA,
        vec!["id", "label"],
        (0..N).map(|i| vec![i.into(), label(i).into()]).collect(),
    )
    .await;
    Part {
        id: 1,
        bin_label: "abc".into(),
    }
    .insert_pool(pool)
    .await
    .unwrap();
    let rows: Vec<(Bin, Vec<Part>)> =
        fetch_with_prefetch_related(Bin::objects().order_by(&[("id", false)]), "parts", pool)
            .await
            .expect("the IN list is chunked");
    let parts: Vec<i64> = rows[0].1.iter().map(|p| p.id).collect();
    assert_eq!(parts, [1], "bin abc");
}

async fn unknown_or_mismatched_relation_is_an_error(pool: &Pool) {
    let err = fetch_with_prefetch_related::<Post, Tag>(Post::objects(), "nope", pool)
        .await
        .expect_err("unknown relation");
    assert!(err.to_string().contains("nope"), "{err}");
    let err = fetch_with_prefetch_related::<Post, Label>(Post::objects(), "tags", pool)
        .await
        .expect_err("wrong target model");
    assert!(
        matches!(err, ExecError::PrefetchTargetMismatch { .. }),
        "{err}"
    );
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        m2m_loads_every_parents_targets,
        m2m_prefetch_runs_constant_queries,
        m2m_empty_parents_and_childless_parents,
        m2m_filtered_targets_keep_their_order,
        m2m_prefetch_past_the_bind_cap,
        m2m_string_keys,
        m2m_uuid_keys,
        reverse_fk_on_a_unique_column,
        reverse_over_a_plain_column,
        reverse_split_keys_load_each_child_once,
        unknown_or_mismatched_relation_is_an_error,
    ],
}
