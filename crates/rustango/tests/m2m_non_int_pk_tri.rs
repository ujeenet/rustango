//! M2M managers on models with a String or Uuid PK bind the real key (#1926),
//! on the source and the destination side (#1950).

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

/// A source whose M2M target has a String PK.
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "m2m1926_shelf",
    app = "m2m1926",
    m2m(
        name = "labels",
        to = "m2m1926_label",
        through = "m2m1926_shelf_label",
        src = "shelf_id",
        dst = "label_code"
    )
)]
pub struct Shelf {
    #[rustango(primary_key)]
    pub id: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(app = "m2m1926", table = "m2m1926_shelf_label")]
pub struct ShelfLabel {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub shelf_id: i64,
    #[rustango(max_length = 64)]
    pub label_code: String,
}

/// A source whose M2M target has a Uuid PK.
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "m2m1926_rack",
    app = "m2m1926",
    m2m(
        name = "badges",
        to = "m2m1926_badge",
        through = "m2m1926_rack_badge",
        src = "rack_id",
        dst = "badge_id"
    )
)]
pub struct Rack {
    #[rustango(primary_key)]
    pub id: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(app = "m2m1926", table = "m2m1926_rack_badge")]
pub struct RackBadge {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub rack_id: i64,
    pub badge_id: uuid::Uuid,
}

/// A source whose junction is not a registered model, as the migration writer makes it.
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "m2m1926_item",
    app = "m2m1926",
    m2m(
        name = "tags",
        to = "m2m1926_tag",
        through = "m2m1926_item_tag",
        src = "item_id",
        dst = "tag_id"
    )
)]
pub struct Item {
    #[rustango(primary_key)]
    pub id: i64,
}

/// `m2m1926_item_tag`, kept out of the model registry.
const ITEM_TAG: &rustango::core::ModelSchema = &{
    use rustango::core::{FieldSchema, FieldType, IndexSchema, ModelSchema};
    const FIELDS: &[FieldSchema] = &[
        FieldSchema::new("item_id", "item_id", FieldType::I64),
        FieldSchema::new("tag_id", "tag_id", FieldType::I64),
    ];
    const INDEXES: &[IndexSchema] = &[{
        let mut i = IndexSchema::new("m2m1926_item_tag_pair", &["item_id", "tag_id"]);
        i.unique = true;
        i
    }];
    let mut s = ModelSchema::new("ItemTag", "m2m1926_item_tag");
    s.fields = FIELDS;
    s.indexes = INDEXES;
    s
};

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::drop_table(pool, ITEM_TAG.table).await;
    rustango::testkit::create_tables(pool, &[ITEM_TAG])
        .await
        .expect("item_tag");
    rustango::testkit::matrix::fresh_table::<RackBadge>(pool).await;
    rustango::testkit::matrix::fresh_table::<PostTag>(pool).await;
    rustango::testkit::matrix::fresh_table::<DocTag>(pool).await;
    rustango::testkit::matrix::fresh_table::<ShelfLabel>(pool).await;
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

/// A key too long for the junction column is an error, not a silent
/// MySQL truncation (#1966); the through model's bounds hold on SQLite too (#2136).
async fn too_long_key_is_an_error(pool: &Pool) {
    let long = post(&"x".repeat(100));
    assert!(
        long.tags_m2m().add(1, pool).await.is_err(),
        "add accepted it"
    );
    // Refused before any statement runs, so no DELETE goes out (#2152).
    let tags = long.tags_m2m();
    let res = rustango::test_assertions::assert_num_queries(0, tags.set(&[1], pool)).await;
    assert!(res.is_err(), "set accepted it");
    assert_eq!(PostTag::objects().count(pool).await.expect("count"), 0);
}

/// The emitters compile against an unregistered junction too; a duplicate `add` is skipped.
async fn unregistered_junction_round_trips(pool: &Pool) {
    let (a, b) = (Item { id: 1 }, Item { id: 2 });
    a.tags_m2m().add(1, pool).await.expect("add");
    a.tags_m2m().add(1, pool).await.expect("duplicate add");
    b.tags_m2m().set(&[2, 3], pool).await.expect("set");
    assert_eq!(a.tags_m2m().all(pool).await.expect("all"), vec![1]);
    assert!(b.tags_m2m().contains(3, pool).await.expect("contains"));
    b.tags_m2m().remove(3, pool).await.expect("remove");
    assert_eq!(b.tags_m2m().all(pool).await.expect("all"), vec![2]);
    a.tags_m2m().clear(pool).await.expect("clear");
    assert!(a.tags_m2m().all(pool).await.expect("all").is_empty());
}

