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
    self, file, manage, MigrateError, Migration, Operation, SchemaChange, SchemaSnapshot,
    TableSnapshot,
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
    assert_eq!(mig.snapshot, SchemaSnapshot { tables: vec![] });

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
