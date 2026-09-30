//! ORM writers that must give the same result on every backend
//! (0.59.11 dialect batch). One body per scenario, three dialects.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::core::{ConflictClause, InsertQuery, Model as _, SqlValue};
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

tri_dialect_test! {
    setup: setup,
    scenarios: [
        insert_or_ignore_on_a_natural_pk,
        insert_or_ignore_reports_a_skip_on_an_auto_pk,
        upsert_reports_the_updated_row,
    ],
}
