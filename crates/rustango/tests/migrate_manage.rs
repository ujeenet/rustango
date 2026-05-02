//! Tests for `rustango::migrate::manage::run` — the Django-style
//! `manage.py` analog.
//!
//! Most of `manage::run` is glue over already-tested runner functions,
//! so these tests focus on:
//!   * argv routing (correct subcommand picked, unknown rejected)
//!   * `make_empty` produces a file with the right shape (pure, no DB)
//!   * a smoke test per live subcommand confirming side effects

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use rustango::migrate::{
    self, file, manage, DataOp, MigrateError, Migration, Operation, SchemaChange, SchemaSnapshot,
    TableSnapshot, append_data_op, make_data_migration,
};
use rustango::sql::sqlx::{self, PgPool, Row};

static COUNTER: AtomicU32 = AtomicU32::new(0);

async fn pool() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    Some(
        PgPool::connect(&url)
            .await
            .expect("connect to DATABASE_URL"),
    )
}

fn fresh_dir(label: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let mut p = std::env::temp_dir();
    p.push(format!("rustango_manage_{label}_{pid}_{n}"));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn unique_table(prefix: &str) -> String {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    format!("mg_{prefix}_{pid}_{n}")
}

fn unique_migration(prefix: &str, idx: u32) -> String {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    format!("{idx:04}_{prefix}_{pid}_{n}")
}

fn snapshot_with_table(table_name: &str) -> SchemaSnapshot {
    let table: TableSnapshot = serde_json::from_value(serde_json::json!({
        "name": table_name,
        "model": "T",
        "fields": [
            {"name": "id", "column": "id", "ty": "i64", "nullable": false, "primary_key": true}
        ]
    }))
    .unwrap();
    SchemaSnapshot {
        tables: vec![table],
        ..Default::default()
    }
}

fn write_migration(dir: &std::path::Path, mig: &Migration) {
    if !dir.exists() {
        std::fs::create_dir_all(dir).unwrap();
    }
    let path = dir.join(format!("{}.json", mig.name));
    file::write(&path, mig).unwrap();
}

async fn drop_table(pool: &PgPool, table: &str) {
    let sql = format!(r#"DROP TABLE IF EXISTS "{table}" CASCADE"#);
    sqlx::query(&sql).execute(pool).await.unwrap();
}

async fn delete_ledger_entry(pool: &PgPool, name: &str) {
    sqlx::query("DELETE FROM __rustango_migrations__ WHERE name = $1")
        .bind(name)
        .execute(pool)
        .await
        .unwrap();
}

fn args(cmd: &[&str]) -> Vec<String> {
    cmd.iter().map(|s| (*s).to_string()).collect()
}

// ---------------- pure: make_empty ----------------

#[test]
fn make_empty_writes_scaffold_with_empty_forward() {
    let dir = fresh_dir("empty_scaffold");
    let mig = manage::make_empty(&dir, "backfill_slugs").unwrap();
    assert_eq!(mig.name, "0001_backfill_slugs");
    assert!(mig.prev.is_none());
    assert!(mig.forward.is_empty());
    assert_eq!(mig.snapshot, SchemaSnapshot { tables: vec![], m2m_tables: vec![], indexes: vec![], checks: vec![] });

    // File exists and round-trips.
    let loaded = file::load(&dir.join("0001_backfill_slugs.json")).unwrap();
    assert_eq!(loaded, mig);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn make_empty_picks_next_index_after_existing_migrations() {
    let dir = fresh_dir("next_index");
    let _ = std::fs::create_dir_all(&dir);
    // Seed a 0003.
    write_migration(
        &dir,
        &Migration {
            name: "0003_existing".into(),
            created_at: "2026-04-28T00:00:00Z".into(),
            prev: None,
            atomic: true,
            scope: rustango::migrate::MigrationScope::default(),
            snapshot: snapshot_with_table("t"),
            forward: vec![],
        },
    );

    let mig = manage::make_empty(&dir, "more").unwrap();
    assert_eq!(mig.name, "0004_more");
    assert_eq!(mig.prev.as_deref(), Some("0003_existing"));
    // Snapshot inherits predecessor's so a follow-up `makemigrations`
    // doesn't see a phantom diff.
    assert_eq!(mig.snapshot, snapshot_with_table("t"));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------- pure: make_data_migration ----------------

#[test]
fn make_data_migration_creates_file_with_data_op() {
    let dir = fresh_dir("data_mig");
    let mig = make_data_migration(
        &dir,
        "backfill_slugs",
        "UPDATE posts SET slug = lower(title)",
        Some("UPDATE posts SET slug = NULL"),
    ).unwrap();

    assert_eq!(mig.name, "0001_backfill_slugs");
    assert_eq!(mig.forward.len(), 1);
    match &mig.forward[0] {
        Operation::Data(d) => {
            assert_eq!(d.sql, "UPDATE posts SET slug = lower(title)");
            assert_eq!(d.reverse_sql.as_deref(), Some("UPDATE posts SET slug = NULL"));
            assert!(d.reversible);
        }
        _ => panic!("expected Data op"),
    }

    let loaded = file::load(&dir.join("0001_backfill_slugs.json")).unwrap();
    assert_eq!(loaded, mig);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn make_data_migration_irreversible_when_no_reverse_sql() {
    let dir = fresh_dir("irreversible");
    let mig = make_data_migration(&dir, "seed", "INSERT INTO config VALUES (1)", None).unwrap();
    match &mig.forward[0] {
        Operation::Data(d) => {
            assert!(!d.reversible);
            assert!(d.reverse_sql.is_none());
        }
        _ => panic!("expected Data op"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn make_data_migration_indexes_after_existing_chain() {
    let dir = fresh_dir("data_after_chain");
    write_migration(&dir, &Migration {
        name: "0001_initial".into(),
        created_at: "2026-01-01T00:00:00Z".into(),
        prev: None,
        atomic: true,
        scope: rustango::migrate::MigrationScope::default(),
        snapshot: snapshot_with_table("t"),
        forward: vec![],
    });
    write_migration(&dir, &Migration {
        name: "0002_add_col".into(),
        created_at: "2026-01-01T00:00:00Z".into(),
        prev: Some("0001_initial".into()),
        atomic: true,
        scope: rustango::migrate::MigrationScope::default(),
        snapshot: snapshot_with_table("t"),
        forward: vec![],
    });
    let mig = make_data_migration(&dir, "backfill", "UPDATE t SET x = 1", None).unwrap();
    assert_eq!(mig.name, "0003_backfill");
    assert_eq!(mig.prev.as_deref(), Some("0002_add_col"));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------- pure: append_data_op ----------------

#[test]
fn append_data_op_adds_op_to_existing_migration() {
    let dir = fresh_dir("append");
    // Seed an initial migration with no ops
    write_migration(&dir, &Migration {
        name: "0001_initial".into(),
        created_at: "2026-01-01T00:00:00Z".into(),
        prev: None,
        atomic: true,
        scope: rustango::migrate::MigrationScope::default(),
        snapshot: snapshot_with_table("t"),
        forward: vec![],
    });

    append_data_op(
        &dir,
        "0001_initial",
        "UPDATE t SET x = 1",
        Some("UPDATE t SET x = 0"),
    ).unwrap();

    let loaded = file::load(&dir.join("0001_initial.json")).unwrap();
    assert_eq!(loaded.forward.len(), 1);
    match &loaded.forward[0] {
        Operation::Data(d) => {
            assert_eq!(d.sql, "UPDATE t SET x = 1");
            assert!(d.reversible);
        }
        _ => panic!("expected Data op"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn append_data_op_error_on_missing_migration() {
    let dir = fresh_dir("append_missing");
    let _ = std::fs::create_dir_all(&dir);
    let err = append_data_op(&dir, "0001_nonexistent", "SELECT 1", None).unwrap_err();
    assert!(matches!(err, MigrateError::Validation(_)));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------- argv: add-data-op subcommand ----------------

#[tokio::test]
async fn add_data_op_cmd_creates_new_migration() {
    let dir = fresh_dir("cmd_create");
    let mut out = Vec::<u8>::new();
    // add-data-op is pure file I/O — no DB needed. Pass a lazy pool.
    let pool = rustango::sql::sqlx::PgPool::connect_lazy("postgres://localhost/dummy").unwrap();
    manage::run_with_writer(
        &pool,
        &dir,
        args(&[
            "add-data-op",
            "--sql", "UPDATE t SET x = 1",
            "--reverse-sql", "UPDATE t SET x = 0",
            "--name", "backfill_x",
        ]),
        &mut out,
    ).await.unwrap();

    let output = String::from_utf8(out).unwrap();
    assert!(output.contains("backfill_x"), "output: {output}");
    assert!(output.contains("reversible"), "output: {output}");

    let files: Vec<_> = std::fs::read_dir(&dir).unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(files.len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn add_data_op_cmd_missing_sql_is_error() {
    let dir = fresh_dir("cmd_no_sql");
    let _ = std::fs::create_dir_all(&dir);
    let mut out = Vec::<u8>::new();
    let pool = rustango::sql::sqlx::PgPool::connect_lazy("postgres://localhost/dummy").unwrap();
    let err = manage::run_with_writer(
        &pool,
        &dir,
        args(&["add-data-op", "--name", "oops"]),
        &mut out,
    ).await.unwrap_err();
    assert!(matches!(err, MigrateError::Validation(_)));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------- argv routing ----------------

#[tokio::test]
async fn run_no_args_prints_help_returns_ok() {
    let Some(pool) = pool().await else {
        return;
    };
    let dir = fresh_dir("noargs");
    manage::run(&pool, &dir, args(&[])).await.unwrap();
}

#[tokio::test]
async fn run_help_subcommand_returns_ok() {
    let Some(pool) = pool().await else {
        return;
    };
    let dir = fresh_dir("help");
    manage::run(&pool, &dir, args(&["--help"])).await.unwrap();
    manage::run(&pool, &dir, args(&["-h"])).await.unwrap();
    manage::run(&pool, &dir, args(&["help"])).await.unwrap();
}

#[tokio::test]
async fn run_unknown_subcommand_is_validation_error() {
    let Some(pool) = pool().await else {
        return;
    };
    let dir = fresh_dir("unknown");
    let err = manage::run(&pool, &dir, args(&["frobnicate"]))
        .await
        .unwrap_err();
    matches!(err, MigrateError::Validation(_));
    assert!(format!("{err}").contains("frobnicate"));
}

#[tokio::test]
async fn makemigrations_unknown_flag_is_validation_error() {
    let Some(pool) = pool().await else {
        return;
    };
    let dir = fresh_dir("bad_flag");
    let err = manage::run(&pool, &dir, args(&["makemigrations", "--bogus"]))
        .await
        .unwrap_err();
    assert!(format!("{err}").contains("--bogus"));
}

#[tokio::test]
async fn makemigrations_empty_without_name_is_error() {
    let Some(pool) = pool().await else {
        return;
    };
    let dir = fresh_dir("empty_noname");
    let err = manage::run(&pool, &dir, args(&["makemigrations", "--empty"]))
        .await
        .unwrap_err();
    assert!(format!("{err}").to_lowercase().contains("name"));
}

#[tokio::test]
async fn downgrade_with_garbage_step_count_is_error() {
    let Some(pool) = pool().await else {
        return;
    };
    let dir = fresh_dir("downgrade_garbage");
    let err = manage::run(&pool, &dir, args(&["downgrade", "five"]))
        .await
        .unwrap_err();
    assert!(format!("{err}").contains("five"));
}

// ---------------- live smoke tests ----------------

#[tokio::test]
async fn migrate_subcommand_applies_pending() {
    let Some(pool) = pool().await else {
        return;
    };
    let table = unique_table("mig_subcmd");
    let mig_name = unique_migration("mig_subcmd", 1);
    let dir = fresh_dir("migrate_subcmd");

    write_migration(
        &dir,
        &Migration {
            name: mig_name.clone(),
            created_at: "2026-04-28T00:00:00Z".into(),
            prev: None,
            atomic: true,
            scope: rustango::migrate::MigrationScope::default(),
            snapshot: snapshot_with_table(&table),
            forward: vec![Operation::Schema(SchemaChange::CreateTable(table.clone()))],
        },
    );

    drop_table(&pool, &table).await;
    delete_ledger_entry(&pool, &mig_name).await;

    manage::run(&pool, &dir, args(&["migrate"])).await.unwrap();

    let exists: bool = sqlx::query(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = $1)",
    )
    .bind(&table)
    .fetch_one(&pool)
    .await
    .unwrap()
    .try_get(0)
    .unwrap();
    assert!(exists);

    drop_table(&pool, &table).await;
    delete_ledger_entry(&pool, &mig_name).await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn migrate_to_target_subcommand_routes_correctly() {
    let Some(pool) = pool().await else {
        return;
    };
    let suffix = COUNTER.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let names = [
        format!("0001_mt_{pid}_{suffix}_a"),
        format!("0002_mt_{pid}_{suffix}_b"),
    ];
    let tables = [
        format!("mg_mt_a_{pid}_{suffix}"),
        format!("mg_mt_b_{pid}_{suffix}"),
    ];
    let dir = fresh_dir("migrate_target");

    for (i, name) in names.iter().enumerate() {
        let prev = if i == 0 {
            None
        } else {
            Some(names[i - 1].clone())
        };
        write_migration(
            &dir,
            &Migration {
                name: name.clone(),
                created_at: "2026-04-28T00:00:00Z".into(),
                prev,
                atomic: true,
                scope: rustango::migrate::MigrationScope::default(),
                snapshot: snapshot_with_table(&tables[i]),
                forward: vec![Operation::Schema(SchemaChange::CreateTable(
                    tables[i].clone(),
                ))],
            },
        );
    }
    for n in &names {
        delete_ledger_entry(&pool, n).await;
    }
    for t in &tables {
        drop_table(&pool, t).await;
    }

    // `migrate <target>` should walk only to 0001.
    manage::run(&pool, &dir, args(&["migrate", &names[0]]))
        .await
        .unwrap();

    let applied = migrate::applied_set(&pool).await.unwrap();
    assert!(applied.contains(&names[0]));
    assert!(!applied.contains(&names[1]), "0002 should NOT be applied");

    for n in &names {
        delete_ledger_entry(&pool, n).await;
    }
    for t in &tables {
        drop_table(&pool, t).await;
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn downgrade_subcommand_steps_back_one_by_default() {
    let Some(pool) = pool().await else {
        return;
    };
    let suffix = COUNTER.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let names = [
        format!("0001_dg_{pid}_{suffix}_a"),
        format!("0002_dg_{pid}_{suffix}_b"),
    ];
    let tables = [
        format!("mg_dg_a_{pid}_{suffix}"),
        format!("mg_dg_b_{pid}_{suffix}"),
    ];
    let dir = fresh_dir("downgrade_subcmd");

    for (i, name) in names.iter().enumerate() {
        let prev = if i == 0 {
            None
        } else {
            Some(names[i - 1].clone())
        };
        write_migration(
            &dir,
            &Migration {
                name: name.clone(),
                created_at: "2026-04-28T00:00:00Z".into(),
                prev,
                atomic: true,
                scope: rustango::migrate::MigrationScope::default(),
                snapshot: snapshot_with_table(&tables[i]),
                forward: vec![Operation::Schema(SchemaChange::CreateTable(
                    tables[i].clone(),
                ))],
            },
        );
    }
    for n in &names {
        delete_ledger_entry(&pool, n).await;
    }
    for t in &tables {
        drop_table(&pool, t).await;
    }

    manage::run(&pool, &dir, args(&["migrate"])).await.unwrap();
    // Default `downgrade` (no arg) → 1 step.
    manage::run(&pool, &dir, args(&["downgrade"]))
        .await
        .unwrap();

    let applied = migrate::applied_set(&pool).await.unwrap();
    assert!(applied.contains(&names[0]));
    assert!(!applied.contains(&names[1]), "head should be rolled back");

    for n in &names {
        delete_ledger_entry(&pool, n).await;
    }
    for t in &tables {
        drop_table(&pool, t).await;
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn showmigrations_subcommand_runs_on_empty_dir() {
    let Some(pool) = pool().await else {
        return;
    };
    let dir = fresh_dir("show_empty");
    let _ = std::fs::create_dir_all(&dir);
    manage::run(&pool, &dir, args(&["showmigrations"]))
        .await
        .unwrap();
    // `status` is the same subcommand.
    manage::run(&pool, &dir, args(&["status"])).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------- v0.3.1: capturable output via run_with_writer ----------------

#[tokio::test]
async fn run_with_writer_captures_help_text() {
    let Some(pool) = pool().await else {
        return;
    };
    let dir = fresh_dir("capture_help");
    let mut buf: Vec<u8> = Vec::new();
    manage::run_with_writer(&pool, &dir, args(&["--help"]), &mut buf)
        .await
        .unwrap();
    let out = String::from_utf8(buf).unwrap();
    // Sanity-check the writer received something help-shaped — no
    // stdout bypass.
    assert!(out.contains("rustango::manage"), "got: {out}");
    assert!(out.contains("makemigrations"), "got: {out}");
    assert!(out.contains("downgrade"), "got: {out}");
}

#[tokio::test]
async fn migrate_dry_run_subcommand_prints_sql_no_writes() {
    let Some(pool) = pool().await else {
        return;
    };
    let table = unique_table("dry_subcmd");
    let mig_name = unique_migration("dry_subcmd", 1);
    let dir = fresh_dir("dry_subcmd");

    write_migration(
        &dir,
        &Migration {
            name: mig_name.clone(),
            created_at: "2026-04-28T00:00:00Z".into(),
            prev: None,
            atomic: true,
            scope: rustango::migrate::MigrationScope::default(),
            snapshot: snapshot_with_table(&table),
            forward: vec![Operation::Schema(SchemaChange::CreateTable(table.clone()))],
        },
    );

    drop_table(&pool, &table).await;
    delete_ledger_entry(&pool, &mig_name).await;

    let mut buf: Vec<u8> = Vec::new();
    manage::run_with_writer(&pool, &dir, args(&["migrate", "--dry-run"]), &mut buf)
        .await
        .unwrap();
    let out = String::from_utf8(buf).unwrap();

    assert!(
        out.contains("DRY RUN"),
        "expected DRY RUN banner, got: {out}"
    );
    assert!(out.contains(&mig_name), "expected migration name, got: {out}");
    assert!(out.contains("CREATE TABLE"), "expected DDL, got: {out}");
    assert!(out.contains("BEGIN"), "atomic migration should show BEGIN, got: {out}");

    // No side effects.
    let exists: bool = sqlx::query(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = $1)",
    )
    .bind(&table)
    .fetch_one(&pool)
    .await
    .unwrap()
    .try_get(0)
    .unwrap();
    assert!(!exists, "dry-run subcommand must not create the table");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn migrate_dry_run_subcommand_with_target_is_rejected() {
    let Some(pool) = pool().await else {
        return;
    };
    let dir = fresh_dir("dry_run_target");
    let _ = std::fs::create_dir_all(&dir);

    let err = manage::run(&pool, &dir, args(&["migrate", "0001_x", "--dry-run"]))
        .await
        .unwrap_err();
    assert!(format!("{err}").contains("dry-run"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn run_with_writer_captures_migrate_output() {
    let Some(pool) = pool().await else {
        return;
    };
    let table = unique_table("capture_mig");
    let mig_name = unique_migration("capture_mig", 1);
    let dir = fresh_dir("capture_migrate");

    write_migration(
        &dir,
        &Migration {
            name: mig_name.clone(),
            created_at: "2026-04-28T00:00:00Z".into(),
            prev: None,
            atomic: true,
            scope: rustango::migrate::MigrationScope::default(),
            snapshot: snapshot_with_table(&table),
            forward: vec![Operation::Schema(SchemaChange::CreateTable(table.clone()))],
        },
    );

    drop_table(&pool, &table).await;
    delete_ledger_entry(&pool, &mig_name).await;

    let mut buf: Vec<u8> = Vec::new();
    manage::run_with_writer(&pool, &dir, args(&["migrate"]), &mut buf)
        .await
        .unwrap();
    let out = String::from_utf8(buf).unwrap();
    assert!(out.contains("applied"), "got: {out}");
    assert!(out.contains(&mig_name), "got: {out}");

    drop_table(&pool, &table).await;
    delete_ledger_entry(&pool, &mig_name).await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn makemigrations_empty_via_run_writes_scaffold() {
    let Some(pool) = pool().await else {
        return;
    };
    let dir = fresh_dir("empty_via_run");

    manage::run(
        &pool,
        &dir,
        args(&["makemigrations", "--empty", "backfill"]),
    )
    .await
    .unwrap();

    let path = dir.join("0001_backfill.json");
    assert!(path.exists(), "expected file at {}", path.display());
    let mig = file::load(&path).unwrap();
    assert!(mig.forward.is_empty(), "scaffold has empty forward");
    let _ = std::fs::remove_dir_all(&dir);
}
