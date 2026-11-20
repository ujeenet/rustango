//! Shared fixture for the row-copy bench and its golden tests: list pages
//! over FK columns, a 1k x 10 prefetch and a 10k-row `values_dict`.
#![allow(dead_code)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt as _;
use rustango::core::{BulkInsertQuery, Model as _, SqlValue};
use rustango::sql::{bulk_insert_pool, Auto, ForeignKey, Pool};
use rustango::Model;
use tower::ServiceExt as _;

pub const PARENTS: i64 = 1_000;
pub const CHILDREN_PER_PARENT: i64 = 10;
pub const TAGS: i64 = 50;
pub const POSTS: i64 = 250;
pub const PAGE: usize = 100;

#[derive(Model, Debug, Clone)]
#[rustango(table = "rc_author", app = "rc", display = "name")]
pub struct RcAuthor {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 100)]
    pub name: String,
    #[rustango(max_length = 200)]
    pub bio: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "rc_category", app = "rc", display = "name")]
pub struct RcCategory {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 100)]
    pub name: String,
    #[rustango(max_length = 200)]
    pub blurb: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "rc_post", app = "rc", admin(list_per_page = 100))]
pub struct RcPost {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    #[rustango(max_length = 400)]
    pub body: String,
    #[rustango(max_length = 20)]
    pub status: String,
    pub score: i64,
    pub author_id: ForeignKey<RcAuthor, i64>,
    pub category_id: ForeignKey<RcCategory, i64>,
}

pub const POST_FIELDS: &[&str] = &[
    "id",
    "title",
    "body",
    "status",
    "score",
    "author_id",
    "category_id",
];

#[derive(Model, Debug, Clone)]
#[rustango(table = "rc_tag", app = "rc")]
pub struct RcTag {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 40)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "rc_parent",
    app = "rc",
    m2m(
        name = "tags",
        to = "rc_tag",
        through = "rc_parent_tag",
        src = "parent_id",
        dst = "tag_id"
    )
)]
pub struct RcParent {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 100)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "rc_parent_tag", app = "rc")]
pub struct RcParentTag {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub parent_id: i64,
    pub tag_id: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "rc_child", app = "rc")]
pub struct RcChild {
    #[rustango(primary_key)]
    pub id: i64,
    pub parent: ForeignKey<RcParent, i64>,
    #[rustango(max_length = 100)]
    pub title: String,
    pub rank: i64,
}

/// A serializer with an owned copy of each rendered field.
#[derive(serde::Serialize)]
pub struct RcPostOut {
    pub id: i64,
    pub title: String,
    pub body: String,
    pub status: String,
    pub score: i64,
}

impl rustango::serializer::ModelSerializer for RcPostOut {
    type Model = RcPost;
    fn from_model(m: &RcPost) -> Self {
        Self {
            id: m.id.get().copied().unwrap_or_default(),
            title: m.title.clone(),
            body: m.body.clone(),
            status: m.status.clone(),
            score: m.score,
        }
    }
    fn writable_fields() -> &'static [&'static str] {
        &[]
    }
}

async fn insert(
    pool: &Pool,
    schema: &'static rustango::core::ModelSchema,
    cols: Vec<&'static str>,
    rows: Vec<Vec<SqlValue>>,
) {
    for chunk in rows.chunks(500) {
        bulk_insert_pool(
            pool,
            &BulkInsertQuery::new(schema, cols.clone(), chunk.to_vec()),
        )
        .await
        .unwrap_or_else(|e| panic!("seeding {}: {e}", schema.table));
    }
}

