//! `#[rustango(unique_when(...))]` — partial unique index DDL emission.
//! Closes #265 / T1.3.
//!
//! Pins:
//!   1. The `unique_when` attribute parses + populates the IndexSchema
//!      with the WHERE clause.
//!   2. The migration writer emits `WHERE <expr>` on PG / SQLite.
//!   3. MySQL silently drops the WHERE clause (no native partial-index
//!      syntax) and surfaces a warning in the rendered batch.

use rustango::core::Model as _;
use rustango::migrate::{render_changes_split_with_dialect, SchemaChange, SchemaSnapshot};
use rustango::sql::{MySql, Postgres, Sqlite};

#[derive(rustango::Model, Debug, Clone)]
#[rustango(table = "uw_user")]
#[rustango(unique_when(
    columns = "email",
    condition = "deleted_at IS NULL",
    name = "uw_unique_active_email"
))]
#[allow(dead_code)]
pub struct User {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 200)]
    email: String,
    deleted_at: Option<chrono::DateTime<chrono::Utc>>,
}

fn unique_when_index() -> &'static rustango::core::IndexSchema {
    User::SCHEMA
        .indexes
        .iter()
        .find(|i| i.name == "uw_unique_active_email")
        .expect("indexes registers unique_when")
}

fn create_index_change() -> SchemaChange {
    let idx = unique_when_index();
    SchemaChange::CreateIndex {
        name: idx.name.to_owned(),
        table: User::SCHEMA.table.to_owned(),
        columns: idx.columns.iter().map(|&s| s.to_owned()).collect(),
        unique: idx.unique,
        method: idx.method.as_str().to_owned(),
        where_clause: idx.where_clause.map(str::to_owned),
    }
}

// ---------- Attribute parsing ----------

#[test]
fn attribute_populates_where_clause_on_schema() {
    let idx = unique_when_index();
    assert!(idx.unique, "unique_when must produce a UNIQUE index");
    assert_eq!(idx.columns, &["email"]);
    assert_eq!(idx.where_clause, Some("deleted_at IS NULL"));
}

// ---------- PG ----------

#[test]
fn pg_emits_where_clause_natively() {
    let snap = SchemaSnapshot::default();
    let batch =
        render_changes_split_with_dialect(&[create_index_change()], &snap, &Postgres).unwrap();
    assert_eq!(batch.warnings.len(), 0);
    let sql = batch.immediate.join("\n");
    assert!(
        sql.contains(r#"CREATE UNIQUE INDEX"#),
        "expected CREATE UNIQUE INDEX, got:\n{sql}"
    );
    assert!(
        sql.contains("WHERE deleted_at IS NULL"),
        "PG should emit WHERE clause natively, got:\n{sql}"
    );
}

// ---------- SQLite ----------

#[test]
fn sqlite_emits_where_clause_natively() {
    let snap = SchemaSnapshot::default();
    let batch =
        render_changes_split_with_dialect(&[create_index_change()], &snap, &Sqlite).unwrap();
    assert_eq!(batch.warnings.len(), 0);
    let sql = batch.immediate.join("\n");
    assert!(
        sql.contains("WHERE deleted_at IS NULL"),
        "SQLite should emit WHERE clause natively, got:\n{sql}"
    );
}

// ---------- MySQL ----------

#[test]
fn mysql_drops_where_clause_and_warns() {
    let snap = SchemaSnapshot::default();
    let batch = render_changes_split_with_dialect(&[create_index_change()], &snap, &MySql).unwrap();
    let sql = batch.immediate.join("\n");
    // MySQL has no partial index — the WHERE clause is dropped.
    assert!(
        !sql.contains("WHERE deleted_at IS NULL"),
        "MySQL must NOT emit WHERE clause, got:\n{sql}"
    );
    // Still emits the UNIQUE INDEX (just without the partial filter).
    assert!(
        sql.contains("CREATE UNIQUE INDEX"),
        "MySQL still emits UNIQUE INDEX, got:\n{sql}"
    );
    // Warning surfaced so the caller knows.
    assert_eq!(
        batch.warnings.len(),
        1,
        "expected one warning, got: {:?}",
        batch.warnings
    );
    assert!(
        batch.warnings[0].contains("partial"),
        "warning should mention partial: {}",
        batch.warnings[0]
    );
}
