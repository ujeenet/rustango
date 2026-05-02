//! Unit tests for the on-disk migration file format
//! (`rustango::migrate::file`).

use std::path::PathBuf;

use rustango::migrate::{
    file, DataOp, MigrateError, Migration, Operation, SchemaChange, SchemaSnapshot, TableSnapshot,
};

fn empty_snapshot() -> SchemaSnapshot {
    SchemaSnapshot { tables: vec![], m2m_tables: vec![], indexes: vec![], checks: vec![] }
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

fn tmp_path(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("rustango_migrate_file_test_{name}.json"));
    p
}

// ---------- shape ----------

#[test]
fn schema_change_serializes_externally_tagged() {
    let raw = serde_json::to_value(SchemaChange::AddColumn {
        table: "article".into(),
        column: "slug".into(),
    })
    .unwrap();
    assert_eq!(
        raw,
        serde_json::json!({"AddColumn": {"table": "article", "column": "slug"}})
    );
}

#[test]
fn operation_uses_lowercase_tags() {
    let s = Operation::Schema(SchemaChange::DropTable("ghost".into()));
    let raw = serde_json::to_value(&s).unwrap();
    assert_eq!(raw, serde_json::json!({"schema": {"DropTable": "ghost"}}));

    let d = Operation::Data(DataOp {
        sql: "UPDATE x SET y = 1".into(),
        reverse_sql: Some("UPDATE x SET y = 0".into()),
        reversible: true,
    });
    let raw = serde_json::to_value(&d).unwrap();
    assert_eq!(
        raw,
        serde_json::json!({
            "data": {"sql": "UPDATE x SET y = 1", "reverse_sql": "UPDATE x SET y = 0", "reversible": true}
        })
    );
}

// ---------- defaults ----------

#[test]
fn migration_atomic_defaults_to_true_when_omitted() {
    let raw = serde_json::json!({
        "name": "0001_initial",
        "created_at": "2026-04-28T00:00:00Z",
        "snapshot": {"tables": []},
        "forward": []
    });
    let mig: Migration = serde_json::from_value(raw).unwrap();
    assert!(mig.atomic, "atomic must default to true");
    assert!(mig.prev.is_none());
}

#[test]
fn data_op_reversible_defaults_to_true() {
    let raw = serde_json::json!({
        "sql": "DELETE FROM tmp",
        "reverse_sql": "INSERT INTO tmp VALUES (1)"
    });
    let d: DataOp = serde_json::from_value(raw).unwrap();
    assert!(d.reversible);
}

// ---------- round-trip ----------

#[test]
fn round_trip_schema_only_migration() {
    let mig = Migration {
        name: "0001_initial".into(),
        created_at: "2026-04-28T00:00:00Z".into(),
        prev: None,
        atomic: true,
        scope: rustango::migrate::MigrationScope::default(),
        snapshot: SchemaSnapshot {
            tables: vec![user_table()],
                                    ..Default::default()
        },
        forward: vec![Operation::Schema(SchemaChange::CreateTable(
            "snap_user".into(),
        ))],
    };
    let json = serde_json::to_string(&mig).unwrap();
    let back: Migration = serde_json::from_str(&json).unwrap();
    assert_eq!(mig, back);
}

#[test]
fn round_trip_mixed_schema_and_data_ops() {
    let mig = Migration {
        name: "0002_backfill_slugs".into(),
        created_at: "2026-04-28T00:00:00Z".into(),
        prev: Some("0001_initial".into()),
        atomic: true,
        scope: rustango::migrate::MigrationScope::default(),
        snapshot: SchemaSnapshot {
            tables: vec![user_table()],
                                    ..Default::default()
        },
        forward: vec![
            Operation::Schema(SchemaChange::AddColumn {
                table: "article".into(),
                column: "slug".into(),
            }),
            Operation::Data(DataOp {
                sql: "UPDATE article SET slug = LOWER(REPLACE(title, ' ', '-'))".into(),
                reverse_sql: Some("UPDATE article SET slug = NULL".into()),
                reversible: true,
            }),
        ],
    };
    let json = serde_json::to_string(&mig).unwrap();
    let back: Migration = serde_json::from_str(&json).unwrap();
    assert_eq!(mig, back);
}