/// `set` splits a list past the backend's bind limit like `bulk_insert` (#2136).
async fn set_past_the_bind_limit(pool: &Pool) {
    let n = i64::try_from(pool.dialect().max_bind_params() / 2 + 1).expect("fits");
    let ids: Vec<i64> = (0..n).collect();
    post("big").tags_m2m().set(&ids, pool).await.expect("set");
    assert_eq!(PostTag::objects().count(pool).await.expect("count"), n);
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

/// String destination keys bind and read back as text (#1950).
async fn string_destination_keys_round_trip(pool: &Pool) {
    let (a, b) = (Shelf { id: 1 }, Shelf { id: 2 });
    a.labels_m2m().add("rust", pool).await.expect("add");
    b.labels_m2m().set(&["go", "zig"], pool).await.expect("set");
    assert!(a
        .labels_m2m()
        .contains("rust", pool)
        .await
        .expect("contains"));
    assert!(!a.labels_m2m().contains("go", pool).await.expect("contains"));
    let mut got: Vec<String> = b.labels_m2m().all_as(pool).await.expect("all_as");
    got.sort_unstable();
    assert_eq!(got, ["go", "zig"]);
    b.labels_m2m().remove("go", pool).await.expect("remove");
    let got: Vec<String> = b.labels_m2m().all_as(pool).await.expect("all_as");
    assert_eq!(got, ["zig"]);
    assert_eq!(ShelfLabel::objects().count(pool).await.expect("count"), 2);
}

/// Uuid destination keys read back on every backend: MySQL keeps CHAR(36) text,
/// SQLite a 16-byte blob or, for a raw-SQL row, text (#1950).
async fn uuid_destination_keys_round_trip(pool: &Pool) {
    let rack = Rack { id: 1 };
    let (a, b) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    rack.badges_m2m().add(a, pool).await.expect("add");
    assert!(rack.badges_m2m().contains(a, pool).await.expect("contains"));
    let got: Vec<uuid::Uuid> = rack.badges_m2m().all_as(pool).await.expect("all_as");
    assert_eq!(got, [a]);
    rack.badges_m2m().set(&[a, b], pool).await.expect("set");
    let mut got: Vec<uuid::Uuid> = rack.badges_m2m().all_as(pool).await.expect("all_as");
    got.sort_unstable();
    let mut want = vec![a, b];
    want.sort_unstable();
    assert_eq!(got, want);
    #[cfg(feature = "sqlite")]
    if let Pool::Sqlite(sq) = pool {
        let c = uuid::Uuid::new_v4();
        rustango::sql::sqlx::query(
            "INSERT INTO m2m1926_rack_badge (rack_id, badge_id) VALUES (2, ?)",
        )
        .bind(c.to_string())
        .execute(sq)
        .await
        .expect("text row");
        let got: Vec<uuid::Uuid> = Rack { id: 2 }
            .badges_m2m()
            .all_as(pool)
            .await
            .expect("text");
        assert_eq!(got, [c]);
    }
}

/// A key that doesn't fit the `dst` column converts when it parses, else errors (#1950).
async fn mistyped_destination_keys_are_checked(pool: &Pool) {
    let p = post("abc-slug");
    p.tags_m2m()
        .add("7", pool)
        .await
        .expect("numeric text converts");
    assert_eq!(p.tags_m2m().all(pool).await.expect("all"), vec![7]);
    let err = p.tags_m2m().add("seven", pool).await.expect_err("text key");
    assert!(
        matches!(
            err,
            ExecError::Query(rustango::core::QueryError::TypeMismatch { .. })
        ),
        "{err:?}"
    );
    let err = Shelf { id: 1 }
        .labels_m2m()
        .contains(5, pool)
        .await
        .expect_err("int key on a text column");
    assert!(matches!(err, ExecError::Query(_)), "{err:?}");
    let err = Rack { id: 1 }
        .badges_m2m()
        .set(&["not-a-uuid"], pool)
        .await
        .expect_err("bad uuid");
    assert!(matches!(err, ExecError::Query(_)), "{err:?}");
    assert_eq!(PostTag::objects().count(pool).await.expect("count"), 1);
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        string_pk_rows_stay_per_source,
        too_long_key_is_an_error,
        set_past_the_bind_limit,
        unregistered_junction_round_trips,
        uuid_pk_rows_stay_per_source,
        unsaved_source_is_refused,
        string_destination_keys_round_trip,
        uuid_destination_keys_round_trip,
        mistyped_destination_keys_are_checked,
    ],
}
