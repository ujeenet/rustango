//! Issue #345 — `manage sqlmigrate <name>` prints the SQL a
//! given migration would emit, without touching the database.
//!
//! `sqlmigrate_one` is pure file I/O + render, so the test
//! can run with no `feature = "postgres"` / `feature = "sqlite"`
//! requirement — the migration render path is dialect-agnostic at
//! this layer (the writer picks the actual placeholder shape at
//! `apply` time).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use rustango::migrate::{
    file, sqlmigrate_one, Migration, Operation, SchemaChange, SchemaSnapshot, TableSnapshot,
};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn fresh_dir(label: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let mut p = std::env::temp_dir();
    p.push(format!("rustango_sqlmigrate_{label}_{pid}_{n}"));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn snapshot_with_post_table() -> SchemaSnapshot {
    let table: TableSnapshot = serde_json::from_value(serde_json::json!({
        "name": "sqlmig_post",
        "model": "Post",
        "fields": [
            {"name": "id", "column": "id", "ty": "i64", "nullable": false, "primary_key": true},
            {"name": "title", "column": "title", "ty": "String", "nullable": false, "primary_key": false}
        ]
    }))
    .unwrap();
    SchemaSnapshot {
        tables: vec![table],
        ..Default::default()
    }
}

fn create_table_migration(name: &str) -> Migration {
    let snap = snapshot_with_post_table();
    Migration {
        name: name.to_owned(),
        created_at: "2026-05-22T00:00:00Z".into(),
        prev: None,
        atomic: true,
        scope: Default::default(),
        replaces: Vec::new(),
        snapshot: snap,
        forward: vec![Operation::Schema(SchemaChange::CreateTable(
            "sqlmig_post".into(),
        ))],
    }
}

#[test]
fn one_migration_produces_create_table_sql() {
    let dir = fresh_dir("create");
    let mig = create_table_migration("0001_init");
    file::write(&dir.join("0001_init.json"), &mig).unwrap();

    let preview = sqlmigrate_one(&dir, "0001_init", &rustango::sql::Postgres).expect("preview");
    assert_eq!(preview.name, "0001_init");
    // Atomic by default → BEGIN should be the first statement, COMMIT the last.
    assert!(preview.atomic);
    assert_eq!(
        preview.statements.first().map(String::as_str),
        Some("BEGIN")
    );
    // The CREATE TABLE statement is rendered for the target table.
    let body = preview.statements.join("\n");
    assert!(
        body.contains("CREATE TABLE") && body.contains("sqlmig_post"),
        "expected CREATE TABLE for sqlmig_post, got:\n{body}"
    );
}

#[test]
fn missing_migration_returns_validation_error() {
    let dir = fresh_dir("missing");
    let mig = create_table_migration("0001_init");
    file::write(&dir.join("0001_init.json"), &mig).unwrap();

    let err = sqlmigrate_one(&dir, "0002_not_there", &rustango::sql::Postgres).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("0002_not_there") && msg.contains("not found"),
        "error should name the missing migration, got: {msg}"
    );
}

#[test]
fn sqlmigrate_is_pure_no_writes() {
    // After invoking sqlmigrate, the directory should still contain
    // exactly the file we created — no ledger row, no scratch file,
    // no application of the migration.
    let dir = fresh_dir("pure");
    let mig = create_table_migration("0001_init");
    file::write(&dir.join("0001_init.json"), &mig).unwrap();

    let _ = sqlmigrate_one(&dir, "0001_init", &rustango::sql::Postgres).expect("preview");
    let entries: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
    assert_eq!(
        entries.len(),
        1,
        "sqlmigrate must not write to the migrations dir"
    );
}

/// #2025 — the preview renders for the target dialect, from the previous
/// snapshot, so MySQL shows the FK drop a column drop runs first.
#[test]
fn preview_renders_for_the_target_dialect() {
    let dir = fresh_dir("dialect");
    let post = |with_author: bool| {
        let mut fields = vec![
            serde_json::json!({"name": "id", "column": "id", "ty": "i64", "nullable": false, "primary_key": true}),
        ];
        if with_author {
            fields.push(serde_json::json!({
                "name": "author", "column": "author_id", "ty": "i64", "nullable": true,
                "primary_key": false, "fk": {"kind": "fk", "to": "sqlmig_post", "on": "id"}
            }));
        }
        let table: TableSnapshot = serde_json::from_value(serde_json::json!({
            "name": "sqlmig_post", "model": "Post", "fields": fields
        }))
        .unwrap();
        SchemaSnapshot {
            tables: vec![table],
            ..Default::default()
        }
    };
    let mut first = create_table_migration("0001_init");
    first.snapshot = post(true);
    file::write(&dir.join("0001_init.json"), &first).unwrap();
    let drop = Migration {
        name: "0002_drop".into(),
        prev: Some("0001_init".into()),
        snapshot: post(false),
        forward: vec![Operation::Schema(SchemaChange::DropColumn {
            table: "sqlmig_post".into(),
            column: "author_id".into(),
        })],
        ..first.clone()
    };
    file::write(&dir.join("0002_drop.json"), &drop).unwrap();

    let my = sqlmigrate_one(&dir, "0002_drop", &rustango::sql::MySql).expect("preview");
    let body = my.statements.join("\n");
    let fk = body.find("DROP FOREIGN KEY").expect(&body);
    let col = body.find("DROP COLUMN `author_id`").expect(&body);
    assert!(fk < col, "{body}");
    let pg = sqlmigrate_one(&dir, "0002_drop", &rustango::sql::Postgres).expect("preview");
    let body = pg.statements.join("\n");
    assert!(
        body.contains("DROP COLUMN \"author_id\"") && !body.contains("FOREIGN KEY"),
        "{body}"
    );
}
