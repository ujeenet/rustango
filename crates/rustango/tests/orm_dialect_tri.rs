//! ORM writers that must give the same result on every backend
//! (0.59.11 dialect batch). One body per scenario, three dialects.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::core::joins::aliased;
use rustango::core::{
    ConflictClause, InsertQuery, Model as _, Op, SearchClause, SqlValue, WhereExpr,
};
use rustango::sql::{Auto, CounterPool as _, FetcherPool as _, InsertReturningPool, Pool};
use rustango::testkit::matrix::fresh_table;
use rustango::{tri_dialect_test, Model};

/// A natural string PK: no `id` column at all.
#[derive(Model, Debug, Clone)]
#[rustango(table = "orm_dialect_tri_code")]
#[rustango(app = "orm_dialect_tri")]
pub struct Code {
    #[rustango(primary_key, max_length = 32)]
    pub code: String,
    pub n: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "orm_dialect_tri_post")]
#[rustango(app = "orm_dialect_tri")]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64, unique)]
    pub slug: String,
    #[rustango(max_length = 64)]
    pub title: String,
    pub parent_id: Option<i64>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "orm_dialect_tri_blob")]
#[rustango(app = "orm_dialect_tri")]
pub struct Blob {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub token: uuid::Uuid,
    pub data: Vec<u8>,
}

fn post(slug: &str, parent_id: Option<i64>) -> Post {
    Post {
        id: Auto::default(),
        slug: slug.into(),
        title: slug.into(),
        parent_id,
    }
}

async fn setup(pool: &Pool) {
    fresh_table::<Code>(pool).await;
    fresh_table::<Post>(pool).await;
    fresh_table::<Blob>(pool).await;
}

async fn posts(pool: &Pool) -> Vec<Post> {
    Post::objects()
        .order_by(&[("id", false)])
        .fetch(pool)
        .await
        .expect("fetch posts")
}

/// The PK a single-row insert reports, whatever the backend's shape.
fn reported_id(r: InsertReturningPool) -> i64 {
    #[allow(unused_imports)]
    use rustango::sql::sqlx::Row as _;
    #[allow(unreachable_patterns)]
    match r {
        #[cfg(feature = "postgres")]
        InsertReturningPool::PgRow(row) => row.try_get("id").expect("id"),
        #[cfg(feature = "mysql")]
        InsertReturningPool::MySqlAutoId(id) => id,
        #[cfg(feature = "sqlite")]
        InsertReturningPool::SqliteRow(row) => row.try_get("id").expect("id"),
        _ => unreachable!(),
    }
}

/// #1887: MySQL wrote `id = id`, so a model without an `id` column failed.
async fn insert_or_ignore_on_a_natural_pk(pool: &Pool) {
    let first = Code {
        code: "a".into(),
        n: 1,
    };
    assert!(first.insert_or_ignore(pool).await.expect("first insert"));
    let dup = Code {
        code: "a".into(),
        n: 2,
    };
    assert!(
        !dup.insert_or_ignore(pool).await.expect("duplicate insert"),
        "a skipped row must report false"
    );
    let rows: Vec<Code> = Code::objects().fetch(pool).await.expect("fetch");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].n, 1, "the duplicate must not overwrite");
}

/// #1887: MySQL counted the skip as an affected row and said true.
async fn insert_or_ignore_reports_a_skip_on_an_auto_pk(pool: &Pool) {
    assert!(post("x", None)
        .insert_or_ignore(pool)
        .await
        .expect("insert"));
    assert!(
        !post("x", None).insert_or_ignore(pool).await.expect("dup"),
        "a skipped row must report false"
    );
    assert_eq!(Post::objects().count(pool).await.expect("count"), 1);

    // A skip returns no row, as an empty RETURNING does.
    let q = InsertQuery::new(
        Post::SCHEMA,
        vec!["slug", "title"],
        vec![SqlValue::from("x"), SqlValue::from("again")],
    )
    .returning(vec!["id"])
    .on_conflict(ConflictClause::DoNothing);
    assert!(rustango::sql::insert_returning_pool(pool, &q)
        .await
        .is_err());
}

