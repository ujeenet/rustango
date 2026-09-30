//! M2M managers on models with a String or Uuid PK bind the real key (#1926).

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::sql::{Auto, CounterPool as _, ExecError, Pool};
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "m2m1926_post",
    app = "m2m1926",
    m2m(
        name = "tags",
        to = "m2m1926_tag",
        through = "m2m1926_post_tag",
        src = "post_id",
        dst = "tag_id"
    )
)]
pub struct Post {
    #[rustango(primary_key, max_length = 64)]
    pub slug: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(app = "m2m1926", table = "m2m1926_post_tag")]
pub struct PostTag {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub post_id: String,
    pub tag_id: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "m2m1926_doc",
    app = "m2m1926",
    m2m(
        name = "tags",
        to = "m2m1926_tag",
        through = "m2m1926_doc_tag",
        src = "doc_id",
        dst = "tag_id"
    )
)]
pub struct Doc {
    #[rustango(primary_key)]
    pub id: uuid::Uuid,
}

#[derive(Model, Debug, Clone)]
#[rustango(app = "m2m1926", table = "m2m1926_doc_tag")]
pub struct DocTag {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub doc_id: uuid::Uuid,
    pub tag_id: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "m2m1926_note",
    app = "m2m1926",
    m2m(
        name = "tags",
        to = "m2m1926_tag",
        through = "m2m1926_post_tag",
        src = "post_id",
        dst = "tag_id"
    )
)]
pub struct Note {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
}

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<PostTag>(pool).await;
    rustango::testkit::matrix::fresh_table::<DocTag>(pool).await;
}

fn post(slug: &str) -> Post {
    Post { slug: slug.into() }
}

/// On MySQL a text column compared to `0` matches every letter-first row.
async fn string_pk_rows_stay_per_source(pool: &Pool) {
    let (abc, def) = (post("abc-slug"), post("def-slug"));
    abc.tags_m2m().add(1, pool).await.expect("add abc");
    def.tags_m2m().add(2, pool).await.expect("add def");

    assert_eq!(abc.tags_m2m().all(pool).await.expect("all"), vec![1]);
    assert!(abc.tags_m2m().contains(1, pool).await.expect("contains"));
    assert!(!abc.tags_m2m().contains(2, pool).await.expect("contains"));

    abc.tags_m2m().set(&[3, 4], pool).await.expect("set");
    let mut got = abc.tags_m2m().all(pool).await.expect("all");
    got.sort_unstable();
    assert_eq!(got, vec![3, 4]);
    assert_eq!(def.tags_m2m().all(pool).await.expect("all"), vec![2]);

    abc.tags_m2m().remove(3, pool).await.expect("remove");
    abc.tags_m2m().clear(pool).await.expect("clear");
    assert_eq!(def.tags_m2m().all(pool).await.expect("all"), vec![2]);
    let stored = PostTag::objects()
        .filter("post_id", "def-slug")
        .count(pool)
        .await
        .expect("count");
    assert_eq!(stored, 1);
    assert_eq!(PostTag::objects().count(pool).await.expect("count"), 1);
}

async fn uuid_pk_rows_stay_per_source(pool: &Pool) {
    let a = Doc {
        id: uuid::Uuid::new_v4(),
    };
    let b = Doc {
        id: uuid::Uuid::new_v4(),
    };
    a.tags_m2m().add(1, pool).await.expect("add a");
    b.tags_m2m().set(&[2, 3], pool).await.expect("set b");

    assert_eq!(a.tags_m2m().all(pool).await.expect("all"), vec![1]);
    a.tags_m2m().clear(pool).await.expect("clear");
    let mut got = b.tags_m2m().all(pool).await.expect("all");
    got.sort_unstable();
    assert_eq!(got, vec![2, 3]);
    let stored = DocTag::objects()
        .filter("doc_id", b.id)
        .count(pool)
        .await
        .expect("count");
    assert_eq!(stored, 2);
}

/// An unsaved source has no key; it must not act on anyone's rows.
async fn unsaved_source_is_refused(pool: &Pool) {
    post("abc-slug")
        .tags_m2m()
        .add(1, pool)
        .await
        .expect("seed");
    let unsaved = Note { id: Auto::Unset };
    let err = unsaved.tags_m2m().clear(pool).await.expect_err("unsaved");
    assert!(matches!(err, ExecError::M2mUnsavedSource { .. }), "{err:?}");
    assert!(unsaved.tags_m2m().add(1, pool).await.is_err());
    assert_eq!(PostTag::objects().count(pool).await.expect("count"), 1);
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        string_pk_rows_stay_per_source,
        uuid_pk_rows_stay_per_source,
        unsaved_source_is_refused,
    ],
}
