#![cfg(feature = "mysql")]
//! v0.41 — MySQL parity for `manage inspectdb`. Mirrors
//! `inspectdb_sqlite_live.rs`.
//!
//! Reads `MYSQL_TEST_URL`. Tests skip silently when unset.

use std::sync::OnceLock;

use rustango::sql::Pool;

fn serial_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

async fn make_pool() -> Option<Pool> {
    let url = std::env::var("MYSQL_TEST_URL").ok()?;
    let pool = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("mysql connect");

    // Drop child first so the FK constraint doesn't prevent the parent
    // drop from succeeding.
    let _ = sqlx::query("DROP TABLE IF EXISTS `posts`")
        .execute(&pool)
        .await;
    let _ = sqlx::query("DROP TABLE IF EXISTS `authors`")
        .execute(&pool)
        .await;

    sqlx::query(
        "CREATE TABLE `authors` (
            `id`   BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
            `name` VARCHAR(80) NOT NULL,
            `bio`  TEXT
         )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TABLE `posts` (
            `id`        BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
            `author_id` BIGINT NOT NULL,
            `title`     VARCHAR(200) NOT NULL,
            `published` TINYINT(1) NOT NULL DEFAULT 0,
            CONSTRAINT `fk_posts_author` FOREIGN KEY (`author_id`) REFERENCES `authors`(`id`)
         )",
    )
    .execute(&pool)
    .await
    .unwrap();

    Some(Pool::Mysql(pool))
}

#[tokio::test]
async fn inspectdb_emits_model_with_auto_pk_max_length_and_fk_on_mysql() {
    let _serial = serial_lock().lock().await;
    let Some(pool) = make_pool().await else {
        return;
    };
    let mut buf: Vec<u8> = Vec::new();
    rustango::migrate::manage::run_with_writer(
        &pool,
        std::path::Path::new("./migrations"),
        vec!["inspectdb".to_owned()],
        &mut buf,
    )
    .await
    .expect("inspectdb_cmd");
    let out = String::from_utf8(buf).expect("utf8 output");

    assert!(
        out.contains("pub struct Authors"),
        "expected `pub struct Authors`, got:\n{out}"
    );
    assert!(
        out.contains("pub id: Auto<i64>"),
        "expected Auto<i64> PK on authors, got:\n{out}"
    );
    assert!(
        out.contains("max_length = 80"),
        "expected `max_length = 80` for authors.name, got:\n{out}"
    );
    assert!(
        out.contains("pub bio: Option<String>"),
        "expected `Option<String>` for nullable bio, got:\n{out}"
    );

    assert!(
        out.contains("pub struct Posts"),
        "expected `pub struct Posts`, got:\n{out}"
    );
    assert!(
        out.contains(r#"fk = "authors""#),
        "expected `fk = \"authors\"` on posts.author_id, got:\n{out}"
    );
    assert!(
        out.contains("pub author_id: i64"),
        "expected non-null author_id i64, got:\n{out}"
    );
    assert!(
        out.contains("max_length = 200"),
        "expected `max_length = 200` for posts.title, got:\n{out}"
    );
}

#[tokio::test]
async fn inspectdb_filters_by_table_flag_on_mysql() {
    let _serial = serial_lock().lock().await;
    let Some(pool) = make_pool().await else {
        return;
    };
    let mut buf: Vec<u8> = Vec::new();
    rustango::migrate::manage::run_with_writer(
        &pool,
        std::path::Path::new("./migrations"),
        vec![
            "inspectdb".to_owned(),
            "--table".to_owned(),
            "authors".to_owned(),
        ],
        &mut buf,
    )
    .await
    .expect("inspectdb_cmd --table");
    let out = String::from_utf8(buf).expect("utf8 output");
    assert!(
        out.contains("pub struct Authors"),
        "expected Authors when --table=authors, got:\n{out}"
    );
    assert!(
        !out.contains("pub struct Posts"),
        "did NOT expect Posts when filtered to authors, got:\n{out}"
    );
}