/// In-memory SQLite with every fixture table seeded.
pub async fn seeded_pool() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    rustango::testkit::create_tables(
        &pool,
        &[
            RcAuthor::SCHEMA,
            RcCategory::SCHEMA,
            RcPost::SCHEMA,
            RcTag::SCHEMA,
            RcParent::SCHEMA,
            RcParentTag::SCHEMA,
            RcChild::SCHEMA,
        ],
    )
    .await
    .expect("create tables");
    let named = |n: i64, what: &str| -> Vec<Vec<SqlValue>> {
        (1..=n)
            .map(|i| {
                vec![
                    i.into(),
                    format!("{what} <{i}> & co").into(),
                    format!("About {what} {i}, in some detail").into(),
                ]
            })
            .collect()
    };
    insert(
        &pool,
        RcAuthor::SCHEMA,
        vec!["id", "name", "bio"],
        named(20, "Author"),
    )
    .await;
    insert(
        &pool,
        RcCategory::SCHEMA,
        vec!["id", "name", "blurb"],
        named(8, "Category"),
    )
    .await;
    insert(
        &pool,
        RcPost::SCHEMA,
        vec![
            "id",
            "title",
            "body",
            "status",
            "score",
            "author_id",
            "category_id",
        ],
        (1..=POSTS)
            .map(|i| {
                vec![
                    i.into(),
                    format!("Post \"{i}\" title").into(),
                    format!("Body of post {i}: <p>words</p> and more words").into(),
                    if i % 3 == 0 { "draft" } else { "published" }.into(),
                    (i * 7 % 101).into(),
                    (i % 20 + 1).into(),
                    (i % 8 + 1).into(),
                ]
            })
            .collect(),
    )
    .await;
    insert(
        &pool,
        RcTag::SCHEMA,
        vec!["id", "name"],
        (1..=TAGS)
            .map(|i| vec![i.into(), format!("tag{i}").into()])
            .collect(),
    )
    .await;
    insert(
        &pool,
        RcParent::SCHEMA,
        vec!["id", "name"],
        (1..=PARENTS)
            .map(|i| vec![i.into(), format!("parent {i}").into()])
            .collect(),
    )
    .await;
    insert(
        &pool,
        RcChild::SCHEMA,
        vec!["id", "parent", "title", "rank"],
        (0..PARENTS * CHILDREN_PER_PARENT)
            .map(|i| {
                vec![
                    (i + 1).into(),
                    (i % PARENTS + 1).into(),
                    format!("child {i}").into(),
                    (i / PARENTS).into(),
                ]
            })
            .collect(),
    )
    .await;
    insert(
        &pool,
        RcParentTag::SCHEMA,
        vec!["parent_id", "tag_id"],
        (0..PARENTS * CHILDREN_PER_PARENT)
            .map(|i| {
                vec![
                    (i % PARENTS + 1).into(),
                    ((i % PARENTS + i / PARENTS * 3) % TAGS + 1).into(),
                ]
            })
            .collect(),
    )
    .await;
    pool
}

/// The admin over the fixture, no auth layer.
pub fn admin_app(pool: Pool) -> axum::Router {
    rustango::admin::Builder::new(pool).admin_prefix("").build()
}

pub fn tera() -> Arc<tera::Tera> {
    let mut t = tera::Tera::default();
    t.add_raw_template(
        "rc_post_list.html",
        "<h1>{{ total }}</h1>{% for p in object_list %}<tr><td>{{ p.id }}</td><td>{{ p.title }}</td>\
         <td>{{ p.body }}</td><td>{{ p.status }}</td><td>{{ p.score }}</td>\
         <td>{{ p.author_id_display }}</td><td>{{ p.category_id_display }}</td></tr>{% endfor %}\
         {% for p in posts %}{{ p.id }},{% endfor %}page={{ page }}/{{ total_pages }}",
    )
    .unwrap();
    Arc::new(t)
}

/// A ListView page of [`PAGE`] posts with both FK names resolved.
pub fn list_view_app(pool: Pool) -> axum::Router {
    rustango::template_views::ListView::for_model(RcPost::SCHEMA)
        .allow_anonymous()
        .page_size(PAGE)
        .order_by("id", false)
        .with_fk_display(true)
        .context_object_name("posts")
        .router("/posts", tera(), pool)
}

/// A ViewSet page of [`PAGE`] posts, default projection.
pub fn viewset_app(pool: Pool) -> axum::Router {
    rustango::viewset::ViewSet::for_model(RcPost::SCHEMA, POST_FIELDS)
        .allow_anonymous()
        .page_size(PAGE)
        .router("/api/posts", pool)
}

/// A ViewSet page of [`PAGE`] posts through a serializer.
pub fn viewset_serializer_app(pool: Pool) -> axum::Router {
    rustango::viewset::ViewSet::for_model(RcPost::SCHEMA, POST_FIELDS)
        .allow_anonymous()
        .page_size(PAGE)
        .serializer::<RcPostOut>()
        .router("/api/posts", pool)
}

/// GET `uri`, asserting 200, as text.
pub async fn get(app: &axum::Router, uri: &str) -> String {
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK, "{uri}");
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}