/// #1887: MySQL's LAST_INSERT_ID() named a stale row after an update.
async fn upsert_reports_the_updated_row(pool: &Pool) {
    post("a", None).insert_pool(pool).await.expect("seed a");
    post("b", None).insert_pool(pool).await.expect("seed b");
    let a_id = posts(pool).await[0].id.get().copied().expect("a id");

    let q = InsertQuery::new(
        Post::SCHEMA,
        vec!["slug", "title"],
        vec![SqlValue::from("a"), SqlValue::from("A2")],
    )
    .returning(vec!["id"])
    .on_conflict(ConflictClause::DoUpdate {
        target: vec!["slug"],
        update_columns: vec!["title"],
    });
    let r = rustango::sql::insert_returning_pool(pool, &q)
        .await
        .expect("upsert");
    assert_eq!(reported_id(r), a_id, "the id must name the updated row");
    assert_eq!(posts(pool).await[0].title, "A2");
}

/// #1888: PG typed an all-NULL VALUES column as text.
async fn bulk_update_sets_null_in_every_row(pool: &Pool) {
    post("a", Some(5)).insert_pool(pool).await.expect("seed a");
    post("b", Some(6)).insert_pool(pool).await.expect("seed b");
    let mut rows = posts(pool).await;
    for r in &mut rows {
        r.parent_id = None;
    }
    let n = Post::bulk_update(&rows, &["parent_id"], pool)
        .await
        .expect("bulk_update to NULL");
    assert_eq!(n, 2);
    assert!(posts(pool).await.iter().all(|p| p.parent_id.is_none()));
}

/// Seed `(slug, title)` rows in order.
async fn seed(pool: &Pool, rows: &[(&str, &str)]) {
    for (slug, title) in rows {
        let mut p = post(slug, None);
        p.title = (*title).into();
        p.insert_pool(pool).await.expect("seed");
    }
}

fn slugs(rows: &[Post]) -> Vec<&str> {
    let mut out: Vec<&str> = rows.iter().map(|p| p.slug.as_str()).collect();
    out.sort_unstable();
    out
}

/// `<table>.id = <alias>.id`, for a derived-table join on the PK.
fn same_id(alias: &'static str) -> WhereExpr {
    WhereExpr::ExprCompare {
        lhs: aliased(alias, "id"),
        op: Op::Eq,
        rhs: aliased("orm_dialect_tri_post", "id"),
    }
}

/// #1890: a union's first branch dropped its derived-table join.
async fn union_keeps_the_first_branch_join(pool: &Pool) {
    seed(pool, &[("a", "a"), ("b", "b"), ("c", "c")]).await;
    let only_a = Post::objects().filter("slug", "a").compile().expect("sub");
    let rows = Post::objects()
        .join_sub(only_a, "s", same_id("s"))
        .union(Post::objects().filter("slug", "b"))
        .fetch(pool)
        .await
        .expect("union");
    assert_eq!(slugs(&rows), ["a", "b"]);
}

/// #1890: `values_list_flat` on a union projected only part of it.
async fn union_values_list_flat_in_a_subquery(pool: &Pool) {
    seed(pool, &[("a", "a"), ("b", "b"), ("c", "c")]).await;
    let ids = Post::objects()
        .filter("slug", "a")
        .union(Post::objects().filter("slug", "b"))
        .values_list_flat("id")
        .compile()
        .expect("ids");
    let rows = Post::objects()
        .where_in_subquery("id", ids)
        .fetch(pool)
        .await
        .expect("IN (union)");
    assert_eq!(slugs(&rows), ["a", "b"]);
}

