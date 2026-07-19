//! Tests for `make_migrations_from`.
//!
//! Uses `make_migrations_from` (the testable form) so we can supply a
//! controlled `current` snapshot rather than walking the global
//! inventory registry, which other tests in this binary populate.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use rustango::migrate::{
    file, make_migrations_from, MigrateError, Operation, SchemaChange, SchemaSnapshot,
    TableSnapshot,
};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn fresh_dir(label: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let mut p = std::env::temp_dir();
    p.push(format!("rustango_make_{label}_{pid}_{n}"));
    let _ = std::fs::remove_dir_all(&p);
    p
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

fn post_table() -> TableSnapshot {
    serde_json::from_value(serde_json::json!({
        "name": "snap_post",
        "model": "SnapPost",
        "fields": [
            {"name": "id", "column": "id", "ty": "i64", "nullable": false, "primary_key": true},
            {"name": "title", "column": "title", "ty": "string", "nullable": false, "primary_key": false, "max_length": 200}
        ]
    })).unwrap()
}

fn snapshot_with(tables: Vec<TableSnapshot>) -> SchemaSnapshot {
    SchemaSnapshot {
        tables,
        ..Default::default()
    }
}

// ---------- empty dir → 0001_initial ----------

#[test]
fn empty_dir_first_run_writes_0001_initial() {
    let dir = fresh_dir("0001_initial");
    let current = snapshot_with(vec![user_table(), post_table()]);
    let mig = make_migrations_from(&dir, &current, None).unwrap().unwrap();
    assert_eq!(mig.name, "0001_initial");
    assert!(mig.prev.is_none());
    assert_eq!(mig.forward.len(), 2);
    for op in &mig.forward {
        assert!(matches!(
            op,
            Operation::Schema(SchemaChange::CreateTable(_))
        ));
    }

    // File on disk matches.
    let path = dir.join("0001_initial.json");
    assert!(path.exists(), "expected file at {}", path.display());
    let loaded = file::load(&path).unwrap();
    assert_eq!(loaded.name, "0001_initial");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn empty_dir_idempotent_when_no_models() {
    let dir = fresh_dir("idempotent_empty");
    let current = snapshot_with(vec![]);
    let result = make_migrations_from(&dir, &current, None).unwrap();
    assert!(result.is_none(), "no models + no prior → no migration");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- second run, no diff → None ----------

#[test]
fn second_run_with_no_changes_returns_none() {
    let dir = fresh_dir("no_changes");
    let current = snapshot_with(vec![user_table()]);
    let _first = make_migrations_from(&dir, &current, None).unwrap().unwrap();
    let second = make_migrations_from(&dir, &current, None).unwrap();
    assert!(second.is_none(), "no diff → no second migration");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- auto-naming after the first migration ----------

#[test]
fn second_run_with_added_table_picks_create_name() {
    let dir = fresh_dir("create_name");
    let initial = snapshot_with(vec![user_table()]);
    make_migrations_from(&dir, &initial, None).unwrap().unwrap();

    let next = snapshot_with(vec![user_table(), post_table()]);
    let mig = make_migrations_from(&dir, &next, None).unwrap().unwrap();
    assert_eq!(mig.name, "0002_create_snap_post");
    assert_eq!(mig.prev.as_deref(), Some("0001_initial"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn second_run_with_added_column_picks_add_name() {
    let dir = fresh_dir("add_column");
    let initial = snapshot_with(vec![user_table()]);
    make_migrations_from(&dir, &initial, None).unwrap().unwrap();

    let mut t = user_table();
    t.fields.push(
        serde_json::from_value(serde_json::json!({
            "name": "bio", "column": "bio", "ty": "string", "nullable": true, "primary_key": false
        }))
        .unwrap(),
    );
    t.fields.sort_by(|a, b| a.column.cmp(&b.column));
    let next = snapshot_with(vec![t]);

    let mig = make_migrations_from(&dir, &next, None).unwrap().unwrap();
    assert_eq!(mig.name, "0002_add_bio_to_snap_user");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn dropped_table_picks_drop_name() {
    let dir = fresh_dir("drop_table");
    let initial = snapshot_with(vec![user_table(), post_table()]);
    make_migrations_from(&dir, &initial, None).unwrap().unwrap();

    let next = snapshot_with(vec![user_table()]);
    let mig = make_migrations_from(&dir, &next, None).unwrap().unwrap();
    assert_eq!(mig.name, "0002_drop_snap_post");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mixed_changes_fall_back_to_auto() {
    let dir = fresh_dir("mixed");
    let initial = snapshot_with(vec![user_table()]);
    make_migrations_from(&dir, &initial, None).unwrap().unwrap();

    // Add post table AND drop a column from user.
    let mut user = user_table();
    user.fields.retain(|f| f.column != "name"); // drop "name"
    let next = snapshot_with(vec![user, post_table()]);

    let mig = make_migrations_from(&dir, &next, None).unwrap().unwrap();
    assert_eq!(mig.name, "0002_auto");
    assert!(mig.forward.len() >= 2);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- name override ----------

#[test]
fn name_override_replaces_suffix_keeps_index() {
    let dir = fresh_dir("name_override");
    let initial = snapshot_with(vec![user_table()]);
    make_migrations_from(&dir, &initial, Some("genesis"))
        .unwrap()
        .unwrap();

    let next = snapshot_with(vec![user_table(), post_table()]);
    let mig = make_migrations_from(&dir, &next, Some("introduce_posts"))
        .unwrap()
        .unwrap();
    assert_eq!(mig.name, "0002_introduce_posts");
    assert!(dir.join("0001_genesis.json").exists());
    assert!(dir.join("0002_introduce_posts.json").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- index continues past 9 ----------

#[test]
fn extract_index_handles_zero_padded_names() {
    assert_eq!(file::extract_index("0001_initial"), Some(1));
    assert_eq!(file::extract_index("0042_xyz"), Some(42));
    assert_eq!(file::extract_index("9999_late"), Some(9999));
    assert_eq!(file::extract_index("noindex_here"), None);
    assert_eq!(file::extract_index(""), None);
}

#[test]
fn next_index_is_one_more_than_max_existing() {
    let dir = fresh_dir("indices");
    let initial = snapshot_with(vec![user_table()]);
    make_migrations_from(&dir, &initial, None).unwrap().unwrap();

    let with_post = snapshot_with(vec![user_table(), post_table()]);
    let m2 = make_migrations_from(&dir, &with_post, None)
        .unwrap()
        .unwrap();
    assert!(m2.name.starts_with("0002_"));

    // Drop posts → 0003.
    let m3 = make_migrations_from(&dir, &snapshot_with(vec![user_table()]), None)
        .unwrap()
        .unwrap();
    assert!(m3.name.starts_with("0003_"));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- list_dir ----------

#[test]
fn list_dir_returns_empty_for_missing_directory() {
    let dir = fresh_dir("missing");
    // Directory does not exist.
    let migs = file::list_dir(&dir).unwrap();
    assert!(migs.is_empty());
}

#[test]
fn list_dir_sorts_lexicographically() {
    let dir = fresh_dir("sort");
    // Drop in three migrations with non-sorted file system order.
    let _ = std::fs::create_dir_all(&dir);
    for s in ["0010", "0001", "0003"] {
        let snap = snapshot_with(vec![user_table()]);
        let path = dir.join(format!("{s}_x.json"));
        let mig = rustango::migrate::Migration {
            replaces: Vec::new(),
            name: format!("{s}_x"),
            created_at: "now".into(),
            prev: None,
            atomic: true,
            scope: rustango::migrate::MigrationScope::default(),
            snapshot: snap,
            forward: vec![],
        };
        file::write(&path, &mig).unwrap();
    }

    let migs = file::list_dir(&dir).unwrap();
    let names: Vec<&str> = migs.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, vec!["0001_x", "0003_x", "0010_x"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn list_dir_ignores_non_json_files() {
    let dir = fresh_dir("non_json");
    let _ = std::fs::create_dir_all(&dir);
    std::fs::write(dir.join("README.md"), "not a migration").unwrap();
    let mig = rustango::migrate::Migration {
        replaces: Vec::new(),
        name: "0001_real".into(),
        created_at: "now".into(),
        prev: None,
        atomic: true,
        scope: rustango::migrate::MigrationScope::default(),
        snapshot: snapshot_with(vec![]),
        forward: vec![],
    };
    file::write(&dir.join("0001_real.json"), &mig).unwrap();

    let migs = file::list_dir(&dir).unwrap();
    assert_eq!(migs.len(), 1);
    assert_eq!(migs[0].name, "0001_real");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- additional heuristic edge cases ----------

#[test]
fn name_override_beats_heuristic_even_for_initial() {
    let dir = fresh_dir("override_initial");
    let initial = snapshot_with(vec![user_table(), post_table()]);
    let mig = make_migrations_from(&dir, &initial, Some("startup"))
        .unwrap()
        .unwrap();
    assert_eq!(mig.name, "0001_startup");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn multiple_create_tables_after_initial_names_after_tables() {
    // v0.31.1 — the auto-name heuristic was upgraded so a diff that
    // is exclusively `CreateTable`s (plus their indexes) produces a
    // descriptive name like `create_<a>_and_<b>` instead of the
    // unhelpful `auto`. Pre-v0.31.1 this test asserted "0002_auto";
    // the assertion was updated alongside the heuristic and the test
    // renamed to reflect the new contract.
    let dir = fresh_dir("multi_create");
    let initial = snapshot_with(vec![user_table()]);
    make_migrations_from(&dir, &initial, None).unwrap().unwrap();

    // Add post AND a third table at once.
    let extra: rustango::migrate::TableSnapshot = serde_json::from_value(serde_json::json!({
        "name": "extra", "model": "Extra",
        "fields": [
            {"name": "id", "column": "id", "ty": "i64", "nullable": false, "primary_key": true}
        ]
    }))
    .unwrap();

    let next = snapshot_with(vec![user_table(), post_table(), extra]);
    let mig = make_migrations_from(&dir, &next, None).unwrap().unwrap();
    // Tables are sorted before being joined, so "extra" + "snap_post"
    // (the `post_table()` helper uses table name `snap_post`) come out
    // alphabetically.
    assert_eq!(mig.name, "0002_create_extra_and_snap_post");
    assert_eq!(mig.forward.len(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn multiple_add_columns_falls_back_to_auto() {
    let dir = fresh_dir("multi_addcol");
    let initial = snapshot_with(vec![user_table()]);
    make_migrations_from(&dir, &initial, None).unwrap().unwrap();

    let mut user = user_table();
    for col in ["bio", "score"] {
        user.fields.push(
            serde_json::from_value(serde_json::json!({
                "name": col, "column": col, "ty": "string",
                "nullable": true, "primary_key": false
            }))
            .unwrap(),
        );
    }
    user.fields.sort_by(|a, b| a.column.cmp(&b.column));

    let next = snapshot_with(vec![user]);
    let mig = make_migrations_from(&dir, &next, None).unwrap().unwrap();
    assert_eq!(mig.name, "0002_auto");
    assert_eq!(mig.forward.len(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn create_plus_drop_table_falls_back_to_auto() {
    let dir = fresh_dir("create_and_drop");
    let initial = snapshot_with(vec![user_table(), post_table()]);
    make_migrations_from(&dir, &initial, None).unwrap().unwrap();

    let comment: rustango::migrate::TableSnapshot = serde_json::from_value(serde_json::json!({
        "name": "comment", "model": "Comment",
        "fields": [
            {"name": "id", "column": "id", "ty": "i64", "nullable": false, "primary_key": true}
        ]
    }))
    .unwrap();
    // post is dropped, comment is created.
    let next = snapshot_with(vec![user_table(), comment]);
    let mig = make_migrations_from(&dir, &next, None).unwrap().unwrap();
    assert_eq!(mig.name, "0002_auto");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn generated_migration_has_rfc3339_timestamp() {
    let dir = fresh_dir("rfc3339");
    let mig = make_migrations_from(&dir, &snapshot_with(vec![user_table()]), None)
        .unwrap()
        .unwrap();
    // Loose check: contains "T" and either "Z" or "+" (offset marker).
    let ts = &mig.created_at;
    assert!(ts.contains('T'), "expected 'T' in {ts}");
    assert!(
        ts.contains('Z') || ts.contains('+') || ts.contains('-'),
        "expected timezone marker in {ts}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn generated_migration_forward_is_only_schema_ops() {
    let dir = fresh_dir("only_schema_ops");
    let mig = make_migrations_from(&dir, &snapshot_with(vec![user_table()]), None)
        .unwrap()
        .unwrap();
    for op in &mig.forward {
        assert!(
            matches!(op, rustango::migrate::Operation::Schema(_)),
            "make_migrations should never emit data ops automatically"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn single_drop_column_picks_drop_from_name() {
    let dir = fresh_dir("drop_col_name");
    // user has id + name initially.
    let initial = snapshot_with(vec![user_table()]);
    make_migrations_from(&dir, &initial, None).unwrap().unwrap();

    // Drop `name` from user.
    let mut user = user_table();
    user.fields.retain(|f| f.column != "name");
    let next = snapshot_with(vec![user]);

    let mig = make_migrations_from(&dir, &next, None).unwrap().unwrap();
    assert_eq!(mig.name, "0002_drop_name_from_snap_user");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn prev_field_is_predecessor_name() {
    let dir = fresh_dir("prev_links");
    let initial = snapshot_with(vec![user_table()]);
    let m1 = make_migrations_from(&dir, &initial, None).unwrap().unwrap();
    assert!(m1.prev.is_none());

    let next = snapshot_with(vec![user_table(), post_table()]);
    let m2 = make_migrations_from(&dir, &next, None).unwrap().unwrap();
    assert_eq!(m2.prev.as_deref(), Some(m1.name.as_str()));

    // Drop post → 0003. Should chain to 0002.
    let m3 = make_migrations_from(&dir, &snapshot_with(vec![user_table()]), None)
        .unwrap()
        .unwrap();
    assert_eq!(m3.prev.as_deref(), Some(m2.name.as_str()));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- v0.4: AlterField autogeneration (was v0.3.1 hard error) ----------

fn user_table_with_age_i32() -> TableSnapshot {
    serde_json::from_value(serde_json::json!({
        "name": "snap_user",
        "model": "SnapUser",
        "fields": [
            {"name": "id", "column": "id", "ty": "i64", "nullable": false, "primary_key": true},
            {"name": "age", "column": "age", "ty": "i32", "nullable": false, "primary_key": false}
        ]
    }))
    .unwrap()
}

fn user_table_with_age_i64() -> TableSnapshot {
    serde_json::from_value(serde_json::json!({
        "name": "snap_user",
        "model": "SnapUser",
        "fields": [
            {"name": "id", "column": "id", "ty": "i64", "nullable": false, "primary_key": true},
            {"name": "age", "column": "age", "ty": "i64", "nullable": false, "primary_key": false}
        ]
    }))
    .unwrap()
}

#[test]
fn type_change_emits_alter_column_type_op() {
    // v0.4 Slice 3: i32 → i64 used to be the v0.3.1 hard error;
    // now `make_migrations_from` produces a concrete `AlterColumnType` op.
    let dir = fresh_dir("type_change");
    let prev = snapshot_with(vec![user_table_with_age_i32()]);
    make_migrations_from(&dir, &prev, None).unwrap().unwrap();

    let next = snapshot_with(vec![user_table_with_age_i64()]);
    let mig = make_migrations_from(&dir, &next, None).unwrap().unwrap();
    assert_eq!(mig.forward.len(), 1);
    match &mig.forward[0] {
        Operation::Schema(SchemaChange::AlterColumnType {
            table,
            column,
            from,
            to,
        }) => {
            assert_eq!(table, "snap_user");
            assert_eq!(column, "age");
            assert_eq!(from, "i32");
            assert_eq!(to, "i64");
        }
        other => panic!("expected AlterColumnType, got {other:?}"),
    }
    assert_eq!(mig.name, "0002_alter_age_on_snap_user_i32_to_i64");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn nullability_flip_emits_alter_column_nullable_op() {
    let dir = fresh_dir("null_flip");
    let prev = snapshot_with(vec![user_table()]);
    make_migrations_from(&dir, &prev, None).unwrap().unwrap();

    let mut next_t = user_table();
    next_t
        .fields
        .iter_mut()
        .find(|f| f.column == "name")
        .unwrap()
        .nullable = true;
    let mig = make_migrations_from(&dir, &snapshot_with(vec![next_t]), None)
        .unwrap()
        .unwrap();
    assert_eq!(mig.forward.len(), 1);
    match &mig.forward[0] {
        Operation::Schema(SchemaChange::AlterColumnNullable {
            table,
            column,
            nullable,
        }) => {
            assert_eq!(table, "snap_user");
            assert_eq!(column, "name");
            assert!(*nullable);
        }
        other => panic!("expected AlterColumnNullable, got {other:?}"),
    }
    assert_eq!(mig.name, "0002_make_name_on_snap_user_nullable");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn max_length_change_emits_alter_column_max_length_op() {
    let dir = fresh_dir("max_length");
    let prev = snapshot_with(vec![user_table()]);
    make_migrations_from(&dir, &prev, None).unwrap().unwrap();

    let mut next_t = user_table();
    next_t
        .fields
        .iter_mut()
        .find(|f| f.column == "name")
        .unwrap()
        .max_length = Some(64);
    let mig = make_migrations_from(&dir, &snapshot_with(vec![next_t]), None)
        .unwrap()
        .unwrap();
    assert_eq!(mig.forward.len(), 1);
    match &mig.forward[0] {
        Operation::Schema(SchemaChange::AlterColumnMaxLength {
            table,
            column,
            from,
            to,
        }) => {
            assert_eq!(table, "snap_user");
            assert_eq!(column, "name");
            assert_eq!(*from, Some(32));
            assert_eq!(*to, Some(64));
        }
        other => panic!("expected AlterColumnMaxLength, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn alter_combined_with_create_table_in_one_migration() {
    // Mixed-shape diff: type alter on existing table + new table.
    // Both end up in one migration; the v0.3.1 hard-error workaround
    // is gone now that AlterColumn is a real op.
    let dir = fresh_dir("mixed_meta_and_create");
    let prev = snapshot_with(vec![user_table_with_age_i32()]);
    make_migrations_from(&dir, &prev, None).unwrap().unwrap();

    let next = snapshot_with(vec![user_table_with_age_i64(), post_table()]);
    let mig = make_migrations_from(&dir, &next, None).unwrap().unwrap();
    assert_eq!(mig.forward.len(), 2);
    let kinds: Vec<&'static str> = mig
        .forward
        .iter()
        .map(|op| match op {
            Operation::Schema(SchemaChange::CreateTable(_)) => "CreateTable",
            Operation::Schema(SchemaChange::AlterColumnType { .. }) => "AlterColumnType",
            _ => "other",
        })
        .collect();
    // CreateTable comes before AlterColumn* by detect_changes' order.
    assert_eq!(kinds, vec!["CreateTable", "AlterColumnType"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn primary_key_change_still_hits_the_hard_error() {
    // PK alters need a dedicated slice; they still surface as the
    // detect_unsupported_field_changes hard error. Same for min/max
    // (CHECK), FK, and Auto add/remove.
    let dir = fresh_dir("pk_change");
    let mut t = user_table();
    t.fields
        .iter_mut()
        .find(|f| f.column == "id")
        .unwrap()
        .primary_key = true;
    make_migrations_from(&dir, &snapshot_with(vec![t.clone()]), None)
        .unwrap()
        .unwrap();

    // Flip primary_key off — change isn't supported by AlterColumn
    // ops, so we expect the hard error.
    let mut t2 = t.clone();
    t2.fields
        .iter_mut()
        .find(|f| f.column == "id")
        .unwrap()
        .primary_key = false;
    let err = make_migrations_from(&dir, &snapshot_with(vec![t2]), None).unwrap_err();
    let msg = match err {
        MigrateError::Validation(m) => m,
        other => panic!("expected Validation, got {other:?}"),
    };
    assert!(msg.contains("primary_key changed"), "{msg}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn snapshot_in_generated_migration_matches_input() {
    let dir = fresh_dir("snapshot_match");
    let want = snapshot_with(vec![user_table(), post_table()]);
    let mig = make_migrations_from(&dir, &want, None).unwrap().unwrap();
    assert_eq!(mig.snapshot, want);
    let _ = std::fs::remove_dir_all(&dir);
}
