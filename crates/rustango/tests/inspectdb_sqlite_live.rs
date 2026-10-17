#![cfg(feature = "sqlite")]
//! Live integration test for `manage inspectdb` against a SQLite DB.
//!
//! v0.38 slice 28 — inspectdb is tri-dialect; this test creates a
//! temporary SQLite database with a couple of tables (one with a FK,
//! one with composite columns) and asserts that the emitted Rust
//! source contains the expected `#[derive(Model)]` shape.

use rustango::sql::{Auto, Pool};
use rustango::Model;

async fn make_pool() -> Pool {
    let tmp = tempfile::NamedTempFile::new().expect("tempfile");
    let url = format!("sqlite://{}?mode=rwc", tmp.path().display());
    std::mem::forget(tmp);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("sqlite connect");

    sqlx::query(
        "CREATE TABLE authors (
            id   INTEGER PRIMARY KEY AUTOINCREMENT,
            name VARCHAR(80) NOT NULL,
            bio  TEXT
         )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TABLE posts (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            author_id INTEGER NOT NULL REFERENCES authors(id),
            title     VARCHAR(200) NOT NULL,
            published BOOLEAN NOT NULL DEFAULT 0
         )",
    )
    .execute(&pool)
    .await
    .unwrap();

    Pool::Sqlite(pool)
}

#[tokio::test]
async fn inspectdb_emits_model_with_auto_pk_max_length_and_fk_on_sqlite() {
    let pool = make_pool().await;
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

    // Authors table — autoincrement PK + max_length(80) + nullable bio.
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

    // Posts table — FK back to authors.
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
async fn inspectdb_filters_by_table_flag_on_sqlite() {
    let pool = make_pool().await;
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

/// Items and their text, so the test can hold inspectdb's output to a
/// model that compiles.
macro_rules! compiled_with_text {
    ($text:ident; $($item:tt)*) => {
        $($item)*
        const $text: &str = stringify!($($item)*);
    };
}

// What inspectdb must emit for `Order_Items` below (#1230).
compiled_with_text! { ORDER_ITEMS;
    #[derive(Model, Debug, Clone)]
    #[rustango(table = "Order_Items")]
    pub struct OrderItems {
        #[rustango(primary_key)]
        pub id: Auto<i64>,
        #[rustango(column = "OrderDate", default = "'a::b'")]
        pub orderdate: String,
        #[rustango(default = "'it''s \"x\" \\ y'")]
        pub r#type: String,
        #[rustango(column = "self", default = "0")]
        pub self_: i64,
    }
}

/// Mixed-case and keyword names and quoted defaults emit a model that
/// compiles (#1230).
#[tokio::test]
async fn inspectdb_emits_compiling_models_for_awkward_names_on_sqlite() {
    let schema = <OrderItems as rustango::core::Model>::SCHEMA;
    assert_eq!(schema.field("orderdate").map(|f| f.column), Some("OrderDate"));
    let pool = make_pool().await;
    sqlx::query(
        r#"CREATE TABLE "Order_Items" (
            id INTEGER PRIMARY KEY,
            "OrderDate" TEXT NOT NULL DEFAULT 'a::b',
            "type" TEXT NOT NULL DEFAULT 'it''s "x" \ y',
            "self" INTEGER NOT NULL DEFAULT 0
         )"#,
    )
    .execute(pool.as_sqlite().expect("sqlite"))
    .await
    .unwrap();
    let mut buf: Vec<u8> = Vec::new();
    rustango::migrate::manage::run_with_writer(
        &pool,
        std::path::Path::new("./migrations"),
        vec!["inspectdb".into(), "--table".into(), "Order_Items".into()],
        &mut buf,
    )
    .await
    .expect("inspectdb_cmd");
    let out = String::from_utf8(buf).expect("utf8 output");
    let start = out.find("#[derive(Model").expect("a model");
    let end = start + out[start..].find("\n}").expect("struct end") + 2;
    let squash = |s: &str| s.split_whitespace().collect::<String>();
    assert_eq!(squash(&out[start..end]), squash(ORDER_ITEMS), "got:\n{out}");
}
