//! Django-parity #347 — `RunPython`-shape data migration callbacks.
//!
//! Verifies:
//!   * a registered callback fires during `migrate`
//!   * the migration JSON references the callback by name
//!   * an UN-registered name surfaces a clear MigrateError::Validation
//!   * `sqlmigrate` preview emits `-- RunPython: <name>` for the op

#![cfg(feature = "sqlite")]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use rustango::migrate::callbacks::MigrationCallbackFut;
use rustango::migrate::{
    file, sqlmigrate_one, CallbackOp, MigrateError, Migration, Operation, SchemaSnapshot,
};
use rustango::register_migration_callback;
use rustango::sql::Pool;

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn fresh_dir(label: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let mut p = std::env::temp_dir();
    p.push(format!("rustango_runpython_{label}_{pid}_{n}"));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

// ---------- Registered callback ----------

static FIRED: AtomicUsize = AtomicUsize::new(0);

fn backfill_locale(_pool: Pool) -> MigrationCallbackFut {
    Box::pin(async {
        FIRED.fetch_add(1, Ordering::SeqCst);
        Ok(())
    })
}

register_migration_callback!("runpython_test_backfill", backfill_locale);

// ---------- Helpers ----------

fn empty_snapshot() -> SchemaSnapshot {
    SchemaSnapshot::default()
}

fn callback_migration(name: &str, callback_name: &str) -> Migration {
    Migration {
        name: name.to_owned(),
        created_at: "2026-05-22T00:00:00Z".into(),
        prev: None,
        atomic: true,
        scope: Default::default(),
        replaces: Vec::new(),
        snapshot: empty_snapshot(),
        forward: vec![Operation::Callback(CallbackOp {
            name: callback_name.to_owned(),
            reverse_name: None,
        })],
    }
}

#[test]
fn sqlmigrate_preview_emits_runpython_comment() {
    let dir = fresh_dir("preview");
    let mig = callback_migration("0001_runpython", "runpython_test_backfill");
    file::write(&dir.join("0001_runpython.json"), &mig).unwrap();
    let preview = sqlmigrate_one(&dir, "0001_runpython").expect("preview");
    let body = preview.statements.join("\n");
    assert!(
        body.contains("-- RunPython: runpython_test_backfill"),
        "preview missing RunPython marker:\n{body}"
    );
    // BEGIN + ledger INSERT + COMMIT still appear.
    assert!(body.contains("BEGIN"), "preview missing BEGIN");
    assert!(body.contains("INSERT INTO"), "preview missing ledger row");
}

#[tokio::test]
async fn migrate_fires_registered_callback() {
    let dir = fresh_dir("apply");
    let mig = callback_migration("0001_runpython_apply", "runpython_test_backfill");
    file::write(&dir.join("0001_runpython_apply.json"), &mig).unwrap();

    let before = FIRED.load(Ordering::SeqCst);

    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    rustango::migrate::migrate_pool(&pool, &dir)
        .await
        .expect("migrate");

    let after = FIRED.load(Ordering::SeqCst);
    assert_eq!(
        after - before,
        1,
        "callback should have fired exactly once (counter {before} → {after})"
    );
}

#[tokio::test]
async fn migrate_with_unknown_callback_surfaces_validation_error() {
    let dir = fresh_dir("unknown");
    let mig = callback_migration("0001_unknown_cb", "this_callback_is_not_registered");
    file::write(&dir.join("0001_unknown_cb.json"), &mig).unwrap();

    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    let err = rustango::migrate::migrate_pool(&pool, &dir)
        .await
        .unwrap_err();
    let msg = format!("{err}");
    assert!(
        matches!(err, MigrateError::Validation(_)),
        "expected MigrateError::Validation, got: {err:?}"
    );
    assert!(
        msg.contains("this_callback_is_not_registered")
            && msg.contains("register_migration_callback"),
        "error should name the callback + point at the registration macro: {msg}"
    );
}