#[test]
fn round_trip_irreversible_data_op() {
    let mig = Migration {
        name: "0003_purge_pii".into(),
        created_at: "2026-04-28T00:00:00Z".into(),
        prev: Some("0002_backfill_slugs".into()),
        atomic: false,
        scope: rustango::migrate::MigrationScope::default(),
        snapshot: empty_snapshot(),
        forward: vec![Operation::Data(DataOp {
            sql: "DELETE FROM events WHERE created < NOW() - INTERVAL '90 days'".into(),
            reverse_sql: None,
            reversible: false,
        })],
    };
    let json = serde_json::to_string(&mig).unwrap();
    let back: Migration = serde_json::from_str(&json).unwrap();
    assert_eq!(mig, back);
    assert!(!back.atomic);
    if let Operation::Data(d) = &back.forward[0] {
        assert!(!d.reversible);
        assert!(d.reverse_sql.is_none());
    } else {
        panic!("expected Data op");
    }
}

// ---------- file I/O ----------

#[test]
fn write_then_load_round_trip_via_filesystem() {
    let mig = Migration {
        name: "0001_initial".into(),
        created_at: "2026-04-28T00:00:00Z".into(),
        prev: None,
        atomic: true,
        scope: rustango::migrate::MigrationScope::default(),
        snapshot: SchemaSnapshot {
            tables: vec![user_table()],
                                    ..Default::default()
        },
        forward: vec![Operation::Schema(SchemaChange::CreateTable(
            "snap_user".into(),
        ))],
    };
    let path = tmp_path("write_load_round_trip");
    let _ = std::fs::remove_file(&path);
    file::write(&path, &mig).unwrap();
    let back = file::load(&path).unwrap();
    assert_eq!(mig, back);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn load_rejects_inconsistent_reversible_without_reverse_sql() {
    // reversible=true but reverse_sql missing → contradiction.
    let raw = serde_json::json!({
        "name": "0002_bad",
        "created_at": "2026-04-28T00:00:00Z",
        "prev": "0001_initial",
        "snapshot": {"tables": []},
        "forward": [{"data": {"sql": "UPDATE x SET y = 1", "reversible": true}}]
    });
    let path = tmp_path("inconsistent_reversible");
    std::fs::write(&path, serde_json::to_string(&raw).unwrap()).unwrap();
    let err = file::load(&path).unwrap_err();
    let _ = std::fs::remove_file(&path);
    match err {
        MigrateError::Validation(msg) => {
            assert!(msg.contains("reversible=true"), "got: {msg}");
            assert!(msg.contains("reverse_sql"), "got: {msg}");
        }
        other => panic!("expected Validation error, got: {other:?}"),
    }
}

#[test]
fn load_missing_file_is_io_error() {
    let path = tmp_path("definitely_does_not_exist_aaaa");
    let _ = std::fs::remove_file(&path);
    let err = file::load(&path).unwrap_err();
    matches!(err, MigrateError::Io(_));
}

// ---------- ordering ----------

#[test]
fn names_lex_sort_correctly_for_apply_order() {
    let mut names = vec![
        "0010_drop_table_foo".to_string(),
        "0001_initial".to_string(),
        "0003_add_slug".to_string(),
        "0002_backfill".to_string(),
    ];
    names.sort();
    assert_eq!(
        names,
        vec![
            "0001_initial",
            "0002_backfill",
            "0003_add_slug",
            "0010_drop_table_foo",
        ]
    );
}

// ---------- additional edge cases ----------

#[test]
fn migration_with_empty_forward_round_trips() {
    let mig = Migration {
        name: "0001_marker".into(),
        created_at: "2026-04-28T00:00:00Z".into(),
        prev: None,
        atomic: true,
        scope: rustango::migrate::MigrationScope::default(),
        snapshot: empty_snapshot(),
        forward: vec![],
    };
    let json = serde_json::to_string(&mig).unwrap();
    let back: Migration = serde_json::from_str(&json).unwrap();
    assert_eq!(mig, back);
}

#[test]
fn migration_prev_explicit_null_parses_as_none() {
    let raw = serde_json::json!({
        "name": "0001_x",
        "created_at": "2026-04-28T00:00:00Z",
        "prev": null,
        "snapshot": {"tables": []},
        "forward": []
    });
    let mig: Migration = serde_json::from_value(raw).unwrap();
    assert!(mig.prev.is_none());
}

#[test]
fn migration_atomic_explicit_false_parses_as_false() {
    let raw = serde_json::json!({
        "name": "0001_x",
        "created_at": "2026-04-28T00:00:00Z",
        "atomic": false,
        "snapshot": {"tables": []},
        "forward": []
    });
    let mig: Migration = serde_json::from_value(raw).unwrap();
    assert!(!mig.atomic);
}

#[test]
fn data_op_explicit_irreversible_with_no_reverse_sql_is_valid() {
    // Explicit `reversible: false` means the migration is intentionally
    // one-way; no reverse_sql required.
    let raw = serde_json::json!({
        "sql": "DELETE FROM events WHERE created < NOW() - INTERVAL '90 days'",
        "reversible": false
    });
    let d: DataOp = serde_json::from_value(raw).unwrap();
    assert!(!d.reversible);
    assert!(d.reverse_sql.is_none());
}

#[test]
fn unknown_top_level_fields_are_ignored() {
    // Forward-compat: a future field on Migration shouldn't break older readers.
    let raw = serde_json::json!({
        "name": "0001_x",
        "created_at": "2026-04-28T00:00:00Z",
        "snapshot": {"tables": []},
        "forward": [],
        "future_field_that_does_not_exist_yet": {"hello": "world"}
    });
    let mig: Migration = serde_json::from_value(raw).unwrap();
    assert_eq!(mig.name, "0001_x");
}

#[test]
fn list_dir_propagates_malformed_file_as_json_error() {
    let dir = tmp_path("malformed");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("0001_bad.json"), "{not valid json").unwrap();
    let err = file::list_dir(&dir).unwrap_err();
    let _ = std::fs::remove_dir_all(&dir);
    matches!(err, MigrateError::Json(_));
}

