//! Live, multi-dialect tests for the relation eager-aggregate family
//! `QuerySet::annotate_count` / `annotate_sum` / `annotate_avg` /
//! `annotate_max` / `annotate_min` / `annotate_exists` — issue #830
//! slice 4/5 (`withCount`/`withSum`/`withExists`/… by relation name).
//!
//! Exercises the correlated aggregate projection end-to-end against real
//! engines: seed authors with differing book counts and page totals, then
//! assert each parent row carries the right `<rel>_<agg>` value in the
//! returned `HashMap` rows. Because each aggregate comes from a
//! correlated subquery (not a JOIN) the counts never double-count, and
//! the childless parent still appears (with count 0).
//!
//! - SQLite always runs (in-memory, isolated per test).
//! - Postgres runs when `DATABASE_URL` is set; MySQL when `MYSQL_TEST_URL`
//!   is set — each in a single comprehensive test (shared server, so it
//!   `DROP`s + re-creates its tables up front to stay self-contained).
//!
//! The models are backend-agnostic; only the table DDL + pool setup
//! differ per dialect.

#![allow(dead_code)]

use std::collections::HashMap;

use rustango::core::SqlValue;
use rustango::sql::{Auto, ForeignKey};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "arl_author",
    reverse_has(name = "books", child = "Book", child_fk_column = "author_id",)
)]
pub struct Author {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 40)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "arl_book")]
pub struct Book {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub title: String,
    pub author_id: ForeignKey<Author, i64>,
    pub pages: i64,
}

fn get_i64(row: &HashMap<String, SqlValue>, key: &str) -> i64 {
    match row.get(key).unwrap_or(&SqlValue::Null) {
        SqlValue::I64(n) => *n,
        other => panic!("expected i64 at `{key}`, got {other:?}"),
    }
}

fn get_string<'r>(row: &'r HashMap<String, SqlValue>, key: &str) -> &'r str {
    match row.get(key).unwrap_or(&SqlValue::Null) {
        SqlValue::String(s) => s,
        other => panic!("expected string at `{key}`, got {other:?}"),
    }
}

/// Insert an author with one book per entry in `page_counts`; return PK.
async fn seed_author(pool: &rustango::sql::Pool, name: &str, page_counts: &[i64]) -> i64 {
    let mut author = Author {
        id: Auto::default(),
        name: name.into(),
    };
    author.save_pool(pool).await.unwrap();
    let id = *author.id.get().unwrap();
    for (i, pages) in page_counts.iter().enumerate() {
        let mut book = Book {
            id: Auto::default(),
            title: format!("{name}-{i}"),
            author_id: ForeignKey::from(id),
            pages: *pages,
        };
        book.save_pool(pool).await.unwrap();
    }
    id
}

/// Seed: Zero=0 books; One=1 book (100p); Three=3 books (10+20+30).
async fn seed(pool: &rustango::sql::Pool) {
    seed_author(pool, "Zero", &[]).await;
    seed_author(pool, "One", &[100]).await;
    seed_author(pool, "Three", &[10, 20, 30]).await;
}

// ----------------------------------------------------------------- SQLite

#[cfg(feature = "sqlite")]
mod sqlite_live {
    use super::*;
    use rustango::sql::{sqlx, Pool};

