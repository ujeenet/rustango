//! Django-parity #346 — `manage makemigrations --merge` reconciles
//! a divergent migration chain by writing an empty-forward
//! `NNNN_merge.json` file whose `prev` points at the lex-last leaf.
//!
//! The unit tests in `crates/rustango/src/migrate/make.rs::tests` cover
//! `make_merge_migration_from` directly with synthetic snapshots; this
//! file exercises the integration path — drop two divergent migration
//! JSONs into a temp directory, invoke the public
//! `make_merge_migration_from` entry point, and assert the resulting
//! file is readable + parseable by `file::list_dir`.

#![cfg(all(feature = "sqlite", feature = "tenancy"))]

use rustango::migrate::file::{list_dir, Migration};
use rustango::migrate::make::make_merge_migration_from;
use rustango::migrate::snapshot::SchemaSnapshot;
use rustango::migrate::MigrationScope;

fn empty_snap() -> SchemaSnapshot {
    SchemaSnapshot {
        tables: vec![],
        m2m_tables: vec![],
        indexes: vec![],
        checks: vec![],
        excludes: vec![],
    }
}

fn mig_at(name: &str, prev: Option<&str>) -> Migration {
    Migration {
        name: name.into(),
        created_at: "2026-01-01T00:00:00Z".into(),
        prev: prev.map(str::to_owned),
        atomic: true,
        scope: MigrationScope::Tenant,
        snapshot: empty_snap(),
        forward: vec![],
    }
}

fn tempdir() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let mut p = std::env::temp_dir();
    p.push(format!("rustango_merge_live_{pid}_{n}"));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn write(dir: &std::path::Path, mig: &Migration) {
    std::fs::write(
        dir.join(format!("{}.json", mig.name)),
        serde_json::to_string_pretty(mig).unwrap(),
    )
    .unwrap();
}

#[test]
fn merge_round_trips_through_list_dir() {
    let dir = tempdir();
    write(&dir, &mig_at("0001_initial", None));
    write(&dir, &mig_at("0002_branch_a", Some("0001_initial")));
    write(&dir, &mig_at("0002_branch_b", Some("0001_initial")));

    // Sanity — both leaves visible before merge.
    let before = list_dir(&dir).expect("list before merge");
    assert_eq!(before.len(), 3);

    let mig = make_merge_migration_from(&dir, &empty_snap())
        .expect("merge ok")
        .expect("expected a merge file to be written");
    assert_eq!(mig.name, "0003_merge");
    assert!(mig.forward.is_empty());
    assert_eq!(mig.prev.as_deref(), Some("0002_branch_b"));

    // The merge file must be discoverable by the standard loader so
    // every downstream verb (apply / showmigrations / sqlmigrate)
    // sees it just like any other migration.
    let after = list_dir(&dir).expect("list after merge");
    assert_eq!(after.len(), 4);
    let names: Vec<&str> = after.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "0001_initial",
            "0002_branch_a",
            "0002_branch_b",
            "0003_merge"
        ]
    );

    // The merge file fully validates against the existing chain
    // checker — every `prev` link resolves inside the dir.
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn merge_against_single_leaf_writes_nothing() {
    let dir = tempdir();
    write(&dir, &mig_at("0001_initial", None));
    write(&dir, &mig_at("0002_add", Some("0001_initial")));

    let result = make_merge_migration_from(&dir, &empty_snap()).expect("ok");
    assert!(result.is_none(), "linear chain → no merge file");

    // Directory contents unchanged.
    let after = list_dir(&dir).expect("list");
    assert_eq!(after.len(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}
