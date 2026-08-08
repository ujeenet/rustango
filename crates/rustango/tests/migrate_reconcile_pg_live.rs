#![cfg(feature = "postgres")]
//! Squash reconciliation on live Postgres, through the **legacy `PgPool`
//! runner** (#1167).
//!
//! `migrate(&PgPool, dir)` is a separate entry point from the tri-dialect
//! `migrate_pool`, so reconcile is wired into it separately — this test is
//! what proves that wiring. Without it a squash would hit `relation ...
//! already exists` (42P07) on any Postgres project still using the classic
//! entry point.
//!
//! Reads `DATABASE_URL`; skips silently when unset.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use rustango::migrate::{
    self, file, Migration, Operation, SchemaChange, SchemaSnapshot, TableSnapshot,
};
use rustango::sql::sqlx::{self, PgPool, Row};

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
    dir.push(format!("rustango_reconcile_pg_{}_{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for m in ms {
        file::write(&dir.join(format!("{}.json", m.name)), m).unwrap();
    }
    dir
}

#[tokio::test]
async fn squash_reconciles_on_postgres_legacy_runner() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping — set DATABASE_URL");
        return;
    };
    let pool = PgPool::connect(&url).await.expect("connect postgres");

    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let t_a = format!("rec_pg_a_{pid}_{n}");
    let t_b = format!("rec_pg_b_{pid}_{n}");
    let m1 = format!("1{n:03}_a_{pid}");
    let m2 = format!("2{n:03}_b_{pid}");
    let sq = format!("3{n:03}_squashed_{pid}");

    for t in [&t_a, &t_b] {
        sqlx::query(&format!("DROP TABLE IF EXISTS \"{t}\""))
            .execute(&pool)
            .await
            .unwrap();
    }

    // History runs for real through the legacy runner.
    let a = mig(&m1, std::slice::from_ref(&t_a), &[]);
    let b = mig(&m2, std::slice::from_ref(&t_b), &[]);
    let dir = write_dir(&[&a, &b]);
    let first = migrate::migrate(&pool, &dir).await.unwrap();
    assert_eq!(first.len(), 2);
    sqlx::query(&format!("INSERT INTO \"{t_a}\" (id) VALUES (9)"))
        .execute(&pool)
        .await
        .unwrap();

    // The squash must reconcile rather than re-CREATE (42P07).
    let squash = mig(&sq, &[t_a.clone(), t_b.clone()], &[m1.clone(), m2.clone()]);
    let dir = write_dir(&[&a, &b, &squash]);
    let applied = migrate::migrate(&pool, &dir)
        .await
        .expect("squash must reconcile on the legacy PgPool runner, not hit 42P07");
    assert_eq!(applied.len(), 1);

    // Squash recorded, predecessors tombstoned.
    let rows: Vec<String> = sqlx::query(&format!(
        "SELECT name FROM {LEDGER} WHERE name = ANY($1) ORDER BY name"
    ))
    .bind(vec![m1.clone(), m2.clone(), sq.clone()])
    .fetch_all(&pool)
    .await
    .unwrap()
    .into_iter()
    .map(|r| r.try_get::<String, _>("name").unwrap())
    .collect();
    assert_eq!(rows, vec![sq.clone()], "only the squash should remain");

    // Data intact — no DDL ran.
    let v: i64 = sqlx::query(&format!("SELECT id FROM \"{t_a}\""))
        .fetch_one(&pool)
        .await
        .unwrap()
        .try_get("id")
        .unwrap();
    assert_eq!(v, 9);

    // Idempotent — predecessor files remain on disk but are superseded.
    assert!(migrate::migrate(&pool, &dir).await.unwrap().is_empty());

    // Cleanup.
    for t in [&t_a, &t_b] {
        let _ = sqlx::query(&format!("DROP TABLE IF EXISTS \"{t}\""))
            .execute(&pool)
            .await;
    }
    let _ = sqlx::query(&format!("DELETE FROM {LEDGER} WHERE name = $1"))
        .bind(&sq)
        .execute(&pool)
        .await;
    let _ = std::fs::remove_dir_all(&dir);
    println!("Postgres (legacy runner) squash reconcile OK");
}