    async fn make_pool() -> Pool {
        let p = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("sqlite memory");
        sqlx::query(
            "CREATE TABLE arl_author (
                id   INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL
            )",
        )
        .execute(&p)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE arl_book (
                id        INTEGER PRIMARY KEY AUTOINCREMENT,
                title     TEXT NOT NULL,
                author_id INTEGER NOT NULL,
                pages     INTEGER NOT NULL
            )",
        )
        .execute(&p)
        .await
        .unwrap();
        p.into()
    }

    async fn counts_by_name(pool: &Pool) -> HashMap<String, i64> {
        Author::objects()
            .annotate_count("books")
            .fetch(pool)
            .await
            .unwrap()
            .iter()
            .map(|r| (get_string(r, "name").to_owned(), get_i64(r, "books_count")))
            .collect()
    }

    #[tokio::test]
    async fn annotate_count_returns_zero_for_childless_parent() {
        let pool = make_pool().await;
        seed(&pool).await;
        let counts = counts_by_name(&pool).await;
        // Correlated COUNT(*) returns 0 (not NULL/absent) for the childless row.
        assert_eq!(counts.get("Zero"), Some(&0));
        assert_eq!(counts.get("One"), Some(&1));
        assert_eq!(counts.get("Three"), Some(&3));
    }

    #[tokio::test]
    async fn annotate_count_projects_parent_columns_too() {
        let pool = make_pool().await;
        seed(&pool).await;
        let rows = Author::objects()
            .annotate_count("books")
            .fetch(&pool)
            .await
            .unwrap();
        assert_eq!(rows.len(), 3);
        for row in &rows {
            assert!(row.contains_key("id"), "missing parent `id`: {row:?}");
            assert!(row.contains_key("name"), "missing parent `name`: {row:?}");
            assert!(row.contains_key("books_count"), "missing count: {row:?}");
        }
    }

    #[tokio::test]
    async fn annotate_sum_totals_child_column() {
        let pool = make_pool().await;
        seed(&pool).await;
        let by_name: HashMap<String, i64> = Author::objects()
            .annotate_sum("books", "pages")
            .fetch(&pool)
            .await
            .unwrap()
            .iter()
            .map(|r| {
                (
                    get_string(r, "name").to_owned(),
                    get_i64(r, "books_sum_pages"),
                )
            })
            .collect();
        assert_eq!(by_name.get("One"), Some(&100));
        assert_eq!(by_name.get("Three"), Some(&60)); // 10 + 20 + 30
    }

    #[tokio::test]
    async fn annotate_max_and_min_over_child_column() {
        let pool = make_pool().await;
        seed(&pool).await;
        let row = Author::objects()
            .filter("name", "Three")
            .annotate_max("books", "pages")
            .fetch(&pool)
            .await
            .unwrap();
        assert_eq!(get_i64(&row[0], "books_max_pages"), 30);

        let row = Author::objects()
            .filter("name", "Three")
            .annotate_min("books", "pages")
            .fetch(&pool)
            .await
            .unwrap();
        assert_eq!(get_i64(&row[0], "books_min_pages"), 10);
    }

    #[tokio::test]
    async fn annotate_count_composes_with_a_where_filter() {
        let pool = make_pool().await;
        seed(&pool).await;
        let rows = Author::objects()
            .filter("name", "Three")
            .annotate_count("books")
            .fetch(&pool)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(get_i64(&rows[0], "books_count"), 3);
    }

    #[tokio::test]
    async fn annotate_exists_flags_presence_as_one_or_zero() {
        let pool = make_pool().await;
        seed(&pool).await;
        let by_name: HashMap<String, i64> = Author::objects()
            .annotate_exists("books")
            .fetch(&pool)
            .await
            .unwrap()
            .iter()
            .map(|r| (get_string(r, "name").to_owned(), get_i64(r, "books_exists")))
            .collect();
        assert_eq!(by_name.get("Zero"), Some(&0)); // no books -> 0
        assert_eq!(by_name.get("One"), Some(&1));
        assert_eq!(by_name.get("Three"), Some(&1));
    }

    #[tokio::test]
    async fn unknown_relation_errors_at_compile_time() {
        let pool = make_pool().await;
        seed(&pool).await;
        let err = Author::objects()
            .annotate_count("nope")
            .fetch(&pool)
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("nope"));
    }
}

// --------------------------------------------------------------- Postgres

#[cfg(feature = "postgres")]
mod pg_live {
    use super::*;
    use rustango::sql::{sqlx, Pool};