/// #1890: a paginated union counted its first branch only.
async fn paginated_union_counts_every_branch(pool: &Pool) {
    seed(pool, &[("a", "a"), ("b", "b"), ("c", "c")]).await;
    let qs = Post::objects()
        .filter("slug", "a")
        .union(Post::objects().filter("slug", "b"))
        .order_by(&[("id", false)])
        .limit(1);
    let page = rustango::sql::fetch_paginated_pool(qs, pool)
        .await
        .expect("paginated union");
    assert_eq!(page.total, 2, "the total spans both branches");
    assert_eq!(slugs(&page.rows), ["a"]);
}

/// #1890: the MySQL / SQLite `distinct_on` fallback dropped search and
/// derived-table joins.
async fn distinct_on_keeps_search_and_derived_joins(pool: &Pool) {
    seed(pool, &[("a", "apple"), ("b", "apple"), ("c", "banana")]).await;
    let by_title = || {
        Post::objects()
            .distinct_on(&["title"])
            .order_by(&[("title", false), ("id", false)])
    };

    let mut q = by_title().compile().expect("compile");
    q.search = Some(SearchClause {
        columns: vec!["title"],
        query: "ban".into(),
    });
    let rows: Vec<Post> = rustango::sql::select_rows_pool(pool, &q)
        .await
        .expect("search");
    assert_eq!(slugs(&rows), ["c"]);

    let only_b = Post::objects().filter("slug", "b").compile().expect("sub");
    let rows = by_title()
        .join_sub(only_b, "s", same_id("s"))
        .fetch(pool)
        .await
        .expect("join");
    assert_eq!(slugs(&rows), ["b"]);
}

/// #1890: `paginate()` sent no ORDER BY, so a page followed heap order.
async fn paginate_orders_by_pk(pool: &Pool) {
    seed(pool, &[("a", "a"), ("b", "b"), ("c", "c")]).await;
    // An UPDATE moves the row to the end of a PostgreSQL heap.
    let mut a = posts(pool).await.remove(0);
    a.title = "a2".into();
    a.save_pool(pool).await.expect("update a");
    let (rows, total) = Post::objects()
        .paginate(1, 2, pool)
        .await
        .expect("paginate");
    assert_eq!(total, 3);
    let got: Vec<&str> = rows.iter().map(|p| p.slug.as_str()).collect();
    assert_eq!(got, ["a", "b"], "the first page is the two lowest PKs");
}

/// #1901: `values()` read a Uuid or bytes column as Null (SQLite and PG)
/// or a Uuid as text (MySQL).
async fn values_decode_uuid_and_bytes(pool: &Pool) {
    let tok = uuid::uuid!("6f1c2a4e-9b7d-4c3a-8e21-0d5f4b6a7c89");
    let mut b = Blob {
        id: Auto::default(),
        token: tok,
        data: vec![0, 1, 255],
    };
    b.insert_pool(pool).await.expect("seed");
    let want = [SqlValue::Uuid(tok), SqlValue::Binary(vec![0, 1, 255])];

    let list = Blob::objects()
        .values_list(&["token", "data"])
        .fetch(pool)
        .await
        .expect("values_list");
    assert_eq!(list, [want.to_vec()]);

    let dict = Blob::objects()
        .values_dict(&["token", "data"])
        .fetch(pool)
        .await
        .expect("values_dict");
    assert_eq!(dict[0]["token"], want[0]);
    assert_eq!(dict[0]["data"], want[1]);
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        insert_or_ignore_on_a_natural_pk,
        insert_or_ignore_reports_a_skip_on_an_auto_pk,
        upsert_reports_the_updated_row,
        bulk_update_sets_null_in_every_row,
        union_keeps_the_first_branch_join,
        union_values_list_flat_in_a_subquery,
        paginated_union_counts_every_branch,
        distinct_on_keeps_search_and_derived_joins,
        paginate_orders_by_pk,
        values_decode_uuid_and_bytes,
    ],
}