#[test]
fn extract_index_handles_overflow_gracefully() {
    // u32::MAX = 4_294_967_295. Anything larger should not panic; it
    // returns None because the parse fails.
    assert_eq!(file::extract_index("99999999999_overflow"), None);
}

#[test]
fn extract_index_handles_leading_zeros() {
    assert_eq!(file::extract_index("00000001_x"), Some(1));
    assert_eq!(file::extract_index("00000000_zero"), Some(0));
}

#[test]
fn list_dir_rejects_broken_prev_chain() {
    // 0002 declares prev=0001 but 0001 isn't in the dir → load-time error.
    let dir = tmp_path("broken_chain");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mig = Migration {
        name: "0002_orphan".into(),
        created_at: "2026-04-28T00:00:00Z".into(),
        prev: Some("0001_initial".into()),
        atomic: true,
        scope: rustango::migrate::MigrationScope::default(),
        snapshot: empty_snapshot(),
        forward: vec![],
    };
    file::write(&dir.join("0002_orphan.json"), &mig).unwrap();

    let err = file::list_dir(&dir).unwrap_err();
    let _ = std::fs::remove_dir_all(&dir);
    match err {
        MigrateError::Validation(msg) => {
            assert!(msg.contains("broken migration chain"), "got: {msg}");
            assert!(msg.contains("0002_orphan"), "got: {msg}");
            assert!(msg.contains("0001_initial"), "got: {msg}");
        }
        other => panic!("expected Validation error, got: {other:?}"),
    }
}

#[test]
fn list_dir_accepts_well_formed_chain() {
    let dir = tmp_path("good_chain");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let m1 = Migration {
        name: "0001_initial".into(),
        created_at: "2026-04-28T00:00:00Z".into(),
        prev: None,
        atomic: true,
        scope: rustango::migrate::MigrationScope::default(),
        snapshot: empty_snapshot(),
        forward: vec![],
    };
    let m2 = Migration {
        name: "0002_next".into(),
        created_at: "2026-04-28T00:00:00Z".into(),
        prev: Some("0001_initial".into()),
        atomic: true,
        scope: rustango::migrate::MigrationScope::default(),
        snapshot: empty_snapshot(),
        forward: vec![],
    };
    file::write(&dir.join("0001_initial.json"), &m1).unwrap();
    file::write(&dir.join("0002_next.json"), &m2).unwrap();

    let migs = file::list_dir(&dir).unwrap();
    assert_eq!(migs.len(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn list_dir_skips_subdirectories() {
    let dir = tmp_path("list_dir_subdirs");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("nested")).unwrap();
    let mig = Migration {
        name: "0001_x".into(),
        created_at: "now".into(),
        prev: None,
        atomic: true,
        scope: rustango::migrate::MigrationScope::default(),
        snapshot: empty_snapshot(),
        forward: vec![],
    };
    file::write(&dir.join("0001_x.json"), &mig).unwrap();

    let migs = file::list_dir(&dir).unwrap();
    assert_eq!(migs.len(), 1, "subdirectories should be ignored");
    let _ = std::fs::remove_dir_all(&dir);
}
