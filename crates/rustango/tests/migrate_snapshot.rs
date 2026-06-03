//! Snapshot + diff unit tests for v0.2 migration support.

use rustango::migrate::{
    detect_changes, render_changes, SchemaChange, SchemaSnapshot, TableSnapshot,
};
use rustango::Model;

#[derive(Model)]
#[allow(dead_code)]
#[rustango(table = "snap_user")]
pub struct SnapUser {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 32)]
    name: String,
}

// ---------- snapshot from registry ----------

#[test]
fn snapshot_from_registry_includes_registered_tables() {
    let snap = SchemaSnapshot::from_registry();
    assert!(
        snap.table("snap_user").is_some(),
        "snap_user missing from snapshot",
    );
    let t = snap.table("snap_user").unwrap();
    assert_eq!(t.model, "SnapUser");
    let id = t.field("id").unwrap();
    assert_eq!(id.ty, "i64");
    assert!(id.primary_key);
    let name = t.field("name").unwrap();
    assert_eq!(name.ty, "string");
    assert_eq!(name.max_length, Some(32));
}

#[test]
fn snapshot_round_trips_through_json() {
    let snap = SchemaSnapshot::from_registry();
    let json = serde_json::to_string_pretty(&snap).unwrap();
    let back: SchemaSnapshot = serde_json::from_str(&json).unwrap();
    assert_eq!(snap, back, "snapshot lost data through JSON round-trip");
}

// ---------- diff ----------

fn empty_snapshot() -> SchemaSnapshot {
    SchemaSnapshot {
        tables: vec![],
        m2m_tables: vec![],
        indexes: vec![],
        checks: vec![],
        excludes: vec![],
    }
}

fn user_table() -> TableSnapshot {
    serde_json::from_value(serde_json::json!({
        "name": "snap_user",
        "model": "SnapUser",
        "fields": [
            {"name": "id", "column": "id", "ty": "i64", "nullable": false, "primary_key": true},
            {"name": "name", "column": "name", "ty": "string", "nullable": false, "primary_key": false, "max_length": 32}
        ]
    })).unwrap()
}

#[test]
fn diff_detects_added_table() {
    let prev = empty_snapshot();
    let current = SchemaSnapshot {
        tables: vec![user_table()],
        ..Default::default()
    };
    let changes = detect_changes(&prev, &current);
    assert_eq!(changes, vec![SchemaChange::CreateTable("snap_user".into())]);
}

#[test]
fn diff_detects_dropped_table() {
    let prev = SchemaSnapshot {
        tables: vec![user_table()],
        ..Default::default()
    };
    let current = empty_snapshot();
    let changes = detect_changes(&prev, &current);
    assert_eq!(changes, vec![SchemaChange::DropTable("snap_user".into())]);
}

#[test]
fn diff_detects_added_column() {
    let prev = SchemaSnapshot {
        tables: vec![user_table()],
        ..Default::default()
    };
    let mut current = prev.clone();
    current.tables[0].fields.push(serde_json::from_value(serde_json::json!({
        "name": "email", "column": "email", "ty": "string", "nullable": true, "primary_key": false, "max_length": 100
    })).unwrap());
    current.tables[0]
        .fields
        .sort_by(|a, b| a.column.cmp(&b.column));

    let changes = detect_changes(&prev, &current);
    assert_eq!(
        changes,
        vec![SchemaChange::AddColumn {
            table: "snap_user".into(),
            column: "email".into(),
        }],
    );
}

#[test]
fn diff_detects_dropped_column() {
    let mut prev = SchemaSnapshot {
        tables: vec![user_table()],
        ..Default::default()
    };
    prev.tables[0].fields.push(serde_json::from_value(serde_json::json!({
        "name": "email", "column": "email", "ty": "string", "nullable": true, "primary_key": false
    })).unwrap());
    prev.tables[0]
        .fields
        .sort_by(|a, b| a.column.cmp(&b.column));
    let current = SchemaSnapshot {
        tables: vec![user_table()],
        ..Default::default()
    };

    let changes = detect_changes(&prev, &current);
    assert_eq!(
        changes,
        vec![SchemaChange::DropColumn {
            table: "snap_user".into(),
            column: "email".into(),
        }],
    );
}

#[test]
fn diff_no_changes_when_snapshots_equal() {
    let snap = SchemaSnapshot {
        tables: vec![user_table()],
        ..Default::default()
    };
    assert!(detect_changes(&snap, &snap).is_empty());
}

// ---------- render ----------

