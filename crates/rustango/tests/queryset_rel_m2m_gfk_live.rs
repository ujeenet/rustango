//! Live, multi-dialect tests for the **M2M** and **generic-FK (GFK)**
//! arms of the relation-existence / eager-aggregate family — issue #830.
//!
//! Validates end-to-end against real engines that `where_has` /
//! `annotate_count` / `annotate_sum` resolve M2M (junction) and GFK
//! (content-type-discriminated child) relations correctly. The GFK
//! fixture includes a **wrong-content-type decoy** child row to prove
//! the content-type discriminator excludes children pointing at a
//! different model.
//!
//! - SQLite always runs (in-memory).
//! - Postgres runs when `DATABASE_URL` is set; MySQL when
//!   `MYSQL_TEST_URL` is set — each `DROP`s + recreates its tables.

#![allow(dead_code)]

use std::collections::HashMap;

use rustango::core::SqlValue;
use rustango::sql::FetcherPool as _;
use rustango::Model;

// M2M: Post <-> Tag through rmg_post_tags
#[derive(Model)]
#[rustango(
    table = "rmg_post",
    m2m(
        name = "tags",
        to = "rmg_tag",
        through = "rmg_post_tags",
        src = "post_id",
        dst = "tag_id",
        auto_create = false,
    )
)]
pub struct Post {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 80)]
    title: String,
}

#[derive(Model)]
#[rustango(table = "rmg_tag")]
pub struct Tag {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 40)]
    name: String,
    weight: i64,
}

// GFK: Article with generic comments
#[derive(Model)]
#[rustango(
    table = "rmg_article",
    generic_has(
        name = "comments",
        child = "GComment",
        ct_column = "content_type_id",
        pk_column = "object_pk"
    )
)]
pub struct Article {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 80)]
    title: String,
}

#[derive(Model)]
#[rustango(table = "rmg_gcomment")]
pub struct GComment {
    #[rustango(primary_key)]
    id: i64,
    content_type_id: i64,
    object_pk: i64,
    #[rustango(max_length = 200)]
    body: String,
    score: i64,
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

/// `Some(n)` for an integer cell, `None` for SQL `NULL`. `SUM` over zero
/// rows is standard-SQL `NULL` on PG/MySQL (SQLite's `CAST` yields 0), so
/// the sum maps skip childless rows rather than pin that per-dialect quirk.
fn get_i64_opt(row: &HashMap<String, SqlValue>, key: &str) -> Option<i64> {
    match row.get(key).unwrap_or(&SqlValue::Null) {
        SqlValue::I64(n) => Some(*n),
        SqlValue::Null => None,
        other => panic!("expected i64/NULL at `{key}`, got {other:?}"),
    }
}

/// The shared assertions, run against an already-seeded pool (see
/// [`seed_sql`] for the fixture). A gcomment under a *different* content
/// type must be excluded by the GFK discriminator — that's article `R`.
async fn assert_relations(pool: &rustango::sql::Pool) {
    // --- M2M counts: A=2, B=1, C=0 ---
    let by_post: HashMap<String, i64> = Post::objects()
        .annotate_count("tags")
        .fetch(pool)
        .await
        .unwrap()
        .iter()
        .map(|r| (get_string(r, "title").to_owned(), get_i64(r, "tags_count")))
        .collect();
    assert_eq!(by_post.get("A"), Some(&2), "post A has 2 tags");
    assert_eq!(by_post.get("B"), Some(&1));
    assert_eq!(by_post.get("C"), Some(&0), "post C has no tags");

    // --- M2M sum of a target column through the junction: A=12, B=5 ---
    let weights: HashMap<String, i64> = Post::objects()
        .annotate_sum("tags", "weight")
        .fetch(pool)
        .await
        .unwrap()
        .iter()
        .filter_map(|r| {
            Some((
                get_string(r, "title").to_owned(),
                get_i64_opt(r, "tags_sum_weight")?,
            ))
        })
        .collect();
    assert_eq!(weights.get("A"), Some(&12), "A: weights 5 + 7");
    assert_eq!(weights.get("B"), Some(&5));

    // --- M2M where_has: A, B (not C) ---
    let mut has_tags: Vec<String> = Post::objects()
        .where_has("tags")
        .fetch(pool)
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.title)
        .collect();
    has_tags.sort();
    assert_eq!(has_tags, vec!["A", "B"]);

