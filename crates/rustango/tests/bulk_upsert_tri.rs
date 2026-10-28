//! `Model::bulk_upsert_pool` on every backend — one body, three dialects
//! (#267, #1461).
//!
//! Replaces `bulk_upsert_live.rs`, `bulk_upsert_mysql_live.rs` and
//! `bulk_upsert_sqlite_live.rs`: 539 lines saying the same thing three
//! times, differing only in pool construction, hand-written DDL, and two
//! read helpers that unwrapped `Pool` to a concrete driver type.
//!
//! Pins the "import a batch, re-run it, nothing duplicates" pattern —
//! the top reason users escape to raw SQL on other ORM stacks:
//!
//!   1. first call inserts every row;
//!   2. a second call with overlapping natural keys updates only the
//!      listed columns and leaves the rest alone;
//!   3. `insert_or_ignore` skips conflicts instead of overwriting.
//!
//! The emitted SQL differs — `ON CONFLICT (slug) DO UPDATE` on
//! PostgreSQL and SQLite, `ON DUPLICATE KEY UPDATE` on MySQL — but the
//! **observable behaviour does not**, which is why there is no
//! `by_dialect!` here. A divergence would be a bug, and one shared body
//! asserting the same outcome on all three is how it would be caught.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::sql::{Auto, CounterPool as _, FetcherPool as _, Pool};
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(table = "bulk_upsert_tri_post")]
#[rustango(app = "bulk_upsert_tri")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    /// The conflict target. `unique` is what makes an upsert an upsert
    /// rather than a second insert.
    #[rustango(max_length = 64, unique)]
    pub slug: String,
    #[rustango(max_length = 200)]
    pub title: String,
    pub view_count: i64,
}

fn post(slug: &str, title: &str, view_count: i64) -> Post {
    Post {
        id: Auto::default(),
        slug: slug.into(),
        title: title.into(),
        view_count,
    }
}

/// Read one row back through the ORM.
///
/// The three files this replaces each unwrapped `Pool` to their driver's
/// concrete type and ran `sqlx::query_as` — three copies of the same
/// SELECT in three quoting styles, which is precisely the per-dialect
/// plumbing the shared harness exists to delete.
async fn fetch(pool: &Pool, slug: &str) -> (String, i64) {
    let rows: Vec<Post> = Post::objects()
        .filter("slug", slug.to_owned())
        .fetch(pool)
        .await
        .expect("fetch by slug");
    let row = rows
        .first()
        .unwrap_or_else(|| panic!("no row with slug `{slug}`"));
    (row.title.clone(), row.view_count)
}

async fn count(pool: &Pool) -> i64 {
    Post::objects().count(pool).await.expect("count")
}

async fn first_call_inserts_all_rows(pool: &Pool) {
    let rows = vec![post("a", "Alpha", 1), post("b", "Beta", 2)];
    Post::bulk_upsert_pool(&rows, &["slug"], &["title", "view_count"], pool)
        .await
        .expect("upsert, first call");

    assert_eq!(count(pool).await, 2, "both rows should land");
    assert_eq!(fetch(pool, "a").await, ("Alpha".to_owned(), 1));
    assert_eq!(fetch(pool, "b").await, ("Beta".to_owned(), 2));
}

/// The behaviour the whole feature exists for: a column absent from
/// `update_cols` must survive the second call untouched.
async fn second_call_updates_listed_columns_only(pool: &Pool) {
    Post::bulk_upsert_pool(
        &[post("a", "Alpha", 10)],
        &["slug"],
        &["title", "view_count"],
        pool,
    )
    .await
    .expect("seed");

    // `view_count` is deliberately NOT in the update list, and the row
    // carries 999 to prove the value is ignored rather than absent.
    Post::bulk_upsert_pool(
        &[post("a", "Alpha (revised)", 999)],
        &["slug"],
        &["title"],
        pool,
    )
    .await
    .expect("second upsert");

    assert_eq!(count(pool).await, 1, "upsert must not insert a duplicate");
    let (title, view_count) = fetch(pool, "a").await;
    assert_eq!(title, "Alpha (revised)", "a listed column should update");
    assert_eq!(
        view_count, 10,
        "view_count is not in update_cols, so the 999 in the batch must be ignored"
    );
}

async fn insert_or_ignore_skips_conflicts(pool: &Pool) {
    Post::bulk_upsert_pool(&[post("a", "Alpha", 10)], &["slug"], &["title"], pool)
        .await
        .expect("seed");

    // One conflicting row, one new. The new one lands; the existing one
    // is left exactly as it was.
    Post::bulk_insert_or_ignore_pool(&[post("a", "OVERWRITTEN", 999), post("b", "Beta", 2)], pool)
        .await
        .expect("insert_or_ignore");

    assert_eq!(count(pool).await, 2, "the non-conflicting row should land");
    let (title, view_count) = fetch(pool, "a").await;
    assert_eq!(
        title, "Alpha",
        "insert_or_ignore must not overwrite an existing row"
    );
    assert_eq!(view_count, 10);
}