    #[tokio::test]
    async fn count_and_sum_over_relation() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("DATABASE_URL unset — skipping PG annotate-relation live test");
            return;
        };
        let pg = sqlx::PgPool::connect(&url).await.expect("connect PG");
        for ddl in [
            "DROP TABLE IF EXISTS arl_book",
            "DROP TABLE IF EXISTS arl_author",
            "CREATE TABLE arl_author (id BIGSERIAL PRIMARY KEY, name VARCHAR(40) NOT NULL)",
            "CREATE TABLE arl_book (id BIGSERIAL PRIMARY KEY, title VARCHAR(80) NOT NULL, \
             author_id BIGINT NOT NULL, pages BIGINT NOT NULL)",
        ] {
            sqlx::query(ddl).execute(&pg).await.expect(ddl);
        }
        let pool: Pool = pg.into();
        seed(&pool).await;

        let counts: HashMap<String, i64> = Author::objects()
            .annotate_count("books")
            .fetch(&pool)
            .await
            .unwrap()
            .iter()
            .map(|r| (get_string(r, "name").to_owned(), get_i64(r, "books_count")))
            .collect();
        assert_eq!(counts.get("Zero"), Some(&0));
        assert_eq!(counts.get("One"), Some(&1));
        assert_eq!(counts.get("Three"), Some(&3));

        let three = Author::objects()
            .filter("name", "Three")
            .annotate_sum("books", "pages")
            .fetch(&pool)
            .await
            .unwrap();
        assert_eq!(get_i64(&three[0], "books_sum_pages"), 60);

        let exists: HashMap<String, i64> = Author::objects()
            .annotate_exists("books")
            .fetch(&pool)
            .await
            .unwrap()
            .iter()
            .map(|r| (get_string(r, "name").to_owned(), get_i64(r, "books_exists")))
            .collect();
        assert_eq!(exists.get("Zero"), Some(&0));
        assert_eq!(exists.get("One"), Some(&1));
        assert_eq!(exists.get("Three"), Some(&1));
    }
}

// ------------------------------------------------------------------ MySQL

#[cfg(feature = "mysql")]
mod my_live {
    use super::*;
    use rustango::sql::{sqlx, Pool};

    #[tokio::test]
    async fn count_and_sum_over_relation() {
        let Ok(url) = std::env::var("MYSQL_TEST_URL") else {
            eprintln!("MYSQL_TEST_URL unset — skipping MySQL annotate-relation live test");
            return;
        };
        let my = sqlx::MySqlPool::connect(&url).await.expect("connect MySQL");
        for ddl in [
            "DROP TABLE IF EXISTS arl_book",
            "DROP TABLE IF EXISTS arl_author",
            "CREATE TABLE arl_author (id BIGINT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(40) NOT NULL)",
            "CREATE TABLE arl_book (id BIGINT AUTO_INCREMENT PRIMARY KEY, title VARCHAR(80) NOT NULL, \
             author_id BIGINT NOT NULL, pages BIGINT NOT NULL)",
        ] {
            sqlx::query(ddl).execute(&my).await.expect(ddl);
        }
        let pool: Pool = my.into();
        seed(&pool).await;

        let counts: HashMap<String, i64> = Author::objects()
            .annotate_count("books")
            .fetch(&pool)
            .await
            .unwrap()
            .iter()
            .map(|r| (get_string(r, "name").to_owned(), get_i64(r, "books_count")))
            .collect();
        assert_eq!(counts.get("Zero"), Some(&0));
        assert_eq!(counts.get("One"), Some(&1));
        assert_eq!(counts.get("Three"), Some(&3));

        let three = Author::objects()
            .filter("name", "Three")
            .annotate_sum("books", "pages")
            .fetch(&pool)
            .await
            .unwrap();
        assert_eq!(get_i64(&three[0], "books_sum_pages"), 60);

        let exists: HashMap<String, i64> = Author::objects()
            .annotate_exists("books")
            .fetch(&pool)
            .await
            .unwrap()
            .iter()
            .map(|r| (get_string(r, "name").to_owned(), get_i64(r, "books_exists")))
            .collect();
        assert_eq!(exists.get("Zero"), Some(&0));
        assert_eq!(exists.get("One"), Some(&1));
        assert_eq!(exists.get("Three"), Some(&1));
    }
}