    // --- GFK counts: P=2, Q=1, R=0 (decoy under a different ct excluded) ---
    let by_article: HashMap<String, i64> = Article::objects()
        .annotate_count("comments")
        .fetch(pool)
        .await
        .unwrap()
        .iter()
        .map(|r| {
            (
                get_string(r, "title").to_owned(),
                get_i64(r, "comments_count"),
            )
        })
        .collect();
    assert_eq!(by_article.get("P"), Some(&2), "P has 2 comments");
    assert_eq!(by_article.get("Q"), Some(&1));
    assert_eq!(
        by_article.get("R"),
        Some(&0),
        "R's only comment is under a different content type — must be excluded"
    );

    // --- GFK sum of a child column: P=7, Q=10 ---
    let scores: HashMap<String, i64> = Article::objects()
        .annotate_sum("comments", "score")
        .fetch(pool)
        .await
        .unwrap()
        .iter()
        .filter_map(|r| {
            Some((
                get_string(r, "title").to_owned(),
                get_i64_opt(r, "comments_sum_score")?,
            ))
        })
        .collect();
    assert_eq!(scores.get("P"), Some(&7), "P: scores 3 + 4");
    assert_eq!(scores.get("Q"), Some(&10));

    // --- GFK where_has: P, Q (not R) ---
    let mut commented: Vec<String> = Article::objects()
        .where_has("comments")
        .fetch(pool)
        .await
        .unwrap()
        .into_iter()
        .map(|a| a.title)
        .collect();
    commented.sort();
    assert_eq!(commented, vec!["P", "Q"]);
}

/// Seed statements shared across dialects. `reserved_table` is the
/// dialect-quoted form of the reserved `table` column (`"table"` on
/// PG/SQLite, `` `table` `` on MySQL). Content-type id `1` maps to
/// `rmg_article`; id `99` is a decoy model — the gcomment under ct `99`
/// (pointing at article `3`/`R`) must be excluded by the GFK
/// discriminator.
fn seed_sql(reserved_table: &str) -> Vec<String> {
    vec![
        "INSERT INTO rmg_post (id, title) VALUES (1, 'A'), (2, 'B'), (3, 'C')".to_owned(),
        "INSERT INTO rmg_tag (id, name, weight) VALUES (10, 'x', 5), (20, 'y', 7)".to_owned(),
        "INSERT INTO rmg_post_tags (post_id, tag_id) VALUES (1, 10), (1, 20), (2, 10)".to_owned(),
        "INSERT INTO rmg_article (id, title) VALUES (1, 'P'), (2, 'Q'), (3, 'R')".to_owned(),
        // content type rows: id 1 = rmg_article, id 99 = some other model.
        format!(
            "INSERT INTO rustango_content_types (id, app_label, model_name, {reserved_table}) \
             VALUES (1, 'app', 'article', 'rmg_article'), (99, 'app', 'other', 'rmg_other')"
        ),
        // comments: P(obj 1) x2, Q(obj 2) x1 under ct 1; one decoy under ct 99 → R(obj 3).
        format!(
            "INSERT INTO rmg_gcomment (id, content_type_id, object_pk, body, score) VALUES \
             (1, 1, 1, 'a', 3), (2, 1, 1, 'b', 4), (3, 1, 2, 'c', 10), (4, 99, 3, 'decoy', 1000)"
        ),
    ]
}

// ----------------------------------------------------------------- SQLite

#[cfg(feature = "sqlite")]
mod sqlite_live {
    use super::*;
    use rustango::sql::{sqlx, Pool};

    async fn pool() -> Pool {
        let p = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("sqlite");
        for ddl in [
            "CREATE TABLE rmg_post (id INTEGER PRIMARY KEY, title TEXT NOT NULL)",
            "CREATE TABLE rmg_tag (id INTEGER PRIMARY KEY, name TEXT NOT NULL, weight INTEGER NOT NULL)",
            "CREATE TABLE rmg_post_tags (post_id INTEGER NOT NULL, tag_id INTEGER NOT NULL)",
            "CREATE TABLE rmg_article (id INTEGER PRIMARY KEY, title TEXT NOT NULL)",
            "CREATE TABLE rmg_gcomment (id INTEGER PRIMARY KEY, content_type_id INTEGER NOT NULL, object_pk INTEGER NOT NULL, body TEXT NOT NULL, score INTEGER NOT NULL)",
            "CREATE TABLE rustango_content_types (id INTEGER PRIMARY KEY, app_label TEXT NOT NULL, model_name TEXT NOT NULL, \"table\" TEXT NOT NULL)",
        ] {
            sqlx::query(ddl).execute(&p).await.expect(ddl);
        }
        for stmt in seed_sql("\"table\"") {
            sqlx::query(&stmt).execute(&p).await.expect(&stmt);
        }
        p.into()
    }

