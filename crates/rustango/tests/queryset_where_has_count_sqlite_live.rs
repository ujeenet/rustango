#![cfg(feature = "sqlite")]
//! Live SQLite test for `QuerySet::where_has_count(name, op, n)` —
//! issue #830 slice 3 (count-comparator `has($rel, $op, $n)`).
//!
//! Exercises the correlated `(SELECT COUNT(*) FROM <child> WHERE
//! <child_fk> = <outer>.<pk>) <op> n` predicate end-to-end against a
//! real engine: seed authors with differing book counts, then assert
//! each comparison operator selects the right parents.

use rustango::core::Op;
use rustango::sql::{sqlx, Auto, FetcherPool as _, ForeignKey, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "whc_author",
    reverse_has(name = "books", child = "Book", child_fk_column = "author_id",)
)]
#[allow(dead_code)]
pub struct Author {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 40)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "whc_book")]
#[allow(dead_code)]
pub struct Book {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub title: String,
    pub author_id: ForeignKey<Author, i64>,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE whc_author (
            id   INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TABLE whc_book (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            title     TEXT NOT NULL,
            author_id INTEGER NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    p.into()
}

/// Insert an author with `book_count` books; return its PK.
async fn seed_author(pool: &Pool, name: &str, book_count: usize) -> i64 {
    let mut author = Author {
        id: Auto::default(),
        name: name.into(),
    };
    author.save_pool(pool).await.unwrap();
    let id = *author.id.get().unwrap();
    for i in 0..book_count {
        let mut book = Book {
            id: Auto::default(),
            title: format!("{name}-{i}"),
            author_id: ForeignKey::from(id),
        };
        book.save_pool(pool).await.unwrap();
    }
    id
}

/// Seed a fixed fixture: Zero=0 books, One=1, Three=3, Five=5.
async fn seed(pool: &Pool) {
    seed_author(pool, "Zero", 0).await;
    seed_author(pool, "One", 1).await;
    seed_author(pool, "Three", 3).await;
    seed_author(pool, "Five", 5).await;
}

async fn names_for(pool: &Pool, op: Op, n: i64) -> Vec<String> {
    let mut names: Vec<String> = Author::objects()
        .where_has_count("books", op, n)
        .fetch_pool(pool)
        .await
        .unwrap()
        .into_iter()
        .map(|a| a.name)
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn gt_selects_parents_above_threshold() {
    let pool = make_pool().await;
    seed(&pool).await;
    // count > 3 → only Five.
    assert_eq!(names_for(&pool, Op::Gt, 3).await, vec!["Five"]);
}

#[tokio::test]
async fn gte_selects_at_and_above_threshold() {
    let pool = make_pool().await;
    seed(&pool).await;
    // count >= 3 → Three, Five.
    assert_eq!(names_for(&pool, Op::Gte, 3).await, vec!["Five", "Three"]);
}

#[tokio::test]
async fn eq_selects_exact_count() {
    let pool = make_pool().await;
    seed(&pool).await;
    assert_eq!(names_for(&pool, Op::Eq, 1).await, vec!["One"]);
    // Zero books matches `= 0` — the correlated COUNT(*) returns 0, not NULL.
    assert_eq!(names_for(&pool, Op::Eq, 0).await, vec!["Zero"]);
}

#[tokio::test]
async fn lt_and_lte_select_below_threshold() {
    let pool = make_pool().await;
    seed(&pool).await;
    // count < 3 → Zero, One.
    assert_eq!(names_for(&pool, Op::Lt, 3).await, vec!["One", "Zero"]);
    // count <= 1 → Zero, One.
    assert_eq!(names_for(&pool, Op::Lte, 1).await, vec!["One", "Zero"]);
}

#[tokio::test]
async fn ne_excludes_exact_count() {
    let pool = make_pool().await;
    seed(&pool).await;
    // count != 0 → everyone with at least one book.
    assert_eq!(
        names_for(&pool, Op::Ne, 0).await,
        vec!["Five", "One", "Three"]
    );
}

#[tokio::test]
async fn composes_with_other_filters() {
    let pool = make_pool().await;
    seed(&pool).await;
    // count >= 1 AND name = "One" → just One. Confirms the correlated
    // subquery AND-joins cleanly with a plain column predicate.
    let rows = Author::objects()
        .filter("name", "One")
        .where_has_count("books", Op::Gte, 1)
        .fetch_pool(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "One");
}

#[tokio::test]
async fn unknown_relation_errors_at_compile_time() {
    let pool = make_pool().await;
    seed(&pool).await;
    let err = Author::objects()
        .where_has_count("nope", Op::Gt, 1)
        .fetch_pool(&pool)
        .await
        .unwrap_err();
    assert!(format!("{err}").contains("nope"));
}
