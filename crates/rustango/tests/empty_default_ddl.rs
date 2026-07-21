//! Regression for #1161 — a field with `#[rustango(default = "")]` must
//! produce **appliable** `CREATE TABLE` DDL: the empty-string default has to
//! render as the quoted literal `''`, not as nothing (which collapses to
//! `DEFAULT  NOT NULL` and the driver rejects with `near "NOT": syntax error`).
#![cfg(feature = "sqlite")]

use rustango::core::Model as _;
use rustango::migrate::{detect_changes, render_changes_split_with_dialect, SchemaSnapshot};
use rustango::sql::{raw_execute_pool, sqlx, Pool};

// The issue's exact repro model.
#[derive(rustango::Model, Debug, Clone)]
#[rustango(table = "empty_default_demo")]
#[allow(dead_code)]
pub struct Demo {
    #[rustango(primary_key)]
    pub id: rustango::sql::Auto<i64>,
    #[rustango(max_length = 64, default = "")]
    pub name: String,
}

fn create_ddl(dialect: &dyn rustango::sql::Dialect) -> Vec<String> {
    let snap = SchemaSnapshot::from_models(&[Demo::SCHEMA]);
    let changes = detect_changes(&SchemaSnapshot::default(), &snap);
    let batch =
        render_changes_split_with_dialect(&changes, &snap, dialect).expect("render Demo DDL");
    batch
        .immediate
        .into_iter()
        .chain(batch.deferred_fks)
        .collect()
}

#[test]
fn empty_default_renders_quoted_literal() {
    let ddl = create_ddl(&rustango::sql::Sqlite).join("\n");
    assert!(ddl.contains("DEFAULT ''"), "expected DEFAULT '': {ddl}");
    assert!(
        !ddl.contains("DEFAULT  "),
        "empty default leaked a blank: {ddl}"
    );
}

#[tokio::test]
async fn empty_default_applies_and_defaults_to_empty_string() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let url = format!("sqlite:{}?mode=rwc", tmp.path().join("demo.db").display());
    let pool = Pool::connect(&url).await.expect("sqlite connect");

    for stmt in create_ddl(pool.dialect()) {
        raw_execute_pool(&pool, &stmt, Vec::new())
            .await
            .expect("CREATE TABLE with an empty-string default must apply (#1161)");
    }

    let Pool::Sqlite(sq) = &pool else {
        unreachable!()
    };
    // Insert without supplying `name` → the `DEFAULT ''` fills it.
    sqlx::query("INSERT INTO empty_default_demo DEFAULT VALUES")
        .execute(sq)
        .await
        .expect("insert using the default");
    let name: String = sqlx::query_scalar("SELECT name FROM empty_default_demo")
        .fetch_one(sq)
        .await
        .expect("read back");
    assert_eq!(name, "", "the empty-string default should store `''`");
}