    #[tokio::test]
    async fn m2m_and_gfk_relations() {
        let pool = pool().await;
        assert_relations(&pool).await;
    }
}

// --------------------------------------------------------------- Postgres

#[cfg(feature = "postgres")]
mod pg_live {
    use super::*;
    use rustango::sql::{sqlx, Pool};

    #[tokio::test]
    async fn m2m_and_gfk_relations() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("DATABASE_URL unset — skipping PG M2M/GFK live test");
            return;
        };
        let pg = sqlx::PgPool::connect(&url).await.expect("connect PG");
        for ddl in [
            "DROP TABLE IF EXISTS rmg_post_tags",
            "DROP TABLE IF EXISTS rmg_gcomment",
            "DROP TABLE IF EXISTS rmg_post",
            "DROP TABLE IF EXISTS rmg_tag",
            "DROP TABLE IF EXISTS rmg_article",
            "DROP TABLE IF EXISTS rustango_content_types",
            "CREATE TABLE rmg_post (id BIGINT PRIMARY KEY, title VARCHAR(80) NOT NULL)",
            "CREATE TABLE rmg_tag (id BIGINT PRIMARY KEY, name VARCHAR(40) NOT NULL, weight BIGINT NOT NULL)",
            "CREATE TABLE rmg_post_tags (post_id BIGINT NOT NULL, tag_id BIGINT NOT NULL)",
            "CREATE TABLE rmg_article (id BIGINT PRIMARY KEY, title VARCHAR(80) NOT NULL)",
            "CREATE TABLE rmg_gcomment (id BIGINT PRIMARY KEY, content_type_id BIGINT NOT NULL, object_pk BIGINT NOT NULL, body VARCHAR(200) NOT NULL, score BIGINT NOT NULL)",
            "CREATE TABLE rustango_content_types (id BIGINT PRIMARY KEY, app_label VARCHAR(100) NOT NULL, model_name VARCHAR(100) NOT NULL, \"table\" VARCHAR(100) NOT NULL)",
        ] {
            sqlx::query(ddl).execute(&pg).await.expect(ddl);
        }
        for stmt in seed_sql("\"table\"") {
            sqlx::query(&stmt).execute(&pg).await.expect(&stmt);
        }
        let pool: Pool = pg.into();
        assert_relations(&pool).await;
    }
}

// ------------------------------------------------------------------ MySQL

#[cfg(feature = "mysql")]
mod my_live {
    use super::*;
    use rustango::sql::{sqlx, Pool};

    #[tokio::test]
    async fn m2m_and_gfk_relations() {
        let Ok(url) = std::env::var("MYSQL_TEST_URL") else {
            eprintln!("MYSQL_TEST_URL unset — skipping MySQL M2M/GFK live test");
            return;
        };
        let my = sqlx::MySqlPool::connect(&url).await.expect("connect MySQL");
        for ddl in [
            "DROP TABLE IF EXISTS rmg_post_tags",
            "DROP TABLE IF EXISTS rmg_gcomment",
            "DROP TABLE IF EXISTS rmg_post",
            "DROP TABLE IF EXISTS rmg_tag",
            "DROP TABLE IF EXISTS rmg_article",
            "DROP TABLE IF EXISTS rustango_content_types",
            "CREATE TABLE rmg_post (id BIGINT PRIMARY KEY, title VARCHAR(80) NOT NULL)",
            "CREATE TABLE rmg_tag (id BIGINT PRIMARY KEY, name VARCHAR(40) NOT NULL, weight BIGINT NOT NULL)",
            "CREATE TABLE rmg_post_tags (post_id BIGINT NOT NULL, tag_id BIGINT NOT NULL)",
            "CREATE TABLE rmg_article (id BIGINT PRIMARY KEY, title VARCHAR(80) NOT NULL)",
            "CREATE TABLE rmg_gcomment (id BIGINT PRIMARY KEY, content_type_id BIGINT NOT NULL, object_pk BIGINT NOT NULL, body VARCHAR(200) NOT NULL, score BIGINT NOT NULL)",
            "CREATE TABLE rustango_content_types (id BIGINT PRIMARY KEY, app_label VARCHAR(100) NOT NULL, model_name VARCHAR(100) NOT NULL, `table` VARCHAR(100) NOT NULL)",
        ] {
            sqlx::query(ddl).execute(&my).await.expect(ddl);
        }
        for stmt in seed_sql("`table`") {
            sqlx::query(&stmt).execute(&my).await.expect(&stmt);
        }
        let pool: Pool = my.into();
        assert_relations(&pool).await;
    }
}
