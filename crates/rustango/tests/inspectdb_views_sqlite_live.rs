#![cfg(feature = "sqlite")]
//! `manage inspectdb` view-walking on SQLite — closes #293 / T2.10.
//!
//! Creates a fixture DB with a table AND a view, runs inspectdb, and
//! asserts the emitted source contains:
//!   1. The table's `#[derive(Model)]` block (unchanged behavior).
//!   2. A view-backed `#[derive(Model)]` block carrying both
//!      `table = "..."` and the `view` marker.
//!   3. The view's underlying SQL as a fenced ```sql doc-comment
//!      block above the struct so reviewers can read it inline.

use rustango::sql::Pool;

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
        "CREATE TABLE orders (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            customer  VARCHAR(80) NOT NULL,
            total     INTEGER NOT NULL
         )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE VIEW big_orders AS
            SELECT id, customer, total FROM orders WHERE total > 1000",
    )
    .execute(&pool)
    .await
    .unwrap();

    Pool::Sqlite(pool)
}

#[tokio::test]
async fn inspectdb_walks_views_and_emits_view_marker_on_sqlite() {
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

    // Table half — emitted as before.
    assert!(
        out.contains("pub struct Orders"),
        "expected `pub struct Orders` (table), got:\n{out}"
    );
    assert!(
        out.contains(r#"#[rustango(table = "orders")]"#),
        "table emission should not carry the `view` flag, got:\n{out}"
    );

    // View half — emitted with the `view` marker.
    assert!(
        out.contains("pub struct BigOrders"),
        "expected `pub struct BigOrders` (view), got:\n{out}"
    );
    assert!(
        out.contains(r#"#[rustango(table = "big_orders", view)]"#),
        "expected `#[rustango(table = \"big_orders\", view)]` on view emission, got:\n{out}"
    );

    // The view definition must land in the doc comment as a fenced
    // ```sql block so reviewers can read it without `pg_views`.
    assert!(
        out.contains("```sql"),
        "expected fenced ```sql block for the view definition, got:\n{out}"
    );
    assert!(
        out.contains("SELECT"),
        "expected the view body to appear in the doc comment, got:\n{out}"
    );
    assert!(
        out.contains("WHERE total > 1000"),
        "expected the view's WHERE clause in the doc comment, got:\n{out}"
    );
}

#[tokio::test]
async fn inspectdb_table_only_run_excludes_views_on_sqlite() {
    let pool = make_pool().await;
    let mut buf: Vec<u8> = Vec::new();
    rustango::migrate::manage::run_with_writer(
        &pool,
        std::path::Path::new("./migrations"),
        vec![
            "inspectdb".to_owned(),
            "--table".to_owned(),
            "orders".to_owned(),
        ],
        &mut buf,
    )
    .await
    .expect("inspectdb_cmd --table=orders");
    let out = String::from_utf8(buf).expect("utf8 output");
    assert!(
        out.contains("pub struct Orders"),
        "expected Orders when --table=orders, got:\n{out}"
    );
    assert!(
        !out.contains("pub struct BigOrders"),
        "--table filter should not pull the view through, got:\n{out}"
    );
}
