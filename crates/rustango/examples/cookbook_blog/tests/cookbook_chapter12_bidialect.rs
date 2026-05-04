//! Cookbook Chapter 12 — bi-dialect (PostgreSQL + MySQL parity).
//!
//! Verifies the cookbook's models build the same shape against MySQL
//! as they do against PG, and that the ORM round-trips a row through
//! both backends.
//!
//! Skips silently if `MYSQL_TEST_URL` is unset (matches the
//! framework's mysql_live convention).
//!
//! Setup the MySQL container:
//!
//! ```sh
//! docker run -d --name rustango-mysql \
//!   -e MYSQL_ROOT_PASSWORD=rustango \
//!   -e MYSQL_DATABASE=cookbook_blog_my \
//!   -e MYSQL_USER=rustango \
//!   -e MYSQL_PASSWORD=rustango \
//!   -p 3406:3306 mysql:8.0
//! export MYSQL_TEST_URL=mysql://rustango:rustango@127.0.0.1:3406/cookbook_blog_my
//! cargo test --test cookbook_chapter12_bidialect
//! ```

use cookbook_blog::apps::blog::models::Rating;
use rustango::core::Op;
use rustango::sql::{sqlx, Auto, FetcherPool, Pool};

async fn pool() -> Option<Pool> {
    let url = std::env::var("MYSQL_TEST_URL").ok()?;
    Some(Pool::connect(&url).await.expect("connect mysql"))
}

async fn fresh_table(p: &Pool) {
    use sqlx::Executor as _;
    let raw = match p {
        Pool::Postgres(_) => unreachable!("MYSQL_TEST_URL should yield Pool::Mysql"),
        Pool::Mysql(my) => my,
    };
    raw.execute("DROP TABLE IF EXISTS cookbook_rating").await.unwrap();
    raw.execute(
        r#"CREATE TABLE cookbook_rating (
            id BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
            score BIGINT NOT NULL,
            CHECK (score >= 1 AND score <= 5)
        ) ENGINE=InnoDB"#,
    ).await.unwrap();
}

// §12.140 — same Rating model writes + reads via MySQL using the same
// Pool-aware ORM API the cookbook's PG paths use. The model is
// dialect-agnostic; the framework picks AUTO_INCREMENT vs BIGSERIAL
// behind the scenes via its Backend trait dispatch.
#[tokio::test]
async fn cookbook_rating_round_trips_against_mysql() {
    let Some(p) = pool().await else { return };
    fresh_table(&p).await;

    let mut r = Rating { id: Auto::Unset, score: 4 };
    r.save_pool(&p).await.expect("save_pool against mysql");
    let id = match r.id { Auto::Set(v) => v, _ => panic!("AUTO_INCREMENT didn't fill id") };
    assert!(id > 0, "MySQL AUTO_INCREMENT must assign positive id");

    let rows: Vec<Rating> = Rating::objects()
        .filter("id", Op::Eq, id)
        .fetch_pool(&p).await.expect("fetch_pool against mysql");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].score, 4);
}

// §12.141 — bulk i64 ↔ MySQL BIGINT decode (regression for batch3).
#[tokio::test]
async fn mysql_decodes_multi_row_select() {
    let Some(p) = pool().await else { return };
    fresh_table(&p).await;

    for s in 1..=5 {
        let mut r = Rating { id: Auto::Unset, score: s };
        r.save_pool(&p).await.unwrap();
    }
    let rows: Vec<Rating> = Rating::objects()
        .fetch_pool(&p).await.expect("fetch_pool against mysql");
    assert_eq!(rows.len(), 5);
    let total: i64 = rows.iter().map(|r| r.score).sum();
    assert_eq!(total, 1 + 2 + 3 + 4 + 5);
}
