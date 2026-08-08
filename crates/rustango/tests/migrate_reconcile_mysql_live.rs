#![cfg(all(feature = "mysql", feature = "tenancy"))]
//! Squash reconciliation on live MySQL (#1167).
//!
//! Reads `MYSQL_TEST_URL`; skips silently when unset. MySQL is the strict
//! dialect here — a re-run `CREATE TABLE` fails with error 1050 — so this is
//! where a broken reconcile actually bites.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use rustango::migrate::{
    self, file, Migration, Operation, SchemaChange, SchemaSnapshot, TableSnapshot,
};
use rustango::sql::sqlx::{self, Row};
use rustango::sql::Pool;

static COUNTER: AtomicU32 = AtomicU32::new(0);
const LEDGER: &str = "__rustango_migrations__";

fn table_snap(table: &str) -> TableSnapshot {
    serde_json::from_value(serde_json::json!({
        "name": table,
        "model": "T",
        "fields": [
            {"name": "id", "column": "id", "ty": "i64", "nullable": false, "primary_key": true}
        ]
    }))
    .unwrap()
}

fn mig(name: &str, tables: &[String], replaces: &[String]) -> Migration {
    Migration {
        name: name.to_owned(),
        created_at: "2026-08-06T00:00:00Z".into(),
        prev: None,
        atomic: true,
        scope: migrate::MigrationScope::default(),
        replaces: replaces.to_vec(),
        snapshot: SchemaSnapshot {
            tables: tables.iter().map(|t| table_snap(t)).collect(),
            ..Default::default()
        },
        forward: tables
            .iter()
            .map(|t| Operation::Schema(SchemaChange::CreateTable(t.clone())))
            .collect(),
    }
}

fn write_dir(ms: &[&Migration]) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut dir = std::env::temp_dir();
    dir.push(format!("rustango_reconcile_my_{}_{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for m in ms {
        file::write(&dir.join(format!("{}.json", m.name)), m).unwrap();
    }
    dir
}

/// Same-ledger reconcile against real MySQL: the predecessors ran, so their
/// tables exist. The squash must be recorded and them tombstoned — no 1050.
#[tokio::test]
async fn squash_reconciles_on_mysql_without_colliding() {
    let Ok(url) = std::env::var("MYSQL_TEST_URL") else {
        eprintln!("skipping — set MYSQL_TEST_URL");
        return;
    };
    let my = sqlx::MySqlPool::connect(&url).await.expect("connect mysql");
    let pool = Pool::Mysql(my.clone());

    // Unique names so parallel runs / reruns don't collide.
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let t_a = format!("rec_a_{pid}_{n}");
    let t_b = format!("rec_b_{pid}_{n}");
    let m1 = format!("1{n:03}_a_{pid}");
    let m2 = format!("2{n:03}_b_{pid}");
    let sq = format!("3{n:03}_squashed_{pid}");

    for t in [&t_a, &t_b] {
        sqlx::query(&format!("DROP TABLE IF EXISTS `{t}`"))
            .execute(&my)
            .await
            .unwrap();
    }
    for name in [&m1, &m2, &sq] {
        let _ = sqlx::query(&format!("DELETE FROM {LEDGER} WHERE name = ?"))
            .bind(name)
            .execute(&my)
            .await;
    }

    // History runs for real.
    let a = mig(&m1, std::slice::from_ref(&t_a), &[]);
    let b = mig(&m2, std::slice::from_ref(&t_b), &[]);
    let dir = write_dir(&[&a, &b]);
    let first = migrate::migrate_pool(&pool, &dir).await.unwrap();
    assert_eq!(first.len(), 2);
    sqlx::query(&format!("INSERT INTO `{t_a}` (id) VALUES (5)"))
        .execute(&my)
        .await
        .unwrap();

    // The squash collapses both — must reconcile, not re-CREATE.
    let squash = mig(&sq, &[t_a.clone(), t_b.clone()], &[m1.clone(), m2.clone()]);
    let dir = write_dir(&[&a, &b, &squash]);
    let applied = migrate::migrate_pool(&pool, &dir)
        .await
        .expect("squash must reconcile on MySQL, not hit error 1050");
    assert_eq!(applied.len(), 1);

    // Squash recorded, predecessors tombstoned.
    let rows: Vec<String> = sqlx::query(&format!(
        "SELECT name FROM {LEDGER} WHERE name IN (?, ?, ?) ORDER BY name"
    ))
    .bind(&m1)
    .bind(&m2)
    .bind(&sq)
    .fetch_all(&my)
    .await
    .unwrap()
    .into_iter()
    .map(|r| r.try_get::<String, _>("name").unwrap())
    .collect();
    assert_eq!(rows, vec![sq.clone()], "only the squash should remain");

    // Data intact — no DDL ran.
    let v: i64 = sqlx::query(&format!("SELECT id FROM `{t_a}`"))
        .fetch_one(&my)
        .await
        .unwrap()
        .try_get("id")
        .unwrap();
    assert_eq!(v, 5);

    // Idempotent — the predecessor files are still on disk but superseded.
    assert!(migrate::migrate_pool(&pool, &dir).await.unwrap().is_empty());

    // Cleanup.
    for t in [&t_a, &t_b] {
        let _ = sqlx::query(&format!("DROP TABLE IF EXISTS `{t}`"))
            .execute(&my)
            .await;
    }
    let _ = sqlx::query(&format!("DELETE FROM {LEDGER} WHERE name = ?"))
        .bind(&sq)
        .execute(&my)
        .await;
    let _ = std::fs::remove_dir_all(&dir);
    println!("MySQL squash reconcile OK");
}