/// An empty batch is a no-op, not an error and not a malformed statement
/// with an empty `VALUES` list.
///
/// Only the SQLite file covered this. It is dialect-agnostic behaviour,
/// so all three get it now — which is the cheap half of #1461: a
/// scenario written once runs everywhere it applies.
async fn empty_batch_is_a_noop(pool: &Pool) {
    Post::bulk_upsert_pool(&[], &["slug"], &["title"], pool)
        .await
        .expect("empty upsert should be accepted");
    Post::bulk_insert_or_ignore_pool(&[], pool)
        .await
        .expect("empty insert_or_ignore should be accepted");

    assert_eq!(count(pool).await, 0, "an empty batch must write nothing");
}

/// A conflict key repeated in one batch keeps the last row on every backend.
async fn repeated_key_in_one_batch_keeps_the_last(pool: &Pool) {
    let rows = [
        post("a", "First", 1),
        post("b", "Beta", 2),
        post("a", "Last", 3),
    ];
    Post::bulk_upsert_pool(&rows, &["slug"], &["title", "view_count"], pool)
        .await
        .expect("repeated key in a fresh batch");
    assert_eq!(fetch(pool, "a").await, ("Last".into(), 3));
    let again = [post("a", "X", 4), post("a", "Y", 5)];
    Post::bulk_upsert_pool(&again, &["slug"], &["title", "view_count"], pool)
        .await
        .expect("repeated key over an existing row");
    assert_eq!(fetch(pool, "a").await, ("Y".into(), 5));
    assert_eq!(count(pool).await, 2);
}

/// No update columns is a typed error on every backend, not bad SQL.
async fn empty_update_cols_is_refused(pool: &Pool) {
    let err = Post::bulk_upsert_pool(&[post("a", "Alpha", 1)], &["slug"], &[], pool)
        .await
        .expect_err("empty update_cols");
    assert!(
        matches!(
            err,
            rustango::sql::ExecError::Sql(rustango::sql::SqlError::EmptyUpdateSet)
        ),
        "{err:?}"
    );
    assert_eq!(count(pool).await, 0);
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "bulk_upsert_tri_doc")]
#[rustango(app = "bulk_upsert_tri")]
pub struct Doc {
    #[rustango(primary_key, default_uuid_v7)]
    pub id: Auto<uuid::Uuid>,
    #[rustango(max_length = 64, unique)]
    pub slug: String,
    #[rustango(max_length = 64)]
    pub title: String,
}

async fn doc_title(pool: &Pool, slug: &str) -> String {
    let rows: Vec<Doc> = Doc::objects()
        .filter("slug", slug.to_owned())
        .fetch(pool)
        .await
        .expect("fetch doc");
    rows[0].title.clone()
}

/// A caller-set `default_uuid_v7` pk can clash, so a slug upsert must
/// not update the row that owns that pk.
async fn caller_set_uuid_pk_never_updates_another_row(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Doc>(pool).await;
    let doc = |id: Auto<uuid::Uuid>, slug: &str, title: &str| Doc {
        id,
        slug: slug.into(),
        title: title.into(),
    };
    let fresh = [doc(Auto::Unset, "a", "A"), doc(Auto::Unset, "b", "B")];
    Doc::bulk_upsert_pool(&fresh, &["slug"], &["title"], pool)
        .await
        .expect("generated pks never clash");
    let a: Vec<Doc> = Doc::objects()
        .filter("slug", "a")
        .fetch(pool)
        .await
        .unwrap();
    let a_id = a[0].id.get().copied().expect("id");
    let r = Doc::bulk_upsert_pool(
        &[doc(Auto::Set(a_id), "c", "X")],
        &["slug"],
        &["title"],
        pool,
    )
    .await;
    assert!(r.is_err(), "a pk clash is not a slug conflict");
    assert_eq!(doc_title(pool, "a").await, "A");
    assert_eq!(doc_title(pool, "b").await, "B");
}

tri_dialect_test! {
    model: Post,
    scenarios: [
        caller_set_uuid_pk_never_updates_another_row,
        empty_update_cols_is_refused,
        first_call_inserts_all_rows,
        second_call_updates_listed_columns_only,
        insert_or_ignore_skips_conflicts,
        empty_batch_is_a_noop,
        repeated_key_in_one_batch_keeps_the_last,
    ],
}