#[test]
fn render_create_table_emits_full_ddl() {
    let current = SchemaSnapshot {
        tables: vec![user_table()],
        ..Default::default()
    };
    let ddl = render_changes(&[SchemaChange::CreateTable("snap_user".into())], &current).unwrap();
    assert_eq!(ddl.len(), 1);
    assert!(ddl[0].starts_with(r#"CREATE TABLE "snap_user" ("#));
    assert!(ddl[0].contains(r#""name" VARCHAR(32) NOT NULL"#));
    assert!(ddl[0].contains(r#""id" BIGINT NOT NULL PRIMARY KEY"#));
}

#[test]
fn render_drop_table_emits_cascade() {
    let ddl = render_changes(
        &[SchemaChange::DropTable("snap_user".into())],
        &empty_snapshot(),
    )
    .unwrap();
    assert_eq!(ddl, vec![r#"DROP TABLE "snap_user" CASCADE"#]);
}

#[test]
fn render_add_nullable_column() {
    let mut current = SchemaSnapshot {
        tables: vec![user_table()],
        ..Default::default()
    };
    current.tables[0].fields.push(
        serde_json::from_value(serde_json::json!({
            "name": "bio", "column": "bio", "ty": "string", "nullable": true, "primary_key": false
        }))
        .unwrap(),
    );

    let ddl = render_changes(
        &[SchemaChange::AddColumn {
            table: "snap_user".into(),
            column: "bio".into(),
        }],
        &current,
    )
    .unwrap();
    assert_eq!(
        ddl,
        vec![r#"ALTER TABLE "snap_user" ADD COLUMN "bio" TEXT"#]
    );
}

#[test]
fn render_add_not_null_column_is_rejected() {
    let mut current = SchemaSnapshot {
        tables: vec![user_table()],
        ..Default::default()
    };
    current.tables[0].fields.push(
        serde_json::from_value(serde_json::json!({
            "name": "score", "column": "score", "ty": "i32", "nullable": false, "primary_key": false
        }))
        .unwrap(),
    );

    let err = render_changes(
        &[SchemaChange::AddColumn {
            table: "snap_user".into(),
            column: "score".into(),
        }],
        &current,
    )
    .unwrap_err();
    assert!(
        err.contains("NOT NULL"),
        "expected NOT NULL rejection, got: {err}",
    );
}

#[test]
fn render_drop_column_emits_alter() {
    let ddl = render_changes(
        &[SchemaChange::DropColumn {
            table: "snap_user".into(),
            column: "old_field".into(),
        }],
        &empty_snapshot(),
    )
    .unwrap();
    assert_eq!(
        ddl,
        vec![r#"ALTER TABLE "snap_user" DROP COLUMN "old_field""#],
    );
}

#[test]
fn render_add_not_null_with_default_is_permitted() {
    let mut current = SchemaSnapshot {
        tables: vec![user_table()],
        ..Default::default()
    };
    current.tables[0].fields.push(
        serde_json::from_value(serde_json::json!({
            "name": "score", "column": "score", "ty": "i32",
            "nullable": false, "primary_key": false,
            "default": "0"
        }))
        .unwrap(),
    );

    let ddl = render_changes(
        &[SchemaChange::AddColumn {
            table: "snap_user".into(),
            column: "score".into(),
        }],
        &current,
    )
    .unwrap();
    assert_eq!(
        ddl,
        vec![r#"ALTER TABLE "snap_user" ADD COLUMN "score" INTEGER DEFAULT 0 NOT NULL"#]
    );
}

#[test]
fn render_create_table_with_default_emits_default_clause() {
    let mut current = SchemaSnapshot {
        tables: vec![user_table()],
        ..Default::default()
    };
    // Simulate a fresh `snap_post` with `status` carrying a default.
    let post: TableSnapshot = serde_json::from_value(serde_json::json!({
        "name": "snap_post",
        "model": "SnapPost",
        "fields": [
            {"name": "id", "column": "id", "ty": "i64", "nullable": false, "primary_key": true},
            {
                "name": "status", "column": "status", "ty": "string",
                "nullable": false, "primary_key": false,
                "max_length": 16, "default": "'draft'"
            }
        ]
    }))
    .unwrap();
    current.tables.push(post);

    let ddl = render_changes(&[SchemaChange::CreateTable("snap_post".into())], &current).unwrap();
    assert!(
        ddl[0].contains(r#""status" VARCHAR(16) DEFAULT 'draft' NOT NULL"#),
        "expected DEFAULT in CREATE TABLE, got: {}",
        ddl[0]
    );
}

#[test]
fn render_add_not_null_without_default_still_rejected() {
    // Regression: only the *combination* of NOT NULL + no default is the error.
    let mut current = SchemaSnapshot {
        tables: vec![user_table()],
        ..Default::default()
    };
    current.tables[0].fields.push(
        serde_json::from_value(serde_json::json!({
            "name": "score", "column": "score", "ty": "i32",
            "nullable": false, "primary_key": false
        }))
        .unwrap(),
    );

    let err = render_changes(
        &[SchemaChange::AddColumn {
            table: "snap_user".into(),
            column: "score".into(),
        }],
        &current,
    )
    .unwrap_err();
    assert!(err.contains("NOT NULL"), "got: {err}");
    assert!(
        err.contains("default"),
        "expected hint about default, got: {err}"
    );
}

#[test]
fn render_create_with_fk_emits_alter_after() {
    let mut current = SchemaSnapshot {
        tables: vec![user_table()],
        ..Default::default()
    };
    let post: TableSnapshot = serde_json::from_value(serde_json::json!({
        "name": "snap_post",
        "model": "SnapPost",
        "fields": [
            {"name": "id", "column": "id", "ty": "i64", "nullable": false, "primary_key": true},
            {
                "name": "author_id", "column": "author_id", "ty": "i64",
                "nullable": false, "primary_key": false,
                "fk": {"kind": "fk", "to": "snap_user", "on": "id"}
            }
        ]
    }))
    .unwrap();
    current.tables.push(post);

    let ddl = render_changes(&[SchemaChange::CreateTable("snap_post".into())], &current).unwrap();
    assert_eq!(ddl.len(), 2);
    assert!(ddl[0].starts_with(r#"CREATE TABLE "snap_post""#));
    assert!(ddl[1].contains(r#"FOREIGN KEY ("author_id") REFERENCES "snap_user""#));
}
