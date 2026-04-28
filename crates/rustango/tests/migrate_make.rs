//! Tests for `make_migrations_from`.
//!
//! Uses `make_migrations_from` (the testable form) so we can supply a
//! controlled `current` snapshot rather than walking the global
//! inventory registry, which other tests in this binary populate.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use rustango::migrate::{
    file, make_migrations_from, Operation, SchemaChange, SchemaSnapshot, TableSnapshot,
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
    SchemaSnapshot { tables }
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
            name: format!("{s}_x"),
            created_at: "now".into(),
            prev: None,
            atomic: true,
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
        name: "0001_real".into(),
        created_at: "now".into(),
        prev: None,
        atomic: true,
        snapshot: snapshot_with(vec![]),
        forward: vec![],
    };
    file::write(&dir.join("0001_real.json"), &mig).unwrap();

    let migs = file::list_dir(&dir).unwrap();
    assert_eq!(migs.len(), 1);
    assert_eq!(migs[0].name, "0001_real");
    let _ = std::fs::remove_dir_all(&dir);
}
